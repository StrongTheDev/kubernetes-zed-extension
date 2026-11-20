//! Thin wrapper around the `kubectl` CLI.
//!
//! Shelling out (rather than linking a Kubernetes client) means every auth
//! plugin, kubeconfig layout and cloud provider that works in the user's
//! terminal also works here, with zero extra configuration.

use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Clone, Debug)]
pub struct Kubectl {
    pub bin: String,
    pub context: Option<String>,
    pub kubeconfig: Option<String>,
    /// When false (the default) the runtime session selection overrides
    /// `context`/`kubeconfig`; see [`crate::session`].
    pub ignore_session: bool,
}

impl Default for Kubectl {
    fn default() -> Self {
        Self {
            bin: std::env::var("KUBECTL").unwrap_or_else(|_| "kubectl".into()),
            context: None,
            kubeconfig: None,
            ignore_session: false,
        }
    }
}

pub struct Output {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// stdout on success, otherwise the most useful error text.
    pub fn into_result(self) -> Result<String, String> {
        if self.success {
            Ok(self.stdout)
        } else {
            Err(self.error_text())
        }
    }

    pub fn error_text(&self) -> String {
        let err = self.stderr.trim();
        if err.is_empty() {
            self.stdout.trim().to_string()
        } else {
            err.to_string()
        }
    }
}

/// Rejects values that kubectl would interpret as flags.
pub fn safe_arg(value: &str, what: &str) -> Result<String, String> {
    if value.is_empty() || value.starts_with('-') {
        Err(format!("invalid {what}: {value:?}"))
    } else {
        Ok(value.to_string())
    }
}

/// True when an error just means "no reachable cluster", which we treat as
/// "skip cluster features" rather than as a problem with the user's YAML.
pub fn is_connectivity_error(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    [
        "unable to connect",
        "connection refused",
        "no configuration has been provided",
        "dial tcp",
        "i/o timeout",
        "context deadline exceeded",
        "failed to run",
        "the server has asked for the client to provide credentials",
    ]
    .iter()
    .any(|needle| e.contains(needle))
}

impl Kubectl {
    /// A kubectl bound to one specific kubeconfig file, ignoring the session.
    pub fn for_kubeconfig(&self, path: &str, context: Option<String>) -> Self {
        Self {
            bin: self.bin.clone(),
            kubeconfig: Some(path.to_string()),
            context,
            ignore_session: true,
        }
    }

    /// Effective `(kubeconfig, context)`: a session kubeconfig replaces the
    /// configured one wholesale (its contexts are unrelated); a session
    /// context alone just overrides the context.
    pub fn target(&self) -> (Option<String>, Option<String>) {
        if self.ignore_session {
            return (self.kubeconfig.clone(), self.context.clone());
        }
        let s = crate::session::load();
        if s.kubeconfig.is_some() {
            (s.kubeconfig, s.context)
        } else {
            (self.kubeconfig.clone(), s.context.or_else(|| self.context.clone()))
        }
    }

    /// Stable identity of the cluster we talk to, for cache keys.
    pub fn identity(&self) -> String {
        let (k, c) = self.target();
        format!("{}|{}", k.unwrap_or_default(), c.unwrap_or_default())
    }

    pub fn exec(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let (kubeconfig, context) = self.target();
        let mut cmd = Command::new(&self.bin);
        if let Some(c) = &context {
            cmd.arg("--context").arg(c);
        }
        if let Some(k) = &kubeconfig {
            cmd.arg("--kubeconfig").arg(k);
        }
        cmd.arg("--request-timeout=15s")
            .args(args)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Output {
                    success: false,
                    code: None,
                    stdout: String::new(),
                    stderr: format!("failed to run `{}`: {e}", self.bin),
                }
            }
        };
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            let input = input.to_owned();
            // Write on another thread so a large manifest can't deadlock us
            // against kubectl's own stdout buffer.
            std::thread::spawn(move || {
                let _ = pipe.write_all(input.as_bytes());
            });
        }
        match child.wait_with_output() {
            Ok(out) => Output {
                success: out.status.success(),
                code: out.status.code(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            },
            Err(e) => Output {
                success: false,
                code: None,
                stdout: String::new(),
                stderr: e.to_string(),
            },
        }
    }

    pub fn run(&self, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
        self.exec(args, stdin).into_result()
    }

    /// Context name, plus the kubeconfig file when it isn't the default one.
    pub fn current_context(&self) -> String {
        let (kubeconfig, context) = self.target();
        let ctx = context.unwrap_or_else(|| {
            self.run(&["config", "current-context"], None)
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "<no context>".into())
        });
        match kubeconfig {
            Some(k) => format!(
                "{ctx} @ {}",
                std::path::Path::new(&k).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or(k)
            ),
            None => ctx,
        }
    }

    /// Context names defined in the effective kubeconfig (local, no network).
    pub fn contexts(&self) -> Result<Vec<String>, String> {
        Ok(self
            .run(&["config", "get-contexts", "-o", "name"], None)?
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect())
    }

    /// Server version string, used to confirm a connection works.
    pub fn server_version(&self) -> Result<String, String> {
        let text = self.run(&["version", "-o", "json"], None)?;
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        v["serverVersion"]["gitVersion"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| "cluster did not report a version".into())
    }

    /// `(kind, apiVersion)` pairs from the cluster's discovery API.
    pub fn api_resources(&self) -> Result<Vec<(String, String)>, String> {
        let out = self.run(&["api-resources", "--no-headers"], None)?;
        // Columns: NAME [SHORTNAMES] APIVERSION NAMESPACED KIND
        Ok(out
            .lines()
            .filter_map(|line| {
                let cols: Vec<&str> = line.split_whitespace().collect();
                (cols.len() >= 4).then(|| {
                    (cols[cols.len() - 1].to_string(), cols[cols.len() - 3].to_string())
                })
            })
            .collect())
    }
}
