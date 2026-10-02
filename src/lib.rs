//! The bhai runtime, shared by the CLI and embedding hosts.
//!
//! [`Runtime`] runs the existing agent loop without parsing arguments, opening a
//! terminal, loading credentials or starting a server. Hosts supply a [`Model`],
//! [`SystemPrompt`] and [`Policy`], then drive input, control and event channels.
//! [`RegistryFactory`] replaces the main loop's tools, rather than adding built-ins.
//! See `LIBRARY.md` for lifecycle and process-isolation limits.
//!
//! ```no_run
//! use std::sync::Arc;
//! use bhai::{AgentEvent, Cancel, Model, Policy, Registry, Roots, Runtime, SystemPrompt};
//! use bhai::permissions::{Answer, Mode, Rules};
//! use tokio::sync::mpsc;
//!
//! async fn worker(model: Arc<dyn Model>, roots: Roots) -> anyhow::Result<()> {
//!     let policy = Arc::new(Policy::new(Mode::Ask, Rules::default(), roots.home, roots.cwd));
//!     let mut runtime = Runtime::new(model, "worker-1", SystemPrompt::default(), policy);
//!     // A host-only worker with no tools. Add host Tool implementations here.
//!     runtime.registry = Some(Arc::new(|_, _| Registry::empty()));
//!     let (user, rx_user) = mpsc::channel(1);
//!     let (_control, rx_control) = mpsc::channel(1);
//!     let (events, mut rx_events) = mpsc::unbounded_channel();
//!     let cancel = Arc::new(Cancel::default());
//!     let task = tokio::spawn(runtime.run(rx_user, rx_control, events, cancel));
//!     user.send("Describe the task".into()).await?;
//!     drop(user); // Exit after processing the queued input.
//!     while let Some(event) = rx_events.recv().await {
//!         if let AgentEvent::Approval { reply, .. } = event {
//!             let _ = reply.send(Answer::Reject);
//!         }
//!     }
//!     task.await?;
//!     Ok(())
//! }
//! ```

pub mod agent;
mod app;
mod askpass;
mod auth;
pub mod background;
mod bgview;
mod branch;
pub mod cache;
mod childenv;
#[doc(hidden)]
pub mod cli;
pub mod client;
mod clipboard;
mod commands;
pub mod compact;
pub mod config;
mod debug;
#[cfg(feature = "dictation")]
mod dictation;
mod diff;
mod egress;
pub mod entries;
mod environment;
mod external;
mod files;
mod frontmatter;
pub mod goal;
pub mod identity;
mod images;
mod input;
pub mod instructions;
pub mod judge;
pub mod limits;
mod links;
mod markdown;
pub mod mcp;
pub mod memory;
mod mermaid;
pub mod models;
mod notify;
mod ollama;
mod palette;
pub mod permissions;
pub mod plan;
pub mod profile;
pub mod prompt;
mod redact;
mod runtime;
mod sandbox;
pub mod schedule;
pub mod schedules;
mod search;
pub mod server;
pub mod session;
pub mod sessions;
pub mod skills;
mod speed;
mod startup;
mod statusline;
mod syntax;
mod title;
pub mod tokens;
pub mod tools;
mod trace;
mod ui;
mod websocket;
pub mod workflow;
pub mod worktrees;
mod wrap;

pub use agent::{AgentEvent, Cancel, Control, Model, UserInput};
pub use instructions::Roots;
pub use permissions::Policy;
pub use prompt::SystemPrompt;
pub use runtime::{RegistryFactory, Runtime};
pub use title::Name;
pub use tools::{Registry, Tool};
