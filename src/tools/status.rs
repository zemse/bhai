//! A pull-only glance at context, subscription quota and background work.

use std::sync::Arc;
use std::time::Instant;

use serde_json::{Value, json};

use super::{BoxFuture, Tool, bash};
use crate::agent::{Cancel, Children};
use crate::monitor::{Monitors, View};

pub const NAME: &str = "status";
pub type Context = Arc<std::sync::Mutex<Option<Value>>>;

#[derive(Default)]
pub struct Quota(std::sync::Mutex<Option<(crate::limits::RateLimits, i64)>>);

impl Quota {
    pub fn clear(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn observe(&self, limits: crate::limits::RateLimits) {
        let mut current = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let limits = limits.over(current.map(|(limits, _)| limits));
        *current = Some((limits, chrono::Utc::now().timestamp()));
    }

    pub fn snapshot(&self, now: i64) -> Option<Value> {
        let (limits, observed_at) = (*self.0.lock().unwrap_or_else(|e| e.into_inner()))?;
        let mut value = serde_json::to_value(limits).ok()?;
        value["observed_at"] = json!(observed_at);
        value["age_seconds"] = json!((now - observed_at).max(0));
        Some(value)
    }
}

pub fn context(input: u64, limits: crate::compact::Limits, model: &str, estimated: bool) -> Value {
    let window = limits.window(model);
    json!({
        "input_tokens": input,
        "window_tokens": window,
        "percent": 100.0 * input as f64 / window.max(1) as f64,
        "auto_compact_tokens": (limits.compact_at * window as f64).round() as u64,
        "estimated": estimated,
    })
}

/// The input tokens and window of the latest context reading.
pub fn reading(context: &Context) -> Option<(u64, u64)> {
    let context = context.lock().unwrap_or_else(|e| e.into_inner());
    let context = context.as_ref()?;
    Some((
        context["input_tokens"].as_u64()?,
        context["window_tokens"].as_u64()?,
    ))
}

/// What a pull-only read last answered in full, so an unchanged repeat can be brief.
#[derive(Default)]
pub struct Seen(std::sync::Mutex<Option<(Value, Instant, Option<u64>)>>);

impl Seen {
    /// The seconds since the last full answer when `key` is what it showed and context
    /// grew by less than `step`; otherwise `key` becomes the last full answer. Context
    /// that shrank means compaction may have folded that answer out of the conversation.
    pub fn repeat(&self, key: Value, input: Option<u64>, step: u64) -> Option<u64> {
        let mut last = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((seen, at, before)) = last.as_ref()
            && *seen == key
            && match (*before, input) {
                (Some(before), Some(now)) => now >= before && now - before < step,
                (before, now) => before == now,
            }
        {
            return Some(at.elapsed().as_secs());
        }
        *last = Some((key, Instant::now(), input));
        None
    }
}

pub struct Status {
    pub children: Children,
    pub cancel: Arc<Cancel>,
    pub monitors: Option<Arc<Monitors>>,
    pub context: Context,
    pub quota: Arc<Quota>,
    pub seen: Seen,
}

impl Tool for Status {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": NAME,
            "description": "Read context usage (input_tokens, window_tokens, percent, estimated) and a succinct JSON snapshot of background work. Context reflects the latest model call; estimated is true when backend usage is unavailable. \
        rate_limits, when available, reports subscription window used_percent, window_minutes, resets_at (Unix seconds), credits, observed_at and age_seconds from this agent's latest quota report, not a live fetch. \
        Commands include running sessions and exited ones with next: collect (use write_stdin with that id). \
        Children include running assignments and report_queued outcomes not yet consumed by the parent. Monitors include state and a short summary. \
        Empty sections are omitted. A read that finds nothing changed since the last one says so briefly. \
        Reads do not wait, collect results or change anything; push updates arrive separately.",
            "strict": true,
            "parameters": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn parallel(&self) -> bool {
        true
    }

    fn describe(&self, _args: &Value) -> Result<String, String> {
        Ok("read context, quota and background status".to_string())
    }

