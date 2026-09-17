//! The agent loop: call the model, run the tools it asks for, feed the results back,
//! repeat until it stops asking for tools.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::client::{Client, Delta, Usage};
use crate::permissions::{Decision, Policy};
use crate::profile::{self, Measured, Profile};
use crate::prompt::SystemPrompt;
use crate::tools::{self, Registry};

/// Hard cap on model calls in a single turn, so a confused loop cannot run forever.
const MAX_STEPS: usize = 40;
/// Stop the turn after this many consecutive rounds where every tool call failed.
const MAX_ERROR_ROUNDS: usize = 3;

/// Everything the agent tells the UI.
#[derive(Debug)]
pub enum AgentEvent {
    Reasoning(String),
    Text(String),
    /// The agent wants to run a tool call; `reply` carries the user's decision back.
    Approval {
        tool: String,
        /// The command, or a one-line summary of the call.
        command: String,
        reply: oneshot::Sender<bool>,
    },
    ToolStart(String),
    ToolOutput(String),
    ToolRejected(String),
    /// A notice for the transcript, such as a call the policy allowed.
    Info(String),
    /// Token counts for the model call that just finished.
    Usage(Usage),
    Error(String),
    /// The agent is done with this turn and is waiting for input.
    TurnEnd,
}

/// Requests answered by the agent task, which owns the history, even mid-turn.
#[derive(Debug)]
pub enum Control {
    /// A token breakdown of the context the next request would send.
    Context(oneshot::Sender<Profile>),
}

/// `usage_log` is the JSONL file each model call's usage is appended to, if any.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    prompt: SystemPrompt,
    policy: Arc<Policy>,
    mut rx_user: mpsc::Receiver<String>,
    mut rx_control: mpsc::Receiver<Control>,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: Arc<AtomicBool>,
    usage_log: Option<PathBuf>,
) {
    let client = match Client::new() {
        Ok(client) => client,
        Err(e) => {
            let _ = tx.send(AgentEvent::Error(format!("{e:#}")));
            return;
        }
    };
    let registry = Registry::new(prompt.skills.clone());
    let tools = registry.schemas();
    let mut history: Vec<Value> = Vec::new();
    let mut measured: Option<Measured> = None;

    loop {
        let message = tokio::select! {
            Some(Control::Context(reply)) = rx_control.recv() => {
                let _ = reply.send(profile::build(&prompt, &tools, &history, measured));
                continue;
            }
            message = rx_user.recv() => match message {
                Some(message) => message,
                None => break,
            },
        };
        // `Session::submit` clears `cancel` before sending, so an early interrupt holds.
        history.push(json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": message }],
        }));

        // The turn holds the history, so mid-turn requests see it as the turn started.
        let (before, measured_before) = (history.clone(), measured);
        let result = {
            let turn = turn(
                &client,
                &registry,
                &policy,
                &tools,
                &prompt.text,
                &mut history,
                &tx,
                &cancel,
                &mut measured,
                usage_log.as_deref(),
            );
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    result = &mut turn => break result,
                    Some(Control::Context(reply)) = rx_control.recv() => {
                        let _ = reply.send(profile::build(
                            &prompt,
                            &tools,
                            &before,
                            measured_before,
                        ));
                    }
                }
            }
        };
        if let Err(e) = result {
            let _ = tx.send(AgentEvent::Error(format!("{e:#}")));
        }
        let _ = tx.send(AgentEvent::TurnEnd);
    }
}

