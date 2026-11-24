//! Model Context Protocol server (stdio, newline-delimited JSON-RPC).
//!
//! Registered by the extension as the `kubernetes` context server, so Zed's
//! Agent panel can browse and operate the cluster — the closest Zed equivalent
//! of the JetBrains "Services" tool window today.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::kubectl::{safe_arg, Kubectl};
use crate::session::{self, Session};
use crate::{manifest, yaml};

const MAX_OUTPUT: usize = 100_000;
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Tools that change which cluster is used (local state only, never the cluster).
const SESSION_TOOLS: &[&str] = &[
    "kubernetes_use_kubeconfig",
    "kubernetes_use_context",
    "kubernetes_use_namespace",
    "kubernetes_reset_session",
];

struct Tool {
    name: &'static str,
    description: &'static str,
    mutating: bool,
    schema: fn() -> Value,
}

fn tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "kubernetes_contexts",
            description: "List kubeconfig contexts and show which one is current.",
            mutating: false,
            schema: || json!({ "type": "object", "properties": {} }),
        },
        Tool {
            name: "kubernetes_use_kubeconfig",
            description: "Connect to the cluster defined in a kubeconfig file for this session (does not modify ~/.kube/config). Affects the editor too. If the file has several contexts and none is given, the file's current-context is used.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "Absolute path to a kubeconfig file" },
                    "context": { "type": "string" }
                }
            }),
        },
        Tool {
            name: "kubernetes_use_context",
            description: "Switch the session to another context of the active kubeconfig.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["context"],
                "properties": { "context": { "type": "string" } }
            }),
        },
        Tool {
            name: "kubernetes_use_namespace",
            description: "Set the default namespace for this session (used when a tool call or manifest names none). Pass an empty string to go back to the context's default.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["namespace"],
                "properties": { "namespace": { "type": "string" } }
            }),
        },
        Tool {
            name: "kubernetes_reset_session",
            description: "Forget the session cluster selection and go back to the default kubeconfig/context.",
            mutating: false,
            schema: || json!({ "type": "object", "properties": {} }),
        },
        Tool {
            name: "kubernetes_api_resources",
            description: "List resource types the cluster serves (kinds, short names, API groups, namespaced or not).",
            mutating: false,
            schema: || json!({ "type": "object", "properties": {} }),
        },
        Tool {
            name: "kubernetes_get",
            description: "List or fetch resources (like `kubectl get`). Use output=yaml to read a full object.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["resource"],
                "properties": {
                    "resource": { "type": "string", "description": "Resource type, e.g. pods, deployments, svc, crd name" },
                    "name": { "type": "string" },
                    "namespace": { "type": "string" },
                    "all_namespaces": { "type": "boolean" },
                    "label_selector": { "type": "string", "description": "e.g. app=web,tier!=db" },
                    "output": { "type": "string", "enum": ["wide", "yaml", "json", "name"], "default": "wide" }
                }
            }),
        },
        Tool {
            name: "kubernetes_describe",
            description: "Describe a resource, including status, conditions and recent events.",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["resource"],
                "properties": {
                    "resource": { "type": "string" },
                    "name": { "type": "string" },
                    "namespace": { "type": "string" },
                    "label_selector": { "type": "string" }
                }
            }),
        },
        Tool {
            name: "kubernetes_logs",
            description: "Fetch container logs for a pod or workload (target may be `pod-name` or `deployment/web`).",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["target"],
                "properties": {
                    "target": { "type": "string" },
                    "namespace": { "type": "string" },
                    "container": { "type": "string" },
                    "tail": { "type": "integer", "default": 200 },
                    "previous": { "type": "boolean", "description": "Logs from the previous (crashed) container instance" }
                }
            }),
        },
        Tool {
            name: "kubernetes_events",
            description: "Show recent cluster events, optionally for one object (e.g. for=pod/web-123).",
            mutating: false,
            schema: || json!({
                "type": "object",
                "properties": {
                    "namespace": { "type": "string" },
                    "all_namespaces": { "type": "boolean" },
                    "for": { "type": "string" }
                }
            }),
        },
        Tool {
            name: "kubernetes_explain",
            description: "Documentation for a resource or field path (e.g. deployment.spec.strategy).",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string" },
                    "api_version": { "type": "string" }
                }
            }),
        },
        Tool {
            name: "kubernetes_top",
            description: "CPU/memory usage of pods or nodes (requires metrics-server).",
            mutating: false,
            schema: || json!({
                "type": "object",
                "required": ["kind"],
                "properties": {
                    "kind": { "type": "string", "enum": ["pods", "nodes"] },
                    "namespace": { "type": "string" },
                    "all_namespaces": { "type": "boolean" }
                }
            }),
        },
        Tool {
            name: "kubernetes_apply",
            description: "Apply a YAML manifest to the cluster. Set dry_run=true to validate without changing anything.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["manifest"],
                "properties": {
                    "manifest": { "type": "string", "description": "One or more YAML documents" },
                    "namespace": { "type": "string" },
                    "dry_run": { "type": "boolean", "default": false }
                }
            }),
        },
        Tool {
            name: "kubernetes_delete",
            description: "Delete a resource from the cluster.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["resource", "name"],
                "properties": {
                    "resource": { "type": "string" },
                    "name": { "type": "string" },
                    "namespace": { "type": "string" }
                }
            }),
        },
        Tool {
            name: "kubernetes_scale",
            description: "Scale a Deployment, StatefulSet or ReplicaSet.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["resource", "name", "replicas"],
                "properties": {
                    "resource": { "type": "string", "description": "e.g. deployment" },
                    "name": { "type": "string" },
                    "replicas": { "type": "integer", "minimum": 0 },
                    "namespace": { "type": "string" }
                }
            }),
        },
        Tool {
            name: "kubernetes_rollout_restart",
            description: "Trigger a rolling restart of a Deployment, StatefulSet or DaemonSet.",
            mutating: true,
            schema: || json!({
                "type": "object",
                "required": ["resource", "name"],
                "properties": {
                    "resource": { "type": "string" },
                    "name": { "type": "string" },
                    "namespace": { "type": "string" }
                }
            }),
        },
    ]
}

