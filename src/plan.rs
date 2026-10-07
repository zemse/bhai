//! Live progress on a goal, or a standalone checklist for analysis. `update_plan` can
//! replace or incrementally edit it. The full list stays on disk and in the UI state;
//! the model reads a focused window, restated through compaction.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Goal-owned progress, with a standalone checklist when there is no goal.
#[derive(Clone, Default)]
pub struct Shared {
    goal: crate::goal::Shared,
    standalone: Arc<Mutex<Option<Plan>>>,
}

impl Shared {
    pub fn new(goal: crate::goal::Shared) -> Self {
        Self {
            goal,
            standalone: Arc::default(),
        }
    }

    pub fn get(&self) -> Option<Plan> {
        let goal = self.goal.lock().unwrap_or_else(|e| e.into_inner());
        match goal.as_ref() {
            Some(goal) if goal.state == crate::goal::State::Complete => self
                .standalone
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .or_else(|| goal.plan.clone()),
            Some(goal) => goal.plan.clone(),
            None => self
                .standalone
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    pub fn update(&self, args: &Value) -> Result<Option<Plan>, String> {
        let mut goal = self.goal.lock().unwrap_or_else(|e| e.into_inner());
        let previous = match goal.as_ref() {
            Some(goal) if goal.state != crate::goal::State::Complete => goal.plan.clone(),
            _ => self
                .standalone
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        };
        let next = Plan::edited(args, previous.as_ref())?;
        match goal.as_mut() {
            Some(goal) if goal.state != crate::goal::State::Complete => {
                if let Some(previous) = &goal.plan {
                    for step in &previous.steps {
                        if !next
                            .as_ref()
                            .is_some_and(|p| p.steps.iter().any(|s| s.step == step.step))
                        {
                            return Err(format!(
                                "Keep the existing step {:?}; mark unnecessary work skipped with a reason instead of dropping it.",
                                step.step
                            ));
                        }
                    }
                }
                goal.plan = next.clone();
            }
            _ => *self.standalone.lock().unwrap_or_else(|e| e.into_inner()) = next.clone(),
        }
        Ok(next)
    }

    pub fn clear_standalone(&self) {
        self.standalone
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }

    pub fn restore_standalone(&self, plan: Option<Plan>) {
        *self.standalone.lock().unwrap_or_else(|e| e.into_inner()) = plan;
    }
}

/// Steps a plan may hold, and characters a step may run to: it is a checklist the user
/// glances at, not a place to write the work out.
const MAX_STEPS: usize = 500;
const MAX_STEP: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    InProgress,
    Completed,
    Skipped,
}

impl Status {
    pub fn resolved(self) -> bool {
        matches!(self, Self::Completed | Self::Skipped)
    }

