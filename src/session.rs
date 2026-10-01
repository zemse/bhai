//! The session hub: fans agent events out to every consumer (the TUI, the debug server)
//! and holds the pending approval so exactly one of them answers it.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::agent::{AgentEvent, Cancel, Control, Mailboxes, Rejecter};
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
/// Events kept for a `/events` client that reconnects to replay.
pub const EVENT_LOG: usize = 4096;

/// The recent events, each with its `seq`: 1 for the first of the run, then one more for
/// each, so a gap in what a reader holds is a gap in what it saw.
#[derive(Default)]
struct Log {
    last: u64,
    ring: VecDeque<(u64, Event)>,
}

/// What [`Session::since`] has for a reader at some `seq`.
#[derive(Debug, Default, PartialEq)]
pub struct Since {
    /// Events after it that the log has already let go of.
    pub missed: u64,
    /// The rest, oldest first.
    pub events: Vec<(u64, Event)>,
}

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
    /// The front of the queue joined the running turn's history, between its steps.
    Steered(String),
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
    /// A call that did not run: who stopped it, and why when they said.
    ToolRejected {
        tool: String,
        summary: String,
        by: Rejecter,
        reason: String,
    },
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
    /// `/compact` started, from the prompt box or from the front of the queue; the
    /// string says what it keeps.
    Compacting(String),
    /// The judge is deciding that call, or `None` once it has.
    Judging(Option<String>),
    /// History was compacted; earlier history indexes no longer hold. `summary` is what
    /// the earlier turns were folded into, when they were folded rather than evicted.
    Compacted {
        notice: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// Tokens gone from what the last call read, as estimated.
        freed: u64,
    },
    /// History was dropped, so the transcript that showed it goes too.
    Cleared,
    /// A compacted copy of the history is ready for `/compact-then`, and a call on it
    /// would read this many tokens; `None` once it no longer stands for the history.
    Fork(Option<u64>),
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
    /// The turn about to end ran this many seconds, and ended at this local time;
    /// `verb` is the word the transcript says it with, picked afresh for each turn.
    Done {
        seconds: u64,
        at: String,
        verb: String,
    },
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
    /// The `seq` of the last event published before this was read. Every event this
    /// state does not reflect comes after it, so `/events` from here misses nothing.
    pub seq: u64,
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

/// What waits behind the running turn.
#[derive(Debug, Clone, PartialEq)]
enum Waiting {
    Prompt(Prompt),
    /// `/compact`, with what the summary should keep. It runs as a turn of its own, so
    /// it waits for the running one to end, and what was typed after it waits too.
    Compact(Option<String>),
}

impl Waiting {
    /// As it was typed.
    fn shown(&self) -> String {
        match self {
            Waiting::Prompt(prompt) => prompt.shown.clone(),
            Waiting::Compact(None) => "/compact".to_string(),
            Waiting::Compact(Some(asked)) => format!("/compact {asked}"),
        }
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
    /// An effort change would re-read the whole conversation uncached; the cache goes
    /// cold on its own after this long.
    CacheWarm(Duration),
    /// The Codex API takes no such `reasoning.effort`.
    UnknownEffort(String),
    /// `/compact-then` with no compacted copy to continue from.
    NoFork,
}

impl fmt::Display for SubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SubmitError::Busy => f.write_str("a turn is already running"),
            SubmitError::Closed => f.write_str("the agent is not accepting messages"),
            SubmitError::CacheWarm(left) => write!(
                f,
                "this model caches per effort, so a change now re-reads the whole conversation uncached. It goes through once the cache expires in {}m{:02}s, or after /clear; a gpt-6 model changes effort without the miss",
                left.as_secs() / 60,
                left.as_secs() % 60
            ),
            SubmitError::NoFork => f.write_str(
                "there is no compacted copy yet: one is made while the conversation is idle, just before its cache lapses",
            ),
            SubmitError::UnknownEffort(effort) => write!(
                f,
                "the API takes no effort `{effort}`; it takes {}",
                crate::client::EFFORTS.join(", ")
            ),
        }
    }
}

#[derive(Default)]
struct Inner {
    working: bool,
    /// What was typed while a turn ran, oldest first.
    queue: VecDeque<Waiting>,
    total: Usage,
    calls: u64,
    children: Usage,
    last_usage: Option<Usage>,
    /// When the conversation's last call was sent, which is when the backend last wrote
    /// or reused its cached prefix, while that may be warm.
    last_call: Option<Instant>,
    /// When the call in flight was sent.
    sending: Option<Instant>,
    /// When the running turn started.
    started: Option<Instant>,
    /// What the running turn is said to be doing, and to have done once it ends.
    verb: Option<(&'static str, &'static str)>,
    last_cache_break: Option<CacheBreak>,
    rate_limits: Option<RateLimits>,
    /// The tokens a call on the compacted copy would read, while there is one.
    fork: Option<u64>,
    next_id: u64,
    /// Calls waiting on the user, oldest first. Parallel workflow steps each park one,
    /// so there can be several; only the front is on screen.
    pending: VecDeque<(Approval, oneshot::Sender<Answer>)>,
}

