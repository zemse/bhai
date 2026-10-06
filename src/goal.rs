//! A goal the session works toward on its own, for `/goal`. While it is active, every
//! time the agent goes idle it opens another turn on it, until the model says it is done
//! or blocked, the user pauses or clears it, or its token budget is spent. Only the user
//! resumes user pauses and changes budgets. The model may adopt tasks from chat and
//! resume blocked tasks on the user's reply.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::client::Usage;

/// Zero means no token limit; only the user sets a finite budget.
pub const DEFAULT_BUDGET: u64 = 0;

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
    Paused,
    /// The model said the objective is met.
    Complete,
    /// The model said it cannot go on without the user.
    Blocked,
    /// The budget ran out.
    Spent,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Active => "active",
            State::Paused => "paused",
            State::Complete => "complete",
            State::Blocked => "blocked",
            State::Spent => "budget spent",
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
    /// Zero means unlimited.
    pub budget: u64,
    /// Consecutive autonomous turns without a work tool call.
    #[serde(default)]
    idle_turns: u8,
    /// Uncached input and output tokens spent while it was active, its children's too.
    pub spent: u64,
    pub state: State,
    /// Why it is blocked or paused, when something said.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// The children's spend already charged, so a settle adds only what is new. The
    /// ledger it reads starts again with the process, so a resumed goal starts at zero.
    #[serde(skip)]
    children: u64,
}

impl Goal {
    /// A goal on `objective`, counting the children's spend from `children` on.
    pub fn new(objective: &str, budget: u64, children: u64) -> Self {
        Self {
            objective: objective.to_string(),
            requirements: Vec::new(),
            verification: Vec::new(),
            plan: None,
            budget,
            idle_turns: 0,
            spent: 0,
            state: State::Active,
            note: String::new(),
            children,
        }
    }

    pub fn active(&self) -> bool {
        self.state == State::Active
    }

    /// Replacing the specification never changes the lifecycle or accounting.
    pub fn update(&mut self, spec: Specification) {
        self.objective = spec.objective;
        self.requirements = spec.requirements;
        self.verification = spec.verification;
    }

    /// Charge one model call of the session's own.
    pub fn charge(&mut self, usage: &Usage) {
        if self.active() {
            self.spent += cost(usage);
            self.check();
        }
    }

    /// Charge what the children have spent since the last look, from their running
    /// total. Only while it is active: a resume starts counting again from where they are.
    pub fn settle(&mut self, children: u64) {
        if self.active() {
            self.spent += children.saturating_sub(self.children);
            self.children = self.children.max(children);
            self.check();
        }
    }

    fn check(&mut self) {
        if self.active() && self.budget != 0 && self.spent >= self.budget {
            self.state = State::Spent;
        }
    }

    /// Stop it until the user resumes it, saying why.
    pub fn pause(&mut self, why: &str) {
        if self.active() {
            self.state = State::Paused;
            self.note = why.to_string();
        }
    }

    /// Pause narration-only loops without imposing a token budget.
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

    pub fn credits(&self) -> String {
        if self.budget == 0 {
            format!("{} tokens spent, no token limit", self.spent)
        } else {
            format!("{} of {} tokens", self.spent, self.budget)
        }
    }

    /// What the model is told when a turn opens on it.
    pub fn prompt(&self) -> String {
        format!(
            "{CONTINUE} Use the latest <goal_context> specification and recent results. \
Verify its requirements with the listed checks before completing; finish unblocked work \
before asking for essential input. {}.",
            self.credits()
        )
    }

    /// One line on where it stands, credits included.
    pub fn line(&self) -> String {
        let mut line = format!(
            "goal {}: {} ({})",
            self.state.label(),
            self.objective,
            self.credits()
        );
        if !self.note.is_empty() {
            line.push_str(&format!(": {}", self.note));
        }
        match self.state {
            State::Active | State::Complete => {}
            State::Spent => line.push_str("; /goal budget <tokens> to raise it"),
            State::Paused => line.push_str("; /goal resume to carry on"),
            State::Blocked => line.push_str("; reply with the missing input or /goal resume"),
        }
        line
    }
}

/// What a call costs a goal: the input the cache did not serve, and the output.
pub fn cost(usage: &Usage) -> u64 {
    usage.input.saturating_sub(usage.cached) + usage.output
}