    fn execute<'a>(&'a self, _args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let children = self
                .children
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|child| {
                    self.cancel.running(&child.id) || self.cancel.awaiting_result(&child.id)
                })
                .map(|child| {
                    let state = if self.cancel.running(&child.id) {
                        "running"
                    } else {
                        "report_queued"
                    };
                    json!({"id": child.id, "task": brief(&child.description), "state": state})
                })
                .collect();
            let monitors = self.monitors.as_ref().map_or_else(Vec::new, |m| m.views());
            let mut out = snapshot(bash::kept(), children, monitors);
            if let Some(context) = self
                .context
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                out["context"] = context;
            }
            if let Some(quota) = self.quota.snapshot(chrono::Utc::now().timestamp()) {
                out["rate_limits"] = quota;
            }
            let reading = reading(&self.context);
            let step = reading.map_or(u64::MAX, |(_, window)| window / CONTEXT_STEP);
            match self.seen.repeat(key(&out), reading.map(|r| r.0), step) {
                Some(secs) => (unchanged(secs, reading), true),
                None => (out.to_string(), true),
            }
        })
    }
}

/// Context growth, as a fraction of the window, that a brief repeat may not hide.
const CONTEXT_STEP: u64 = 20;

/// The parts of a snapshot whose change is news: ages and observation times tick on
/// their own, context is compared apart, and quota counts only whole percents.
fn key(snapshot: &Value) -> Value {
    let mut key = snapshot.clone();
    if let Some(object) = key.as_object_mut() {
        object.remove("context");
    }
    if let Some(rows) = key.get_mut("bash").and_then(Value::as_array_mut) {
        for row in rows.iter_mut().filter_map(Value::as_object_mut) {
            row.remove("age_s");
        }
    }
    if let Some(limits) = key.get_mut("rate_limits").and_then(Value::as_object_mut) {
        limits.remove("observed_at");
        limits.remove("age_seconds");
        for window in ["primary", "secondary"] {
            if let Some(used) = limits
                .get_mut(window)
                .and_then(|w| w.get_mut("used_percent"))
            {
                *used = json!(used.as_f64().unwrap_or_default().floor());
            }
        }
    }
    key
}

fn unchanged(secs: u64, reading: Option<(u64, u64)>) -> String {
    let context = reading.map_or_else(String::new, |(input, window)| {
        format!(" apart from context, now {input} of {window} tokens")
    });
    format!(
        "Unchanged since the status read {secs}s ago{context}. Child reports and monitor hooks \
arrive as messages without polling."
    )
}

fn brief(text: &str) -> String {
    crate::redact::apply(text)
        .chars()
        .filter(|c| !c.is_control())
        .take(160)
        .collect()
}

