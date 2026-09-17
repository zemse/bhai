//! Prompt cache guard: every request of a conversation must be an append-only
//! extension of the one before it, or the cached prefix is lost. The monitor then
//! checks that the server actually served the prefix from its cache.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};

use crate::client::Usage;

/// The request fields that must stay byte-identical for the life of a conversation.
const FIXED: [&str; 9] = [
    "model",
    "reasoning",
    "prompt_cache_key",
    "instructions",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "include",
    "store",
];

/// Why a request would miss the cached prefix of the one before it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CacheBreak {
    /// The first differing field, or `input[i]` for a history item.
    pub field: String,
    pub detail: String,
}

/// A request body, serialized piecewise so the next one can be compared cheaply.
struct Recorded {
    fixed: Vec<String>,
    input: Vec<String>,
}

/// Remembers one conversation's last request body and checks the next against it.
pub struct CacheGuard {
    conversation: String,
    previous: Option<Recorded>,
    /// Where breaks are appended as JSONL, if anywhere.
    log: Option<PathBuf>,
    strict: bool,
}

impl CacheGuard {
    pub fn new(conversation: impl Into<String>, log: Option<PathBuf>, strict: bool) -> Self {
        Self {
            conversation: conversation.into(),
            previous: None,
            log,
            strict,
        }
    }

    pub fn strict(&self) -> bool {
        self.strict
    }

    /// Check `body` before it is sent and remember it. A break is logged and returned;
    /// in strict mode it is an error instead and `body` is not remembered.
    pub fn check(&mut self, body: &Value) -> Result<Option<CacheBreak>> {
        let next = record(body);
        let found = self.previous.as_ref().and_then(|prev| compare(prev, &next));
        if let Some(found) = &found {
            if let Some(path) = &self.log
                && let Err(e) = self.append(path, found)
            {
                eprintln!("bhai: cache log: {e:#}");
            }
            if self.strict {
                bail!("cache break: {} ({})", found.field, found.detail);
            }
        }
        self.previous = Some(next);
        Ok(found)
    }

    /// Remember `body` as sent without checking it, for a conversation resumed from disk.
    pub fn seed(&mut self, body: &Value) {
        self.previous = Some(record(body));
    }

    /// Forget the last request, for a break that is intended, such as a compaction.
    #[allow(dead_code)] // no intended break exists yet
    pub fn reset(&mut self, reason: &str) {
        if self.previous.take().is_some()
            && let Some(path) = &self.log
        {
            let reset = CacheBreak {
                field: "reset".to_string(),
                detail: reason.to_string(),
            };
            let _ = self.append(path, &reset);
        }
    }

    fn append(&self, path: &Path, found: &CacheBreak) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let line = json!({
            "timestamp": chrono::Local::now().to_rfc3339(),
            "conversation": self.conversation,
            "field": found.field,
            "detail": found.detail,
        });
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("could not open {}", path.display()))?;
        writeln!(file, "{line}")?;
        Ok(())
    }
}

/// Prompts shorter than this many tokens are never cached.
pub const MIN_CACHED: u64 = 1024;
/// Cached prefixes grow in steps of this many tokens.
const CACHE_STEP: u64 = 128;
/// A call this long after the previous one may find its prefix evicted.
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
/// A hit ratio below this is a miss.
const MISS_RATIO: f64 = 0.5;
/// Consecutive misses that pause the session.
pub const MAX_MISSES: usize = 3;

/// How much of one call's input the cache was expected to serve, and how much it did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Hit {
    /// `None` when the call was not judged.
    pub expected_cached: Option<u64>,
    /// Cached over expected tokens, when judged.
    pub hit_ratio: Option<f64>,
}

impl Hit {
    pub fn miss(&self) -> bool {
        self.hit_ratio.is_some_and(|r| r < MISS_RATIO)
    }
}

/// The tokens a call can expect from the cache when the previous call sent `input`.
pub fn expected_cached(input: u64) -> u64 {
    if input < MIN_CACHED {
        return 0;
    }
    input / CACHE_STEP * CACHE_STEP
}

/// Judges each call of one conversation against the cache the previous call left.
#[derive(Debug, Default)]
pub struct CacheMonitor {
    /// The previous call's input tokens and when it finished.
    previous: Option<(u64, Instant)>,
    /// When the current call was sent, and whether its request broke the cache.
    sent: Option<(Instant, bool)>,
    misses: usize,
}

impl CacheMonitor {
    /// Note a request going out, with the guard's verdict on it.
    pub fn sent(&mut self, found: Option<&CacheBreak>, now: Instant) {
        self.sent = Some((now, found.is_some()));
    }

    /// Judge a finished call. Only a call sent within the cache lifetime of the previous
    /// one, with a clean request and an expected prefix, is judged.
    pub fn observe(&mut self, usage: &Usage, now: Instant) -> Hit {
        let (at, broke) = self.sent.take().unwrap_or((now, false));
        let expected = self
            .previous
            .filter(|(_, done)| !broke && at.saturating_duration_since(*done) < CACHE_TTL)
            .map(|(input, _)| expected_cached(input))
            .filter(|expected| *expected > 0);
        self.previous = Some((usage.input, now));
        let hit = Hit {
            expected_cached: expected,
            hit_ratio: expected.map(|e| usage.cached as f64 / e as f64),
        };
        if hit.miss() {
            self.misses += 1;
        } else if hit.hit_ratio.is_some() {
            self.misses = 0;
        }
        hit
    }

