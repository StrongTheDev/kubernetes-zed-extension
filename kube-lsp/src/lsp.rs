//! Language server for Kubernetes manifests.
//!
//! Zed has no API for extension-provided panels, so the LSP surface is where
//! the IDE-style features live:
//!
//! | JetBrains feature                    | Here                                         |
//! |--------------------------------------|----------------------------------------------|
//! | Field documentation                  | hover → `kubectl explain <kind>.<path>`      |
//! | Apply / delete / diff with cluster   | code actions (`ctrl-.` / `cmd-.`)            |
//! | Describe / live YAML / logs / events | code actions that open a result buffer       |
//! | Schema validation                    | OpenAPI checks while typing, dry-run on save |
//! | Field completion                     | completion from the cluster's OpenAPI v3     |
//! | Find usages / rename                 | references and rename across the workspace   |
//! | Structure view                       | document symbols (outline panel)             |

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use serde_json::{json, Value};

use crate::install::{self, TaskState};
use crate::kubectl::{is_connectivity_error, safe_arg, Kubectl};
use crate::manifest::{self, Batch};
use crate::refs::{self, Place, Reference, Target};
use crate::schema::{self, Problem, Shape, Spot};
use crate::session::{self, Session};
use crate::tree::{self, Pos};
use crate::yaml::{self, Doc};
use crate::{buffers, checks, cluster};

const COMMANDS: &[(&str, &str)] = &[
    ("kube.apply", "Apply"),
    ("kube.dryRun", "Dry-run (server)"),
    ("kube.diff", "Diff against cluster"),
    ("kube.getLive", "Open live YAML from cluster"),
    ("kube.describe", "Describe"),
    ("kube.events", "Show events"),
    ("kube.logs", "Show logs"),
    ("kube.delete", "Delete from cluster"),
];

/// Commands that choose which cluster everything talks to.
const SESSION_COMMANDS: &[&str] =
    &["kube.useKubeconfig", "kube.switchContext", "kube.setNamespace", "kube.resetSession", "kube.installTasks"];

/// Wait for a pause in typing before checking the document.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

/// Prompt buttons get unwieldy beyond this; the task has no such limit.
const MAX_PROMPT_CHOICES: usize = 20;
const USE_CONTEXT_DEFAULT: &str = "(use the context's default)";

const WORKLOADS: &[&str] = &["Pod", "Deployment", "StatefulSet", "DaemonSet", "ReplicaSet", "Job"];

/// Used when the cluster is unreachable so completion still helps offline.
const FALLBACK_KINDS: &[(&str, &str)] = &[
    ("ConfigMap", "v1"),
    ("CronJob", "batch/v1"),
    ("DaemonSet", "apps/v1"),
    ("Deployment", "apps/v1"),
    ("HorizontalPodAutoscaler", "autoscaling/v2"),
    ("Ingress", "networking.k8s.io/v1"),
    ("Job", "batch/v1"),
    ("Namespace", "v1"),
    ("NetworkPolicy", "networking.k8s.io/v1"),
    ("PersistentVolumeClaim", "v1"),
    ("Pod", "v1"),
    ("Role", "rbac.authorization.k8s.io/v1"),
    ("RoleBinding", "rbac.authorization.k8s.io/v1"),
    ("Secret", "v1"),
    ("Service", "v1"),
    ("ServiceAccount", "v1"),
    ("StatefulSet", "apps/v1"),
];

// ---------------------------------------------------------------------------
// Client handle, shareable with worker threads.

#[derive(Clone)]
struct Client {
    sender: Arc<dyn Fn(Message) + Send + Sync>,
    next_id: Arc<AtomicI32>,
}

impl Client {
    fn send(&self, msg: Message) {
        (self.sender)(msg);
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(Message::Notification(Notification::new(method.into(), params)));
    }

    fn next_request_id(&self) -> RequestId {
        RequestId::from(self.next_id.fetch_add(1, Ordering::SeqCst))
    }

    fn request_with(&self, id: RequestId, method: &str, params: Value) {
        self.send(Message::Request(Request::new(id, method.into(), params)));
    }

    fn request(&self, method: &str, params: Value) -> RequestId {
        let id = self.next_request_id();
        self.request_with(id.clone(), method, params);
        id
    }

    fn respond(&self, id: RequestId, result: Value) {
        self.send(Message::Response(Response::new_ok(id, result)));
    }

    /// 1 = error, 2 = warning, 3 = info, 4 = log.
    fn show(&self, kind: u8, text: impl Into<String>) {
        self.notify("window/showMessage", json!({ "type": kind, "message": text.into() }));
    }

    fn log(&self, text: impl Into<String>) {
        self.notify("window/logMessage", json!({ "type": 4, "message": text.into() }));
    }

    fn open_document(&self, path: &std::path::Path) {
        self.request("window/showDocument", json!({ "uri": path_to_uri(path), "takeFocus": true }));
    }
}

// ---------------------------------------------------------------------------

/// `(kind, apiVersion)` from the discovery API.
type KindEntry = (String, String);

/// Server->client prompts awaiting the user's answer.
enum Pending {
    Delete { batch: Batch, label: String },
    /// Pick a context; `kubeconfig` is set when connecting a new file.
    Context { kubeconfig: Option<String>, choices: Vec<String> },
    Namespace,
}

struct Server {
    client: Client,
    kubectl: Kubectl,
    options: Options,
    documents: HashMap<String, String>,
    /// Bumped on every change, so a slow check can't overwrite newer results.
    generations: Arc<Mutex<HashMap<String, u64>>>,
    explain_cache: Arc<Mutex<HashMap<String, Option<String>>>>,
    /// Keyed by cluster identity, so switching clusters doesn't serve stale kinds.
    kinds_cache: Arc<Mutex<HashMap<String, Vec<KindEntry>>>>,
    pending: HashMap<RequestId, Pending>,
    /// Workspace folders, scanned for references and their targets.
    roots: Vec<PathBuf>,
    index: Arc<Mutex<refs::Index>>,
    schemas: Arc<schema::Store>,
    cluster: Arc<cluster::Cache>,
}

#[derive(Clone, Copy)]
struct Options {
    /// Server-side dry-run when a file is opened or saved.
    validate_on_save: bool,
    /// OpenAPI checks while typing.
    schema_validation: bool,
    /// Warnings about references that lead nowhere.
    check_references: bool,
}

