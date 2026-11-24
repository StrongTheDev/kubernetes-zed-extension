//! Lets terminal tasks see unsaved edits.
//!
//! Tasks only get a file path and a line number, but the buffer may differ
//! from the file on disk. While it does, the language server keeps a small
//! outline of the buffer (which resource is on which lines, never its
//! contents) where `kube-lsp logs --file --line` can find it.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::session::{state_dir, write_private};
use crate::yaml;

/// What a task needs to know about the resource on a line.
#[derive(Debug, Clone, PartialEq)]
pub struct Resource {
    pub kind: Option<String>,
    pub name: Option<String>,
    pub namespace: Option<String>,
    /// `spec.selector.matchLabels`.
    pub labels: Vec<(String, String)>,
    pub start: usize,
    pub end: usize,
}

pub fn outline(text: &str) -> Vec<Resource> {
    let lines = yaml::lines(text);
    yaml::parse(&lines)
        .iter()
        .map(|d| Resource {
            kind: d.kind.clone(),
            name: d.name.clone(),
            namespace: d.namespace.clone(),
            // Only these have logs, and scanning every CRD would be slow.
            labels: match d.kind.as_deref() {
                Some(k) if crate::cli::WORKLOADS.contains(&k) => {
                    yaml::pairs_under(&lines, d, &["spec", "selector", "matchLabels"])
                }
                _ => Vec::new(),
            },
            start: d.start,
            end: d.end,
        })
        .collect()
}

fn store_path(file: &Path) -> PathBuf {
    let mut key = file.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        key = key.to_lowercase(); // drive letter case varies between callers
    }
    let mut h = DefaultHasher::new();
    key.hash(&mut h);
    state_dir().join("buffers").join(format!("{:016x}.json", h.finish()))
}

/// Records the outline of an edited buffer, or forgets it when the buffer
/// matches the file again.
pub fn record(file: &Path, text: &str) {
    let clean = std::fs::read_to_string(file).is_ok_and(|disk| yaml::lines(&disk) == yaml::lines(text));
    if clean {
        return forget(file);
    }
    let docs: Vec<Value> = outline(text)
        .iter()
        .map(|r| {
            json!({
                "kind": r.kind, "name": r.name, "namespace": r.namespace,
                "labels": r.labels, "start": r.start, "end": r.end
            })
        })
        .collect();
    let _ = write_private(&store_path(file), Value::Array(docs).to_string().as_bytes());
}

pub fn forget(file: &Path) {
    let _ = std::fs::remove_file(store_path(file));
}

/// The outline of `file` as the editor sees it, falling back to the file.
pub fn load(file: &Path) -> Result<Vec<Resource>, String> {
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let path = store_path(file);
    // Older than the file means it was left behind by a crash.
    if modified(&path).is_some_and(|m| modified(file).is_none_or(|f| m >= f)) {
        if let Some(found) = std::fs::read_to_string(&path).ok().and_then(|t| decode(&t)) {
            return Ok(found);
        }
    }
    std::fs::read_to_string(file).map(|t| outline(&t)).map_err(|e| format!("{}: {e}", file.display()))
}

fn decode(text: &str) -> Option<Vec<Resource>> {
    let field = |v: &Value, k: &str| v[k].as_str().map(String::from);
    serde_json::from_str::<Value>(text)
        .ok()?
        .as_array()?
        .iter()
        .map(|v| {
            Some(Resource {
                kind: field(v, "kind"),
                name: field(v, "name"),
                namespace: field(v, "namespace"),
                labels: v["labels"]
                    .as_array()?
                    .iter()
                    .map(|p| Some((p[0].as_str()?.to_string(), p[1].as_str()?.to_string())))
                    .collect::<Option<_>>()?,
                start: v["start"].as_u64()? as usize,
                end: v["end"].as_u64()? as usize,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsaved_buffer_wins_over_disk() {
        let dir = std::env::temp_dir().join(format!("kube-lsp-buffers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.yaml");
        let disk = "apiVersion: v1\nkind: Pod\nmetadata:\n  name: old\n";
        std::fs::write(&file, disk).unwrap();
        let edited = "# note\napiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: new\nspec:\n  selector:\n    matchLabels:\n      app: web\n";
        record(&file, edited);
        assert_eq!(load(&file).unwrap(), outline(edited));
        record(&file, disk);
        assert_eq!(load(&file).unwrap()[0].name.as_deref(), Some("old"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
