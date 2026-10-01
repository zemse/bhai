//! The real binary, `--serve --headless`, on a fake Responses backend: what a turn sends
//! and what `/events` fans out, with no model call and no quota.
//!
//! `BHAI_TEST_BASE_URL` only moves the backend in a debug build, so these only run there.
#![cfg(debug_assertions)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(20);
/// A JWT whose `exp` is 2100-01-01, so auth never tries to refresh it.
const ACCESS_TOKEN: &str = "e30.eyJleHAiOjQxMDI0NDQ4MDB9.c2ln";
const ACCOUNT: &str = "acct-fake";

/// One request the fake answered.
#[derive(Debug, Clone)]
struct Seen {
    path: &'static str,
    headers: HeaderMap,
    body: Value,
}

/// The fake: replies to `/codex/responses` in the order queued, and keeps every request.
#[derive(Default)]
struct Fake {
    replies: Mutex<VecDeque<String>>,
    /// The `x-codex-turn-state` each reply carries, in the same order; none past the end.
    turn_states: Mutex<VecDeque<&'static str>>,
    seen: Mutex<Vec<Seen>>,
}

impl Fake {
    fn responses(&self) -> Vec<Seen> {
        let seen = self.seen.lock().unwrap();
        seen.iter()
            .filter(|s| s.path == "/codex/responses")
            .cloned()
            .collect()
    }
}

async fn responses(State(fake): State<Arc<Fake>>, headers: HeaderMap, body: String) -> Response {
    let body = serde_json::from_str(&body).unwrap_or(Value::Null);
    let cwd = working_directory(&body);
    fake.seen.lock().unwrap().push(Seen {
        path: "/codex/responses",
        headers,
        body,
    });
    let turn_state = fake.turn_states.lock().unwrap().pop_front();
    match fake.replies.lock().unwrap().pop_front() {
        Some(sse) => {
            let sse = sse.replace(CWD, &cwd);
            let mut response = ([(header::CONTENT_TYPE, "text/event-stream")], sse).into_response();
            if let Some(state) = turn_state.filter(|s| !s.is_empty()) {
                let value = header::HeaderValue::from_static(state);
                response.headers_mut().insert("x-codex-turn-state", value);
            }
            response
        }
        None => (StatusCode::BAD_REQUEST, "the fake has no reply queued").into_response(),
    }
}

/// Stands in for the working directory in a queued reply, which the fake fills in from
/// the request, since a write takes an absolute path.
const CWD: &str = "{cwd}";

/// The `Working directory:` line of the system prompt in `body`.
fn working_directory(body: &Value) -> String {
    let text = body["instructions"].as_str().unwrap_or_default();
    text.lines()
        .find_map(|line| line.trim().strip_prefix("- Working directory: "))
        .unwrap_or_default()
        .to_string()
}

async fn usage(State(fake): State<Arc<Fake>>, headers: HeaderMap) -> Response {
    fake.seen.lock().unwrap().push(Seen {
        path: "/wham/usage",
        headers,
        body: Value::Null,
    });
    axum::Json(json!({})).into_response()
}

async fn serve_fake(fake: Arc<Fake>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/codex/responses", post(responses))
        .route("/wham/usage", get(usage))
        .with_state(fake);
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}")
}

/// A Responses stream of `events`, each framed the way the backend frames it.
fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

fn completed() -> Value {
    json!({
        "type": "response.completed",
        "response": { "output": [], "usage": { "input_tokens": 120, "output_tokens": 7 } }
    })
}

/// The model saying `text`, in two deltas.
fn says(text: &str) -> String {
    let (head, tail) = text.split_at(text.len() / 2);
    sse(&[
        json!({ "type": "response.output_text.delta", "delta": head }),
        json!({ "type": "response.output_text.delta", "delta": tail }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": text }]
            }
        }),
        completed(),
    ])
}

