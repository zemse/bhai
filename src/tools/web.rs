//! `web_search`: the ChatGPT backend's search, which Codex offers as `web.run`, on the
//! credentials the session already runs on. The endpoint (`/codex/alpha/search`) is
//! undocumented and may be closed to some plans; its shape follows nanocodex's client.
//! Each call sends the last two messages the user typed, and up to 4 KB of the answers
//! between them, along with the commands, so the search has the conversation's context.
//! It only reads, and the config is what turns it on, so a call needs no approval.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::{BoxFuture, Conversation, Live, MAX_OUTPUT, Tool, floor_boundary, truncate};
use crate::auth::{self, Auth};
use crate::client::{self, Provider};

pub const NAME: &str = "web_search";

/// What startup says when the tool is on, since turning it on is what consents to it.
pub const NOTICE: &str = "web_search: on; each search sends your last two messages to \
chatgpt.com's undocumented search endpoint";

const PATH: &str = "/alpha/search";
/// The commands passed through, each a list of objects. Codex also has `image_query`
/// and `screenshot`, whose images bhai has no way to show the model.
const COMMANDS: [&str; 8] = [
    "search_query",
    "open",
    "click",
    "find",
    "finance",
    "weather",
    "sports",
    "time",
];
const LENGTHS: [&str; 3] = ["short", "medium", "long"];
const MAX_QUERIES: usize = 4;
const TIMEOUT: Duration = Duration::from_secs(45);
const ATTEMPTS: usize = 2;
const RETRY_DELAY: Duration = Duration::from_millis(200);
const MAX_RESPONSE: usize = 1 << 20;
/// Asked of the search rather than cut afterwards, about what one tool result holds.
const MAX_OUTPUT_TOKENS: usize = MAX_OUTPUT / 4;
/// Bytes of assistant text sent as context, all messages together.
const ASSISTANT_CONTEXT: usize = 4_000;
const ERROR_PREVIEW: usize = 512;
const SUMMARY: usize = 200;
const TICK: Duration = Duration::from_millis(50);

