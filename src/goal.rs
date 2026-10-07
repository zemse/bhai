//! A goal the session works toward on its own, for `/goal`. While it is active, every
//! time the agent goes idle it opens another turn on it, until the model says it is done
//! or blocked, or the user pauses or clears it. Only the user resumes user pauses.
//! The model may adopt tasks from chat and resume blocked tasks on the user's reply.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The line that opens the message a goal turn starts on, so a history read back from
/// disk shows it as the harness's rather than as something the user typed.
pub const CONTINUE: &str = "Continue working toward the goal the user set.";

/// The goal as the agent loop and the `goal` tool both see it.
pub type Shared = Arc<Mutex<Option<Goal>>>;

/// The specification is a reminder, not a transcript or implementation plan.
pub const MAX_SPEC: usize = 1_024;
pub const MAX_OBJECTIVE: usize = 240;
pub const MAX_ITEMS: usize = 5;
pub const MAX_ITEM: usize = 160;
const CONTEXT: &str = "<goal_context>";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Specification {
    pub objective: String,
    pub requirements: Vec<String>,
    pub verification: Vec<String>,
}

impl Specification {
    pub fn parse(value: &Value) -> Result<Self, String> {
        let mut spec: Self = serde_json::from_value(value.clone())
            .map_err(|e| format!("`spec` needs objective, requirements and verification: {e}"))?;
        spec.objective = spec.objective.trim().to_string();
        if spec.objective.is_empty() || spec.objective.chars().count() > MAX_OBJECTIVE {
            return Err(format!("objective must be 1..{MAX_OBJECTIVE} characters."));
        }
        for (name, items) in [
            ("requirements", &mut spec.requirements),
            ("verification", &mut spec.verification),
        ] {
            if items.is_empty() || items.len() > MAX_ITEMS {
                return Err(format!("{name} must contain 1..{MAX_ITEMS} short items."));
            }
            for item in items {
                *item = item.trim().to_string();
                if item.is_empty() || item.chars().count() > MAX_ITEM {
                    return Err(format!(
                        "each {name} item must be 1..{MAX_ITEM} characters."
                    ));
                }
            }
        }
        let size = spec.objective.chars().count()
            + spec
                .requirements
                .iter()
                .chain(&spec.verification)
                .map(|s| s.chars().count())
                .sum::<usize>();
        if size > MAX_SPEC {
            return Err(format!(
                "goal specification exceeds {MAX_SPEC} characters; shorten it."
            ));
        }
        Ok(spec)
    }
}

/// Only changed specifications append a context item, leaving the cached prefix alone.
pub fn context(history: &[Value], goal: Option<&Goal>) -> Option<Value> {
    let latest = restated(history);
    if latest.is_none() && goal.is_none() {
        return None;
    }
    let spec = goal.map(|g| {
        json!({
            "objective": g.objective,
            "requirements": g.requirements,
            "verification": g.verification,
        })
    });
    let item = json!({
        "type": "message",
        "role": "developer",
        "content": [{"type": "input_text", "text": format!(
            "{CONTEXT}\n{}\n</goal_context>", json!(spec)
        )}],
    });
    (latest.as_ref() != Some(&item)).then_some(item)
}

pub fn is_context(item: &Value) -> bool {
    item["role"] == "developer"
        && item
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.starts_with(CONTEXT))
}

