//! Turning documents into `kubectl … -f -` input.

use serde_json::Value;

use crate::yaml::Doc;

/// Documents that can go to kubectl in one call.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub manifest: String,
    /// `--namespace <default>` when it applies.
    pub namespace: Vec<String>,
}

/// Groups documents by whether the default namespace applies to them.
///
/// kubectl rejects a `--namespace` that contradicts a namespace written in
/// the manifest, so documents that pin another one need a separate call.
/// Order is kept (a Namespace must be applied before what goes into it).
pub fn batches(default_namespace: Option<&str>, lines: &[&str], docs: &[&Doc]) -> Vec<Batch> {
    let mut out: Vec<Batch> = Vec::new();
    for doc in docs {
        let text = doc.text(lines);
        let namespace = match default_namespace {
            Some(ns) if !pins_other_namespace(doc, &text, ns) => vec!["--namespace".to_string(), ns.to_string()],
            _ => Vec::new(),
        };
        match out.last_mut() {
            Some(last) if last.namespace == namespace => {
                last.manifest.push_str("\n---\n");
                last.manifest.push_str(&text);
            }
            _ => out.push(Batch { manifest: text, namespace }),
        }
    }
    out
}

fn pins_other_namespace(doc: &Doc, text: &str, default: &str) -> bool {
    if text.trim_start().starts_with('{') {
        // JSON manifest, possibly a List with namespaced items.
        let parsed = serde_json::from_str(text).unwrap_or(Value::Null);
        return parsed.get("items").is_some()
            || parsed["metadata"]["namespace"].as_str().is_some_and(|n| n != default);
    }
    if doc.kind.as_deref().is_some_and(|k| k.ends_with("List")) {
        return true; // items carry their own namespaces
    }
    doc.namespace.as_deref().is_some_and(|n| n != default)
}

/// Hides the values of a Secret printed by `kubectl get -o yaml`, keeping
/// its keys. Also drops the last-applied annotation, which repeats them.
pub fn redact_secret(yaml: &str) -> String {
    let mut out = String::new();
    let mut in_data = false;
    let mut dropping: Option<usize> = None;
    for line in yaml.lines() {
        let indent = line.len() - line.trim_start().len();
        if let Some(parent) = dropping {
            if line.trim().is_empty() || indent > parent {
                continue;
            }
            dropping = None;
        }
        if indent == 0 {
            in_data = matches!(line, "data:" | "stringData:");
        } else if in_data {
            let key = line.split(':').next().unwrap_or_default();
            out.push_str(&format!("{key}: <redacted>\n"));
            dropping = Some(indent);
            continue;
        } else if line.trim_start().starts_with("kubectl.kubernetes.io/last-applied-configuration:") {
            dropping = Some(indent);
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yaml;

    fn grouped(src: &str, default: Option<&str>) -> Vec<(usize, bool)> {
        let lines = yaml::lines(src);
        let docs = yaml::parse(&lines);
        batches(default, &lines, &docs.iter().collect::<Vec<_>>())
            .iter()
            .map(|b| (b.manifest.matches("kind:").count(), !b.namespace.is_empty()))
            .collect()
    }

    #[test]
    fn mixed_namespaces_keep_the_default_for_the_rest() {
        let src = "kind: A\n---\nkind: B\nmetadata:\n  namespace: other\n---\nkind: C\n---\nkind: D\nmetadata:\n  namespace: dev\n";
        assert_eq!(grouped(src, Some("dev")), [(1, true), (1, false), (2, true)]);
        assert_eq!(grouped(src, None), [(4, false)]);
        assert_eq!(grouped("kind: ConfigMapList\nitems: []\n", Some("dev")), [(1, false)]);
    }

    #[test]
    fn namespace_text_inside_data_is_not_a_pin() {
        // The old check looked for any line starting with `namespace:`.
        assert_eq!(grouped("kind: ConfigMap\ndata:\n  a: |\n    namespace: x\n", Some("dev")), [(1, true)]);
    }

    #[test]
    fn json_manifests() {
        assert!(!grouped("{\"kind\": \"A\", \"metadata\": {\"namespace\": \"x\"}}", Some("dev"))[0].1);
        assert!(grouped("{\"kind\": \"A\", \"metadata\": {}}", Some("dev"))[0].1);
    }

    #[test]
    fn secrets_are_redacted() {
        let src = "apiVersion: v1\ndata:\n  password: aHVudGVyMg==\n  long: |\n    c2VjcmV0\nkind: Secret\nmetadata:\n  annotations:\n    kubectl.kubernetes.io/last-applied-configuration: |\n      {\"data\":{\"password\":\"aHVudGVyMg==\"}}\n    keep: me\n  name: db\n";
        let got = redact_secret(src);
        assert!(!got.contains("aHVudGVyMg") && !got.contains("c2VjcmV0"), "{got}");
        assert!(got.contains("  password: <redacted>\n  long: <redacted>\nkind: Secret"));
        assert!(got.contains("keep: me") && got.contains("name: db"));
    }
}
