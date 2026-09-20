//! The session hub: fans agent events out to every consumer (the TUI, the debug server)
//! and holds the pending approval so exactly one of them answers it.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::agent::{AgentEvent, Control};
use crate::cache::{CacheBreak, Hit};
use crate::client::Usage;
use crate::entries::Entries;
use crate::judge::Judge;
use crate::limits::RateLimits;
use crate::permissions::{Answer, Mode, Offers, Policy, Remember};
use crate::profile::{CallTokens, EntryTokens, Profile};
use crate::workflow::Workflow;

/// Events a slow consumer can fall behind by before it starts missing them.
const EVENT_BUFFER: usize = 4096;

/// An agent event as consumers see it: cloneable, serializable, approvals by id.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    /// A user message was accepted and a turn started.
    User(String),
    /// A user message typed while a turn ran; it waits at `position` in the queue.
    Queued {
        position: usize,
        text: String,
    },
    Reasoning(String),
    Text(String),
    Approval {
        id: u64,
        tool: String,
        command: String,
        #[serde(flatten)]
        offers: Offers,
    },
    /// A pending approval was answered, by whichever consumer got there first.
    Resolved {
        id: u64,
        accepted: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        remember: Option<Remember>,
    },
    ToolStart(String),
    /// Output of the running call so far; never part of history.
    ToolProgress(String),
    ToolOutput(String),
    ToolRejected(String),
    Usage(Usage),
    /// Usage of a child agent's model call.
    ChildUsage(Usage),
    /// A finished model call, split for per-entry token badges.
    Call(CallTokens),
    /// The user message or tool result just shown is history item `index`.
    Item(usize),
    /// A request broke the prompt cache, or `None` when the parent's last one was clean.
    Cache(Option<CacheBreak>),
    /// How well the cache served a judged call.
    CacheHit(Hit),
    /// That many judged calls in a row missed the cached prefix.
    CacheStalled(usize),
    /// The latest rate-limit headroom.
    RateLimits(RateLimits),
    /// A local notice, such as where `/context` wrote its export.
    Info(String),
    /// The judge is deciding that call, or `None` once it has.
    Judging(Option<String>),
    /// History was compacted; earlier history indexes no longer hold.
    Compacted(String),
    /// The permission mode changed.
    Mode(Mode),
    Error(String),
    Interrupted,
    TurnEnd,
}

/// A tool call waiting for approval.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Approval {
    pub id: u64,
    pub tool: String,
    pub command: String,
    /// Rules the user may choose to remember.
    #[serde(flatten)]
    pub offers: Offers,
}

/// A snapshot of the session, as `GET /state` reports it.
#[derive(Debug, Clone, Serialize)]
pub struct State {
    pub model: String,
    /// The reasoning effort the model runs at.
    pub effort: String,
    pub identity: String,
    pub mode: Mode,
    pub working: bool,
    /// Prompts waiting for the running turn, in the order they will run.
    pub queued: Vec<String>,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    /// Model calls finished, children not included.
    pub calls: u64,
    /// Usage of the most recent model call.
    pub last_usage: Option<Usage>,
    /// Usage of every child agent, summed; not in the counts above.
    pub children: Usage,
    /// Usage of the auto-approval judge; not in the counts above either.
    pub judge: Usage,
    /// The most recent prompt cache break, if there has been one.
    pub last_cache_break: Option<CacheBreak>,
    /// The latest rate-limit headroom, once the backend has reported it.
    pub rate_limits: Option<RateLimits>,
    pub pending: Option<Approval>,
    /// Transcript entries with token attribution, as the hover badges show them.
    pub entries: Vec<EntryTokens>,
}

/// A submitted prompt: what the agent is sent, and what the transcript shows. The two
/// differ for `/<skill>`, which reads as a command but goes to the model as a request to
/// use that skill.
#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    pub text: String,
    pub shown: String,
}

impl Prompt {
    /// A prompt shown as something other than what it sends.
    pub fn shown_as(text: String, shown: String) -> Self {
        Self { text, shown }
    }
}

/// A plain prompt shows exactly what it sends.
impl From<String> for Prompt {
    fn from(text: String) -> Self {
        Self {
            shown: text.clone(),
            text,
        }
    }
}

/// What `submit` did with the message.
#[derive(Debug, PartialEq)]
pub enum Submitted {
    /// The turn started.
    Started,
    /// A turn was running, so the message waits at this place in the queue.
    Queued { position: usize },
}

