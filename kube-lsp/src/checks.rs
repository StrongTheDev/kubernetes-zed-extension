//! Warnings about references that lead nowhere.
//!
//! An object is only reported missing when it is neither in the workspace nor
//! in the cluster; when the cluster can't be asked, nothing is reported.

use crate::cluster::Lookup;
use crate::refs::{self, Contents, Entry, Reference};
use crate::schema::Problem;
use crate::tree::{self, Node, Pos};
use crate::yaml::{self, Doc};

/// API groups whose reference fields mean what we think they mean. Custom
/// resources reuse names like `secretRef` for other things.
const KNOWN_GROUPS: &[&str] = &[
    "",
    "apps",
    "batch",
    "autoscaling",
    "networking.k8s.io",
    "policy",
    "rbac.authorization.k8s.io",
    "storage.k8s.io",
];

const CHECKED_KINDS: &[&str] = &[
    "ConfigMap",
    "Secret",
    "PersistentVolumeClaim",
    "ServiceAccount",
    "Service",
    "StorageClass",
    "PriorityClass",
    "IngressClass",
    "RuntimeClass",
    "Role",
    "ClusterRole",
    "Deployment",
    "StatefulSet",
    "ReplicaSet",
];

/// Problems in `docs[index]`, given everything known about the workspace.
pub fn check(uri: &str, lines: &[&str], doc: &Doc, all: &Contents, cluster: &dyn Lookup) -> Vec<Problem> {
    let group = doc.api_version.as_deref().and_then(|v| v.split_once('/')).map_or("", |(g, _)| g);
    if !KNOWN_GROUPS.contains(&group) {
        return Vec::new();
    }
    let Some(root) = tree::parse(lines, doc) else { return Vec::new() };
    let mut out = Vec::new();
    named(uri, lines, doc, all, cluster, &mut out);
    own_selector(doc, &root, &mut out);
    match doc.kind.as_deref() {
        Some("Service") => service(doc, &root, all, cluster, &mut out),
        Some("Ingress") => ingress(doc, &root, all, &mut out),
        _ => {}
    }
    out
}

fn warning(pos: Pos, message: String) -> Problem {
    Problem { pos, severity: 2, message }
}

fn whole_line(line: usize) -> Pos {
    Pos { line, start: 0, end: usize::MAX }
}

fn place_pos(lines: &[&str], line: usize) -> Pos {
    match yaml::value_range(lines[line]) {
        // Positions are in characters.
        Some((a, b)) => Pos { line, start: lines[line][..a].chars().count(), end: lines[line][..b].chars().count() },
        None => whole_line(line),
    }
}

fn named(uri: &str, lines: &[&str], doc: &Doc, all: &Contents, cluster: &dyn Lookup, out: &mut Vec<Problem>) {
    let cert_manager =
        lines[doc.start..doc.end].iter().any(|l| l.trim_start().trim_start_matches('"').starts_with("cert-manager.io/"));
    for usage in all.usages.iter().filter(|u| u.uri == uri && (doc.start..doc.end).contains(&u.place.line)) {
        let Reference::Named { kind, name, namespace } = &usage.reference else { continue };
        let line = usage.place.line;
        // The buffer may have changed since the index was built.
        if !CHECKED_KINDS.contains(&kind.as_str())
            || lines.get(line) != Some(&usage.place.text.as_str())
            || name.contains(['$', '{'])
            || (kind == "ServiceAccount" && name == "default")
            || (kind == "Secret" && doc.kind.as_deref() == Some("Ingress") && cert_manager)
        {
            continue;
        }
        let siblings = yaml::siblings(lines, doc, line);
        let sibling = |k: &str| siblings.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        if sibling("optional") == Some("true") {
            continue;
        }
        let hits = refs::resolve(&usage.reference, &all.entries);
        if hits.is_empty() {
            let known = cluster.names(kind, namespace.as_deref().filter(|_| !refs::is_cluster_scoped(kind)));
            if known.is_some_and(|names| !names.contains(name)) {
                let message = format!("{kind} `{name}` was not found in this workspace or in the cluster");
                out.push(warning(place_pos(lines, line), message));
            }
            continue;
        }
        // `configMapKeyRef` / `secretKeyRef`
        let Some(key) = sibling("key").filter(|k| !k.is_empty()) else { continue };
        let keys: Option<Vec<&Vec<String>>> = hits.iter().map(|e| e.keys.as_ref()).collect();
        if keys.is_some_and(|k| !k.iter().any(|keys| keys.iter().any(|x| x == key))) {
            let at = (doc.start..doc.end)
                .filter(|l| l.abs_diff(line) <= siblings.len())
                .find(|l| yaml::key_of(lines[*l]).is_some_and(|(_, k, v)| k == "key" && yaml::unquote(v) == key))
                .unwrap_or(line);
            out.push(warning(place_pos(lines, at), format!("{kind} `{name}` has no key `{key}`")));
        }
    }
}