/// What `/goal` was asked to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Show,
    Set(String),
    Pause,
    Resume,
    Budget(u64),
    Clear,
}

pub const USAGE: &str = "/goal <objective> to set one, /goal pause|resume|clear, /goal budget \
<tokens> (such as 50k or 1m), /goal budget none to remove the limit";

impl Command {
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        Ok(match text {
            "" => Command::Show,
            "pause" => Command::Pause,
            "resume" => Command::Resume,
            "clear" => Command::Clear,
            _ => match text.strip_prefix("budget ") {
                Some(n) if n.trim() == "none" => Command::Budget(0),
                Some(n) => Command::Budget(
                    tokens(n.trim())
                        .ok_or_else(|| format!("not a token count: {}. {USAGE}", n.trim()))?,
                ),
                None => Command::Set(text.to_string()),
            },
        })
    }
}

/// `50000`, `50k` or `1m`.
fn tokens(text: &str) -> Option<u64> {
    let lower = text.to_ascii_lowercase();
    let (digits, scale) = match lower.as_bytes().last()? {
        b'k' => (&lower[..lower.len() - 1], 1_000),
        b'm' => (&lower[..lower.len() - 1], 1_000_000),
        _ => (lower.as_str(), 1),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(scale))
        .filter(|&n| n > 0)
}

