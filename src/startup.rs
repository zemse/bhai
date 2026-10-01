//! `BHAI_STARTUP_TIMING=1` prints how long each startup stage took to stderr, one JSON
//! line per stage as it ends, so a slow start can be pinned on the config, MCP or the
//! backend. The lines carry stage names and times only, nothing from the config.

use std::sync::Mutex;
use std::time::Instant;

static CLOCK: Mutex<Option<Clock>> = Mutex::new(None);

struct Clock {
    start: Instant,
    last: Instant,
}

impl Clock {
    fn new(now: Instant) -> Self {
        Self {
            start: now,
            last: now,
        }
    }

    /// The line for the stage that ends at `now`, which starts the next one.
    fn mark(&mut self, stage: &str, now: Instant) -> String {
        let ms =
            |since: Instant| (now.duration_since(since).as_secs_f64() * 10_000.0).round() / 10.0;
        let line = serde_json::json!({
            "stage": stage,
            "ms": ms(self.last),
            "total_ms": ms(self.start),
        });
        self.last = now;
        line.to_string()
    }
}

fn enabled(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

/// Starts the clock when the variable asks for it; call first thing in `main`.
pub fn begin() {
    if enabled(std::env::var("BHAI_STARTUP_TIMING").ok().as_deref()) {
        *lock() = Some(Clock::new(Instant::now()));
    }
}

/// Ends the stage in progress under `stage`.
pub fn mark(stage: &str) {
    if let Some(clock) = lock().as_mut() {
        eprintln!("{}", clock.mark(stage, Instant::now()));
    }
}

/// Ends the last stage and stops the clock, so nothing after it is written to a terminal
/// the TUI is drawing on.
pub fn end(stage: &str) {
    mark(stage);
    *lock() = None;
}

fn lock() -> std::sync::MutexGuard<'static, Option<Clock>> {
    CLOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn each_stage_is_timed_from_the_last_and_from_the_start() {
        let start = Instant::now();
        let mut clock = Clock::new(start);
        let first: serde_json::Value =
            serde_json::from_str(&clock.mark("config", start + Duration::from_micros(1_260)))
                .unwrap();
        assert_eq!(
            first,
            serde_json::json!({"stage": "config", "ms": 1.3, "total_ms": 1.3})
        );
        let second: serde_json::Value =
            serde_json::from_str(&clock.mark("mcp", start + Duration::from_millis(812))).unwrap();
        assert_eq!(
            second,
            serde_json::json!({"stage": "mcp", "ms": 810.7, "total_ms": 812.0})
        );
    }

    #[test]
    fn only_a_set_non_zero_value_turns_it_on() {
        assert!(enabled(Some("1")));
        assert!(enabled(Some("yes")));
        assert!(!enabled(Some("0")));
        assert!(!enabled(Some("")));
        assert!(!enabled(None));
    }
}
