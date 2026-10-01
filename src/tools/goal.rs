//! `goal`, for the main agent: end the goal the user set, as met or as blocked. It
//! offers nothing else, so the model can stop working on its own but never start again,
//! and never moves the budget. Needs no approval.

use serde_json::{Value, json};

use super::{BoxFuture, Tool};
use crate::goal::{Shared, State};

pub const NAME: &str = "goal";

pub struct Goal {
    pub goal: Shared,
}

impl Tool for Goal {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": NAME,
            "description": "End the goal the user set with /goal, if there is one: `complete` \
        once it is met, `blocked` when you cannot go on without the user. Until then the session \
        keeps opening turns on it.",
            "strict": true,
            "parameters": {
                "type": "object",
                "properties": {
                    "status": { "type": "string", "enum": ["complete", "blocked"] },
                    "reason": { "type": "string", "description": "What was done, or what blocks it." }
                },
                "required": ["status", "reason"],
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        status(args).map(|state| format!("goal {}", state.label()))
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let state = match status(args) {
                Ok(state) => state,
                Err(e) => return (e, false),
            };
            let mut goal = self.goal.lock().unwrap_or_else(|e| e.into_inner());
            match goal.as_mut() {
                Some(goal) if goal.active() => {
                    goal.state = state;
                    goal.note = args
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    (
                        format!(
                            "Goal marked {}. Tell the user and end your turn.",
                            state.label()
                        ),
                        true,
                    )
                }
                Some(goal) => (
                    format!(
                        "The goal is {}, not active; nothing changed.",
                        goal.state.label()
                    ),
                    false,
                ),
                None => ("There is no goal set.".to_string(), false),
            }
        })
    }
}

fn status(args: &Value) -> Result<State, String> {
    match args.get("status").and_then(Value::as_str) {
        Some("complete") => Ok(State::Complete),
        Some("blocked") => Ok(State::Blocked),
        _ => Err("`status` must be `complete` or `blocked`.".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[tokio::test]
    async fn the_model_can_end_an_active_goal_and_nothing_else() {
        let shared: Shared = Arc::new(Mutex::new(Some(crate::goal::Goal::new("x", 100, 0))));
        let tool = Goal {
            goal: Arc::clone(&shared),
        };
        let (out, ok) = tool
            .execute(&json!({"status": "resume", "reason": ""}))
            .await;
        assert!(!ok, "{out}");
        let (_, ok) = tool
            .execute(&json!({"status": "blocked", "reason": "needs a key"}))
            .await;
        assert!(ok);
        let goal = shared.lock().unwrap().clone().unwrap();
        assert_eq!(
            (goal.state, goal.note.as_str()),
            (State::Blocked, "needs a key")
        );
        // Ended, it stays ended: only the user resumes it.
        let (_, ok) = tool
            .execute(&json!({"status": "complete", "reason": "done"}))
            .await;
        assert!(!ok);
        assert_eq!(
            shared.lock().unwrap().as_ref().unwrap().state,
            State::Blocked
        );
    }
}