    /// The box the step is drawn with.
    pub fn mark(self) -> &'static str {
        match self {
            Status::Pending => "[ ]",
            Status::InProgress => "[>]",
            Status::Completed => "[x]",
            Status::Skipped => "[-]",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub step: String,
    pub status: Status,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

impl Step {
    pub fn text(&self) -> String {
        let mut text = format!("{} {}", self.status.mark(), self.step);
        if !self.reason.is_empty() {
            text.push_str(&format!(": {}", self.reason));
        }
        text
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// Why the plan changed, when the model said.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub explanation: String,
    pub steps: Vec<Step>,
}

impl Plan {
    /// Incremental edits avoid resending finished work just to update a status.
    pub fn edited(args: &Value, previous: Option<&Plan>) -> Result<Option<Plan>, String> {
        if args.get("plan").is_some() {
            if args.get("changes").is_some() || args.get("append").is_some() {
                return Err("Use either a whole plan or changes/append, not both.".to_string());
            }
            return Self::parse(args);
        }
        let mut steps =
            previous.map_or_else(Vec::new, |p| p.steps.iter().map(|s| json!(s)).collect());
        if let Some(changes) = args.get("changes") {
            let changes = changes.as_array().ok_or("`changes` must be an array.")?;
            for change in changes {
                let name = change
                    .get("step")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .ok_or("a change needs a step name.")?;
                let step = steps
                    .iter_mut()
                    .find(|s| s["step"] == name)
                    .ok_or_else(|| format!("no existing step {name:?}."))?;
                step["status"] = change
                    .get("status")
                    .cloned()
                    .ok_or("a change needs a status.")?;
                step["reason"] = change.get("reason").cloned().unwrap_or(json!(""));
            }
        }
        if let Some(append) = args.get("append") {
            steps.extend(
                append
                    .as_array()
                    .ok_or("`append` must be an array.")?
                    .iter()
                    .cloned(),
            );
        }
        if args.get("changes").is_none() && args.get("append").is_none() {
            return Err("supply `plan`, `changes` or `append`.".to_string());
        }
        let explanation = args
            .get("explanation")
            .cloned()
            .unwrap_or_else(|| json!(previous.map_or("", |p| p.explanation.as_str())));
        Self::parse(&json!({"plan": steps, "explanation": explanation}))
    }

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
                        "step {} needs a `status` of `pending`, `in_progress`, `completed` or `skipped`.",
                        at + 1
                    )
                })?;
            let reason = item
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim();
            if reason.chars().count() > MAX_STEP || (status == Status::Skipped && reason.is_empty())
            {
                return Err(format!(
                    "step {} needs a short reason when skipped (at most {MAX_STEP} characters).",
                    at + 1
                ));
            }
            if steps.iter().any(|s: &Step| s.step == step) {
                return Err(format!(
                    "step {} duplicates {:?}; step names must be unique.",
                    at + 1,
                    step
                ));
            }
            steps.push(Step {
                step: step.to_string(),
                status,
                reason: reason.to_string(),
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

    /// Every step is completed or explicitly skipped.
    pub fn done(&self) -> bool {
        self.steps.iter().all(|s| s.status.resolved())
    }

    pub fn skipped(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| s.status == Status::Skipped)
            .count()
    }

    /// The next call needs the current work, not every finished step.
    pub fn focused(&self) -> String {
        if self.steps.len() <= 12 {
            return self.text();
        }
        let focus = self
            .current()
            .or_else(|| self.steps.iter().position(|s| !s.status.resolved()))
            .unwrap_or(self.steps.len().saturating_sub(1));
        let top = focus.saturating_sub(2);
        let end = (top + 12).min(self.steps.len());
        format!(
            "{}; showing steps {}..{} (use update_plan offset/limit for other steps):\n{}",
            self.line(),
            top + 1,
            end,
            self.steps[top..end]
                .iter()
                .map(Step::text)
                .collect::<Vec<_>>()
                .join("\n")
        )
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
            .map(Step::text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One line on where it stands, for a probe.
    pub fn line(&self) -> String {
        let mut line = format!("plan {}/{} done", self.completed(), self.steps.len());
        if self.skipped() > 0 {
            line.push_str(&format!(", {} skipped", self.skipped()));
        }
        if let Some(at) = self.current() {
            line.push_str(&format!(": {}", self.steps[at].step));
        }
        line
    }
}

const CONTEXT: &str = "<plan_context>";

pub fn is_context(item: &Value) -> bool {
    item["role"] == "developer"
        && item
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .is_some_and(|s| s.starts_with(CONTEXT))
}

pub fn restated_context(history: &[Value]) -> Option<Value> {
    history.iter().rev().find(|item| is_context(item)).cloned()
}

pub fn context(history: &[Value], plan: Option<&Plan>) -> Option<Value> {
    let latest = restated_context(history);
    if latest.is_none() && plan.is_none() {
        return None;
    }
    let text = plan.map_or("No plan.".to_string(), Plan::focused);
    let item = json!({"type": "message", "role": "developer", "content": [{
        "type": "input_text", "text": format!("{CONTEXT}\n{text}\n</plan_context>")
    }]});
    (latest.as_ref() != Some(&item)).then_some(item)
}

/// What a summary is followed by when there is a plan, so the model still has the
/// checklist once the call that set it is folded away.
pub fn restated(summary: &str, plan: Option<&Plan>) -> String {
    match plan {
        Some(plan) => format!(
            "{summary}\n\nYour plan, as you last set it with `update_plan`:\n{}",
            plan.focused()
        ),
        None => summary.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn goal_owned_edits_preserve_progress_and_never_drop_steps() {
        let goal = Arc::new(Mutex::new(Some(crate::goal::Goal::new("ship"))));
        let shared = Shared::new(Arc::clone(&goal));
        shared
            .update(&json!({"plan": [
                {"step": "inspect", "status": "completed"},
                {"step": "implement", "status": "in_progress"}
            ]}))
            .unwrap();
        let owned = goal.lock().unwrap().as_ref().unwrap().plan.clone();
        assert_eq!(owned, shared.get());
        let previous = shared.get();
        assert!(shared.update(&json!({"plan": []})).is_err());
        assert!(
            shared
                .update(&json!({"changes": [{"step": "implement", "status": "skipped"}]}))
                .is_err()
        );
        assert!(
            shared
                .update(&json!({"changes": [{"step": "missing", "status": "completed"}]}))
                .is_err()
        );
        assert_eq!(shared.get(), previous);
        shared.update(&json!({
            "changes": [{"step": "implement", "status": "skipped", "reason": "existing implementation suffices"}],
            "append": [{"step": "verify", "status": "in_progress"}]
        })).unwrap();
        let updated = shared.get().unwrap();
        assert_eq!(updated.steps[0].status, Status::Completed);
        assert_eq!(updated.skipped(), 1);
        assert!(!updated.done());
        shared
            .update(&json!({"changes": [{"step": "verify", "status": "completed"}]}))
            .unwrap();
        assert!(shared.get().unwrap().done());
        assert!(
            goal.lock().unwrap().as_ref().unwrap().active(),
            "checkboxes do not complete a goal"
        );
        assert_eq!(goal.lock().unwrap().as_ref().unwrap().objective, "ship");
    }

    #[test]
    fn standalone_plans_do_not_start_or_mutate_completed_goals() {
        let shared = Shared::default();
        shared
            .update(&json!({"append": [{"step": "analyse", "status": "pending"}]}))
            .unwrap();
        assert!(shared.goal.lock().unwrap().is_none());
        shared.update(&json!({"plan": []})).unwrap();
        assert!(shared.get().is_none());
        let mut saved = crate::goal::Goal::new("finished");
        saved.plan =
            Plan::parse(&json!({"plan": [{"step": "old", "status": "completed"}]})).unwrap();
        saved.state = crate::goal::State::Complete;
        *shared.goal.lock().unwrap() = Some(saved.clone());
        shared
            .update(&json!({"append": [{"step": "new analysis", "status": "pending"}]}))
            .unwrap();
        assert_eq!(shared.goal.lock().unwrap().as_ref(), Some(&saved));
        assert_eq!(shared.get().unwrap().steps[0].step, "new analysis");
    }

    #[test]
    fn growing_plans_keep_full_progress_but_context_stays_focused() {
        let steps: Vec<_> = (0..500).map(|i| json!({"step": format!("step {i}"),
            "status": if i < 450 { "completed" } else if i == 450 { "in_progress" } else { "pending" }
        })).collect();
        let plan = Plan::parse(&json!({"plan": steps})).unwrap().unwrap();
        assert_eq!(plan.steps.len(), 500);
        assert_eq!(plan.completed(), 450);
        let snapshot = context(&[], Some(&plan)).unwrap();
        let text = snapshot["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("[>] step 450"));
        assert!(!text.contains("[x] step 0\n"));
        assert!(text.lines().count() <= 15);
        assert!(context(&[snapshot], Some(&plan)).is_none());
        assert!(Plan::parse(&json!({"plan": [
            {"step": "duplicate", "status": "pending"}, {"step": "duplicate", "status": "completed"}
        ]})).is_err());
    }

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
