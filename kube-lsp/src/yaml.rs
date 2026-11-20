//! Minimal, position-preserving YAML analysis.
//!
//! A full YAML parser loses line/column information we need for hovers and
//! diagnostics, and Kubernetes manifests are overwhelmingly block-style, so an
//! indentation-based scanner is both simpler and more useful here.

#[derive(Debug, Clone, Default)]
pub struct Doc {
    /// First line of the document (after any `---`).
    pub start: usize,
    /// One past the last line.
    pub end: usize,
    pub api_version: Option<String>,
    pub kind: Option<String>,
    pub kind_line: Option<usize>,
    pub name: Option<String>,
    pub namespace: Option<String>,
}

impl Doc {
    pub fn is_resource(&self) -> bool {
        self.api_version.is_some() && self.kind.is_some()
    }

    /// Human label such as `Deployment/nginx`.
    pub fn label(&self) -> String {
        format!(
            "{}/{}",
            self.kind.as_deref().unwrap_or("?"),
            self.name.as_deref().unwrap_or("<unnamed>")
        )
    }

    pub fn text(&self, lines: &[&str]) -> String {
        lines[self.start..self.end].join("\n")
    }

    /// A kubeconfig file (`apiVersion: v1`, `kind: Config`). Never sent to a cluster.
    pub fn is_kubeconfig(&self) -> bool {
        self.kind.as_deref() == Some("Config") && self.api_version.as_deref() == Some("v1")
    }

    /// Whether the cluster can meaningfully validate/apply this document.
    pub fn is_cluster_object(&self, lines: &[&str]) -> bool {
        self.is_resource()
            && !self.is_kubeconfig()
            && !self
                .api_version
                .as_deref()
                .is_some_and(|v| v.starts_with("kustomize.config.k8s.io"))
            && !lines[self.start..self.end].iter().any(|l| l.contains("{{"))
    }
}

pub fn lines(text: &str) -> Vec<&str> {
    text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect()
}

fn is_separator(line: &str) -> bool {
    line == "---" || line.starts_with("--- ") || line.starts_with("---\t")
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    let s = s.split(" #").next().unwrap_or(s).trim();
    s.trim_matches(|c| c == '"' || c == '\'')
}

/// Parses a mapping-key line. Returns `(column of key, key, raw value)`.
/// List markers are skipped, so `  - name: x` yields `(4, "name", "x")`.
pub fn key_of(line: &str) -> Option<(usize, &str, &str)> {
    let mut col = line.len() - line.trim_start().len();
    let mut rest = &line[col..];
    if rest.starts_with('#') {
        return None;
    }
    while rest.starts_with("- ") || rest == "-" {
        let after = &rest[1..];
        let trimmed = after.trim_start();
        col += 1 + (after.len() - trimmed.len());
        rest = trimmed;
    }
    if rest.is_empty() || rest.starts_with(['{', '[', '#', '|', '>']) {
        return None;
    }

    let colon = if let Some(q) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') {
        let close = rest[1..].find(q)? + 1;
        (rest[close + 1..].starts_with(':')).then_some(close + 1)?
    } else {
        let bytes = rest.as_bytes();
        let mut found = None;
        for (i, &b) in bytes.iter().enumerate() {
            match b {
                b':' if i + 1 == bytes.len() || bytes[i + 1] == b' ' || bytes[i + 1] == b'\t' => {
                    found = Some(i);
                    break;
                }
                b' ' | b'\t' => return None,
                _ => {}
            }
        }
        found?
    };
    let key = rest[..colon].trim_matches(|c| c == '"' || c == '\'');
    Some((col, key, rest[colon + 1..].trim()))
}

/// Column where meaningful content starts (after indentation and `- `).
fn content_col(line: &str) -> usize {
    let mut col = line.len() - line.trim_start().len();
    let mut rest = line.trim_start();
    while let Some(after) = rest.strip_prefix('-') {
        if !(after.is_empty() || after.starts_with(' ')) {
            break;
        }
        let trimmed = after.trim_start();
        col += 1 + (after.len() - trimmed.len());
        rest = trimmed;
    }
    col
}

/// Splits a (possibly multi-document) YAML file and extracts the identifying
/// fields of each Kubernetes object.
pub fn parse(lines: &[&str]) -> Vec<Doc> {
    let mut docs = Vec::new();
    let mut start = 0;
    for i in 0..=lines.len() {
        if i == lines.len() || is_separator(lines[i]) {
            if i > start || i == lines.len() {
                docs.push(analyze(lines, start, i));
            }
            start = i + 1;
        }
    }
    docs.retain(|d| d.end > d.start);
    docs
}