/// A workload whose selector doesn't match its own pods is always rejected.
fn own_selector(doc: &Doc, root: &Node, out: &mut Vec<Problem>) {
    if !matches!(doc.kind.as_deref(), Some("Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet")) {
        return;
    }
    let Some(template) = root.at(&["spec", "template"]) else { return };
    let labels = template.at(&["metadata", "labels"]).map(Node::pairs).unwrap_or_default();
    for (key, value) in root.at(&["spec", "selector", "matchLabels"]).map(Node::entries).unwrap_or_default() {
        let Some(want) = value.str() else { continue };
        if !labels.iter().any(|(k, v)| *k == key.name && v == want) {
            let message = format!("pod template has no label `{}: {want}`, so this selector can't match", key.name);
            out.push(Problem { pos: key.pos, severity: 1, message });
        }
    }
}

fn compatible(a: &Option<String>, b: &Option<String>) -> bool {
    a.is_none() || b.is_none() || a == b
}

fn service(doc: &Doc, root: &Node, all: &Contents, cluster: &dyn Lookup, out: &mut Vec<Problem>) {
    let Some(selector) = root.at(&["spec", "selector"]) else { return };
    let labels = selector.pairs();
    if labels.is_empty() || labels.iter().any(|(_, v)| v.contains(['$', '{'])) {
        return;
    }
    let workloads: Vec<&Entry> =
        all.entries.iter().filter(|e| refs::selects(&labels, e) && compatible(&doc.namespace, &e.namespace)).collect();
    if workloads.is_empty() {
        if cluster.any_pod(doc.namespace.as_deref(), &labels) == Some(false) {
            let pos = selector.entries().first().map_or(selector.pos, |(k, _)| k.pos);
            let message = "no workload in this workspace and no pod in the cluster matches this selector".to_string();
            out.push(warning(pos, message));
        }
        return;
    }
    let ports: Vec<_> = workloads.iter().flat_map(|e| &e.ports).collect();
    for port in root.at(&["spec", "ports"]).map(Node::items).unwrap_or_default() {
        let Some(target) = port.get("targetPort").or_else(|| port.get("port")) else { continue };
        let Some(text) = target.str() else { continue };
        let names: Vec<&str> = workloads.iter().map(|e| e.name.as_str()).collect();
        let owner = names[0];
        match refs::port_number(target) {
            None => {
                if !ports.iter().any(|(n, _)| n.as_deref() == Some(text)) {
                    out.push(warning(target.pos, format!("no container of `{owner}` has a port named `{text}`")));
                }
            }
            // Declaring container ports is optional, so this is only a hint.
            Some(number) => {
                let declared: Vec<u32> = ports.iter().filter_map(|(_, n)| *n).collect();
                let all_declare = workloads.iter().all(|e| !e.ports.is_empty());
                if all_declare && !declared.contains(&number) {
                    let list = declared.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
                    let message = format!("`{owner}` declares port {list}, but not {number}");
                    out.push(Problem { pos: target.pos, severity: 3, message });
                }
            }
        }
    }
}

fn ingress(doc: &Doc, root: &Node, all: &Contents, out: &mut Vec<Problem>) {
    let Some(spec) = root.get("spec") else { return };
    let rules = spec.get("rules").map(Node::items).unwrap_or_default();
    let paths = rules.iter().flat_map(|r| r.at(&["http", "paths"]).map(Node::items).unwrap_or_default());
    let backends = paths.filter_map(|p| p.at(&["backend", "service"])).chain(spec.at(&["defaultBackend", "service"]));
    for backend in backends {
        let Some(name) = backend.get("name").and_then(Node::str) else { continue };
        let services: Vec<&Entry> = all
            .entries
            .iter()
            .filter(|e| e.kind == "Service" && e.name == name && compatible(&doc.namespace, &e.namespace))
            .filter(|e| !e.ports.is_empty())
            .collect();
        if services.is_empty() {
            continue;
        }
        let ports: Vec<_> = services.iter().flat_map(|e| &e.ports).collect();
        if let Some(node) = backend.at(&["port", "name"]) {
            let wanted = node.str().unwrap_or_default();
            if !ports.iter().any(|(n, _)| n.as_deref() == Some(wanted)) {
                out.push(warning(node.pos, format!("Service `{name}` has no port named `{wanted}`")));
            }
        }
        if let Some(node) = backend.at(&["port", "number"]) {
            let wanted = refs::port_number(node);
            if wanted.is_some() && !ports.iter().any(|(_, n)| *n == wanted) {
                let text = node.str().unwrap_or_default();
                out.push(warning(node.pos, format!("Service `{name}` has no port {text}")));
            }
        }
    }
}

/// Whether the kustomization next to `path` uses the file as a patch. Patches
/// are incomplete objects, so most checks would be wrong about them.
pub fn is_patch(path: &std::path::Path) -> bool {
    let (Some(dir), Some(file)) = (path.parent(), path.file_name().map(|f| f.to_string_lossy().into_owned())) else {
        return false;
    };
    ["kustomization.yaml", "kustomization.yml", "Kustomization"]
        .iter()
        .filter_map(|k| std::fs::read_to_string(dir.join(k)).ok())
        .any(|text| patch_files(&text).contains(&file))
}