/// Apply `command` to the goal, with the children's running total for a new one. `Err`
/// says why it does not apply; `Show` changes nothing.
pub fn apply(goal: &mut Option<Goal>, command: Command, children: u64) -> Result<(), String> {
    match (command, goal.as_mut()) {
        (Command::Show, _) => {}
        (Command::Set(objective), _) => {
            *goal = Some(Goal::new(&objective, DEFAULT_BUDGET, children))
        }
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
            if goal.budget != 0 && goal.spent >= goal.budget {
                return Err(format!(
                    "the goal has spent its {} token budget; raise it with /goal budget <tokens>",
                    goal.budget
                ));
            }
            goal.state = State::Active;
            goal.idle_turns = 0;
            goal.note.clear();
            goal.children = goal.children.max(children);
        }
        (Command::Budget(budget), Some(goal)) => {
            goal.budget = budget;
            match goal.state {
                // Raised past what it spent, it waits for the user rather than starting.
                State::Spent if budget == 0 || goal.spent < budget => {
                    goal.state = State::Paused;
                    goal.note = "the budget was raised".to_string();
                }
                _ => goal.check(),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, cached: u64, output: u64) -> Usage {
        Usage {
            input,
            cached,
            cache_write: 0,
            output,
            reasoning: 0,
        }
    }

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
        let mut goal = Goal::new("build", DEFAULT_BUDGET, 0);
        goal.update(Specification::parse(&specification()).unwrap());
        let mut history = vec![context(&[], Some(&goal)).unwrap()];
        assert!(context(&history, Some(&goal)).is_none());
        goal.charge(&usage(10, 0, 1));
        goal.pause("interrupted");
        assert!(
            context(&history, Some(&goal)).is_none(),
            "accounting and state do not repeat the spec"
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
        assert_eq!(goal.spent, 8);
        assert_eq!(goal.state, State::Blocked);
    }

    #[test]
    fn commands_parse() {
        assert_eq!(Command::parse(" "), Ok(Command::Show));
        assert_eq!(Command::parse("pause"), Ok(Command::Pause));
        assert_eq!(Command::parse("budget 50k"), Ok(Command::Budget(50_000)));
        assert_eq!(Command::parse("budget 2M"), Ok(Command::Budget(2_000_000)));
        assert_eq!(Command::parse("budget 1200"), Ok(Command::Budget(1_200)));
        assert!(Command::parse("budget lots").is_err());
        assert!(Command::parse("budget 0").is_err());
        assert_eq!(
            Command::parse("make the tests pass"),
            Ok(Command::Set("make the tests pass".to_string()))
        );
    }

    #[test]
    fn only_uncached_input_and_output_are_charged() {
        let mut goal = Goal::new("x", 100, 0);
        goal.charge(&usage(50, 40, 5));
        assert_eq!(goal.spent, 15);
        assert!(goal.active());
    }

    #[test]
    fn the_budget_ends_the_goal_with_the_children_counted() {
        let mut goal = Goal::new("x", 100, 30);
        goal.charge(&usage(60, 0, 0));
        // Thirty were spent before the goal was set; forty since.
        goal.settle(70);
        assert_eq!(goal.spent, 100);
        assert_eq!(goal.state, State::Spent);
        // Nothing more is charged once it has ended.
        goal.charge(&usage(10, 0, 0));
        goal.settle(90);
        assert_eq!(goal.spent, 100);
    }

    #[test]
    fn a_spent_goal_resumes_only_once_the_user_raises_its_budget() {
        let mut goal = Some(Goal::new("x", 10, 0));
        goal.as_mut().unwrap().charge(&usage(20, 0, 0));
        let err = apply(&mut goal, Command::Resume, 0).unwrap_err();
        assert!(err.contains("/goal budget"), "{err}");
        apply(&mut goal, Command::Budget(50), 0).unwrap();
        assert_eq!(goal.as_ref().unwrap().state, State::Paused);
        apply(&mut goal, Command::Resume, 0).unwrap();
        assert!(goal.as_ref().unwrap().active());
        // Lowered under what it spent, it ends there and then.
        apply(&mut goal, Command::Budget(5), 0).unwrap();
        assert_eq!(goal.as_ref().unwrap().state, State::Spent);
    }

    #[test]
    fn a_resume_does_not_charge_what_the_children_spent_while_it_was_paused() {
        let mut goal = Some(Goal::new("x", 1_000, 0));
        apply(&mut goal, Command::Pause, 0).unwrap();
        goal.as_mut().unwrap().settle(300);
        apply(&mut goal, Command::Resume, 300).unwrap();
        goal.as_mut().unwrap().settle(310);
        assert_eq!(goal.unwrap().spent, 10);
    }

    #[test]
    fn commands_on_no_goal_say_how_to_set_one() {
        let mut goal = None;
        assert!(apply(&mut goal, Command::Pause, 0).is_err());
        assert!(apply(&mut goal, Command::Show, 0).is_ok());
        apply(&mut goal, Command::Set("ship it".to_string()), 0).unwrap();
        assert_eq!(goal.as_ref().unwrap().budget, DEFAULT_BUDGET);
        apply(&mut goal, Command::Clear, 0).unwrap();
        assert!(goal.is_none());
    }

    #[test]
    fn goals_are_unlimited_unless_the_user_sets_a_budget() {
        let mut shared = None;
        apply(&mut shared, Command::Set("work".to_string()), 0).unwrap();
        let goal = shared.as_mut().unwrap();
        goal.charge(&usage(1_000_000, 0, 1));
        assert!(goal.active());
        assert!(goal.prompt().contains("no token limit"));
        apply(&mut shared, Command::Budget(10), 0).unwrap();
        assert_eq!(shared.as_ref().unwrap().state, State::Spent);
        assert_eq!(Command::parse("budget none"), Ok(Command::Budget(0)));
        apply(&mut shared, Command::Budget(0), 0).unwrap();
        assert_eq!(shared.as_ref().unwrap().state, State::Paused);
        apply(&mut shared, Command::Resume, 0).unwrap();
        assert!(shared.unwrap().active());
    }

    #[test]
    fn narration_only_turns_pause_and_work_resets_the_streak() {
        let mut goal = Goal::new("work", DEFAULT_BUDGET, 0);
        goal.progress(false);
        goal.progress(false);
        goal.progress(true);
        goal.progress(false);
        goal.progress(false);
        assert!(goal.active());
        goal.progress(false);
        assert_eq!(goal.state, State::Paused);
        let mut shared = Some(goal);
        apply(&mut shared, Command::Resume, 0).unwrap();
        shared.as_mut().unwrap().progress(false);
        assert!(shared.unwrap().active());
    }

    #[test]
    fn the_line_shows_the_credits_and_the_way_on() {
        let mut goal = Goal::new("fix it", 100, 0);
        goal.charge(&usage(30, 0, 2));
        assert_eq!(goal.line(), "goal active: fix it (32 of 100 tokens)");
        goal.pause("interrupted");
        assert_eq!(
            goal.line(),
            "goal paused: fix it (32 of 100 tokens): interrupted; /goal resume to carry on"
        );
    }
}
