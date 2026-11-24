//! Field completion and validation from the cluster's OpenAPI v3 schemas.
//!
//! The API server publishes one OpenAPI document per group/version, CRDs
//! included, under a URL that carries a content hash. Documents are cached on
//! disk by that hash, so they are downloaded once and keep working offline.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::kubectl::Kubectl;
use crate::session::{state_dir, write_private};
use crate::tree::{self, Node, Pos};
use crate::yaml::{self, Doc};

/// How often to look for new or changed CRDs.
const INDEX_TTL: Duration = Duration::from_secs(300);
/// How long to leave an unreachable cluster alone.
const RETRY_AFTER: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Fetching and caching

#[derive(Default)]
pub struct Store {
    indexes: Mutex<HashMap<String, (Instant, Duration, Paths)>>,
    documents: Mutex<HashMap<PathBuf, Arc<Document>>>,
}

/// `apis/apps/v1` → server-relative URL with hash.
type Paths = Arc<HashMap<String, String>>;

impl Store {
    /// The schemas of one `apiVersion`. `None` when the cluster doesn't serve
    /// it, or was never reachable. Without `fetch` only cached schemas are
    /// used, for work that must not wait for the cluster.
    pub fn get(&self, kubectl: &Kubectl, api_version: &str, fetch: bool) -> Option<Arc<Document>> {
        let key = if api_version.contains('/') { format!("apis/{api_version}") } else { format!("api/{api_version}") };
        let dir = cache_dir(kubectl);
        let url = self.paths(kubectl, &dir, fetch)?.get(&key)?.clone();
        let file = dir.join(document_file(&key, &url));
        if let Some(doc) = self.documents.lock().unwrap().get(&file) {
            return Some(doc.clone());
        }
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(_) => {
                // The path comes from the server; don't let it point elsewhere.
                if !fetch || !url.starts_with("/openapi/v3/") {
                    return None;
                }
                let t = kubectl.run(&["get", "--raw", &url], None).ok()?;
                remove_older(&dir, &key);
                let _ = write_private(&file, t.as_bytes());
                t
            }
        };
        let doc = Arc::new(Document::parse(&text, api_version)?);
        self.documents.lock().unwrap().insert(file, doc.clone());
        Some(doc)
    }

    fn paths(&self, kubectl: &Kubectl, dir: &std::path::Path, fetch: bool) -> Option<Paths> {
        let identity = kubectl.identity();
        if !fetch {
            // Don't wait for a fetch in progress either.
            return self.indexes.try_lock().ok()?.get(&identity).map(|(.., paths)| paths.clone());
        }
        // Held while fetching, so parallel requests don't all hit the server.
        let mut indexes = self.indexes.lock().unwrap();
        if let Some((at, ttl, paths)) = indexes.get(&identity) {
            if at.elapsed() < *ttl {
                return Some(paths.clone());
            }
        }
        let file = dir.join("index.json");
        let (text, ttl) = match kubectl.run(&["get", "--raw", "/openapi/v3"], None) {
            Ok(t) => {
                let _ = write_private(&file, t.as_bytes());
                (t, INDEX_TTL)
            }
            Err(_) => (std::fs::read_to_string(&file).unwrap_or_default(), RETRY_AFTER),
        };
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let paths: Paths = Arc::new(
            parsed["paths"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k.clone(), v["serverRelativeURL"].as_str()?.to_string())))
                .collect(),
        );
        indexes.insert(identity, (Instant::now(), ttl, paths.clone()));
        Some(paths)
    }
}

fn cache_dir(kubectl: &Kubectl) -> PathBuf {
    let mut h = DefaultHasher::new();
    kubectl.identity().hash(&mut h);
    state_dir().join("openapi").join(format!("{:016x}", h.finish()))
}

fn file_prefix(key: &str) -> String {
    let safe: String = key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '_' }).collect();
    format!("{safe}@")
}

fn document_file(key: &str, url: &str) -> String {
    let mut h = DefaultHasher::new();
    url.hash(&mut h);
    format!("{}{:016x}.json", file_prefix(key), h.finish())
}