pub fn run(kubectl: Kubectl, read_only: bool) -> Result<(), String> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&kubectl, read_only, &msg),
            Err(e) => Some(json!({
                "jsonrpc": "2.0", "id": null,
                "error": { "code": -32700, "message": format!("parse error: {e}") }
            })),
        };
        if let Some(resp) = response {
            writeln!(stdout, "{resp}").map_err(|e| e.to_string())?;
            stdout.flush().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn handle(kubectl: &Kubectl, read_only: bool, msg: &Value) -> Option<Value> {
    let id = msg.get("id")?.clone(); // notifications get no response
    let params = &msg["params"];
    let result = match msg["method"].as_str().unwrap_or_default() {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION),
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "kubernetes", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Tools for inspecting and operating the Kubernetes cluster selected by the user's kubeconfig. Prefer read-only tools; confirm intent before apply/delete/scale/restart."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({
            "tools": tools()
                .iter()
                .filter(|t| !(read_only && t.mutating))
                .map(|t| json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": (t.schema)(),
                    "annotations": {
                        "readOnlyHint": !t.mutating && !SESSION_TOOLS.contains(&t.name),
                        "destructiveHint": t.mutating
                    }
                }))
                .collect::<Vec<_>>()
        })),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default();
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let mutating = tools().iter().find(|t| t.name == name).map(|t| t.mutating);
            let outcome = match mutating {
                None => Err(format!("unknown tool: {name}")),
                Some(true) if read_only => Err("server is running in read-only mode".into()),
                Some(_) => call_tool(kubectl, name, &args),
            };
            let (text, is_error) = match outcome {
                Ok(t) => (t, false),
                Err(e) => (e, true),
            };
            Ok(json!({ "content": [{ "type": "text", "text": truncate(text) }], "isError": is_error }))
        }
        other => Err((-32601, format!("method not found: {other}"))),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    })
}

fn truncate(mut text: String) -> String {
    if text.len() > MAX_OUTPUT {
        let mut cut = MAX_OUTPUT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n… output truncated; narrow the query (namespace, label_selector, name).");
    }
    if text.trim().is_empty() {
        "(no output)".into()
    } else {
        text
    }
}

/// Builds kubectl argv from tool arguments, validating every user value.
struct Args<'a> {
    input: &'a Value,
    argv: Vec<String>,
}

