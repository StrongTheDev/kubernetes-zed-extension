//! Cross-resource references: go-to-definition, find usages and rename.
//!
//! Recognises the reference fields Kubernetes uses (`configMapRef.name`,
//! `claimName`, `roleRef`, label selectors, …) and resolves them against the
//! manifests in the workspace.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::tree::{self, Node};
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
    let is_service_selector =
        doc.kind.as_deref() == Some("Service") && path.len() == 3 && path[..2] == ["spec", "selector"];
    if parent == "matchLabels" || is_service_selector {
        let block_prefix: Vec<&str> = path[..path.len() - 1].iter().map(String::as_str).collect();
        let labels = yaml::pairs_under(lines, doc, &block_prefix);
        return (!labels.is_empty()).then_some(Reference::Selector { labels, namespace: ns });
    }

    let service_selector = doc.kind.as_deref() == Some("Service") && path == ["spec", "selector"];
    if (key == "matchLabels" || service_selector) && raw.starts_with('{') {
        let labels = yaml::flow_pairs(raw);
        return (!labels.is_empty()).then_some(Reference::Selector { labels, namespace: ns });
    }
    if value.is_empty() {
        return None;
    }
    let sibling =
        |k: &str| yaml::siblings(lines, doc, line).into_iter().find(|(key, _)| key == k).map(|(_, v)| v);
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
        // `patches[].target` in a kustomization.
        ("target", "name") if path.first().is_some_and(|p| p.starts_with("patches")) => sibling("kind")?,
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

/// A value's place in a file (byte columns).
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    pub line: usize,
    pub start: usize,
    pub end: usize,
    /// Text of the line, to convert columns for the client.
    pub text: String,
}

impl Place {
    fn value_on(lines: &[&str], line: usize) -> Option<Self> {
        let (start, end) = yaml::value_range(lines[line])?;
        Some(Self { line, start, end, text: lines[line].to_string() })
    }
}

/// One object found in a workspace manifest.
#[derive(Debug, Clone)]
pub struct Entry {
    pub uri: String,
    pub line: usize,
    pub kind: String,
    pub name: String,
    pub namespace: Option<String>,
    /// Where the name is written.
    pub name_place: Option<Place>,
    pub pod_labels: Vec<(String, String)>,
    /// Container ports of a workload, or ports of a Service: `(name, number)`.
    pub ports: Vec<(Option<String>, Option<u32>)>,
    /// Keys of a ConfigMap or Secret; `None` when they aren't known.
    pub keys: Option<Vec<String>>,
}

impl Entry {
    fn new(uri: &str, kind: &str, name: &str, namespace: Option<String>, line: usize) -> Self {
        Self {
            uri: uri.to_string(),
            line,
            kind: kind.to_string(),
            name: name.to_string(),
            namespace,
            name_place: None,
            pod_labels: Vec::new(),
            ports: Vec::new(),
            keys: None,
        }
    }
}

/// One reference found in a workspace manifest.
#[derive(Debug, Clone)]
pub struct Usage {
    pub uri: String,
    pub place: Place,
    pub reference: Reference,
}

#[derive(Debug, Clone, Default)]
pub struct Contents {
    pub entries: Vec<Entry>,
    pub usages: Vec<Usage>,
}

/// The pod template of a workload (the object itself for a Pod).
fn pod(root: &Node, kind: &str) -> Option<Node> {
    let path: &[&str] = match kind {
        "Pod" => &[],
        "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet" | "Job" | "ReplicationController" => {
            &["spec", "template"]
        }
        "CronJob" => &["spec", "jobTemplate", "spec", "template"],
        _ => return None,
    };
    root.at(path).cloned()
}

pub fn port_number(node: &Node) -> Option<u32> {
    node.str()?.parse().ok()
}

fn is_kustomization(uri: &str, doc: &Doc) -> bool {
    doc.kind.as_deref() == Some("Kustomization") && doc.api_version.as_deref().is_none_or(|v| v.starts_with("kustomize.config.k8s.io"))
        || ["/kustomization.yaml", "/kustomization.yml", "/Kustomization"].iter().any(|f| uri.ends_with(f))
}