/// Why a message was not submitted.
#[derive(Debug, PartialEq)]
pub enum SubmitError {
    Busy,
    Closed,
}

impl fmt::Display for SubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SubmitError::Busy => "a turn is already running",
            SubmitError::Closed => "the agent is not accepting messages",
        })
    }
}

#[derive(Default)]
struct Inner {
    working: bool,
    /// Prompts typed while a turn ran, oldest first.
    queue: VecDeque<Prompt>,
    total: Usage,
    calls: u64,
    children: Usage,
    last_usage: Option<Usage>,
    last_cache_break: Option<CacheBreak>,
    rate_limits: Option<RateLimits>,
    next_id: u64,
    pending: Option<(Approval, oneshot::Sender<Answer>)>,
}

pub struct Session {
    model: String,
    effort: String,
    identity: String,
    events: broadcast::Sender<Event>,
    inner: Mutex<Inner>,
    entries: Mutex<Entries>,
    tx_user: mpsc::Sender<String>,
    tx_control: mpsc::Sender<Control>,
    cancel: Arc<AtomicBool>,
    policy: Arc<Policy>,
    /// The auto-approval judge, when one runs; it owns its own usage and budget.
    judge: Option<Arc<Judge>>,
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model: String,
        effort: String,
        identity: String,
        tx_user: mpsc::Sender<String>,
        tx_control: mpsc::Sender<Control>,
        cancel: Arc<AtomicBool>,
        policy: Arc<Policy>,
        judge: Option<Arc<Judge>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            model,
            effort,
            identity,
            events: broadcast::channel(EVENT_BUFFER).0,
            inner: Mutex::default(),
            entries: Mutex::default(),
            tx_user,
            tx_control,
            cancel,
            policy,
            judge,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn state(&self) -> State {
        let inner = self.lock();
        State {
            model: self.model.clone(),
            effort: self.effort.clone(),
            identity: self.identity.clone(),
            mode: self.policy.mode(),
            working: inner.working,
            queued: inner.queue.iter().map(|p| p.shown.clone()).collect(),
            input_tokens: inner.total.input,
            cached_tokens: inner.total.cached,
            output_tokens: inner.total.output,
            reasoning_tokens: inner.total.reasoning,
            calls: inner.calls,
            last_usage: inner.last_usage,
            children: inner.children,
            judge: self
                .judge
                .as_ref()
                .map_or_else(Usage::default, |j| j.total()),
            last_cache_break: inner.last_cache_break.clone(),
            rate_limits: inner.rate_limits,
            pending: inner.pending.as_ref().map(|(approval, _)| approval.clone()),
            entries: self.entries().attributed(),
        }
    }

    /// Start a turn with `prompt`, or queue it when one is already running.
    pub fn submit(&self, prompt: impl Into<Prompt>) -> Result<Submitted, SubmitError> {
        let prompt = prompt.into();
        let mut inner = self.lock();
        if inner.working {
            let text = prompt.shown.clone();
            inner.queue.push_back(prompt);
            let position = inner.queue.len();
            self.publish(Event::Queued { position, text });
            return Ok(Submitted::Queued { position });
        }
        // Cleared here, not when the agent picks the message up, so an interrupt that
        // lands in between still stops the turn.
        self.cancel.store(false, Ordering::Relaxed);
        self.tx_user
            .try_send(prompt.text)
            .map_err(|_| SubmitError::Closed)?;
        inner.working = true;
        self.publish(Event::User(prompt.shown));
        Ok(Submitted::Started)
    }

    /// The prompts waiting for the running turn, for `/queue`, as they were typed.
    pub fn queued(&self) -> Vec<String> {
        self.lock().queue.iter().map(|p| p.shown.clone()).collect()
    }

    /// Drop every queued prompt, for `/queue clear`. Returns how many there were.
    pub fn clear_queue(&self) -> usize {
        let dropped = self.lock().queue.drain(..).count();
        if dropped > 0 {
            self.entries().drop_queued();
            self.publish(Event::Info(format!("dropped {dropped} queued prompt(s)")));
        }
        dropped
    }

    /// Hand the next queued prompt to the agent, with the state locked. A prompt the
    /// agent will not take keeps its place rather than being lost.
    fn start_queued(&self, inner: &mut Inner) {
        let Some(prompt) = inner.queue.pop_front() else {
            return;
        };
        self.cancel.store(false, Ordering::Relaxed);
        if let Err(e) = self.tx_user.try_send(prompt.text.clone()) {
            inner
                .queue
                .push_front(Prompt::shown_as(e.into_inner(), prompt.shown));
            inner.working = false;
            return;
        }
        inner.working = true;
        self.publish(Event::User(prompt.shown));
    }

