//! Cached answers to small questions about the live cluster.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::kubectl::{is_connectivity_error, safe_arg, Kubectl};

/// How long answers stay usable when nothing newer may be fetched.
const STALE_AFTER: Duration = Duration::from_secs(600);
/// How old an answer may be before a save fetches it again.
const REFRESH_AFTER: Duration = Duration::from_secs(10);
const RETRY_AFTER: Duration = Duration::from_secs(30);

type Names = Arc<HashSet<String>>;
type Answers = HashMap<String, (Instant, Option<Names>)>;

#[derive(Default)]
pub struct Cache {
    answers: Mutex<Answers>,
    /// Clusters that didn't answer, so one outage isn't one timeout per question.
    unreachable: Mutex<HashMap<String, Instant>>,
}

/// What the checks may ask. `None` = unknown (offline, not allowed, …).
pub trait Lookup {
    /// Names of all objects of a kind; `namespace` is `None` for the default.
    fn names(&self, kind: &str, namespace: Option<&str>) -> Option<Names>;
    /// Whether any pod carries these labels.
    fn any_pod(&self, namespace: Option<&str>, labels: &[(String, String)]) -> Option<bool>;
}

/// A view of the cache for one cluster. Without `refresh` it never calls the
/// cluster, for use while typing.
pub struct View<'a> {
    pub cache: &'a Cache,
    pub kubectl: &'a Kubectl,
    pub refresh: bool,
}

impl View<'_> {
    fn ask(&self, args: &[String]) -> Option<Names> {
        let identity = self.kubectl.identity();
        let default = self.kubectl.default_namespace().unwrap_or_default();
        let key = format!("{identity}|{default}|{}", args.join(" "));
        let known = self.cache.answers.lock().unwrap().get(&key).cloned();
        let max_age = if self.refresh { REFRESH_AFTER } else { STALE_AFTER };
        if let Some((at, answer)) = known {
            if at.elapsed() < max_age {
                return answer;
            }
        }
        let down = self.cache.unreachable.lock().unwrap().get(&identity).is_some_and(|t| t.elapsed() < RETRY_AFTER);
        if !self.refresh || down {
            return None;
        }
        let out = self.kubectl.exec(&args.iter().map(String::as_str).collect::<Vec<_>>(), None);
        let answer = if out.success {
            // `configmap/web` or `deployment.apps/web`
            let names = out.stdout.lines().filter_map(|l| Some(l.trim().split_once('/')?.1.to_string()));
            Some(Arc::new(names.collect::<HashSet<_>>()))
        } else {
            if is_connectivity_error(&out.error_text()) {
                self.cache.unreachable.lock().unwrap().insert(identity, Instant::now());
            }
            None
        };
        self.cache.answers.lock().unwrap().insert(key, (Instant::now(), answer.clone()));
        answer
    }
}

fn namespaced(mut args: Vec<String>, namespace: Option<&str>) -> Option<Vec<String>> {
    if let Some(ns) = namespace {
        args.extend(["--namespace".to_string(), safe_arg(ns, "namespace").ok()?]);
    }
    Some(args)
}

impl Lookup for View<'_> {
    fn names(&self, kind: &str, namespace: Option<&str>) -> Option<Names> {
        let kind = safe_arg(kind, "kind").ok()?.to_lowercase();
        self.ask(&namespaced(vec!["get".into(), kind, "-o".into(), "name".into()], namespace)?)
    }

    fn any_pod(&self, namespace: Option<&str>, labels: &[(String, String)]) -> Option<bool> {
        let selector = labels.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",");
        let args = vec!["get".into(), "pods".into(), format!("--selector={selector}"), "-o".into(), "name".into()];
        Some(!self.ask(&namespaced(args, namespace)?)?.is_empty())
    }
}