pub fn run(mut kubectl: Kubectl) -> Result<(), String> {
    let (connection, io_threads) = Connection::stdio();

    let commands: Vec<&str> =
        COMMANDS.iter().map(|(c, _)| *c).chain(SESSION_COMMANDS.iter().copied()).collect();
    let capabilities = json!({
        "textDocumentSync": { "openClose": true, "change": 1, "save": { "includeText": false } },
        "hoverProvider": true,
        "completionProvider": { "triggerCharacters": [":", " "] },
        "codeActionProvider": true,
        "documentSymbolProvider": true,
        "definitionProvider": true,
        "referencesProvider": true,
        "renameProvider": { "prepareProvider": true },
        "executeCommandProvider": { "commands": commands },
    });
    let init = connection
        .initialize(capabilities)
        .map_err(|e| format!("initialize failed: {e}"))?;

    let opts = init.get("initializationOptions").cloned().unwrap_or(Value::Null);
    let opt_str = |key: &str| opts.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
    let opt_bool = |key: &str| opts.get(key).and_then(Value::as_bool).unwrap_or(true);
    if let Some(c) = opt_str("context") {
        kubectl.context = Some(c);
    }
    if let Some(k) = opt_str("kubeconfig") {
        kubectl.kubeconfig = Some(k);
    }
    if let Some(b) = opt_str("kubectl") {
        kubectl.bin = b;
    }
    if let Some(ns) = opt_str("namespace") {
        kubectl.namespace = Some(ns);
    }
    let mut roots: Vec<PathBuf> = init["workspaceFolders"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f["uri"].as_str().and_then(uri_to_path))
        .collect();
    if roots.is_empty() {
        roots.extend(init["rootUri"].as_str().and_then(uri_to_path));
    }

    let raw_sender = connection.sender.clone();
    let client = Client {
        sender: Arc::new(move |m| {
            let _ = raw_sender.send(m);
        }),
        next_id: Arc::new(AtomicI32::new(1)),
    };

    let mut server = Server {
        client,
        kubectl,
        options: Options {
            validate_on_save: opt_bool("validateOnSave"),
            schema_validation: opt_bool("schemaValidation"),
            check_references: opt_bool("checkReferences"),
        },
        documents: HashMap::new(),
        generations: Default::default(),
        explain_cache: Default::default(),
        kinds_cache: Default::default(),
        pending: HashMap::new(),
        roots,
        index: Default::default(),
        schemas: Default::default(),
        cluster: Default::default(),
    };

    let client = server.client.clone();
    std::thread::spawn(move || {
        session::sweep();
        if let Err(e) = install::refresh_launcher() {
            client.log(format!("could not update the task launcher: {e}"));
        }
    });

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req).map_err(|e| e.to_string())? {
                    break;
                }
                server.on_request(req);
            }
            Message::Notification(n) => server.on_notification(n),
            Message::Response(resp) => server.on_response(resp),
        }
    }
    for path in server.documents.keys().filter_map(|u| uri_to_path(u)) {
        buffers::forget(&path);
    }
    // The writer thread only stops once every sender clone is gone.
    drop(server);
    drop(connection);
    io_threads.join().map_err(|e| e.to_string())
}

impl Server {
    fn on_notification(&mut self, n: Notification) {
        let p = &n.params;
        let uri = p["textDocument"]["uri"].as_str().unwrap_or_default().to_string();
        match n.method.as_str() {
            "textDocument/didOpen" => {
                let text = p["textDocument"]["text"].as_str().unwrap_or_default().to_string();
                self.documents.insert(uri.clone(), text);
                self.publish(&uri, true);
            }
            "textDocument/didChange" => {
                if let Some(text) = p["contentChanges"]
                    .as_array()
                    .and_then(|c| c.last())
                    .and_then(|c| c["text"].as_str())
                {
                    self.documents.insert(uri.clone(), text.to_string());
                    self.publish(&uri, false);
                }
            }
            "textDocument/didSave" => {
                if let Some(path) = uri_to_path(&uri) {
                    buffers::forget(&path);
                }
                self.publish(&uri, true);
            }
            "textDocument/didClose" => {
                self.documents.remove(&uri);
                self.next_generation(&uri);
                if let Some(path) = uri_to_path(&uri) {
                    buffers::forget(&path);
                }
                self.client.notify(
                    "textDocument/publishDiagnostics",
                    json!({ "uri": uri, "diagnostics": [] }),
                );
            }
            _ => {}
        }
    }

    fn on_response(&mut self, resp: Response) {
        let Some(pending) = self.pending.remove(&resp.id) else { return };
        let Some(choice) = resp
            .response_result
            .as_ref()
            .ok()
            .and_then(|r| r["title"].as_str())
            .map(String::from)
        else {
            if let Pending::Context { kubeconfig: Some(k), .. } = pending {
                session::discard_unused(&k); // dismissed: don't keep credentials
            }
            return;
        };
        let (client, kubectl) = (self.client.clone(), self.kubectl.clone());
        match pending {
            Pending::Delete { batch, label } if choice == "Delete" => {
                std::thread::spawn(move || {
                    let ctx = kubectl.current_context();
                    match run_batch(&kubectl, &["delete", "-f", "-"], &batch) {
                        Ok(out) => client.show(3, format!("[{ctx}] {}", out.trim())),
                        Err(e) => client.show(1, format!("[{ctx}] delete {label} failed: {e}")),
                    }
                });
            }
            Pending::Context { kubeconfig, choices } if choices.contains(&choice) => {
                let kubeconfig = kubeconfig.or_else(|| session::load().kubeconfig);
                self.activate(Session { kubeconfig, context: Some(choice), namespace: None });
            }
            Pending::Context { kubeconfig: Some(k), .. } => session::discard_unused(&k), // "Cancel"
            Pending::Namespace => {
                let mut current = session::load();
                current.namespace = (choice != USE_CONTEXT_DEFAULT).then_some(choice);
                match session::save(&current) {
                    Ok(()) => self.client.show(
                        3,
                        format!("Kubernetes: default namespace is now {}", self.kubectl.effective_namespace()),
                    ),
                    Err(e) => self.client.show(1, format!("Kubernetes: could not save session: {e}")),
                }
            }
            _ => {}
        }
    }

    fn on_request(&mut self, req: Request) {
        let id = req.id.clone();
        let p = req.params;
        match req.method.as_str() {
            "textDocument/hover" => self.hover(id, &p),
            "textDocument/completion" => self.completion(id, &p),
            "textDocument/definition" => self.definition(id, &p),
            "textDocument/references" => self.references(id, &p),
            "textDocument/prepareRename" => {
                let result = self.prepare_rename(&p);
                self.client.respond(id, result);
            }
            "textDocument/rename" => self.rename(id, &p),
            "textDocument/codeAction" => {
                let result = self.code_actions(&p);
                self.client.respond(id, result);
            }
            "textDocument/documentSymbol" => {
                let result = self.symbols(&p);
                self.client.respond(id, result);
            }
            "workspace/executeCommand" => {
                self.execute(&p);
                self.client.respond(id, Value::Null);
            }
            _ => self.client.send(Message::Response(Response::new_err(
                id,
                lsp_server::ErrorCode::MethodNotFound as i32,
                format!("unsupported: {}", req.method),
            ))),
        }
    }

    fn text(&self, params: &Value) -> Option<(String, String)> {
        let uri = params["textDocument"]["uri"].as_str()?.to_string();
        let text = self.documents.get(&uri)?.clone();
        Some((uri, text))
    }

    // -- diagnostics --------------------------------------------------------

