//! Cross-resource references, for go-to-definition.
//!
//! Recognises the reference fields Kubernetes uses (`configMapRef.name`,
//! `claimName`, `roleRef`, label selectors, …) and resolves them against the
//! manifests in the workspace.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::yaml::{self, Doc};

#[derive(Debug, Clone, PartialEq)]
pub enum Reference {
    /// A specific object, e.g. `ConfigMap app-config`.
    Named { kind: String, name: String, namespace: Option<String> },
    /// A label selector; matches objects whose pods carry all these labels.
    Selector { labels: Vec<(String, String)>, namespace: Option<String> },
}

/// Kinds that are not namespaced, so namespace must not constrain matches.
const CLUSTER_SCOPED: &[&str] = &[
    "ClusterRole",
    "ClusterRoleBinding",
    "IngressClass",
    "Namespace",
    "Node",
    "PersistentVolume",
    "PriorityClass",
    "RuntimeClass",
    "StorageClass",
    "CustomResourceDefinition",
];

pub fn is_cluster_scoped(kind: &str) -> bool {
    CLUSTER_SCOPED.contains(&kind)
}

/// What the line under the cursor refers to, if anything.
pub fn reference_at(lines: &[&str], doc: &Doc, line: usize) -> Option<Reference> {
    let (_, key, raw) = yaml::key_of(lines[line])?;
    let path = yaml::key_path(lines, doc.start, line);
    let value = yaml::unquote(raw).to_string();
    let parent = path.len().checked_sub(2).map(|i| path[i].as_str()).unwrap_or("");
    let ns = doc.namespace.clone();

    // Label selectors: `spec.selector.<k>` on a Service, `…matchLabels.<k>` elsewhere.
    let is_service_selector = doc.kind.as_deref() == Some("Service") && path.len() == 3 && path[..2] == ["spec", "selector"];
    if parent == "matchLabels" || is_service_selector {
        let block_prefix: Vec<&str> = path[..path.len() - 1].iter().map(String::as_str).collect();
        let labels = yaml::pairs_under(lines, doc, &block_prefix);
        return (!labels.is_empty()).then_some(Reference::Selector { labels, namespace: ns });
    }

    if value.is_empty() {
        return None;
    }
    let sibling = |k: &str| {
        yaml::siblings(lines, doc, line).into_iter().find(|(key, _)| key == k).map(|(_, v)| v)
    };
    let kind: String = match (parent, key) {
        ("configMapRef" | "configMapKeyRef" | "configMap", "name") => "ConfigMap".into(),
        ("secretRef" | "secretKeyRef" | "imagePullSecrets", "name") => "Secret".into(),
        ("secret" | "tls", "secretName") => "Secret".into(),
        (_, "claimName") => "PersistentVolumeClaim".into(),
        (_, "serviceAccountName" | "serviceAccount") => "ServiceAccount".into(),
        (_, "serviceName") => "Service".into(),
        ("service", "name") if path.iter().any(|p| p == "backend" || p == "defaultBackend") => "Service".into(),
        (_, "storageClassName") => "StorageClass".into(),
        (_, "priorityClassName") => "PriorityClass".into(),
        (_, "ingressClassName") => "IngressClass".into(),
        (_, "runtimeClassName") => "RuntimeClass".into(),
        (_, "volumeName") => "PersistentVolume".into(),
        ("scaleTargetRef" | "roleRef" | "subjects" | "targetRef", "name") => sibling("kind")?,
        _ => return None,
    };
    let namespace = if is_cluster_scoped(&kind) {
        None
    } else if parent == "subjects" {
        sibling("namespace").or(ns)
    } else {
        ns
    };
    Some(Reference::Named { kind, name: value, namespace })
}

/// One object found in a workspace manifest.
#[derive(Debug, Clone)]
pub struct Entry {
    pub uri: String,
    pub line: usize,
    pub kind: String,
    pub name: String,
    pub namespace: Option<String>,
    pub pod_labels: Vec<(String, String)>,
}

pub fn entries(uri: &str, text: &str) -> Vec<Entry> {
    let lines = yaml::lines(text);
    yaml::parse(&lines)
        .iter()
        .filter(|d| d.is_resource() && !d.is_kubeconfig())
        .filter_map(|d| {
            Some(Entry {
                uri: uri.to_string(),
                line: d.name_line.or(d.kind_line).unwrap_or(d.start),
                kind: d.kind.clone()?,
                name: d.name.clone()?,
                namespace: d.namespace.clone(),
                pod_labels: yaml::pod_labels(&lines, d),
            })
        })
        .collect()
}

/// Matches, best first (same namespace before others).
pub fn resolve<'a>(reference: &Reference, all: &'a [Entry]) -> Vec<&'a Entry> {
    // `None` namespace = "whatever the default is"; treat it as compatible.
    let ns_rank = |want: &Option<String>, have: &Option<String>| match (want, have) {
        (Some(a), Some(b)) if a == b => 0,
        (None, None) => 0,
        (Some(_), Some(_)) => 2,
        _ => 1,
    };
    let mut hits: Vec<(u8, &Entry)> = match reference {
        Reference::Named { kind, name, namespace } => all
            .iter()
            .filter(|e| &e.kind == kind && &e.name == name)
            .map(|e| (if is_cluster_scoped(kind) { 0 } else { ns_rank(namespace, &e.namespace) }, e))
            .collect(),
        Reference::Selector { labels, namespace } => all
            .iter()
            .filter(|e| !e.pod_labels.is_empty() && labels.iter().all(|l| e.pod_labels.contains(l)))
            .map(|e| (ns_rank(namespace, &e.namespace), e))
            .collect(),
    };
    hits.sort_by_key(|(rank, e)| (*rank, e.uri.clone(), e.line));
    // Different-namespace matches are only useful when nothing better exists.
    let best = hits.first().map(|(r, _)| *r).unwrap_or(0);
    hits.into_iter().filter(|(r, _)| *r == best || *r < 2).map(|(_, e)| e).collect()
}

