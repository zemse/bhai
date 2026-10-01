//! The checklist the model keeps for the user with `update_plan`. Each call replaces the
//! whole list. The TUI shows it above the prompt, `/state` reports it, the session file
//! records it for a resume, and a compaction restates it in the summary, since the call
//! that set it may be among the turns folded away.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The plan as the agent loop and the `update_plan` tool both see it.
pub type Shared = Arc<Mutex<Option<Plan>>>;

/// Steps a plan may hold, and characters a step may run to: it is a checklist the user
/// glances at, not a place to write the work out.
const MAX_STEPS: usize = 50;
const MAX_STEP: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    InProgress,
    Completed,
}

impl Status {
    /// The box the step is drawn with.
    pub fn mark(self) -> &'static str {
        match self {
            Status::Pending => "[ ]",
            Status::InProgress => "[>]",
            Status::Completed => "[x]",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub step: String,
    pub status: Status,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// Why the plan changed, when the model said.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub explanation: String,
    pub steps: Vec<Step>,
}

impl Plan {
    /// The plan an `update_plan` call sets; `None` for an empty list, which clears it.
    /// At most one step is in progress, a rule nanocodex states but never checks.
    pub fn parse(args: &Value) -> Result<Option<Plan>, String> {
        let Some(items) = args.get("plan").and_then(Value::as_array) else {
            return Err("missing required array field `plan`.".to_string());
        };
        if items.len() > MAX_STEPS {
            return Err(format!(
                "a plan holds at most {MAX_STEPS} steps, got {}.",
                items.len()
            ));
        }
        let mut steps = Vec::with_capacity(items.len());
        for (at, item) in items.iter().enumerate() {
            let step = item
                .get("step")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| format!("step {} has no `step` text.", at + 1))?;
            if step.chars().count() > MAX_STEP {
                return Err(format!(
                    "step {} runs past {MAX_STEP} characters; keep each step to a line.",
                    at + 1
                ));
            }
            let status = item
                .get("status")
                .cloned()
                .and_then(|s| serde_json::from_value(s).ok())
                .ok_or_else(|| {
                    format!(
                        "step {} needs a `status` of `pending`, `in_progress` or `completed`.",
                        at + 1
                    )
                })?;
            steps.push(Step {
                step: step.to_string(),
                status,
            });
        }
        let running = steps
            .iter()
            .filter(|s| s.status == Status::InProgress)
            .count();
        if running > 1 {
            return Err(format!(
                "{running} steps are `in_progress`; at most one may be. Nothing changed."
            ));
        }
        if steps.is_empty() {
            return Ok(None);
        }
        let explanation = args
            .get("explanation")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        Ok(Some(Plan { explanation, steps }))
    }

    pub fn completed(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| s.status == Status::Completed)
            .count()
    }

    /// Every step is completed, so there is nothing left on it to watch.
    pub fn done(&self) -> bool {
        self.completed() == self.steps.len()
    }

    /// The step in progress, if one is.
    pub fn current(&self) -> Option<usize> {
        self.steps
            .iter()
            .position(|s| s.status == Status::InProgress)
    }

    /// One line per step, as a summary restates it to the model.
    pub fn text(&self) -> String {
        self.steps
            .iter()
            .map(|s| format!("{} {}", s.status.mark(), s.step))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One line on where it stands, for a probe.
    pub fn line(&self) -> String {
        let mut line = format!("plan {}/{} done", self.completed(), self.steps.len());
        if let Some(at) = self.current() {
            line.push_str(&format!(": {}", self.steps[at].step));
        }
        line
    }
}

/// What a summary is followed by when there is a plan, so the model still has the
/// checklist once the call that set it is folded away.
pub fn restated(summary: &str, plan: Option<&Plan>) -> String {
    match plan {
        Some(plan) => format!(
            "{summary}\n\nYour plan, as you last set it with `update_plan`:\n{}",
            plan.text()
        ),
        None => summary.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_plan_parses_trimmed_and_an_empty_one_clears() {
        let plan = Plan::parse(&json!({
            "explanation": " found the bug ",
            "plan": [
                {"step": " read it ", "status": "completed"},
                {"step": "fix it", "status": "in_progress"},
                {"step": "test it", "status": "pending"},
            ]
        }))
        .unwrap()
        .unwrap();
        assert_eq!(plan.explanation, "found the bug");
        assert_eq!(plan.steps[0].step, "read it");
        assert_eq!((plan.completed(), plan.current()), (1, Some(1)));
        assert!(!plan.done());
        assert_eq!(plan.text(), "[x] read it\n[>] fix it\n[ ] test it");
        assert_eq!(plan.line(), "plan 1/3 done: fix it");
        assert_eq!(Plan::parse(&json!({"plan": []})), Ok(None));
    }

    #[test]
    fn at_most_one_step_is_in_progress() {
        let err = Plan::parse(&json!({"plan": [
            {"step": "a", "status": "in_progress"},
            {"step": "b", "status": "in_progress"},
        ]}))
        .unwrap_err();
        assert!(err.contains("at most one"), "{err}");
    }

    #[test]
    fn a_bad_step_is_refused_with_its_number() {
        for (args, said) in [
            (json!({}), "`plan`"),
            (
                json!({"plan": [{"step": "a", "status": "doing"}]}),
                "step 1",
            ),
            (
                json!({"plan": [{"step": "a", "status": "pending"}, {"step": " ", "status": "pending"}]}),
                "step 2",
            ),
            (
                json!({"plan": [{"step": "x".repeat(MAX_STEP + 1), "status": "pending"}]}),
                "characters",
            ),
        ] {
            let err = Plan::parse(&args).unwrap_err();
            assert!(err.contains(said), "{err}");
        }
        let many: Vec<Value> = (0..=MAX_STEPS)
            .map(|_| json!({"step": "a", "status": "pending"}))
            .collect();
        assert!(Plan::parse(&json!({ "plan": many })).is_err());
    }

    #[test]
    fn a_summary_carries_the_plan() {
        let plan = Plan::parse(&json!({"plan": [{"step": "ship", "status": "pending"}]}))
            .unwrap()
            .unwrap();
        assert_eq!(restated("did things", None), "did things");
        let text = restated("did things", Some(&plan));
        assert!(text.starts_with("did things\n\n"), "{text}");
        assert!(text.ends_with("\n[ ] ship"), "{text}");
    }
}