    fn next_generation(&self, uri: &str) -> u64 {
        let mut all = self.generations.lock().unwrap();
        let generation = all.entry(uri.to_string()).or_insert(0);
        *generation += 1;
        *generation
    }

    /// Checks a document in the background. `saved` (also used when a file is
    /// opened) allows calls to the cluster; otherwise only cached data is used.
    fn publish(&self, uri: &str, saved: bool) {
        let Some(text) = self.documents.get(uri).cloned() else { return };
        let known = self.generations.lock().unwrap().contains_key(uri);
        let is_manifest = {
            let lines = yaml::lines(&text);
            yaml::parse(&lines).iter().any(|d| d.api_version.is_some() || d.kind.is_some())
        };
        if !is_manifest && !known {
            return; // Not a Kubernetes manifest; stay out of the way.
        }
        let generation = self.next_generation(uri);
        let generations = self.generations.clone();
        let (client, uri) = (self.client.clone(), uri.to_string());
        let analysis = Analysis {
            kubectl: self.kubectl.clone(),
            options: self.options,
            roots: self.roots.clone(),
            open: self.documents.clone(),
            index: self.index.clone(),
            schemas: self.schemas.clone(),
            cluster: self.cluster.clone(),
        };
        std::thread::spawn(move || {
            let current = || generations.lock().unwrap().get(&uri) == Some(&generation);
            let publish = |diags: &[Value]| {
                if current() {
                    client.notify("textDocument/publishDiagnostics", json!({ "uri": uri, "diagnostics": diags }));
                }
            };
            if !saved {
                std::thread::sleep(DEBOUNCE);
                if !current() {
                    return;
                }
            }
            if !is_manifest {
                return publish(&[]);
            }
            let lines = yaml::lines(&text);
            let docs = yaml::parse(&lines);
            let path = uri_to_path(&uri);
            if let Some(path) = path.as_deref().filter(|_| !saved && current()) {
                buffers::record(path, &text);
            }
            let patch = path.as_deref().is_some_and(checks::is_patch);
            let mut diags = local_diagnostics(&lines, &docs);
            if saved {
                publish(&diags); // the rest may wait for the cluster
            }
            diags.extend(analysis.run(&uri, &lines, &docs, patch, saved));
            publish(&diags);
            // A kustomize patch is not a complete object, so it can't be dry-run.
            if !saved || !analysis.options.validate_on_save || patch {
                return;
            }
            let kubectl = &analysis.kubectl;
            let default_ns = kubectl.default_namespace();
            for doc in docs.iter().filter(|d| d.is_cluster_object(&lines)) {
                if !current() {
                    return;
                }
                let batches = manifest::batches(default_ns.as_deref(), &lines, &[doc]);
                let base = ["apply", "--dry-run=server", "--validate=strict", "-o", "name", "-f", "-"];
                let out = kubectl.exec(&batch_args(&base, &batches[0]), Some(&batches[0].manifest));
                if out.success {
                    continue;
                }
                let err = out.error_text();
                if is_connectivity_error(&err) {
                    client.log(format!("skipping server validation: {err}"));
                    break;
                }
                let found = server_diagnostic(&lines, doc, &err);
                // Don't repeat what the schema check already says, more precisely.
                let decoding = err.contains("cannot unmarshal") || err.contains("strict decoding error");
                let reported = |d: &Value| {
                    let line = d["range"]["start"]["line"].as_u64().unwrap_or(0) as usize;
                    d["severity"] == 1
                        && (d["range"]["start"]["line"] == found["range"]["start"]["line"]
                            || (decoding && (doc.start..doc.end).contains(&line)))
                };
                if !diags.iter().any(reported) {
                    diags.push(found);
                }
            }
            publish(&diags);
        });
    }

    // -- hover ---------------------------------------------------------------

    fn hover(&self, id: RequestId, p: &Value) {
        let Some((_, text)) = self.text(p) else {
            return self.client.respond(id, Value::Null);
        };
        let line = p["position"]["line"].as_u64().unwrap_or(0) as usize;
        let lines = yaml::lines(&text);
        let docs = yaml::parse(&lines);
        let Some((_, doc)) = yaml::doc_at(&docs, line) else {
            return self.client.respond(id, Value::Null);
        };
        let (Some(kind), Some(api)) = (doc.kind.clone(), doc.api_version.clone()) else {
            return self.client.respond(id, Value::Null);
        };
        if doc.is_kubeconfig() {
            return self.client.respond(id, Value::Null);
        }
        let path = yaml::key_path(&lines, doc.start, line);
        if path.is_empty() {
            return self.client.respond(id, Value::Null);
        }
        let target = if path.len() == 1 && (path[0] == "kind" || path[0] == "apiVersion") {
            kind.clone()
        } else {
            format!("{kind}.{}", path.join("."))
        };
        let line_len = utf16_len(lines[line]);
        let (client, kubectl, cache) =
            (self.client.clone(), self.kubectl.clone(), self.explain_cache.clone());

        std::thread::spawn(move || {
            let key = format!("{}|{api}|{target}", kubectl.identity());
            let cached = cache.lock().unwrap().get(&key).cloned();
            let doc_text = cached.unwrap_or_else(|| {
                let out = kubectl.exec(&["explain", &target, &format!("--api-version={api}")], None);
                let text = out.success.then(|| out.stdout.trim().to_string());
                // Don't cache connectivity failures; the cluster may come back.
                if out.success || !is_connectivity_error(&out.error_text()) {
                    cache.lock().unwrap().insert(key, text.clone());
                }
                text
            });
            let result = match doc_text {
                Some(t) => json!({
                    "contents": { "kind": "markdown", "value": format!("**{target}** `{api}`\n\n```text\n{t}\n```") },
                    "range": { "start": { "line": line, "character": 0 }, "end": { "line": line, "character": line_len } }
                }),
                None => Value::Null,
            };
            client.respond(id, result);
        });
    }

    // -- completion ----------------------------------------------------------

