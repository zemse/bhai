//! The session hub: fans agent events out to every consumer (the TUI, the debug server)
//! and holds the pending approval so exactly one of them answers it.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::agent::{AgentEvent, Cancel, Control, Mailboxes};
use crate::cache::{CacheBreak, Hit};
use crate::client::Usage;
use crate::entries::{Entries, Entry};
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
    /// A turn started on its own, on the reports of children that finished while the
    /// session was idle; the string says what they were doing. Nothing was typed, so
    /// the transcript shows the reports, not a message.
    Resumed(String),
    /// What this session is working on, in a few words. The TUI puts it in the
    /// terminal's title; nothing else has a use for it.
    Titled(String),
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
    /// A tool call started: which tool, and the one-line summary of the call.
    ToolStart {
        tool: String,
        summary: String,
    },
    /// Output of the running call so far; never part of history.
    ToolProgress(String),
    ToolOutput(String),
    ToolRejected(String),
    Usage(Usage),
    /// Usage of a child agent's model call.
    ChildUsage(Usage),
    /// A child agent started, with who it runs as and what it was asked.
    ChildStarted {
        id: String,
        identity: String,
        description: String,
        task: String,
    },
    /// A child agent ended, and whether it got where it was going.
    ChildEnded {
        id: String,
        ok: bool,
    },
    /// Something a child agent said, for that child's own pane.
    Child {
        id: String,
        event: Box<Event>,
    },
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
    /// A model call went out with this many tokens of prompt behind it; the wait for
    /// its first token is the backend reading them.
    Sending(u64),
    /// A model call is on the wire, or is over; what the tokens a second readout times.
    Streaming(bool),
    /// A local notice, such as where `/context` wrote its export.
    Info(String),
    /// The judge is deciding that call, or `None` once it has.
    Judging(Option<String>),
    /// History was compacted; earlier history indexes no longer hold. `summary` is what
    /// the earlier turns were folded into, when they were folded rather than evicted.
    Compacted {
        notice: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    /// History was dropped, so the transcript that showed it goes too.
    Cleared,
    /// The permission mode changed.
    Mode(Mode),
    /// `/model` switched the session to this model and reasoning effort.
    Model {
        model: String,
        effort: String,
    },
    Error(String),
    /// The turn failed rather than answering. The history still stands, so `retry` can
    /// run the same turn again without the user retyping anything.
    TurnFailed(String),
    Interrupted,
    TurnEnd,
}

/// How far a child agent got.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildState {
    Running,
    Done,
    Failed,
}

/// One child agent as the panel lists it: what it is and how it is doing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChildRow {
    pub id: String,
    pub identity: String,
    pub description: String,
    pub state: ChildState,
    /// Events heard from this child. A coarse sign of progress, since a child that is
    /// working says something every few seconds and one that is wedged says nothing.
    pub steps: u32,
    /// When the last of them arrived. Reported as seconds, which is the form a reader
    /// wants: from outside, a child in a retry loop and one on a long build look the
    /// same until you can see how long it has been quiet.
    #[serde(rename = "idle_secs", serialize_with = "idle_secs")]
    pub last: Instant,
}

impl ChildRow {
    pub fn idle(&self) -> Duration {
        self.last.elapsed()
    }
}

fn idle_secs<S: serde::Serializer>(last: &Instant, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_u64(last.elapsed().as_secs())
}

/// A child agent's own transcript, which the panel opens and a message can be typed
/// into. Its entries are behind their own lock so a pane can be read while the session
/// goes on publishing.
struct Pane {
    row: ChildRow,
    entries: Arc<Mutex<Entries>>,
}

/// One child agent as `/export-debug` writes it: its row and everything its pane showed.
#[derive(Debug, Clone)]
pub struct ChildLog {
    pub row: ChildRow,
    pub entries: Vec<Entry>,
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
    /// Calls waiting on the user, oldest first. Parallel workflow steps each park one,
    /// so there can be several; only the front is on screen.
    pending: VecDeque<(Approval, oneshot::Sender<Answer>)>,
}

