//! `update_plan`, for the main agent: progress on the goal, or a standalone checklist
//! for analysis. It touches nothing outside the session, so it needs no approval.

use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{BoxFuture, Tool};
use crate::agent::AgentEvent;
use crate::plan::{Plan, Shared};

pub const NAME: &str = "update_plan";

pub struct UpdatePlan {
    pub plan: Shared,
    /// Told as the call runs, so the panel moves with the work rather than at turn end.
    pub tx: mpsc::UnboundedSender<AgentEvent>,
}

impl Tool for UpdatePlan {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        let step = json!({
            "type": "object",
            "properties": {
                "step": { "type": "string", "description": "Unique step name; use its exact existing name for changes." },
                "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "skipped"] },
                "reason": { "type": "string", "description": "Required when skipped: why the work is no longer necessary." }
            },
            "required": ["step", "status"],
            "additionalProperties": false
        });
        json!({
            "type": "function",
            "name": NAME,
            "description": "Update live progress on the current goal, or a standalone plan for \
        analysis (never creates a goal). Use `changes` to update existing steps by name and \
        `append` to add discovered work; neither resends the whole list. Alternatively `plan` \
        replaces the list. Keep completed work and existing steps on a goal; mark unnecessary \
        steps `skipped` with a reason instead of dropping them. Up to 500 steps, one in_progress. \
        Adapt affected steps when goal requirements change. Completing the plan does not \
        complete the goal or replace verification. With no edits, read steps using offset \
        (zero-based) and limit (default 20, maximum 50). Empty plan clears standalone lists only.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "explanation": { "type": "string" },
                    "plan": { "type": "array", "items": step },
                    "changes": { "type": "array", "items": step },
                    "append": { "type": "array", "items": step },
                    "offset": { "type": "integer", "minimum": 0 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50 }
                },
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        if args.get("plan").is_some() {
            return Ok(Plan::parse(args)?.map_or("plan cleared".to_string(), |p| p.line()));
        }
        Ok(if editing(args) {
            "plan updated"
        } else {
            "read plan"
        }
        .to_string())
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            if !editing(args) {
                let Some(plan) = self.plan.get() else {
                    return ("There is no plan.".to_string(), true);
                };
                let offset = match args.get("offset") {
                    None => 0,
                    Some(value) => match value.as_u64().and_then(|n| usize::try_from(n).ok()) {
                        Some(n) => n,
                        None => {
                            return ("offset must be a nonnegative integer.".to_string(), false);
                        }
                    },
                };
                let limit = match args.get("limit") {
                    None => 20,
                    Some(value) => match value.as_u64() {
                        Some(n @ 1..=50) => n as usize,
                        _ => return ("limit must be 1..50.".to_string(), false),
                    },
                };
                let text = plan
                    .steps
                    .iter()
                    .enumerate()
                    .skip(offset)
                    .take(limit)
                    .map(|(i, s)| format!("{}: {}", i + 1, s.text()))
                    .collect::<Vec<_>>()
                    .join("\n");
                return (format!("{}\n{text}", plan.line()), true);
            }
            let plan = match self.plan.update(args) {
                Ok(plan) => plan,
                Err(e) => return (e, false),
            };
            let _ = self.tx.send(AgentEvent::Plan(plan));
            ("Plan updated".to_string(), true)
        })
    }
}

fn editing(args: &Value) -> bool {
    ["plan", "changes", "append"]
        .iter()
        .any(|key| args.get(key).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn incremental_updates_and_paged_reads_keep_the_full_list() {
        let shared = Shared::default();
        let (tx, _rx) = mpsc::unbounded_channel();
        let tool = UpdatePlan {
            plan: shared.clone(),
            tx,
        };
        assert!(tool.execute(&json!({"append": [
            {"step": "inspect", "status": "in_progress"}, {"step": "verify", "status": "pending"}
        ]})).await.1);
        assert!(tool.execute(&json!({"changes": [
            {"step": "inspect", "status": "completed"}, {"step": "verify", "status": "in_progress"}
        ]})).await.1);
        let before = shared.get();
        let (out, ok) = tool.execute(&json!({"offset": 1, "limit": 1})).await;
        assert!(
            ok && out.contains("2: [>] verify") && !out.contains("[x] inspect"),
            "{out}"
        );
        assert_eq!(shared.get(), before);
        assert!(!tool.execute(&json!({"limit": 51})).await.1);
        assert!(!tool.execute(&json!({"offset": -1})).await.1);
    }

    #[tokio::test]
    async fn a_call_replaces_the_plan_and_says_so_and_a_bad_one_changes_nothing() {
        let shared = Shared::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tool = UpdatePlan {
            plan: shared.clone(),
            tx,
        };
        assert!(!tool.needs_approval());
        let args = json!({"plan": [
            {"step": "a", "status": "completed"},
            {"step": "b", "status": "in_progress"}
        ]});
        assert_eq!(tool.describe(&args).unwrap(), "plan 1/2 done: b");
        assert!(tool.execute(&args).await.1);
        let set = shared.get().unwrap();
        assert_eq!(set.steps.len(), 2);
        assert!(matches!(rx.try_recv(), Ok(AgentEvent::Plan(Some(p))) if p == set));
        assert!(
            !tool
                .execute(&json!({"plan": [
                    {"step": "a", "status": "in_progress"},
                    {"step": "b", "status": "in_progress"}
                ]}))
                .await
                .1
        );
        assert_eq!(shared.get(), Some(set));
        assert!(rx.try_recv().is_err());
        assert!(tool.execute(&json!({"plan": []})).await.1);
        assert!(shared.get().is_none());
        assert!(matches!(rx.try_recv(), Ok(AgentEvent::Plan(None))));
    }
}