    fn completion(&self, id: RequestId, p: &Value) {
        let Some((_, text)) = self.text(p) else {
            return self.client.respond(id, Value::Null);
        };
        let line_no = p["position"]["line"].as_u64().unwrap_or(0) as usize;
        let character = p["position"]["character"].as_u64().unwrap_or(0) as usize;
        let lines = yaml::lines(&text);
        let Some(line) = lines.get(line_no) else {
            return self.client.respond(id, Value::Null);
        };
        let prefix = &line[..byte_index(line, character)];

        let (field, value) = if let Some(v) = prefix.strip_prefix("kind:") {
            ("kind", v)
        } else if let Some(v) = prefix.strip_prefix("apiVersion:") {
            ("apiVersion", v)
        } else {
            return self.complete_field(id, &lines, line_no, prefix.len());
        };
        let typed = value.trim_start();
        let start = utf16_len(&prefix[..prefix.len() - typed.len()]);
        let need_space = !value.starts_with(' ');
        let range = json!({
            "start": { "line": line_no, "character": start },
            "end": { "line": line_no, "character": character }
        });
        let (client, kubectl, cache) =
            (self.client.clone(), self.kubectl.clone(), self.kinds_cache.clone());

        std::thread::spawn(move || {
            let identity = kubectl.identity();
            let cached = cache.lock().unwrap().get(&identity).cloned();
            let kinds = cached
                .or_else(|| {
                    let found = kubectl.api_resources().ok()?;
                    cache.lock().unwrap().insert(identity, found.clone());
                    Some(found)
                })
                .unwrap_or_else(|| {
                    FALLBACK_KINDS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
                });
            let mut seen = std::collections::BTreeSet::new();
            let items: Vec<Value> = kinds
                .iter()
                .filter_map(|(kind, api)| {
                    let (label, detail) = if field == "kind" {
                        (kind.clone(), api.clone())
                    } else {
                        (api.clone(), String::new())
                    };
                    seen.insert(label.clone()).then(|| {
                        let insert = if need_space { format!(" {label}") } else { label.clone() };
                        json!({
                            "label": label,
                            "detail": detail,
                            "kind": 7,
                            "textEdit": { "range": range, "newText": insert }
                        })
                    })
                })
                .collect();
            client.respond(id, json!({ "isIncomplete": false, "items": items }));
        });
    }

    /// Field names and values from the cluster's schemas.
    fn complete_field(&self, id: RequestId, lines: &[&str], line: usize, col: usize) {
        let docs = yaml::parse(lines);
        let found = yaml::doc_at(&docs, line).and_then(|(_, doc)| {
            let target = (doc.api_version.clone()?, doc.kind.clone()?);
            (!doc.is_kubeconfig()).then_some(())?;
            Some((target, schema::spot(lines, doc, line, col)?))
        });
        let Some(((api, kind), spot)) = found else {
            return self.client.respond(id, Value::Null);
        };
        let text = lines[line].to_string();
        let (client, kubectl, schemas) = (self.client.clone(), self.kubectl.clone(), self.schemas.clone());
        std::thread::spawn(move || {
            let items = schemas
                .get(&kubectl, &api, true)
                .map(|d| completion_items(&d, &kind, &spot, line, &text, col))
                .unwrap_or_default();
            client.respond(id, json!({ "isIncomplete": false, "items": items }));
        });
    }

    // -- code actions & commands --------------------------------------------

    fn code_actions(&self, p: &Value) -> Value {
        let Some((uri, text)) = self.text(p) else { return json!([]) };
        let line = p["range"]["start"]["line"].as_u64().unwrap_or(0) as usize;
        let lines = yaml::lines(&text);
        let docs = yaml::parse(&lines);
        let Some((index, doc)) = yaml::doc_at(&docs, line) else { return json!([]) };
        let action = |cmd: &str, title: String, idx: i64| {
            json!({
                "title": title,
                "kind": "source.kubernetes",
                "command": { "title": title, "command": cmd, "arguments": [uri, idx] }
            })
        };
        let mut session_actions = Vec::new();
        if doc.is_resource() {
            if !doc.is_kubeconfig() {
                session_actions.push(action("kube.switchContext", "Kubernetes: Switch cluster context...".into(), -1));
                session_actions.push(action("kube.setNamespace", "Kubernetes: Set default namespace...".into(), -1));
            }
            if session::load().is_active() {
                session_actions.push(action(
                    "kube.resetSession",
                    "Kubernetes: Disconnect session, use default kubeconfig".into(),
                    -1,
                ));
            }
        }
        match install::task_state() {
            _ if !doc.is_resource() => {}
            TaskState::Current => {}
            TaskState::Missing => {
                session_actions.push(action("kube.installTasks", "Kubernetes: Install command palette tasks".into(), -1));
            }
            TaskState::Outdated => {
                session_actions.push(action("kube.installTasks", "Kubernetes: Update command palette tasks".into(), -1));
            }
        }
        if doc.is_kubeconfig() {
            session_actions.insert(0, action("kube.useKubeconfig", "Kubernetes: Connect using this kubeconfig".into(), -1));
            return Value::Array(session_actions);
        }
        if !doc.is_cluster_object(&lines) {
            return Value::Array(session_actions);
        }
        let label = doc.label();
        let mut actions: Vec<Value> = COMMANDS
            .iter()
            .filter(|(cmd, _)| {
                *cmd != "kube.logs" || doc.kind.as_deref().is_some_and(|k| WORKLOADS.contains(&k))
            })
            .filter(|(cmd, _)| doc.name.is_some() || !matches!(*cmd, "kube.logs" | "kube.events"))
            .map(|(cmd, title)| action(cmd, format!("Kubernetes: {title} {label}"), index as i64))
            .collect();
        let count = docs.iter().filter(|d| d.is_cluster_object(&lines)).count();
        if count > 1 {
            actions.push(action("kube.apply", format!("Kubernetes: Apply all {count} resources in file"), -1));
            actions.push(action("kube.diff", format!("Kubernetes: Diff all {count} resources in file"), -1));
        }
        actions.extend(session_actions);
        Value::Array(actions)
    }

    fn execute(&mut self, p: &Value) {
        let command = p["command"].as_str().unwrap_or_default().to_string();
        let args = p["arguments"].as_array().cloned().unwrap_or_default();
        let uri = args.first().and_then(Value::as_str).unwrap_or_default();
        let index = args.get(1).and_then(Value::as_i64).unwrap_or(-1);
        match command.as_str() {
            "kube.switchContext" => return self.prompt_context(None),
            "kube.setNamespace" => return self.prompt_namespace(),
            "kube.installTasks" => {
                return match install::install_tasks() {
                    Ok(message) => self.client.show(3, format!("Kubernetes: {message}")),
                    Err(e) => self.client.show(1, format!("Kubernetes: {e}")),
                };
            }
            "kube.resetSession" => {
                return match session::clear() {
                    Ok(()) => self.client.show(
                        3,
                        format!("Kubernetes: session cleared; now using {}", self.kubectl.current_context()),
                    ),
                    Err(e) => self.client.show(1, format!("Kubernetes: could not clear session: {e}")),
                };
            }
            _ => {}
        }
        let Some(text) = self.documents.get(uri).cloned() else {
            return self.client.show(1, "Kubernetes: document is not open");
        };
        if command == "kube.useKubeconfig" {
            return self.use_kubeconfig(uri, &text);
        }
        let lines = yaml::lines(&text);
        let docs = yaml::parse(&lines);
        let selected: Vec<&Doc> = if index < 0 {
            docs.iter().filter(|d| d.is_cluster_object(&lines)).collect()
        } else {
            match docs.get(index as usize) {
                Some(d) => vec![d],
                None => return self.client.show(1, "Kubernetes: resource no longer exists in file"),
            }
        };
        let batches = manifest::batches(self.kubectl.default_namespace().as_deref(), &lines, &selected);
        let doc = (index >= 0).then(|| selected[0].clone());
        let label = doc.as_ref().map(Doc::label).unwrap_or_else(|| "all resources".into());

        if command == "kube.delete" {
            let ctx = self.kubectl.current_context();
            let id = self.client.request(
                "window/showMessageRequest",
                json!({
                    "type": 2,
                    "message": format!("Delete {label} from cluster context '{ctx}'?"),
                    "actions": [{ "title": "Delete" }, { "title": "Cancel" }]
                }),
            );
            if let Some(batch) = batches.into_iter().next() {
                self.pending.insert(id, Pending::Delete { batch, label });
            }
            return;
        }

        let (client, kubectl) = (self.client.clone(), self.kubectl.clone());
        std::thread::spawn(move || run_command(&client, &kubectl, &command, &batches, doc.as_ref(), &label));
    }