pub fn scan(uri: &str, text: &str) -> Contents {
    let lines = yaml::lines(text);
    let mut out = Contents::default();
    for doc in yaml::parse(&lines) {
        if doc.is_kubeconfig() {
            continue;
        }
        if is_kustomization(uri, &doc) {
            out.entries.extend(generated(uri, &lines, &doc));
            out.usages.extend(usages(uri, &lines, &doc));
            continue;
        }
        if !doc.is_resource() {
            continue;
        }
        out.entries.extend(entries(uri, &lines, &doc));
        out.usages.extend(usages(uri, &lines, &doc));
    }
    out
}

fn entries(uri: &str, lines: &[&str], doc: &Doc) -> Vec<Entry> {
    let (Some(kind), Some(name)) = (doc.kind.as_deref(), doc.name.as_deref()) else { return Vec::new() };
    let line = doc.name_line.or(doc.kind_line).unwrap_or(doc.start);
    let mut entry = Entry::new(uri, kind, name, doc.namespace.clone(), line);
    entry.name_place = doc.name_line.and_then(|l| Place::value_on(lines, l));
    entry.pod_labels = yaml::pod_labels(lines, doc);
    let mut out = Vec::new();

    let detailed = ["Service", "ConfigMap", "Secret", "SealedSecret", "ExternalSecret", "Certificate"];
    if !entry.pod_labels.is_empty() || detailed.contains(&kind) {
        if let Some(root) = tree::parse(lines, doc) {
            if let Some(pod) = pod(&root, kind) {
                if let Some(labels) = pod.at(&["metadata", "labels"]) {
                    entry.pod_labels = labels.pairs();
                }
                for group in ["containers", "initContainers"] {
                    for c in pod.at(&["spec", group]).map(Node::items).unwrap_or_default() {
                        for p in c.get("ports").map(Node::items).unwrap_or_default() {
                            let name = p.get("name").and_then(Node::str).map(String::from);
                            entry.ports.push((name, p.get("containerPort").and_then(port_number)));
                        }
                    }
                }
            }
            match kind {
                "Service" => {
                    for p in root.at(&["spec", "ports"]).map(Node::items).unwrap_or_default() {
                        let name = p.get("name").and_then(Node::str).map(String::from);
                        entry.ports.push((name, p.get("port").and_then(port_number)));
                    }
                }
                "ConfigMap" | "Secret" => {
                    let keys = ["data", "stringData", "binaryData"]
                        .iter()
                        .filter_map(|k| root.get(k))
                        .flat_map(|n| n.entries().iter().map(|(k, _)| k.name.clone()))
                        .collect();
                    entry.keys = Some(keys);
                }
                // Controllers that create a Secret from this object.
                "SealedSecret" => out.push(Entry { kind: "Secret".into(), ..entry.clone() }),
                "ExternalSecret" | "Certificate" => {
                    let path: &[&str] =
                        if kind == "Certificate" { &["spec", "secretName"] } else { &["spec", "target", "name"] };
                    match root.at(path) {
                        Some(node) => {
                            if let Some(name) = node.str() {
                                let mut e = Entry::new(uri, "Secret", name, doc.namespace.clone(), node.pos.line);
                                e.name_place = Place::value_on(lines, node.pos.line);
                                out.push(e);
                            }
                        }
                        None if kind == "ExternalSecret" => {
                            out.push(Entry { kind: "Secret".into(), ..entry.clone() });
                        }
                        None => {}
                    }
                }
                _ => {}
            }
        }
    }
    out.insert(0, entry);
    out
}

/// ConfigMaps and Secrets that a kustomization generates.
fn generated(uri: &str, lines: &[&str], doc: &Doc) -> Vec<Entry> {
    let Some(root) = tree::parse(lines, doc) else { return Vec::new() };
    let namespace = root.get("namespace").and_then(Node::str).map(String::from);
    [("configMapGenerator", "ConfigMap"), ("secretGenerator", "Secret")]
        .iter()
        .flat_map(|(field, kind)| {
            root.get(field).map(Node::items).unwrap_or_default().iter().filter_map(|item| {
                let node = item.get("name")?;
                let mut e = Entry::new(uri, kind, node.str()?, namespace.clone(), node.pos.line);
                e.name_place = Place::value_on(lines, node.pos.line);
                Some(e)
            })
        })
        .collect()
}

