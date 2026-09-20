//! The Ollama backend, for a model running on this machine.
//!
//! bhai speaks Responses-shaped items everywhere, so this module is a translator: the
//! history and the tool schemas go out as Ollama chat messages, and the reply comes back
//! as the same `message` and `function_call` items the Codex path produces, so nothing
//! downstream knows which backend answered.
//!
//! `POST {url}/api/chat` streams one JSON object per line rather than SSE, and a tool
//! call arrives whole in a single chunk, so nothing is reassembled from deltas.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::client::{Delta, Error, IDLE_TIMEOUT, Usage};

pub const DEFAULT_URL: &str = "http://localhost:11434";
/// What picks this backend in a model id: `ollama:gemma4:e2b`.
pub const PREFIX: &str = "ollama:";

/// The model id as Ollama knows it, without the prefix that chose the backend.
pub fn model_name(spec: &str) -> &str {
    spec.strip_prefix(PREFIX).unwrap_or(spec)
}

/// The `/api/chat` request body for one call.
pub fn request_body(model: &str, instructions: &str, tools: &[Value], input: &[Value]) -> Value {
    let mut body = json!({
        "model": model_name(model),
        "messages": messages(instructions, input),
        "stream": true,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tool_defs(tools));
    }
    body
}

/// The history as chat messages: the instructions as a system message, then every item
/// that carries something the model reads. Reasoning items are dropped, since the
/// encrypted thinking they hold is the other backend's and cannot be replayed here.
pub fn messages(instructions: &str, input: &[Value]) -> Vec<Value> {
    let mut out = Vec::with_capacity(input.len() + 1);
    if !instructions.is_empty() {
        out.push(json!({ "role": "system", "content": instructions }));
    }
    // A `function_call_output` carries only the call id, and Ollama pairs a result with
    // its tool by name, so the names are remembered as the calls go past.
    let mut names: HashMap<&str, &str> = HashMap::new();
    for item in input {
        let field = |key: &str| item.get(key).and_then(Value::as_str);
        let text = crate::tokens::item_text(item).unwrap_or_default();
        match field("type") {
            Some("message") => out.push(json!({
                "role": field("role").unwrap_or("user"),
                "content": text,
            })),
            Some("function_call") => {
                let name = field("name").unwrap_or_default();
                if let Some(id) = field("call_id") {
                    names.insert(id, name);
                }
                out.push(json!({
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{ "function": {
                        "name": name,
                        "arguments": arguments(field("arguments").unwrap_or_default()),
                    }}],
                }));
            }
            Some("function_call_output") => out.push(json!({
                "role": "tool",
                "tool_name": field("call_id")
                    .and_then(|id| names.get(id).copied())
                    .unwrap_or_default(),
                "content": text,
            })),
            _ => {}
        }
    }
    out
}

/// The flat Responses function schemas as Ollama's nested ones.
pub fn tool_defs(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let field = |key: &str| tool.get(key).cloned();
            json!({
                "type": "function",
                "function": {
                    "name": field("name").unwrap_or_else(|| json!("")),
                    "description": field("description").unwrap_or_else(|| json!("")),
                    "parameters": field("parameters")
                        .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                }
            })
        })
        .collect()
}

