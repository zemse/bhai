//! A goal the session works toward on its own, for `/goal`. While it is active, every
//! time the agent goes idle it opens another turn on it, until the model says it is done
//! or blocked, the user pauses or clears it, or its token budget is spent. Only the user
//! starts it again: the model can end a goal, never resume one or move its budget.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::client::Usage;

/// Tokens a goal set without a budget may spend.
pub const DEFAULT_BUDGET: u64 = 200_000;

/// The line that opens the message a goal turn starts on, so a history read back from
/// disk shows it as the harness's rather than as something the user typed.
pub const CONTINUE: &str = "Continue working toward the goal the user set.";

/// The goal as the agent loop and the `goal` tool both see it.
pub type Shared = Arc<Mutex<Option<Goal>>>;

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
    pub budget: u64,
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
            budget,
            spent: 0,
            state: State::Active,
            note: String::new(),
            children,
        }
    }

    pub fn active(&self) -> bool {
        self.state == State::Active
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
        if self.active() && self.spent >= self.budget {
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

    /// What the model is told when a turn opens on it.
    pub fn prompt(&self) -> String {
        format!(
            "{CONTINUE}\n\nGoal: {}\n\nSpent {} of its {} token budget. Carry on from where you \
stopped. When the goal is met, call `goal` with status `complete`; if you cannot go on without \
the user, call it with status `blocked` and say why.",
            self.objective, self.spent, self.budget
        )
    }

    /// One line on where it stands, credits included.
    pub fn line(&self) -> String {
        let mut line = format!(
            "goal {}: {} ({} of {} tokens)",
            self.state.label(),
            self.objective,
            self.spent,
            self.budget
        );
        if !self.note.is_empty() {
            line.push_str(&format!(": {}", self.note));
        }
        match self.state {
            State::Active | State::Complete => {}
            State::Spent => line.push_str("; /goal budget <tokens> to raise it"),
            State::Paused | State::Blocked => line.push_str("; /goal resume to carry on"),
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
<tokens> (such as 50k or 1m)";

impl Command {
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        Ok(match text {
            "" => Command::Show,
            "pause" => Command::Pause,
            "resume" => Command::Resume,
            "clear" => Command::Clear,
            _ => match text.strip_prefix("budget ") {
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
            if goal.spent >= goal.budget {
                return Err(format!(
                    "the goal has spent its {} token budget; raise it with /goal budget <tokens>",
                    goal.budget
                ));
            }
            goal.state = State::Active;
            goal.note.clear();
            goal.children = goal.children.max(children);
        }
        (Command::Budget(budget), Some(goal)) => {
            goal.budget = budget;
            match goal.state {
                // Raised past what it spent, it waits for the user rather than starting.
                State::Spent if goal.spent < budget => {
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