pub struct Session {
    /// The model and the reasoning effort it runs at; `/model` changes both.
    model: Mutex<(String, String)>,
    identity: String,
    events: broadcast::Sender<Event>,
    inner: Mutex<Inner>,
    entries: Mutex<Entries>,
    /// The child agents of the running turn, oldest first.
    children: Mutex<Vec<Pane>>,
    /// Children the panel has let go of, oldest first, kept so the debug export can
    /// still say what they did.
    ended: Mutex<Vec<Pane>>,
    /// Where a message typed into a child's pane is posted; the agent fills it in as
    /// each child starts.
    mailboxes: Mailboxes,
    tx_user: mpsc::Sender<String>,
    tx_control: mpsc::Sender<Control>,
    cancel: Arc<Cancel>,
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
        cancel: Arc<Cancel>,
        policy: Arc<Policy>,
        judge: Option<Arc<Judge>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            model: Mutex::new((model, effort)),
            identity,
            events: broadcast::channel(EVENT_BUFFER).0,
            inner: Mutex::default(),
            entries: Mutex::default(),
            children: Mutex::default(),
            ended: Mutex::default(),
            mailboxes: Mailboxes::default(),
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
        let (model, effort) = self.model();
        State {
            model,
            effort,
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
            pending: inner.pending.front().map(|(approval, _)| approval.clone()),
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
        self.cancel.clear();
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
        self.cancel.clear();
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

    /// Answer the approval on screen, only if it is `id` when one is given, and only
    /// with a rule it offered. Returns the id answered, or `None` if there was nothing
    /// (or something else) to answer.
    pub fn answer(&self, answer: Answer, id: Option<u64>) -> Option<u64> {
        let mut inner = self.lock();
        let (approval, _) = inner.pending.front()?;
        if id.is_some_and(|id| approval.id != id)
            || answer
                .remember()
                .is_some_and(|r| approval.offers.get(r).is_none())
        {
            return None;
        }
        let (approval, reply) = inner.pending.pop_front()?;
        let _ = reply.send(answer);
        self.publish(Event::Resolved {
            id: approval.id,
            accepted: answer.accepted(),
            remember: answer.remember(),
        });
        // Whatever was parked behind it takes its place on screen.
        if let Some((next, _)) = inner.pending.front() {
            self.publish(Event::Approval {
                id: next.id,
                tool: next.tool.clone(),
                command: next.command.clone(),
                offers: next.offers.clone(),
            });
        }
        Some(approval.id)
    }

    /// Stop the running turn and every child still running, rejecting every pending
    /// approval. Returns false when there was nothing to stop.
    pub fn interrupt(&self) -> bool {
        // A child detached in an earlier turn outlives it, so an idle session can still
        // have work in flight to stop.
        if !self.lock().working && !self.child_running() {
            return false;
        }
        // Cancel first so the agent does not move on to the next call once rejected.
        self.cancel.stop();
        // Every parked call, not just the one on screen: parallel steps park their own.
        while self.answer(Answer::Reject, None).is_some() {}
        self.publish(Event::Interrupted);
        // What was typed behind the turn keeps its place: an interrupt cancels the call
        // in flight, not the prompts the user has already asked for. `/queue clear`
        // drops them.
        true
    }

    /// The model the session talks to and the effort it runs at.
    pub fn model(&self) -> (String, String) {
        self.model.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Talk to `model` at `effort` from the next turn on, for `/model`. `window` is that
    /// model's context window where its backend says, so compaction keeps its bearings.
    /// Refused while a turn runs, since a switch mid-call would answer with one model
    /// what another was asked.
    pub fn set_model(
        &self,
        model: String,
        effort: String,
        window: Option<u64>,
    ) -> Result<(), SubmitError> {
        let inner = self.lock();
        if inner.working {
            return Err(SubmitError::Busy);
        }
        self.tx_control
            .try_send(Control::Model {
                model: model.clone(),
                effort: effort.clone(),
                window,
            })
            .map_err(|_| SubmitError::Closed)?;
        *self.model.lock().unwrap_or_else(|e| e.into_inner()) = (model.clone(), effort.clone());
        self.publish(Event::Model { model, effort });
        Ok(())
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

    /// The mode the config asked for, which trust may be holding back, for the debug
    /// export: a session sitting in `ask` when it was started in `auto` is a question
    /// about trust rather than about the mode.
    pub fn wanted_mode(&self) -> Mode {
        self.policy.wanted()
    }

    /// Whether this project's own settings files are trusted.
    pub fn trusted(&self) -> bool {
        self.policy.trusted()
    }

    /// Add an allow rule, for `/allow`. It is the user's own, so it holds in every mode
    /// and is saved where a remembered approval goes. `auto` mode never prompts, so this
    /// is the only way to permit a call from the prompt rather than by changing mode:
    /// saying "allow that" in a message reaches the model, which cannot grant it.
    pub fn allow(&self, rule: &str) -> anyhow::Result<String> {
        Ok(match self.policy.remember(rule)? {
            Some(store) => format!("allowed {rule}, saved to {}", store.display()),
            None => format!("allowed {rule} for this session"),
        })
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
    /// `asked` is what the user wants the summary to keep, from `/compact <prompt>`.
    pub fn compact(&self, asked: Option<String>) -> Result<(), SubmitError> {
        let mut inner = self.lock();
        if inner.working {
            return Err(SubmitError::Busy);
        }
        self.cancel.clear();
        let notice = match &asked {
            Some(asked) => format!("compacting history, keeping {asked}"),
            None => "compacting history".to_string(),
        };
        self.tx_control
            .try_send(Control::Compact(asked))
            .map_err(|_| SubmitError::Closed)?;
        inner.working = true;
        self.publish(Event::Info(notice));
        Ok(())
    }

    /// Run the last turn again, after it failed. Nothing is added to the history, so the
    /// call goes out as the failed one did, and the user retypes nothing.
    pub fn retry(&self) -> Result<(), SubmitError> {
        let mut inner = self.lock();
        if inner.working {
            return Err(SubmitError::Busy);
        }
        self.cancel.clear();
        self.tx_control
            .try_send(Control::Retry)
            .map_err(|_| SubmitError::Closed)?;
        inner.working = true;
        self.publish(Event::Info("retrying".to_string()));
        Ok(())
    }

    /// Drop the conversation: the agent's history and the transcript that showed it.
    /// Refused while a turn runs, since the turn holds the history it would drop.
    pub fn clear(&self) -> Result<(), SubmitError> {
        let inner = self.lock();
        if inner.working {
            return Err(SubmitError::Busy);
        }
        self.tx_control
            .try_send(Control::Clear)
            .map_err(|_| SubmitError::Closed)?;
        Ok(())
    }

    /// Run `workflow` now, as a turn of its own, unless one is already running. The
    /// agent asks for confirmation before it launches anything.
    pub fn workflow(&self, workflow: Arc<Workflow>, input: String) -> Result<(), SubmitError> {
        let mut inner = self.lock();
        if inner.working {
            return Err(SubmitError::Busy);
        }
        self.cancel.clear();
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
                let id = inner.next_id;
                inner.pending.push_back((
                    Approval {
                        id,
                        tool: tool.clone(),
                        command: command.clone(),
                        offers: offers.clone(),
                    },
                    reply,
                ));
                // One prompt at a time: a call parked behind another is published when
                // that one is answered, rather than replacing it on screen unseen.
                if inner.pending.len() > 1 {
                    return;
                }
                Event::Approval {
                    id,
                    tool,
                    command,
                    offers,
                }
            }
            AgentEvent::ToolStart { tool, summary } => Event::ToolStart { tool, summary },
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
            AgentEvent::ChildStarted {
                id,
                identity,
                description,
                task,
            } => Event::ChildStarted {
                id,
                identity,
                description,
                task,
            },
            AgentEvent::ChildEnded { id, ok } => Event::ChildEnded { id, ok },
            AgentEvent::Child { id, event } => match said(*event) {
                Some(event) => Event::Child {
                    id,
                    event: Box::new(event),
                },
                // Nothing a pane can show, so nothing is published.
                None => return,
            },
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
            AgentEvent::Sending(tokens) => Event::Sending(tokens),
            AgentEvent::Streaming(on) => Event::Streaming(on),
            AgentEvent::Info(s) => Event::Info(s),
            AgentEvent::Judging(what) => Event::Judging(what),
            AgentEvent::Titled(name) => Event::Titled(name),
            AgentEvent::Compacted { notice, summary } => Event::Compacted { notice, summary },
            AgentEvent::Cleared => Event::Cleared,
            AgentEvent::Error(s) => Event::Error(s),
            AgentEvent::TurnFailed(s) => Event::TurnFailed(s),
            // The agent is working again without anything having been typed, so the
            // session says so: what the user sends now queues behind it, as it would
            // behind any other turn.
            AgentEvent::Resumed(what) => {
                inner.working = true;
                Event::Resumed(what)
            }
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
        self.route(&event);
        self.entries().apply(&event);
        // No subscribers is fine; the event is simply dropped.
        let _ = self.events.send(event);
    }

    /// Keep the child panes up with the event, before the transcript sees it.
    fn route(&self, event: &Event) {
        let mut children = self.panes();
        match event {
            // The panel lists what is running and what this turn started, so a new turn
            // clears the finished rows and leaves the children still going.
            Event::User(_) | Event::Resumed(_) => {
                let (running, ended): (Vec<_>, Vec<_>) = std::mem::take(&mut *children)
                    .into_iter()
                    .partition(|pane| pane.row.state == ChildState::Running);
                *children = running;
                self.ended
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(ended);
            }
            Event::ChildStarted {
                id,
                identity,
                description,
                task,
            } => {
                let entries = Entries::default();
                let entries = Arc::new(Mutex::new(entries));
                entries
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(Entry::User(task.clone()));
                children.push(Pane {
                    row: ChildRow {
                        id: id.clone(),
                        identity: identity.clone(),
                        description: description.clone(),
                        state: ChildState::Running,
                        steps: 0,
                        last: Instant::now(),
                    },
                    entries,
                });
            }
            Event::ChildEnded { id, ok } => {
                if let Some(pane) = children.iter_mut().find(|p| p.row.id == *id) {
                    pane.row.state = match ok {
                        true => ChildState::Done,
                        false => ChildState::Failed,
                    };
                }
            }
            Event::Child { id, event } => {
                if let Some(pane) = children.iter_mut().find(|p| p.row.id == *id) {
                    pane.row.steps += 1;
                    pane.row.last = Instant::now();
                    let entries = Arc::clone(&pane.entries);
                    drop(children);
                    entries
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .apply(event);
                }
            }
            _ => {}
        }
    }

    /// The mailboxes to hand the agent, so what is typed into a pane reaches its child.
    pub fn mailboxes(&self) -> Mailboxes {
        Arc::clone(&self.mailboxes)
    }

    /// Post `text` to a running child agent. It joins that child's history before its
    /// next model call, and shows in its pane straight away.
    pub fn steer(&self, id: &str, text: String) -> Result<(), SubmitError> {
        let posted = self
            .mailboxes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .map(|tx| tx.send(text.clone()).is_ok())
            .unwrap_or_default();
        if !posted {
            return Err(SubmitError::Closed);
        }
        self.publish(Event::Child {
            id: id.to_string(),
            event: Box::new(Event::User(text)),
        });
        Ok(())
    }

    /// Whether any child agent is still running, detached from the turn that started it.
    pub fn child_running(&self) -> bool {
        self.panes()
            .iter()
            .any(|pane| pane.row.state == ChildState::Running)
    }

    /// The child agents of the running turn, for the panel.
    pub fn children(&self) -> Vec<ChildRow> {
        self.panes().iter().map(|pane| pane.row.clone()).collect()
    }

    /// Every child agent of the session, the ones the panel has let go of first.
    pub fn child_logs(&self) -> Vec<ChildLog> {
        let log = |pane: &Pane| ChildLog {
            row: pane.row.clone(),
            entries: pane
                .entries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .list
                .clone(),
        };
        let mut logs: Vec<ChildLog> = self
            .ended
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(log)
            .collect();
        logs.extend(self.panes().iter().map(log));
        logs
    }

    /// One child agent's transcript, which the pane renders and reads live.
    pub fn child_entries(&self, id: &str) -> Option<Arc<Mutex<Entries>>> {
        self.panes()
            .iter()
            .find(|pane| pane.row.id == id)
            .map(|pane| Arc::clone(&pane.entries))
    }

    fn panes(&self) -> MutexGuard<'_, Vec<Pane>> {
        self.children.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The transcript. Never lock the session state while holding it.
    pub fn entries(&self) -> MutexGuard<'_, Entries> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What a child said, as an event for its own pane. Only what `run_child` wraps can
/// get here; anything else has no place in a pane and is dropped.
fn said(event: AgentEvent) -> Option<Event> {
    Some(match event {
        AgentEvent::Reasoning(s) => Event::Reasoning(s),
        AgentEvent::Text(s) => Event::Text(s),
        AgentEvent::ToolStart { tool, summary } => Event::ToolStart { tool, summary },
        AgentEvent::ToolProgress(s) => Event::ToolProgress(s),
        AgentEvent::ToolOutput(s) => Event::ToolOutput(s),
        AgentEvent::ToolRejected(s) => Event::ToolRejected(s),
        AgentEvent::Info(s) => Event::Info(s),
        AgentEvent::Error(s) => Event::Error(s),
        _ => return None,
    })
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
        let cancel = Arc::new(Cancel::default());
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

    /// Start child `id` and have it say something, as `run_child` does.
    fn child(session: &Session, id: &str, task: &str) {
        session.on_agent(AgentEvent::ChildStarted {
            id: id.to_string(),
            identity: "worker".to_string(),
            description: "look around".to_string(),
            task: task.to_string(),
        });
    }

    #[test]
    fn a_childs_work_lands_in_its_own_pane_not_the_transcript() {
        let (session, _rx) = session();
        child(&session, "a1", "count the files");
        child(&session, "b2", "read the docs");
        session.on_agent(AgentEvent::Child {
            id: "a1".to_string(),
            event: Box::new(AgentEvent::ToolStart {
                tool: "bash".to_string(),
                summary: "ls".to_string(),
            }),
        });
        session.on_agent(AgentEvent::ChildEnded {
            id: "a1".to_string(),
            ok: false,
        });

        let rows = session.children();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].identity, "worker");
        assert_eq!(rows[0].state, ChildState::Failed);
        assert_eq!(rows[1].state, ChildState::Running);
        // The transcript shows the `agent` call and what comes back, nothing from inside.
        assert!(session.entries().list.is_empty());

        let pane = session.child_entries("a1").unwrap();
        let pane = pane.lock().unwrap();
        assert!(matches!(&pane.list[0], Entry::User(t) if t == "count the files"));
        assert!(matches!(&pane.list[1], Entry::Command { summary, .. } if summary == "ls"));
        assert!(session.child_entries("nope").is_none());
    }

    #[test]
    fn a_new_turn_clears_the_finished_panes_and_keeps_the_running_ones() {
        let (session, _rx) = session();
        child(&session, "a1", "count the files");
        child(&session, "b2", "read the docs");
        session.on_agent(AgentEvent::ChildEnded {
            id: "a1".to_string(),
            ok: true,
        });
        assert!(session.child_running());
        session.publish(Event::User("something else".to_string()));
        let rows = session.children();
        assert_eq!(rows.len(), 1, "the child that finished is gone");
        assert_eq!(rows[0].id, "b2", "the one still running is not");

        session.on_agent(AgentEvent::ChildEnded {
            id: "b2".to_string(),
            ok: true,
        });
        assert!(!session.child_running());
        session.publish(Event::User("and again".to_string()));
        assert!(session.children().is_empty());

        // The panel lets them go, the debug export does not.
        let logs = session.child_logs();
        let ids: Vec<&str> = logs.iter().map(|log| log.row.id.as_str()).collect();
        assert_eq!(ids, ["a1", "b2"]);
        assert!(matches!(&logs[1].entries[0], Entry::User(t) if t == "read the docs"));
    }

    #[test]
    fn an_idle_session_with_a_child_still_running_can_be_interrupted() {
        let (session, _rx) = session();
        assert!(!session.interrupt(), "nothing running, nothing to stop");
        child(&session, "a1", "count the files");
        assert!(
            session.interrupt(),
            "the detached child is still work in flight"
        );
        session.on_agent(AgentEvent::ChildEnded {
            id: "a1".to_string(),
            ok: false,
        });
        assert!(!session.interrupt());
    }

    #[test]
    fn a_message_reaches_a_running_child_and_shows_in_its_pane() {
        let (session, _rx) = session();
        child(&session, "a1", "count the files");
        // Nothing is listening until the agent opens the child's mailbox.
        assert!(session.steer("a1", "and the tests".to_string()).is_err());

        let (_mailbox, mut rx) = crate::agent::Mailbox::open(&session.mailboxes(), "a1");
        session.steer("a1", "and the tests".to_string()).unwrap();
        assert_eq!(rx.try_recv().unwrap(), "and the tests");
        let pane = session.child_entries("a1").unwrap();
        let pane = pane.lock().unwrap();
        assert!(matches!(&pane.list[1], Entry::User(t) if t == "and the tests"));

        drop(_mailbox);
        assert!(
            session.steer("a1", "too late".to_string()).is_err(),
            "a child that has ended has no mailbox"
        );
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
        // An eviction has no summary, so it says nothing where one would be.
        let json = serde_json::to_value(Event::Compacted {
            notice: "compacted history".to_string(),
            summary: None,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"type": "compacted", "data": {"notice": "compacted history"}})
        );
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
    fn a_queued_prompt_joins_the_transcript_where_the_history_puts_it() {
        // It used to be shown the moment it was typed, which put it before the rest of
        // the running turn's answer, while the model saw it after. The transcript read
        // as a conversation that never happened in that order.
        let (session, mut rx_user) = session();
        session.submit("a".to_string()).unwrap();
        assert_eq!(rx_user.try_recv().unwrap(), "a");
        session.on_agent(AgentEvent::Text("half an ".to_string()));
        session.submit("b".to_string()).unwrap();
        session.on_agent(AgentEvent::Text("answer".to_string()));
        {
            let entries = session.entries();
            let texts: Vec<_> = entries.list.iter().map(Entry::text).collect();
            assert_eq!(texts, ["a", "half an answer"]);
        }

        session.on_agent(AgentEvent::TurnEnd);
        assert_eq!(rx_user.try_recv().unwrap(), "b");
        let entries = session.entries();
        let texts: Vec<_> = entries.list.iter().map(Entry::text).collect();
        assert_eq!(texts, ["a", "half an answer", "b"]);
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
    fn an_interrupt_keeps_the_queue() {
        // Interrupting used to drop what was waiting, so a correction typed while the
        // turn went wrong was thrown away by the very keypress that stopped it.
        let (session, mut rx_user) = session();
        session.submit("a".to_string()).unwrap();
        assert_eq!(rx_user.try_recv().unwrap(), "a");
        session.submit("b".to_string()).unwrap();
        session.submit("c".to_string()).unwrap();
        assert!(session.interrupt());
        assert_eq!(session.state().queued, ["b", "c"]);
        // Nothing is sent until the cancelled turn ends; then the queue drains as usual.
        assert!(rx_user.try_recv().is_err());
        session.on_agent(AgentEvent::TurnEnd);
        assert_eq!(rx_user.try_recv().unwrap(), "b");
        assert!(session.state().working);
        // The interrupt cleared the cancel flag on the way out, so the new turn runs.
        assert!(!session.cancel.stopped());
        session.on_agent(AgentEvent::TurnEnd);
        assert_eq!(rx_user.try_recv().unwrap(), "c");
        session.on_agent(AgentEvent::TurnEnd);
        assert!(!session.state().working);
        let entries = session.entries();
        let texts: Vec<_> = entries.list.iter().map(Entry::text).collect();
        assert_eq!(texts, ["a", "interrupted", "b", "c"]);
    }

    #[test]
    fn one_interrupt_stops_one_turn() {
        // Stopping the queue as well takes either an interrupt each or `/queue clear`.
        let (session, mut rx_user) = session();
        session.submit("a".to_string()).unwrap();
        session.submit("b".to_string()).unwrap();
        assert_eq!(rx_user.try_recv().unwrap(), "a");

        assert!(session.interrupt());
        session.on_agent(AgentEvent::TurnEnd);
        assert_eq!(rx_user.try_recv().unwrap(), "b");
        assert!(session.state().working);

        assert!(session.interrupt());
        session.on_agent(AgentEvent::TurnEnd);
        assert!(!session.state().working);
        assert!(rx_user.try_recv().is_err());
    }

    #[test]
    fn clearing_the_queue_after_an_interrupt_stops_everything() {
        let (session, mut rx_user) = session();
        session.submit("a".to_string()).unwrap();
        session.submit("b".to_string()).unwrap();
        session.submit("c".to_string()).unwrap();
        assert_eq!(rx_user.try_recv().unwrap(), "a");
        assert!(session.interrupt());
        assert_eq!(session.clear_queue(), 2);
        session.on_agent(AgentEvent::TurnEnd);
        assert!(!session.state().working);
        assert!(rx_user.try_recv().is_err());
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
        session.cancel.stop();
        session.submit("hi".to_string()).unwrap();
        assert!(!session.cancel.stopped());
        assert!(session.interrupt());
        assert!(session.cancel.stopped());
    }

    /// Parallel workflow steps each park a call. The second must not displace the first
    /// on screen, and neither may come back rejected without the user having decided it.
    #[test]
    fn two_calls_waiting_at_once_are_each_put_to_the_user() {
        let (session, _rx) = session();
        session.submit("go".to_string()).unwrap();
        let mut events = session.subscribe();
        let mut first = approval(&session);
        let mut second = approval(&session);

        // The second is parked, not shown and not dropped.
        assert_eq!(session.state().pending.map(|a| a.id), Some(1));
        assert!(matches!(
            events.try_recv().unwrap(),
            Event::Approval { id: 1, .. }
        ));
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(first.try_recv(), Err(oneshot::error::TryRecvError::Empty));

        assert_eq!(session.answer(Answer::Accept(None), Some(1)), Some(1));
        assert_eq!(first.try_recv(), Ok(Answer::Accept(None)));
        // Answering the first brings the second up in its place.
        assert_eq!(session.state().pending.map(|a| a.id), Some(2));
        assert!(matches!(events.try_recv().unwrap(), Event::Resolved { .. }));
        assert!(matches!(
            events.try_recv().unwrap(),
            Event::Approval { id: 2, .. }
        ));

        assert_eq!(session.answer(Answer::Reject, Some(2)), Some(2));
        assert_eq!(second.try_recv(), Ok(Answer::Reject));
        assert!(session.state().pending.is_none());
    }

    #[test]
    fn an_interrupt_rejects_every_call_that_was_waiting() {
        let (session, _rx) = session();
        session.submit("go".to_string()).unwrap();
        let mut first = approval(&session);
        let mut second = approval(&session);
        assert!(session.interrupt());
        assert_eq!(first.try_recv(), Ok(Answer::Reject));
        assert_eq!(second.try_recv(), Ok(Answer::Reject));
    }

    #[test]
    fn interrupt_cancels_and_rejects_the_pending_approval() {
        let (session, _rx) = session();
        assert!(!session.interrupt());
        session.submit("a".to_string()).unwrap();
        let mut wait = approval(&session);
        assert!(session.interrupt());
        assert!(session.cancel.stopped());
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