    // -- cluster selection ---------------------------------------------------

    /// "Connect using this kubeconfig": works for a file on disk (relative
    /// certificate paths keep working) and for pasted, unsaved contents.
    fn use_kubeconfig(&mut self, uri: &str, text: &str) {
        let normalize = |s: &str| s.replace("\r\n", "\n");
        let on_disk = uri_to_path(uri)
            .filter(|p| std::fs::read_to_string(p).is_ok_and(|disk| normalize(&disk) == normalize(text)));
        let path = match on_disk {
            Some(p) => p,
            None => match session::store_kubeconfig(text) {
                Ok(p) => p,
                Err(e) => return self.client.show(1, format!("Kubernetes: could not store kubeconfig: {e}")),
            },
        };
        self.prompt_context(Some(path.to_string_lossy().into_owned()));
    }

    /// Lists the contexts of `kubeconfig` (or of the active one) and lets the
    /// user pick; a lone context in a newly connected file is used directly.
    fn prompt_context(&mut self, kubeconfig: Option<String>) {
        let kubectl = match &kubeconfig {
            Some(path) => self.kubectl.for_kubeconfig(path, None),
            None => self.kubectl.clone(),
        };
        let choices = match kubectl.contexts() {
            Ok(c) if !c.is_empty() => c,
            other => {
                if let Some(k) = &kubeconfig {
                    session::discard_unused(k);
                }
                return self.client.show(
                    1,
                    match other {
                        Err(e) => format!("Kubernetes: invalid kubeconfig: {e}"),
                        Ok(_) => "Kubernetes: this kubeconfig defines no contexts".into(),
                    },
                );
            }
        };
        if choices.len() == 1 && kubeconfig.is_some() {
            let context = choices.into_iter().next();
            return self.activate(Session { kubeconfig, context, namespace: None });
        }
        let mut actions: Vec<Value> = choices.iter().map(|c| json!({ "title": c })).collect();
        actions.push(json!({ "title": "Cancel" }));
        let id = self.client.request(
            "window/showMessageRequest",
            json!({
                "type": 3,
                "message": format!("Kubernetes: choose a context (currently {})", self.kubectl.current_context()),
                "actions": actions
            }),
        );
        self.pending.insert(id, Pending::Context { kubeconfig, choices });
    }

    /// Lists the cluster's namespaces (network call, so off the main thread)
    /// and asks for the session's default.
    fn prompt_namespace(&mut self) {
        let id = self.client.next_request_id();
        self.pending.insert(id.clone(), Pending::Namespace);
        let (client, kubectl) = (self.client.clone(), self.kubectl.clone());
        std::thread::spawn(move || {
            let names: Vec<String> = match kubectl.run(&["get", "namespaces", "-o", "name"], None) {
                Ok(out) => out
                    .lines()
                    .filter_map(|l| l.trim().strip_prefix("namespace/"))
                    .map(String::from)
                    .collect(),
                Err(e) => return client.show(1, format!("Kubernetes: could not list namespaces: {e}")),
            };
            let current = kubectl.effective_namespace();
            let mut actions: Vec<Value> =
                names.iter().take(MAX_PROMPT_CHOICES).map(|n| json!({ "title": n })).collect();
            actions.push(json!({ "title": USE_CONTEXT_DEFAULT }));
            let more = if names.len() > MAX_PROMPT_CHOICES {
                format!(
                    " Showing {MAX_PROMPT_CHOICES} of {}; the \"Kubernetes: Set default namespace…\" task lists all.",
                    names.len()
                )
            } else {
                String::new()
            };
            client.request_with(
                id,
                "window/showMessageRequest",
                json!({
                    "type": 3,
                    "message": format!("Kubernetes: choose the default namespace (currently {current}).{more}"),
                    "actions": actions
                }),
            );
        });
    }

    fn activate(&mut self, new: Session) {
        if let Err(e) = session::save(&new) {
            return self.client.show(1, format!("Kubernetes: could not save session: {e}"));
        }
        let (client, kubectl, label) = (self.client.clone(), self.kubectl.clone(), new.describe());
        // Verify in the background; connecting can take a while.
        std::thread::spawn(move || match kubectl.server_version() {
            Ok(v) => client.show(
                3,
                format!("Kubernetes: connected to {label} (server {v}). Editor actions and Agent tools now use this cluster."),
            ),
            Err(e) => client.show(2, format!("Kubernetes: switched to {label}, but the cluster is not reachable: {e}")),
        });
    }

    // -- go to definition ----------------------------------------------------

    /// Jumps from a reference (`configMapRef.name`, `claimName`, `roleRef`,
    /// a label selector, …) to the object in the workspace; for named
    /// references not found locally, opens the live object from the cluster.
    fn definition(&self, id: RequestId, p: &Value) {
        let Some((_, text)) = self.text(p) else {
            return self.client.respond(id, Value::Null);
        };
        let line = p["position"]["line"].as_u64().unwrap_or(0) as usize;
        let lines = yaml::lines(&text);
        let docs = yaml::parse(&lines);
        let reference = match yaml::doc_at(&docs, line) {
            Some((_, doc)) if !doc.is_kubeconfig() && line < lines.len() => refs::reference_at(&lines, doc, line),
            _ => None,
        };
        let Some(reference) = reference else {
            return self.client.respond(id, Value::Null);
        };
        let (client, kubectl) = (self.client.clone(), self.kubectl.clone());
        let (index, roots, open) = (self.index.clone(), self.roots.clone(), self.documents.clone());

        std::thread::spawn(move || {
            let all = index.lock().unwrap().collect(&roots, &open, path_to_uri);
            let hits: Vec<Value> =
                refs::resolve(&reference, &all.entries).iter().map(|e| location(&e.uri, e.line)).collect();
            if !hits.is_empty() {
                return client.respond(id, Value::Array(hits));
            }
            let Reference::Named { kind, name, namespace } = reference else {
                client.show(3, "Kubernetes: no workload in this workspace matches that selector");
                return client.respond(id, Value::Null);
            };
            client.respond(id, live_object_location(&client, &kubectl, &kind, &name, namespace.as_deref()));
        });
    }

    // -- find usages and rename ----------------------------------------------

    fn target(&self, p: &Value) -> Option<Target> {
        let (uri, text) = self.text(p)?;
        refs::target_at(&uri, &text, p["position"]["line"].as_u64()? as usize)
    }

