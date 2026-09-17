//! The session hub: fans agent events out to every consumer (the TUI, the debug server)
//! and holds the pending approval so exactly one of them answers it.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::agent::AgentEvent;

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
        command: String,
    },
    /// A pending approval was answered, by whichever consumer got there first.
    Resolved {
        id: u64,
        accepted: bool,
    },
    ToolStart(String),
    ToolOutput(String),
    ToolRejected(String),
    Usage {
        input: u64,
        output: u64,
    },
    Error(String),
    Interrupted,
    TurnEnd,
}

/// A command waiting for approval.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Approval {
    pub id: u64,
    pub command: String,
}

/// A snapshot of the session, as `GET /state` reports it.
#[derive(Debug, Clone, Serialize)]
pub struct State {
    pub model: String,
    pub working: bool,
    pub input_tokens: u64,
    pub output_tokens: u64,
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
    input_tokens: u64,
    output_tokens: u64,
    next_id: u64,
    pending: Option<(Approval, oneshot::Sender<bool>)>,
}

pub struct Session {
    model: String,
    events: broadcast::Sender<Event>,
    inner: Mutex<Inner>,
    tx_user: mpsc::Sender<String>,
    cancel: Arc<AtomicBool>,
}

impl Session {
    pub fn new(model: String, tx_user: mpsc::Sender<String>, cancel: Arc<AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            model,
            events: broadcast::channel(EVENT_BUFFER).0,
            inner: Mutex::default(),
            tx_user,
            cancel,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn state(&self) -> State {
        let inner = self.lock();
        State {
            model: self.model.clone(),
            working: inner.working,
            input_tokens: inner.input_tokens,
            output_tokens: inner.output_tokens,
            pending: inner.pending.as_ref().map(|(approval, _)| approval.clone()),
        }
    }

    /// Start a turn with `text`, unless one is already running.
    pub fn submit(&self, text: String) -> Result<(), SubmitError> {
        let mut inner = self.lock();
        if inner.working {
            return Err(SubmitError::Busy);
        }
        self.tx_user
            .try_send(text.clone())
            .map_err(|_| SubmitError::Closed)?;
        inner.working = true;
        self.publish(Event::User(text));
        Ok(())
    }

    /// Answer the pending approval, only if it is `id` when one is given. Returns the id
    /// answered, or `None` if there was nothing (or something else) to answer.
    pub fn answer(&self, accept: bool, id: Option<u64>) -> Option<u64> {
        let mut inner = self.lock();
        if id.is_some_and(|id| inner.pending.as_ref().map(|(a, _)| a.id) != Some(id)) {
            return None;
        }
        let (approval, reply) = inner.pending.take()?;
        let _ = reply.send(accept);
        self.publish(Event::Resolved {
            id: approval.id,
            accepted: accept,
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
        self.answer(false, None);
        self.publish(Event::Interrupted);
        true
    }

    fn on_agent(&self, event: AgentEvent) {
        let mut inner = self.lock();
        let event = match event {
            AgentEvent::Reasoning(s) => Event::Reasoning(s),
            AgentEvent::Text(s) => Event::Text(s),
            AgentEvent::Approval { command, reply } => {
                inner.next_id += 1;
                let approval = Approval {
                    id: inner.next_id,
                    command: command.clone(),
                };
                inner.pending = Some((approval, reply));
                Event::Approval {
                    id: inner.next_id,
                    command,
                }
            }
            AgentEvent::ToolStart(s) => Event::ToolStart(s),
            AgentEvent::ToolOutput(s) => Event::ToolOutput(s),
            AgentEvent::ToolRejected(s) => Event::ToolRejected(s),
            AgentEvent::Usage { input, output } => {
                inner.input_tokens += input;
                inner.output_tokens += output;
                Event::Usage { input, output }
            }
            AgentEvent::Error(s) => Event::Error(s),
            AgentEvent::TurnEnd => {
                inner.working = false;
                Event::TurnEnd
            }
        };
        // Published under the lock so the state and the event order always agree.
        self.publish(event);
    }

    fn publish(&self, event: Event) {
        // No subscribers is fine; the event is simply dropped.
        let _ = self.events.send(event);
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
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
        let cancel = Arc::new(AtomicBool::new(false));
        (Session::new("m".to_string(), tx_user, cancel), rx_user)
    }

    fn approval(session: &Session) -> oneshot::Receiver<bool> {
        let (reply, wait) = oneshot::channel();
        session.on_agent(AgentEvent::Approval {
            command: "ls".to_string(),
            reply,
        });
        wait
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
                command: "ls".to_string()
            }
        );
        // A stale id from another consumer does nothing.
        assert_eq!(session.answer(false, Some(7)), None);
        assert_eq!(session.answer(true, Some(1)), Some(1));
        assert_eq!(session.answer(false, None), None);
        assert_eq!(wait.try_recv(), Ok(true));
        assert_eq!(
            events.try_recv().unwrap(),
            Event::Resolved {
                id: 1,
                accepted: true
            }
        );
        assert!(session.state().pending.is_none());
    }

    #[test]
    fn interrupt_cancels_and_rejects_the_pending_approval() {
        let (session, _rx) = session();
        assert!(!session.interrupt());
        session.submit("a".to_string()).unwrap();
        let mut wait = approval(&session);
        assert!(session.interrupt());
        assert!(session.cancel.load(Ordering::Relaxed));
        assert_eq!(wait.try_recv(), Ok(false));
    }

    #[test]
    fn usage_accumulates() {
        let (session, _rx) = session();
        session.on_agent(AgentEvent::Usage {
            input: 3,
            output: 1,
        });
        session.on_agent(AgentEvent::Usage {
            input: 4,
            output: 2,
        });
        let state = session.state();
        assert_eq!((state.input_tokens, state.output_tokens), (7, 3));
    }
}
