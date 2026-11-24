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
//!
//! Binary lookup order: the `binary`/`command` path from settings, then
//! `kube-lsp` on PATH (handy while developing), then a release asset
//! downloaded from GitHub into the extension's work directory.

use zed_extension_api::{
    self as zed,
    serde_json::{self, json},
    settings::{CommandSettings, ContextServerSettings, LspSettings},
    Command, ContextServerConfiguration, ContextServerId, LanguageServerId, Project, Result,
    Worktree,
};

const BINARY: &str = "kube-lsp";
const REPO: &str = "StrongTheDev/kubernetes-zed-extension";
const DOWNLOAD_DIR_PREFIX: &str = "kube-lsp-";

#[derive(Default)]
struct KubernetesExtension {
    /// Binary resolved for this session, shared by the LSP and MCP servers
    /// so both always run the same build.
    cached_binary: Option<String>,
}

fn missing_binary_message(download_error: &str) -> String {
    format!(
        "`{BINARY}` is not on PATH and could not be downloaded ({download_error}). Install it with \
         `cargo install --path kube-lsp` from the extension repository, or set \
         `lsp.kube-lsp.binary.path` / `context_servers.kubernetes.command.path` in your Zed settings."
    )
}

/// Release asset for this platform; must match `.github/workflows/kube-lsp.yml`.
fn asset_name() -> Result<(String, zed::DownloadedFileType, &'static str)> {
    let (os, arch) = zed::current_platform();
    let arch = match arch {
        zed::Architecture::Aarch64 => "aarch64",
        zed::Architecture::X8664 => "x86_64",
        zed::Architecture::X86 => return Err("32-bit x86 is not supported".into()),
    };
    Ok(match os {
        zed::Os::Mac => (format!("{BINARY}-{arch}-apple-darwin.tar.gz"), zed::DownloadedFileType::GzipTar, BINARY),
        zed::Os::Linux => (format!("{BINARY}-{arch}-unknown-linux-musl.tar.gz"), zed::DownloadedFileType::GzipTar, BINARY),
        zed::Os::Windows => (format!("{BINARY}-{arch}-pc-windows-msvc.zip"), zed::DownloadedFileType::Zip, "kube-lsp.exe"),
    })
}

impl KubernetesExtension {
    /// Returns an absolute path to a downloaded `kube-lsp`, fetching the
    /// latest release when it is not present yet.
    fn download(&mut self, language_server_id: Option<&LanguageServerId>) -> Result<String> {
        if let Some(path) = &self.cached_binary {
            if std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
                return Ok(path.clone());
            }
        }
        let status = |s: zed::LanguageServerInstallationStatus| {
            if let Some(id) = language_server_id {
                zed::set_language_server_installation_status(id, &s);
            }
        };
        status(zed::LanguageServerInstallationStatus::CheckingForUpdate);
        let (asset_name, file_type, exe) = asset_name()?;
        let release = zed::latest_github_release(
            REPO,
            zed::GithubReleaseOptions { require_assets: true, pre_release: false },
        )?;
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == asset_name)
            .ok_or_else(|| format!("release {} has no asset {asset_name}", release.version))?;

        let dir = format!("{DOWNLOAD_DIR_PREFIX}{}", release.version);
        let relative = format!("{dir}/{exe}");
        if !std::fs::metadata(&relative).is_ok_and(|m| m.is_file()) {
            status(zed::LanguageServerInstallationStatus::Downloading);
            zed::download_file(&asset.download_url, &dir, file_type)
                .map_err(|e| format!("downloading {asset_name}: {e}"))?;
            zed::make_file_executable(&relative)?;
            // Keep only the current version.
            if let Ok(entries) = std::fs::read_dir(".") {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with(DOWNLOAD_DIR_PREFIX) && name != dir {
                        let _ = std::fs::remove_dir_all(entry.path());
                    }
                }
            }
        }
        status(zed::LanguageServerInstallationStatus::None);

        // Zed starts the process from elsewhere, so hand it an absolute path.
        let absolute = std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(&relative)
            .to_string_lossy()
            .into_owned();
        self.cached_binary = Some(absolute.clone());
        Ok(absolute)
    }

    /// PATH first, then the cached/downloaded binary.
    fn locate(&mut self, on_path: Option<String>, language_server_id: Option<&LanguageServerId>) -> Result<String> {
        if let Some(path) = on_path {
            self.cached_binary = Some(path.clone());
            return Ok(path);
        }
        self.download(language_server_id).map_err(|e| missing_binary_message(&e))
    }
}

/// Applies user-provided `binary`/`command` settings on top of defaults.
fn resolve(
    configured: Option<CommandSettings>,
    default_path: impl FnOnce() -> Result<String>,
    default_args: Vec<String>,
    mut env: Vec<(String, String)>,
) -> Result<Command> {
    let (path, args, extra_env) = match configured {
        Some(c) => (c.path, c.arguments, c.env),
        None => (None, None, None),
    };
    let command = match path {
        Some(p) => p,
        None => default_path()?,
    };
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
        Self::default()
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command> {
        let settings = LspSettings::for_worktree(language_server_id.as_ref(), worktree)
            .unwrap_or_default();
        // The shell env carries KUBECONFIG, cloud-provider auth helpers, etc.
        let on_path = worktree.which(BINARY);
        resolve(
            settings.binary,
            || self.locate(on_path, Some(language_server_id)),
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
        for (key, flag) in [
            ("context", "--context"),
            ("kubeconfig", "--kubeconfig"),
            ("namespace", "--namespace"),
            ("kubectl", "--kubectl"),
        ] {
            if let Some(v) = opts.get(key).and_then(|v| v.as_str()).filter(|v| !v.is_empty()) {
                args.push(flag.into());
                args.push(v.into());
            }
        }
        // No worktree here to search PATH with: reuse the binary the language
        // server resolved, else download it, else hope it is on PATH.
        resolve(
            settings.command,
            || Ok(self.cached_binary.clone().or_else(|| self.download(None).ok()).unwrap_or_else(|| BINARY.into())),
            args,
            Vec::new(),
        )
    }

    fn context_server_configuration(
        &mut self,
        _context_server_id: &ContextServerId,
        _project: &Project,
    ) -> Result<Option<ContextServerConfiguration>> {
        Ok(Some(ContextServerConfiguration {
            installation_instructions: format!(
                "The `kube-lsp` helper is downloaded automatically from GitHub releases \
                 (or put your own build on PATH). Make sure `kubectl` works in your shell:\n\n\
                 ```sh\nkubectl config current-context\n```\n\n{}",
                "Set `read_only` to `true` to hide the apply/delete/scale/restart tools."
            ),
            settings_schema: serde_json::to_string(&json!({
                "type": "object",
                "properties": {
                    "read_only": { "type": "boolean", "description": "Only expose read-only tools." },
                    "context": { "type": "string", "description": "kubeconfig context to use (default: current-context)." },
                    "kubeconfig": { "type": "string", "description": "Path to a kubeconfig file." },
                    "namespace": { "type": "string", "description": "Default namespace (default: the context's)." },
                    "kubectl": { "type": "string", "description": "Path to the kubectl binary." }
                }
            }))
            .map_err(|e| e.to_string())?,
            default_settings: "{\n  \"read_only\": false,\n  \"context\": \"\"\n}".into(),
        }))
    }
}

zed::register_extension!(KubernetesExtension);