/// Caches parsed workspace manifests by modification time.
#[derive(Default)]
pub struct Index {
    files: HashMap<PathBuf, (SystemTime, Vec<Entry>)>,
}

const SKIP_DIRS: &[&str] = &["node_modules", "target", "vendor", "dist", "build"];
const MAX_FILES: usize = 5000;
const MAX_SIZE: u64 = 512 * 1024;

impl Index {
    /// Entries from every manifest under `roots`, with open (possibly
    /// unsaved) documents taking precedence over their on-disk contents.
    pub fn collect(&mut self, roots: &[PathBuf], open: &HashMap<String, String>, to_uri: impl Fn(&Path) -> String) -> Vec<Entry> {
        let mut files = Vec::new();
        for root in roots {
            walk(root, &mut files);
        }
        let open_uris: std::collections::HashSet<&str> = open.keys().map(String::as_str).collect();
        let mut out = Vec::new();
        for path in files {
            let uri = to_uri(&path);
            if open_uris.contains(uri.as_str()) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            if let Some((cached, list)) = self.files.get(&path) {
                if *cached == mtime {
                    out.extend(list.iter().cloned());
                    continue;
                }
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let list = entries(&uri, &text);
            out.extend(list.iter().cloned());
            self.files.insert(path, (mtime, list));
        }
        for (uri, text) in open {
            out.extend(entries(uri, text));
        }
        out
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    if out.len() >= MAX_FILES {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for entry in read.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_ref()) {
                walk(&path, out);
            }
        } else if (name.ends_with(".yaml") || name.ends_with(".yml"))
            && entry.metadata().map(|m| m.len() <= MAX_SIZE).unwrap_or(false)
        {
            out.push(path);
            if out.len() >= MAX_FILES {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEPLOY: &str = "\
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
  namespace: shop
spec:
  selector:
    matchLabels:
      app: web
  template:
    metadata:
      labels:
        app: web
        tier: front
    spec:
      serviceAccountName: web-sa
      containers:
        - name: app
          image: nginx
          envFrom:
            - configMapRef:
                name: web-config
          env:
            - name: PASSWORD
              valueFrom:
                secretKeyRef:
                  name: db-secret
                  key: password
      volumes:
        - name: data
          persistentVolumeClaim:
            claimName: web-data
---
apiVersion: v1
kind: Service
metadata:
  name: web
  namespace: shop
spec:
  selector:
    app: web
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: rb
  namespace: shop
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: view
subjects:
  - kind: ServiceAccount
    name: web-sa
    namespace: shop
";

    fn at(line_contains: &str, occurrence: usize) -> Option<Reference> {
        let lines = yaml::lines(DEPLOY);
        let docs = yaml::parse(&lines);
        let line = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains(line_contains))
            .nth(occurrence)
            .unwrap()
            .0;
        let (_, doc) = yaml::doc_at(&docs, line).unwrap();
        reference_at(&lines, doc, line)
    }

    fn named(kind: &str, name: &str, ns: Option<&str>) -> Option<Reference> {
        Some(Reference::Named { kind: kind.into(), name: name.into(), namespace: ns.map(String::from) })
    }

    #[test]
    fn detects_named_references() {
        assert_eq!(at("name: web-config", 0), named("ConfigMap", "web-config", Some("shop")));
        assert_eq!(at("name: db-secret", 0), named("Secret", "db-secret", Some("shop")));
        assert_eq!(at("claimName", 0), named("PersistentVolumeClaim", "web-data", Some("shop")));
        assert_eq!(at("serviceAccountName", 0), named("ServiceAccount", "web-sa", Some("shop")));
        assert_eq!(at("name: view", 0), named("ClusterRole", "view", None));
        assert_eq!(at("name: web-sa", 0), named("ServiceAccount", "web-sa", Some("shop")));
        assert_eq!(at("name: app", 0), None); // container name is not a reference
        assert_eq!(at("key: password", 0), None);
    }

    #[test]
    fn selector_resolves_to_workload() {
        let sel = at("app: web", 2).unwrap(); // the Service's selector
        let all = entries("file:///x.yaml", DEPLOY);
        let hits = resolve(&sel, &all);
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].kind.as_str(), hits[0].name.as_str()), ("Deployment", "web"));
    }

    #[test]
    fn named_resolution_prefers_namespace() {
        let other = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: web-config\n  namespace: other\n---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: web-config\n  namespace: shop\n";
        let all = entries("file:///cm.yaml", other);
        let hits = resolve(&named("ConfigMap", "web-config", Some("shop")).unwrap(), &all);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].namespace.as_deref(), Some("shop"));
        assert_eq!(hits[0].line, 9); // the second document's `name:` line
    }
}
