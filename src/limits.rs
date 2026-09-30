//! Rate-limit headroom as the ChatGPT backend reports it, in response headers
//! (`x-codex-primary-used-percent` and friends, as openai/codex reads them) or in a
//! `codex.rate_limits` stream event, and the credit balance, which only `/wham/usage`
//! reliably carries.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone};
use reqwest::header::HeaderMap;
use serde::Serialize;
use serde_json::{Value, json};

use crate::auth::Auth;

/// Percent used at which a window turns yellow.
pub const WARN: f64 = 75.0;
/// Percent used at which a window turns red.
pub const ALERT: f64 = 90.0;

/// Rate limits and credits, as openai/codex reads them. Not under `/backend-api/codex`:
/// that copy of the path answers 403.
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
/// Seconds between usage fetches, whether a session is idle or a call brings one.
const REFRESH_SECS: i64 = 60;
/// How often an idle session asks whether a fetch is due.
pub const REFRESH: Duration = Duration::from_secs(REFRESH_SECS as u64);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// When the last usage fetch was started, unix seconds.
static LAST_FETCH: AtomicI64 = AtomicI64::new(0);

/// The latest usage of each limit window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct RateLimits {
    pub primary: Option<Window>,
    pub secondary: Option<Window>,
    pub credits: Option<Credits>,
}

/// Credits, spent once the windows run out. A Team seat's allowance is under
/// `spend_control`, with `credits.balance` null; other plans put it in the balance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Credits {
    pub unlimited: bool,
    pub remaining: Option<f64>,
    pub limit: Option<f64>,
    pub used: Option<f64>,
    /// Unix seconds at which the allowance resets.
    pub resets_at: Option<i64>,
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
        let flag = |name: &str| match get(name)?.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        };
        let credits = match (
            flag("x-codex-credits-has-credits"),
            flag("x-codex-credits-unlimited"),
        ) {
            (Some(has), Some(unlimited)) => Credits::from_balance(
                has,
                unlimited,
                get("x-codex-credits-balance").and_then(decimal),
            ),
            _ => None,
        };
        Self {
            primary: window("primary"),
            secondary: window("secondary"),
            credits,
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
            credits: limits.get("credits").and_then(Credits::from_credits),
        }
        .some()
    }

    /// Read a `/wham/usage` body, whose windows are `primary_window` and
    /// `secondary_window` under `rate_limit`, measured in seconds.
    pub fn from_usage(body: &Value, now: i64) -> Option<Self> {
        let window = |which: &str| {
            let w = body.get("rate_limit")?.get(which)?;
            let used = w.get("used_percent")?.as_f64()?;
            let number = |key: &str| w.get(key).and_then(Value::as_i64);
            Some(Window {
                used_percent: used,
                window_minutes: w
                    .get("limit_window_seconds")
                    .and_then(Value::as_u64)
                    .map(|s| s / 60),
                resets_at: number("reset_at")
                    .or_else(|| number("reset_after_seconds").map(|s| now + s)),
            })
        };
        Self {
            primary: window("primary_window"),
            secondary: window("secondary_window"),
            credits: body
                .get("spend_control")
                .and_then(|s| Credits::from_spend_control(s, now))
                .or_else(|| body.get("credits").and_then(Credits::from_credits)),
        }
        .some()
    }

    /// `self`, with the credits of `before` kept when it reports none: the stream says
    /// nothing about a balance that lives in `spend_control`.
    pub fn over(mut self, before: Option<Self>) -> Self {
        if self.credits.is_none() {
            self.credits = before.and_then(|b| b.credits);
        }
        self
    }

    fn some(self) -> Option<Self> {
        (self.primary.is_some() || self.secondary.is_some() || self.credits.is_some())
            .then_some(self)
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
        until(self.resets_at?, now)
    }
}

