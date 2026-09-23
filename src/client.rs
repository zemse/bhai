//! Minimal streaming client for the Responses API behind a ChatGPT/Codex subscription.
//!
//! Endpoint, headers and request shape mirror openai/codex (`codex-rs/core/src/client.rs`
//! and `codex-rs/model-provider-info`): `POST https://chatgpt.com/backend-api/codex/responses`
//! with the ChatGPT access token as a bearer and the workspace id in `ChatGPT-Account-ID`.
//!
//! A model id prefixed `ollama:` is served by Ollama on this machine instead; the body
//! and the stream are then [`crate::ollama`]'s, and everything else here is the same.

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};

use crate::auth::{self, Auth};
use crate::cache::{self, CacheBreak, CacheGuard};
use crate::compact;
use crate::limits::{self, RateLimits};
use crate::ollama;

pub(crate) const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub(crate) const ORIGINATOR: &str = "codex_cli_rs";
const DEFAULT_MODEL: &str = "gpt-5.5";
const DEFAULT_EFFORT: &str = "medium";
/// Give up on a stream that has produced nothing for this long.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// How often a wait inside [`IDLE_TIMEOUT`] looks at the cancel flag.
const CANCEL_POLL: Duration = Duration::from_millis(50);
const MAX_ATTEMPTS: usize = 3;

/// Where a session's inference runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// The Responses API behind the ChatGPT/Codex subscription.
    Codex,
    /// A model served by Ollama on this machine.
    Ollama,
}

impl Provider {
    /// The backend a model id names: `ollama:<name>` is local, anything else is Codex.
    pub fn of(model: &str) -> Self {
        match model.starts_with(ollama::PREFIX) {
            true => Provider::Ollama,
            false => Provider::Codex,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Provider::Codex => "codex",
            Provider::Ollama => "ollama",
        }
    }
}

/// What the config asks for, under the environment and above the Codex CLI's own file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Choice {
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Where the Ollama server is, when the model is one of its own.
    pub ollama_url: Option<String>,
}

/// What the UI is told while a turn streams.
#[derive(Debug, Clone)]
pub enum Delta {
    Reasoning(String),
    Text(String),
    Usage(Usage),
    /// The request about to be sent breaks the prompt cache, or `None` when it is clean.
    Cache(Option<CacheBreak>),
    /// Rate-limit headroom from the response headers or a stream event.
    RateLimits(RateLimits),
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

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    session_id: String,
    /// The prompt cache key; the session id unless this is a child's client.
    cache_key: String,
    model: String,
    effort: String,
    /// Where the Ollama server is, for a model served by one.
    ollama_url: String,
    /// The configured context window, when one is set; what Ollama is asked to hold.
    window: Option<u64>,
    /// The conversation's cache guard; shared by clones, fresh for each child.
    guard: Arc<Mutex<CacheGuard>>,
    /// `--profile`: where the rate-limit response headers are logged.
    header_log: Option<PathBuf>,
}