const DESCRIPTION: &str = "Search the web and read pages, through the ChatGPT backend's \
search. Put one or more commands in a call; they run together.
- `search_query`: [{\"q\": \"...\"}], optionally with `recency` (days) and `domains`. At most \
4 queries; with more than 3, set `response_length` to medium or long.
- `open`: [{\"ref_id\": \"turn0search0\"}] or a URL as the ref_id, optionally with `lineno`.
- `click`: [{\"ref_id\": \"turn0fetch3\", \"id\": 17}], a numbered link on an opened page.
- `find`: [{\"ref_id\": \"turn0fetch3\", \"pattern\": \"...\"}], text on a page.
- `finance`: [{\"ticker\": \"AMD\", \"type\": \"equity\", \"market\": \"USA\"}], `weather`: \
[{\"location\": \"San Francisco, CA\"}], `sports`: [{\"fn\": \"standings\", \"league\": \
\"nfl\"}], `time`: [{\"utc_offset\": \"+03:00\"}].
Search when the answer may have changed since you were trained (versions, APIs of a library \
that moves, prices, news) or the user asks for sources, and rely on primary sources such as \
official docs. Results carry ids like `turn0search0`: use them in later calls only, and cite \
pages in your answer as Markdown links to the page itself. The user's last two messages go \
along as context. Runs without approval.";

pub struct WebSearch {
    endpoint: String,
}

impl WebSearch {
    /// The search under the Codex endpoints the session's model calls.
    pub fn codex() -> Self {
        Self {
            endpoint: format!("{}{PATH}", client::base_url()),
        }
    }
}

impl Tool for WebSearch {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        let list = |properties: Value, required: &[&str]| {
            json!({
                "type": "array",
                "items": {"type": "object", "properties": properties, "required": required}
            })
        };
        let string = json!({"type": "string"});
        let integer = json!({"type": "integer"});
        json!({
            "type": "function",
            "name": NAME,
            "description": DESCRIPTION,
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "search_query": list(json!({
                        "q": string,
                        "recency": {"type": "integer", "description": "Only results from this many recent days."},
                        "domains": {"type": "array", "items": string}
                    }), &["q"]),
                    "open": list(json!({"ref_id": string, "lineno": integer}), &["ref_id"]),
                    "click": list(json!({"ref_id": string, "id": integer}), &["ref_id", "id"]),
                    "find": list(json!({"ref_id": string, "pattern": string}), &["ref_id", "pattern"]),
                    "finance": list(json!({
                        "ticker": string,
                        "type": {"type": "string", "enum": ["equity", "fund", "crypto", "index"]},
                        "market": {"type": "string", "description": "ISO 3166-1 alpha-3 country code, \"OTC\", or \"\" for crypto."}
                    }), &["ticker", "type"]),
                    "weather": list(json!({
                        "location": string,
                        "start": {"type": "string", "description": "YYYY-MM-DD; defaults to today."},
                        "duration": {"type": "integer", "description": "Days; defaults to 7."}
                    }), &["location"]),
                    "sports": list(json!({
                        "fn": {"type": "string", "enum": ["schedule", "standings"]},
                        "league": {"type": "string", "enum": ["nba", "wnba", "nfl", "nhl", "mlb", "epl", "ncaamb", "ncaawb", "ipl"]},
                        "team": string,
                        "opponent": string,
                        "date_from": string,
                        "date_to": string,
                        "num_games": integer
                    }), &["fn", "league"]),
                    "time": list(json!({"utc_offset": string}), &["utc_offset"]),
                    "response_length": {"type": "string", "enum": LENGTHS}
                },
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        let commands = commands(args)?;
        Ok(summary(&commands))
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        static NEVER: AtomicBool = AtomicBool::new(false);
        let live = Live {
            progress: &|_| {},
            cancel: &NEVER,
            conversation: None,
        };
        self.execute_live(args, live)
    }

    fn execute_live<'a>(
        &'a self,
        args: &'a Value,
        live: Live<'a>,
    ) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let Some(conversation) = live.conversation else {
                return ("`web_search` runs only inside a turn.".to_string(), false);
            };
            if Provider::of(conversation.model) != Provider::Codex {
                let why = format!(
                    "`web_search` runs on the Codex backend, and `{}` is not a Codex model.",
                    conversation.model
                );
                return (why, false);
            }
            let commands = match commands(args) {
                Ok(commands) => commands,
                Err(e) => return (e, false),
            };
            let body = request_body(&conversation, commands);
            let search = tokio::time::timeout(TIMEOUT, self.search(&body));
            tokio::pin!(search);
            let mut tick = tokio::time::interval(TICK);
            loop {
                tokio::select! {
                    biased;
                    _ = tick.tick() => {
                        if live.cancel.load(Ordering::Relaxed) {
                            let why = "The user interrupted the turn; the search did not finish.";
                            return (why.to_string(), false);
                        }
                    }
                    out = &mut search => return match out {
                        Ok(Ok(text)) => (truncate(&text), true),
                        Ok(Err(e)) => (e, false),
                        Err(_) => (format!("the search timed out after {}s", TIMEOUT.as_secs()), false),
                    },
                }
            }
        })
    }
}

impl WebSearch {
    /// The search's text, after one retry of a failed send or a 5xx, and one fresh token
    /// after a 401.
    async fn search(&self, body: &Value) -> Result<String, String> {
        let http = http();
        let mut auth = auth::load(http).await.map_err(|e| format!("{e:#}"))?;
        let mut recovered = false;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let (status, bytes) = match post(http, &self.endpoint, &auth, body).await {
                Ok(answered) => answered,
                Err(failure) if failure.retry && attempt < ATTEMPTS => {
                    tokio::time::sleep(RETRY_DELAY).await;
                    continue;
                }
                Err(failure) => return Err(failure.message),
            };
            // A token revoked early is not near expiry, so `load` sent it.
            if status == reqwest::StatusCode::UNAUTHORIZED && !recovered {
                recovered = true;
                attempt -= 1;
                auth = auth::recover(http, &auth)
                    .await
                    .map_err(|e| format!("{e:#}"))?;
                continue;
            }
            if status.is_server_error() && attempt < ATTEMPTS {
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
            if !status.is_success() {
                return Err(refused(status, &bytes));
            }
            return answer(&bytes);
        }
    }
}

/// One client for every search, so its connections are reused.
fn http() -> &'static reqwest::Client {
    static HTTP: OnceLock<reqwest::Client> = OnceLock::new();
    HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_default()
    })
}

struct Failure {
    message: String,
    retry: bool,
}

async fn post(
    http: &reqwest::Client,
    endpoint: &str,
    auth: &Auth,
    body: &Value,
) -> Result<(reqwest::StatusCode, Vec<u8>), Failure> {
    let mut request = http
        .post(endpoint)
        .bearer_auth(&auth.access_token)
        .header("Accept", "application/json")
        .header("originator", client::ORIGINATOR)
        .header(
            "User-Agent",
            concat!("bhai/", env!("CARGO_PKG_VERSION"), " (codex_cli_rs)"),
        )
        .json(body);
    if let Some(account_id) = &auth.account_id {
        request = request.header("ChatGPT-Account-ID", account_id);
    }
    let mut response = request.send().await.map_err(|e| Failure {
        message: format!("the search request failed: {e}"),
        retry: true,
    })?;
    let status = response.status();
    let too_large = || Failure {
        message: format!("the search answered with more than {MAX_RESPONSE} bytes"),
        retry: false,
    };
    if response
        .content_length()
        .is_some_and(|n| n > MAX_RESPONSE as u64)
    {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if bytes.len() + chunk.len() > MAX_RESPONSE {
                    return Err(too_large());
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok((status, bytes)),
            Err(e) => {
                return Err(Failure {
                    message: format!("could not read the search's answer: {e}"),
                    retry: true,
                });
            }
        }
    }
}

/// What a refusal says, with the start of its body.
fn refused(status: reqwest::StatusCode, bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let end = floor_boundary(&text, ERROR_PREVIEW.min(text.len()));
    let more = if end < text.len() { "..." } else { "" };
    let hint = match status.as_u16() {
        403 | 404 => " The search endpoint is undocumented and may not be open to this plan.",
        _ => "",
    };
    format!(
        "the search answered {status}: {}{more}{hint}",
        text[..end].trim()
    )
}

/// The `output` of a search's answer: the text the model reads. `results` repeats its
/// sources in a structured form, which nothing here shows.
fn answer(bytes: &[u8]) -> Result<String, String> {
    let parsed: Value = serde_json::from_slice(bytes)
        .map_err(|e| format!("the search's answer was not JSON: {e}"))?;
    match parsed.get("output").and_then(Value::as_str) {
        Some("") => Ok("The search found nothing.".to_string()),
        Some(text) => Ok(text.to_string()),
        None => Err("the search's answer has no `output` text.".to_string()),
    }
}

/// The commands in `args`, checked: each a list of objects, empty and null ones dropped.
fn commands(args: &Value) -> Result<Map<String, Value>, String> {
    let Some(given) = args.as_object() else {
        return Err("arguments must be a JSON object of commands.".to_string());
    };
    let mut commands = Map::new();
    for (key, value) in given {
        if value.is_null() {
            continue;
        }
        if key == "response_length" {
            match value.as_str() {
                Some(length) if LENGTHS.contains(&length) => {
                    commands.insert(key.clone(), value.clone());
                }
                _ => return Err("`response_length` is one of short, medium, long.".to_string()),
            }
            continue;
        }
        if !COMMANDS.contains(&key.as_str()) {
            return Err(format!(
                "unknown command `{key}`; the commands are {}.",
                COMMANDS.join(", ")
            ));
        }
        match value.as_array() {
            Some(list) if list.iter().all(Value::is_object) => {
                if !list.is_empty() {
                    commands.insert(key.clone(), value.clone());
                }
            }
            _ => return Err(format!("`{key}` must be a list of objects.")),
        }
    }
    if !commands.keys().any(|k| COMMANDS.contains(&k.as_str())) {
        return Err(
            "give at least one command, such as {\"search_query\": [{\"q\": \"...\"}]}."
                .to_string(),
        );
    }
    let queries = commands.get("search_query").and_then(Value::as_array);
    if queries.is_some_and(|q| q.len() > MAX_QUERIES) {
        return Err(format!(
            "`search_query` takes at most {MAX_QUERIES} queries in one call."
        ));
    }
    Ok(commands)
}

/// One line for the user: what each command looks for.
fn summary(commands: &Map<String, Value>) -> String {
    let mut parts = Vec::new();
    for command in COMMANDS {
        let Some(list) = commands.get(command).and_then(Value::as_array) else {
            continue;
        };
        let (field, quoted) = match command {
            "search_query" => ("q", true),
            "find" => ("pattern", true),
            "finance" => ("ticker", false),
            "weather" => ("location", false),
            "sports" => ("league", false),
            "time" => ("utc_offset", false),
            _ => ("ref_id", false),
        };
        let each: Vec<String> = list
            .iter()
            .map(|entry| {
                let text = entry[field].as_str().unwrap_or("?");
                match quoted {
                    true => format!("{text:?}"),
                    false => text.to_string(),
                }
            })
            .collect();
        let name = match command {
            "search_query" => "search",
            other => other,
        };
        parts.push(format!("{name} {}", each.join(", ")));
    }
    let mut line = format!("web_search: {}", parts.join("; "));
    if line.len() > SUMMARY {
        line.truncate(floor_boundary(&line, SUMMARY));
        line.push_str("...");
    }
    line
}

fn request_body(conversation: &Conversation<'_>, commands: Map<String, Value>) -> Value {
    let mut body = json!({
        "id": conversation.id,
        "model": conversation.model,
        "commands": commands,
        "settings": {"allowed_callers": ["direct"], "external_web_access": true},
        "max_output_tokens": MAX_OUTPUT_TOKENS,
    });
    if let Some(input) = recent_input(conversation.history) {
        body["input"] = Value::Array(input);
    }
    body
}

/// The context a search is sent: from the second-to-last message the user typed through
/// the last, with the answers between them cut to [`ASSISTANT_CONTEXT`] bytes. Tool calls,
/// commentary and what bhai writes in the user's place are left out, and only text goes.
fn recent_input(history: &[Value]) -> Option<Vec<Value>> {
    let mut messages: Vec<(bool, String)> = history.iter().filter_map(visible).collect();
    let latest = messages.iter().rposition(|(user, _)| *user)?;
    messages.truncate(latest + 1);
    let first = messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, (user, _))| *user)
        .nth(1)
        .map_or(latest, |(index, _)| index);
    messages.drain(..first);
    let mut left = ASSISTANT_CONTEXT;
    let mut input = Vec::new();
    for (user, text) in messages {
        if user {
            input.push(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": text}],
            }));
            continue;
        }
        if left == 0 {
            continue;
        }
        let text = cut_middle(&text, left);
        left = left.saturating_sub(text.len());
        input.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}],
        }));
    }
    Some(input)
}

