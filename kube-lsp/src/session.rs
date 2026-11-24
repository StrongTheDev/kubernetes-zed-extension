//! The "active cluster" chosen at runtime, like JetBrains' per-session
//! kubeconfig selection.
//!
//! It is stored in a small JSON file so the language server and the MCP
//! server (separate processes) always agree on which cluster they talk to.
//! It overrides the kubeconfig/context from Zed settings until it is reset,
//! and it never modifies the user's own kubeconfig (no `use-context`).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    pub kubeconfig: Option<String>,
    pub context: Option<String>,
    pub namespace: Option<String>,
}

impl Session {
    pub fn is_active(&self) -> bool {
        self.kubeconfig.is_some() || self.context.is_some() || self.namespace.is_some()
    }

    /// e.g. `staging (from prod-clusters.yaml)`.
    pub fn describe(&self) -> String {
        let ctx = match &self.namespace {
            Some(ns) => format!("{}/{ns}", self.context.as_deref().unwrap_or("current-context")),
            None => self.context.clone().unwrap_or_else(|| "current-context".into()),
        };
        match &self.kubeconfig {
            Some(k) => format!(
                "{ctx} (from {})",
                std::path::Path::new(k).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| k.clone())
            ),
            None => ctx.to_string(),
        }
    }
}

pub fn state_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
    };
    base.unwrap_or_else(std::env::temp_dir).join("kube-lsp")
}

fn session_file() -> PathBuf {
    std::env::var_os("KUBE_LSP_SESSION_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir().join("session.json"))
}

pub fn load() -> Session {
    let Ok(text) = std::fs::read_to_string(session_file()) else { return Session::default() };
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let field = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
    Session { kubeconfig: field("kubeconfig"), context: field("context"), namespace: field("namespace") }
}

/// Creates or replaces a file only the current user can read.
pub fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    // On Windows the per-user LocalAppData directory restricts access.
    opts.open(path)?.write_all(contents)
}

fn pasted_dir() -> PathBuf {
    state_dir().join("kubeconfigs")
}

/// Deletes `kubeconfig` if it is our copy of a pasted one (it holds
/// credentials) and the session doesn't use it. The user's files are never touched.
pub fn discard_unused(kubeconfig: &str) {
    let path = Path::new(kubeconfig);
    if path.starts_with(pasted_dir()) && load().kubeconfig.as_deref() != Some(kubeconfig) {
        let _ = std::fs::remove_file(path);
    }
}

/// Removes leftovers of earlier runs: unused pasted kubeconfigs and
/// command output older than a day.
pub fn sweep() {
    const DAY: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
    let active = load().kubeconfig;
    for dir in [pasted_dir(), output_dir(), state_dir().join("buffers")] {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|m| m.elapsed().is_ok_and(|age| age > DAY));
            if old && active.as_deref().map(Path::new) != Some(path.as_path()) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Where command results (describe, logs, live YAML, …) are written.
pub fn output_dir() -> PathBuf {
    state_dir().join("output")
}

pub fn save(session: &Session) -> std::io::Result<()> {
    let path = session_file();
    let previous = load().kubeconfig;
    let body = json!({
        "kubeconfig": session.kubeconfig,
        "context": session.context,
        "namespace": session.namespace,
    });
    write_private(&path, serde_json::to_string_pretty(&body).unwrap_or_default().as_bytes())?;
    if let Some(old) = previous {
        discard_unused(&old);
    }
    Ok(())
}

pub fn clear() -> std::io::Result<()> {
    let previous = load().kubeconfig;
    match std::fs::remove_file(session_file()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    if let Some(old) = previous {
        discard_unused(&old);
    }
    Ok(())
}

/// Persists a pasted (unsaved) kubeconfig so kubectl can read it.
pub fn store_kubeconfig(content: &str) -> std::io::Result<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut h);
    let path = pasted_dir().join(format!("pasted-{:016x}.yaml", h.finish()));
    write_private(&path, content.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let file = std::env::temp_dir().join(format!("kube-lsp-session-test-{}.json", std::process::id()));
        std::env::set_var("KUBE_LSP_SESSION_FILE", &file);
        let s = Session { kubeconfig: Some("/tmp/a.yaml".into()), context: Some("dev".into()), namespace: None };
        save(&s).unwrap();
        assert_eq!(load(), s);
        assert_eq!(s.describe(), "dev (from a.yaml)");
        let s = Session { namespace: Some("shop".into()), ..s };
        save(&s).unwrap();
        assert_eq!(load().describe(), "dev/shop (from a.yaml)");
        // Pasted copies disappear when the session moves on; other files stay.
        let pasted = store_kubeconfig(&format!("test {}", std::process::id())).unwrap();
        let mine = std::env::temp_dir().join(format!("kube-lsp-mine-{}.yaml", std::process::id()));
        std::fs::write(&mine, "x").unwrap();
        let using = |p: &Path| Session { kubeconfig: Some(p.to_string_lossy().into_owned()), ..Default::default() };
        save(&using(&pasted)).unwrap();
        assert!(pasted.exists());
        save(&using(&mine)).unwrap();
        assert!(!pasted.exists());
        clear().unwrap();
        assert!(mine.exists());
        std::fs::remove_file(mine).unwrap();
        assert!(!load().is_active());
        std::env::remove_var("KUBE_LSP_SESSION_FILE");
    }
}