impl Client {
    pub fn new(choice: &Choice) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .build()
            .context("could not build HTTP client")?;
        let (model, effort) = model_settings(choice);
        let session_id = uuid::Uuid::new_v4().to_string();
        Ok(Self {
            http,
            cache_key: session_id.clone(),
            guard: guard(&session_id, false),
            session_id,
            model,
            effort,
            ollama_url: choice
                .ollama_url
                .clone()
                .or_else(|| env("BHAI_OLLAMA_URL"))
                .unwrap_or_else(|| ollama::DEFAULT_URL.to_string()),
            window: None,
            header_log: None,
        })
    }

    /// The configured context window, which Ollama is asked for rather than left to
    /// guess. Compaction reads the same number, so the two agree on what fits.
    pub fn with_window(mut self, window: Option<u64>) -> Self {
        self.window = window;
        self
    }

    /// Which backend this client's model is served by.
    pub fn provider(&self) -> Provider {
        Provider::of(&self.model)
    }

    /// Fail before the terminal is taken over when the backend cannot serve the model:
    /// no ChatGPT credentials, or an Ollama server that is not running it.
    pub async fn preflight(&self) -> Result<()> {
        match self.provider() {
            Provider::Codex => auth::load(&self.http).await.map(|_| ()),
            Provider::Ollama => ollama::preflight(&self.http, &self.ollama_url, &self.model).await,
        }
    }

    /// `--strict-cache`: refuse to send a request that breaks the prompt cache.
    pub fn strict_cache(mut self, strict: bool) -> Self {
        self.guard = guard(&self.session_id, strict);
        self
    }

    /// Whether `--strict-cache` is on for this client.
    pub fn strict(&self) -> bool {
        self.guard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .strict()
    }

    /// Append the rate-limit response headers of every call to `path`.
    pub fn log_headers(mut self, path: Option<PathBuf>) -> Self {
        self.header_log = path;
        self
    }

    /// An identity's model and effort, where set, in place of the configured ones.
    pub fn with_overrides(mut self, model: Option<String>, effort: Option<String>) -> Self {
        self.model = model.unwrap_or(self.model);
        self.effort = effort.unwrap_or(self.effort);
        self
    }

    /// The same session on another model, for `/model`. The guard is shared with the
    /// client this came from, and a different model reads a different cached prefix, so
    /// the switch forgets the last request rather than reporting it as a break.
    pub fn switch(&self, model: &str, effort: &str) -> Self {
        let switched = self
            .clone()
            .with_overrides(Some(model.to_string()), Some(effort.to_string()));
        switched.reset_cache("the model changed");
        switched
    }

    /// Where this client's Ollama server is, for asking it what it has pulled.
    pub fn ollama_url(&self) -> &str {
        &self.ollama_url
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn effort(&self) -> &str {
        &self.effort
    }

    /// Continue session `id`, so the prompt cache key stays the same. Call before
    /// `strict_cache`, which keeps this id.
    pub fn with_session(mut self, id: &str) -> Self {
        self.session_id = id.to_string();
        self.cache_key = self.session_id.clone();
        self.guard = guard(&self.session_id, false);
        self
    }

    /// Take `input` as already sent with these instructions and tools.
    pub fn seed(&self, instructions: &str, tools: &[Value], input: &[Value]) {
        if self.provider() != Provider::Codex {
            return; // no other backend has a prompt cache to keep
        }
        let body = self.body(instructions, tools, input);
        self.guard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .seed(&body);
    }

    /// Forget the last request, for an intended break such as a compaction.
    pub fn reset_cache(&self, reason: &str) {
        self.guard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reset(reason);
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The client for a child running as `identity`: its overrides, and a cache key of
    /// its own so children of one identity share a cached prefix.
    pub fn for_child(&self, identity: &crate::identity::Identity) -> Self {
        let mut child = self
            .clone()
            .with_overrides(identity.model.clone(), identity.effort.clone());
        child.cache_key = format!("{}-{}", self.session_id, identity.name);
        let strict = self
            .guard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .strict();
        let id = uuid::Uuid::new_v4().simple().to_string();
        child.guard = guard(&format!("{}/{}", child.cache_key, &id[..6]), strict);
        child
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
        let body = self.body(instructions, tools, input);
        // Only the Codex backend has a prompt cache, and the guard reads a Responses
        // body, so an Ollama call is neither checked nor reported.
        if self.provider() == Provider::Codex {
            // Checked once per call, so retries of the same body are not compared.
            let checked = self
                .guard
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .check(&body);
            let found = match checked {
                Ok(found) => found,
                // Strict mode refuses the call, but the break still belongs in the status bar.
                Err(e) => {
                    if let Some(found) = cache::refused(&e) {
                        on_delta(Delta::Cache(Some(found.clone())));
                    }
                    return Err(e);
                }
            };
            on_delta(Delta::Cache(found));
        }

        let mut backoff = Duration::from_millis(500);
        let mut last_err = None;
        for attempt in 1..=MAX_ATTEMPTS {
            match self.attempt(self.provider(), &body, on_delta, cancel).await {
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

    /// One call outside the conversation: no tools, its own `prompt_cache_key` suffix,
    /// and no cache guard, so it never disturbs the conversation's cached prefix. One
    /// attempt only, so a caller's timeout means what it says. Returns the assistant's
    /// text and what the call cost.
    pub async fn aside(
        &self,
        key: &str,
        model: &str,
        effort: &str,
        instructions: &str,
        text: &str,
    ) -> Result<(String, Usage)> {
        let input = [json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": text }],
        })];
        let provider = Provider::of(model);
        let body = match provider {
            Provider::Codex => request_body(
                model,
                effort,
                &format!("{}-{key}", self.session_id),
                instructions,
                &[],
                &input,
            ),
            Provider::Ollama => {
                ollama::request_body(model, instructions, &[], &input, self.window(model))
            }
        };
        let mut usage = Usage::default();
        let mut on_delta = |delta: Delta| {
            if let Delta::Usage(found) = delta {
                usage = found;
            }
        };
        let items = match self
            .attempt(
                provider,
                &body,
                &mut on_delta,
                &Arc::new(AtomicBool::new(false)),
            )
            .await
        {
            Ok(items) => items,
            Err(Error::Interrupted) => bail!("interrupted"),
            Err(Error::Retryable(e) | Error::Fatal(e)) => return Err(e),
        };
        Ok((output_text(&items), usage))
    }

    /// The request body for one call, in whichever shape this client's backend reads.
    fn body(&self, instructions: &str, tools: &[Value], input: &[Value]) -> Value {
        match self.provider() {
            Provider::Codex => request_body(
                &self.model,
                &self.effort,
                &self.cache_key,
                instructions,
                tools,
                input,
            ),
            Provider::Ollama => ollama::request_body(
                &self.model,
                instructions,
                tools,
                input,
                self.window(&self.model),
            ),
        }
    }

    /// The window to ask a backend for: the configured one, else what `model` is
    /// assumed to hold, which is what compaction measures against too.
    fn window(&self, model: &str) -> u64 {
        self.window
            .unwrap_or_else(|| compact::default_window(model))
    }

    /// One call. `provider` is passed rather than read off the client, since `aside`
    /// may run its model on the other backend.
    async fn attempt(
        &self,
        provider: Provider,
        body: &Value,
        on_delta: &mut impl FnMut(Delta),
        cancel: &Arc<AtomicBool>,
    ) -> std::result::Result<Vec<Value>, Error> {
        if provider == Provider::Ollama {
            return ollama::attempt(&self.http, &self.ollama_url, body, on_delta, cancel).await;
        }
        let auth = auth::load(&self.http).await.map_err(Error::Fatal)?;

        let resp = watched(
            self.request(&auth, body).send(),
            cancel,
            "request timed out",
        )
        .await?
        .map_err(|e| Error::Retryable(anyhow!("request failed: {e}")))?;

        // A debug aid only; a failed write must not fail the call or draw over the TUI.
        if let Some(path) = &self.header_log {
            let _ = limits::log_headers(path, resp.headers());
        }
        if let Some(found) =
            RateLimits::from_headers(resp.headers(), chrono::Utc::now().timestamp())
        {
            on_delta(Delta::RateLimits(found));
        }

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
        let mut buf: Vec<u8> = Vec::new();
        // The ChatGPT backend leaves `response.completed.response.output` empty, so the
        // turn is assembled from the per-item `done` events instead.
        let mut items: Vec<Value> = Vec::new();
        let mut completed = false;

        loop {
            let chunk = match watched(stream.next(), cancel, "stream idle for too long").await? {
                None => break,
                Some(Err(e)) => return Err(Error::Retryable(anyhow!("stream error: {e}"))),
                Some(Ok(chunk)) => chunk,
            };
            buf.extend_from_slice(&chunk);

            while let Some(line) = take_line(&mut buf) {
                let line = line.trim_end_matches('\r');
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
                    // `incomplete` is terminal too: the model stopped at a cap, and what
                    // it produced is the answer rather than something to send again.
                    "response.completed" | "response.incomplete" => {
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
                    "codex.rate_limits" => {
                        let now = chrono::Utc::now().timestamp();
                        if let Some(found) = RateLimits::from_event(&event, now) {
                            on_delta(Delta::RateLimits(found));
                        }
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
            // The rest of the batch is drained above, so nothing sharing the terminal
            // event's chunk is lost by leaving here rather than waiting for the close.
            if completed {
                break;
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

/// Await `fut` while watching `cancel`, giving up after [`IDLE_TIMEOUT`] with `idle` as
/// the message. The flag is polled rather than awaited, since an `AtomicBool` has nothing
/// to wake on, and a stream can go a long time between chunks with an interrupt pending.
pub(crate) async fn watched<T>(
    fut: impl Future<Output = T>,
    cancel: &Arc<AtomicBool>,
    idle: &str,
) -> std::result::Result<T, Error> {
    let deadline = tokio::time::Instant::now() + IDLE_TIMEOUT;
    tokio::pin!(fut);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Interrupted);
        }
        let until = (tokio::time::Instant::now() + CANCEL_POLL).min(deadline);
        match tokio::time::timeout_at(until, &mut fut).await {
            Ok(done) => return Ok(done),
            Err(_) if until == deadline => return Err(Error::Retryable(anyhow!("{idle}"))),
            Err(_) => {}
        }
    }
}

/// The next complete line in `buf`, removed from it. Decoding is per line and not per
/// chunk, since a network chunk can end in the middle of a multi-byte character.
pub(crate) fn take_line(buf: &mut Vec<u8>) -> Option<String> {
    let nl = buf.iter().position(|b| *b == b'\n')?;
    let line = String::from_utf8_lossy(&buf[..nl]).into_owned();
    buf.drain(..=nl);
    Some(line)
}

/// A fresh guard for `conversation`, logging to `.bhai/debug/cache.jsonl`.
fn guard(conversation: &str, strict: bool) -> Arc<Mutex<CacheGuard>> {
    let log = crate::profile::debug_dir().join("cache.jsonl");
    Arc::new(Mutex::new(CacheGuard::new(conversation, Some(log), strict)))
}

/// The Responses API request body for one model call.
pub fn request_body(
    model: &str,
    effort: &str,
    cache_key: &str,
    instructions: &str,
    tools: &[Value],
    input: &[Value],
) -> Value {
    json!({
        "model": model,
        "instructions": instructions,
        "input": input,
        "tools": tools,
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": { "effort": effort, "summary": "auto" },
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": cache_key,
    })
}

/// The assistant's text across the output items of one call.
fn output_text(items: &[Value]) -> String {
    items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect()
}

pub(crate) enum Error {
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

/// Model and reasoning effort: the environment first, then bhai's own config, then
/// whatever `~/.codex/config.toml` has at top level. The last of those is deliberately
/// a line scan rather than a TOML dependency.
fn model_settings(choice: &Choice) -> (String, String) {
    let mut model = env("BHAI_MODEL").or_else(|| choice.model.clone());
    let mut effort = env("BHAI_EFFORT").or_else(|| choice.effort.clone());

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

/// An environment variable, where it is set to something.
fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
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
    fn a_character_split_across_chunks_is_decoded_whole() {
        let text = "data: {\"delta\":\"🙂\"}\n";
        let split = text.find('🙂').unwrap() + 2;
        let mut buf = text.as_bytes()[..split].to_vec();
        assert_eq!(take_line(&mut buf), None);
        buf.extend_from_slice(&text.as_bytes()[split..]);
        assert_eq!(
            take_line(&mut buf).as_deref(),
            Some("data: {\"delta\":\"🙂\"}")
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn an_ollama_call_says_how_much_context_it_wants() {
        let ollama = || {
            Client::new(&Choice::default())
                .unwrap()
                .with_overrides(Some("ollama:gemma4:e2b".to_string()), None)
        };
        // Unset, the server would apply its own 4096 and truncate the instructions.
        let assumed = ollama().body("be brief", &[], &[]);
        assert_eq!(assumed["options"]["num_ctx"], compact::OLLAMA_WINDOW);
        // The configured window wins, so compaction and the server agree on what fits.
        let configured = ollama().with_window(Some(8192)).body("be brief", &[], &[]);
        assert_eq!(configured["options"]["num_ctx"], 8192);
    }

    #[tokio::test]
    async fn a_watched_wait_gives_up_as_soon_as_the_turn_is_cancelled() {
        let cancel = Arc::new(AtomicBool::new(true));
        let waited = watched(std::future::pending::<()>(), &cancel, "idle").await;
        assert!(matches!(waited, Err(Error::Interrupted)));
    }

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
    fn a_child_has_its_own_cache_key_and_guard() {
        let parent = Client::new(&Choice::default()).unwrap().strict_cache(true);
        let identity = crate::identity::Identity {
            name: "reader".to_string(),
            ..crate::identity::Identity::default()
        };
        let child = parent.for_child(&identity);
        assert_ne!(child.cache_key, parent.cache_key);
        assert!(!Arc::ptr_eq(&child.guard, &parent.guard));
        assert!(child.guard.lock().unwrap().strict());
        let body = request_body("m", "e", "k", "i", &[], &[]);
        parent.guard.lock().unwrap().check(&body).unwrap();
        let other = request_body("m", "e", "k", "changed", &[], &[]);
        assert_eq!(child.guard.lock().unwrap().check(&other).unwrap(), None);
    }

    #[test]
    fn missing_usage_counts_as_zero() {
        let usage = Usage::from_completed(&json!({"type": "response.completed"}));
        assert_eq!(usage, Usage::default());
        assert_eq!(usage.cache_rate(), None);
    }
}