/// Cheap pre-filter, so only plausible lines pay for a path lookup.
fn may_be_reference(key: &str) -> bool {
    matches!(
        key,
        "name"
            | "secretName"
            | "claimName"
            | "serviceAccountName"
            | "serviceAccount"
            | "serviceName"
            | "storageClassName"
            | "priorityClassName"
            | "ingressClassName"
            | "runtimeClassName"
            | "volumeName"
    )
}

fn usages(uri: &str, lines: &[&str], doc: &Doc) -> Vec<Usage> {
    // These hold data or schemas, not references.
    if matches!(doc.kind.as_deref(), Some("ConfigMap" | "Secret" | "CustomResourceDefinition")) {
        return Vec::new();
    }
    let mask = yaml::block_mask(lines, doc);
    let mut selector_block: Option<usize> = None;
    let mut out = Vec::new();
    for line in doc.start..doc.end {
        if mask[line - doc.start] {
            continue;
        }
        let Some((col, key, value)) = yaml::key_of(lines[line]) else { continue };
        if selector_block.is_some_and(|c| col <= c) {
            selector_block = None;
        }
        let opens_selector = key == "matchLabels" || (key == "selector" && doc.kind.as_deref() == Some("Service"));
        if opens_selector {
            selector_block = Some(col);
            if !value.starts_with('{') {
                continue;
            }
        }
        if selector_block.is_none() && !may_be_reference(key) {
            continue;
        }
        let Some(reference) = reference_at(lines, doc, line) else { continue };
        // A selector is one usage, however many labels it lists.
        if matches!(reference, Reference::Selector { .. }) {
            if selector_block.is_none() {
                continue;
            }
            selector_block = None;
        }
        if let Some(place) = Place::value_on(lines, line) {
            out.push(Usage { uri: uri.to_string(), place, reference });
        }
    }
    out
}

/// `None` namespace = "whatever the default is", so it is compatible with any.
fn same_namespace(kind: &str, a: &Option<String>, b: &Option<String>) -> bool {
    is_cluster_scoped(kind) || a.is_none() || b.is_none() || a == b
}

pub fn selects(labels: &[(String, String)], entry: &Entry) -> bool {
    !entry.pod_labels.is_empty() && labels.iter().all(|l| entry.pod_labels.contains(l))
}

/// Matches, best first (same namespace before others).
pub fn resolve<'a>(reference: &Reference, all: &'a [Entry]) -> Vec<&'a Entry> {
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
        Reference::Selector { labels, namespace } => {
            all.iter().filter(|e| selects(labels, e)).map(|e| (ns_rank(namespace, &e.namespace), e)).collect()
        }
    };
    hits.sort_by_key(|(rank, e)| (*rank, e.uri.clone(), e.line));
    // Different-namespace matches are only useful when nothing better exists.
    let best = hits.first().map(|(r, _)| *r).unwrap_or(0);
    hits.into_iter().filter(|(r, _)| *r == best || *r < 2).map(|(_, e)| e).collect()
}

/// The object whose usages or name the cursor is about.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub kind: String,
    pub name: String,
    pub namespace: Option<String>,
    pub place: Place,
}

/// On an object's own name, or on a reference to it.
pub fn target_at(uri: &str, text: &str, line: usize) -> Option<Target> {
    let contents = scan(uri, text);
    let definition = contents
        .entries
        .iter()
        .find(|e| e.name_place.as_ref().is_some_and(|p| p.line == line))
        .and_then(|e| {
            Some(Target {
                kind: e.kind.clone(),
                name: e.name.clone(),
                namespace: e.namespace.clone(),
                place: e.name_place.clone()?,
            })
        });
    definition.or_else(|| {
        contents.usages.into_iter().find(|u| u.place.line == line).and_then(|u| match u.reference {
            Reference::Named { kind, name, namespace } => Some(Target { kind, name, namespace, place: u.place }),
            Reference::Selector { .. } => None,
        })
    })
}