fn remove_older(dir: &std::path::Path, key: &str) {
    let prefix = file_prefix(key);
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

// ---------------------------------------------------------------------------
// Navigating a schema

pub struct Document {
    schemas: Map<String, Value>,
    kinds: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    Scalar,
    Object,
    List,
    /// Free-form (`IntOrString`, embedded JSON, …): anything goes.
    Any,
}

pub struct Field {
    pub name: String,
    pub type_label: String,
    pub description: String,
    pub required: bool,
    pub shape: Shape,
}

impl Document {
    pub fn parse(text: &str, api_version: &str) -> Option<Self> {
        let mut root: Value = serde_json::from_str(text).ok()?;
        let Value::Object(schemas) = root["components"]["schemas"].take() else { return None };
        let (group, version) = api_version.split_once('/').unwrap_or(("", api_version));
        let mut kinds = HashMap::new();
        for (name, schema) in &schemas {
            for gvk in schema["x-kubernetes-group-version-kind"].as_array().into_iter().flatten() {
                if gvk["group"].as_str() == Some(group) && gvk["version"].as_str() == Some(version) {
                    if let Some(kind) = gvk["kind"].as_str() {
                        kinds.insert(kind.to_string(), name.clone());
                    }
                }
            }
        }
        Some(Self { schemas, kinds })
    }

    pub fn kind(&self, kind: &str) -> Option<&Value> {
        self.schemas.get(self.kinds.get(kind)?)
    }

    /// Follows `$ref`, also when wrapped as `allOf: [{$ref}]` (how OpenAPI v3
    /// attaches a description or default to a reference).
    fn resolve<'a>(&'a self, mut schema: &'a Value) -> &'a Value {
        for _ in 0..16 {
            let target = schema["$ref"].as_str().or_else(|| match schema["allOf"].as_array() {
                Some(all) if all.len() == 1 && schema.get("properties").is_none() => all[0]["$ref"].as_str(),
                _ => None,
            });
            match target.and_then(|t| self.schemas.get(t.strip_prefix("#/components/schemas/")?)) {
                Some(next) => schema = next,
                None => break,
            }
        }
        schema
    }

    fn shape(&self, schema: &Value) -> Shape {
        let s = self.resolve(schema);
        match s["type"].as_str() {
            Some("object") => Shape::Object,
            Some("array") => Shape::List,
            Some("string" | "integer" | "number" | "boolean") if !int_or_string(s) => Shape::Scalar,
            _ if s.get("properties").is_some() => Shape::Object,
            _ => Shape::Any,
        }
    }

    /// The schema for `key` inside `schema`, looking through list items.
    fn child<'a>(&'a self, schema: &'a Value, key: &str) -> Option<&'a Value> {
        let s = self.element(schema);
        s["properties"]
            .get(key)
            .or_else(|| s.get("additionalProperties").filter(|v| v.is_object()))
    }

    /// `schema` itself, or the item schema if it is a (nested) list.
    fn element<'a>(&'a self, schema: &'a Value) -> &'a Value {
        let mut s = self.resolve(schema);
        for _ in 0..8 {
            match s.get("items") {
                Some(items) => s = self.resolve(items),
                None => break,
            }
        }
        s
    }

    /// Schema at a key path such as `["spec", "containers", "image"]`.
    pub fn at<'a, S: AsRef<str>>(&'a self, kind: &str, path: &[S]) -> Option<&'a Value> {
        path.iter().try_fold(self.kind(kind)?, |s, key| self.child(s, key.as_ref()))
    }

    pub fn fields(&self, schema: &Value) -> Vec<Field> {
        let s = self.element(schema);
        let required = required_keys(s);
        s["properties"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, property)| Field {
                name: name.clone(),
                type_label: self.type_label(property),
                description: self.description(property),
                required: required.contains(&name.as_str()),
                shape: self.shape(property),
            })
            .collect()
    }

    fn description(&self, schema: &Value) -> String {
        schema["description"]
            .as_str()
            .or_else(|| self.resolve(schema)["description"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    /// e.g. `string`, `[]Container`, `map[string]string`.
    fn type_label(&self, schema: &Value) -> String {
        let name = schema["$ref"].as_str().or_else(|| schema["allOf"][0]["$ref"].as_str());
        if let Some(n) = name.and_then(|r| r.rsplit(['.', '/']).next()) {
            return n.to_string();
        }
        if int_or_string(schema) {
            return "IntOrString".into();
        }
        match schema["type"].as_str() {
            Some("array") => format!("[]{}", schema.get("items").map_or("any".into(), |i| self.type_label(i))),
            Some("object") => match schema.get("additionalProperties").filter(|v| v.is_object()) {
                Some(v) => format!("map[string]{}", self.type_label(v)),
                None => "object".into(),
            },
            Some(t) => t.to_string(),
            None => "any".into(),
        }
    }

    /// Values worth offering for a scalar (or for the items of a list).
    pub fn allowed_values(&self, schema: &Value) -> Vec<String> {
        let s = self.element(schema);
        if let Some(values) = s["enum"].as_array() {
            return values.iter().filter_map(Value::as_str).filter(|v| !v.is_empty()).map(String::from).collect();
        }
        match s["type"].as_str() {
            Some("boolean") => vec!["true".into(), "false".into()],
            _ => schema["default"].as_str().filter(|d| !d.is_empty()).map(String::from).into_iter().collect(),
        }
    }

    pub fn is_list(&self, schema: &Value) -> bool {
        self.shape(schema) == Shape::List
    }
}

fn int_or_string(s: &Value) -> bool {
    s["format"].as_str() == Some("int-or-string") || s["x-kubernetes-int-or-string"] == Value::Bool(true)
}

fn required_keys(s: &Value) -> Vec<&str> {
    s["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect()
}

// ---------------------------------------------------------------------------
// Validation

#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    pub pos: Pos,
    /// LSP severity: 1 = error, 2 = warning.
    pub severity: u8,
    pub message: String,
}

impl Document {
    /// Checks an object against its schema. `partial` is for patches, which
    /// legitimately leave required fields out.
    pub fn validate(&self, kind: &str, root: &Node, partial: bool) -> Vec<Problem> {
        let mut out = Vec::new();
        if let Some(schema) = self.kind(kind) {
            self.check(schema, root, "", partial, &mut out);
        }
        out
    }

    fn check(&self, schema: &Value, node: &Node, path: &str, partial: bool, out: &mut Vec<Problem>) {
        let s = self.resolve(schema);
        let problem = |pos: Pos, severity: u8, message: String| Problem { pos, severity, message };
        let field = path.rsplit('.').next().filter(|f| !f.is_empty()).unwrap_or("this object");
        let wrong_type = |want: &str, got: &str| {
            problem(node.pos, 1, format!("`{field}` should be {want}, not {got}"))
        };
        match &node.value {
            tree::Value::Unknown => {}
            tree::Value::Map(entries) => {
                match self.shape(schema) {
                    Shape::Any => return,
                    Shape::Object => {}
                    Shape::List => return out.push(wrong_type("a list (start items with `- `)", "a mapping")),
                    Shape::Scalar => return out.push(wrong_type(&article(&s["type"]), "a mapping")),
                }
                let properties = s["properties"].as_object();
                let extra = s.get("additionalProperties");
                let merged = entries.iter().any(|(k, _)| k.name == "<<");
                let open = s["x-kubernetes-preserve-unknown-fields"] == Value::Bool(true)
                    || extra == Some(&Value::Bool(true))
                    || properties.is_none();
                for (key, value) in entries.iter().filter(|(k, _)| k.name != "<<") {
                    let sub = if path.is_empty() { key.name.clone() } else { format!("{path}.{}", key.name) };
                    match properties.and_then(|p| p.get(&key.name)).or(extra.filter(|e| e.is_object())) {
                        Some(next) => self.check(next, value, &sub, partial, out),
                        None if open => {}
                        None => {
                            let known = properties.into_iter().flatten().map(|(k, _)| k.as_str());
                            let hint = closest(&key.name, known)
                                .map(|k| format!("; did you mean `{k}`?"))
                                .unwrap_or_default();
                            out.push(problem(key.pos, 1, format!("unknown field `{sub}`{hint}")));
                        }
                    }
                }
                if !partial && !merged {
                    let missing: Vec<String> = required_keys(s)
                        .into_iter()
                        .filter(|r| !entries.iter().any(|(k, _)| k.name == *r))
                        // A defaulted field may be omitted even if "required".
                        .filter(|r| !has_default(&s["properties"][*r]))
                        .map(|r| format!("`{r}`"))
                        .collect();
                    if !missing.is_empty() {
                        let plural = if missing.len() > 1 { "s" } else { "" };
                        let message = format!("missing required field{plural} {}", missing.join(", "));
                        out.push(problem(node.pos, 2, message));
                    }
                }
            }
            tree::Value::Seq(items) => match self.shape(schema) {
                Shape::Any => {}
                Shape::List => {
                    if let Some(item) = s.get("items") {
                        for value in items {
                            self.check(item, value, path, partial, out);
                        }
                    }
                }
                Shape::Object => out.push(wrong_type("a mapping", "a list")),
                Shape::Scalar => out.push(wrong_type(&article(&s["type"]), "a list")),
            },
            tree::Value::Scalar { text, plain } => {
                let value = if *plain { plain_type(text) } else { Plain::Str };
                if value == Plain::Null {
                    return; // same as leaving the field out
                }
                match self.shape(schema) {
                    Shape::Any => return,
                    Shape::Object => return out.push(wrong_type("a mapping", value.describe())),
                    Shape::List => return out.push(wrong_type("a list (start items with `- `)", value.describe())),
                    Shape::Scalar => {}
                }
                let ok = match s["type"].as_str() {
                    Some("string") => matches!(value, Plain::Str | Plain::Ambiguous),
                    Some("integer") => matches!(value, Plain::Int | Plain::Ambiguous) || whole(text, value),
                    Some("number") => matches!(value, Plain::Int | Plain::Float | Plain::Ambiguous),
                    Some("boolean") => matches!(value, Plain::Bool | Plain::Ambiguous),
                    _ => true,
                };
                if !ok {
                    let quote = if s["type"].as_str() == Some("string") { "; put it in quotes" } else { "" };
                    let mut p = wrong_type(&article(&s["type"]), value.describe());
                    p.message.push_str(quote);
                    return out.push(p);
                }
                let allowed: Vec<&str> =
                    s["enum"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                if !allowed.is_empty() && !text.is_empty() && !allowed.contains(&text.as_str()) {
                    let hint = closest(text, allowed.iter().copied())
                        .map(|k| format!("; did you mean `{k}`?"))
                        .unwrap_or_default();
                    let message = format!("`{field}` must be one of {}{hint}", allowed.join(", "));
                    out.push(problem(node.pos, 1, message));
                }
            }
        }
    }
}

/// OpenAPI v3 writes `default: {}` on every nested struct; that is not a
/// usable default for a field the user has to fill in.
fn has_default(property: &Value) -> bool {
    match property.get("default") {
        None => false,
        Some(Value::Object(m)) => !m.is_empty(),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(v) => !v.is_null(),
    }
}

fn article(schema_type: &Value) -> String {
    match schema_type.as_str() {
        Some("integer") => "an integer".into(),
        Some(t) => format!("a {t}"),
        None => "a scalar".into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Plain {
    Null,
    Bool,
    Int,
    Float,
    Str,
    /// Parsers disagree (`y`, `1_000`), so accept it anywhere.
    Ambiguous,
}

impl Plain {
    fn describe(self) -> &'static str {
        match self {
            Plain::Bool => "a boolean",
            Plain::Int | Plain::Float => "a number",
            Plain::Null => "null",
            Plain::Str | Plain::Ambiguous => "a string",
        }
    }
}

/// What kubectl's YAML 1.1 parser makes of an unquoted scalar.
fn plain_type(text: &str) -> Plain {
    match text {
        "" | "~" | "null" | "Null" | "NULL" => return Plain::Null,
        "true" | "True" | "TRUE" | "false" | "False" | "FALSE" | "yes" | "Yes" | "YES" | "no" | "No" | "NO"
        | "on" | "On" | "ON" | "off" | "Off" | "OFF" => return Plain::Bool,
        "y" | "Y" | "n" | "N" => return Plain::Ambiguous,
        ".inf" | "-.inf" | "+.inf" | ".nan" | ".NaN" | ".Inf" => return Plain::Float,
        _ => {}
    }
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    if !digits.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
        return Plain::Str;
    }
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(rest) = digits.strip_prefix(prefix) {
            let valid = !rest.is_empty() && rest.chars().all(|c| c.is_digit(radix));
            return if valid { Plain::Int } else { Plain::Str };
        }
    }
    if digits.bytes().all(|b| b.is_ascii_digit()) {
        return Plain::Int;
    }
    if digits.contains('_') {
        let numeric = digits.bytes().all(|b| b.is_ascii_digit() || b == b'_' || b == b'.');
        return if numeric { Plain::Ambiguous } else { Plain::Str };
    }
    let numeric = digits.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'));
    if numeric && digits.parse::<f64>().is_ok() {
        Plain::Float
    } else {
        Plain::Str
    }
}

/// `3.0` and `1e3` are integers once converted to JSON.
fn whole(text: &str, value: Plain) -> bool {
    value == Plain::Float && text.parse::<f64>().is_ok_and(|f| f.is_finite() && f.fract() == 0.0)
}

/// The most similar candidate, if it is plausibly a typo.
fn closest<'a>(word: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let lower = word.to_lowercase();
    candidates
        .map(|c| (distance(&lower, &c.to_lowercase()), c))
        .filter(|(d, _)| *d <= 2 && *d < word.len().div_ceil(2))
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substituted = previous + usize::from(ca != *cb);
            previous = row[j + 1];
            row[j + 1] = substituted.min(previous + 1).min(row[j] + 1);
        }
    }
    row[b.len()]
}

