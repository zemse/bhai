//! Minimal streaming client for the Responses API behind a ChatGPT/Codex subscription.
//!
//! Endpoint, headers and request shape mirror openai/codex (`codex-rs/core/src/client.rs`
//! and `codex-rs/model-provider-info`): `POST https://chatgpt.com/backend-api/codex/responses`
//! with the ChatGPT access token as a bearer and the workspace id in `ChatGPT-Account-ID`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};

use crate::auth::{self, Auth};

const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const ORIGINATOR: &str = "codex_cli_rs";
const DEFAULT_MODEL: &str = "gpt-5.5";
const DEFAULT_EFFORT: &str = "medium";
/// Give up on a stream that has produced nothing for this long.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_ATTEMPTS: usize = 3;

/// What the UI is told while a turn streams.
#[derive(Debug, Clone)]
pub enum Delta {
    Reasoning(String),
    Text(String),
    Usage(Usage),
}

/// Token counts for one model call, as `response.completed` reports them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Usage {
    pub input: u64,
    /// Input tokens served from the prompt cache.
    pub cached: u64,
    pub output: u64,
    /// Output tokens spent on reasoning.
    pub reasoning: u64,
}

impl Usage {
    /// Read the counts from a `response.completed` event; missing fields count as zero.
    pub fn from_completed(event: &Value) -> Self {
        let count = |path: &str| {
            event
                .pointer(&format!("/response/usage/{path}"))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        Self {
            input: count("input_tokens"),
            cached: count("input_tokens_details/cached_tokens"),
            output: count("output_tokens"),
            reasoning: count("output_tokens_details/reasoning_tokens"),
        }
    }

    /// Percent of the input that was a cache hit, when there was any input.
    pub fn cache_rate(&self) -> Option<f64> {
        (self.input > 0).then(|| self.cached as f64 * 100.0 / self.input as f64)
    }
}

pub struct Client {
    http: reqwest::Client,
    session_id: String,
    model: String,
    effort: String,
}

impl Client {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .build()
            .context("could not build HTTP client")?;
        let (model, effort) = model_settings();
        Ok(Self {
            http,
            session_id: uuid::Uuid::new_v4().to_string(),
            model,
            effort,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Run one model call and return the assistant's output items verbatim,
    /// so they can be replayed into the next request unmodified.
    pub async fn respond(
        &self,
        instructions: &str,
        tools: &[Value],
        input: &[Value],
        on_delta: &mut impl FnMut(Delta),
        cancel: &Arc<AtomicBool>,
    ) -> Result<Vec<Value>> {
        let body = json!({
            "model": self.model,
            "instructions": instructions,
            "input": input,
            "tools": tools,
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "reasoning": { "effort": self.effort, "summary": "auto" },
            "store": false,
            "stream": true,
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": self.session_id,
        });

        let mut backoff = Duration::from_millis(500);
        let mut last_err = None;
        for attempt in 1..=MAX_ATTEMPTS {
            match self.attempt(&body, on_delta, cancel).await {
                Ok(items) => return Ok(items),
                Err(Error::Interrupted) => bail!("interrupted"),
                Err(Error::Fatal(e)) => return Err(e),
                Err(Error::Retryable(e)) => {
                    last_err = Some(e);
                    if attempt < MAX_ATTEMPTS {
                        tokio::time::sleep(backoff).await;
                        backoff *= 3;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("request failed")))
    }

    async fn attempt(
        &self,
        body: &Value,
        on_delta: &mut impl FnMut(Delta),
        cancel: &Arc<AtomicBool>,
    ) -> std::result::Result<Vec<Value>, Error> {
        let auth = auth::load(&self.http).await.map_err(Error::Fatal)?;

        let resp = self
            .request(&auth, body)
            .send()
            .await
            .map_err(|e| Error::Retryable(anyhow!("request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let msg = api_error_message(&body).unwrap_or(body);
            return Err(if status.as_u16() == 401 {
                Error::Fatal(anyhow!(
                    "unauthorized ({status}): {msg}. Run `codex login`."
                ))
            } else if status.as_u16() == 429 || status.as_u16() >= 500 {
                Error::Retryable(anyhow!("{status}: {msg}"))
            } else {
                Error::Fatal(anyhow!("{status}: {msg}"))
            });
        }

        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        // The ChatGPT backend leaves `response.completed.response.output` empty, so the
        // turn is assembled from the per-item `done` events instead.
        let mut items: Vec<Value> = Vec::new();
        let mut completed = false;

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
                let line = buf[..nl].trim_end_matches('\r').to_string();
                buf.drain(..=nl);
                let Some(data) = line.strip_prefix("data:") else {
                    continue; // `event:` lines and blank separators carry nothing we need
                };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let Ok(event) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                debug_log(data);
                match event.get("type").and_then(Value::as_str).unwrap_or("") {
                    "response.output_text.delta" => {
                        if let Some(d) = event.get("delta").and_then(Value::as_str) {
                            on_delta(Delta::Text(d.to_string()));
                        }
                    }
                    "response.reasoning_summary_text.delta" => {
                        if let Some(d) = event.get("delta").and_then(Value::as_str) {
                            on_delta(Delta::Reasoning(d.to_string()));
                        }
                    }
                    "response.reasoning_summary_part.added" => {
                        on_delta(Delta::Reasoning("\n".to_string()));
                    }
                    "response.output_item.done" => {
                        if let Some(item) = event.get("item") {
                            items.push(item.clone());
                        }
                    }
                    "response.completed" => {
                        completed = true;
                        // Some deployments do populate it; prefer their copy when present.
                        if let Some(output) = event
                            .pointer("/response/output")
                            .and_then(Value::as_array)
                            .filter(|output| !output.is_empty())
                        {
                            items = output.clone();
                        }
                        on_delta(Delta::Usage(Usage::from_completed(&event)));
                    }
                    "response.failed" => {
                        let msg = event
                            .pointer("/response/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("response failed");
                        return Err(Error::Retryable(anyhow!("{msg}")));
                    }
                    "error" => {
                        let msg = event
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("stream error");
                        return Err(Error::Retryable(anyhow!("{msg}")));
                    }
                    _ => {}
                }
            }
        }

        if completed {
            Ok(items)
        } else {
            Err(Error::Retryable(anyhow!(
                "stream ended before response.completed"
            )))
        }
    }

    fn request(&self, auth: &Auth, body: &Value) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .post(format!("{BASE_URL}/responses"))
            .header("Authorization", format!("Bearer {}", auth.access_token))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", ORIGINATOR)
            .header("session-id", &self.session_id)
            .header(
                "User-Agent",
                concat!("bhai/", env!("CARGO_PKG_VERSION"), " (codex_cli_rs)"),
            )
            .json(body);
        if let Some(account_id) = &auth.account_id {
            req = req.header("ChatGPT-Account-ID", account_id);
        }
        req
    }
}

enum Error {
    Retryable(anyhow::Error),
    Fatal(anyhow::Error),
    Interrupted,
}

/// Set `BHAI_DEBUG_SSE=/path/to/file` to append every raw stream event, for when the
/// backend changes shape under you.
fn debug_log(line: &str) {
    let Ok(path) = std::env::var("BHAI_DEBUG_SSE") else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(file, "{line}");
    }
}

fn api_error_message(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    v.pointer("/error/message")
        .or_else(|| v.pointer("/detail"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Model and reasoning effort: env overrides first, then whatever `~/.codex/config.toml`
/// has at top level. Deliberately a line scan rather than a TOML dependency.
fn model_settings() -> (String, String) {
    let mut model = std::env::var("BHAI_MODEL").ok().filter(|s| !s.is_empty());
    let mut effort = std::env::var("BHAI_EFFORT").ok().filter(|s| !s.is_empty());

    if model.is_none() || effort.is_none() {
        let config = auth::codex_home()
            .ok()
            .and_then(|home| std::fs::read_to_string(home.join("config.toml")).ok())
            .unwrap_or_default();
        for line in config.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                break; // top-level keys only
            }
            if let Some(v) = toml_string(line, "model") {
                model.get_or_insert(v);
            } else if let Some(v) = toml_string(line, "model_reasoning_effort") {
                effort.get_or_insert(v);
            }
        }
    }

    (
        model.unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        effort.unwrap_or_else(|| DEFAULT_EFFORT.to_string()),
    )
}

fn toml_string(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim();
    Some(rest.trim_matches('"').to_string()).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_is_read_from_response_completed() {
        let event: Value = serde_json::from_str(
            r#"{"type":"response.completed","response":{"id":"r1","output":[],"usage":{
                "input_tokens":1200,"input_tokens_details":{"cached_tokens":900},
                "output_tokens":80,"output_tokens_details":{"reasoning_tokens":64},
                "total_tokens":1280}}}"#,
        )
        .unwrap();
        let usage = Usage::from_completed(&event);
        assert_eq!(
            usage,
            Usage {
                input: 1200,
                cached: 900,
                output: 80,
                reasoning: 64,
            }
        );
        assert_eq!(usage.cache_rate(), Some(75.0));
    }

    #[test]
    fn missing_usage_counts_as_zero() {
        let usage = Usage::from_completed(&json!({"type": "response.completed"}));
        assert_eq!(usage, Usage::default());
        assert_eq!(usage.cache_rate(), None);
    }
}