    /// Answer the pending approval, only if it is `id` when one is given, and only with
    /// a rule it offered. Returns the id answered, or `None` if there was nothing (or
    /// something else) to answer.
    pub fn answer(&self, answer: Answer, id: Option<u64>) -> Option<u64> {
        let mut inner = self.lock();
        let (approval, _) = inner.pending.as_ref()?;
        if id.is_some_and(|id| approval.id != id)
            || answer
                .remember()
                .is_some_and(|r| approval.offers.get(r).is_none())
        {
            return None;
        }
        let (approval, reply) = inner.pending.take()?;
        let _ = reply.send(answer);
        self.publish(Event::Resolved {
            id: approval.id,
            accepted: answer.accepted(),
            remember: answer.remember(),
        });
        Some(approval.id)
    }

    /// Stop the running turn, rejecting any pending approval. Returns false when idle.
    pub fn interrupt(&self) -> bool {
        if !self.lock().working {
            return false;
        }
        // Cancel first so the agent does not move on to the next call once rejected.
        self.cancel.store(true, Ordering::Relaxed);
        self.answer(Answer::Reject, None);
        self.publish(Event::Interrupted);
        // Stop means stop, so nothing that was waiting behind the turn runs.
        self.clear_queue();
        true
    }

    /// Switch the permission mode. The prompt and tools stay as they are.
    /// Set the mode, as far as trust allows, and return the mode now in force.
    pub fn set_mode(&self, mode: Mode) -> Mode {
        let _inner = self.lock();
        let mode = self.policy.set_mode(mode);
        self.publish(Event::Mode(mode));
        mode
    }

    /// Move to the next permission mode the project may be in, and return it. In an
    /// untrusted project that is `ask` and stays `ask`.
    pub fn cycle_mode(&self) -> Mode {
        let _inner = self.lock();
        let mode = self.policy.set_mode(self.policy.next_mode());
        self.publish(Event::Mode(mode));
        mode
    }

    /// What `/permissions` prints.
    pub fn permissions(&self) -> String {
        let mut out = self.policy.describe();
        if self.policy.mode() == Mode::Auto {
            out.push_str(&match &self.judge {
                Some(judge) => judge.describe(),
                None => "\njudge: off".to_string(),
            });
        }
        out
    }

    /// Honour the repo-supplied allow rules, for `/trust`.
    pub fn trust(&self) -> anyhow::Result<String> {
        let notice = self.policy.trust()?;
        // Trusting the project lets it into the mode the config asked for.
        self.publish(Event::Mode(self.policy.mode()));
        Ok(notice)
    }

    /// Stop honouring the repo-supplied allow rules, for `/untrust`.
    pub fn untrust(&self) -> anyhow::Result<String> {
        let notice = self.policy.untrust()?;
        self.publish(Event::Mode(self.policy.mode()));
        Ok(notice)
    }

    /// Summarise the history now, as a turn of its own, unless one is already running.
    pub fn compact(&self) -> Result<(), SubmitError> {
        let mut inner = self.lock();
        if inner.working {
            return Err(SubmitError::Busy);
        }
        self.cancel.store(false, Ordering::Relaxed);
        self.tx_control
            .try_send(Control::Compact)
            .map_err(|_| SubmitError::Closed)?;
        inner.working = true;
        self.publish(Event::Info("compacting history".to_string()));
        Ok(())
    }

    /// Run `workflow` now, as a turn of its own, unless one is already running. The
    /// agent asks for confirmation before it launches anything.
    pub fn workflow(&self, workflow: Arc<Workflow>, input: String) -> Result<(), SubmitError> {
        let mut inner = self.lock();
        if inner.working {
            return Err(SubmitError::Busy);
        }
        self.cancel.store(false, Ordering::Relaxed);
        self.tx_control
            .try_send(Control::Workflow { workflow, input })
            .map_err(|_| SubmitError::Closed)?;
        inner.working = true;
        Ok(())
    }

    /// Ask the agent for a token breakdown of its context; `None` if it has gone away.
    pub async fn context(&self) -> Option<Profile> {
        let (reply, wait) = oneshot::channel();
        self.tx_control.send(Control::Context(reply)).await.ok()?;
        wait.await.ok()
    }