impl<'a> Args<'a> {
    fn new(input: &'a Value, base: &[&str]) -> Self {
        Self { input, argv: base.iter().map(|s| s.to_string()).collect() }
    }
    fn str(&self, key: &str) -> Option<&'a str> {
        self.input.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
    }
    fn flag(&self, key: &str) -> bool {
        self.input.get(key).and_then(Value::as_bool).unwrap_or(false)
    }
    fn positional(&mut self, key: &str, required: bool) -> Result<&mut Self, String> {
        match self.str(key) {
            Some(v) => self.argv.push(safe_arg(v, key)?),
            None if required => return Err(format!("missing required argument `{key}`")),
            None => {}
        }
        Ok(self)
    }
    fn option(&mut self, key: &str, flag: &str) -> Result<&mut Self, String> {
        if let Some(v) = self.str(key) {
            self.argv.push(format!("{flag}={v}"));
        }
        Ok(self)
    }
    fn namespace(&mut self) -> Result<&mut Self, String> {
        if self.flag("all_namespaces") {
            self.argv.push("--all-namespaces".into());
            return Ok(self);
        }
        if let Some(ns) = self.str("namespace") {
            self.argv.push("-n".into());
            self.argv.push(safe_arg(ns, "namespace")?);
        }
        Ok(self)
    }
    fn push(&mut self, s: impl Into<String>) -> &mut Self {
        self.argv.push(s.into());
        self
    }
    fn run(&self, kubectl: &Kubectl, stdin: Option<&str>) -> Result<String, String> {
        let argv: Vec<&str> = self.argv.iter().map(String::as_str).collect();
        kubectl.run(&argv, stdin)
    }
}

