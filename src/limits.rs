//! Rate-limit headroom as the ChatGPT backend reports it, in response headers
//! (`x-codex-primary-used-percent` and friends, as openai/codex reads them) or in a
//! `codex.rate_limits` stream event.

use std::io::Write;
use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Local, TimeZone};
use reqwest::header::HeaderMap;
use serde::Serialize;
use serde_json::{Value, json};

/// Percent used at which a window turns yellow.
pub const WARN: f64 = 75.0;
/// Percent used at which a window turns red.
pub const ALERT: f64 = 90.0;

/// The latest usage of each limit window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct RateLimits {
    pub primary: Option<Window>,
    pub secondary: Option<Window>,
}

/// One limit window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Window {
    pub used_percent: f64,
    pub window_minutes: Option<u64>,
    /// Unix seconds at which the window resets.
    pub resets_at: Option<i64>,
}

impl RateLimits {
    /// Read the `x-codex-*` headers; `now` is unix seconds, for relative reset times.
    pub fn from_headers(headers: &HeaderMap, now: i64) -> Option<Self> {
        let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        let window = |which: &str| {
            let used = get(&format!("x-codex-{which}-used-percent"))?
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|p| p.is_finite())?;
            let number = |suffix: &str| {
                get(&format!("x-codex-{which}-{suffix}")).and_then(|v| v.trim().parse::<i64>().ok())
            };
            Some(Window {
                used_percent: used,
                window_minutes: number("window-minutes").and_then(|m| u64::try_from(m).ok()),
                resets_at: number("resets-at")
                    .or_else(|| number("reset-after-seconds").map(|s| now + s)),
            })
        };
        Self {
            primary: window("primary"),
            secondary: window("secondary"),
        }
        .some()
    }

    /// Read a `codex.rate_limits` event, with the windows at the top or under `rate_limits`.
    pub fn from_event(event: &Value, now: i64) -> Option<Self> {
        let limits = event.get("rate_limits").unwrap_or(event);
        let window = |which: &str| {
            let w = limits.get(which)?;
            let used = w.get("used_percent")?.as_f64()?;
            let number = |key: &str| w.get(key).and_then(Value::as_i64);
            Some(Window {
                used_percent: used,
                window_minutes: w.get("window_minutes").and_then(Value::as_u64),
                resets_at: number("resets_at")
                    .or_else(|| number("reset_at"))
                    .or_else(|| number("reset_after_seconds").map(|s| now + s)),
            })
        };
        Self {
            primary: window("primary"),
            secondary: window("secondary"),
        }
        .some()
    }

    fn some(self) -> Option<Self> {
        (self.primary.is_some() || self.secondary.is_some()).then_some(self)
    }

    /// The windows present, in order.
    pub fn windows(&self) -> impl Iterator<Item = Window> {
        [self.primary, self.secondary].into_iter().flatten()
    }
}

impl Window {
    /// A short name for the window's length: `5h`, `wk`, `90m`, `24h`.
    pub fn label(&self) -> String {
        match self.window_minutes {
            None => "?".to_string(),
            Some(m) if m <= 300 && m % 60 != 0 => format!("{m}m"),
            Some(m) if m <= 300 => format!("{}h", m / 60),
            Some(10080) => "wk".to_string(),
            Some(m) if m % 60 == 0 => format!("{}h", m / 60),
            Some(m) => format!("{m}m"),
        }
    }

    /// How long until the window resets: `43m`, or `2h14m`. Past a day it is the local
    /// weekday and time instead, which is how far off a weekly window usually is and
    /// what a count of hours stops saying anything about.
    pub fn resets_in(&self, now: DateTime<Local>) -> Option<String> {
        let at = Local.timestamp_opt(self.resets_at?, 0).single()?;
        let minutes = (at - now).num_minutes();
        Some(if minutes <= 0 {
            "now".to_string()
        } else if minutes < 60 {
            format!("{minutes}m")
        } else if minutes < 24 * 60 {
            match (minutes / 60, minutes % 60) {
                (hours, 0) => format!("{hours}h"),
                (hours, rest) => format!("{hours}h{rest}m"),
            }
        } else {
            at.format("%a %H:%M").to_string()
        })
    }
}