impl Target {
    pub fn definitions<'a>(&self, all: &'a Contents) -> Vec<&'a Entry> {
        all.entries
            .iter()
            .filter(|e| e.kind == self.kind && e.name == self.name)
            .filter(|e| same_namespace(&self.kind, &self.namespace, &e.namespace))
            .collect()
    }

    /// References by name.
    pub fn named_usages<'a>(&self, all: &'a Contents) -> Vec<&'a Usage> {
        all.usages
            .iter()
            .filter(|u| match &u.reference {
                Reference::Named { kind, name, namespace } => {
                    kind == &self.kind && name == &self.name && same_namespace(kind, &self.namespace, namespace)
                }
                Reference::Selector { .. } => false,
            })
            .collect()
    }

    /// Selectors (of Services, policies, …) that pick this workload's pods.
    /// The workload's own selector isn't a usage.
    pub fn selector_usages<'a>(&self, all: &'a Contents) -> Vec<&'a Usage> {
        let definitions = self.definitions(all);
        all.usages
            .iter()
            .filter(|u| match &u.reference {
                Reference::Selector { labels, namespace } => definitions.iter().any(|d| {
                    selects(labels, d)
                        && same_namespace(&self.kind, namespace, &d.namespace)
                        && !(u.uri == d.uri && own_selector(all, u, d))
                }),
                Reference::Named { .. } => false,
            })
            .collect()
    }
}

/// Whether `usage` lies in the same document as `definition`: the first
/// object starting above the usage is the one it belongs to.
fn own_selector(all: &Contents, usage: &Usage, definition: &Entry) -> bool {
    all.entries
        .iter()
        .filter(|e| e.uri == usage.uri && e.line <= usage.place.line)
        .max_by_key(|e| e.line)
        .is_some_and(|e| e.line == definition.line)
}

/// Caches parsed workspace manifests by modification time.
#[derive(Default)]
pub struct Index {
    files: HashMap<PathBuf, (SystemTime, Contents)>,
    /// Last directory walk, reused briefly because diagnostics ask often.
    walked: Option<(Instant, Vec<PathBuf>)>,
}

const SKIP_DIRS: &[&str] = &["node_modules", "target", "vendor", "dist", "build"];
const MAX_FILES: usize = 5000;
const MAX_SIZE: u64 = 512 * 1024;
const WALK_TTL: Duration = Duration::from_secs(2);

impl Index {
    /// Everything in the manifests under `roots`, with open (possibly
    /// unsaved) documents taking precedence over their on-disk contents.
    pub fn collect(
        &mut self,
        roots: &[PathBuf],
        open: &HashMap<String, String>,
        to_uri: impl Fn(&Path) -> String,
    ) -> Contents {
        let files = match self.walked.take() {
            Some((at, files)) if at.elapsed() < WALK_TTL => (at, files),
            _ => {
                let mut files = Vec::new();
                for root in roots {
                    walk(root, &mut files);
                }
                (Instant::now(), files)
            }
        };
        let open_uris: HashSet<String> = open.keys().map(|u| comparable(u)).collect();
        let mut out = Contents::default();
        for path in &files.1 {
            let uri = to_uri(path);
            if open_uris.contains(&comparable(&uri)) {
                continue;
            }
            let Ok(mtime) = std::fs::metadata(path).and_then(|m| m.modified()) else { continue };
            let cached = self.files.get(path).filter(|(at, _)| *at == mtime);
            let contents = match cached {
                Some((_, c)) => c.clone(),
                None => {
                    let Ok(text) = std::fs::read_to_string(path) else { continue };
                    let c = scan(&uri, &text);
                    self.files.insert(path.clone(), (mtime, c.clone()));
                    c
                }
            };
            out.entries.extend(contents.entries);
            out.usages.extend(contents.usages);
        }
        for (uri, text) in open {
            let contents = scan(uri, text);
            out.entries.extend(contents.entries);
            out.usages.extend(contents.usages);
        }
        self.walked = Some(files);
        out
    }
}