    fn references(&self, id: RequestId, p: &Value) {
        let Some(target) = self.target(p) else {
            return self.client.respond(id, Value::Null);
        };
        let with_declaration = p["context"]["includeDeclaration"].as_bool().unwrap_or(false);
        let client = self.client.clone();
        let (index, roots, open) = (self.index.clone(), self.roots.clone(), self.documents.clone());
        std::thread::spawn(move || {
            let all = index.lock().unwrap().collect(&roots, &open, path_to_uri);
            let mut out: Vec<Value> = Vec::new();
            if with_declaration {
                out.extend(target.definitions(&all).iter().map(|e| match &e.name_place {
                    Some(p) => json!({ "uri": e.uri, "range": place_range(p) }),
                    None => location(&e.uri, e.line),
                }));
            }
            let usages = target.named_usages(&all).into_iter().chain(target.selector_usages(&all));
            out.extend(usages.map(|u| json!({ "uri": u.uri, "range": place_range(&u.place) })));
            client.respond(id, Value::Array(out));
        });
    }

    fn prepare_rename(&self, p: &Value) -> Value {
        match self.target(p) {
            Some(t) => json!({ "range": place_range(&t.place), "placeholder": t.name }),
            None => Value::Null,
        }
    }

    fn rename(&self, id: RequestId, p: &Value) {
        let new_name = p["newName"].as_str().unwrap_or_default().trim().to_string();
        let Some(target) = self.target(p) else {
            return self.client.respond(id, Value::Null);
        };
        if let Err(e) = valid_name(&target.kind, &new_name) {
            return self.client.send(Message::Response(Response::new_err(
                id,
                lsp_server::ErrorCode::InvalidParams as i32,
                e,
            )));
        }
        let client = self.client.clone();
        let (index, roots, open) = (self.index.clone(), self.roots.clone(), self.documents.clone());
        std::thread::spawn(move || {
            let all = index.lock().unwrap().collect(&roots, &open, path_to_uri);
            let definitions = target.definitions(&all);
            let defined = !definitions.is_empty();
            let places = definitions
                .into_iter()
                .filter_map(|e| Some((&e.uri, e.name_place.as_ref()?)))
                .chain(target.named_usages(&all).into_iter().map(|u| (&u.uri, &u.place)));
            let mut changes: HashMap<String, Vec<Value>> = HashMap::new();
            for (uri, place) in places {
                let edit = json!({ "range": place_range(place), "newText": new_name });
                let edits = changes.entry(uri.clone()).or_default();
                if !edits.contains(&edit) {
                    edits.push(edit);
                }
            }
            client.respond(id, json!({ "changes": changes }));
            let old = &target.name;
            client.show(
                3,
                if defined {
                    format!("Kubernetes: renamed in files only. Applying creates `{new_name}`; delete `{old}` from the cluster yourself.")
                } else {
                    format!("Kubernetes: `{old}` isn't defined in this workspace, so only the references were changed.")
                },
            );
        });
    }

    // -- symbols -------------------------------------------------------------

    fn symbols(&self, p: &Value) -> Value {
        let Some((_, text)) = self.text(p) else { return json!([]) };
        let lines = yaml::lines(&text);
        let symbols: Vec<Value> = yaml::parse(&lines)
            .iter()
            .filter(|d| d.kind.is_some())
            .map(|d| {
                let last = d.end.saturating_sub(1);
                let sel = d.kind_line.unwrap_or(d.start);
                json!({
                    "name": d.label(),
                    "detail": d.namespace.clone().or(d.api_version.clone()).unwrap_or_default(),
                    "kind": 19,
                    "range": {
                        "start": { "line": d.start, "character": 0 },
                        "end": { "line": last, "character": utf16_len(lines[last]) }
                    },
                    "selectionRange": {
                        "start": { "line": sel, "character": 0 },
                        "end": { "line": sel, "character": utf16_len(lines[sel]) }
                    }
                })
            })
            .collect();
        Value::Array(symbols)
    }
}

fn run_command(
    client: &Client,
    kubectl: &Kubectl,
    command: &str,
    batches: &[Batch],
    doc: Option<&Doc>,
    label: &str,
) {
    let ctx = kubectl.current_context();
    // Manifest (`-f -`) commands, with the default namespace where it applies.
    let run_manifest = |base: &[&str]| -> Result<String, String> {
        let mut done = String::new();
        for batch in batches {
            match run_batch(kubectl, base, batch) {
                Ok(out) => done.push_str(&out),
                Err(e) if done.is_empty() => return Err(e),
                Err(e) => return Err(format!("{e} (already done: {})", summarize(&done))),
            }
        }
        Ok(done)
    };
    let report = |res: Result<String, String>, verb: &str| match res {
        Ok(out) => client.show(3, format!("[{ctx}] {}", summarize(&out))),
        Err(e) => client.show(1, format!("[{ctx}] {verb} {label} failed: {e}")),
    };
    let open = |res: Result<String, String>, verb: &str, ext: &str| match res {
        Ok(out) => match write_output(verb, label, ext, &out) {
            Ok(path) => {
                client.open_document(&path);
                client.show(3, format!("[{ctx}] {verb} {label} → {}", path.display()));
            }
            Err(e) => client.show(1, format!("could not write {verb} output: {e}")),
        },
        Err(e) => client.show(1, format!("[{ctx}] {verb} {label} failed: {e}")),
    };
    let target = || -> Result<(String, Vec<String>), String> {
        let doc = doc.ok_or("this action needs a single resource")?;
        let kind = safe_arg(doc.kind.as_deref().unwrap_or_default(), "kind")?;
        let name = safe_arg(doc.name.as_deref().ok_or("resource has no metadata.name")?, "name")?;
        let ns = match &doc.namespace {
            Some(ns) => vec!["-n".to_string(), safe_arg(ns, "namespace")?],
            None => Vec::new(),
        };
        Ok((format!("{}/{name}", kind.to_lowercase()), ns))
    };

    match command {
        "kube.apply" => report(run_manifest(&["apply", "-f", "-"]), "apply"),
        "kube.dryRun" => report(run_manifest(&["apply", "--dry-run=server", "--validate=strict", "-f", "-"]), "dry-run"),
        "kube.diff" => {
            // `kubectl diff` exits 1 when there *are* differences.
            let mut diff = String::new();
            for batch in batches {
                let out = kubectl.exec(&batch_args(&["diff", "-f", "-"], batch), Some(&batch.manifest));
                match out.code {
                    Some(0) => {}
                    Some(1) => diff.push_str(&out.stdout),
                    _ => return open(Err(out.error_text()), "diff", "diff"),
                }
            }
            if diff.is_empty() {
                client.show(3, format!("[{ctx}] {label}: no differences with the cluster"));
            } else {
                open(Ok(diff), "diff", "diff");
            }
        }
        "kube.getLive" => open(run_manifest(&["get", "-f", "-", "-o", "yaml"]), "live", "yaml"),
        "kube.describe" => open(run_manifest(&["describe", "-f", "-"]), "describe", "txt"),
        "kube.events" => open(
            target().and_then(|(t, ns)| {
                let mut args = vec!["events".to_string(), "--for".into(), t];
                args.extend(ns);
                kubectl.run(&args.iter().map(String::as_str).collect::<Vec<_>>(), None)
            }),
            "events",
            "txt",
        ),
        "kube.logs" => open(
            target().and_then(|(t, ns)| {
                let mut args =
                    vec!["logs".to_string(), t, "--all-containers=true".into(), "--tail=500".into(), "--prefix".into()];
                args.extend(ns);
                kubectl.run(&args.iter().map(String::as_str).collect::<Vec<_>>(), None)
            }),
            "logs",
            "log",
        ),
        other => client.show(1, format!("unknown command {other}")),
    }
}