/// Tool arguments as an object. Both backends pass them around as a JSON string;
/// Ollama wants the object, and anything unparseable is sent as no arguments at all.
fn arguments(raw: &str) -> Value {
    serde_json::from_str(raw)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

/// One call: post the body and read the newline-delimited reply into output items.
pub async fn attempt(
    http: &reqwest::Client,
    url: &str,
    body: &Value,
    on_delta: &mut impl FnMut(Delta),
    cancel: &Arc<AtomicBool>,
) -> std::result::Result<Vec<Value>, Error> {
    let endpoint = format!("{}/api/chat", url.trim_end_matches('/'));
    let resp = http
        .post(&endpoint)
        .json(body)
        .send()
        .await
        .map_err(|e| Error::Retryable(anyhow!("request to {endpoint} failed: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let msg = error_message(&text).unwrap_or(text);
        return Err(match status.as_u16() {
            code if code >= 500 => Error::Retryable(anyhow!("{status}: {msg}")),
            _ => Error::Fatal(anyhow!("{status}: {msg}")),
        });
    }

    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut text = String::new();
    let mut calls: Vec<Value> = Vec::new();
    let mut done = false;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Interrupted);
        }
        let chunk = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
            Err(_) => return Err(Error::Retryable(anyhow!("stream idle for too long"))),
            Ok(None) => break,
            Ok(Some(Err(e))) => return Err(Error::Retryable(anyhow!("stream error: {e}"))),
            Ok(Some(Ok(chunk))) => chunk,
        };
        buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].trim().to_string();
            buf.drain(..=nl);
            if line.is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(msg) = event.get("error").and_then(Value::as_str) {
                return Err(Error::Fatal(anyhow!("{msg}")));
            }
            if let Some(part) = event.pointer("/message/thinking").and_then(Value::as_str)
                && !part.is_empty()
            {
                on_delta(Delta::Reasoning(part.to_string()));
            }
            if let Some(part) = event.pointer("/message/content").and_then(Value::as_str)
                && !part.is_empty()
            {
                text.push_str(part);
                on_delta(Delta::Text(part.to_string()));
            }
            if let Some(found) = event
                .pointer("/message/tool_calls")
                .and_then(Value::as_array)
            {
                calls.extend(found.iter().map(call_item));
            }
            if event.get("done").and_then(Value::as_bool) == Some(true) {
                done = true;
                on_delta(Delta::Usage(usage(&event)));
            }
        }
    }

    if !done {
        return Err(Error::Retryable(anyhow!("stream ended before done")));
    }
    Ok(items(text, calls))
}

/// One streamed tool call as a `function_call` item. Ollama sends the arguments as an
/// object and, before 0.6, no id at all, so one is made up to pair the result with.
fn call_item(call: &Value) -> Value {
    let function = call.get("function").unwrap_or(call);
    let arguments = match function.get("arguments") {
        Some(Value::String(raw)) => raw.clone(),
        Some(other) => other.to_string(),
        None => "{}".to_string(),
    };
    json!({
        "type": "function_call",
        "call_id": call
            .get("id")
            .and_then(Value::as_str)
            .map_or_else(|| uuid::Uuid::new_v4().simple().to_string(), str::to_string),
        "name": function.get("name").and_then(Value::as_str).unwrap_or_default(),
        "arguments": arguments,
    })
}

/// The turn's output items: the assistant's text, when it said anything, then its calls.
fn items(text: String, calls: Vec<Value>) -> Vec<Value> {
    let mut items = Vec::with_capacity(calls.len() + 1);
    if !text.is_empty() {
        items.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text }],
        }));
    }
    items.extend(calls);
    items
}

/// Token counts from the final chunk. Ollama serves a cached prefix without saying how
/// much of one it reused, so `cached` stays zero.
fn usage(event: &Value) -> Usage {
    let count = |key: &str| event.get(key).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input: count("prompt_eval_count"),
        cached: 0,
        output: count("eval_count"),
        reasoning: 0,
    }
}

/// Check the server is up and the model is pulled, before the TUI takes the terminal.
pub async fn preflight(http: &reqwest::Client, url: &str, model: &str) -> Result<()> {
    let endpoint = format!("{}/api/tags", url.trim_end_matches('/'));
    let resp =
        http.get(&endpoint).send().await.map_err(|e| {
            anyhow!("no Ollama server at {url} ({e}). Start one with `ollama serve`.")
        })?;
    let body: Value = resp
        .json()
        .await
        .map_err(|e| anyhow!("{endpoint} did not answer JSON: {e}"))?;
    let wanted = model_name(model);
    let installed: Vec<&str> = body
        .get("models")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|m| m.get("name").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    // `gemma4:e2b` and a bare `gemma4`, which Ollama reads as `gemma4:latest`.
    let has = |name: &str| {
        installed.contains(&name)
            || installed
                .iter()
                .any(|i| i.strip_suffix(":latest") == Some(name))
    };
    if !has(wanted) {
        anyhow::bail!(
            "Ollama has no model `{wanted}`. Pull it with `ollama pull {wanted}`, or pick one of: {}",
            match installed.is_empty() {
                true => "nothing is installed".to_string(),
                false => installed.join(", "),
            }
        );
    }
    Ok(())
}

