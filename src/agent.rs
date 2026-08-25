//! The agent loop: call the model, run the tools it asks for, feed the results back,
//! repeat until it stops asking for tools.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::bash;
use crate::client::{Client, Delta};
use crate::prompt::system_prompt;

/// Hard cap on model calls in a single turn, so a confused loop cannot run forever.
const MAX_STEPS: usize = 40;
/// Stop the turn after this many consecutive rounds where every tool call failed.
const MAX_ERROR_ROUNDS: usize = 3;

/// Everything the agent tells the UI.
#[derive(Debug)]
pub enum AgentEvent {
    Reasoning(String),
    Text(String),
    /// The agent wants to run a command; `reply` carries the user's decision back.
    Approval {
        command: String,
        reply: oneshot::Sender<bool>,
    },
    ToolStart(String),
    ToolOutput(String),
    ToolRejected(String),
    /// Token counts for the model call that just finished.
    Usage {
        input: u64,
        output: u64,
    },
    Error(String),
    /// The agent is done with this turn and is waiting for input.
    TurnEnd,
}

pub async fn run(
    mut rx_user: mpsc::Receiver<String>,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: Arc<AtomicBool>,
) {
    let client = match Client::new() {
        Ok(client) => client,
        Err(e) => {
            let _ = tx.send(AgentEvent::Error(format!("{e:#}")));
            return;
        }
    };
    let instructions = system_prompt();
    let mut history: Vec<Value> = Vec::new();

    while let Some(message) = rx_user.recv().await {
        cancel.store(false, Ordering::Relaxed);
        history.push(json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": message }],
        }));

        if let Err(e) = turn(&client, &instructions, &mut history, &tx, &cancel).await {
            let _ = tx.send(AgentEvent::Error(format!("{e:#}")));
        }
        let _ = tx.send(AgentEvent::TurnEnd);
    }
}

async fn turn(
    client: &Client,
    instructions: &str,
    history: &mut Vec<Value>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let mut error_rounds = 0usize;

    for _ in 0..MAX_STEPS {
        let mut on_delta = |delta: Delta| {
            let _ = tx.send(match delta {
                Delta::Reasoning(s) => AgentEvent::Reasoning(s),
                Delta::Text(s) => AgentEvent::Text(s),
                Delta::Usage { input, output } => AgentEvent::Usage { input, output },
            });
        };

        // On interrupt or failure nothing is appended, so the history never holds a
        // function_call without its matching output.
        let items = match client
            .respond(instructions, history, &mut on_delta, cancel)
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
            let (output, ok) = execute(call, tx, cancel).await;
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
    if name != bash::NAME {
        return (
            format!(
                "Unknown tool `{name}`. The only available tool is `{}`.",
                bash::NAME
            ),
            false,
        );
    }

    let arguments = call
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let command = match bash::parse_command(arguments) {
        Ok(command) => command,
        Err(e) => return (format!("Invalid tool call: {e}"), false),
    };

    let (reply, wait) = oneshot::channel();
    if tx
        .send(AgentEvent::Approval {
            command: command.clone(),
            reply,
        })
        .is_err()
    {
        return (
            "Not executed: the session is shutting down.".to_string(),
            false,
        );
    }

    if !wait.await.unwrap_or(false) {
        let _ = tx.send(AgentEvent::ToolRejected(command));
        return (
            "The user rejected this command; it did not run. Do not retry it as-is. Ask what \
they want instead, or try a different approach."
                .to_string(),
            false,
        );
    }

    let _ = tx.send(AgentEvent::ToolStart(command.clone()));
    let output = bash::run(&command).await;
    let _ = tx.send(AgentEvent::ToolOutput(output.clone()));
    (output, true)
}