// ---------------------------------------------------------------------------
// Helpers

fn batch_args<'a>(base: &[&'a str], batch: &'a Batch) -> Vec<&'a str> {
    base.iter().copied().chain(batch.namespace.iter().map(String::as_str)).collect()
}

fn run_batch(kubectl: &Kubectl, base: &[&str], batch: &Batch) -> Result<String, String> {
    kubectl.run(&batch_args(base, batch), Some(&batch.manifest))
}

/// Names the API server would accept.
fn valid_name(kind: &str, name: &str) -> Result<(), String> {
    let rbac = matches!(kind, "Role" | "ClusterRole" | "RoleBinding" | "ClusterRoleBinding");
    let ok = if rbac {
        !name.is_empty() && !name.contains(['/', '%', ' ']) && name != "." && name != ".."
    } else {
        let edge = |c: Option<char>| c.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        name.len() <= 253
            && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
            && edge(name.chars().next())
            && edge(name.chars().last())
    };
    if ok {
        Ok(())
    } else {
        Err(format!("`{name}` is not a valid {kind} name (use lowercase letters, digits, `-` and `.`)"))
    }
}

fn place_range(p: &Place) -> Value {
    json!({
        "start": { "line": p.line, "character": utf16_len(&p.text[..p.start]) },
        "end": { "line": p.line, "character": utf16_len(&p.text[..p.end]) }
    })
}

fn completion_items(
    doc: &schema::Document,
    kind: &str,
    spot: &Spot,
    line: usize,
    text: &str,
    cursor: usize,
) -> Vec<Value> {
    let range = |start: usize, end: usize| {
        json!({
            "start": { "line": line, "character": utf16_len(&text[..start]) },
            "end": { "line": line, "character": utf16_len(&text[..end]) }
        })
    };
    let value_items = |schema: &Value, range: Value, prefix: &str| -> Vec<Value> {
        doc.allowed_values(schema)
            .into_iter()
            .map(|v| {
                json!({
                    "label": v,
                    "kind": 12,
                    "filterText": v,
                    "textEdit": { "range": range, "newText": format!("{prefix}{v}") }
                })
            })
            .collect()
    };
    match spot {
        Spot::Value { path, col, needs_space } => {
            let Some(schema) = doc.at(kind, path) else { return Vec::new() };
            if doc.is_list(schema) {
                return Vec::new(); // list items go on their own lines
            }
            value_items(schema, range(*col, cursor), if *needs_space { " " } else { "" })
        }
        Spot::Key { parent, col, in_item, taken, end, has_colon } => {
            let Some(schema) = doc.at(kind, parent) else { return Vec::new() };
            let range = range(*col, *end);
            let dash = if doc.is_list(schema) && !in_item && taken.is_empty() { "- " } else { "" };
            let fields = doc.fields(schema);
            if fields.is_empty() {
                // A list of plain values, such as `accessModes`.
                return if doc.is_list(schema) { value_items(schema, range, dash) } else { Vec::new() };
            }
            let pad = " ".repeat(col + dash.len() + 2);
            fields
                .into_iter()
                .filter(|f| !taken.contains(&f.name))
                .map(|f| {
                    let tail = match f.shape {
                        _ if *has_colon => String::new(),
                        Shape::Object => format!(":\n{pad}"),
                        Shape::List => format!(":\n{pad}- "),
                        Shape::Scalar | Shape::Any => ": ".to_string(),
                    };
                    let required = if f.required { " (required)" } else { "" };
                    json!({
                        "label": f.name,
                        "kind": 10,
                        "detail": format!("{}{required}", f.type_label),
                        "documentation": { "kind": "markdown", "value": f.description },
                        "filterText": f.name,
                        "sortText": format!("{}{}", if f.required { 0 } else { 1 }, f.name),
                        // The text carries its own indentation.
                        "insertTextMode": 1,
                        "textEdit": { "range": range, "newText": format!("{dash}{}{tail}", f.name) }
                    })
                })
                .collect()
        }
    }
}

/// Everything about one document that needs no server-side dry-run.
struct Analysis {
    kubectl: Kubectl,
    options: Options,
    roots: Vec<PathBuf>,
    open: HashMap<String, String>,
    index: Arc<Mutex<refs::Index>>,
    schemas: Arc<schema::Store>,
    cluster: Arc<cluster::Cache>,
}

impl Analysis {
    fn run(&self, uri: &str, lines: &[&str], docs: &[Doc], patch: bool, refresh: bool) -> Vec<Value> {
        let mut problems: Vec<Problem> = Vec::new();
        let objects = || docs.iter().filter(|d| d.is_cluster_object(lines));
        if self.options.schema_validation {
            for doc in objects() {
                let (Some(api), Some(kind)) = (&doc.api_version, &doc.kind) else { continue };
                let Some(schema) = self.schemas.get(&self.kubectl, api, refresh) else { continue };
                if let Some(root) = tree::parse(lines, doc) {
                    problems.extend(schema.validate(kind, &root, patch));
                }
            }
        }
        if self.options.check_references && !patch {
            let all = self.index.lock().unwrap().collect(&self.roots, &self.open, path_to_uri);
            let cluster = cluster::View { cache: &self.cluster, kubectl: &self.kubectl, refresh };
            for doc in objects() {
                problems.extend(checks::check(uri, lines, doc, &all, &cluster));
            }
        }
        problems.iter().map(|p| problem_diagnostic(lines, p)).collect()
    }
}

fn problem_diagnostic(lines: &[&str], p: &Problem) -> Value {
    let text = lines.get(p.pos.line).copied().unwrap_or_default();
    let utf16 = |chars: usize| text.chars().take(chars).map(char::len_utf16).sum::<usize>();
    let Pos { line, start, end } = p.pos;
    let start = if end == usize::MAX { start.max(text.chars().take_while(|c| c.is_whitespace()).count()) } else { start };
    json!({
        "range": {
            "start": { "line": line, "character": utf16(start) },
            "end": { "line": line, "character": utf16(end.max(start + 1)) }
        },
        "severity": p.severity,
        "source": "kubernetes",
        "message": p.message
    })
}