/// The last specification survives both local and server compaction verbatim.
pub fn restated(history: &[Value]) -> Option<Value> {
    history.iter().rev().find(|item| is_context(item)).cloned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Active,
    /// Older stopped goals remain resumable without starting work on load.
    #[serde(alias = "spent")]
    Paused,
    /// The model said the objective is met.
    Complete,
    /// The model said it cannot go on without the user.
    Blocked,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Active => "active",
            State::Paused => "paused",
            State::Complete => "complete",
            State::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Goal {
    pub objective: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requirements: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verification: Vec<String>,
    /// Progress belongs to the objective, but is not part of its small specification.
    #[serde(default)]
    pub plan: Option<crate::plan::Plan>,
    /// Consecutive autonomous turns without a work tool call.
    #[serde(default)]
    idle_turns: u8,
    pub state: State,
    /// Why it is blocked or paused, when something said.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

impl Goal {
    pub fn new(objective: &str) -> Self {
        Self {
            objective: objective.to_string(),
            requirements: Vec::new(),
            verification: Vec::new(),
            plan: None,
            idle_turns: 0,
            state: State::Active,
            note: String::new(),
        }
    }

    pub fn active(&self) -> bool {
        self.state == State::Active
    }

    /// Replacing the specification never changes the lifecycle or progress.
    pub fn update(&mut self, spec: Specification) {
        self.objective = spec.objective;
        self.requirements = spec.requirements;
        self.verification = spec.verification;
    }

    /// Stop it until the user resumes it, saying why.
    pub fn pause(&mut self, why: &str) {
        if self.active() {
            self.state = State::Paused;
            self.note = why.to_string();
        }
    }

    /// Pause narration-only loops.
    pub fn progress(&mut self, acted: bool) {
        if self.active() {
            self.idle_turns = if acted {
                0
            } else {
                self.idle_turns.saturating_add(1)
            };
            if self.idle_turns >= 3 {
                self.pause("three consecutive turns without work tool calls");
            }
        }
    }

    /// What the model is told when a turn opens on it.
    pub fn prompt(&self) -> String {
        format!(
            "{CONTINUE} Use the latest <goal_context> specification and recent results. \
Verify its requirements with the listed checks before completing; finish unblocked work \
before asking for essential input."
        )
    }

    /// One line on where it stands.
    pub fn line(&self) -> String {
        let mut line = format!("goal {}: {}", self.state.label(), self.objective);
        if !self.note.is_empty() {
            line.push_str(&format!(": {}", self.note));
        }
        match self.state {
            State::Active | State::Complete => {}
            State::Paused => line.push_str("; /goal resume to carry on"),
            State::Blocked => line.push_str("; reply with the missing input or /goal resume"),
        }
        line
    }
}

/// What `/goal` was asked to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Show,
    Set(String),
    Pause,
    Resume,
    Clear,
}

pub const USAGE: &str = "/goal <objective> to set one, /goal pause|resume|clear";

impl Command {
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        Ok(match text {
            "" => Command::Show,
            "pause" => Command::Pause,
            "resume" => Command::Resume,
            "clear" => Command::Clear,
            _ => Command::Set(text.to_string()),
        })
    }
}