fn error_message(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .get("error")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> Vec<Value> {
        vec![
            json!({"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": "list the files"}]}),
            json!({"type": "reasoning", "encrypted_content": "gAAA"}),
            json!({"type": "function_call", "call_id": "c1", "name": "bash",
                   "arguments": "{\"command\":\"ls\"}"}),
            json!({"type": "function_call_output", "call_id": "c1", "output": "Cargo.toml"}),
            json!({"type": "message", "role": "assistant",
                   "content": [{"type": "output_text", "text": "one file"}]}),
        ]
    }

    #[test]
    fn history_becomes_chat_messages() {
        let messages = messages("be brief", &history());
        let roles: Vec<&str> = messages
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        // The reasoning item is dropped: its thinking belongs to the other backend.
        assert_eq!(roles, ["system", "user", "assistant", "tool", "assistant"]);
        assert_eq!(messages[0]["content"], "be brief");
        assert_eq!(messages[1]["content"], "list the files");
        // Arguments go over as an object, not the JSON string the items carry.
        assert_eq!(
            messages[2]["tool_calls"][0]["function"],
            json!({"name": "bash", "arguments": {"command": "ls"}})
        );
        // The result is paired with its tool by name, since Ollama has no call ids.
        assert_eq!(messages[3]["tool_name"], "bash");
        assert_eq!(messages[3]["content"], "Cargo.toml");
    }

    #[test]
    fn unparseable_arguments_go_over_as_none() {
        assert_eq!(arguments("not json"), json!({}));
        assert_eq!(arguments("[1, 2]"), json!({}));
        assert_eq!(arguments(""), json!({}));
    }

    #[test]
    fn tool_schemas_are_nested_under_function() {
        let flat = json!({
            "type": "function",
            "name": "read",
            "description": "Read a file.",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
        });
        let defs = tool_defs(std::slice::from_ref(&flat));
        assert_eq!(defs[0]["type"], "function");
        assert_eq!(defs[0]["function"]["name"], "read");
        assert_eq!(defs[0]["function"]["description"], "Read a file.");
        assert_eq!(defs[0]["function"]["parameters"], flat["parameters"]);
    }

    #[test]
    fn the_prefix_picks_the_backend_but_not_the_model_name() {
        assert_eq!(model_name("ollama:gemma4:e2b"), "gemma4:e2b");
        assert_eq!(model_name("gpt-5.5"), "gpt-5.5");
        let body = request_body("ollama:gemma4:e2b", "hi", &[], &[]);
        assert_eq!(body["model"], "gemma4:e2b");
        assert_eq!(body["stream"], true);
        // An empty tool list is left out rather than sent as one.
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn a_streamed_tool_call_becomes_a_function_call_item() {
        let call = json!({"function": {"name": "bash", "arguments": {"command": "ls"}}});
        let item = call_item(&call);
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["name"], "bash");
        // The agent pairs a result by call id, so one is made up when Ollama sends none.
        assert!(!item["call_id"].as_str().unwrap().is_empty());
        // Arguments come back as the JSON string every tool parses.
        assert_eq!(item["arguments"], "{\"command\":\"ls\"}");
    }

    #[test]
    fn text_and_calls_come_back_as_responses_items() {
        let call = call_item(&json!({"function": {"name": "bash", "arguments": {}}}));
        let turn = items("thinking out loud".to_string(), vec![call]);
        assert_eq!(turn[0]["type"], "message");
        assert_eq!(turn[0]["role"], "assistant");
        assert_eq!(turn[0]["content"][0]["text"], "thinking out loud");
        assert_eq!(turn[1]["type"], "function_call");
        // A silent turn says nothing rather than sending an empty message.
        assert!(items(String::new(), Vec::new()).is_empty());
    }

    #[test]
    fn the_final_chunk_carries_the_counts() {
        let done = json!({"done": true, "prompt_eval_count": 812, "eval_count": 44});
        assert_eq!(
            usage(&done),
            Usage {
                input: 812,
                cached: 0,
                output: 44,
                reasoning: 0,
            }
        );
        assert_eq!(usage(&json!({"done": true})), Usage::default());
    }
}
