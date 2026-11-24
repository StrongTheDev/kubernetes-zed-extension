//! Makes the running binary reachable from Zed tasks.
//!
//! Zed keeps the `kube-lsp` it downloads in a versioned directory that tasks
//! cannot refer to. So the language server copies itself to one fixed path
//! (the *launcher*) whenever it starts, and `install-tasks` writes Zed task
//! entries that call that path. Editor, Agent and tasks then run one build
//! without anything having to be on PATH.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::session::state_dir;

/// The shipped task list, with `"command": "kube-lsp"`.
const TEMPLATE: &str = include_str!("../../zed/tasks.json");

pub fn launcher_path() -> PathBuf {
    state_dir().join("bin").join(if cfg!(windows) { "kube-lsp.exe" } else { "kube-lsp" })
}

/// Copies the running executable over the launcher if they differ.
pub fn refresh_launcher() -> std::io::Result<PathBuf> {
    let target = launcher_path();
    let source = std::env::current_exe()?;
    let canonical = |p: &Path| std::fs::canonicalize(p).ok();
    if canonical(&source).is_some() && canonical(&source) == canonical(&target) {
        return Ok(target);
    }
    let dir = target.parent().expect("launcher path has a parent");
    std::fs::create_dir_all(dir)?;
    remove_stale(dir);
    let wanted = std::fs::read(&source)?;
    if std::fs::read(&target).is_ok_and(|have| have == wanted) {
        return Ok(target);
    }
    // Several windows may start at once, so never write in place.
    let temp = dir.join(format!("new-{}", std::process::id()));
    std::fs::copy(&source, &temp)?;
    if cfg!(windows) && target.exists() {
        // A running task locks its executable: it can be renamed, not replaced.
        std::fs::rename(&target, dir.join(format!("old-{}", std::process::id())))?;
    }
    let moved = std::fs::rename(&temp, &target);
    if moved.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    remove_stale(dir);
    moved.map(|()| target)
}

/// Deletes executables set aside by earlier updates (no longer running).
fn remove_stale(dir: &Path) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with("old-") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Zed's user-wide `tasks.json`.
pub fn zed_tasks_path() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    if let Some(p) = env("KUBE_LSP_ZED_TASKS") {
        return Some(p); // tests
    }
    let dir = if cfg!(windows) {
        env("APPDATA")?.join("Zed")
    } else if cfg!(target_os = "macos") {
        env("HOME")?.join(".config/zed")
    } else {
        env("XDG_CONFIG_HOME").or_else(|| Some(env("HOME")?.join(".config")))?.join("zed")
    };
    Some(dir.join("tasks.json"))
}

#[derive(Debug, PartialEq)]
pub enum TaskState {
    Missing,
    Outdated,
    Current,
}

/// Whether the user's tasks file has every task, pointing at the launcher.
pub fn task_state() -> TaskState {
    let text = zed_tasks_path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    match merge(&text, &launcher_path()) {
        Ok(m) if m.added == 0 && m.updated == 0 => TaskState::Current,
        Ok(m) if m.known == 0 => TaskState::Missing,
        _ => TaskState::Outdated,
    }
}

/// Adds missing tasks and repoints existing ones. Everything else in the
/// file (other tasks, comments, customised fields) is left as it is.
pub fn install_tasks() -> Result<String, String> {
    let launcher = refresh_launcher().map_err(|e| format!("could not install the launcher: {e}"))?;
    let path = zed_tasks_path().ok_or("could not find Zed's configuration directory")?;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let merged = merge(&text, &launcher).map_err(|e| format!("{} was not changed: {e}", path.display()))?;
    if merged.added == 0 && merged.updated == 0 {
        return Ok(format!("Zed tasks are already up to date ({}).", path.display()));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, merged.text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!(
        "Zed tasks installed: {} added, {} updated in {}. Find them with \"task: spawn\".",
        merged.added,
        merged.updated,
        path.display()
    ))
}