impl Inner {
    /// A turn runs from now.
    fn start(&mut self) {
        self.working = true;
        self.started = Some(Instant::now());
        self.verb = Some(crate::entries::verb());
    }
}

pub struct Session {
    /// The model and the reasoning effort it runs at; `/model` changes both.
    model: Mutex<(String, String)>,
    identity: String,
    events: broadcast::Sender<Event>,
    log: Mutex<Log>,
    /// The newest `seq`, for `/events` readers to wait on.
    latest: watch::Sender<u64>,
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
            log: Mutex::default(),
            latest: watch::Sender::new(0),
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

    /// What the running turn is said to be doing, as in `chabārau`.
    pub fn verb(&self) -> Option<&'static str> {
        let inner = self.lock();
        inner
            .verb
            .filter(|_| inner.working)
            .map(|(working, _)| working)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// The `seq` of the newest event, 0 before the first.
    pub fn seq(&self) -> u64 {
        *self.latest.borrow()
    }

    /// Wakes when an event is published.
    pub fn watch_seq(&self) -> watch::Receiver<u64> {
        self.latest.subscribe()
    }

    /// The events published after `after`, as far back as the log still holds them.
    pub fn since(&self, after: u64) -> Since {
        let log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        let Some(&(first, _)) = log.ring.front() else {
            return Since::default();
        };
        let from = after.saturating_add(1);
        let skip = usize::try_from(from.saturating_sub(first)).unwrap_or(usize::MAX);
        Since {
            missed: first.saturating_sub(from),
            events: log.ring.iter().skip(skip).cloned().collect(),
        }
    }

    pub fn state(&self) -> State {
        // Read before the rest, so an event racing the snapshot is replayed, not lost.
        let seq = self.seq();
        let inner = self.lock();
        let (model, effort) = self.model();
        State {
            model,
            effort,
            identity: self.identity.clone(),
            mode: self.policy.mode(),
            working: inner.working,
            queued: inner.queue.iter().map(Waiting::shown).collect(),
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
            seq,
        }
    }

    /// Start a turn with `prompt`, or queue it when one is already running.
    pub fn submit(&self, prompt: impl Into<Prompt>) -> Result<Submitted, SubmitError> {
        let prompt = prompt.into();
        let mut inner = self.lock();
        if inner.working {
            return Ok(self.enqueue(&mut inner, Waiting::Prompt(prompt)));
        }
        // Cleared here, not when the agent picks the message up, so an interrupt that
        // lands in between still stops the turn.
        self.cancel.clear();
        self.tx_user
            .try_send(prompt.text)
            .map_err(|_| SubmitError::Closed)?;
        inner.start();
        self.publish(Event::User(prompt.shown));
        Ok(Submitted::Started)
    }

    /// Start a turn on `prompt` from the compacted copy of the history, for
    /// `/compact-then`. The full history is dropped for the copy, so this is refused
    /// rather than queued while a turn runs on it.
    pub fn submit_forked(&self, prompt: impl Into<Prompt>) -> Result<(), SubmitError> {
        let prompt = prompt.into();
        let mut inner = self.lock();
        if inner.working {
            return Err(SubmitError::Busy);
        }
        if inner.fork.is_none() {
            return Err(SubmitError::NoFork);
        }
        self.cancel.clear();
        self.tx_control
            .try_send(Control::Forked(prompt.text))
            .map_err(|_| SubmitError::Closed)?;
        inner.start();
        self.publish(Event::User(prompt.shown));
        Ok(())
    }

    /// The tokens a call on the compacted copy would read, while there is one.
    pub fn fork(&self) -> Option<u64> {
        self.lock().fork
    }

    /// Put `waiting` at the back of the queue, with the state locked.
    fn enqueue(&self, inner: &mut Inner, waiting: Waiting) -> Submitted {
        let text = waiting.shown();
        inner.queue.push_back(waiting);
        let position = inner.queue.len();
        self.publish(Event::Queued { position, text });
        Submitted::Queued { position }
    }

    /// The prompts waiting for the running turn, for `/queue`, as they were typed.
    pub fn queued(&self) -> Vec<String> {
        self.lock().queue.iter().map(Waiting::shown).collect()
    }