/// The model narrating `text` in the commentary phase, then saying the turn is not over.
fn narrates(text: &str) -> String {
    let item = json!({
        "type": "message",
        "id": "msg_pre",
        "role": "assistant",
        "phase": "commentary",
        "content": [{ "type": "output_text", "text": text }]
    });
    let mut added = item.clone();
    added["content"] = json!([]);
    let mut done = completed();
    done["response"]["end_turn"] = json!(false);
    sse(&[
        json!({ "type": "response.output_item.added", "item": added }),
        json!({ "type": "response.output_text.delta", "item_id": "msg_pre", "delta": text }),
        json!({ "type": "response.output_item.done", "item": item }),
        done,
    ])
}

/// The model asking for `command` in bash.
fn runs(call_id: &str, command: &str) -> String {
    sse(&[
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "name": "bash",
                "call_id": call_id,
                "arguments": json!({ "command": command }).to_string()
            }
        }),
        completed(),
    ])
}

/// The model asking to write `content` to `path`.
fn writes(call_id: &str, path: &str, content: &str) -> String {
    sse(&[
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "name": "write",
                "call_id": call_id,
                "arguments": json!({ "path": path, "content": content }).to_string()
            }
        }),
        completed(),
    ])
}

/// A `home` and a `codex` home under `dir`, the latter holding a login the fake accepts.
fn logged_in(dir: &std::path::Path) -> (PathBuf, PathBuf) {
    let (home, codex) = (dir.join("home"), dir.join("codex"));
    for d in [&home, &codex] {
        std::fs::create_dir_all(d).unwrap();
    }
    let auth = json!({ "tokens": { "access_token": ACCESS_TOKEN, "account_id": ACCOUNT } });
    std::fs::write(codex.join("auth.json"), auth.to_string()).unwrap();
    (home, codex)
}

/// A bhai `--serve 0 --headless` in a scratch home and project, killed on drop.
struct Bhai {
    child: Child,
    base: String,
    token: String,
    http: reqwest::Client,
    dir: PathBuf,
    /// What it wrote to stderr before the server line.
    said: String,
}

impl Drop for Bhai {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Bhai {
    async fn start(backend: &str) -> Bhai {
        Self::start_with(backend, &[]).await
    }

    async fn start_with(backend: &str, env: &[(&str, &str)]) -> Bhai {
        let dir = std::env::temp_dir().join(format!("bhai-fake-{}", uuid::Uuid::new_v4()));
        let (home, codex) = logged_in(&dir);
        let project = dir.join("project");
        std::fs::create_dir_all(&project).unwrap();

        let mut child = Command::new(env!("CARGO_BIN_EXE_bhai"))
            .args(["--serve", "0", "--headless"])
            .current_dir(&project)
            .env("HOME", &home)
            .env("CODEX_HOME", &codex)
            .env("BHAI_TEST_BASE_URL", backend)
            .env_remove("BHAI_MODEL")
            .env_remove("BHAI_MODE")
            .env_remove("BHAI_EFFORT")
            .env_remove("BHAI_STARTUP_TIMING")
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        let mut said = String::new();
        let found = timeout(WAIT, async {
            while let Some(line) = lines.next_line().await.unwrap() {
                if let Some(rest) = line.strip_prefix("bhai: debug server on ") {
                    return Some(rest.to_string());
                }
                said.push_str(&line);
                said.push('\n');
            }
            None
        })
        .await
        .ok()
        .flatten();
        let Some(rest) = found else {
            panic!("bhai did not start its server:\n{said}");
        };
        // `http://127.0.0.1:PORT (x-bhai-token: TOKEN)`
        let (base, token) = rest.split_once(" (x-bhai-token: ").unwrap();
        let token = token.trim_end_matches(')').to_string();
        // The rest of stderr is drained so a full pipe cannot block the binary.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

        Bhai {
            child,
            base: base.to_string(),
            token,
            http: reqwest::Client::new(),
            dir,
            said,
        }
    }

