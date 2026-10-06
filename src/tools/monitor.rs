//! The main agent's session-scoped observers and their approved wake hooks.

use std::sync::Arc;

use serde_json::{Value, json};

use super::{BoxFuture, Tool, string_arg};
use crate::monitor::{Monitors, Spec};

pub const NAME: &str = "monitor";

pub struct Monitor {
    pub monitors: Arc<Monitors>,
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
            "description": "Proactively monitor long-running tasks so the user sees progress without asking. Create an approved lightweight command sampled every 2 seconds (minimum 1); it reads logs or task state and prints one JSON snapshot, then exits. No model calls for updates. Snapshot: {status: running|done|failed, summary: string, tracks: [{id, current, total?: positive number, unit?: string}], details: [string], metrics: [{label, value: number|string, unit?: string}], conditions: {name: boolean}}. Use independent tracks for baseline and newupdate benchmarks; omit total when unknown. Hooks use fixed approved prompts and script-provided conditions, firing on false-to-true transitions with optional sustained_secs and repeat. Minimum cooldown 30s; session wake limit one per 30s, at most one monitor notification queued. Missing conditions are false. A terminal snapshot stops sampling but remains displayed. Errors keep the last valid snapshot marked stale, with backoff. Scripts have a 64 KiB output limit. At most 8 monitors, 16 tracks/metrics/details, 32 conditions. Session-scoped, never automatically restarted. list shows snapshots and hook state; pause/resume control sampling; stop ends the observer only; dismiss stops and removes its card. Creation requires approval of command, workdir, timing and hooks, never judge approval.",
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
                "list" => serde_json::to_string(&self.monitors.views()).map_err(|e| e.to_string()),
                action => self.monitors.control(string_arg(args, "id").unwrap_or_default(), action).map(|_| format!("Monitor {action} applied.")),
            };
            match result {
                Ok(text) => (text, true),
                Err(error) => (error, false),
            }
        })
    }
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
}