fn call_tool(kubectl: &Kubectl, name: &str, input: &Value) -> Result<String, String> {
    let mut a = Args::new(input, &[]);
    match name {
        "kubernetes_contexts" => {
            let list = a.push("config").push("get-contexts").run(kubectl, None)?;
            let s = session::load();
            let header = if s.is_active() {
                format!("Session override active: {} (kubernetes_reset_session to undo)\n\n", s.describe())
            } else {
                String::new()
            };
            Ok(format!(
                "{header}Active: {}\nDefault namespace: {}\n\n{list}",
                kubectl.current_context(),
                kubectl.effective_namespace()
            ))
        }
        "kubernetes_use_kubeconfig" => {
            let path = a.str("path").ok_or("missing required argument `path`")?;
            if !std::path::Path::new(path).is_file() {
                return Err(format!("no such file: {path}"));
            }
            let file = kubectl.for_kubeconfig(path, None);
            let contexts = file.contexts()?;
            let context = match a.str("context") {
                Some(c) if contexts.iter().any(|x| x == c) => c.to_string(),
                Some(c) => return Err(format!("context {c:?} not in file; available: {}", contexts.join(", "))),
                None if contexts.len() == 1 => contexts[0].clone(),
                None => file
                    .run(&["config", "current-context"], None)
                    .map(|c| c.trim().to_string())
                    .map_err(|_| format!("file has several contexts; pass one of: {}", contexts.join(", ")))?,
            };
            activate(kubectl, Session { kubeconfig: Some(path.to_string()), context: Some(context), namespace: None })
        }
        "kubernetes_use_context" => {
            let context = a.str("context").ok_or("missing required argument `context`")?;
            let contexts = kubectl.contexts()?;
            if !contexts.iter().any(|c| c == context) {
                return Err(format!("unknown context {context:?}; available: {}", contexts.join(", ")));
            }
            let kubeconfig = session::load().kubeconfig;
            activate(kubectl, Session { kubeconfig, context: Some(context.to_string()), namespace: None })
        }
        "kubernetes_use_namespace" => {
            let mut s = session::load();
            s.namespace = match input.get("namespace").and_then(Value::as_str).unwrap_or_default() {
                "" => None,
                ns => Some(safe_arg(ns, "namespace")?),
            };
            session::save(&s).map_err(|e| e.to_string())?;
            Ok(format!("Default namespace is now {}.", kubectl.effective_namespace()))
        }
        "kubernetes_reset_session" => {
            session::clear().map_err(|e| e.to_string())?;
            Ok(format!("Session cleared. Now using {}", kubectl.current_context()))
        }
        "kubernetes_api_resources" => a.push("api-resources").run(kubectl, None),
        "kubernetes_get" => {
            let output = a.str("output").unwrap_or("wide");
            if !["wide", "yaml", "json", "name"].contains(&output) {
                return Err(format!("unsupported output: {output}"));
            }
            a.push("get").positional("resource", true)?.positional("name", false)?.namespace()?
                .option("label_selector", "--selector")?
                .push(format!("--output={output}"))
                .run(kubectl, None)
        }
        "kubernetes_describe" => a
            .push("describe")
            .positional("resource", true)?
            .positional("name", false)?
            .namespace()?
            .option("label_selector", "--selector")?
            .run(kubectl, None),
        "kubernetes_logs" => {
            let tail = input.get("tail").and_then(Value::as_i64).unwrap_or(200);
            a.push("logs").positional("target", true)?.namespace()?
                .option("container", "--container")?
                .push(format!("--tail={tail}"));
            if a.str("container").is_none() {
                a.push("--all-containers=true").push("--prefix");
            }
            if a.flag("previous") {
                a.push("--previous");
            }
            a.run(kubectl, None)
        }
        "kubernetes_events" => {
            a.push("events").namespace()?.option("for", "--for")?;
            a.run(kubectl, None)
        }
        "kubernetes_explain" => a
            .push("explain")
            .positional("path", true)?
            .option("api_version", "--api-version")?
            .run(kubectl, None),
        "kubernetes_top" => {
            let kind = a.str("kind").unwrap_or("pods");
            if kind != "pods" && kind != "nodes" {
                return Err("kind must be pods or nodes".into());
            }
            a.push("top").push(kind);
            if kind == "pods" {
                a.namespace()?;
            }
            a.run(kubectl, None)
        }
        "kubernetes_apply" => {
            let manifest = a.str("manifest").ok_or("missing required argument `manifest`")?;
            a.push("apply").push("-f").push("-").namespace()?;
            if a.flag("dry_run") {
                a.push("--dry-run=server");
            }
            if a.str("namespace").is_some() {
                return a.run(kubectl, Some(manifest));
            }
            // Documents that name no namespace get the session's default.
            let lines = yaml::lines(manifest);
            let docs = yaml::parse(&lines);
            let docs: Vec<&yaml::Doc> = docs.iter().filter(|d| !d.text(&lines).trim().is_empty()).collect();
            let mut done = String::new();
            for batch in manifest::batches(kubectl.default_namespace().as_deref(), &lines, &docs) {
                let args: Vec<&str> =
                    a.argv.iter().chain(&batch.namespace).map(String::as_str).collect();
                match kubectl.run(&args, Some(&batch.manifest)) {
                    Ok(out) => done.push_str(&out),
                    Err(e) => return Err(format!("{done}{e}")),
                }
            }
            Ok(done)
        }
        "kubernetes_delete" => a
            .push("delete")
            .positional("resource", true)?
            .positional("name", true)?
            .namespace()?
            .run(kubectl, None),
        "kubernetes_scale" => {
            let replicas = input
                .get("replicas")
                .and_then(Value::as_u64)
                .ok_or("`replicas` must be a non-negative integer")?;
            a.push("scale").positional("resource", true)?.positional("name", true)?.namespace()?
                .push(format!("--replicas={replicas}"))
                .run(kubectl, None)
        }
        "kubernetes_rollout_restart" => a
            .push("rollout")
            .push("restart")
            .positional("resource", true)?
            .positional("name", true)?
            .namespace()?
            .run(kubectl, None),
        other => Err(format!("unknown tool: {other}")),
    }
}

fn activate(kubectl: &Kubectl, new: Session) -> Result<String, String> {
    session::save(&new).map_err(|e| e.to_string())?;
    Ok(match kubectl.server_version() {
        Ok(v) => format!("Connected to {} (server {v}). The editor uses this cluster too.", new.describe()),
        Err(e) => format!("Switched to {}, but the cluster is not reachable: {e}", new.describe()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_hides_mutating_tools() {
        let k = Kubectl::default();
        let resp = handle(&k, true, &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })).unwrap();
        let names: Vec<&str> =
            resp["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"kubernetes_get"));
        assert!(!names.contains(&"kubernetes_delete"));
    }

    #[test]
    fn rejects_flag_injection() {
        let k = Kubectl::default();
        let err = call_tool(&k, "kubernetes_get", &json!({ "resource": "--kubeconfig=/evil" })).unwrap_err();
        assert!(err.contains("invalid resource"));
    }

    #[test]
    fn notifications_get_no_reply() {
        let k = Kubectl::default();
        assert!(handle(&k, false, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none());
    }
}
