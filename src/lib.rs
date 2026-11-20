//! Zed side of the Kubernetes extension.
//!
//! Zed extensions run as sandboxed WASM and cannot draw UI or talk to a
//! cluster themselves, so this crate only *locates and launches* the native
//! `kube-lsp` binary in two roles:
//!
//! * `kube-lsp lsp` — a language server attached to YAML files (hover docs,
//!   code actions, diagnostics, completions, outline).
//! * `kube-lsp mcp` — a Model Context Protocol server that gives Zed's Agent
//!   panel tools to inspect and operate the cluster.

use zed_extension_api::{
    self as zed,
    serde_json::{self, json},
    settings::{CommandSettings, ContextServerSettings, LspSettings},
    Command, ContextServerConfiguration, ContextServerId, LanguageServerId, Project, Result,
    Worktree,
};

const BINARY: &str = "kube-lsp";

struct KubernetesExtension;

fn missing_binary_message() -> String {
    format!(
        "`{BINARY}` was not found on PATH. Build it with \
         `cargo install --path kube-lsp` from the extension repository, or set \
         `lsp.kube-lsp.binary.path` / `context_servers.kubernetes.command.path` in your Zed settings."
    )
}

/// Applies user-provided `binary`/`command` settings on top of defaults.
fn resolve(
    configured: Option<CommandSettings>,
    default_path: Option<String>,
    default_args: Vec<String>,
    mut env: Vec<(String, String)>,
) -> Result<Command> {
    let (path, args, extra_env) = match configured {
        Some(c) => (c.path, c.arguments, c.env),
        None => (None, None, None),
    };
    let command = path.or(default_path).ok_or_else(missing_binary_message)?;
    if let Some(extra) = extra_env {
        env.extend(extra);
    }
    Ok(Command {
        command,
        args: args.unwrap_or(default_args),
        env,
    })
}

impl zed::Extension for KubernetesExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command> {
        let settings = LspSettings::for_worktree(language_server_id.as_ref(), worktree)
            .unwrap_or_default();
        // The shell env carries KUBECONFIG, cloud-provider auth helpers, etc.
        resolve(
            settings.binary,
            worktree.which(BINARY),
            vec!["lsp".into()],
            worktree.shell_env(),
        )
    }

    fn language_server_initialization_options(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Option<serde_json::Value>> {
        let settings = LspSettings::for_worktree(language_server_id.as_ref(), worktree)
            .unwrap_or_default();
        Ok(settings.initialization_options.or(settings.settings))
    }

    fn context_server_command(
        &mut self,
        context_server_id: &ContextServerId,
        project: &Project,
    ) -> Result<Command> {
        let settings = ContextServerSettings::for_project(context_server_id.as_ref(), project)
            .unwrap_or_default();
        let opts = settings.settings.unwrap_or_else(|| json!({}));

        let mut args = vec!["mcp".to_string()];
        if opts.get("read_only").and_then(|v| v.as_bool()).unwrap_or(false) {
            args.push("--read-only".into());
        }
        for (key, flag) in [("context", "--context"), ("kubeconfig", "--kubeconfig"), ("kubectl", "--kubectl")] {
            if let Some(v) = opts.get(key).and_then(|v| v.as_str()).filter(|v| !v.is_empty()) {
                args.push(flag.into());
                args.push(v.into());
            }
        }
        // No worktree here, so fall back to a bare name resolved through PATH.
        resolve(settings.command, Some(BINARY.into()), args, Vec::new())
    }

    fn context_server_configuration(
        &mut self,
        _context_server_id: &ContextServerId,
        _project: &Project,
    ) -> Result<Option<ContextServerConfiguration>> {
        Ok(Some(ContextServerConfiguration {
            installation_instructions: format!(
                "Install the native helper and make sure `kubectl` works in your shell:\n\n\
                 ```sh\ncargo install --path kube-lsp\nkubectl config current-context\n```\n\n{}",
                "Set `read_only` to `true` to hide the apply/delete/scale/restart tools."
            ),
            settings_schema: serde_json::to_string(&json!({
                "type": "object",
                "properties": {
                    "read_only": { "type": "boolean", "description": "Only expose read-only tools." },
                    "context": { "type": "string", "description": "kubeconfig context to use (default: current-context)." },
                    "kubeconfig": { "type": "string", "description": "Path to a kubeconfig file." },
                    "kubectl": { "type": "string", "description": "Path to the kubectl binary." }
                }
            }))
            .map_err(|e| e.to_string())?,
            default_settings: "{\n  \"read_only\": false,\n  \"context\": \"\"\n}".into(),
        }))
    }
}

zed::register_extension!(KubernetesExtension);