// ---------------------------------------------------------------------------
// Completion

#[derive(Debug, PartialEq)]
pub enum Spot {
    /// A (possibly empty) key is being typed.
    Key {
        parent: Vec<String>,
        /// Byte column of the key.
        col: usize,
        /// Whether a `- ` precedes the key on this line.
        in_item: bool,
        /// Keys already present in this mapping.
        taken: Vec<String>,
        /// Byte column where the replaced word ends.
        end: usize,
        /// The key already has its colon.
        has_colon: bool,
    },
    /// The value after `key:` is being typed.
    Value { path: Vec<String>, col: usize, needs_space: bool },
}

/// What the cursor at byte `col` of `line` is completing.
pub fn spot(lines: &[&str], doc: &Doc, line: usize, col: usize) -> Option<Spot> {
    let text = *lines.get(line)?;
    let before = text.get(..col)?;
    if before.trim_start().starts_with('#') || yaml::in_block_scalar(lines, doc, line, col) {
        return None;
    }
    let key_col = yaml::content_col(before);
    let typed = &before[key_col..];
    if let Some((c, key, _)) = yaml::key_of(before) {
        let mut path = yaml::ancestors(lines, doc, line, c);
        path.push(key.to_string());
        let colon = c + text[c..].find(':')?;
        let value = &before[colon + 1..];
        let start = colon + 1 + (value.len() - value.trim_start().len());
        return Some(Spot::Value { path, col: start, needs_space: value.is_empty() });
    }
    if !typed.chars().all(|c| c.is_ascii_alphanumeric() || "-_./$".contains(c)) || typed.starts_with('-') {
        return None;
    }
    let rest = &text[col..];
    let word = rest.find(|c: char| !(c.is_ascii_alphanumeric() || "-_./$".contains(c))).unwrap_or(rest.len());
    let in_item = before.trim_start().starts_with('-');
    if in_item && before.ends_with('-') {
        return None; // `-` still needs its space
    }
    let taken = yaml::mapping_keys(lines, doc, line, key_col, in_item);
    Some(Spot::Key {
        parent: yaml::ancestors(lines, doc, line, key_col),
        col: key_col,
        in_item,
        taken,
        end: col + word,
        has_colon: rest[word..].starts_with(':'),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENAPI: &str = r##"{"components":{"schemas":{
      "Deployment":{"type":"object","x-kubernetes-group-version-kind":[{"group":"apps","version":"v1","kind":"Deployment"}],
        "properties":{"apiVersion":{"type":"string"},"kind":{"type":"string"},
          "metadata":{"default":{},"allOf":[{"$ref":"#/components/schemas/Meta"}]},
          "spec":{"default":{},"allOf":[{"$ref":"#/components/schemas/Spec"}]}}},
      "Meta":{"type":"object","properties":{"name":{"type":"string"},"labels":{"type":"object","additionalProperties":{"type":"string","default":""}}}},
      "Spec":{"type":"object","required":["selector","containers"],"properties":{
          "replicas":{"type":"integer"},"paused":{"type":"boolean"},
          "selector":{"type":"object"},
          "raw":{"type":"object","x-kubernetes-preserve-unknown-fields":true,"properties":{"a":{"type":"string"}}},
          "port":{"allOf":[{"$ref":"#/components/schemas/IntOrString"}]},
          "modes":{"type":"array","items":{"type":"string","enum":["ReadWriteOnce","ReadOnlyMany"]}},
          "containers":{"type":"array","items":{"default":{},"allOf":[{"$ref":"#/components/schemas/Container"}]}}}},
      "IntOrString":{"format":"int-or-string","oneOf":[{"type":"integer"},{"type":"string"}]},
      "Container":{"type":"object","required":["name"],"properties":{
          "name":{"type":"string","default":""},"image":{"type":"string"},
          "env":{"type":"array","items":{"type":"object","properties":{"name":{"type":"string"},"value":{"type":"string"}}}},
          "imagePullPolicy":{"type":"string","enum":["Always","IfNotPresent","Never"]}}}
    }}}"##;

    fn document() -> Document {
        Document::parse(OPENAPI, "apps/v1").unwrap()
    }

    fn problems(src: &str, partial: bool) -> Vec<(usize, String)> {
        let lines = yaml::lines(src);
        let docs = yaml::parse(&lines);
        let root = tree::parse(&lines, &docs[0]).unwrap();
        document().validate("Deployment", &root, partial).into_iter().map(|p| (p.pos.line, p.message)).collect()
    }

    const VALID: &str = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\n  labels: {app: web}\nspec:\n  replicas: 2\n  paused: no\n  selector: {}\n  raw:\n    anything: 1\n  port: http\n  modes: [ReadWriteOnce]\n  containers:\n    - name: app\n      image: nginx\n      env:\n        - name: A\n          value: \"80\"\n";

    #[test]
    fn accepts_valid_objects() {
        assert_eq!(problems(VALID, false), vec![]);
        assert_eq!(problems(&VALID.replace("port: http", "port: 80"), false), vec![]);
        assert_eq!(problems(&VALID.replace("replicas: 2", "replicas:"), false), vec![]);
    }

    #[test]
    fn reports_unknown_fields_with_a_hint() {
        let got = problems(&VALID.replace("replicas", "replica"), false);
        assert_eq!(got, vec![(6, "unknown field `spec.replica`; did you mean `replicas`?".to_string())]);
        let got = problems(&VALID.replace("      image:", "      imagee:"), false);
        assert_eq!(got[0].1, "unknown field `spec.containers.imagee`; did you mean `image`?");
    }

    #[test]
    fn reports_wrong_types() {
        let got = problems(&VALID.replace("\"80\"", "80"), false);
        assert_eq!(got, vec![(18, "`value` should be a string, not a number; put it in quotes".to_string())]);
        assert!(problems(&VALID.replace("app: web", "app: yes"), false)[0].1.contains("not a boolean"));
        assert!(problems(&VALID.replace("replicas: 2", "replicas: two"), false)[0].1.contains("an integer"));
        assert!(problems(&VALID.replace("replicas: 2", "replicas: \"2\""), false)[0].1.contains("not a string"));
        assert!(problems(&VALID.replace("    - name: app\n", "      name: app\n"), false)[0].1.contains("a list"));
        assert!(problems(&VALID.replace("selector: {}", "selector: []"), false)[0].1.contains("a mapping"));
    }

    #[test]
    fn reports_enums_and_missing_fields() {
        let got = problems(&VALID.replace("[ReadWriteOnce]", "[ReadWriteOnc]"), false);
        assert!(got[0].1.contains("did you mean `ReadWriteOnce`"), "{got:?}");
        let src = VALID.replace("- name: app\n      image", "- image");
        assert_eq!(problems(&src, false), vec![(14, "missing required field `name`".to_string())]);
        assert_eq!(problems(&src, true), vec![]);
    }

    #[test]
    fn yaml_scalar_types() {
        for (text, want) in [
            ("80", Plain::Int), ("-1", Plain::Int), ("0644", Plain::Int), ("0x1F", Plain::Int),
            ("1.5", Plain::Float), ("1e3", Plain::Float), ("1.2.3", Plain::Str), ("1Gi", Plain::Str),
            ("v1", Plain::Str), ("on", Plain::Bool), ("", Plain::Null), ("1_000", Plain::Ambiguous),
            ("-", Plain::Str), ("2024-01-01", Plain::Str), (".", Plain::Str), ("10.0.0.1", Plain::Str),
        ] {
            assert_eq!(plain_type(text), want, "{text:?}");
        }
    }

    fn spot_at(src_with_cursor: &str) -> Option<Spot> {
        let (before, _) = src_with_cursor.split_once('^').unwrap();
        let src = src_with_cursor.replacen('^', "", 1);
        let line = before.matches('\n').count();
        let col = before.rsplit('\n').next().unwrap().len();
        let lines = yaml::lines(&src);
        let docs = yaml::parse(&lines);
        let (_, doc) = yaml::doc_at(&docs, line)?;
        spot(&lines, doc, line, col)
    }

    #[test]
    fn locates_keys() {
        let Some(Spot::Key { parent, col, in_item, taken, .. }) =
            spot_at("kind: X\nspec:\n  containers:\n    - name: a\n      im^\n      env: []\n    - name: b\n      image: z\n")
        else {
            panic!()
        };
        assert_eq!((parent.join("."), col, in_item), ("spec.containers".to_string(), 6, false));
        assert_eq!(taken, ["name", "env"]);

        let Some(Spot::Key { parent, in_item, taken, .. }) =
            spot_at("spec:\n  containers:\n  - name: a\n  - ^\n  other: 1\n")
        else {
            panic!()
        };
        assert_eq!((parent.join("."), in_item, taken.len()), ("spec.containers".to_string(), true, 0));

        let Some(Spot::Key { parent, taken, .. }) = spot_at("kind: X\n^\nspec:\n  a: 1\n") else { panic!() };
        assert!(parent.is_empty());
        assert_eq!(taken, ["kind", "spec"]);
    }

    #[test]
    fn locates_values() {
        assert_eq!(
            spot_at("spec:\n  containers:\n    - imagePullPolicy: Al^\n"),
            Some(Spot::Value {
                path: vec!["spec".into(), "containers".into(), "imagePullPolicy".into()],
                col: 23,
                needs_space: false
            })
        );
        assert!(matches!(spot_at("spec:\n  paused:^\n"), Some(Spot::Value { needs_space: true, .. })));
    }

    #[test]
    fn stays_out_of_text() {
        assert_eq!(spot_at("data:\n  a.yaml: |\n    spec:\n      ^\n"), None);
        assert_eq!(spot_at("spec:\n  # rep^\n"), None);
        assert_eq!(spot_at("spec:\n  args:\n    - run it^\n"), None);
        assert!(spot_at("data:\n  a.yaml: |\n    spec:\n  ^\n").is_some());
    }

    #[test]
    fn offers_fields_and_values() {
        let d = document();
        let names = |path: &[&str]| -> Vec<String> {
            d.fields(d.at("Deployment", path).unwrap()).into_iter().map(|f| f.name).collect()
        };
        assert!(names(&["spec", "containers"]).contains(&"imagePullPolicy".to_string()));
        assert_eq!(names(&["spec", "containers", "env"]), ["name", "value"]);
        assert_eq!(d.allowed_values(d.at("Deployment", &["spec", "paused"]).unwrap()), ["true", "false"]);
        assert_eq!(d.allowed_values(d.at("Deployment", &["spec", "modes"]).unwrap()).len(), 2);
        let spec = d.fields(d.at("Deployment", &["spec"]).unwrap());
        let field = |n: &str| spec.iter().find(|f| f.name == n).unwrap();
        assert_eq!(field("containers").type_label, "[]Container");
        assert_eq!((field("containers").shape, field("containers").required), (Shape::List, true));
        assert_eq!(field("port").shape, Shape::Any);
    }
}
