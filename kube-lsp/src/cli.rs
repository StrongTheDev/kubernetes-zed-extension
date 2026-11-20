//! Interactive subcommands meant to be run as Zed tasks (they appear in the
//! command palette under `task: spawn`). They run in Zed's terminal, so they
//! can prompt; the selection is written to the shared session that the
//! language server and the MCP server read on every kubectl call.

use std::io::{BufRead, Write};

use crate::kubectl::Kubectl;
use crate::session::{self, Session};

/// `kube-lsp select-kubeconfig [PATH] [--context NAME]`
pub fn select_kubeconfig(kubectl: &Kubectl, path: Option<String>, context: Option<String>) -> Result<(), String> {
    let path = match path {
        Some(p) => p,
        None => match pick_file()? {
            Some(p) => p,
            None => {
                println!("No file selected; nothing changed.");
                return Ok(());
            }
        },
    };
    let path = std::fs::canonicalize(&path)
        .map_err(|e| format!("{path}: {e}"))?
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_string();

    let file = kubectl.for_kubeconfig(&path, None);
    let contexts = file.contexts().map_err(|e| format!("not a usable kubeconfig: {e}"))?;
    let default = file.run(&["config", "current-context"], None).ok().map(|c| c.trim().to_string());
    let context = match context {
        Some(c) if contexts.contains(&c) => c,
        Some(c) => return Err(format!("context {c:?} is not in {path}")),
        None => choose(&contexts, default.as_deref(), &format!("Contexts in {path}"))?,
    };
    activate(kubectl, Session { kubeconfig: Some(path), context: Some(context) })
}

/// `kube-lsp switch-context [NAME]` — within the active kubeconfig.
pub fn switch_context(kubectl: &Kubectl, context: Option<String>) -> Result<(), String> {
    let contexts = kubectl.contexts()?;
    let current = session::load().context;
    let context = match context {
        Some(c) if contexts.contains(&c) => c,
        Some(c) => return Err(format!("unknown context {c:?}")),
        None => choose(&contexts, current.as_deref(), "Contexts")?,
    };
    let kubeconfig = session::load().kubeconfig;
    activate(kubectl, Session { kubeconfig, context: Some(context) })
}

/// `kube-lsp reset-session`
pub fn reset(kubectl: &Kubectl) -> Result<(), String> {
    session::clear().map_err(|e| e.to_string())?;
    println!("Session cleared. Now using {}.", kubectl.current_context());
    Ok(())
}

/// `kube-lsp status`
pub fn status(kubectl: &Kubectl) -> Result<(), String> {
    let s = session::load();
    if s.is_active() {
        println!("Session: {}", s.describe());
    } else {
        println!("Session: none (using default kubeconfig)");
    }
    println!("Active:  {}", kubectl.current_context());
    match kubectl.server_version() {
        Ok(v) => println!("Server:  {v}"),
        Err(e) => println!("Server:  unreachable ({})", e.lines().next().unwrap_or_default()),
    }
    Ok(())
}

fn activate(kubectl: &Kubectl, new: Session) -> Result<(), String> {
    session::save(&new).map_err(|e| e.to_string())?;
    println!("Selected {}.", new.describe());
    match kubectl.server_version() {
        Ok(v) => println!("Connected (server {v}). Zed's Kubernetes actions and Agent tools now use this cluster."),
        Err(e) => println!(
            "Saved, but the cluster is not reachable right now:\n  {}",
            e.lines().next().unwrap_or_default()
        ),
    }
    Ok(())
}

/// Numbered terminal menu; Enter picks the default.
fn choose(options: &[String], default: Option<&str>, title: &str) -> Result<String, String> {
    match options {
        [] => return Err("no contexts defined".into()),
        [only] => return Ok(only.clone()),
        _ => {}
    }
    let default_idx = default.and_then(|d| options.iter().position(|o| o == d)).unwrap_or(0);
    println!("{title}:");
    for (i, o) in options.iter().enumerate() {
        println!("  {:>2}) {o}{}", i + 1, if i == default_idx { "  (default)" } else { "" });
    }
    let stdin = std::io::stdin();
    loop {
        print!("Select [1-{}, Enter = {}]: ", options.len(), default_idx + 1);
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            return Ok(options[default_idx].clone()); // no stdin: take default
        }
        let line = line.trim();
        if line.is_empty() {
            return Ok(options[default_idx].clone());
        }
        if let Some(o) = line.parse::<usize>().ok().and_then(|n| options.get(n.wrapping_sub(1))) {
            return Ok(o.clone());
        }
        if let Some(o) = options.iter().find(|o| *o == line) {
            return Ok(o.clone());
        }
        println!("Not an option: {line}");
    }
}

/// Opens the platform's native "open file" dialog, starting in `~/.kube`.
/// Returns `None` when the user cancels.
fn pick_file() -> Result<Option<String>, String> {
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
    let kube_dir = std::path::Path::new(&home).join(".kube");
    let start = if kube_dir.is_dir() { kube_dir } else { home.into() };
    let start = start.to_string_lossy().into_owned();

    let attempts: Vec<(&str, Vec<String>)> = if cfg!(windows) {
        // A TopMost owner keeps the dialog in front of Zed.
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms; \
             $o = New-Object System.Windows.Forms.Form -Property @{{TopMost=$true}}; \
             $d = New-Object System.Windows.Forms.OpenFileDialog; \
             $d.Title = 'Select kubeconfig'; \
             $d.InitialDirectory = '{}'; \
             $d.Filter = 'Kubeconfig (*.yaml;*.yml;config)|*.yaml;*.yml;config;*.conf|All files (*.*)|*.*'; \
             if ($d.ShowDialog($o) -eq 'OK') {{ $d.FileName }}",
            start.replace('\'', "''")
        );
        vec![("powershell.exe", vec!["-NoProfile".into(), "-STA".into(), "-Command".into(), script])]
    } else if cfg!(target_os = "macos") {
        let script = format!(
            "POSIX path of (choose file with prompt \"Select kubeconfig\" default location (POSIX file \"{}\"))",
            start.replace('"', "\\\"")
        );
        vec![("osascript", vec!["-e".into(), script])]
    } else {
        vec![
            ("zenity", vec!["--file-selection".into(), "--title=Select kubeconfig".into(), format!("--filename={start}/")]),
            ("kdialog", vec!["--getopenfilename".into(), start.clone()]),
        ]
    };

    for (program, args) in attempts {
        match std::process::Command::new(program).args(&args).output() {
            Ok(out) => {
                let chosen = String::from_utf8_lossy(&out.stdout).trim().to_string();
                // Non-zero exit with no output = user cancelled.
                return Ok((!chosen.is_empty()).then_some(chosen));
            }
            Err(_) => continue, // dialog tool not installed; try the next one
        }
    }

    // No GUI dialog available: ask in the terminal.
    print!("Path to kubeconfig: ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).map_err(|e| e.to_string())?;
    let line = line.trim();
    Ok((!line.is_empty()).then(|| line.to_string()))
}