fn until(resets_at: i64, now: DateTime<Local>) -> Option<String> {
    let at = Local.timestamp_opt(resets_at, 0).single()?;
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

impl Credits {
    /// A `credits` object: `{has_credits, unlimited, balance}`, the balance a decimal
    /// string or null.
    fn from_credits(c: &Value) -> Option<Self> {
        let flag = |key: &str| c.get(key).and_then(Value::as_bool);
        Self::from_balance(
            flag("has_credits")?,
            flag("unlimited").unwrap_or(false),
            c.get("balance").and_then(json_decimal),
        )
    }

    /// Nothing when there are no credits to spend, since a plan without them reports
    /// a balance of `0` that is not worth a place on the bar.
    fn from_balance(has: bool, unlimited: bool, balance: Option<f64>) -> Option<Self> {
        if unlimited {
            return Some(Self {
                unlimited,
                ..Self::default()
            });
        }
        has.then_some(Self {
            remaining: Some(balance?),
            ..Self::default()
        })
    }

    /// `spend_control.individual_limit`, a seat's allowance, whose amounts are decimal
    /// strings. Only a limit counted in credits is one.
    fn from_spend_control(s: &Value, now: i64) -> Option<Self> {
        let l = s.get("individual_limit")?;
        if l.get("unit")
            .and_then(Value::as_str)
            .is_some_and(|unit| unit != "credit")
        {
            return None;
        }
        let amount = |key: &str| l.get(key).and_then(json_decimal);
        let number = |key: &str| l.get(key).and_then(Value::as_i64);
        Some(Self {
            unlimited: false,
            remaining: Some(amount("remaining")?),
            limit: amount("limit"),
            used: amount("used"),
            resets_at: number("reset_at")
                .or_else(|| number("reset_after_seconds").map(|s| now + s)),
        })
    }

    /// Share of the allowance spent, when there is an allowance to measure against.
    pub fn used_percent(&self) -> Option<f64> {
        let limit = self.limit.filter(|l| *l > 0.0)?;
        let used = self.used.or_else(|| Some(limit - self.remaining?))?;
        Some(used / limit * 100.0)
    }

    /// `8.3k/10.0k`, `8.3k` without a limit, or `unlimited`.
    pub fn amount(&self) -> String {
        if self.unlimited {
            return "unlimited".to_string();
        }
        let short = |n: f64| crate::ui::compact(n.max(0.0).floor() as u64);
        match (self.remaining, self.limit) {
            (Some(left), Some(limit)) => format!("{}/{}", short(left), short(limit)),
            (Some(left), None) => short(left),
            _ => "?".to_string(),
        }
    }

    pub fn resets_in(&self, now: DateTime<Local>) -> Option<String> {
        until(self.resets_at?, now)
    }
}

/// A decimal given as a string or a number.
fn json_decimal(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => decimal(s),
        _ => v.as_f64().filter(|n| n.is_finite()),
    }
}

fn decimal(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|n| n.is_finite())
}

/// Whether a model call should fetch usage too, claiming the slot when it should, so
/// the calls of several agents at once make one fetch between them.
pub fn due(now: i64) -> bool {
    let last = LAST_FETCH.load(Ordering::Relaxed);
    now - last >= REFRESH_SECS
        && LAST_FETCH
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

/// `GET /wham/usage`, the body as sent. It names the account's email and ids, so it
/// is read here and never logged.
pub async fn fetch(http: &reqwest::Client, auth: &Auth) -> Result<Value> {
    let mut req = http
        .get(USAGE_URL)
        .timeout(FETCH_TIMEOUT)
        .bearer_auth(&auth.access_token)
        .header("Accept", "application/json")
        .header("originator", crate::client::ORIGINATOR);
    if let Some(account_id) = &auth.account_id {
        req = req.header("ChatGPT-Account-ID", account_id);
    }
    let resp = req.send().await.context("could not ask for usage")?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("the usage endpoint answered {status}");
    }
    resp.json()
        .await
        .context("the usage endpoint did not answer JSON")
}

/// [`fetch`] on a client of its own, for callers that hold none.
pub async fn fetch_now() -> Result<Value> {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_default();
    let auth = crate::auth::load(&http).await?;
    fetch(&http, &auth).await
}

