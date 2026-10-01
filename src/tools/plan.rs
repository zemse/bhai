//! `update_plan`, for the main agent: replace the checklist the user sees above the
//! prompt. Codex-trained models call it by habit. It touches nothing outside the
//! session, so it needs no approval.

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
        json!({
            "type": "function",
            "name": NAME,
            "description": "Updates the task plan the user sees.\nProvide an optional explanation \
        and the whole list of plan items, each with a step and status; it replaces the last one, \
        and an empty list clears it.\nAt most one step can be in_progress at a time. Skip it for \
        work of a step or two.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "explanation": { "type": "string" },
                    "plan": {
                        "type": "array",
                        "description": "The list of steps",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": { "type": "string" },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                }
                            },
                            "required": ["step", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["plan"],
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        Ok(match Plan::parse(args)? {
            Some(plan) => plan.line(),
            None => "plan cleared".to_string(),
        })
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let plan = match Plan::parse(args) {
                Ok(plan) => plan,
                Err(e) => return (e, false),
            };
            *self.plan.lock().unwrap_or_else(|e| e.into_inner()) = plan.clone();
            let _ = self.tx.send(AgentEvent::Plan(plan));
            ("Plan updated".to_string(), true)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[tokio::test]
    async fn a_call_replaces_the_plan_and_says_so_and_a_bad_one_changes_nothing() {
        let shared: Shared = Arc::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tool = UpdatePlan {
            plan: Arc::clone(&shared),
            tx,
        };
        assert!(!tool.needs_approval());
        let args = json!({"plan": [
            {"step": "a", "status": "completed"},
            {"step": "b", "status": "in_progress"},
        ]});
        assert_eq!(tool.describe(&args).unwrap(), "plan 1/2 done: b");
        let (out, ok) = tool.execute(&args).await;
        assert!(ok, "{out}");
        let set = shared.lock().unwrap().clone().unwrap();
        assert_eq!(set.steps.len(), 2);
        assert!(matches!(rx.try_recv(), Ok(AgentEvent::Plan(Some(p))) if p == set));

        let (out, ok) = tool
            .execute(&json!({"plan": [
                {"step": "a", "status": "in_progress"},
                {"step": "b", "status": "in_progress"},
            ]}))
            .await;
        assert!(!ok, "{out}");
        assert_eq!(shared.lock().unwrap().as_ref(), Some(&set));
        assert!(rx.try_recv().is_err());

        let (_, ok) = tool.execute(&json!({"plan": []})).await;
        assert!(ok);
        assert!(shared.lock().unwrap().is_none());
        assert!(matches!(rx.try_recv(), Ok(AgentEvent::Plan(None))));
    }
}