    fn on_agent(&self, event: AgentEvent) {
        let mut inner = self.lock();
        let event = match event {
            AgentEvent::Reasoning(s) => Event::Reasoning(s),
            AgentEvent::Text(s) => Event::Text(s),
            AgentEvent::Approval {
                tool,
                command,
                offers,
                reply,
            } => {
                inner.next_id += 1;
                let approval = Approval {
                    id: inner.next_id,
                    tool: tool.clone(),
                    command: command.clone(),
                    offers: offers.clone(),
                };
                inner.pending = Some((approval, reply));
                Event::Approval {
                    id: inner.next_id,
                    tool,
                    command,
                    offers,
                }
            }
            AgentEvent::ToolStart(s) => Event::ToolStart(s),
            AgentEvent::ToolProgress(s) => Event::ToolProgress(s),
            AgentEvent::ToolOutput(s) => Event::ToolOutput(s),
            AgentEvent::ToolRejected(s) => Event::ToolRejected(s),
            AgentEvent::Usage(usage) => {
                add(&mut inner.total, usage);
                inner.last_usage = Some(usage);
                Event::Usage(usage)
            }
            AgentEvent::ChildUsage(usage) => {
                add(&mut inner.children, usage);
                Event::ChildUsage(usage)
            }
            AgentEvent::Cache(found) => {
                if let Some(found) = &found {
                    inner.last_cache_break = Some(found.clone());
                }
                Event::Cache(found)
            }
            AgentEvent::Call(call) => {
                inner.calls += 1;
                Event::Call(call)
            }
            AgentEvent::Item(index) => Event::Item(index),
            AgentEvent::CacheHit(hit) => Event::CacheHit(hit),
            AgentEvent::CacheStalled(misses) => Event::CacheStalled(misses),
            AgentEvent::RateLimits(limits) => {
                inner.rate_limits = Some(limits);
                Event::RateLimits(limits)
            }
            AgentEvent::Info(s) => Event::Info(s),
            AgentEvent::Judging(what) => Event::Judging(what),
            AgentEvent::Compacted(s) => Event::Compacted(s),
            AgentEvent::Error(s) => Event::Error(s),
            AgentEvent::TurnEnd => {
                // The session keeps working while queued prompts wait behind the turn.
                inner.working = !inner.queue.is_empty();
                Event::TurnEnd
            }
        };
        let ended = event == Event::TurnEnd;
        // Published under the lock so the state and the event order always agree.
        self.publish(event);
        if ended {
            self.start_queued(&mut inner);
        }
    }

    pub fn publish(&self, event: Event) {
        self.entries().apply(&event);
        // No subscribers is fine; the event is simply dropped.
        let _ = self.events.send(event);
    }

    /// The transcript. Never lock the session state while holding it.
    pub fn entries(&self) -> MutexGuard<'_, Entries> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn add(total: &mut Usage, usage: Usage) {
    total.input += usage.input;
    total.cached += usage.cached;
    total.output += usage.output;
    total.reasoning += usage.reasoning;
}