/// What `/usage` and `bhai usage` print: the plan, each window, and the credits.
pub fn report(body: &Value, now: DateTime<Local>) -> String {
    let mut out = String::new();
    if let Some(plan) = body.get("plan_type").and_then(Value::as_str) {
        out.push_str(&format!("plan: {plan}\n"));
    }
    let found = RateLimits::from_usage(body, now.timestamp()).unwrap_or_default();
    for w in found.windows() {
        out.push_str(&format!("{}: {:.0}% used", w.label(), w.used_percent));
        if let Some(left) = w.resets_in(now) {
            out.push_str(&format!(", resets {left}"));
        }
        out.push('\n');
    }
    out.push_str("credits: ");
    match found.credits {
        None => out.push_str("none"),
        Some(c) if c.unlimited => out.push_str("unlimited"),
        Some(c) => {
            let exact = |n: f64| format!("{n:.2}");
            match (c.remaining, c.limit) {
                (Some(left), Some(limit)) => {
                    out.push_str(&format!("{} left of {}", exact(left), exact(limit)))
                }
                (Some(left), None) => out.push_str(&format!("{} left", exact(left))),
                _ => out.push('?'),
            }
            if let Some(used) = c.used_percent() {
                out.push_str(&format!(" ({used:.0}% used)"));
            }
            if let Some(left) = c.resets_in(now) {
                out.push_str(&format!(", resets {left}"));
            }
        }
    }
    if body
        .pointer("/spend_control/reached")
        .and_then(Value::as_bool)
        == Some(true)
    {
        out.push_str("\nthe spend limit is reached");
    }
    out
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
        crate::sessions::private_dir(dir)?;
    }
    let line = json!({
        "timestamp": chrono::Local::now().to_rfc3339(),
        "headers": picked,
    });
    let mut file = crate::sessions::private_append(path)?;
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

    /// A `/wham/usage` body of the Team shape, the allowance under `spend_control`.
    fn team_usage() -> Value {
        json!({
            "plan_type": "team",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 10, "limit_window_seconds": 18000,
                    "reset_after_seconds": 600, "reset_at": 2000,
                },
                "secondary_window": {
                    "used_percent": 2, "limit_window_seconds": 604800,
                    "reset_after_seconds": 6000,
                },
            },
            "credits": { "has_credits": true, "unlimited": false, "balance": null },
            "spend_control": {
                "reached": false,
                "individual_limit": {
                    "unit": "credit", "limit": "10000", "used": "1661.59",
                    "remaining": "8338.41", "reset_after_seconds": 100,
                },
            },
        })
    }

    #[test]
    fn reads_the_team_allowance_from_spend_control() {
        let limits = RateLimits::from_usage(&team_usage(), 1000).unwrap();
        assert_eq!(
            limits.primary,
            Some(Window {
                used_percent: 10.0,
                window_minutes: Some(300),
                resets_at: Some(2000),
            })
        );
        assert_eq!(limits.secondary.unwrap().label(), "wk");
        assert_eq!(limits.secondary.unwrap().resets_at, Some(7000));
        let credits = limits.credits.unwrap();
        assert_eq!(
            credits,
            Credits {
                unlimited: false,
                remaining: Some(8338.41),
                limit: Some(10000.0),
                used: Some(1661.59),
                resets_at: Some(1100),
            }
        );
        assert_eq!(credits.amount(), "8.3k/10.0k");
        assert_eq!(credits.used_percent().unwrap().round(), 17.0);
    }

    #[test]
    fn falls_back_to_the_balance() {
        let body = json!({
            "credits": { "has_credits": true, "unlimited": false, "balance": "250.5" },
            "spend_control": { "reached": false, "individual_limit": null },
        });
        let credits = RateLimits::from_usage(&body, 0).unwrap().credits.unwrap();
        assert_eq!(credits.remaining, Some(250.5));
        assert_eq!(credits.used_percent(), None);
        assert_eq!(credits.amount(), "250");
    }

    #[test]
    fn no_credits_is_nothing_to_show() {
        // What a Plus plan answers: no credits, and a balance of `0` to go with it.
        let body = json!({
            "rate_limit": { "primary_window": { "used_percent": 1, "limit_window_seconds": 18000 } },
            "credits": { "has_credits": false, "unlimited": false, "balance": "0" },
            "spend_control": { "reached": false, "individual_limit": null },
        });
        assert_eq!(RateLimits::from_usage(&body, 0).unwrap().credits, None);
        assert_eq!(RateLimits::from_usage(&json!({}), 0), None);
    }

    #[test]
    fn unlimited_and_other_units() {
        let unlimited = json!({ "credits": { "has_credits": true, "unlimited": true } });
        let credits = RateLimits::from_usage(&unlimited, 0)
            .unwrap()
            .credits
            .unwrap();
        assert!(credits.unlimited);
        assert_eq!(credits.amount(), "unlimited");

        // A limit counted in something else is not a credit balance.
        let mut body = team_usage();
        body["spend_control"]["individual_limit"]["unit"] = json!("usd");
        assert_eq!(RateLimits::from_usage(&body, 0).unwrap().credits, None);
    }

    #[test]
    fn malformed_amounts_are_tolerated() {
        let mut body = team_usage();
        body["spend_control"]["individual_limit"]["remaining"] = json!("lots");
        body["credits"]["balance"] = json!("NaN");
        assert_eq!(RateLimits::from_usage(&body, 0).unwrap().credits, None);

        let mut body = team_usage();
        body["spend_control"]["individual_limit"]["remaining"] = json!(12.5);
        body["spend_control"]["individual_limit"]["limit"] = json!("?");
        let credits = RateLimits::from_usage(&body, 0).unwrap().credits.unwrap();
        assert_eq!(credits.remaining, Some(12.5));
        assert_eq!(credits.limit, None);
        assert_eq!(credits.used_percent(), None);
    }

    #[test]
    fn reads_credits_from_headers_and_the_event() {
        let map = headers(&[
            ("x-codex-credits-has-credits", "True"),
            ("x-codex-credits-unlimited", "false"),
            ("x-codex-credits-balance", " 42.5 "),
        ]);
        let limits = RateLimits::from_headers(&map, 0).unwrap();
        assert_eq!(limits.credits.unwrap().remaining, Some(42.5));
        // Without both flags there is nothing to go on.
        let half = headers(&[("x-codex-credits-balance", "42.5")]);
        assert_eq!(RateLimits::from_headers(&half, 0), None);

        let event = json!({
            "type": "codex.rate_limits",
            "primary": { "used_percent": 10.0, "window_minutes": 300 },
            "credits": { "has_credits": true, "unlimited": false, "balance": null },
        });
        assert_eq!(RateLimits::from_event(&event, 0).unwrap().credits, None);
    }

    #[test]
    fn a_stream_update_keeps_the_fetched_credits() {
        let fetched = RateLimits::from_usage(&team_usage(), 0);
        let streamed = RateLimits::from_event(
            &json!({ "primary": { "used_percent": 50.0, "window_minutes": 300 } }),
            0,
        )
        .unwrap();
        let merged = streamed.over(fetched);
        assert_eq!(merged.primary.unwrap().used_percent, 50.0);
        assert_eq!(merged.credits, fetched.unwrap().credits);
    }

    #[test]
    fn the_report_names_no_account() {
        let mut body = team_usage();
        body["email"] = json!("someone@example.com");
        body["account_id"] = json!("acct-1");
        let now = Local.timestamp_opt(1000, 0).single().unwrap();
        let text = report(&body, now);
        assert!(text.starts_with("plan: team\n5h: 10% used"), "{text}");
        assert!(
            text.contains("credits: 8338.41 left of 10000.00 (17% used), resets 1m"),
            "{text}"
        );
        assert!(!text.contains("example.com") && !text.contains("acct-1"));

        let plus = json!({ "credits": { "has_credits": false, "balance": "0" } });
        assert_eq!(report(&plus, now), "credits: none");
    }

    #[test]
    fn a_fetch_is_due_once_per_interval() {
        let base = LAST_FETCH.load(Ordering::Relaxed).max(1) + 10 * REFRESH_SECS;
        assert!(due(base));
        assert!(!due(base + 1));
        assert!(due(base + REFRESH_SECS));
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
