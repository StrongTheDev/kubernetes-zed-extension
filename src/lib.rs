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
//! Binary lookup order: the `binary`/`command` path from settings (handy
//! while developing), then the latest release downloaded from GitHub into the
//! extension's work directory, then `kube-lsp` on PATH.

use zed_extension_api::{
    self as zed,
    serde_json::{self, json},
    settings::{CommandSettings, ContextServerSettings, LspSettings},
    Command, ContextServerConfiguration, ContextServerId, LanguageServerId, Project, Result,
    Worktree,
};

const BINARY: &str = "kube-lsp";
const REPO: &str = "StrongTheDev/kubernetes-zed-extension";
/// Release tags, which are also the download directories.
const RELEASE_PREFIX: &str = "kube-lsp-v";

#[derive(Default)]
struct KubernetesExtension {
    /// Release in use since Zed started, relative to the work directory.
    downloaded: Option<String>,
    /// `kube-lsp` from the PATH of the last worktree; the MCP server has no
    /// worktree to search with.
    on_path: Option<String>,
}

fn missing_binary_message(download_error: &str) -> String {
    format!(
        "`{BINARY}` could not be downloaded ({download_error}) and is not on PATH. Install it with \
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

/// `kube-lsp-v1.2.3` → `[1, 2, 3]`.
fn version_of(dir: &str) -> Option<Vec<u32>> {
    dir.strip_prefix(RELEASE_PREFIX)?.split('.').map(|part| part.parse().ok()).collect()
}

fn is_file(path: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

/// Releases downloaded earlier, newest first, as paths relative to the
/// extension's work directory.
fn installed(exe: &str) -> Vec<String> {
    let mut found: Vec<(Vec<u32>, String)> = std::fs::read_dir(".")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let dir = entry.file_name().to_string_lossy().into_owned();
            let path = format!("{dir}/{exe}");
            Some((version_of(&dir).filter(|_| is_file(&path))?, path))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|(_, path)| path).collect()
}

impl KubernetesExtension {
    /// The latest released `kube-lsp`, downloaded when it isn't there yet.
    ///
    /// The path is relative on purpose: Zed resolves commands against the
    /// extension's work directory, and an absolute path seen from WASM
    /// (`/C:/…`) is not one Windows understands.
    fn latest_release(&self, language_server_id: Option<&LanguageServerId>) -> Result<String> {
        let status = |s: zed::LanguageServerInstallationStatus| {
            if let Some(id) = language_server_id {
                zed::set_language_server_installation_status(id, &s);
            }
        };
        let (asset_name, file_type, exe) = asset_name()?;
        status(zed::LanguageServerInstallationStatus::CheckingForUpdate);
        let result = (|| {
            let release = zed::latest_github_release(
                REPO,
                zed::GithubReleaseOptions { require_assets: true, pre_release: false },
            )?;
            let dir = release.version.clone();
            if version_of(&dir).is_none() {
                return Err(format!("unexpected release name {dir}"));
            }
            let path = format!("{dir}/{exe}");
            if is_file(&path) {
                return Ok(path);
            }
            let asset = release
                .assets
                .iter()
                .find(|a| a.name == asset_name)
                .ok_or_else(|| format!("release {dir} has no asset {asset_name}"))?;
            status(zed::LanguageServerInstallationStatus::Downloading);
            zed::download_file(&asset.download_url, &dir, file_type)
                .map_err(|e| format!("downloading {asset_name}: {e}"))?;
            zed::make_file_executable(&path)?;
            for old in installed(exe).iter().filter(|p| **p != path) {
                if let Some((dir, _)) = old.split_once('/') {
                    let _ = std::fs::remove_dir_all(dir);
                }
            }
            Ok(path)
        })();
        status(zed::LanguageServerInstallationStatus::None);
        // Offline or rate limited: an earlier download is better than nothing.
        result.or_else(|e| installed(exe).into_iter().next().ok_or(e))
    }

    /// The downloaded release, else `kube-lsp` from PATH. Both servers go
    /// through here, so they always run the same build.
    fn binary(&mut self, on_path: Option<String>, language_server_id: Option<&LanguageServerId>) -> Result<String> {
        if on_path.is_some() {
            self.on_path = on_path;
        }
        if let Some(path) = self.downloaded.as_ref().filter(|p| is_file(p)) {
            return Ok(path.clone());
        }
        match self.latest_release(language_server_id) {
            Ok(path) => {
                self.downloaded = Some(path.clone());
                Ok(path)
            }
            Err(e) => self.on_path.clone().ok_or_else(|| missing_binary_message(&e)),
        }
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
            || self.binary(on_path, Some(language_server_id)),
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
        resolve(
            settings.command,
            || self.binary(None, None),
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
                 and updated when Zed starts. Make sure `kubectl` works in your shell:\n\n\
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
