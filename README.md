# Kubernetes for Zed

Kubernetes tooling for [Zed](https://zed.dev), aiming at feature parity with the
JetBrains Kubernetes plugin. It has two parts:

| Part | Where | What it does |
|---|---|---|
| **Zed extension** (WASM) | repo root (`extension.toml`, `src/lib.rs`) | Registers a YAML language server and an MCP context server, and launches `kube-lsp` for both |
| **`kube-lsp`** (native) | [`kube-lsp/`](kube-lsp) | One binary. `kube-lsp lsp` is the language server and `kube-lsp mcp` is the MCP server. Both talk to the cluster through your `kubectl` |

## What works today

**In the editor (LSP), on any YAML file that contains Kubernetes objects:**

- **Hover docs**: hover any field (e.g. `spec.template.spec.containers.image`) to see
  `kubectl explain` output for that exact path, from your cluster's own schema
  (CRDs included).
- **Code actions** (`ctrl-.`), for the resource under the cursor:
  Apply · Dry-run (server) · Diff against cluster · Open live YAML · Describe ·
  Show events · Show logs (workloads) · Delete (asks first), plus Apply/Diff for
  every resource in the file.
- **Validation on save**: a server-side `--dry-run=server --validate=strict`.
  An unknown or duplicate field is marked on its own line. It is skipped quietly
  when no cluster is reachable.
- **Local checks**: missing `kind` / `apiVersion` / `metadata.name`.
- **Completion** for `kind:` and `apiVersion:` from the cluster's discovery API,
  with a built-in list when you're offline.
- **Outline**: each resource (`Deployment/web`, `Service/web`) appears in the
  outline panel and symbol search.
- **Snippets**: not included. Install the community
  [kubernetes-snippets](https://github.com/zed-kubernetes/kubernetes-snippets)
  extension; the two work side by side.

**In the Agent panel (MCP context server `kubernetes`):**
`kubernetes_contexts`, `_api_resources`, `_get`, `_describe`, `_logs`, `_events`,
`_explain`, `_top`, plus `_use_kubeconfig`, `_use_context` and
`_reset_session` for choosing the cluster. These tools are always available. The ones that
change the cluster are `_apply`, `_delete`, `_scale` and `_rollout_restart`.
Set `read_only: true` to hide them.

Every kubectl call uses your normal kubeconfig, auth plugins and shell
environment. Actions report which context they ran against.

## Choosing a cluster (like JetBrains' "select / paste kubeconfig")

By default the extension uses your default kubeconfig and its `current-context`.
To work against another cluster for the session:

| You want to… | Do this |
|---|---|
| **Pick a kubeconfig file from a dialog** | Command palette, `task: spawn`, then **Kubernetes: Select kubeconfig…**. The OS file picker opens in `~/.kube` and you choose the context in the terminal (setup below) |
| **Use a kubeconfig file** | Open the file in Zed, press `ctrl-.` (`cmd-.`) anywhere in it, then **Kubernetes: Connect using this kubeconfig** |
| **Paste a kubeconfig** | `ctrl-n`, paste, set the language to YAML, then save it anywhere and use the same action. If you connect without saving after an edit, the buffer contents are copied to a private file instead (see below) |
| **Switch context** | `ctrl-.` in any manifest, then **Kubernetes: Switch cluster context…** to pick from the active kubeconfig's contexts |
| **Go back to default** | `ctrl-.`, then **Kubernetes: Disconnect session, use default kubeconfig** |
| **Ask the Agent** | *"connect to the staging context in ~/kube/staging.yaml"* runs the `kubernetes_use_kubeconfig` tool |

If a kubeconfig has several contexts, Zed asks which one to use. The extension
then checks the connection and reports the server version, or a warning if the
cluster can't be reached.

How it behaves:

- **The editor and the Agent share one selection.** It's stored in
  `%LOCALAPPDATA%\kube-lsp\session.json` (`~/.local/state/kube-lsp/` on Linux and
  macOS), so code actions, hovers, validation and Agent tools all hit the same
  cluster.
- **Your own kubeconfig is never modified.** The selection is passed as
  `--kubeconfig/--context` on every call, unlike `kubectl config use-context`.
  Your terminal keeps its own context.
- **The selection outlives a Zed restart** and lasts until you disconnect. It
  takes precedence over the `kubeconfig`/`context` values in settings.
- **Saved files are used in place**, so relative `certificate-authority:` paths
  keep working. Buffer contents that differ from disk are copied to
  `kube-lsp/kubeconfigs/` (mode 0600 on Unix), and that copy is deleted when
  you disconnect.
- A kubeconfig named `config` with no extension isn't detected as YAML. Fix that
  with `"file_types": { "YAML": ["**/.kube/config"] }` in your settings.

### Command palette entries (Zed tasks)

Zed extensions can't add their own command palette actions, so these are
shipped as [tasks](https://zed.dev/docs/tasks) that run `kube-lsp` subcommands:

1. Command palette, `zed: open tasks`, then paste the entries from
   [`zed/tasks.json`](zed/tasks.json). Or put them in a project's
   `.zed/tasks.json`.
2. Optionally bind keys using [`zed/keymap.json`](zed/keymap.json)
   (`ctrl-alt-k` opens the picker).

| Task | Runs | Does |
|---|---|---|
| Kubernetes: Select kubeconfig… | `kube-lsp select-kubeconfig` | Native file dialog (Windows Forms, macOS `osascript`, Linux `zenity`/`kdialog`; falls back to typing a path), then a numbered context menu |
| Kubernetes: Switch context… | `kube-lsp switch-context` | Context menu for the active kubeconfig |
| Kubernetes: Use default kubeconfig | `kube-lsp reset-session` | Clears the selection |
| Kubernetes: Connection status | `kube-lsp status` | Shows the session, the active context and the server version |

The same commands work from any terminal, e.g.
`kube-lsp select-kubeconfig ~/.kube/staging.yaml --context staging`.

For a fixed setup per project, put `kubeconfig`/`context` in the project's
`.zed/settings.json` under `lsp.kube-lsp.initialization_options` and
`context_servers.kubernetes.settings`.

## Install (development)

```sh
# 1. Native helper (needs kubectl on PATH)
cargo install --path kube-lsp

# 2. Zed: command palette → "zed: install dev extension" → pick this folder
```

Optional settings (`~/.config/zed/settings.json`, or `%APPDATA%\Zed\settings.json` on Windows):

```jsonc
{
  "lsp": {
    "kube-lsp": {
      // "binary": { "path": "C:/tools/kube-lsp.exe" },
      "initialization_options": {
        "validateOnSave": true,
        "context": "",          // empty = current-context
        "kubeconfig": ""
      }
    }
  },
  "context_servers": {
    "kubernetes": {
      "settings": { "read_only": false, "context": "" }
    }
  }
}
```

## Parity roadmap vs. JetBrains Kubernetes plugin

| JetBrains feature | Status |
|---|---|
| Field docs / quick documentation | ✅ hover (`kubectl explain`) |
| Apply / delete / diff / dry-run from editor | ✅ code actions |
| Describe, live YAML, events, logs | ✅ code actions → result buffer, ✅ MCP |
| Schema validation incl. CRDs | ✅ server-side dry-run on save |
| Resource structure view | ✅ outline |
| Live templates | ➡️ use [kubernetes-snippets](https://github.com/zed-kubernetes/kubernetes-snippets) |
| Select / paste kubeconfig, switch context | ✅ code actions + prompt, shared with Agent tools |
| Default namespace per session | ⏳ |
| Field completion (all fields, not just kind/apiVersion) | ⏳ next: build from cluster OpenAPI v3 |
| Go-to-definition between resources (selector → labels, configMapRef → ConfigMap) | ⏳ |
| Helm chart support (values navigation, `helm template` preview, lint) | ⏳ |
| Kustomize (`kustomize build` preview, resource references) | ⏳ |
| Cluster tree browser (Services tool window) | ❌ needs UI: companion GPUI app or Zed fork |
| Interactive pod shell / port-forward manager | ⏳ via Zed tasks/terminal, then companion app |
| Log streaming (follow) | ❌ needs UI |

## Layout

```
extension.toml        Zed manifest (language server, context server)
src/lib.rs            WASM extension: locates/launches kube-lsp
kube-lsp/src/
  main.rs             CLI entry: `lsp` | `mcp`
  lsp.rs              Language server
  mcp.rs              MCP server
  kubectl.rs          kubectl wrapper (timeouts, no console window on Windows)
  session.rs          Active cluster selection shared by LSP and MCP
  cli.rs              select-kubeconfig / switch-context / reset-session / status
zed/tasks.json        Command palette entries (Zed tasks)
zed/keymap.json       Optional key bindings
  yaml.rs             Position-preserving manifest analysis (+ tests)
examples/demo.yaml    Try hovers/actions here
```

Run the tests with `cargo test --manifest-path kube-lsp/Cargo.toml`.