    /// Drop every queued prompt, for `/queue clear`. Returns how many there were.
    pub fn clear_queue(&self) -> usize {
        let dropped = self.lock().queue.drain(..).count();
        if dropped > 0 {
            self.publish(Event::Info(format!("dropped {dropped} queued prompt(s)")));
        }
        dropped
    }

    /// Every prompt waiting ahead of the first `/compact`, as what is sent and what is
    /// shown, for the agent to read before its next model call. Nothing is handed over
    /// once the turn is interrupted, so the queue outlives the interrupt.
    pub fn take_queued(&self) -> Vec<(String, String)> {
        let mut inner = self.lock();
        if self.cancel.stopped() {
            return Vec::new();
        }
        let mut taken = Vec::new();
        while let Some(Waiting::Prompt(_)) = inner.queue.front() {
            if let Some(Waiting::Prompt(p)) = inner.queue.pop_front() {
                taken.push((p.text, p.shown));
            }
        }
        taken
    }

    /// Take everything waiting back out of the queue, as typed, to be edited.
    pub fn unqueue(&self) -> Vec<String> {
        self.lock().queue.drain(..).map(|w| w.shown()).collect()
    }

    /// Hand the next queued prompt to the agent, with the state locked. A prompt the
    /// agent will not take keeps its place rather than being lost.
    fn start_queued(&self, inner: &mut Inner) {
        let prompt = match inner.queue.pop_front() {
            None => return,
            Some(Waiting::Compact(asked)) => {
                if let Err(asked) = self.start_compact(inner, asked) {
                    inner.queue.push_front(Waiting::Compact(asked));
                    inner.working = false;
                }
                return;
            }
            Some(Waiting::Prompt(prompt)) => prompt,
        };
        self.cancel.clear();
        if let Err(e) = self.tx_user.try_send(prompt.text.clone()) {
            inner.queue.push_front(Waiting::Prompt(Prompt::shown_as(
                e.into_inner(),
                prompt.shown,
            )));
            inner.working = false;
            return;
        }
        inner.start();
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
        if crate::client::Provider::of(&model) == crate::client::Provider::Codex
            && !crate::client::EFFORTS.contains(&effort.as_str())
        {
            return Err(SubmitError::UnknownEffort(effort));
        }
        if let Some(left) = self.effort_miss(&model, &effort, inner.last_call) {
            return Err(SubmitError::CacheWarm(left));
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

    /// Tokens the next message would re-read uncached, once the last call is older than
    /// the cache lasts: what a `/clear` saves, if the conversation is done with.
    pub fn cold_tokens(&self) -> Option<u64> {
        let inner = self.lock();
        let expired = inner
            .last_call
            .is_some_and(|at| at.elapsed() >= crate::cache::CACHE_TTL);
        match expired && !inner.working {
            true => inner.last_usage.map(|usage| usage.input).filter(|n| *n > 0),
            false => None,
        }
    }

    /// How long the cached prefix has left while idle, once that is under `CACHE_WARNING`.
    pub fn cache_left(&self) -> Option<Duration> {
        let inner = self.lock();
        if inner.working || inner.last_usage.is_none_or(|usage| usage.input == 0) {
            return None;
        }
        crate::cache::CACHE_TTL
            .checked_sub(inner.last_call?.elapsed())
            .filter(|left| !left.is_zero() && *left <= crate::cache::CACHE_WARNING)
    }

    /// How long the cached prefix may stay warm, when putting the current model on
    /// `effort` would re-read it all: a Codex model outside the GPT-6 family renders the
    /// effort ahead of the conversation. `None` when the change costs nothing.
    fn effort_miss(
        &self,
        model: &str,
        effort: &str,
        last_call: Option<Instant>,
    ) -> Option<Duration> {
        let (current, now) = self.model();
        if model != current
            || effort == now
            || crate::client::Provider::of(model) != crate::client::Provider::Codex
            || crate::client::takes_effort_updates(model)
        {
            return None;
        }
        crate::cache::CACHE_TTL
            .checked_sub(last_call?.elapsed())
            .filter(|left| !left.is_zero())
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

    /// Summarise the history as a turn of its own, now or once the running turn and
    /// everything queued before it are done. `asked` is what the user wants the summary
    /// to keep, from `/compact <prompt>`.
    pub fn compact(&self, asked: Option<String>) -> Result<Submitted, SubmitError> {
        let mut inner = self.lock();
        if inner.working {
            return Ok(self.enqueue(&mut inner, Waiting::Compact(asked)));
        }
        self.start_compact(&mut inner, asked)
            .map_err(|_| SubmitError::Closed)?;
        Ok(Submitted::Started)
    }

    /// Hand `/compact` to the agent, with the state locked; `asked` comes back if the
    /// agent will not take it.
    fn start_compact(
        &self,
        inner: &mut Inner,
        asked: Option<String>,
    ) -> Result<(), Option<String>> {
        self.cancel.clear();
        let notice = match &asked {
            Some(asked) => format!("compacting history, keeping {asked}"),
            None => "compacting history".to_string(),
        };
        self.tx_control
            .try_send(Control::Compact(asked))
            .map_err(|e| match e.into_inner() {
                Control::Compact(asked) => asked,
                _ => None,
            })?;
        inner.start();
        self.publish(Event::Compacting(notice));
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
        inner.start();
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
        inner.start();
        Ok(())
    }

    /// Ask the agent for a token breakdown of its context; `None` if it has gone away.
    pub async fn context(&self) -> Option<Profile> {
        let (reply, wait) = oneshot::channel();
        self.tx_control.send(Control::Context(reply)).await.ok()?;
        wait.await.ok()
    }

    pub(crate) fn on_agent(&self, event: AgentEvent) {
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
            AgentEvent::ToolRejected {
                tool,
                summary,
                by,
                reason,
            } => Event::ToolRejected {
                tool,
                summary,
                by,
                reason,
            },
            AgentEvent::Usage(usage) => {
                add(&mut inner.total, usage);
                inner.last_usage = Some(usage);
                inner.last_call = Some(inner.sending.take().unwrap_or_else(Instant::now));
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
            AgentEvent::Steered(shown) => Event::Steered(shown),
            AgentEvent::CacheHit(hit) => Event::CacheHit(hit),
            AgentEvent::CacheStalled(misses) => Event::CacheStalled(misses),
            AgentEvent::RateLimits(limits) => {
                let limits = limits.over(inner.rate_limits);
                inner.rate_limits = Some(limits);
                Event::RateLimits(limits)
            }
            AgentEvent::Sending(tokens) => Event::Sending(tokens),
            AgentEvent::Streaming(on) => {
                if on {
                    inner.sending = Some(Instant::now());
                }
                Event::Streaming(on)
            }
            AgentEvent::Info(s) => Event::Info(s),
            AgentEvent::Judging(what) => Event::Judging(what),
            AgentEvent::Titled(name) => Event::Titled(name),
            // Either way the next call reads a history the cache has never seen.
            AgentEvent::Compacted {
                notice,
                summary,
                freed,
            } => {
                inner.last_call = None;
                if let Some(usage) = &mut inner.last_usage {
                    usage.shrink(freed);
                }
                Event::Compacted {
                    notice,
                    summary,
                    freed,
                }
            }
            AgentEvent::Cleared => {
                inner.last_call = None;
                inner.last_usage = None;
                Event::Cleared
            }
            AgentEvent::Fork(tokens) => {
                inner.fork = tokens;
                Event::Fork(tokens)
            }
            AgentEvent::Effort(effort) => {
                let mut current = self.model.lock().unwrap_or_else(|e| e.into_inner());
                current.1 = effort.clone();
                Event::Model {
                    model: current.0.clone(),
                    effort,
                }
            }
            AgentEvent::Error(s) => Event::Error(s),
            AgentEvent::TurnFailed(s) => Event::TurnFailed(s),
            // The agent is working again without anything having been typed, so the
            // session says so: what the user sends now queues behind it, as it would
            // behind any other turn.
            AgentEvent::Resumed(what) => {
                inner.start();
                Event::Resumed(what)
            }
            AgentEvent::TurnEnd => {
                if let Some(started) = inner.started.take() {
                    let (_, done) = inner.verb.unwrap_or_else(crate::entries::verb);
                    self.publish(Event::Done {
                        seconds: started.elapsed().as_secs(),
                        at: chrono::Local::now().format("%-I:%M %p").to_string(),
                        verb: done.to_string(),
                    });
                }
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
        self.record(&event);
        // No subscribers is fine; the event is simply dropped.
        let _ = self.events.send(event);
    }

    fn record(&self, event: &Event) {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        log.last += 1;
        let seq = log.last;
        if log.ring.len() == EVENT_LOG {
            log.ring.pop_front();
        }
        log.ring.push_back((seq, event.clone()));
        // Under the log's lock, so the value only ever goes up.
        self.latest.send_replace(seq);
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
        AgentEvent::ToolRejected {
            tool,
            summary,
            by,
            reason,
        } => Event::ToolRejected {
            tool,
            summary,
            by,
            reason,
        },
        AgentEvent::Info(s) => Event::Info(s),
        AgentEvent::Error(s) => Event::Error(s),
        _ => return None,
    })
}

/// The prompt of a message that asks to continue from the compacted copy rather than
/// the full history: whatever follows `/compact-then`.
pub fn compact_then(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix("/compact-then")?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim())
}

fn add(total: &mut Usage, usage: Usage) {
    total.input += usage.input;
    total.cached += usage.cached;
    total.output += usage.output;
    total.reasoning += usage.reasoning;
}

/// Watch the agent's task and say so if it stopped on a panic. Nothing else reports it:
/// the loop owns the sender, so a panic ends the turn with no error and no `TurnEnd`, and
/// the session sits on a turn that will never finish. The panic itself is printed by the
/// process hook; this is what lets the session say it is over.
pub async fn watch(task: tokio::task::JoinHandle<()>, tx: mpsc::UnboundedSender<AgentEvent>) {
    let Err(e) = task.await else {
        return;
    };
    if !e.is_panic() {
        return;
    }
    let _ = tx.send(AgentEvent::TurnFailed(
        "the agent loop stopped on a panic; this session cannot take another message".to_string(),
    ));
    let _ = tx.send(AgentEvent::TurnEnd);
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

    /// A panic in the loop ends its task with no error and no `TurnEnd` of its own, so
    /// without this the session stays working and the spinner never stops.
    #[tokio::test]
    async fn a_panicked_loop_ends_the_turn_it_left_running() {
        let (session, _rx_user) = session();
        let (tx_agent, rx_agent) = mpsc::unbounded_channel();
        let pumping = tokio::spawn(pump(Arc::clone(&session), rx_agent));
        let task = tokio::spawn({
            let tx = tx_agent.clone();
            async move {
                let _ = tx.send(AgentEvent::Streaming(true));
                panic!("a tool went wrong");
            }
        });
        watch(task, tx_agent).await;
        // `watch` held the last sender, so the pump ends once it has the events.
        pumping.await.unwrap();
        let state = session.state();
        assert!(!state.working, "{state:?}");
        let said = session
            .entries()
            .list
            .iter()
            .any(|e| matches!(e, Entry::Failed(text) if text.contains("panic")));
        assert!(said, "{:?}", session.entries().list);
    }

    #[test]
    fn the_log_numbers_events_and_keeps_the_newest() {
        let (session, _rx) = session();
        let text = |i: u64| Event::Text(i.to_string());
        assert_eq!(session.since(0), Since::default());
        assert_eq!(session.state().seq, 0);
        let total = EVENT_LOG as u64 + 2;
        for i in 1..=total {
            session.publish(text(i));
        }
        assert_eq!(session.seq(), total);
        assert_eq!(session.state().seq, total);
        let tail = session.since(total - 1);
        assert_eq!((tail.missed, tail.events), (0, vec![(total, text(total))]));
        assert_eq!(session.since(total), Since::default());
        // The first two have rolled out of the ring.
        let all = session.since(0);
        assert_eq!(all.missed, 2);
        assert_eq!(all.events.len(), EVENT_LOG);
        assert_eq!(all.events[0], (3, text(3)));
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
            freed: 7,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"type": "compacted", "data": {"notice": "compacted history", "freed": 7}})
        );
    }