fn patch_files(kustomization: &str) -> Vec<String> {
    let lines = yaml::lines(kustomization);
    let mut out = Vec::new();
    for doc in yaml::parse(&lines) {
        let Some(root) = tree::parse(&lines, &doc) else { continue };
        for field in ["patches", "patchesStrategicMerge", "patchesJson6902"] {
            for item in root.get(field).map(Node::items).unwrap_or_default() {
                if let Some(file) = item.str().or_else(|| item.get("path").and_then(Node::str)) {
                    out.push(file.trim_start_matches("./").to_string());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;

    struct Fake(Option<&'static [&'static str]>);

    impl Lookup for Fake {
        fn names(&self, _: &str, _: Option<&str>) -> Option<Arc<HashSet<String>>> {
            self.0.map(|n| Arc::new(n.iter().map(|s| s.to_string()).collect()))
        }
        fn any_pod(&self, _: Option<&str>, _: &[(String, String)]) -> Option<bool> {
            self.0.map(|n| n.contains(&"pods"))
        }
    }

    const URI: &str = "file:///a.yaml";
    const SRC: &str = "\
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  selector:
    matchLabels:
      app: web
  template:
    metadata:
      labels:
        app: web
    spec:
      containers:
        - name: app
          ports:
            - name: http
              containerPort: 8080
          env:
            - name: A
              valueFrom:
                configMapKeyRef:
                  name: cfg
                  key: a
            - name: B
              valueFrom:
                secretKeyRef:
                  name: maybe
                  key: b
                  optional: true
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: cfg
data:
  a: \"1\"
---
apiVersion: v1
kind: Service
metadata:
  name: web
spec:
  selector:
    app: web
  ports:
    - port: 80
      targetPort: http
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: web
spec:
  rules:
    - http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: web
                port:
                  number: 80
";

    fn messages(src: &str, cluster: Fake) -> Vec<String> {
        let lines = yaml::lines(src);
        let all = refs::scan(URI, src);
        yaml::parse(&lines)
            .iter()
            .flat_map(|d| check(URI, &lines, d, &all, &cluster))
            .map(|p| format!("{}: {}", p.pos.line, p.message))
            .collect()
    }

    const EMPTY: Fake = Fake(Some(&[]));

    #[test]
    fn consistent_files_are_quiet() {
        assert_eq!(messages(SRC, EMPTY), Vec::<String>::new());
    }

    #[test]
    fn missing_objects() {
        let src = SRC.replace("name: cfg\n                  key", "name: nope\n                  key");
        assert_eq!(messages(&src, EMPTY), ["22: ConfigMap `nope` was not found in this workspace or in the cluster"]);
        assert!(messages(&src, Fake(Some(&["nope"]))).is_empty()); // exists in the cluster
        assert!(messages(&src, Fake(None)).is_empty()); // offline: don't guess
    }

    #[test]
    fn missing_keys() {
        let src = SRC.replace("key: a\n", "key: z\n");
        assert_eq!(messages(&src, EMPTY), ["23: ConfigMap `cfg` has no key `z`"]);
    }

    #[test]
    fn selectors() {
        let src = SRC.replace("  selector:\n    app: web\n", "  selector:\n    app: other\n");
        assert_eq!(messages(&src, EMPTY).len(), 1);
        assert!(messages(&src, Fake(Some(&["pods"]))).is_empty());
        let src = SRC.replace("      app: web\n  template", "      app: typo\n  template");
        assert!(messages(&src, EMPTY)[0].contains("pod template has no label `app: typo`"));
    }

    #[test]
    fn ports() {
        let src = SRC.replace("targetPort: http", "targetPort: htp");
        assert!(messages(&src, EMPTY)[0].contains("no container of `web` has a port named `htp`"));
        let src = SRC.replace("targetPort: http", "targetPort: 9090");
        assert!(messages(&src, EMPTY)[0].contains("declares port 8080, but not 9090"));
        let src = SRC.replace("number: 80", "number: 81");
        assert!(messages(&src, EMPTY)[0].contains("Service `web` has no port 81"));
    }

    #[test]
    fn custom_resources_are_left_alone() {
        let src = "apiVersion: source.toolkit.fluxcd.io/v1\nkind: GitRepository\nmetadata:\n  name: x\nspec:\n  secretRef:\n    name: nope\n";
        assert!(messages(src, EMPTY).is_empty());
    }

    #[test]
    fn recognises_patches() {
        let k = "resources: [base.yaml]\npatches:\n  - path: ./p.yaml\n  - target: {kind: Deployment}\n    patch: \"- op: x\"\npatchesStrategicMerge:\n  - q.yaml\n";
        assert_eq!(patch_files(k), ["p.yaml", "q.yaml"]);
    }
}