    /// Whether enough calls missed in a row that the user should be asked to go on.
    pub fn tripped(&self) -> bool {
        self.misses >= MAX_MISSES
    }

    /// The user chose to go on; count misses afresh.
    pub fn resume(&mut self) {
        self.misses = 0;
    }
}

fn record(body: &Value) -> Recorded {
    let text = |v: &Value| serde_json::to_string(v).unwrap_or_default();
    Recorded {
        fixed: FIXED
            .iter()
            .map(|f| body.get(*f).map(text).unwrap_or_default())
            .collect(),
        input: body
            .get("input")
            .and_then(Value::as_array)
            .map(|items| items.iter().map(text).collect())
            .unwrap_or_default(),
    }
}

/// The first way `next` fails to extend `prev`, if any.
fn compare(prev: &Recorded, next: &Recorded) -> Option<CacheBreak> {
    for ((field, a), b) in FIXED.iter().zip(&prev.fixed).zip(&next.fixed) {
        if a != b {
            return Some(CacheBreak {
                field: field.to_string(),
                detail: difference(a, b),
            });
        }
    }
    if next.input.len() < prev.input.len() {
        return Some(CacheBreak {
            field: "input".to_string(),
            detail: format!(
                "shrank from {} to {} items",
                prev.input.len(),
                next.input.len()
            ),
        });
    }
    prev.input
        .iter()
        .zip(&next.input)
        .position(|(a, b)| a != b)
        .map(|i| CacheBreak {
            field: format!("input[{i}]"),
            detail: difference(&prev.input[i], &next.input[i]),
        })
}

