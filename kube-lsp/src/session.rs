//! The "active cluster" chosen at runtime, like JetBrains' per-session
//! kubeconfig selection.
//!
//! It is stored in a small JSON file so the language server and the MCP
//! server (separate processes) always agree on which cluster they talk to.
//! It overrides the kubeconfig/context from Zed settings until it is reset,
//! and it never modifies the user's own kubeconfig (no `use-context`).

use std::path::PathBuf;

use serde_json::{json, Value};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    pub kubeconfig: Option<String>,
    pub context: Option<String>,
}

impl Session {
    pub fn is_active(&self) -> bool {
        self.kubeconfig.is_some() || self.context.is_some()
    }

    /// e.g. `staging (from prod-clusters.yaml)`.
    pub fn describe(&self) -> String {
        let ctx = self.context.as_deref().unwrap_or("current-context");
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
    Session { kubeconfig: field("kubeconfig"), context: field("context") }
}

pub fn save(session: &Session) -> std::io::Result<()> {
    let path = session_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = json!({ "kubeconfig": session.kubeconfig, "context": session.context });
    std::fs::write(path, serde_json::to_string_pretty(&body).unwrap_or_default())
}

pub fn clear() -> std::io::Result<()> {
    // Pasted kubeconfigs hold credentials; drop our copy along with the session.
    if let Some(k) = load().kubeconfig {
        let path = PathBuf::from(k);
        if path.starts_with(state_dir().join("kubeconfigs")) {
            let _ = std::fs::remove_file(path);
        }
    }
    match std::fs::remove_file(session_file()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Persists a pasted (unsaved) kubeconfig so kubectl can read it. The file
/// holds credentials, so it is owner-only on Unix; on Windows the per-user
/// LocalAppData directory already restricts access.
pub fn store_kubeconfig(content: &str) -> std::io::Result<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut h);
    let dir = state_dir().join("kubeconfigs");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("pasted-{:016x}.yaml", h.finish()));

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    opts.open(&path)?.write_all(content.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let file = std::env::temp_dir().join(format!("kube-lsp-session-test-{}.json", std::process::id()));
        std::env::set_var("KUBE_LSP_SESSION_FILE", &file);
        let s = Session { kubeconfig: Some("/tmp/a.yaml".into()), context: Some("dev".into()) };
        save(&s).unwrap();
        assert_eq!(load(), s);
        assert_eq!(s.describe(), "dev (from a.yaml)");
        clear().unwrap();
        assert!(!load().is_active());
        std::env::remove_var("KUBE_LSP_SESSION_FILE");
    }
}