/// A message's text, and whether the user typed it, for the messages a search may see.
fn visible(item: &Value) -> Option<(bool, String)> {
    if item["type"] != "message" {
        return None;
    }
    let (user, part) = match item["role"].as_str()? {
        "user" => (true, "input_text"),
        "assistant" if !client::is_commentary(item) => (false, "output_text"),
        _ => return None,
    };
    let text = match &item["content"] {
        Value::String(text) => text.clone(),
        content => content
            .as_array()?
            .iter()
            .filter(|p| p["type"] == part)
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    };
    let shown = match user {
        true => {
            super::history::typed(&text) && !text.trim_start().starts_with(crate::environment::OPEN)
        }
        false => !text.is_empty(),
    };
    shown.then_some((user, text))
}

/// `text` within `max` bytes, its middle replaced by a note of how much went.
fn cut_middle(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let note = |cut: usize| format!("\n[... {cut} bytes cut ...]\n");
    let room = max.saturating_sub(note(text.len()).len());
    let head = floor_boundary(text, room / 2);
    let mut tail = text.len() - (room - room / 2);
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}{}{}", &text[..head], note(tail - head), &text[tail..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Value {
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})
    }

    fn assistant(text: &str) -> Value {
        json!({"type": "message", "id": "msg_1", "role": "assistant", "content": [{"type": "output_text", "text": text, "annotations": []}]})
    }

    fn conversation(history: &[Value]) -> Conversation<'_> {
        Conversation {
            id: "session-1",
            model: "gpt-5.5",
            history,
        }
    }

    #[test]
    fn the_context_is_the_last_two_typed_messages_and_the_answers_between() {
        let mut commentary = assistant("looking it up");
        commentary["phase"] = json!("commentary");
        let history = [
            user("first question"),
            assistant("first answer"),
            user(&format!(
                "{}\n\ndate: today\n</environment_context>",
                crate::environment::OPEN
            )),
            user("second question"),
            json!({"type": "function_call", "call_id": "c1", "name": "bash", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "c1", "output": "TOOL OUTPUT"}),
            assistant("second answer"),
            user(crate::agent::TURN_ABORTED),
            json!({"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "third question"},
                {"type": "input_image", "image_url": "data:image/png;base64,a"}
            ]}),
            commentary,
        ];
        let input = recent_input(&history).unwrap();
        assert_eq!(
            Value::Array(input),
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "second question"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "second answer"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "third question"}]},
            ])
        );
        assert!(recent_input(&[assistant("only an answer")]).is_none());
        assert_eq!(recent_input(&[user("alone")]).unwrap().len(), 1);
    }

    #[test]
    fn answers_sent_as_context_stay_within_their_budget() {
        let long = "a".repeat(ASSISTANT_CONTEXT * 2);
        let history = [
            user("one"),
            assistant(&long),
            assistant("never reached"),
            user("two"),
        ];
        let input = recent_input(&history).unwrap();
        assert_eq!(input.len(), 3, "{input:?}");
        let text = input[1]["content"][0]["text"].as_str().unwrap();
        assert!(text.len() <= ASSISTANT_CONTEXT, "{}", text.len());
        assert!(text.contains("bytes cut"), "{text}");
        assert_eq!(input[2]["content"][0]["text"], "two");
        assert_eq!(cut_middle("short", 100), "short");
        let cut = cut_middle(&"é".repeat(5_000), 300);
        assert!(cut.len() <= 300 && cut.starts_with('é') && cut.ends_with('é'));
    }

    #[test]
    fn the_request_carries_the_conversation_the_commands_and_the_settings() {
        let history = [user("what changed in tokio 1.50")];
        let commands =
            commands(&json!({"search_query": [{"q": "tokio 1.50"}], "open": null})).unwrap();
        let body = request_body(&conversation(&history), commands);
        assert_eq!(
            body,
            json!({
                "id": "session-1",
                "model": "gpt-5.5",
                "commands": {"search_query": [{"q": "tokio 1.50"}]},
                "settings": {"allowed_callers": ["direct"], "external_web_access": true},
                "max_output_tokens": MAX_OUTPUT_TOKENS,
                "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "what changed in tokio 1.50"}]}],
            })
        );
        let body = request_body(&conversation(&[]), Map::new());
        assert!(body.get("input").is_none(), "{body}");
    }

    #[test]
    fn commands_are_checked_before_anything_is_sent() {
        let tool = WebSearch::codex();
        assert!(!tool.needs_approval());
        let line = tool
            .describe(&json!({
                "search_query": [{"q": "rust 1.95"}, {"q": "edition 2024"}],
                "find": [{"ref_id": "turn0search0", "pattern": "let chains"}],
                "response_length": "medium"
            }))
            .unwrap();
        assert_eq!(
            line,
            "web_search: search \"rust 1.95\", \"edition 2024\"; find \"let chains\""
        );
        for (args, says) in [
            (json!({}), "at least one command"),
            (json!({"response_length": "long"}), "at least one command"),
            (json!({"search_query": []}), "at least one command"),
            (
                json!({"image_query": [{"q": "x"}]}),
                "unknown command `image_query`",
            ),
            (json!({"open": "https://example.com"}), "list of objects"),
            (
                json!({"open": [{"ref_id": "a"}], "response_length": "huge"}),
                "short, medium, long",
            ),
            (
                json!({"search_query": [{"q": "1"}, {"q": "2"}, {"q": "3"}, {"q": "4"}, {"q": "5"}]}),
                "at most 4",
            ),
        ] {
            let err = tool.describe(&args).unwrap_err();
            assert!(err.contains(says), "{args}: {err}");
        }
        let long = json!({"search_query": [{"q": "x".repeat(400)}]});
        assert!(tool.describe(&long).unwrap().len() <= SUMMARY + 3);
    }

    #[test]
    fn the_answer_is_its_output_text() {
        let body = json!({
            "encrypted_output": "ciphertext",
            "output": "Rust 1.95 shipped on ... 【turn0search0】",
            "results": [{"type": "text_result", "ref_id": "turn0search0", "url": "https://blog.rust-lang.org"}]
        });
        assert_eq!(
            answer(body.to_string().as_bytes()).unwrap(),
            "Rust 1.95 shipped on ... 【turn0search0】"
        );
        assert_eq!(
            answer(br#"{"output": ""}"#).unwrap(),
            "The search found nothing."
        );
        assert!(
            answer(br#"{"results": []}"#)
                .unwrap_err()
                .contains("no `output`")
        );
        assert!(answer(b"<html>").unwrap_err().contains("not JSON"));
        let refusal = refused(reqwest::StatusCode::FORBIDDEN, "x".repeat(2_000).as_bytes());
        assert!(
            refusal.starts_with("the search answered 403 Forbidden: xxx"),
            "{refusal}"
        );
        assert!(refusal.contains("...") && refusal.contains("may not be open to this plan"));
        assert!(refusal.len() < 700, "{}", refusal.len());
    }

    #[tokio::test]
    async fn a_call_outside_a_turn_or_on_ollama_sends_nothing() {
        let tool = WebSearch {
            endpoint: "http://127.0.0.1:9/never".to_string(),
        };
        let args = json!({"search_query": [{"q": "x"}]});
        let (out, ok) = tool.execute(&args).await;
        assert!(!ok && out.contains("only inside a turn"), "{out}");

        static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);
        let live = Live {
            progress: &|_| {},
            cancel: &NOT_CANCELLED,
            conversation: Some(Conversation {
                id: "s",
                model: "ollama:qwen3",
                history: &[],
            }),
        };
        let (out, ok) = tool.execute_live(&args, live).await;
        assert!(!ok && out.contains("not a Codex model"), "{out}");
    }

    /// A local server that answers every request with `status` and `body`.
    async fn serve(status: u16, body: String) -> String {
        let app = axum::Router::new().route(
            "/search",
            axum::routing::post(move || async move {
                (axum::http::StatusCode::from_u16(status).unwrap(), body)
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        format!("http://{addr}/search")
    }

    #[tokio::test]
    async fn a_post_reads_the_status_and_body_and_refuses_a_huge_answer() {
        let auth = Auth {
            access_token: "token".to_string(),
            account_id: Some("acct".to_string()),
        };
        let body = json!({});
        let endpoint = serve(502, "bad gateway".to_string()).await;
        let (status, bytes) = match post(http(), &endpoint, &auth, &body).await {
            Ok(answered) => answered,
            Err(f) => panic!("{}", f.message),
        };
        assert_eq!(status, 502);
        assert_eq!(bytes, b"bad gateway");

        let endpoint = serve(200, "x".repeat(MAX_RESPONSE + 1)).await;
        match post(http(), &endpoint, &auth, &body).await {
            Ok(_) => panic!("a body over the cap was read"),
            Err(f) => assert!(!f.retry && f.message.contains("more than"), "{}", f.message),
        }
    }
}