/// Append every `x-codex-*` and `x-ratelimit-*` header to the JSONL file at `path`.
pub fn log_headers(path: &Path, headers: &HeaderMap) -> Result<()> {
    let picked: serde_json::Map<String, Value> = headers
        .iter()
        .filter(|(name, _)| {
            let name = name.as_str();
            name.starts_with("x-codex-") || name.starts_with("x-ratelimit-")
        })
        .map(|(name, value)| {
            let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
            (name.as_str().to_string(), Value::String(value))
        })
        .collect();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let line = json!({
        "timestamp": chrono::Local::now().to_rfc3339(),
        "headers": picked,
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(k, v)| {
                (
                    HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    HeaderValue::from_str(v).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn reads_both_reset_forms() {
        let map = headers(&[
            ("x-codex-primary-used-percent", "42.5"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-after-seconds", "60"),
            ("x-codex-secondary-used-percent", "17"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-resets-at", "1700000000"),
        ]);
        let limits = RateLimits::from_headers(&map, 1000).unwrap();
        assert_eq!(
            limits.primary,
            Some(Window {
                used_percent: 42.5,
                window_minutes: Some(300),
                resets_at: Some(1060),
            })
        );
        assert_eq!(
            limits.secondary,
            Some(Window {
                used_percent: 17.0,
                window_minutes: Some(10080),
                resets_at: Some(1_700_000_000),
            })
        );
    }

    #[test]
    fn missing_headers_give_nothing() {
        assert_eq!(RateLimits::from_headers(&HeaderMap::new(), 0), None);
        let map = headers(&[("x-codex-primary-window-minutes", "300")]);
        assert_eq!(RateLimits::from_headers(&map, 0), None);
    }

    #[test]
    fn garbage_is_tolerated() {
        let map = headers(&[
            ("x-codex-primary-used-percent", "lots"),
            ("x-codex-secondary-used-percent", "5"),
            ("x-codex-secondary-window-minutes", "-3"),
            ("x-codex-secondary-resets-at", "soon"),
        ]);
        let limits = RateLimits::from_headers(&map, 0).unwrap();
        assert_eq!(limits.primary, None);
        assert_eq!(
            limits.secondary,
            Some(Window {
                used_percent: 5.0,
                window_minutes: None,
                resets_at: None,
            })
        );
        let nan = headers(&[("x-codex-primary-used-percent", "NaN")]);
        assert_eq!(RateLimits::from_headers(&nan, 0), None);
    }

    #[test]
    fn reads_the_stream_event() {
        let event = json!({
            "type": "codex.rate_limits",
            "rate_limits": {
                "primary": { "used_percent": 80.0, "window_minutes": 300, "reset_at": 5 },
                "secondary": null,
            }
        });
        let limits = RateLimits::from_event(&event, 0).unwrap();
        assert_eq!(limits.primary.unwrap().resets_at, Some(5));
        assert_eq!(limits.secondary, None);
        assert_eq!(RateLimits::from_event(&json!({"type": "x"}), 0), None);
    }

    #[test]
    fn labels_follow_the_window_length() {
        let label = |m| {
            Window {
                used_percent: 0.0,
                window_minutes: m,
                resets_at: None,
            }
            .label()
        };
        assert_eq!(label(Some(300)), "5h");
        assert_eq!(label(Some(90)), "90m");
        assert_eq!(label(Some(10080)), "wk");
        assert_eq!(label(Some(1440)), "24h");
        assert_eq!(label(Some(1000)), "1000m");
        assert_eq!(label(None), "?");
    }

    #[test]
    fn a_reset_counts_down_until_it_is_a_day_off() {
        // On a whole second, since a reset is unix seconds and the fraction would eat
        // a minute off every count below.
        let now = Local
            .timestamp_opt(Local::now().timestamp(), 0)
            .single()
            .unwrap();
        let in_minutes = |m: i64| {
            Window {
                used_percent: 0.0,
                window_minutes: None,
                resets_at: Some((now + chrono::TimeDelta::minutes(m)).timestamp()),
            }
            .resets_in(now)
        };
        assert_eq!(in_minutes(43).as_deref(), Some("43m"));
        assert_eq!(in_minutes(134).as_deref(), Some("2h14m"));
        assert_eq!(in_minutes(180).as_deref(), Some("3h"));
        assert_eq!(in_minutes(-5).as_deref(), Some("now"));
        // A weekly window is days off, where the clock time is the useful answer.
        let weekly = now + chrono::TimeDelta::minutes(3 * 24 * 60);
        assert_eq!(
            in_minutes(3 * 24 * 60).as_deref(),
            Some(weekly.format("%a %H:%M").to_string().as_str())
        );
        assert_eq!(
            Window {
                used_percent: 0.0,
                window_minutes: None,
                resets_at: None,
            }
            .resets_in(now),
            None
        );
    }

    #[test]
    fn logs_only_limit_headers() {
        let dir = std::env::temp_dir().join(format!("bhai-limits-{}", uuid::Uuid::new_v4()));
        let path = dir.join("debug/headers.jsonl");
        let map = headers(&[
            ("x-codex-primary-used-percent", "1"),
            ("x-ratelimit-remaining-requests", "9"),
            ("authorization", "Bearer secret"),
            ("content-type", "text/event-stream"),
        ]);
        log_headers(&path, &map).unwrap();
        let line: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let logged = line["headers"].as_object().unwrap();
        assert_eq!(logged.len(), 2);
        assert_eq!(logged["x-ratelimit-remaining-requests"], "9");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
