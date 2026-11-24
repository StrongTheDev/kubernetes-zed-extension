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
- **Go to definition** (`F12` / `cmd-click`): jump from a reference to the
  object it names, anywhere in the workspace. Works for `configMapRef`/`configMapKeyRef`/
  `configMap` volumes, `secretRef`/`secretKeyRef`/`secret` volumes/`imagePullSecrets`/
  Ingress TLS, `claimName`, `serviceAccountName`, StatefulSet `serviceName`, Ingress
  backends, `scaleTargetRef`, RBAC `roleRef` and `subjects`, and the storage, priority,
  ingress and runtime class names. On a **label selector** (Service `selector`,
  `matchLabels`) it lists the workloads whose pods carry those labels. Named
  objects that aren't in the workspace open as live YAML from the cluster.
- **Outline**: each resource (`Deployment/web`, `Service/web`) appears in the
  outline panel and symbol search.
- **Snippets**: not included. Install the community
  [kubernetes-snippets](https://github.com/zed-kubernetes/kubernetes-snippets)
  extension; the two work side by side.

**In the Agent panel (MCP context server `kubernetes`):**
`kubernetes_contexts`, `_api_resources`, `_get`, `_describe`, `_logs`, `_events`,
`_explain`, `_top`, plus `_use_kubeconfig`, `_use_context`, `_use_namespace`
and `_reset_session` for choosing the cluster. These tools are always available. The ones that
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
| **Set a default namespace** | `ctrl-.`, then **Kubernetes: Set default namespace…**, or the task of the same name (lists every namespace) |
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
- **The default namespace** applies to manifests without `metadata.namespace`
  (apply, diff, validation, and so on) and to Agent tools called without a
  namespace. Manifests that name their namespace keep it. Switching kubeconfig
  or context resets it to the context's own default.
- A kubeconfig named `config` with no extension isn't detected as YAML. Fix that
  with `"file_types": { "YAML": ["**/.kube/config"] }` in your settings.

### Command palette entries (Zed tasks)

Zed extensions can't add their own command palette actions, so these are
shipped as [tasks](https://zed.dev/docs/tasks) that run `kube-lsp` subcommands:

1. Command palette, `zed: open tasks`, then paste the entries from
   [`zed/tasks.json`](zed/tasks.json). Or put them in a project's
   `.zed/tasks.json`.
2. Optionally bind keys using [`zed/keymap.json`](zed/keymap.json)
   (`ctrl-alt-k` opens the picker, `ctrl-alt-l` streams logs for the resource
   under the cursor).

Tasks call `kube-lsp` from PATH. If you rely on the copy the extension
downloads, put a build on PATH too (`cargo install --path kube-lsp`, or a
binary from the releases page).

| Task | Runs | Does |
|---|---|---|
| Kubernetes: Select kubeconfig… | `kube-lsp select-kubeconfig` | Native file dialog (Windows Forms, macOS `osascript`, Linux `zenity`/`kdialog`; falls back to typing a path), then a numbered context menu |
| Kubernetes: Switch context… | `kube-lsp switch-context` | Context menu for the active kubeconfig |
| Kubernetes: Set default namespace… | `kube-lsp set-namespace` | Namespace menu from the cluster |
| Kubernetes: Stream logs (resource under cursor) | `kube-lsp logs --file $ZED_FILE --line $ZED_ROW` | Follows logs of the Pod or workload under the cursor. Workloads use their `matchLabels`, so every pod is followed, not just one |
| Kubernetes: Stream logs… | `kube-lsp logs` | Pick a pod or workload from the cluster, then follow its logs |
| Kubernetes: Use default kubeconfig | `kube-lsp reset-session` | Clears the selection |
| Kubernetes: Connection status | `kube-lsp status` | Shows the session, the active context and the server version |

The same commands work from any terminal, e.g.
`kube-lsp select-kubeconfig ~/.kube/staging.yaml --context staging`.

For a fixed setup per project, put `kubeconfig`/`context` in the project's
`.zed/settings.json` under `lsp.kube-lsp.initialization_options` and
`context_servers.kubernetes.settings`.

## Install

The extension finds `kube-lsp` in this order:

1. The `binary.path` / `command.path` setting
2. `kube-lsp` on PATH (for development builds)
3. The latest [GitHub release](https://github.com/StrongTheDev/kubernetes-zed-extension/releases),
   downloaded automatically for your platform (macOS, Linux musl and Windows,
   each on x86_64 and arm64)

For development:

```sh
cargo install --path kube-lsp     # needs kubectl on PATH at runtime
# Zed: command palette → "zed: install dev extension" → pick this folder
```

### Releasing kube-lsp

[`.github/workflows/kube-lsp.yml`](.github/workflows/kube-lsp.yml) runs clippy and
tests, then builds all six targets (with caching) on every push or PR that touches
`kube-lsp/`. The archives are uploaded as workflow artifacts. On `main`, if the
version in `kube-lsp/Cargo.toml` has no `kube-lsp-v<version>` release yet, the
workflow publishes that release with the archives attached. **To ship a new
binary, bump the version.** Pushes without a bump only produce artifacts.

Optional settings (`~/.config/zed/settings.json`, or `%APPDATA%\Zed\settings.json` on Windows):

```jsonc
{
  "lsp": {
    "kube-lsp": {
      // "binary": { "path": "C:/tools/kube-lsp.exe" },
      "initialization_options": {
        "validateOnSave": true,
        "context": "",          // empty = current-context
        "kubeconfig": "",
        "namespace": ""         // empty = the context's namespace
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
| Default namespace per session | ✅ code action, task, Agent tool |
| Field completion (all fields, not just kind/apiVersion) | ⏳ next: build from cluster OpenAPI v3 |
| Go-to-definition between resources (selector → labels, configMapRef → ConfigMap) | ✅ workspace-wide, cluster fallback |
| Find usages (ConfigMap → who mounts it) | ⏳ |
| Helm chart support (values navigation, `helm template` preview, lint) | ⏳ |
| Kustomize (`kustomize build` preview, resource references) | ⏳ |
| Cluster tree browser (Services tool window) | ❌ needs UI: companion GPUI app or Zed fork |
| Interactive pod shell / port-forward manager | ⏳ via Zed tasks/terminal, then companion app |
| Log streaming (follow) | ✅ task in Zed's terminal (all pods of a workload) |

## Layout

```
extension.toml        Zed manifest (language server, context server)
src/lib.rs            WASM extension: finds or downloads kube-lsp, launches it
kube-lsp/src/
  main.rs             CLI entry: lsp | mcp | session commands | logs
  lsp.rs              Language server
  mcp.rs              MCP server
  refs.rs             Cross-resource references (go to definition) + workspace index
  kubectl.rs          kubectl wrapper (cluster/namespace selection, timeouts)
  session.rs          Active cluster selection shared by LSP and MCP
  cli.rs              select-kubeconfig / switch-context / set-namespace / logs / status
  yaml.rs             Position-preserving manifest analysis
zed/tasks.json        Command palette entries (Zed tasks)
zed/keymap.json       Optional key bindings
.github/workflows/    kube-lsp release builds, extension check
examples/demo.yaml    Try hovers/actions here
```

Run the tests with `cargo test --manifest-path kube-lsp/Cargo.toml`.
