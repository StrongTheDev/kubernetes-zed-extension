mod buffers;
mod checks;
mod cli;
mod cluster;
mod install;
mod kubectl;
mod lsp;
mod manifest;
mod mcp;
mod refs;
mod schema;
mod session;
mod tree;
mod yaml;

use kubectl::Kubectl;

const USAGE: &str = "\
kube-lsp — Kubernetes tooling for Zed

USAGE:
    kube-lsp lsp [--context NAME] [--kubeconfig PATH] [--namespace NS] [--kubectl PATH]
    kube-lsp mcp [--read-only] [--context NAME] [--kubeconfig PATH] [--namespace NS] [--kubectl PATH]

  Session commands (run them as Zed tasks; they affect the editor and Agent):
    kube-lsp select-kubeconfig [PATH] [--context NAME]   no PATH opens a file picker
    kube-lsp switch-context [NAME]
    kube-lsp set-namespace [NAME | -]                     `-` = the context's default
    kube-lsp reset-session
    kube-lsp status
    kube-lsp install-tasks                                add these commands to Zed's tasks

  Logs (streams until Ctrl+C):
    kube-lsp logs [POD | KIND/NAME] [--namespace NS] [--container C] [--tail N]
    kube-lsp logs --file PATH --line N      resource under the cursor ($ZED_FILE/$ZED_ROW)
";

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "lsp".into());
    let mut kubectl = Kubectl::default();
    let mut read_only = false;
    let mut positional: Option<String> = None;
    let mut namespace: Option<String> = None;
    let mut logs = cli::LogsOptions { target: None, file: None, line: None, namespace: None, container: None, tail: 200 };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--read-only" => read_only = true,
            "--context" => kubectl.context = args.next(),
            "--kubeconfig" => kubectl.kubeconfig = args.next(),
            "--kubectl" => kubectl.bin = args.next().unwrap_or(kubectl.bin),
            "--namespace" | "-n" => namespace = args.next(),
            "--file" => logs.file = args.next(),
            "--line" => logs.line = args.next().and_then(|l| l.parse().ok()),
            "--container" | "-c" => logs.container = args.next(),
            "--tail" => logs.tail = args.next().and_then(|t| t.parse().ok()).unwrap_or(logs.tail),
            "--stdio" => {} // passed by some clients; stdio is the only transport
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            _ if (arg == "-" || !arg.starts_with('-')) && positional.is_none() => positional = Some(arg),
            _ => {
                eprintln!("unknown argument: {arg}\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }

    let result = match mode.as_str() {
        "lsp" => {
            kubectl.namespace = namespace;
            lsp::run(kubectl)
        }
        "mcp" => {
            kubectl.namespace = namespace;
            mcp::run(kubectl, read_only)
        }
        // For session commands `--context` names the context to select.
        "select-kubeconfig" => {
            let choice = kubectl.context.take();
            cli::select_kubeconfig(&kubectl, positional, choice)
        }
        "switch-context" => {
            let choice = positional.or_else(|| kubectl.context.take());
            cli::switch_context(&kubectl, choice)
        }
        "set-namespace" => cli::set_namespace(&kubectl, positional.or(namespace)),
        "logs" => {
            logs.target = positional;
            logs.namespace = namespace;
            cli::logs(&kubectl, logs)
        }
        "install-tasks" => install::install_tasks().map(|message| println!("{message}")),
        "reset-session" => cli::reset(&kubectl),
        "status" => cli::status(&kubectl),
        "--version" | "version" => {
            println!("kube-lsp {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("kube-lsp: {e}");
        std::process::exit(1);
    }
}