/// Feed the agent's events into the session until the agent goes away.
pub async fn pump(session: Arc<Session>, mut rx_agent: mpsc::UnboundedReceiver<AgentEvent>) {
    while let Some(event) = rx_agent.recv().await {
        session.on_agent(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entries::Entry;

    fn session() -> (Arc<Session>, mpsc::Receiver<String>) {
        let (tx_user, rx_user) = mpsc::channel(1);
        let (tx_control, _) = mpsc::channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let policy = Arc::new(Policy::default());
        (
            Session::new(
                "m".to_string(),
                "medium".to_string(),
                "general".to_string(),
                tx_user,
                tx_control,
                cancel,
                policy,
                None,
            ),
            rx_user,
        )
    }

    fn approval(session: &Session) -> oneshot::Receiver<Answer> {
        let (reply, wait) = oneshot::channel();
        session.on_agent(AgentEvent::Approval {
            tool: "bash".to_string(),
            command: "ls".to_string(),
            offers: Offers {
                exact: Some("Bash(ls)".to_string()),
                prefix: None,
            },
            reply,
        });
        wait
    }

    #[tokio::test]
    async fn context_is_none_once_the_agent_is_gone() {
        let (session, _rx) = session();
        assert!(session.context().await.is_none());
    }

    #[test]
    fn events_serialize_with_a_type_tag() {
        let json = serde_json::to_value(Event::Text("hi".to_string())).unwrap();
        assert_eq!(json, serde_json::json!({"type": "text", "data": "hi"}));
        let json = serde_json::to_value(Event::TurnEnd).unwrap();
        assert_eq!(json, serde_json::json!({"type": "turn_end"}));
    }

    #[test]
    fn prompts_sent_while_a_turn_runs_queue_in_order() {
        let (session, mut rx_user) = session();
        let mut events = session.subscribe();
        assert_eq!(session.submit("a".to_string()), Ok(Submitted::Started));
        assert_eq!(rx_user.try_recv().unwrap(), "a");
        assert_eq!(
            session.submit("b".to_string()),
            Ok(Submitted::Queued { position: 1 })
        );
        assert_eq!(
            session.submit("c".to_string()),
            Ok(Submitted::Queued { position: 2 })
        );
        let state = session.state();
        assert!(state.working);
        assert_eq!(state.queued, ["b", "c"]);

        // The first queued prompt starts as the turn ends, the next behind it.
        session.on_agent(AgentEvent::TurnEnd);
        assert_eq!(rx_user.try_recv().unwrap(), "b");
        assert!(session.state().working);
        assert_eq!(session.state().queued, ["c"]);
        session.on_agent(AgentEvent::TurnEnd);
        assert_eq!(rx_user.try_recv().unwrap(), "c");
        session.on_agent(AgentEvent::TurnEnd);
        assert!(!session.state().working);
        assert!(session.state().queued.is_empty());

        let seen: Vec<Event> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        assert_eq!(
            seen,
            [
                Event::User("a".to_string()),
                Event::Queued {
                    position: 1,
                    text: "b".to_string()
                },
                Event::Queued {
                    position: 2,
                    text: "c".to_string()
                },
                Event::TurnEnd,
                Event::User("b".to_string()),
                Event::TurnEnd,
                Event::User("c".to_string()),
                Event::TurnEnd,
            ]
        );
        // The queued entries became the user entries, with nothing left over.
        let entries = session.entries();
        let kinds: Vec<_> = entries.list.iter().map(Entry::kind).collect();
        assert_eq!(kinds, ["user", "user", "user"]);
        let texts: Vec<_> = entries.list.iter().map(Entry::text).collect();
        assert_eq!(texts, ["a", "b", "c"]);
    }

    #[test]
    fn a_prompt_can_show_something_other_than_what_it_sends() {
        let (session, mut rx_user) = session();
        let mut events = session.subscribe();
        let typed = || Prompt::shown_as("Use the `pdf` skill.".to_string(), "/pdf".to_string());
        session.submit(typed()).unwrap();
        assert_eq!(rx_user.try_recv().unwrap(), "Use the `pdf` skill.");
        assert_eq!(events.try_recv(), Ok(Event::User("/pdf".to_string())));

        // Queued, and then started from the queue, it still shows as it was typed.
        session.submit(typed()).unwrap();
        assert_eq!(session.queued(), ["/pdf"]);
        assert_eq!(session.state().queued, ["/pdf"]);
        session.on_agent(AgentEvent::TurnEnd);
        assert_eq!(rx_user.try_recv().unwrap(), "Use the `pdf` skill.");
        let seen: Vec<Event> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        assert!(seen.contains(&Event::User("/pdf".to_string())), "{seen:?}");
        let entries = session.entries();
        let texts: Vec<_> = entries.list.iter().map(Entry::text).collect();
        assert_eq!(texts, ["/pdf", "/pdf"]);
    }

    #[test]
    fn an_interrupt_drops_the_queue() {
        let (session, mut rx_user) = session();
        session.submit("a".to_string()).unwrap();
        assert_eq!(rx_user.try_recv().unwrap(), "a");
        session.submit("b".to_string()).unwrap();
        session.submit("c".to_string()).unwrap();
        assert!(session.interrupt());
        assert!(session.state().queued.is_empty());
        session.on_agent(AgentEvent::TurnEnd);
        assert!(!session.state().working);
        assert!(rx_user.try_recv().is_err());
        let entries = session.entries();
        let texts: Vec<_> = entries.list.iter().map(Entry::text).collect();
        assert_eq!(
            texts,
            [
                "a",
                "dropped: b",
                "dropped: c",
                "interrupted",
                "dropped 2 queued prompt(s)"
            ]
        );
    }

    #[test]
    fn clearing_the_queue_leaves_the_turn_running() {
        let (session, _rx) = session();
        assert_eq!(session.clear_queue(), 0);
        session.submit("a".to_string()).unwrap();
        session.submit("b".to_string()).unwrap();
        assert_eq!(session.queued(), ["b"]);
        assert_eq!(session.clear_queue(), 1);
        assert!(session.queued().is_empty());
        assert!(session.state().working);
    }

    #[test]
    fn an_approval_is_answered_exactly_once() {
        let (session, _rx) = session();
        let mut events = session.subscribe();
        let mut wait = approval(&session);
        assert_eq!(
            events.try_recv().unwrap(),
            Event::Approval {
                id: 1,
                tool: "bash".to_string(),
                command: "ls".to_string(),
                offers: Offers {
                    exact: Some("Bash(ls)".to_string()),
                    prefix: None,
                },
            }
        );
        // A stale id from another consumer does nothing, nor does a rule not offered.
        assert_eq!(session.answer(Answer::Reject, Some(7)), None);
        let prefix = Answer::Accept(Some(Remember::Prefix));
        assert_eq!(session.answer(prefix, Some(1)), None);
        let exact = Answer::Accept(Some(Remember::Exact));
        assert_eq!(session.answer(exact, Some(1)), Some(1));
        assert_eq!(session.answer(Answer::Reject, None), None);
        assert_eq!(wait.try_recv(), Ok(exact));
        assert_eq!(
            events.try_recv().unwrap(),
            Event::Resolved {
                id: 1,
                accepted: true,
                remember: Some(Remember::Exact),
            }
        );
        assert!(session.state().pending.is_none());
    }

    #[test]
    fn an_interrupt_before_the_agent_starts_survives() {
        let (session, _rx) = session();
        session.cancel.store(true, Ordering::Relaxed);
        session.submit("hi".to_string()).unwrap();
        assert!(!session.cancel.load(Ordering::Relaxed));
        assert!(session.interrupt());
        assert!(session.cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn interrupt_cancels_and_rejects_the_pending_approval() {
        let (session, _rx) = session();
        assert!(!session.interrupt());
        session.submit("a".to_string()).unwrap();
        let mut wait = approval(&session);
        assert!(session.interrupt());
        assert!(session.cancel.load(Ordering::Relaxed));
        assert_eq!(wait.try_recv(), Ok(Answer::Reject));
    }

    #[test]
    fn mode_changes_are_published_and_reported() {
        let (session, _rx) = session();
        let mut events = session.subscribe();
        assert_eq!(session.state().mode, Mode::Ask);
        assert_eq!(session.cycle_mode(), Mode::Auto);
        assert_eq!(events.try_recv().unwrap(), Event::Mode(Mode::Auto));
        session.set_mode(Mode::Ask);
        assert_eq!(events.try_recv().unwrap(), Event::Mode(Mode::Ask));
        assert_eq!(session.state().mode, Mode::Ask);
        let json = serde_json::to_value(Event::Mode(Mode::Bypass)).unwrap();
        assert_eq!(json, serde_json::json!({"type": "mode", "data": "bypass"}));
    }

    #[test]
    fn the_last_cache_break_outlives_clean_calls() {
        let (session, _rx) = session();
        let found = CacheBreak {
            field: "tools".to_string(),
            detail: "changed".to_string(),
        };
        assert_eq!(session.state().last_cache_break, None);
        session.on_agent(AgentEvent::Cache(Some(found.clone())));
        session.on_agent(AgentEvent::Cache(None));
        assert_eq!(session.state().last_cache_break, Some(found));
    }

    #[test]
    fn usage_accumulates() {
        let (session, _rx) = session();
        let usage = |input, cached, output, reasoning| Usage {
            input,
            cached,
            output,
            reasoning,
        };
        session.on_agent(AgentEvent::Usage(usage(3, 0, 1, 1)));
        session.on_agent(AgentEvent::Usage(usage(4, 2, 2, 0)));
        let state = session.state();
        assert_eq!((state.input_tokens, state.output_tokens), (7, 3));
        assert_eq!((state.cached_tokens, state.reasoning_tokens), (2, 1));
        assert_eq!(state.last_usage, Some(usage(4, 2, 2, 0)));
        session.on_agent(AgentEvent::Call(CallTokens::default()));
        assert_eq!(session.state().calls, 1);

        // A child's usage is counted apart and leaves the last call alone.
        session.on_agent(AgentEvent::ChildUsage(usage(10, 5, 3, 1)));
        session.on_agent(AgentEvent::ChildUsage(usage(1, 0, 1, 0)));
        let state = session.state();
        assert_eq!(state.input_tokens, 7);
        assert_eq!(state.children, usage(11, 5, 4, 1));
        assert_eq!(state.last_usage, Some(usage(4, 2, 2, 0)));
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["children"]["input"], 11);
    }
}