struct Merged {
    text: String,
    added: usize,
    updated: usize,
    /// Tasks of ours that were already there.
    known: usize,
}

fn merge(src: &str, launcher: &Path) -> Result<Merged, String> {
    let command = Value::String(launcher.to_string_lossy().into_owned());
    let template = parse_jsonc(TEMPLATE)?;
    let template = template.as_array().ok_or("task template is not a list")?;

    let src = if tokens(src)?.is_empty() { format!("{src}[\n]\n") } else { src.to_string() };
    let original = parse_jsonc(&src)?;
    if !original.is_array() {
        return Err("it is not a list of tasks".into());
    }
    let toks = tokens(&src)?;
    let existing = top_level_tasks(&src, &toks);

    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let (mut added, mut updated, mut known) = (0, 0, 0);
    let mut insertion = String::new();
    for task in template {
        let label = task["label"].as_str().unwrap_or_default();
        match existing.iter().find(|t| t.label == label) {
            Some(found) => {
                known += 1;
                if let Some((a, b)) = found.command {
                    if serde_json::from_str::<Value>(&src[a..b]).ok().as_ref() != Some(&command) {
                        updated += 1;
                        edits.push((a, b, command.to_string()));
                    }
                }
            }
            None => {
                added += 1;
                if !insertion.is_empty() {
                    insertion.push(',');
                }
                insertion.push_str("\n  ");
                insertion.push_str(&render_task(task, &command).replace('\n', "\n  "));
            }
        }
    }
    if added > 0 {
        let close = toks.last().ok_or("it is empty")?;
        let before = &toks[toks.len() - 2];
        if !matches!(&src[before.start..before.end], "[" | ",") {
            insertion.insert(0, ',');
        }
        // Right after the last element, so trailing comments stay below it.
        if !src[before.end..close.start].contains('\n') {
            insertion.push('\n');
        }
        edits.push((before.end, before.end, insertion));
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut text = src.clone();
    for (a, b, new) in edits {
        text.replace_range(a..b, &new);
    }
    // Never write a file Zed could not read back.
    let expected = original.as_array().map_or(0, Vec::len) + added;
    if parse_jsonc(&text)?.as_array().map(Vec::len) != Some(expected) {
        return Err("internal error: merged tasks don't add up".into());
    }
    Ok(Merged { text, added, updated, known })
}

/// Keeps the template's key order (serde_json would sort keys).
fn render_task(task: &Value, command: &Value) -> String {
    let mut out = format!("{{\n  \"label\": {},\n  \"command\": {command}", task["label"]);
    for key in ["args", "use_new_terminal", "allow_concurrent_runs", "reveal"] {
        let Some(v) = task.get(key) else { continue };
        let rendered = match v.as_array() {
            Some(items) => format!("[{}]", items.iter().map(Value::to_string).collect::<Vec<_>>().join(", ")),
            None => v.to_string(),
        };
        out.push_str(&format!(",\n  \"{key}\": {rendered}"));
    }
    out.push_str("\n}");
    out
}

struct Token {
    start: usize,
    end: usize,
}

/// JSONC tokens (strings, punctuation, literals); comments are skipped.
fn tokens(src: &str) -> Result<Vec<Token>, String> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = if src.starts_with('\u{feff}') { 3 } else { 0 };
    while i < b.len() {
        let start = i;
        match b[i] {
            c if c.is_ascii_whitespace() => {
                i += 1;
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                i = src[i..].find('\n').map_or(b.len(), |n| i + n);
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = src[i + 2..].find("*/").map(|n| i + n + 4).ok_or("unterminated comment")?;
                continue;
            }
            b'"' => {
                i += 1;
                loop {
                    match b.get(i) {
                        Some(b'\\') => i += 2,
                        Some(b'"') => break,
                        Some(_) => i += 1,
                        None => return Err("unterminated string".into()),
                    }
                }
                i += 1;
            }
            b'{' | b'}' | b'[' | b']' | b',' | b':' => i += 1,
            _ => {
                while i < b.len() && !b"{}[],:\"/".contains(&b[i]) && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i == start {
                    return Err(format!("unexpected character at byte {start}"));
                }
            }
        }
        out.push(Token { start, end: i });
    }
    Ok(out)
}

