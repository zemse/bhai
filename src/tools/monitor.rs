//! The main agent's session-scoped observers and their approved wake hooks.

use std::sync::Arc;

use serde_json::{Value, json};

use super::{BoxFuture, Tool, status, string_arg};
use crate::monitor::{Monitors, Spec, View};

pub const NAME: &str = "monitor";

pub struct Monitor {
    pub monitors: Arc<Monitors>,
    pub context: status::Context,
    pub seen: status::Seen,
}

pub fn reads_only(args: &Value) -> bool {
    matches!(
        string_arg(args, "action"),
        Some("list" | "pause" | "resume" | "stop" | "dismiss")
    )
}

fn spec(args: &Value) -> Result<Spec, String> {
    let mut args = args.clone();
    let object = args
        .as_object_mut()
        .ok_or("monitor arguments must be an object")?;
    object.remove("action");
    let spec: Spec = serde_json::from_value(args).map_err(|e| e.to_string())?;
    spec.validate()?;
    Ok(spec)
}

impl Tool for Monitor {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function", "name": NAME, "strict": false,
            "description": "Proactively monitor long-running tasks so the user sees progress without asking. Create an approved lightweight command sampled every 2 seconds (minimum 1); it reads logs or task state and prints one JSON snapshot, then exits. No model calls for updates. Snapshot: {status: running|done|failed, summary: string, tracks: [{id, current, total?: positive number, unit?: string}], details: [string], metrics: [{label, value: number|string, unit?: string}], conditions: {name: boolean}}. Use independent tracks for baseline and newupdate benchmarks; omit total when unknown. Hooks use fixed approved prompts and script-provided conditions, firing on false-to-true transitions with optional sustained_secs and repeat. Minimum cooldown 30s; session wake limit one per 30s, at most one monitor notification queued. Missing conditions are false. A terminal snapshot stops sampling but remains displayed. Errors keep the last valid snapshot marked stale, with backoff. Scripts have a 64 KiB output limit. At most 8 monitors, 16 tracks/metrics/details, 32 conditions. Session-scoped, never automatically restarted. list shows snapshots and hook state; pause/resume control sampling; stop ends the observer only; dismiss stops and removes its card. Creation reviews command, workdir, timing and hooks through the permission policy, including the judge in auto mode.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": {"type":"string", "enum":["create","list","pause","resume","stop","dismiss"]},
                    "name": {"type":"string", "description":"For create: short task name."},
                    "command": {"type":"string", "description":"For create: non-interactive sampler command run with bash -lc."},
                    "workdir": {"type":"string", "description":"For create: absolute working directory."},
                    "interval_secs": {"type":"integer", "minimum":1, "maximum":3600},
                    "timeout_secs": {"type":"integer", "minimum":1, "maximum":30},
                    "hooks": {"type":"array", "maxItems":8, "items": {
                        "type":"object", "properties": {
                            "condition":{"type":"string"}, "prompt":{"type":"string"},
                            "cooldown_secs":{"type":"integer", "minimum":30, "maximum":86400},
                            "sustained_secs":{"type":"integer", "minimum":0, "maximum":86400},
                            "repeat":{"type":"boolean"}
                        }, "required":["condition","prompt"], "additionalProperties":false
                    }},
                    "id": {"type":"string", "description":"For pause, resume, stop or dismiss: monitor id."}
                }, "required":["action"], "additionalProperties":false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        match string_arg(args, "action") {
            Some("create") => {
                let spec = spec(args)?;
                Ok(format!(
                    "monitor create: {}",
                    crate::redact::apply(&serde_json::to_string(&spec).map_err(|e| e.to_string())?)
                ))
            }
            Some("list") => Ok("monitor list".into()),
            Some(action @ ("pause" | "resume" | "stop" | "dismiss")) => {
                let id = string_arg(args, "id")
                    .filter(|s| !s.is_empty())
                    .ok_or("monitor action needs id")?;
                Ok(format!("monitor {action} {id}"))
            }
            _ => Err("monitor action must be create, list, pause, resume, stop or dismiss".into()),
        }
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            if let Err(error) = self.describe(args) {
                return (error, false);
            }
            let result = match string_arg(args, "action").unwrap_or_default() {
                "create" => spec(args).and_then(|spec| self.monitors.add(spec)).map(|id| format!("Monitor {id} started. Its card updates without model calls; /monitor inspects it.")),
                "list" => self.list(),
                action => self.monitors.control(string_arg(args, "id").unwrap_or_default(), action).map(|_| format!("Monitor {action} applied.")),
            };
            match result {
                Ok(text) => (text, true),
                Err(error) => (error, false),
            }
        })
    }
}

impl Monitor {
    fn list(&self) -> Result<String, String> {
        let views = self.monitors.views();
        let input = status::reading(&self.context).map(|(input, _)| input);
        if let Some(secs) = self.seen.repeat(key(&views), input, u64::MAX) {
            return Ok(format!(
                "Monitors unchanged since the list {secs}s ago. Hooks wake you when their \
conditions fire."
            ));
        }
        serde_json::to_string(&views).map_err(|e| e.to_string())
    }
}

/// What a list shows apart from the ages that tick between samples.
fn key(views: &[View]) -> Value {
    views
        .iter()
        .map(|view| json!([view.id, view.name, view.state, view.snapshot, view.error]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_is_the_only_action_that_needs_approval() {
        assert!(!reads_only(&json!({"action":"create"})));
        assert!(!reads_only(&json!({"action":"invalid"})));
        for action in ["list", "pause", "resume", "stop", "dismiss"] {
            assert!(reads_only(&json!({"action":action})));
        }
        assert!(spec(&json!({"action":"create","name":"bench","command":"cat /tmp/state.json","workdir":"/tmp"})).is_ok());
        assert!(
            spec(&json!({"action":"create","name":"bench","command":"x","workdir":"relative"}))
                .is_err()
        );
    }

    #[tokio::test]
    async fn an_unchanged_list_is_brief_until_a_monitor_or_the_context_changes() {
        let tool = Monitor {
            monitors: Monitors::new(std::sync::Weak::new()),
            context: Arc::default(),
            seen: status::Seen::default(),
        };
        let reading = |input| {
            let limits = crate::compact::Limits::default();
            Some(status::context(input, limits, "gpt-5.5", false))
        };
        *tool.context.lock().unwrap() = reading(50_000);
        let list = json!({"action": "list"});
        let (created, ok) = tool
            .execute(&json!({
                "action": "create", "name": "slow", "command": "sleep 20",
                "workdir": "/tmp", "timeout_secs": 30
            }))
            .await;
        assert!(ok, "{created}");
        let id = tool.monitors.views()[0].id.clone();
        let (full, _) = tool.execute(&list).await;
        assert!(full.contains("\"slow\""), "{full}");
        *tool.context.lock().unwrap() = reading(90_000);
        let (brief, _) = tool.execute(&list).await;
        assert!(
            brief.starts_with("Monitors unchanged since the list"),
            "{brief}"
        );
        assert!(brief.len() < full.len());
        assert!(tool.execute(&json!({"action": "pause", "id": id})).await.1);
        let (paused, _) = tool.execute(&list).await;
        assert!(paused.contains("\"paused\""), "{paused}");
        assert!(
            tool.execute(&list)
                .await
                .0
                .starts_with("Monitors unchanged")
        );
        *tool.context.lock().unwrap() = reading(20_000);
        assert!(tool.execute(&list).await.0.contains("\"paused\""));
        assert!(
            tool.execute(&json!({"action": "dismiss", "id": id}))
                .await
                .1
        );
    }
}
