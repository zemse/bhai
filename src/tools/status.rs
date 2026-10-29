//! A pull-only glance at context, subscription quota and background work.

use std::sync::Arc;

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
        value["checkpoint_recommended"] = json!(limits.windows().any(|w| {
            w.window_minutes == Some(7 * 24 * 60)
                && w.used_percent >= 98.0
                && w.resets_at.is_none_or(|reset| reset > now)
        }));
        Some(value)
    }

    pub fn warning(&self) -> Option<String> {
        let value = self.snapshot(chrono::Utc::now().timestamp())?;
        (value["checkpoint_recommended"] == true).then(|| {
            format!(
                "For this turn, subscription weekly usage is at least 98%. Checkpoint now; a later \
user request may resume work. Do not start new \
experiments, delegate more work or spend quota on model-based compaction. Preserve modified \
files and write a concise on-disk handoff with verified results, uncommitted work, running \
commands, child IDs and exact next steps. Ask active children to checkpoint; do not discard \
their work or kill useful commands. Finish only bounded essential verification/landing if \
feasible, then pause and tell the user the reset time if known. Do not auto-switch models, \
weaken permissions or mark an unfinished goal complete or blocked solely because quota is \
low. Latest reported quota (not a reservation or guarantee of remaining calls): {value}"
            )
        })
    }
}

pub fn context(input: u64, window: u64, estimated: bool) -> Value {
    json!({
        "input_tokens": input,
        "window_tokens": window,
        "percent": 100.0 * input as f64 / window.max(1) as f64,
        "estimated": estimated,
    })
}

pub struct Status {
    pub children: Children,
    pub cancel: Arc<Cancel>,
    pub monitors: Option<Arc<Monitors>>,
    pub context: Context,
    pub quota: Arc<Quota>,
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
        checkpoint_recommended means a reported weekly window is at least 98% used and has not reached its known reset time. Other sessions and children share quota, so headroom is not guaranteed. \
        Commands include running sessions and exited ones with next: collect (use write_stdin with that id). \
        Children include only active tasks. Monitors include state and a short summary. \
        Empty sections are omitted. Reads do not wait, collect results or change anything; push updates arrive separately.",
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
                .filter(|child| self.cancel.running(&child.id))
                .map(|child| {
                    json!({"id": child.id, "task": brief(&child.description), "state": "running"})
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
            (out.to_string(), true)
        })
    }
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
        };
        for (input, window, estimated, percent) in [(25, 100, false, 25.0), (120, 100, true, 120.0)]
        {
            *status.context.lock().unwrap() = Some(context(input, window, estimated));
            let (text, ok) = status.execute(&json!({})).await;
            assert!(ok);
            let value: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["context"]["percent"], json!(percent));
            assert_eq!(value["context"]["estimated"], json!(estimated));
            assert_eq!(value["context"]["input_tokens"], json!(input));
            assert_eq!(value["context"]["window_tokens"], json!(window));
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
        assert_eq!(value["rate_limits"]["checkpoint_recommended"], true);
        let observed_at = value["rate_limits"]["observed_at"].as_i64().unwrap();
        assert_eq!(
            status.quota.snapshot(observed_at + 60).unwrap()["age_seconds"],
            60
        );
        assert!(status.quota.warning().unwrap().contains("Checkpoint now"));
    }

    #[test]
    fn checkpoint_threshold_uses_window_duration_and_known_reset() {
        use crate::limits::{RateLimits, Window};

        let quota = Quota::default();
        assert!(quota.snapshot(0).is_none());
        assert!(quota.warning().is_none());
        for (used, minutes, reset, expected) in [
            (97.99, Some(10080), Some(200), false),
            (98.0, Some(10080), Some(200), true),
            (100.0, Some(10080), None, true),
            (100.0, Some(10080), Some(100), false),
            (100.0, Some(10080), Some(99), false),
            (99.0, Some(300), Some(200), false),
            (99.0, None, Some(200), false),
        ] {
            let window = Some(Window {
                used_percent: used,
                window_minutes: minutes,
                resets_at: reset,
            });
            for secondary in [false, true] {
                quota.observe(RateLimits {
                    primary: if secondary { None } else { window },
                    secondary: if secondary { window } else { None },
                    credits: None,
                });
                assert_eq!(
                    quota.snapshot(100).unwrap()["checkpoint_recommended"],
                    expected
                );
            }
        }
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
        assert!(quota.warning().is_some());
        quota.observe(RateLimits {
            primary: window(1.0),
            ..RateLimits::default()
        });
        let value = quota.snapshot(chrono::Utc::now().timestamp()).unwrap();
        assert_eq!(value["primary"]["used_percent"], 1.0);
        assert_eq!(value["credits"]["remaining"], 12.0);
        assert!(quota.warning().is_none());
        quota.clear();
        assert!(quota.snapshot(0).is_none());
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
        for _ in 0..2 {
            let (text, success) = status.execute(&json!({})).await;
            assert!(success);
            let value: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(
                value["children"],
                json!([
                    {"id": "active", "task": "review changes", "state": "running"}
                ])
            );
        }
        assert_eq!(status.children.lock().unwrap().len(), 2);
        assert!(status.cancel.running("active"));
        drop(active);
        let (text, _) = status.execute(&json!({})).await;
        let value: Value = serde_json::from_str(&text).unwrap();
        assert!(value.get("children").is_none());
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
        };
        let begun = std::time::Instant::now();
        loop {
            let (text, success) = status.execute(&json!({})).await;
            assert!(success);
            let value: Value = serde_json::from_str(&text).unwrap();
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
