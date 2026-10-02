//! Host configuration for the existing agent loop.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::agent::{self, AgentEvent, Cancel, Control, Delegation, Inbox, Model, Saved, UserInput};
use crate::compact::Limits;
use crate::judge::Judge;
use crate::permissions::Policy;
use crate::prompt::SystemPrompt;
use crate::tools::Registry;

/// Builds the main loop's complete registry at startup and on model switches.
/// Names and schemas must remain stable across rebuilds. Child registries are separate.
pub type RegistryFactory = Arc<dyn Fn(&SystemPrompt, &Arc<dyn Model>) -> Registry + Send + Sync>;

/// A session's runtime configuration, with no CLI or backend startup side effects.
///
/// `new` disables persistence, delegation, judging and naming. Without `registry`,
/// the existing built-in registry is used. Use worker processes for independent roots:
/// built-in tools and environment messages still use the process working directory.
#[non_exhaustive]
pub struct Runtime {
    /// The backend, including hosts that implement `Model` themselves.
    pub model: Arc<dyn Model>,
    /// The session id used for transcripts and profiling.
    pub session_id: String,
    /// The already-built system prompt.
    pub prompt: SystemPrompt,
    /// The permission policy applied to every tool call.
    pub policy: Arc<Policy>,
    /// Optional automatic approval judge.
    pub judge: Option<Arc<Judge>>,
    /// Optional terminal-title model.
    pub namer: Option<Arc<dyn crate::Name>>,
    /// Messages queued by a session hub during a turn.
    pub inbox: Option<Inbox>,
    /// Optional usage JSONL destination.
    pub usage_log: Option<PathBuf>,
    /// Optional built-in child-agent configuration.
    pub delegation: Option<Delegation>,
    /// Optional persistence and resumed history.
    pub saved: Option<Saved>,
    /// Context compaction limits.
    pub limits: Limits,
    /// Replaces the main registry, including goal, plan and delegation tools.
    pub registry: Option<RegistryFactory>,
}

impl Runtime {
    /// Configure an unsaved session without loading files or contacting a backend.
    pub fn new(
        model: Arc<dyn Model>,
        session_id: impl Into<String>,
        prompt: SystemPrompt,
        policy: Arc<Policy>,
    ) -> Self {
        Self {
            model,
            session_id: session_id.into(),
            prompt,
            policy,
            judge: None,
            namer: None,
            inbox: None,
            usage_log: None,
            delegation: None,
            saved: None,
            limits: Limits::default(),
            registry: None,
        }
    }

    /// Run until the input channel closes. Errors and approvals arrive as events.
    ///
    /// Drain `events` concurrently and answer approval oneshots, or use `session::pump`.
    /// Interrupt a turn through `cancel`; retain the task handle and await shutdown.
    /// Bash sessions outlive it, being process-wide; [`crate::tools::bash::kill_all`]
    /// ends them.
    pub async fn run(
        self,
        user: mpsc::Receiver<UserInput>,
        control: mpsc::Receiver<Control>,
        events: mpsc::UnboundedSender<AgentEvent>,
        cancel: Arc<Cancel>,
    ) {
        agent::run_configured(
            self.model,
            self.session_id,
            self.prompt,
            self.policy,
            self.judge,
            self.namer,
            user,
            self.inbox,
            control,
            events,
            cancel,
            self.usage_log,
            self.delegation,
            self.saved,
            self.limits,
            self.registry,
        )
        .await;
    }
}