fn location(uri: &str, line: usize) -> Value {
    json!({
        "uri": uri,
        "range": { "start": { "line": line, "character": 0 }, "end": { "line": line, "character": 0 } }
    })
}

/// Fetches an object from the cluster into a read-only scratch file.
fn live_object_location(client: &Client, kubectl: &Kubectl, kind: &str, name: &str, namespace: Option<&str>) -> Value {
    let args = (|| -> Result<Vec<String>, String> {
        let mut args = vec!["get".into(), safe_arg(kind, "kind")?, safe_arg(name, "name")?, "-o".into(), "yaml".into()];
        if let Some(ns) = namespace.filter(|_| !refs::is_cluster_scoped(kind)) {
            args.extend(["-n".into(), safe_arg(ns, "namespace")?]);
        }
        Ok(args)
    })();
    let fetched = args.and_then(|a| kubectl.run(&a.iter().map(String::as_str).collect::<Vec<_>>(), None));
    // Jumping to a reference shouldn't leave credentials in a file.
    let fetched = fetched.map(|t| if kind == "Secret" { manifest::redact_secret(&t) } else { t });
    match fetched.and_then(|yaml_text| {
        write_output("live", &format!("{kind}/{name}"), "yaml", &yaml_text)
            .map_err(|e| e.to_string())
            .map(|path| (path, yaml_text))
    }) {
        Ok((path, yaml_text)) => {
            let uri = path_to_uri(&path);
            let line = refs::scan(&uri, &yaml_text).entries.first().map(|e| e.line).unwrap_or(0);
            client.show(3, format!("Kubernetes: {kind}/{name} is not in this workspace; opened the live object from the cluster"));
            location(&uri, line)
        }
        Err(e) => {
            client.show(3, format!("Kubernetes: {kind}/{name} not found in the workspace or cluster ({})", e.lines().next().unwrap_or_default()));
            Value::Null
        }
    }
}

fn local_diagnostics(lines: &[&str], docs: &[Doc]) -> Vec<Value> {
    let mut diags = Vec::new();
    for doc in docs.iter().filter(|d| d.api_version.is_some() || d.kind.is_some()) {
        let mut push = |line: usize, msg: &str| {
            diags.push(diagnostic(lines, line, 1, msg.to_string()));
        };
        if doc.kind.is_none() {
            push(doc.start, "Kubernetes resource is missing `kind`");
        }
        if doc.api_version.is_none() {
            push(doc.kind_line.unwrap_or(doc.start), "Kubernetes resource is missing `apiVersion`");
        }
        let exempt = doc
            .kind
            .as_deref()
            .is_some_and(|k| k.ends_with("List") || k == "Kustomization" || k.ends_with("Config"));
        let generated = lines[doc.start..doc.end].iter().any(|l| l.trim_start().starts_with("generateName:"));
        if doc.kind.is_some() && doc.name.is_none() && !exempt && !generated {
            push(doc.kind_line.unwrap_or(doc.start), "Kubernetes resource is missing `metadata.name`");
        }
    }
    diags
}

fn server_diagnostic(lines: &[&str], doc: &Doc, err: &str) -> Value {
    // Try to point at the exact offending field, e.g. `unknown field "spec.replica"`.
    let line = ["unknown field \"", "duplicate field \""]
        .iter()
        .find_map(|needle| {
            let start = err.find(needle)? + needle.len();
            let field = &err[start..start + err[start..].find('"')?];
            yaml::find_path_line(lines, doc, field)
        })
        .or(doc.kind_line)
        .unwrap_or(doc.start);
    let severity = if err.contains("NotFound") { 2 } else { 1 };
    let message = err.trim_start_matches("Error from server").trim_start_matches(|c: char| c != ':' && c != ' ');
    diagnostic(lines, line, severity, format!("cluster dry-run: {}", message.trim_start_matches([':', ' '])))
}

fn diagnostic(lines: &[&str], line: usize, severity: u8, message: String) -> Value {
    let text = lines.get(line).copied().unwrap_or_default();
    let indent = utf16_len(&text[..text.len() - text.trim_start().len()]);
    json!({
        "range": {
            "start": { "line": line, "character": indent },
            "end": { "line": line, "character": utf16_len(text) }
        },
        "severity": severity,
        "source": "kubernetes",
        "message": message
    })
}

fn summarize(out: &str) -> String {
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    match lines.len() {
        0 => "done".into(),
        1..=6 => lines.join("; "),
        n => format!("{}; … ({} more)", lines[..5].join("; "), n - 5),
    }
}

fn write_output(verb: &str, label: &str, ext: &str, content: &str) -> std::io::Result<std::path::PathBuf> {
    let safe: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' })
        .collect();
    let path = session::output_dir().join(format!("{verb}-{safe}.{ext}"));
    session::write_private(&path, content.as_bytes())?;
    Ok(path)
}

fn path_to_uri(path: &std::path::Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from(if raw.starts_with('/') { "file://" } else { "file:///" });
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                decoded.push(b);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    let mut path = String::from_utf8(decoded).ok()?;
    // file:///C:/x -> C:/x on Windows
    if cfg!(windows) && path.len() > 2 && path.as_bytes()[0] == b'/' && path.as_bytes()[2] == b':' {
        path.remove(0);
    }
    Some(path.into())
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Converts an LSP UTF-16 column to a byte index into `line`.
fn byte_index(line: &str, utf16_col: usize) -> usize {
    let mut units = 0;
    for (i, c) in line.char_indices() {
        if units >= utf16_col {
            return i;
        }
        units += c.len_utf16();
    }
    line.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_roundtrip() {
        let p = std::path::Path::new(if cfg!(windows) { r"C:\Users\a b\x.yaml" } else { "/home/a b/x.yaml" });
        let back = uri_to_path(&path_to_uri(p)).unwrap();
        assert_eq!(back.to_string_lossy().replace('\\', "/"), p.to_string_lossy().replace('\\', "/"));
    }

    #[test]
    fn kubeconfig_is_never_a_cluster_object() {
        let src = "apiVersion: v1\nkind: Config\nclusters: []\ncontexts: []\n";
        let lines = yaml::lines(src);
        let docs = yaml::parse(&lines);
        assert!(docs[0].is_kubeconfig());
        assert!(!docs[0].is_cluster_object(&lines));
    }

    #[test]
    fn uri_encoding() {
        assert_eq!(
            path_to_uri(std::path::Path::new(r"C:\Users\a b\x.yaml")),
            "file:///C:/Users/a%20b/x.yaml"
        );
    }

    #[test]
    fn points_server_error_at_field() {
        let src = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: x\nspec:\n  replica: 2\n";
        let lines = yaml::lines(src);
        let docs = yaml::parse(&lines);
        let d = server_diagnostic(
            &lines,
            &docs[0],
            "Error from server (BadRequest): error when creating \"STDIN\": Deployment in version \"v1\" cannot be handled as a Deployment: strict decoding error: unknown field \"spec.replica\"",
        );
        assert_eq!(d["range"]["start"]["line"], 5);
    }
}
