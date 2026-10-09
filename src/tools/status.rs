//! A pull-only glance at context usage, commands, active children and session observers.

use std::sync::Arc;

use serde_json::{Value, json};

use super::{BoxFuture, Tool, bash};
use crate::agent::{Cancel, Children};
use crate::monitor::{Monitors, View};

pub const NAME: &str = "status";
pub type Context = Arc<std::sync::Mutex<Option<Value>>>;

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
        Commands include running sessions and exited ones with next: collect (use write_stdin with that id). \
        Children include only active tasks, including those waiting for a slot. Monitors include state and a short summary. \
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
        Ok("read context and background status".to_string())
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