fn snapshot(commands: Vec<bash::Running>, children: Vec<Value>, monitors: Vec<View>) -> Value {
    let mut out = json!({});
    if !commands.is_empty() {
        let mut commands = commands;
        commands.sort_by_key(|command| command.id);
        out["bash"] = commands
            .into_iter()
            .map(|command| {
                let mut row = json!({"id": command.id, "command": brief(&command.command)});
                if command.exited {
                    row["state"] = json!("exited");
                    row["next"] = json!("collect");
                } else {
                    row["state"] = json!("running");
                    row["age_s"] = json!(command.age.as_secs());
                }
                row
            })
            .collect();
    }
    if !children.is_empty() {
        out["children"] = children.into();
    }
    if !monitors.is_empty() {
        out["monitors"] = monitors
            .into_iter()
            .map(|monitor| {
                let summary = monitor.snapshot.map_or_else(String::new, |s| s.summary);
                json!({"id": monitor.id, "state": monitor.state, "summary": brief(&summary)})
            })
            .collect();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ChildUsage;
    use crate::monitor::Snapshot;
    use std::time::Duration;

    #[tokio::test]
    async fn context_is_visible_even_without_background_work() {
        let status = Status {
            children: Arc::default(),
            cancel: Arc::default(),
            monitors: None,
            context: Arc::default(),
            quota: Arc::default(),
            seen: Seen::default(),
        };
        for (input, window, estimated, percent) in [(25, 100, false, 25.0), (120, 100, true, 120.0)]
        {
            let limits = crate::compact::Limits {
                window: Some(window),
                ..crate::compact::Limits::default()
            };
            *status.context.lock().unwrap() = Some(context(input, limits, "any", estimated));
            let (text, ok) = status.execute(&json!({})).await;
            assert!(ok);
            let value: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["context"]["percent"], json!(percent));
            assert_eq!(value["context"]["estimated"], json!(estimated));
            assert_eq!(value["context"]["input_tokens"], json!(input));
            assert_eq!(value["context"]["window_tokens"], json!(window));
            assert_eq!(value["context"]["auto_compact_tokens"], json!(80));
        }
    }

    #[tokio::test]
    async fn quota_reports_windows_credits_and_observation_age() {
        use crate::limits::{Credits, RateLimits, Window};

        let status = Status {
            children: Arc::default(),
            cancel: Arc::default(),
            monitors: None,
            context: Arc::default(),
            quota: Arc::default(),
            seen: Seen::default(),
        };
        let (text, ok) = status.execute(&json!({})).await;
        assert!(ok);
        assert!(
            serde_json::from_str::<Value>(&text)
                .unwrap()
                .get("rate_limits")
                .is_none()
        );
        status.quota.observe(RateLimits {
            primary: Some(Window {
                used_percent: 98.5,
                window_minutes: Some(10080),
                resets_at: Some(i64::MAX),
            }),
            credits: Some(Credits {
                remaining: Some(12.0),
                ..Credits::default()
            }),
            ..RateLimits::default()
        });
        let (text, ok) = status.execute(&json!({})).await;
        assert!(ok);
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["rate_limits"]["primary"]["used_percent"], 98.5);
        assert_eq!(value["rate_limits"]["primary"]["resets_at"], i64::MAX);
        assert_eq!(value["rate_limits"]["credits"]["remaining"], 12.0);
        assert!(value["rate_limits"].get("checkpoint_recommended").is_none());
        let observed_at = value["rate_limits"]["observed_at"].as_i64().unwrap();
        assert_eq!(
            status.quota.snapshot(observed_at + 60).unwrap()["age_seconds"],
            60
        );
    }

    #[test]
    fn refreshed_windows_replace_usage_and_keep_unreported_credits() {
        use crate::limits::{Credits, RateLimits, Window};

        let quota = Quota::default();
        let window = |used_percent| {
            Some(Window {
                used_percent,
                window_minutes: Some(10080),
                resets_at: None,
            })
        };
        quota.observe(RateLimits {
            primary: window(99.0),
            credits: Some(Credits {
                remaining: Some(12.0),
                ..Credits::default()
            }),
            ..RateLimits::default()
        });
        quota.observe(RateLimits {
            primary: window(100.0),
            ..RateLimits::default()
        });
        quota.observe(RateLimits {
            primary: window(1.0),
            ..RateLimits::default()
        });
        let value = quota.snapshot(chrono::Utc::now().timestamp()).unwrap();
        assert_eq!(value["primary"]["used_percent"], 1.0);
        assert_eq!(value["credits"]["remaining"], 12.0);
        quota.observe(RateLimits {
            primary: window(100.0),
            credits: Some(Credits {
                remaining: Some(0.0),
                ..Credits::default()
            }),
            ..RateLimits::default()
        });
        assert_eq!(quota.snapshot(0).unwrap()["credits"]["remaining"], 0.0);
        quota.clear();
        assert!(quota.snapshot(0).is_none());
    }

    #[tokio::test]
    async fn a_repeat_is_brief_until_work_or_context_changes() {
        let status = Status {
            children: Arc::default(),
            cancel: Arc::default(),
            monitors: None,
            context: Arc::default(),
            quota: Arc::default(),
            seen: Seen::default(),
        };
        let limits = crate::compact::Limits {
            window: Some(200_000),
            ..crate::compact::Limits::default()
        };
        let args = json!({});
        let read = |input| {
            *status.context.lock().unwrap() = Some(context(input, limits, "any", false));
            status.execute(&args)
        };
        let full = |text: &str| serde_json::from_str::<Value>(text).is_ok();
        let child = status.cancel.child("c1");
        status.children.lock().unwrap().push(ChildUsage {
            id: "c1".into(),
            description: "review".into(),
            ..ChildUsage::default()
        });
        assert!(full(&read(50_000).await.0));
        let (brief, ok) = read(59_000).await;
        assert!(ok);
        assert_eq!(
            brief,
            "Unchanged since the status read 0s ago apart from context, now 59000 of 200000 \
tokens. Child reports and monitor hooks arrive as messages without polling."
        );
        // A twentieth of the window since the last full read is news, and so is less context.
        assert!(full(&read(60_000).await.0));
        assert!(!full(&read(60_500).await.0));
        assert!(full(&read(30_000).await.0));
        assert!(!full(&read(30_000).await.0));
        drop(child);
        let (text, _) = read(30_000).await;
        assert!(full(&text) && !text.contains("c1"), "{text}");
        status.quota.observe(crate::limits::RateLimits {
            primary: Some(crate::limits::Window {
                used_percent: 40.2,
                window_minutes: None,
                resets_at: None,
            }),
            ..crate::limits::RateLimits::default()
        });
        assert!(full(&read(30_000).await.0));
        status.quota.observe(crate::limits::RateLimits {
            primary: Some(crate::limits::Window {
                used_percent: 40.7,
                window_minutes: None,
                resets_at: None,
            }),
            ..crate::limits::RateLimits::default()
        });
        assert!(!full(&read(30_000).await.0));
    }

    #[test]
    fn empty_sections_are_omitted() {
        assert_eq!(snapshot(Vec::new(), Vec::new(), Vec::new()), json!({}));
    }

    #[test]
    fn commands_show_running_age_or_collect_without_output_details() {
        let command = |id, exited| bash::Running {
            id,
            command: "cargo test".into(),
            tty: true,
            group: Some(123),
            age: Duration::from_secs(18),
            exited,
            tail: "verbose output".into(),
        };
        assert_eq!(
            snapshot(
                vec![command(43, false), command(42, true)],
                Vec::new(),
                Vec::new()
            ),
            json!({"bash": [
                {"id": 42, "command": "cargo test", "state": "exited", "next": "collect"},
                {"id": 43, "command": "cargo test", "state": "running", "age_s": 18}
            ]})
        );
    }

    #[test]
    fn monitors_only_show_state_and_brief_summary() {
        let monitor = View {
            id: "m1".into(),
            name: "build".into(),
            state: "stale".into(),
            snapshot: Some(Snapshot {
                summary: "320/1000 processed".into(),
                ..Snapshot::default()
            }),
            error: Some("sampling failed".into()),
            updated_secs_ago: Some(30),
            detail: "verbose details".into(),
        };
        assert_eq!(
            snapshot(Vec::new(), Vec::new(), vec![monitor]),
            json!({"monitors": [{"id": "m1", "state": "stale", "summary": "320/1000 processed"}]})
        );
    }

    #[test]
    fn text_is_bounded_and_redacted() {
        crate::redact::register("status-secret-e12c");
        assert_eq!(brief("echo status-secret-e12c\n"), "echo [REDACTED]");
        assert_eq!(brief(&"界".repeat(200)).chars().count(), 160);
    }

    #[tokio::test]
    async fn reads_keep_active_children_and_leave_tracking_untouched() {
        let status = Status {
            children: Arc::default(),
            cancel: Arc::default(),
            monitors: None,
            context: Arc::default(),
            quota: Arc::default(),
            seen: Seen::default(),
        };
        let active = status.cancel.child("active");
        status.children.lock().unwrap().extend([
            ChildUsage {
                id: "active".into(),
                description: "review changes".into(),
                ..ChildUsage::default()
            },
            ChildUsage {
                id: "done".into(),
                description: "finished work".into(),
                ..ChildUsage::default()
            },
        ]);
        let (text, success) = status.execute(&json!({})).await;
        assert!(success);
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["children"],
            json!([
                {"id": "active", "task": "review changes", "state": "running"}
            ])
        );
        let (text, success) = status.execute(&json!({})).await;
        assert!(success);
        assert!(
            text.starts_with("Unchanged since the status read"),
            "{text}"
        );
        assert_eq!(status.children.lock().unwrap().len(), 2);
        assert!(status.cancel.running("active"));
        drop(active);
        let (text, _) = status.execute(&json!({})).await;
        let value: Value = serde_json::from_str(&text).unwrap();
        assert!(value.get("children").is_none());
    }

    #[tokio::test]
    async fn status_distinguishes_a_queued_report_from_a_running_child() {
        let status = Status {
            children: Arc::default(),
            cancel: Arc::default(),
            monitors: None,
            context: Arc::default(),
            quota: Arc::default(),
            seen: Seen::default(),
        };
        status.children.lock().unwrap().push(ChildUsage {
            id: "abcdef".into(),
            description: "verify".into(),
            ..ChildUsage::default()
        });
        status.cancel.expect_result("abcdef");
        let (text, ok) = status.execute(&json!({})).await;
        assert!(ok);
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["children"][0]["state"], "report_queued");
        assert!(status.cancel.has_children());
        status.cancel.acknowledge_result("abcdef");
        let (text, _) = status.execute(&json!({})).await;
        assert!(
            serde_json::from_str::<Value>(&text)
                .unwrap()
                .get("children")
                .is_none()
        );
    }

    #[tokio::test]
    async fn status_leaves_exited_command_output_for_collection() {
        let (output, success) = bash::Bash
            .execute(&json!({
                "command": "sleep 1; printf status-result-e12c; exit 7",
                "yield_time_ms": 250
            }))
            .await;
        assert!(success);
        let Some(bash::Outcome::Running(id)) = bash::outcome(&output) else {
            panic!("expected a kept session: {output}");
        };
        let status = Status {
            children: Arc::default(),
            cancel: Arc::default(),
            monitors: None,
            context: Arc::default(),
            quota: Arc::default(),
            seen: Seen::default(),
        };
        let begun = std::time::Instant::now();
        loop {
            let (text, success) = status.execute(&json!({})).await;
            assert!(success);
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            };
            let row = value["bash"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == id)
                .unwrap();
            if row["state"] == "exited" {
                assert_eq!(row["next"], "collect");
                break;
            }
            assert!(
                begun.elapsed() < Duration::from_secs(5),
                "command never exited"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let (output, success) = super::super::stdin::WriteStdin
            .execute(&json!({"session_id": id}))
            .await;
        assert!(success);
        assert_eq!(bash::outcome(&output), Some(bash::Outcome::Failed(7)));
        assert!(output.contains("status-result-e12c"), "{output}");
        let (text, _) = status.execute(&json!({})).await;
        let value: Value = serde_json::from_str(&text).unwrap();
        assert!(
            value
                .get("bash")
                .and_then(Value::as_array)
                .is_none_or(|rows| rows.iter().all(|r| r["id"] != id))
        );
    }

    #[test]
    fn registry_exposes_a_parallel_approval_free_read() {
        let registry = super::super::Registry::new(Vec::new()).with_tool(Box::new(Status {
            children: Arc::default(),
            cancel: Arc::default(),
            monitors: None,
            context: Arc::default(),
            quota: Arc::default(),
            seen: Seen::default(),
        }));
        let tool = registry.get(NAME).unwrap();
        assert!(!tool.needs_approval());
        assert!(tool.parallel());
        assert_eq!(
            tool.schema()["parameters"],
            json!({
                "type": "object", "properties": {}, "additionalProperties": false
            })
        );
    }
}
