//! The session hub: fans agent events out to every consumer (the TUI, the debug server)
//! and holds the pending approval so exactly one of them answers it.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::agent::{AgentEvent, Control};
use crate::cache::{CacheBreak, Hit};
use crate::client::Usage;
use crate::permissions::{Answer, Mode, Offers, Policy, Remember};
use crate::profile::Profile;

/// Events a slow consumer can fall behind by before it starts missing them.
const EVENT_BUFFER: usize = 4096;

/// An agent event as consumers see it: cloneable, serializable, approvals by id.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    /// A user message was accepted and a turn started.
    User(String),
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
    ToolOutput(String),
    ToolRejected(String),
    Usage(Usage),
    /// Usage of a child agent's model call.
    ChildUsage(Usage),
    /// A request broke the prompt cache, or `None` when the parent's last one was clean.
    Cache(Option<CacheBreak>),
    /// How well the cache served a judged call.
    CacheHit(Hit),
    /// A local notice, such as where `/context` wrote its export.
    Info(String),
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
    pub identity: String,
    pub mode: Mode,
    pub working: bool,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    /// Usage of the most recent model call.
    pub last_usage: Option<Usage>,
    /// Usage of every child agent, summed; not in the counts above.
    pub children: Usage,
    /// The most recent prompt cache break, if there has been one.
    pub last_cache_break: Option<CacheBreak>,
    pub pending: Option<Approval>,
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
    total: Usage,
    children: Usage,
    last_usage: Option<Usage>,
    last_cache_break: Option<CacheBreak>,
    next_id: u64,
    pending: Option<(Approval, oneshot::Sender<Answer>)>,
}

pub struct Session {
    model: String,
    identity: String,
    events: broadcast::Sender<Event>,
    inner: Mutex<Inner>,
    tx_user: mpsc::Sender<String>,
    tx_control: mpsc::Sender<Control>,
    cancel: Arc<AtomicBool>,
    policy: Arc<Policy>,
}

impl Session {
    pub fn new(
        model: String,
        identity: String,
        tx_user: mpsc::Sender<String>,
        tx_control: mpsc::Sender<Control>,
        cancel: Arc<AtomicBool>,
        policy: Arc<Policy>,
    ) -> Arc<Self> {
        Arc::new(Self {
            model,
            identity,
            events: broadcast::channel(EVENT_BUFFER).0,
            inner: Mutex::default(),
            tx_user,
            tx_control,
            cancel,
            policy,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn state(&self) -> State {
        let inner = self.lock();
        State {
            model: self.model.clone(),
            identity: self.identity.clone(),
            mode: self.policy.mode(),
            working: inner.working,
            input_tokens: inner.total.input,
            cached_tokens: inner.total.cached,
            output_tokens: inner.total.output,
            reasoning_tokens: inner.total.reasoning,
            last_usage: inner.last_usage,
            children: inner.children,
            last_cache_break: inner.last_cache_break.clone(),
            pending: inner.pending.as_ref().map(|(approval, _)| approval.clone()),
        }
    }

    /// Start a turn with `text`, unless one is already running.
    pub fn submit(&self, text: String) -> Result<(), SubmitError> {
        let mut inner = self.lock();
        if inner.working {
            return Err(SubmitError::Busy);
        }
        // Cleared here, not when the agent picks the message up, so an interrupt that
        // lands in between still stops the turn.
        self.cancel.store(false, Ordering::Relaxed);
        self.tx_user
            .try_send(text.clone())
            .map_err(|_| SubmitError::Closed)?;
        inner.working = true;
        self.publish(Event::User(text));
        Ok(())
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
        true
    }

    /// Switch the permission mode. The prompt and tools stay as they are.
    pub fn set_mode(&self, mode: Mode) {
        let _inner = self.lock();
        self.policy.set_mode(mode);
        self.publish(Event::Mode(mode));
    }

    /// Move to the next permission mode and return it.
    pub fn cycle_mode(&self) -> Mode {
        let _inner = self.lock();
        let mode = self.policy.mode().next();
        self.policy.set_mode(mode);
        self.publish(Event::Mode(mode));
        mode
    }

    /// What `/permissions` prints.
    pub fn permissions(&self) -> String {
        self.policy.describe()
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
            AgentEvent::CacheHit(hit) => Event::CacheHit(hit),
            AgentEvent::Info(s) => Event::Info(s),
            AgentEvent::Error(s) => Event::Error(s),
            AgentEvent::TurnEnd => {
                inner.working = false;
                Event::TurnEnd
            }
        };
        // Published under the lock so the state and the event order always agree.
        self.publish(event);
    }

    pub fn publish(&self, event: Event) {
        // No subscribers is fine; the event is simply dropped.
        let _ = self.events.send(event);
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

    fn session() -> (Arc<Session>, mpsc::Receiver<String>) {
        let (tx_user, rx_user) = mpsc::channel(1);
        let (tx_control, _) = mpsc::channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let policy = Arc::new(Policy::default());
        (
            Session::new(
                "m".to_string(),
                "general".to_string(),
                tx_user,
                tx_control,
                cancel,
                policy,
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
    fn submit_is_rejected_while_a_turn_runs() {
        let (session, mut rx_user) = session();
        assert_eq!(session.submit("a".to_string()), Ok(()));
        assert_eq!(rx_user.try_recv().unwrap(), "a");
        assert_eq!(session.submit("b".to_string()), Err(SubmitError::Busy));
        session.on_agent(AgentEvent::TurnEnd);
        assert_eq!(session.submit("c".to_string()), Ok(()));
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