#[allow(clippy::too_many_arguments)]
async fn turn(
    client: &Client,
    registry: &Registry,
    policy: &Policy,
    tools: &[Value],
    instructions: &str,
    history: &mut Vec<Value>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &Arc<AtomicBool>,
    measured: &mut Option<Measured>,
    usage_log: Option<&Path>,
) -> anyhow::Result<()> {
    let mut error_rounds = 0usize;

    for _ in 0..MAX_STEPS {
        let sent = history.len();
        let mut on_delta = |delta: Delta| {
            let _ = tx.send(match delta {
                Delta::Reasoning(s) => AgentEvent::Reasoning(s),
                Delta::Text(s) => AgentEvent::Text(s),
                Delta::Usage(usage) => {
                    *measured = Some(Measured {
                        input_tokens: usage.input,
                        items: sent,
                    });
                    if let Some(path) = usage_log
                        && let Err(e) = profile::log_usage(path, &usage, sent)
                    {
                        let _ = tx.send(AgentEvent::Error(format!("usage log: {e:#}")));
                    }
                    AgentEvent::Usage(usage)
                }
            });
        };

        // On interrupt or failure nothing is appended, so the history never holds a
        // function_call without its matching output.
        let items = match client
            .respond(instructions, tools, history, &mut on_delta, cancel)
            .await
        {
            Ok(items) => items,
            // An interrupt is the user's decision, not an error worth reporting.
            Err(_) if cancel.load(Ordering::Relaxed) => return Ok(()),
            Err(e) => return Err(e),
        };

        let calls: Vec<&Value> = items
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
            .collect();

        if calls.is_empty() {
            history.extend(items.iter().cloned());
            return Ok(());
        }

        let mut results = Vec::with_capacity(calls.len());
        let mut all_failed = true;
        for call in &calls {
            let call_id = call
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let (output, ok) = execute(registry, policy, call, tx, cancel).await;
            all_failed &= !ok;
            results.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": output,
            }));
        }

        // Append the assistant items and every matching result together.
        history.extend(items.iter().cloned());
        history.extend(results);

        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }

        error_rounds = if all_failed { error_rounds + 1 } else { 0 };
        if error_rounds >= MAX_ERROR_ROUNDS {
            let _ = tx.send(AgentEvent::Error(
                "stopped: the last few tool calls all failed".to_string(),
            ));
            return Ok(());
        }
    }

    let _ = tx.send(AgentEvent::Error(format!(
        "stopped after {MAX_STEPS} steps without finishing"
    )));
    Ok(())
}

/// Returns the tool output and whether it counts as a success.
async fn execute(
    registry: &Registry,
    policy: &Policy,
    call: &Value,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &Arc<AtomicBool>,
) -> (String, bool) {
    if cancel.load(Ordering::Relaxed) {
        return (
            "Not executed: the user interrupted the turn.".to_string(),
            false,
        );
    }

    let name = call.get("name").and_then(Value::as_str).unwrap_or_default();
    let Some(tool) = registry.get(name) else {
        return (registry.unknown(name), false);
    };

    let arguments = call
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (args, summary) = match tools::parse_arguments(arguments)
        .and_then(|args| tool.describe(&args).map(|summary| (args, summary)))
    {
        Ok(parsed) => parsed,
        Err(e) => return (format!("Invalid tool call: {e}"), false),
    };

    // The policy answers first; only `Ask` reaches the prompt.
    match policy.check(name, &args, tool.needs_approval()) {
        Decision::Allow(reason) => {
            if tool.needs_approval() {
                let _ = tx.send(AgentEvent::Info(format!(
                    "auto-allowed: {summary} ({reason})"
                )));
            }
        }
        Decision::Deny(reason) => {
            let _ = tx.send(AgentEvent::ToolRejected(format!("{summary} ({reason})")));
            return (
                format!(
                    "Blocked by the user's permission settings ({reason}); it did not run. Do \
not retry it. Try a different approach, or ask the user."
                ),
                false,
            );
        }
        Decision::Ask => {
            if let Some(result) = ask(name, &summary, tx).await {
                return result;
            }
        }
    }

    let _ = tx.send(AgentEvent::ToolStart(summary));
    let (output, ok) = tool.execute(&args).await;
    let _ = tx.send(AgentEvent::ToolOutput(output.clone()));
    (output, ok)
}

/// Prompt the user for a call. `None` means approved; otherwise the result to return.
async fn ask(
    name: &str,
    summary: &str,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Option<(String, bool)> {
    let (reply, wait) = oneshot::channel();
    if tx
        .send(AgentEvent::Approval {
            tool: name.to_string(),
            command: summary.to_string(),
            reply,
        })
        .is_err()
    {
        return Some((
            "Not executed: the session is shutting down.".to_string(),
            false,
        ));
    }

    if wait.await.unwrap_or(false) {
        return None;
    }
    let _ = tx.send(AgentEvent::ToolRejected(summary.to_string()));
    Some((
        "The user rejected this call; it did not run. Do not retry it as-is. Ask what they \
want instead, or try a different approach."
            .to_string(),
        false,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn context_is_answered_while_idle() {
        let (_tx_user, rx_user) = mpsc::channel(1);
        let (tx_control, rx_control) = mpsc::channel(1);
        let (tx, _rx) = mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        tokio::spawn(run(
            crate::prompt::system_prompt(&[], Vec::new()),
            Arc::new(Policy::default()),
            rx_user,
            rx_control,
            tx,
            cancel,
            None,
        ));

        let (reply, wait) = oneshot::channel();
        tx_control.send(Control::Context(reply)).await.unwrap();
        let profile = wait.await.unwrap();
        let labels: Vec<_> = profile.items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"system prompt"));
        assert!(labels.contains(&"tool: bash"));
        assert!(labels.contains(&"tool: edit"));
        assert!(profile.calibration.is_none());
    }
}