/// Apply `command` to the goal. `Err` says why it does not apply; `Show` changes nothing.
pub fn apply(goal: &mut Option<Goal>, command: Command) -> Result<(), String> {
    match (command, goal.as_mut()) {
        (Command::Show, _) => {}
        (Command::Set(objective), _) => *goal = Some(Goal::new(&objective)),
        (Command::Clear, _) => *goal = None,
        (_, None) => return Err(format!("there is no goal. {USAGE}")),
        (Command::Pause, Some(goal)) => match goal.active() {
            true => goal.pause("paused by you"),
            false => return Err(format!("the goal is already {}", goal.state.label())),
        },
        (Command::Resume, Some(goal)) => {
            if goal.active() {
                return Err("the goal is already active".to_string());
            }
            goal.state = State::Active;
            goal.idle_turns = 0;
            goal.note.clear();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specification() -> Value {
        json!({
            "objective": " build a generator ",
            "requirements": [" separate Yul and Huff ", "use state_4 directories"],
            "verification": [" compare state_4 with existing outputs "]
        })
    }

    #[test]
    fn specifications_are_small_and_validated_before_use() {
        let parsed = Specification::parse(&specification()).unwrap();
        assert_eq!(parsed.objective, "build a generator");
        assert_eq!(parsed.requirements[0], "separate Yul and Huff");
        assert_eq!(
            parsed.verification[0],
            "compare state_4 with existing outputs"
        );
        for (key, value) in [
            ("objective", json!("x".repeat(MAX_OBJECTIVE + 1))),
            ("requirements", json!([])),
            ("verification", json!([" "])),
            ("requirements", json!(vec!["x"; MAX_ITEMS + 1])),
            ("verification", json!(["x".repeat(MAX_ITEM + 1)])),
        ] {
            let mut spec = specification();
            spec[key] = value;
            assert!(Specification::parse(&spec).is_err(), "{spec}");
        }
        let too_large = json!({
            "objective": "x".repeat(MAX_OBJECTIVE),
            "requirements": vec!["x".repeat(MAX_ITEM); MAX_ITEMS],
            "verification": ["x".repeat(MAX_ITEM)]
        });
        assert!(
            Specification::parse(&too_large)
                .unwrap_err()
                .contains("1024")
        );
    }

    #[test]
    fn context_appends_only_for_specification_changes_and_clear() {
        let mut goal = Goal::new("build");
        goal.update(Specification::parse(&specification()).unwrap());
        let mut history = vec![context(&[], Some(&goal)).unwrap()];
        assert!(context(&history, Some(&goal)).is_none());
        goal.pause("interrupted");
        assert!(
            context(&history, Some(&goal)).is_none(),
            "state does not repeat the spec"
        );
        goal.requirements
            .push("reject unsupported widths".to_string());
        let updated = context(&history, Some(&goal)).unwrap();
        history.push(updated.clone());
        assert_eq!(restated(&history), Some(updated));
        assert!(context(&history, Some(&goal)).is_none());
        let clear = context(&history, None).unwrap();
        assert!(
            clear["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("null")
        );
        history.push(clear);
        assert!(context(&history, None).is_none());
        assert!(context(&[], None).is_none());
    }

    #[test]
    fn older_saved_goals_load_without_specification_fields() {
        let goal: Goal = serde_json::from_value(json!({
            "objective": "old", "budget": 0, "spent": 8, "state": "blocked", "note": "needs input"
        }))
        .unwrap();
        assert!(goal.requirements.is_empty());
        assert!(goal.verification.is_empty());
        assert_eq!(goal.state, State::Blocked);
    }

    #[test]
    fn legacy_stopped_goals_resume_and_serialize_without_accounting() {
        let mut shared: Option<Goal> = Some(
            serde_json::from_value(json!({
                "objective": "finish stages", "budget": 200000, "spent": 242276,
                "state": "spent", "requirements": ["finish all stages"],
                "verification": ["run the gates"],
                "plan": {"steps": [{"step": "Stage 4", "status": "in_progress"}]}
            }))
            .unwrap(),
        );
        assert_eq!(shared.as_ref().unwrap().state, State::Paused);
        let progress = shared.as_ref().unwrap().plan.clone();
        apply(&mut shared, Command::Resume).unwrap();
        let goal = shared.unwrap();
        assert!(goal.active());
        assert_eq!(goal.plan, progress);
        assert_eq!(goal.requirements, ["finish all stages"]);
        assert_eq!(goal.verification, ["run the gates"]);
        let saved = serde_json::to_value(&goal).unwrap();
        assert!(saved.get("budget").is_none());
        assert!(saved.get("spent").is_none());
        assert_eq!(saved["state"], "active");
    }

    #[test]
    fn commands_parse() {
        assert_eq!(Command::parse(" "), Ok(Command::Show));
        assert_eq!(Command::parse("pause"), Ok(Command::Pause));
        assert_eq!(
            Command::parse("make the tests pass"),
            Ok(Command::Set("make the tests pass".to_string()))
        );
    }

    #[test]
    fn commands_on_no_goal_say_how_to_set_one() {
        let mut goal = None;
        assert!(apply(&mut goal, Command::Pause).is_err());
        assert!(apply(&mut goal, Command::Show).is_ok());
        apply(&mut goal, Command::Set("ship it".to_string())).unwrap();
        assert_eq!(goal.as_ref().unwrap().objective, "ship it");
        apply(&mut goal, Command::Clear).unwrap();
        assert!(goal.is_none());
    }

    #[test]
    fn narration_only_turns_pause_and_work_resets_the_streak() {
        let mut goal = Goal::new("work");
        goal.progress(false);
        goal.progress(false);
        goal.progress(true);
        goal.progress(false);
        goal.progress(false);
        assert!(goal.active());
        goal.progress(false);
        assert_eq!(goal.state, State::Paused);
        let mut shared = Some(goal);
        apply(&mut shared, Command::Resume).unwrap();
        shared.as_mut().unwrap().progress(false);
        assert!(shared.unwrap().active());
    }

    #[test]
    fn the_line_shows_the_objective_and_the_way_on() {
        let mut goal = Goal::new("fix it");
        assert_eq!(goal.line(), "goal active: fix it");
        goal.pause("interrupted");
        assert_eq!(
            goal.line(),
            "goal paused: fix it: interrupted; /goal resume to carry on"
        );
    }
}