/// Clients differ in how they write a drive letter (`C:`, `c:`, `c%3A`).
fn comparable(uri: &str) -> String {
    if cfg!(windows) {
        uri.replace("%3A", ":").replace("%3a", ":").to_lowercase()
    } else {
        uri.to_string()
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
        } else if (name.ends_with(".yaml") || name.ends_with(".yml") || name == "Kustomization")
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
    const URI: &str = "file:///x.yaml";

    fn line_of(src: &str, needle: &str, occurrence: usize) -> usize {
        yaml::lines(src).iter().enumerate().filter(|(_, l)| l.contains(needle)).nth(occurrence).unwrap().0
    }

    fn at(line_contains: &str, occurrence: usize) -> Option<Reference> {
        let lines = yaml::lines(DEPLOY);
        let docs = yaml::parse(&lines);
        let line = line_of(DEPLOY, line_contains, occurrence);
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
        let all = scan(URI, DEPLOY);
        let hits = resolve(&sel, &all.entries);
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].kind.as_str(), hits[0].name.as_str()), ("Deployment", "web"));
    }

    #[test]
    fn named_resolution_prefers_namespace() {
        let other = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: web-config\n  namespace: other\n---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: web-config\n  namespace: shop\n";
        let all = scan("file:///cm.yaml", other);
        let hits = resolve(&named("ConfigMap", "web-config", Some("shop")).unwrap(), &all.entries);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].namespace.as_deref(), Some("shop"));
        assert_eq!(hits[0].line, 9); // the second document's `name:` line
    }

    #[test]
    fn finds_usages_from_either_end() {
        let all = scan(URI, DEPLOY);
        // From the Deployment's name: the Service selects it, its own selector doesn't count.
        let t = target_at(URI, DEPLOY, line_of(DEPLOY, "name: web", 0)).unwrap();
        assert_eq!((t.kind.as_str(), t.name.as_str()), ("Deployment", "web"));
        let lines: Vec<usize> = t.selector_usages(&all).iter().map(|u| u.place.line).collect();
        assert_eq!(lines, [line_of(DEPLOY, "app: web", 2)]);
        // From a reference: every reference to the same ServiceAccount.
        let t = target_at(URI, DEPLOY, line_of(DEPLOY, "serviceAccountName", 0)).unwrap();
        let used: Vec<&str> = t.named_usages(&all).iter().map(|u| &u.place.text[u.place.start..u.place.end]).collect();
        assert_eq!(used, ["web-sa", "web-sa"]);
        assert!(target_at(URI, DEPLOY, line_of(DEPLOY, "image:", 0)).is_none());
    }

    #[test]
    fn inline_selectors() {
        let src = DEPLOY.replace("  selector:
    app: web
", "  selector: {app: web}
");
        let all = scan(URI, &src);
        let t = target_at(URI, &src, line_of(&src, "name: web", 0)).unwrap();
        let used = t.selector_usages(&all);
        assert_eq!(used.len(), 1);
        assert_eq!(used[0].place.line, line_of(&src, "selector: {", 0));
        assert_eq!(resolve(&used[0].reference, &all.entries)[0].name, "web");
    }

    #[test]
    fn other_namespaces_are_not_usages() {
        let src = "apiVersion: v1\nkind: Pod\nmetadata:\n  name: p\n  namespace: other\nspec:\n  serviceAccountName: web-sa\n";
        let mut all = scan(URI, DEPLOY);
        all.usages.extend(scan("file:///y.yaml", src).usages);
        let t = target_at(URI, DEPLOY, line_of(DEPLOY, "serviceAccountName", 0)).unwrap();
        assert!(t.named_usages(&all).iter().all(|u| u.uri == URI));
    }

    #[test]
    fn text_is_not_scanned() {
        let src = "apiVersion: v1\nkind: Pod\nmetadata:\n  name: p\nspec:\n  containers:\n    - args:\n        - |\n          spec:\n            claimName: fake\n";
        assert!(scan(URI, src).usages.is_empty());
    }

    #[test]
    fn knows_generated_objects() {
        let k = "configMapGenerator:\n  - name: web-config\n    literals: [a=b]\npatches:\n  - target:\n      kind: Deployment\n      name: web\n";
        let c = scan("file:///o/kustomization.yaml", k);
        assert_eq!((c.entries[0].kind.as_str(), c.entries[0].name.as_str()), ("ConfigMap", "web-config"));
        assert_eq!(c.usages.len(), 1);
        let cert = "apiVersion: cert-manager.io/v1\nkind: Certificate\nmetadata:\n  name: c\nspec:\n  secretName: tls\n";
        let c = scan(URI, cert);
        assert!(c.entries.iter().any(|e| e.kind == "Secret" && e.name == "tls"));
    }
}
