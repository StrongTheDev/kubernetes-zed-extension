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
    activate(kubectl, Session { kubeconfig: Some(path), context: Some(context), namespace: None })
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
    activate(kubectl, Session { kubeconfig, context: Some(context), namespace: None })
}

/// `kube-lsp set-namespace [NAME]` — `-` clears it (use the context's default).
pub fn set_namespace(kubectl: &Kubectl, name: Option<String>) -> Result<(), String> {
    const CONTEXT_DEFAULT: &str = "(use the context's default)";
    let chosen = match name {
        Some(n) if n == "-" => CONTEXT_DEFAULT.to_string(),
        Some(n) => n,
        None => {
            let mut names: Vec<String> = kubectl
                .run(&["get", "namespaces", "-o", "name"], None)?
                .lines()
                .filter_map(|l| l.trim().strip_prefix("namespace/"))
                .map(String::from)
                .collect();
            names.insert(0, CONTEXT_DEFAULT.into());
            let current = kubectl.effective_namespace();
            choose(&names, Some(current.as_str()), &format!("Namespaces in {}", kubectl.current_context()))?
        }
    };
    let mut s = session::load();
    s.namespace = (chosen != CONTEXT_DEFAULT).then_some(chosen);
    session::save(&s).map_err(|e| e.to_string())?;
    println!("Default namespace is now {}.", kubectl.effective_namespace());
    Ok(())
}

pub struct LogsOptions {
    pub target: Option<String>,
    pub file: Option<String>,
    pub line: Option<usize>,
    pub namespace: Option<String>,
    pub container: Option<String>,
    pub tail: u32,
}

/// `kube-lsp logs` — follows logs in the terminal until Ctrl+C.
///
/// The target comes from (in order) `--file/--line` (the resource under the
/// cursor, via Zed task variables), a positional `pod` / `kind/name`, or an
/// interactive menu of pods and workloads.
pub fn logs(kubectl: &Kubectl, opts: LogsOptions) -> Result<(), String> {
    let (selector, namespace, label) = if let (Some(file), Some(line)) = (&opts.file, opts.line) {
        target_from_file(file, line)?
    } else if let Some(t) = &opts.target {
        (vec![crate::kubectl::safe_arg(t, "target")?], opts.namespace.clone(), t.clone())
    } else {
        pick_workload(kubectl, opts.namespace.as_deref())?
    };
    let namespace = namespace.or(opts.namespace);

    let mut args: Vec<String> = vec!["logs".into()];
    args.extend(selector);
    args.extend([
        "--follow".into(),
        format!("--tail={}", opts.tail),
        "--prefix".into(),
        "--max-log-requests=20".into(),
    ]);
    match &opts.container {
        Some(c) => args.extend(["--container".into(), crate::kubectl::safe_arg(c, "container")?]),
        None => args.push("--all-containers=true".into()),
    }
    if let Some(ns) = &namespace {
        args.extend(["--namespace".into(), crate::kubectl::safe_arg(ns, "namespace")?]);
    }
    let ns_label = namespace.unwrap_or_else(|| kubectl.effective_namespace());
    println!("Following logs of {label} in {ns_label} ({}). Ctrl+C to stop.\n", kubectl.current_context());
    let code = kubectl.exec_interactive(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
    if code != 0 {
        return Err(format!("kubectl exited with status {code}"));
    }
    Ok(())
}

pub const WORKLOADS: &[&str] = &["Deployment", "StatefulSet", "DaemonSet", "ReplicaSet", "Job"];

/// `(kubectl logs selector args, namespace, label)` for the resource at
/// 1-based `line` of a manifest file.
fn target_from_file(file: &str, line: usize) -> Result<(Vec<String>, Option<String>, String), String> {
    let resources = crate::buffers::load(std::path::Path::new(file))?;
    let line = line.saturating_sub(1);
    let doc =
        resources.into_iter().find(|r| (r.start..r.end).contains(&line)).ok_or("the cursor is not inside a resource")?;
    let kind = doc.kind.clone().ok_or("the resource under the cursor has no kind")?;
    let name = doc.name.clone().ok_or("the resource under the cursor has no metadata.name")?;
    let label = format!("{kind}/{name}");
    let selector = if kind == "Pod" {
        vec![format!("pod/{}", crate::kubectl::safe_arg(&name, "name")?)]
    } else if WORKLOADS.contains(&kind.as_str()) {
        // A label selector follows every pod, not just the one kubectl would pick.
        let labels = &doc.labels;
        if labels.is_empty() {
            vec![format!("{}/{name}", kind.to_lowercase())]
        } else {
            vec!["--selector".into(), labels.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",")]
        }
    } else {
        return Err(format!("{label} has no logs; put the cursor on a Pod or workload ({})", WORKLOADS.join(", ")));
    };
    Ok((selector, doc.namespace.clone(), label))
}

fn pick_workload(kubectl: &Kubectl, namespace: Option<&str>) -> Result<(Vec<String>, Option<String>, String), String> {
    let mut args = vec!["get", "pods,deployments,statefulsets,daemonsets,jobs", "-o", "name"];
    if let Some(ns) = namespace {
        args.extend(["--namespace", ns]);
    }
    let names: Vec<String> = kubectl.run(&args, None)?.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    if names.is_empty() {
        return Err(format!("no pods or workloads in namespace {}", namespace.map(String::from).unwrap_or_else(|| kubectl.effective_namespace())));
    }
    let chosen = choose(&names, None, &format!("Workloads in {}", kubectl.current_context()))?;
    if chosen.starts_with("pod/") {
        return Ok((vec![chosen.clone()], namespace.map(String::from), chosen));
    }
    // Follow all of a workload's pods via its selector.
    let mut sel_args = vec!["get", chosen.as_str(), "-o", "jsonpath={.spec.selector.matchLabels}"];
    if let Some(ns) = namespace {
        sel_args.extend(["--namespace", ns]);
    }
    let labels: Option<String> = kubectl
        .run(&sel_args, None)
        .ok()
        .and_then(|j| serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&j).ok())
        .map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|v| format!("{k}={v}"))).collect::<Vec<_>>().join(","))
        .filter(|s| !s.is_empty());
    let selector = match labels {
        Some(l) => vec!["--selector".into(), l],
        None => vec![chosen.clone()],
    };
    Ok((selector, namespace.map(String::from), chosen))
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
    println!("Namespace: {}", kubectl.effective_namespace());
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
