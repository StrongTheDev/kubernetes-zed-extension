mod cli;
mod kubectl;
mod lsp;
mod mcp;
mod session;
mod yaml;

use kubectl::Kubectl;

const USAGE: &str = "\
kube-lsp — Kubernetes tooling for Zed

USAGE:
    kube-lsp lsp [--context NAME] [--kubeconfig PATH] [--kubectl PATH]
    kube-lsp mcp [--read-only] [--context NAME] [--kubeconfig PATH] [--kubectl PATH]

  Session commands (run them as Zed tasks; they affect the editor and Agent):
    kube-lsp select-kubeconfig [PATH] [--context NAME]   no PATH opens a file picker
    kube-lsp switch-context [NAME]
    kube-lsp reset-session
    kube-lsp status
";

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "lsp".into());
    let mut kubectl = Kubectl::default();
    let mut read_only = false;
    let mut positional: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--read-only" => read_only = true,
            "--context" => kubectl.context = args.next(),
            "--kubeconfig" => kubectl.kubeconfig = args.next(),
            "--kubectl" => kubectl.bin = args.next().unwrap_or(kubectl.bin),
            "--stdio" => {} // passed by some clients; stdio is the only transport
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            _ if !arg.starts_with('-') && positional.is_none() => positional = Some(arg),
            _ => {
                eprintln!("unknown argument: {arg}\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }

    let result = match mode.as_str() {
        "lsp" => lsp::run(kubectl),
        "mcp" => mcp::run(kubectl, read_only),
        // For session commands `--context` names the context to select.
        "select-kubeconfig" => {
            let choice = kubectl.context.take();
            cli::select_kubeconfig(&kubectl, positional, choice)
        }
        "switch-context" => {
            let choice = positional.or_else(|| kubectl.context.take());
            cli::switch_context(&kubectl, choice)
        }
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