/// Parses JSON with comments and trailing commas.
fn parse_jsonc(src: &str) -> Result<Value, String> {
    let toks = tokens(src)?;
    let mut clean = String::new();
    for (i, t) in toks.iter().enumerate() {
        let text = &src[t.start..t.end];
        let trailing =
            text == "," && toks.get(i + 1).is_some_and(|n| matches!(&src[n.start..n.end], "]" | "}"));
        if !trailing {
            clean.push_str(text);
            clean.push(' ');
        }
    }
    serde_json::from_str(&clean).map_err(|e| format!("not valid JSON ({e})"))
}

struct ExistingTask {
    label: String,
    /// Byte range of the `command` value.
    command: Option<(usize, usize)>,
}

fn top_level_tasks(src: &str, toks: &[Token]) -> Vec<ExistingTask> {
    let text = |i: usize| toks.get(i).map_or("", |t| &src[t.start..t.end]);
    let mut out: Vec<ExistingTask> = Vec::new();
    let mut depth = 0;
    for i in 0..toks.len() {
        match text(i) {
            "{" | "[" => {
                depth += 1;
                if depth == 2 && text(i) == "{" {
                    out.push(ExistingTask { label: String::new(), command: None });
                }
            }
            "}" | "]" => depth -= 1,
            key if depth == 2 && text(i + 1) == ":" => {
                let (Some(task), Some(value)) = (out.last_mut(), toks.get(i + 2)) else { continue };
                match key {
                    "\"label\"" => {
                        task.label = serde_json::from_str(&src[value.start..value.end]).unwrap_or_default();
                    }
                    "\"command\"" => task.command = Some((value.start, value.end)),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count() -> usize {
        parse_jsonc(TEMPLATE).unwrap().as_array().unwrap().len()
    }

    #[test]
    fn fills_an_empty_file() {
        for src in ["", "// nothing yet\n", "[]", "[\n  // none\n]\n"] {
            let m = merge(src, Path::new("/opt/kube-lsp")).unwrap();
            assert_eq!((m.added, m.updated, m.known), (count(), 0, 0), "{src:?}");
            let again = merge(&m.text, Path::new("/opt/kube-lsp")).unwrap();
            assert_eq!((again.added, again.updated, again.known), (0, 0, count()));
            assert_eq!(again.text, m.text);
        }
    }

    #[test]
    fn keeps_foreign_tasks_comments_and_customisations() {
        let src = "// mine\n[\n  { \"label\": \"build\", \"command\": \"make // not a comment\", },\n  {\n    \"label\": \"Kubernetes: Connection status\",\n    \"command\": \"kube-lsp\", // from PATH\n    \"args\": [\"status\"],\n    \"hide\": \"on_success\"\n  },\n  // end\n]\n";
        let m = merge(src, Path::new(r"C:\a b\kube-lsp.exe")).unwrap();
        assert_eq!((m.added, m.updated, m.known), (count() - 1, 1, 1));
        for kept in ["// mine", "make // not a comment", "// from PATH", "\"hide\": \"on_success\"", "// end"] {
            assert!(m.text.contains(kept), "{kept} missing from\n{}", m.text);
        }
        assert!(m.text.contains(r#""C:\\a b\\kube-lsp.exe""#));
        assert!(!m.text.contains("\"kube-lsp\""));
    }

    #[test]
    fn refuses_files_it_cannot_parse() {
        assert!(merge("[ { \"label\": ", Path::new("x")).is_err());
        assert!(merge("{}", Path::new("x")).is_err());
    }
}