    fn project(&self) -> PathBuf {
        self.dir.join("project")
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .header("x-bhai-token", &self.token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn state(&self) -> Value {
        self.http
            .get(format!("{}/state", self.base))
            .header("x-bhai-token", &self.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// `/events` from the first event on.
    async fn events(&self) -> Events {
        let response = self
            .http
            .get(format!("{}/events", self.base))
            .header("x-bhai-token", &self.token)
            .header("last-event-id", "0")
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", response.status());
        Events {
            stream: response.bytes_stream().boxed(),
            buf: String::new(),
            got: Vec::new(),
        }
    }
}

/// What `/events` sent, read as far as asked.
struct Events {
    stream: BoxStream<'static, reqwest::Result<Bytes>>,
    buf: String,
    got: Vec<Value>,
}

impl Events {
    /// Read up to and including the first event of type `kind`, and return it.
    async fn until(&mut self, kind: &str) -> Value {
        let read = timeout(WAIT, async {
            loop {
                while let Some(end) = self.buf.find("\n\n") {
                    let frame: String = self.buf.drain(..end + 2).collect();
                    let Some(data) = frame.lines().find_map(|l| l.strip_prefix("data: ")) else {
                        continue;
                    };
                    let event: Value = serde_json::from_str(data).unwrap();
                    self.got.push(event.clone());
                    if event["type"] == kind {
                        return event;
                    }
                }
                let chunk = self.stream.next().await.expect("/events ended").unwrap();
                self.buf.push_str(&String::from_utf8_lossy(&chunk));
            }
        })
        .await;
        match read {
            Ok(event) => event,
            Err(_) => panic!("no {kind} event; got {:#?}", self.got),
        }
    }

    fn kinds(&self) -> Vec<&str> {
        self.got.iter().filter_map(|e| e["type"].as_str()).collect()
    }

    fn text(&self) -> String {
        self.got
            .iter()
            .filter(|e| e["type"] == "text")
            .filter_map(|e| e["data"].as_str())
            .collect()
    }
}

/// The input items of a request that are `kind`.
fn items<'a>(body: &'a Value, kind: &str) -> Vec<&'a Value> {
    body["input"]
        .as_array()
        .map(|input| input.iter().filter(|i| i["type"] == kind).collect())
        .unwrap_or_default()
}

fn mentions(body: &Value, text: &str) -> bool {
    body["input"].to_string().contains(text)
}

#[tokio::test]
async fn a_prompt_reaches_the_backend_and_its_answer_reaches_events() {
    let fake = Arc::new(Fake::default());
    fake.replies
        .lock()
        .unwrap()
        .push_back(says("ok from the fake"));
    let bhai = Bhai::start(&serve_fake(fake.clone()).await).await;
    let mut events = bhai.events().await;

    let (status, answer) = bhai.post("/prompt", json!({ "text": "say ok" })).await;
    assert_eq!(status, StatusCode::OK, "{answer}");

    let user = events.until("user").await;
    assert_eq!(user["data"], "say ok");
    events.until("turn_end").await;
    assert_eq!(events.text(), "ok from the fake");
    let kinds = events.kinds();
    for kind in ["streaming", "usage", "done"] {
        assert!(kinds.contains(&kind), "no {kind} in {kinds:?}");
    }
    let usage = events.got.iter().find(|e| e["type"] == "usage").unwrap();
    assert_eq!(usage["data"]["input"], 120, "{usage}");
    assert_eq!(usage["data"]["output"], 7, "{usage}");

    let sent = fake.responses();
    assert_eq!(sent.len(), 1, "{sent:#?}");
    let request = &sent[0];
    assert_eq!(
        request.headers["authorization"],
        format!("Bearer {ACCESS_TOKEN}").as_str()
    );
    assert_eq!(request.headers["chatgpt-account-id"], ACCOUNT);
    assert_eq!(request.body["stream"], true);
    assert!(mentions(&request.body, "say ok"), "{}", request.body);

    let state = bhai.state().await;
    assert_eq!(state["calls"], 1, "{state}");
    assert_eq!(state["input_tokens"], 120, "{state}");
    assert_eq!(state["output_tokens"], 7, "{state}");
}

#[tokio::test]
async fn startup_timing_names_each_stage_in_order_before_the_server_line() {
    let fake = Arc::new(Fake::default());
    let backend = serve_fake(fake).await;
    let bhai = Bhai::start_with(&backend, &[("BHAI_STARTUP_TIMING", "1")]).await;
    let stages: Vec<Value> = bhai
        .said
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    let names: Vec<&str> = stages.iter().filter_map(|s| s["stage"].as_str()).collect();
    assert_eq!(
        names,
        [
            "resume",
            "config",
            "mcp",
            "prompt",
            "client",
            "preflight",
            "session",
            "start"
        ],
        "{}",
        bhai.said
    );
    let total = |s: &Value| s["total_ms"].as_f64().unwrap();
    assert!(stages.windows(2).all(|w| total(&w[0]) <= total(&w[1])));
    assert!(stages.iter().all(|s| s["ms"].as_f64().unwrap() >= 0.0));

    let quiet = Bhai::start(&backend).await;
    assert!(!quiet.said.contains("\"stage\""), "{}", quiet.said);
}

#[tokio::test]
async fn a_tool_call_waits_for_approve_runs_and_its_output_goes_back() {
    let fake = Arc::new(Fake::default());
    {
        let mut replies = fake.replies.lock().unwrap();
        replies.push_back(runs("call_1", "touch made-by-the-fake"));
        replies.push_back(says("made it"));
    }
    let bhai = Bhai::start(&serve_fake(fake.clone()).await).await;
    let mut events = bhai.events().await;

    let (status, answer) = bhai.post("/prompt", json!({ "text": "make a file" })).await;
    assert_eq!(status, StatusCode::OK, "{answer}");

    let approval = events.until("approval").await;
    assert_eq!(approval["data"]["tool"], "bash", "{approval}");
    assert!(!bhai.project().join("made-by-the-fake").exists());
    let id = approval["data"]["id"].clone();
    let (status, answer) = bhai.post("/approve", json!({ "id": id })).await;
    assert_eq!(status, StatusCode::OK, "{answer}");

    let resolved = events.until("resolved").await;
    assert_eq!(resolved["data"]["accepted"], true, "{resolved}");
    events.until("tool_output").await;
    events.until("turn_end").await;
    assert_eq!(events.text(), "made it");
    assert!(bhai.project().join("made-by-the-fake").exists());

    let sent = fake.responses();
    assert_eq!(sent.len(), 2, "{sent:#?}");
    let calls = items(&sent[1].body, "function_call");
    assert!(calls.iter().any(|c| c["call_id"] == "call_1"), "{calls:?}");
    let outputs = items(&sent[1].body, "function_call_output");
    assert!(
        outputs.iter().any(|o| o["call_id"] == "call_1"),
        "{outputs:?}"
    );
}

#[tokio::test]
async fn a_rejected_call_sends_the_refusal_back_and_runs_nothing() {
    let fake = Arc::new(Fake::default());
    {
        let mut replies = fake.replies.lock().unwrap();
        replies.push_back(runs("call_9", "touch never-made"));
        replies.push_back(says("understood"));
    }
    let bhai = Bhai::start(&serve_fake(fake.clone()).await).await;
    let mut events = bhai.events().await;

    bhai.post("/prompt", json!({ "text": "make a file" })).await;
    let approval = events.until("approval").await;
    let (status, answer) = bhai
        .post("/reject", json!({ "id": approval["data"]["id"] }))
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");

    events.until("tool_rejected").await;
    events.until("turn_end").await;
    assert!(!bhai.project().join("never-made").exists());
    assert!(
        !events.kinds().contains(&"tool_start"),
        "{:?}",
        events.kinds()
    );

    let sent = fake.responses();
    assert_eq!(sent.len(), 2, "{sent:#?}");
    let outputs = items(&sent[1].body, "function_call_output");
    assert!(
        outputs.iter().any(|o| o["call_id"] == "call_9"),
        "{outputs:?}"
    );
}

#[tokio::test]
async fn a_commentary_preamble_on_end_turn_false_does_not_end_the_turn() {
    let fake = Arc::new(Fake::default());
    {
        let mut replies = fake.replies.lock().unwrap();
        replies.push_back(narrates("looking first"));
        replies.push_back(says("here it is"));
    }
    let bhai = Bhai::start(&serve_fake(fake.clone()).await).await;
    let mut events = bhai.events().await;

    let (status, answer) = bhai.post("/prompt", json!({ "text": "find it" })).await;
    assert_eq!(status, StatusCode::OK, "{answer}");

    events.until("turn_end").await;
    assert_eq!(events.text(), "here it is");
    let commentary: String = events
        .got
        .iter()
        .filter(|e| e["type"] == "commentary")
        .filter_map(|e| e["data"].as_str())
        .collect();
    assert_eq!(commentary, "looking first");

    let sent = fake.responses();
    assert_eq!(sent.len(), 2, "{sent:#?}");
    // The preamble goes back as it came, phase and all.
    let messages = items(&sent[1].body, "message");
    assert!(
        messages
            .iter()
            .any(|m| m["phase"] == "commentary" && m.to_string().contains("looking first")),
        "{messages:?}"
    );
}

#[tokio::test]
async fn a_turn_sends_back_its_first_routing_token_and_the_next_turn_starts_without() {
    let fake = Arc::new(Fake::default());
    {
        let mut replies = fake.replies.lock().unwrap();
        replies.push_back(narrates("looking first"));
        replies.push_back(says("here it is"));
        replies.push_back(says("again"));
        let mut states = fake.turn_states.lock().unwrap();
        states.extend(["sticky-1", "sticky-other", "sticky-2"]);
    }
    let bhai = Bhai::start(&serve_fake(fake.clone()).await).await;
    let mut events = bhai.events().await;

    bhai.post("/prompt", json!({ "text": "find it" })).await;
    events.until("turn_end").await;
    bhai.post("/prompt", json!({ "text": "once more" })).await;
    events.until("turn_end").await;

    let sent = fake.responses();
    assert_eq!(sent.len(), 3, "{sent:#?}");
    let carried: Vec<Option<&str>> = sent
        .iter()
        .map(|s| {
            s.headers
                .get("x-codex-turn-state")
                .map(|v| v.to_str().unwrap())
        })
        .collect();
    assert_eq!(carried, [None, Some("sticky-1"), None]);
    // A header, not a body field: the body is the cached prefix.
    assert!(!sent[1].body.to_string().contains("sticky-1"));
}

/// A stream that fails in band with `code`.
fn fails(code: &str) -> String {
    sse(&[json!({
        "type": "response.failed",
        "response": { "error": { "code": code, "message": format!("failed: {code}") } }
    })])
}

#[tokio::test]
async fn a_context_overflow_is_not_sent_again() {
    let fake = Arc::new(Fake::default());
    fake.replies
        .lock()
        .unwrap()
        .push_back(fails("context_length_exceeded"));
    let bhai = Bhai::start(&serve_fake(fake.clone()).await).await;
    let mut events = bhai.events().await;

    let (status, answer) = bhai.post("/prompt", json!({ "text": "go" })).await;
    assert_eq!(status, StatusCode::OK, "{answer}");

    let failed = events.until("turn_failed").await;
    assert_eq!(
        failed["data"], "failed: context_length_exceeded",
        "{failed}"
    );
    // A first turn has nothing earlier to fold, so there is nothing to send again.
    assert_eq!(fake.responses().len(), 1);
    let infos: Vec<&Value> = events.got.iter().filter(|e| e["type"] == "info").collect();
    assert_eq!(infos.len(), 1, "{:#?}", events.got);
    assert!(
        infos[0]["data"]
            .as_str()
            .is_some_and(|m| m.starts_with("nothing to compact")),
        "{:#?}",
        events.got
    );
}

#[tokio::test]
async fn a_context_overflow_compacts_and_the_turn_runs_again() {
    let fake = Arc::new(Fake::default());
    {
        let mut replies = fake.replies.lock().unwrap();
        replies.push_back(says("one"));
        replies.push_back(fails("context_length_exceeded"));
        replies.push_back(says("the summary"));
        replies.push_back(says("two"));
    }
    let bhai = Bhai::start(&serve_fake(fake.clone()).await).await;
    let mut events = bhai.events().await;
    bhai.post("/prompt", json!({ "text": "first" })).await;
    events.until("turn_end").await;

    bhai.post("/prompt", json!({ "text": "second" })).await;
    events.until("turn_end").await;
    assert!(
        !events.kinds().contains(&"turn_failed"),
        "{:#?}",
        events.got
    );
    assert!(events.kinds().contains(&"compacted"), "{:#?}", events.got);
    assert_eq!(events.text(), "onetwo");
    let sent = fake.responses();
    assert_eq!(sent.len(), 4);
    let retried = &sent[3].body;
    assert!(mentions(
        retried,
        "Summary of earlier conversation:\\nthe summary"
    ));
    assert!(mentions(retried, "second"));
    assert!(!mentions(retried, "\"one\""));
}

/// The backend answering a `compaction_trigger` with its opaque item.
fn compacts(encrypted: &str) -> String {
    sse(&[
        json!({
            "type": "response.output_item.done",
            "item": { "type": "compaction", "id": "cmp_1", "encrypted_content": encrypted }
        }),
        completed(),
    ])
}

#[tokio::test]
async fn a_gpt6_overflow_compacts_on_the_backend_and_the_turn_runs_again() {
    let fake = Arc::new(Fake::default());
    {
        let mut replies = fake.replies.lock().unwrap();
        replies.push_back(says("one"));
        replies.push_back(fails("context_length_exceeded"));
        replies.push_back(compacts("opaque-state"));
        replies.push_back(says("two"));
    }
    let backend = serve_fake(fake.clone()).await;
    let bhai = Bhai::start_with(&backend, &[("BHAI_MODEL", "gpt-6-sol")]).await;
    let mut events = bhai.events().await;
    bhai.post("/prompt", json!({ "text": "first" })).await;
    events.until("turn_end").await;

    bhai.post("/prompt", json!({ "text": "second" })).await;
    events.until("turn_end").await;
    assert!(
        !events.kinds().contains(&"turn_failed"),
        "{:#?}",
        events.got
    );
    assert!(events.kinds().contains(&"compacted"), "{:#?}", events.got);
    assert_eq!(events.text(), "onetwo");

    let sent = fake.responses();
    assert_eq!(sent.len(), 4, "{sent:#?}");
    assert_eq!(sent[0].body["model"], "gpt-6-sol");
    let asked = sent[2].body["input"].as_array().unwrap();
    assert_eq!(asked.last(), Some(&json!({ "type": "compaction_trigger" })));
    let retried = &sent[3].body;
    let kept = items(retried, "compaction");
    assert_eq!(kept.len(), 1, "{retried}");
    assert_eq!(kept[0]["encrypted_content"], "opaque-state");
    assert!(mentions(retried, "first") && mentions(retried, "second"));
    assert!(!mentions(retried, "\"one\""));
    assert!(items(retried, "compaction_trigger").is_empty());
}

#[tokio::test]
async fn a_transient_failure_is_retried_and_says_so() {
    let fake = Arc::new(Fake::default());
    {
        let mut replies = fake.replies.lock().unwrap();
        // The failed attempt streams some text first, which the retry takes back.
        let partial = sse(&[json!({ "type": "response.output_text.delta", "delta": "first ti" })]);
        replies.push_back(partial + &fails("server_is_overloaded"));
        replies.push_back(says("second time"));
    }
    let bhai = Bhai::start(&serve_fake(fake.clone()).await).await;
    let mut events = bhai.events().await;

    let (status, answer) = bhai.post("/prompt", json!({ "text": "go" })).await;
    assert_eq!(status, StatusCode::OK, "{answer}");

    let retrying = events.until("retrying").await;
    let said = retrying["data"].as_str().unwrap_or_default();
    assert!(said.starts_with("retrying (2/3) in "), "{retrying}");
    assert!(said.contains("failed: server_is_overloaded"), "{retrying}");
    assert_eq!(events.text(), "first ti");
    events.until("turn_end").await;
    assert_eq!(events.text(), "first tisecond time");
    assert_eq!(fake.responses().len(), 2);
}

/// `evals/run.py` running the real `bhai exec` on the fake, through a write: the runner's
/// fresh workspace is an untrusted project, so this fails unless the runner trusts it.
#[tokio::test]
async fn the_eval_runner_trusts_its_workspace_so_bhai_exec_can_write() {
    let found = std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        eprintln!("skipped: python3 is not available");
        return;
    }
    let fake = Arc::new(Fake::default());
    {
        let mut replies = fake.replies.lock().unwrap();
        let hello = format!("{CWD}/hello.txt");
        replies.push_back(writes("call_w", &hello, "Hello, world!\n"));
        replies.push_back(says("wrote it"));
    }
    let backend = serve_fake(fake.clone()).await;
    let dir = std::env::temp_dir().join(format!("bhai-fake-eval-{}", uuid::Uuid::new_v4()));
    let (home, codex) = logged_in(&dir);
    let out = dir.join("out");
    let ran = timeout(
        WAIT,
        Command::new("python3")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/evals/run.py"))
            .args(["--agent", "bhai", "--bhai", env!("CARGO_BIN_EXE_bhai")])
            .arg("--out")
            .arg(&out)
            .arg("write-greeting")
            .env("HOME", &home)
            .env("CODEX_HOME", &codex)
            .env("BHAI_TEST_BASE_URL", &backend)
            .env_remove("BHAI_MODEL")
            .env_remove("BHAI_MODE")
            .env_remove("BHAI_EFFORT")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("run.py did not finish")
    .unwrap();
    let said = String::from_utf8_lossy(&ran.stderr).into_owned();
    assert!(ran.status.success(), "run.py exited {}: {said}", ran.status);

    let results = std::fs::read_to_string(out.join("results.jsonl")).unwrap();
    let r: Value = serde_json::from_str(results.trim()).unwrap();
    let log = std::fs::read_to_string(out.join("write-greeting.bhai.1.jsonl")).unwrap();
    let stderr = std::fs::read_to_string(out.join("write-greeting.bhai.1.stderr")).unwrap();
    assert_eq!(r["reward"], 1.0, "{r}\n{log}\n{stderr}");
    assert_eq!(r["exit"], 0, "{r}");
    assert_eq!(r["tokens"]["input"], 240, "{r}");
    // Trusted, `auto` stays `auto` and the write needs no approval.
    assert!(stderr.contains("trusted this project"), "{stderr}");
    assert!(!log.contains(r#""type":"approval""#), "{log}");
    assert!(!log.contains(r#""type":"tool_rejected""#), "{log}");

    let sent = fake.responses();
    assert_eq!(sent.len(), 2, "{sent:#?}");
    let outputs = items(&sent[1].body, "function_call_output");
    assert!(
        outputs.iter().any(|o| o["call_id"] == "call_w"),
        "{outputs:?}"
    );
    // The trial's entry is gone from the trust store again.
    let store = home.join(".config/bhai/trust.json");
    let entries: Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
    assert_eq!(entries, json!({}), "{store:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