fn difference(a: &str, b: &str) -> String {
    let at = a
        .bytes()
        .zip(b.bytes())
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()));
    format!(
        "{} bytes, was {}, first difference at byte {at}",
        b.len(),
        a.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(text: &str) -> Value {
        json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] })
    }

    fn body(instructions: &str, tools: Vec<Value>, input: &[Value]) -> Value {
        crate::client::request_body("m", "medium", "key", instructions, &tools, input)
    }

    fn tools() -> Vec<Value> {
        vec![json!({ "type": "function", "name": "bash" })]
    }

    fn guard() -> CacheGuard {
        CacheGuard::new("c1", None, false)
    }

    fn field(found: Option<CacheBreak>) -> String {
        found.expect("a break").field
    }

    #[test]
    fn appending_to_the_input_is_clean() {
        let mut guard = guard();
        let one = [message("a")];
        let two = [message("a"), message("b"), message("c")];
        assert_eq!(guard.check(&body("i", tools(), &one)).unwrap(), None);
        assert_eq!(guard.check(&body("i", tools(), &two)).unwrap(), None);
    }

    #[test]
    fn a_retry_of_the_same_body_is_clean() {
        let mut guard = guard();
        let same = body("i", tools(), &[message("a")]);
        assert_eq!(guard.check(&same).unwrap(), None);
        assert_eq!(guard.check(&same).unwrap(), None);
    }

    #[test]
    fn changed_instructions_break() {
        let mut guard = guard();
        guard.check(&body("i", tools(), &[])).unwrap();
        let found = guard.check(&body("j", tools(), &[])).unwrap().unwrap();
        assert_eq!(found.field, "instructions");
        assert!(
            found.detail.contains("first difference at byte 1"),
            "{found:?}"
        );
    }

    #[test]
    fn changed_tools_break() {
        let mut guard = guard();
        guard.check(&body("i", tools(), &[])).unwrap();
        let mut more = tools();
        more.push(json!({ "type": "function", "name": "read" }));
        assert_eq!(field(guard.check(&body("i", more, &[])).unwrap()), "tools");
    }

    #[test]
    fn a_mutated_earlier_item_breaks() {
        let mut guard = guard();
        guard
            .check(&body("i", tools(), &[message("a"), message("b")]))
            .unwrap();
        let edited = [message("a"), message("B"), message("c")];
        assert_eq!(
            field(guard.check(&body("i", tools(), &edited)).unwrap()),
            "input[1]"
        );
    }

    #[test]
    fn a_removed_item_breaks() {
        let mut guard = guard();
        guard
            .check(&body("i", tools(), &[message("a"), message("b")]))
            .unwrap();
        let found = guard.check(&body("i", tools(), &[message("b")])).unwrap();
        assert_eq!(field(found), "input");
    }

    #[test]
    fn the_break_is_logged_and_clears_on_the_next_clean_call() {
        let dir = std::env::temp_dir().join(format!("bhai-cache-{}", uuid::Uuid::new_v4()));
        let path = dir.join("debug").join("cache.jsonl");
        let mut guard = CacheGuard::new("c1", Some(path.clone()), false);
        guard.check(&body("i", tools(), &[])).unwrap();
        assert!(guard.check(&body("j", tools(), &[])).unwrap().is_some());
        let one = [message("a")];
        assert_eq!(guard.check(&body("j", tools(), &one)).unwrap(), None);

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["conversation"], "c1");
        assert_eq!(lines[0]["field"], "instructions");
        assert!(lines[0]["timestamp"].is_string());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn strict_mode_errors_and_keeps_the_last_sent_body() {
        let mut guard = CacheGuard::new("c1", None, true);
        guard.check(&body("i", tools(), &[])).unwrap();
        let err = guard.check(&body("j", tools(), &[])).unwrap_err();
        assert!(
            err.to_string().contains("cache break: instructions"),
            "{err}"
        );
        assert_eq!(guard.check(&body("i", tools(), &[])).unwrap(), None);
    }

    fn usage(input: u64, cached: u64) -> Usage {
        Usage {
            input,
            cached,
            ..Usage::default()
        }
    }

    /// Send and finish one call a second after `at`; returns the call's end.
    fn call(monitor: &mut CacheMonitor, at: Instant, u: Usage, broke: bool) -> (Hit, Instant) {
        let found = broke.then(|| CacheBreak {
            field: "tools".to_string(),
            detail: String::new(),
        });
        monitor.sent(found.as_ref(), at);
        let done = at + Duration::from_secs(1);
        (monitor.observe(&u, done), done)
    }

    #[test]
    fn expected_cache_rounds_down_to_whole_steps() {
        assert_eq!(expected_cached(1023), 0);
        assert_eq!(expected_cached(1024), 1024);
        assert_eq!(expected_cached(1300), 1280);
        assert_eq!(expected_cached(6672), 6656);
    }

    #[test]
    fn a_hit_is_judged_and_the_first_call_is_not() {
        let mut monitor = CacheMonitor::default();
        let (first, t) = call(&mut monitor, Instant::now(), usage(2000, 0), false);
        assert_eq!(first, Hit::default());
        let (hit, _) = call(&mut monitor, t, usage(2100, 1920), false);
        assert_eq!(hit.expected_cached, Some(1920));
        assert_eq!(hit.hit_ratio, Some(1.0));
        assert!(!hit.miss());
    }

    #[test]
    fn a_miss_is_below_half() {
        let mut monitor = CacheMonitor::default();
        let (_, t) = call(&mut monitor, Instant::now(), usage(2000, 0), false);
        let (hit, _) = call(&mut monitor, t, usage(2100, 128), false);
        assert!(hit.miss(), "{hit:?}");
        assert!(!monitor.tripped());
    }

    #[test]
    fn an_idle_gap_a_break_or_a_small_prompt_is_not_judged() {
        let mut monitor = CacheMonitor::default();
        let (_, t) = call(&mut monitor, Instant::now(), usage(2000, 0), false);
        let (idle, t) = call(&mut monitor, t + CACHE_TTL, usage(2000, 0), false);
        assert_eq!(idle, Hit::default());
        let (broke, t) = call(&mut monitor, t, usage(2000, 0), true);
        assert_eq!(broke, Hit::default());
        let (_, t) = call(&mut monitor, t, usage(1000, 0), false);
        let (small, _) = call(&mut monitor, t, usage(1100, 0), false);
        assert_eq!(small, Hit::default());
    }

    #[test]
    fn three_misses_trip_the_breaker_and_a_hit_resets_it() {
        let mut monitor = CacheMonitor::default();
        let (_, mut t) = call(&mut monitor, Instant::now(), usage(2000, 0), false);
        for _ in 0..2 {
            t = call(&mut monitor, t, usage(2000, 0), false).1;
        }
        // An unjudged call neither counts nor resets.
        t = call(&mut monitor, t, usage(2000, 0), true).1;
        assert!(!monitor.tripped());
        t = call(&mut monitor, t, usage(2000, 0), false).1;
        assert!(monitor.tripped());
        t = call(&mut monitor, t, usage(2000, 1920), false).1;
        assert!(!monitor.tripped());

        for _ in 0..3 {
            t = call(&mut monitor, t, usage(2000, 0), false).1;
        }
        assert!(monitor.tripped());
        monitor.resume();
        assert!(!monitor.tripped());
    }

    #[test]
    fn a_seeded_guard_checks_the_next_call_against_the_seed() {
        let mut guard = CacheGuard::new("c1", None, true);
        guard.seed(&body("i", tools(), &[message("a")]));
        let next = body("i", tools(), &[message("a"), message("b")]);
        assert_eq!(guard.check(&next).unwrap(), None);
        let mut guard = CacheGuard::new("c1", None, true);
        guard.seed(&body("i", tools(), &[message("a")]));
        assert!(guard.check(&body("j", tools(), &[message("a")])).is_err());
    }

    #[test]
    fn reset_allows_a_named_break() {
        let mut guard = CacheGuard::new("c1", None, true);
        guard.check(&body("i", tools(), &[message("a")])).unwrap();
        guard.reset("compaction");
        assert_eq!(guard.check(&body("j", tools(), &[])).unwrap(), None);
        assert!(guard.check(&body("k", tools(), &[])).is_err());
    }
}