fn analyze(lines: &[&str], start: usize, end: usize) -> Doc {
    let mut doc = Doc { start, end, ..Default::default() };
    let mut in_metadata = false;
    let mut meta_indent: Option<usize> = None;
    for (i, line) in lines.iter().enumerate().take(end).skip(start) {
        let Some((col, key, value)) = key_of(line) else { continue };
        if col == 0 && !line.starts_with('-') {
            in_metadata = key == "metadata";
            match key {
                "apiVersion" => doc.api_version = Some(unquote(value).to_string()),
                "kind" => {
                    doc.kind = Some(unquote(value).to_string());
                    doc.kind_line = Some(i);
                }
                _ => {}
            }
        } else if in_metadata {
            let indent = *meta_indent.get_or_insert(col);
            if col == indent {
                match key {
                    "name" => doc.name = Some(unquote(value).to_string()),
                    "namespace" => doc.namespace = Some(unquote(value).to_string()),
                    _ => {}
                }
            }
        }
    }
    doc
}

/// The chain of mapping keys enclosing `line`, e.g.
/// `["spec", "template", "spec", "containers", "image"]`.
pub fn key_path(lines: &[&str], doc_start: usize, line: usize) -> Vec<String> {
    let text = lines[line];
    if text.trim().is_empty() || text.trim_start().starts_with('#') {
        return Vec::new();
    }
    let (mut indent, mut path) = match key_of(text) {
        Some((col, key, _)) => (col, vec![key.to_string()]),
        None => (content_col(text), Vec::new()),
    };
    for i in (doc_start..line).rev() {
        if indent == 0 {
            break;
        }
        if let Some((col, key, _)) = key_of(lines[i]) {
            if col < indent {
                path.push(key.to_string());
                indent = col;
            }
        }
    }
    path.reverse();
    path
}

/// Finds the line declaring a dotted field path (as reported by the API
/// server, e.g. `spec.template.spec.containers[0].imagePullPolicyy`).
pub fn find_path_line(lines: &[&str], doc: &Doc, dotted: &str) -> Option<usize> {
    let wanted: Vec<String> = dotted
        .split('.')
        .map(|seg| seg.split('[').next().unwrap_or(seg).to_string())
        .filter(|s| !s.is_empty())
        .collect();
    (doc.start..doc.end)
        .find(|&i| key_of(lines[i]).is_some() && key_path(lines, doc.start, i) == wanted)
}

pub fn doc_at(docs: &[Doc], line: usize) -> Option<(usize, &Doc)> {
    docs.iter().enumerate().find(|(_, d)| line >= d.start && line < d.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\n  namespace: shop\n  labels:\n    name: not-this\nspec:\n  template:\n    spec:\n      containers:\n        - name: app\n          image: nginx:1.27\n          ports:\n            - containerPort: 80\n---\napiVersion: v1\nkind: Service\nmetadata:\n  name: \"web\"\n";

    #[test]
    fn splits_and_extracts_metadata() {
        let l = lines(SAMPLE);
        let docs = parse(&l);
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].label(), "Deployment/web");
        assert_eq!(docs[0].namespace.as_deref(), Some("shop"));
        assert_eq!(docs[0].api_version.as_deref(), Some("apps/v1"));
        assert_eq!(docs[1].label(), "Service/web");
        assert_eq!(docs[1].start, 16);
    }

    #[test]
    fn computes_key_paths() {
        let l = lines(SAMPLE);
        let path = |line| key_path(&l, 0, line).join(".");
        assert_eq!(path(12), "spec.template.spec.containers.image");
        assert_eq!(path(11), "spec.template.spec.containers.name");
        assert_eq!(path(14), "spec.template.spec.containers.ports.containerPort");
        assert_eq!(path(6), "metadata.labels.name");
        assert_eq!(path(1), "kind");
    }

    #[test]
    fn finds_server_reported_paths() {
        let l = lines(SAMPLE);
        let docs = parse(&l);
        assert_eq!(
            find_path_line(&l, &docs[0], "spec.template.spec.containers[0].ports[0].containerPort"),
            Some(14)
        );
    }

    #[test]
    fn key_parsing_edge_cases() {
        assert_eq!(key_of("image: nginx:1.2"), Some((0, "image", "nginx:1.2")));
        assert_eq!(key_of("- http://example.com"), None);
        assert_eq!(key_of("  \"a b\": 1"), Some((2, "a b", "1")));
        assert_eq!(key_of("# comment: no"), None);
    }
}