    #[test]
    fn compact_sent_while_a_turn_runs_waits_for_it_to_end() {
        let (tx_user, mut rx_user) = mpsc::channel(4);
        let (tx_control, mut rx_control) = mpsc::channel(4);
        let session = Session::new(
            "m".to_string(),
            "medium".to_string(),
            "general".to_string(),
            tx_user,
            tx_control,
            Arc::new(Cancel::default()),
            Arc::new(Policy::default()),
            None,
        );
        session.submit("a".to_string()).unwrap();
        assert_eq!(rx_user.try_recv().unwrap(), "a");
        session.submit("b".to_string()).unwrap();
        assert_eq!(
            session.compact(Some("the plan".to_string())),
            Ok(Submitted::Queued { position: 2 })
        );
        session.submit("c".to_string()).unwrap();
        assert_eq!(session.queued(), ["b", "/compact the plan", "c"]);

        // The running turn takes what was typed before the compact, not what came after.
        assert_eq!(session.take_queued(), [("b".to_string(), "b".to_string())]);
        assert!(session.take_queued().is_empty());
        assert!(rx_control.try_recv().is_err());

        session.on_agent(AgentEvent::TurnEnd);
        assert!(matches!(
            rx_control.try_recv(),
            Ok(Control::Compact(Some(asked))) if asked == "the plan"
        ));
        assert!(session.state().working);
        assert_eq!(session.queued(), ["c"]);
        session.on_agent(AgentEvent::TurnEnd);
        assert_eq!(rx_user.try_recv().unwrap(), "c");
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

        let seen: Vec<Event> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|e| !matches!(e, Event::Done { .. }))
            .collect();
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
        let kinds: Vec<_> = entries
            .list
            .iter()
            .map(Entry::kind)
            .filter(|k| *k != "done")
            .collect();
        assert_eq!(kinds, ["user", "user", "user"]);
        let texts: Vec<_> = entries
            .list
            .iter()
            .filter(|e| e.kind() != "done")
            .map(Entry::text)
            .collect();
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
            let texts: Vec<_> = entries
                .list
                .iter()
                .filter(|e| e.kind() != "done")
                .map(Entry::text)
                .collect();
            assert_eq!(texts, ["a", "half an answer"]);
        }

        session.on_agent(AgentEvent::TurnEnd);
        assert_eq!(rx_user.try_recv().unwrap(), "b");
        let entries = session.entries();
        let texts: Vec<_> = entries
            .list
            .iter()
            .filter(|e| e.kind() != "done")
            .map(Entry::text)
            .collect();
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
        let seen: Vec<Event> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|e| !matches!(e, Event::Done { .. }))
            .collect();
        assert!(seen.contains(&Event::User("/pdf".to_string())), "{seen:?}");
        let entries = session.entries();
        let texts: Vec<_> = entries
            .list
            .iter()
            .filter(|e| e.kind() != "done")
            .map(Entry::text)
            .collect();
        assert_eq!(texts, ["/pdf", "/pdf"]);
    }

    #[test]
    fn the_agent_takes_the_whole_queue_unless_interrupted() {
        let (session, _rx_user) = session();
        session.submit("a".to_string()).unwrap();
        session.submit("b".to_string()).unwrap();
        session
            .submit(Prompt::shown_as(
                "Use the `pdf` skill.".to_string(),
                "/pdf".to_string(),
            ))
            .unwrap();
        // An interrupted turn reads nothing more, so the queue waits for the next one.
        session.interrupt();
        assert!(session.take_queued().is_empty());
        assert_eq!(session.queued(), ["b", "/pdf"]);

        session.cancel.clear();
        assert_eq!(
            session.take_queued(),
            [
                ("b".to_string(), "b".to_string()),
                ("Use the `pdf` skill.".to_string(), "/pdf".to_string()),
            ]
        );
        assert!(session.queued().is_empty());
    }

    #[test]
    fn unqueue_hands_back_what_was_typed() {
        let (session, _rx_user) = session();
        session.submit("a".to_string()).unwrap();
        session.submit("b".to_string()).unwrap();
        session
            .submit(Prompt::shown_as(
                "Use the `pdf` skill.".to_string(),
                "/pdf".to_string(),
            ))
            .unwrap();
        assert_eq!(session.unqueue(), ["b", "/pdf"]);
        assert!(session.queued().is_empty());
        // Nothing is left to start once the turn ends.
        session.on_agent(AgentEvent::TurnEnd);
        assert!(!session.state().working);
    }

    /// What is typed while the agent answers goes in before its next call, each as a
    /// message of its own, rather than one turn apiece once the turn has ended.
    #[tokio::test]
    async fn what_is_queued_joins_the_running_turn() {
        use crate::agent::fake::{Fake, say};

        let (tx_user, rx_user) = mpsc::channel(1);
        let (tx_control, rx_control) = mpsc::channel(1);
        let (tx_agent, rx_agent) = mpsc::unbounded_channel();
        let cancel = Arc::new(Cancel::default());
        let policy = Arc::new(Policy::default());
        let session = Session::new(
            "fake".to_string(),
            "medium".to_string(),
            "general".to_string(),
            tx_user,
            tx_control,
            Arc::clone(&cancel),
            Arc::clone(&policy),
            None,
        );
        tokio::spawn(pump(Arc::clone(&session), rx_agent));
        let typing = Arc::clone(&session);
        let fake = Fake::new(vec![vec![say("one")], vec![say("two")]]).during(move |call| {
            if call == 0 {
                typing.submit("b".to_string()).unwrap();
                let skill = "Use the `pdf` skill.".to_string();
                typing
                    .submit(Prompt::shown_as(skill, "/pdf".to_string()))
                    .unwrap();
            }
        });
        let bodies = Arc::clone(&fake.bodies);
        let inbox: crate::agent::Inbox = {
            let session = Arc::clone(&session);
            Arc::new(move || session.take_queued())
        };
        tokio::spawn(crate::agent::run_with(
            Arc::new(fake),
            "sess".to_string(),
            crate::prompt::system_prompt(&[], Vec::new()),
            policy,
            None,
            None,
            rx_user,
            Some(inbox),
            rx_control,
            tx_agent,
            cancel,
            None,
            None,
            None,
            crate::compact::Limits::default(),
        ));

        let mut events = session.subscribe();
        session.submit("a".to_string()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while session.state().working {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the turn ends");

        // One turn, two calls: the second reads both messages, apart.
        let bodies = bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2);
        let typed: Vec<_> = bodies[1].1["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["role"] == "user")
            .map(|item| item["content"][0]["text"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(typed, ["a", "b", "Use the `pdf` skill."]);
        // They show after the call they were typed during, as typed, and as messages
        // of the same turn rather than turns of their own.
        let seen: Vec<Event> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|e| !matches!(e, Event::Done { .. }))
            .collect();
        let at = |want: &Event| seen.iter().position(|e| e == want).unwrap();
        let first_call = seen
            .iter()
            .position(|e| matches!(e, Event::Call(_)))
            .unwrap();
        assert!(
            first_call < at(&Event::Steered("b".to_string())),
            "{seen:?}"
        );
        assert!(at(&Event::Steered("b".to_string())) < at(&Event::Steered("/pdf".to_string())));
        let turns = seen.iter().filter(|e| matches!(e, Event::User(_))).count();
        assert_eq!(turns, 1, "{seen:?}");
        let entries = session.entries();
        let texts: Vec<_> = entries
            .list
            .iter()
            .filter(|e| e.kind() != "done")
            .map(Entry::text)
            .collect();
        assert_eq!(texts, ["a", "b", "/pdf"]);
        assert!(session.queued().is_empty());
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
        let texts: Vec<_> = entries
            .list
            .iter()
            .filter(|e| e.kind() != "done")
            .map(Entry::text)
            .collect();
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

    #[test]
    fn compaction_takes_what_it_freed_off_the_last_call() {
        let (session, _rx) = session();
        let usage = |input, cached| Usage {
            input,
            cached,
            output: 5,
            reasoning: 0,
        };
        session.on_agent(AgentEvent::Usage(usage(900, 800)));
        session.on_agent(AgentEvent::Compacted {
            notice: "compacted history".to_string(),
            summary: None,
            freed: 600,
        });
        // The totals are what was spent and stay; only the fill the bar reads goes down.
        let state = session.state();
        assert_eq!(state.input_tokens, 900);
        assert_eq!(state.last_usage, Some(usage(300, 300)));
        session.on_agent(AgentEvent::Cleared);
        assert_eq!(session.state().last_usage, None);
    }

    /// A session on `model`, with the control end kept open so a switch goes through.
    fn on(model: &str) -> (Arc<Session>, mpsc::Receiver<Control>) {
        let (tx_user, _) = mpsc::channel(1);
        let (tx_control, rx_control) = mpsc::channel(4);
        let session = Session::new(
            model.to_string(),
            "medium".to_string(),
            "general".to_string(),
            tx_user,
            tx_control,
            Arc::new(Cancel::default()),
            Arc::new(Policy::default()),
            None,
        );
        (session, rx_control)
    }

    fn called(session: &Session, input: u64) {
        session.on_agent(AgentEvent::Usage(Usage {
            input,
            ..Usage::default()
        }));
    }

    #[test]
    fn a_turn_says_how_long_it_ran_as_it_ends() {
        let (session, _control) = on("gpt-5.5");
        let mut events = session.subscribe();
        // Nothing was running, so there is nothing to time.
        session.on_agent(AgentEvent::TurnEnd);
        assert_eq!(events.try_recv().unwrap(), Event::TurnEnd);
        assert_eq!(session.verb(), None);
        session.compact(None).unwrap();
        let working = session.verb().expect("a running turn has a verb");
        session.on_agent(AgentEvent::TurnEnd);
        assert_eq!(session.verb(), None);
        let done = std::iter::from_fn(|| events.try_recv().ok())
            .find(|e| matches!(e, Event::Done { .. }))
            .expect("a done event");
        let Event::Done { seconds, at, verb } = done else {
            unreachable!()
        };
        assert_eq!(seconds, 0);
        // What it said it was doing is what it says it did.
        assert!(
            crate::entries::VERBS.contains(&(working, verb.as_str())),
            "{verb}"
        );
        assert!(at.ends_with("AM") || at.ends_with("PM"), "{at}");
    }

    #[test]
    fn compact_then_takes_the_prompt_after_it() {
        assert_eq!(compact_then("/compact-then fix it"), Some("fix it"));
        assert_eq!(compact_then("  /compact-then\nfix it "), Some("fix it"));
        assert_eq!(compact_then("/compact-then"), Some(""));
        assert_eq!(compact_then("/compact-thenx"), None);
        assert_eq!(compact_then("/compact fix it"), None);
        assert_eq!(compact_then("say /compact-then"), None);
    }

    #[test]
    fn compact_then_runs_on_the_copy_only_while_there_is_one() {
        let (session, mut control) = on("gpt-5.5");
        assert_eq!(
            session.submit_forked(Prompt::from("go on".to_string())),
            Err(SubmitError::NoFork)
        );
        session.on_agent(AgentEvent::Fork(Some(9_000)));
        assert_eq!(session.fork(), Some(9_000));
        session
            .submit_forked(Prompt::from("go on".to_string()))
            .unwrap();
        assert!(matches!(control.try_recv(), Ok(Control::Forked(m)) if m == "go on"));
        assert_eq!(
            session.submit_forked(Prompt::from("again".to_string())),
            Err(SubmitError::Busy)
        );
        session.on_agent(AgentEvent::Fork(None));
        assert_eq!(session.fork(), None);
    }

    #[test]
    fn an_effort_change_waits_for_a_warm_cache_unless_the_model_takes_updates() {
        let (session, _control) = on("gpt-5.6-sol");
        // Nothing sent yet, so nothing is cached to lose.
        assert_eq!(
            session.set_model("gpt-5.6-sol".into(), "high".into(), None),
            Ok(())
        );
        called(&session, 90_000);
        let refused = session.set_model("gpt-5.6-sol".into(), "low".into(), None);
        assert!(
            matches!(refused, Err(SubmitError::CacheWarm(_))),
            "{refused:?}"
        );
        assert_eq!(session.model().1, "high");
        // Another model is a switch the user chose, not an effort change.
        assert_eq!(
            session.set_model("gpt-5.5".into(), "low".into(), None),
            Ok(())
        );
        called(&session, 90_000);
        session.on_agent(AgentEvent::Cleared);
        assert_eq!(
            session.set_model("gpt-5.5".into(), "high".into(), None),
            Ok(())
        );

        let (session, _control) = on("gpt-6-sol");
        called(&session, 90_000);
        assert_eq!(
            session.set_model("gpt-6-sol".into(), "xhigh".into(), None),
            Ok(())
        );
    }

    #[test]
    fn an_effort_the_api_does_not_take_is_refused_and_a_refused_update_is_undone() {
        let (session, _control) = on("gpt-6-astra");
        let refused = session.set_model("gpt-6-astra".into(), "ultra".into(), None);
        assert_eq!(
            refused,
            Err(SubmitError::UnknownEffort("ultra".to_string()))
        );
        // Ollama sends no effort at all, so there is nothing to check.
        assert_eq!(
            session.set_model("ollama:gemma4:e4b".into(), "ultra".into(), None),
            Ok(())
        );

        let (session, _control) = on("gpt-6-astra");
        session
            .set_model("gpt-6-astra".into(), "minimal".into(), None)
            .unwrap();
        let mut events = session.subscribe();
        session.on_agent(AgentEvent::Effort("medium".to_string()));
        assert_eq!(session.model().1, "medium");
        assert!(matches!(
            events.try_recv(),
            Ok(Event::Model { effort, .. }) if effort == "medium"
        ));
    }

    #[test]
    fn an_expired_cache_says_what_a_clear_saves() {
        let (session, _control) = on("gpt-5.6-sol");
        assert_eq!(session.cold_tokens(), None);
        called(&session, 86_000);
        assert_eq!(session.cold_tokens(), None, "still warm");
        session.lock().last_call = Some(Instant::now() - crate::cache::CACHE_TTL);
        assert_eq!(session.cold_tokens(), Some(86_000));
        // The same age no longer holds the effort back either.
        assert_eq!(
            session.set_model("gpt-5.6-sol".into(), "low".into(), None),
            Ok(())
        );
        session.on_agent(AgentEvent::Cleared);
        assert_eq!(session.cold_tokens(), None);
    }

    #[test]
    fn the_cache_counts_down_its_last_minutes() {
        let (session, _control) = on("gpt-5.6-sol");
        called(&session, 86_000);
        assert_eq!(session.cache_left(), None, "not close yet");
        let ttl = crate::cache::CACHE_TTL;
        let warning = crate::cache::CACHE_WARNING;
        session.lock().last_call = Some(Instant::now() - (ttl - warning / 2));
        let left = session.cache_left().expect("counting down");
        assert!(left <= warning / 2 && left > warning / 4, "{left:?}");
        session.lock().last_call = Some(Instant::now() - ttl);
        assert_eq!(session.cache_left(), None, "expired instead");
        assert_eq!(session.cold_tokens(), Some(86_000));
    }

    #[test]
    fn the_cache_clock_starts_when_the_call_is_sent() {
        let (session, _control) = on("gpt-5.6-sol");
        session.on_agent(AgentEvent::Streaming(true));
        let sent = session.lock().sending.expect("stamped");
        session.on_agent(AgentEvent::Streaming(false));
        called(&session, 86_000);
        assert_eq!(session.lock().last_call, Some(sent));
    }
}
