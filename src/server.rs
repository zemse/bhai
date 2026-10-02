//! A localhost debug server: inspect and drive a running session over HTTP.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{self, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;

use crate::permissions::{Answer, Mode, Remember};
use crate::session::{self, Session, SubmitError, Submitted};

/// Port `--serve` listens on when none is given.
pub const DEFAULT_PORT: u16 = 7878;
/// The header every request carries the run's token in.
pub const TOKEN_HEADER: &str = "x-bhai-token";

/// A token for one run, printed where the server's address is. The server can run
/// commands as the user, and a port on localhost is open to every process on the machine,
/// so knowing the port is not enough.
pub fn mint() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Bind to 127.0.0.1 only; the server can run commands, so it never faces the network.
pub async fn bind(port: u16) -> anyhow::Result<TcpListener> {
    Ok(TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?)
}

pub async fn serve(
    listener: TcpListener,
    session: Arc<Session>,
    token: String,
) -> anyhow::Result<()> {
    Ok(axum::serve(listener, router(session, token)).await?)
}

fn router(session: Arc<Session>, token: String) -> Router {
    Router::new()
        .route("/state", get(state))
        .route("/events", get(events))
        .route("/prompt", post(prompt))
        .route("/approve", post(approve))
        .route("/reject", post(reject))
        .route("/interrupt", post(interrupt))
        .route("/retry", post(retry))
        .route("/goal", post(goal))
        .route("/mode", post(mode))
        .route("/model", post(model))
        .route("/fast", post(fast))
        .route("/context", get(context))
        .route("/permissions", get(permissions))
        .route("/allow", post(allow))
        .route("/trust", post(trust))
        .route("/untrust", post(untrust))
        .route("/children", get(children))
        .route("/steer", post(steer))
        .route("/children/{id}/interrupt", post(interrupt_child))
        .layer(middleware::from_fn(move |request, next| {
            let token = token.clone();
            async move { local_only(&token, request, next).await }
        }))
        .layer(Extension(Arc::new(Seen::default())))
        .with_state(session)
}

/// Refuse browsers: any web page could otherwise POST `/approve` to localhost, and a
/// rebound DNS name could drive the whole session. The token is what makes the rest of
/// the machine's processes, which can reach the port as easily as the user can, say where
/// they got it: it is printed once, where the address is.
async fn local_only(token: &str, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    let name = host.rsplit_once(':').map_or(host, |(name, _)| name);
    if headers.contains_key(header::ORIGIN) || !matches!(name, "127.0.0.1" | "localhost") {
        return error(
            StatusCode::FORBIDDEN,
            "only local, non-browser clients are allowed",
        );
    }
    let given = headers
        .get(TOKEN_HEADER)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    if !constant_eq(given, token) {
        return error(
            StatusCode::FORBIDDEN,
            &format!(
                "this session's {TOKEN_HEADER} is required; it is printed where the server's address is"
            ),
        );
    }
    next.run(request).await
}

/// Compare without stopping at the first wrong byte.
fn constant_eq(given: &str, token: &str) -> bool {
    if given.len() != token.len() {
        return false;
    }
    given
        .bytes()
        .zip(token.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[derive(Deserialize)]
struct Prompt {
    text: String,
    id: Option<String>,
}

#[derive(Deserialize)]
struct ModeBody {
    mode: Mode,
}

/// A switch: either field left out stays as it is, so `{"effort": "low"}` keeps the model.
#[derive(Deserialize)]
struct ModelBody {
    model: Option<String>,
    effort: Option<String>,
}

#[derive(Deserialize)]
struct FastBody {
    on: bool,
}

#[derive(Deserialize)]
struct GoalBody {
    /// What follows `/goal`: an objective, `pause`, `resume`, `budget <n>`, `clear`, or
    /// nothing to have it shown.
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct AllowBody {
    /// A permission rule as `/allow` takes it, such as `Bash(cargo test:*)`.
    rule: String,
}

#[derive(Deserialize)]
struct Steer {
    /// The child agent to tell, as `/children` lists it.
    id: String,
    text: String,
}

async fn state(State(session): State<Arc<Session>>) -> Response {
    Json(session.state()).into_response()
}

/// The live stream, each event with its `seq` as the SSE `id`. A `Last-Event-ID` header
/// replays what came after that id first, so a client that dropped picks up where it was;
/// `0` replays all the log holds, and the `seq` in `/state` is where a snapshot leaves off.
async fn events(State(session): State<Arc<Session>>, headers: HeaderMap) -> Response {
    let latest = session.seq();
    let after = match headers.get("last-event-id") {
        None => latest,
        Some(value) => match value
            .to_str()
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
        {
            Some(after) if after <= latest => after,
            // Ids restart with each run, so one from the future is from another run.
            Some(_) => {
                return error(
                    StatusCode::CONFLICT,
                    "that event id is not from this run; reconnect without Last-Event-ID",
                );
            }
            None => return error(StatusCode::BAD_REQUEST, "Last-Event-ID is not a number"),
        },
    };
    Sse::new(stream(session, after))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Events after `after`, read from the session's log as it grows.
fn stream(
    session: Arc<Session>,
    after: u64,
) -> impl Stream<Item = Result<sse::Event, axum::Error>> {
    let wake = session.watch_seq();
    let state = (session, wake, after, VecDeque::new());
    futures_util::stream::unfold(
        state,
        |(session, mut wake, mut after, mut ready)| async move {
            while ready.is_empty() {
                wake.borrow_and_update();
                let since = session.since(after);
                if since.missed > 0 {
                    ready.push_back(Sent::Lagged(since.missed));
                }
                if let Some(&(last, _)) = since.events.last() {
                    after = last;
                }
                ready.extend(since.events.into_iter().map(|(seq, e)| Sent::Event(seq, e)));
                if ready.is_empty() {
                    wake.changed().await.ok()?;
                }
            }
            let item = ready.pop_front()?.into_sse();
            Some((item, (session, wake, after, ready)))
        },
    )
}

/// One thing `/events` sends.
enum Sent {
    Event(u64, session::Event),
    /// A consumer that fell behind the log is told how many it missed, rather than left
    /// with a hole in the stream that reads as nothing having happened.
    Lagged(u64),
}

impl Sent {
    fn value(&self) -> serde_json::Result<Value> {
        match self {
            Sent::Event(_, event) => serde_json::to_value(event),
            Sent::Lagged(n) => Ok(json!({ "type": "lagged", "data": n })),
        }
    }

    fn into_sse(self) -> Result<sse::Event, axum::Error> {
        let value = self.value().map_err(axum::Error::new)?;
        let event = match self {
            Sent::Event(seq, _) => sse::Event::default().id(seq.to_string()),
            Sent::Lagged(_) => sse::Event::default(),
        };
        event.json_data(value)
    }
}

/// The child agents of the running turn, with each one's transcript.
async fn children(State(session): State<Arc<Session>>) -> Response {
    let children: Vec<_> = session
        .children()
        .into_iter()
        .map(|row| {
            // The child's own transcript in full: nothing here is attributed, since a
            // child's calls index its own history rather than the session's.
            let entries: Vec<_> = session
                .child_entries(&row.id)
                .map(|entries| {
                    entries
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .list
                        .iter()
                        .map(|entry| json!({ "kind": entry.kind(), "text": entry.text() }))
                        .collect()
                })
                .unwrap_or_default();
            json!({ "child": row, "entries": entries })
        })
        .collect();
    Json(json!({ "children": children })).into_response()
}

/// Stop one running child agent, as ctrl+x in its pane does.
async fn interrupt_child(State(session): State<Arc<Session>>, Path(id): Path<String>) -> Response {
    match session.interrupt_child(&id) {
        true => ok(),
        false => error(
            StatusCode::NOT_FOUND,
            "no child agent of that id is running",
        ),
    }
}

/// Post a message to a running child agent, as typing into its pane does.
async fn steer(State(session): State<Arc<Session>>, Json(body): Json<Steer>) -> Response {
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return error(StatusCode::BAD_REQUEST, "text is empty");
    }
    match session.steer(&body.id, text) {
        Ok(()) => ok(),
        Err(e @ SubmitError::TooLong(_)) => error(StatusCode::PAYLOAD_TOO_LARGE, &e.to_string()),
        Err(_) => error(
            StatusCode::NOT_FOUND,
            "no child agent of that id is running",
        ),
    }
}

/// Prompts accepted under a client-chosen `id`, with the text and the answer given.
#[derive(Default)]
struct Seen(Mutex<HashMap<String, (String, Value)>>);

/// `{"text", "id"?}`. A retry carrying the `id` of an accepted prompt gets the original
/// answer back and submits nothing; the same `id` with other text is a conflict.
async fn prompt(
    State(session): State<Arc<Session>>,
    Extension(seen): Extension<Arc<Seen>>,
    Json(body): Json<Prompt>,
) -> Response {
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return error(StatusCode::BAD_REQUEST, "text is empty");
    }
    let Some(id) = body.id else {
        return match admit(&session, &text) {
            Ok(answer) => Json(answer).into_response(),
            Err((status, message)) => error(status, &message),
        };
    };
    // Held across the submit so two requests with one id cannot both get through.
    let mut seen = seen.0.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((first, answer)) = seen.get(&id) {
        if *first != text {
            return error(
                StatusCode::CONFLICT,
                "that id was used for a different prompt",
            );
        }
        return Json(answer.clone()).into_response();
    }
    match admit(&session, &text) {
        Ok(answer) => {
            seen.insert(id, (text, answer.clone()));
            Json(answer).into_response()
        }
        Err((status, message)) => error(status, &message),
    }
}

/// Submit `text`, answering `{"ok": true}` or, when it joined the queue, the position too.
fn admit(session: &Session, text: &str) -> Result<Value, (StatusCode, String)> {
    if let Some(rest) = crate::session::compact_then(text) {
        if rest.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                "/compact-then takes a prompt".to_string(),
            ));
        }
        let prompt = crate::session::Prompt::shown_as(rest.to_string(), text.to_string());
        return match session.submit_forked(prompt) {
            Ok(()) => Ok(json!({ "ok": true })),
            Err(e) => Err((StatusCode::CONFLICT, e.to_string())),
        };
    }
    if let Some(question) = crate::session::btw(text) {
        if question.is_empty() {
            return Err((StatusCode::BAD_REQUEST, "/btw takes a question".to_string()));
        }
        return match session.btw(question) {
            Ok(()) => Ok(json!({ "ok": true })),
            Err(e) => Err((StatusCode::CONFLICT, e.to_string())),
        };
    }
    if text.trim() == "/fork" {
        return match session.fork_session() {
            Ok(()) => Ok(json!({ "ok": true })),
            Err(e) => Err((StatusCode::CONFLICT, e.to_string())),
        };
    }
    match session.submit(text.to_string()) {
        Ok(Submitted::Started) => Ok(json!({ "ok": true })),
        Ok(Submitted::Queued { position }) => Ok(json!({ "ok": true, "queued": position })),
        // A busy session queues instead of refusing, so the agent is gone.
        Err(e) => Err((StatusCode::SERVICE_UNAVAILABLE, e.to_string())),
    }
}

#[derive(Debug, Default, Deserialize)]
struct Approve {
    /// Which offered rule to remember.
    remember: Option<Remember>,
    /// The approval being answered, as `/events` and `/state` give it.
    id: Option<u64>,
}

/// The body is optional: `{"id"?, "remember"?: "exact" | "prefix"}`. With an `id`, an
/// approval other than that one is left unanswered and the request is a 409.
async fn approve(State(session): State<Arc<Session>>, body: Bytes) -> Response {
    let approve: Approve = match parse_body(&body) {
        Ok(approve) => approve,
        Err(message) => return error(StatusCode::BAD_REQUEST, &message),
    };
    let Some(pending) = session.state().pending else {
        return error(StatusCode::CONFLICT, "no approval is pending");
    };
    if approve.id.is_some_and(|id| id != pending.id) {
        return error(StatusCode::CONFLICT, "that approval is not the one pending");
    }
    if let Some(remember) = approve.remember
        && pending.offers.get(remember).is_none()
    {
        let message = format!("this approval offers no {} rule", remember.as_str());
        return error(StatusCode::BAD_REQUEST, &message);
    }
    answer(&session, Answer::Accept(approve.remember), Some(pending.id))
}

#[derive(Debug, Default, Deserialize)]
struct Reject {
    id: Option<u64>,
}

/// The body is optional: `{"id"?}`, which `/approve` explains.
async fn reject(State(session): State<Arc<Session>>, body: Bytes) -> Response {
    let reject: Reject = match parse_body(&body) {
        Ok(reject) => reject,
        Err(message) => return error(StatusCode::BAD_REQUEST, &message),
    };
    answer(&session, Answer::Reject, reject.id)
}

fn parse_body<T: Default + serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, String> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(body).map_err(|e| format!("bad body: {e}"))
}

fn answer(session: &Session, answer: Answer, id: Option<u64>) -> Response {
    match session.answer(answer, id) {
        Some(id) => Json(json!({ "ok": true, "id": id })).into_response(),
        None => error(StatusCode::CONFLICT, "no such approval is pending"),
    }
}

async fn interrupt(State(session): State<Arc<Session>>) -> Response {
    if session.interrupt() {
        ok()
    } else {
        error(StatusCode::CONFLICT, "no turn is running")
    }
}

/// Run the last turn again, after it failed. Nothing is added to the history, so this is
/// the same call going out again rather than a new message.
async fn retry(State(session): State<Arc<Session>>) -> Response {
    match session.retry() {
        Ok(()) => ok(),
        Err(e) => error(StatusCode::CONFLICT, &e.to_string()),
    }
}

/// `/goal`, which is how a headless run is set working on its own. What came of it is in
/// the event stream, and the goal as it stands in `/state`.
async fn goal(State(session): State<Arc<Session>>, Json(body): Json<GoalBody>) -> Response {
    match session.set_goal(&body.text) {
        Ok(()) => ok(),
        Err(e) => error(StatusCode::BAD_REQUEST, &e),
    }
}

async fn mode(State(session): State<Arc<Session>>, Json(body): Json<ModeBody>) -> Response {
    // An untrusted project only has `ask`, so the mode in force may not be the one asked
    // for. The reply says which it is rather than pretending.
    let mode = session.set_mode(body.mode);
    Json(json!({ "ok": true, "mode": mode })).into_response()
}

/// `/model`, and `/effort` with the model left out.
async fn model(State(session): State<Arc<Session>>, Json(body): Json<ModelBody>) -> Response {
    let (model, effort) = session.model();
    let (model, effort) = (body.model.unwrap_or(model), body.effort.unwrap_or(effort));
    match session.set_model(model.clone(), effort.clone(), None) {
        Ok(()) => Json(json!({ "ok": true, "model": model, "effort": effort })).into_response(),
        Err(e) => error(StatusCode::CONFLICT, &e.to_string()),
    }
}

/// `/fast on|off`. The agent may refuse it, so `Event::Fast` says what is in force.
async fn fast(State(session): State<Arc<Session>>, Json(body): Json<FastBody>) -> Response {
    match session.set_fast(body.on) {
        Ok(()) => ok(),
        Err(e) => error(StatusCode::CONFLICT, &e.to_string()),
    }
}

/// The rules in force, as `/permissions` shows them.
async fn permissions(State(session): State<Arc<Session>>) -> Response {
    Json(json!({ "permissions": session.permissions() })).into_response()
}

/// `/allow`, for a session nobody is sitting in front of: a headless run is exactly where
/// a prompt cannot be answered, so it is also where a rule has to be settable.
async fn allow(State(session): State<Arc<Session>>, Json(body): Json<AllowBody>) -> Response {
    match session.allow(body.rule.trim()) {
        Ok(text) => Json(json!({ "ok": true, "allowed": text })).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    }
}

async fn trust(State(session): State<Arc<Session>>) -> Response {
    match session.trust() {
        Ok(text) => Json(json!({ "ok": true, "trusted": text, "mode": session.state().mode }))
            .into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    }
}

async fn untrust(State(session): State<Arc<Session>>) -> Response {
    match session.untrust() {
        Ok(text) => Json(json!({ "ok": true, "untrusted": text, "mode": session.state().mode }))
            .into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    }
}

async fn context(State(session): State<Arc<Session>>) -> Response {
    match session.context().await {
        Some(profile) => Json(profile).into_response(),
        None => error(StatusCode::SERVICE_UNAVAILABLE, "the agent is not running"),
    }
}

fn ok() -> Response {
    Json(json!({ "ok": true })).into_response()
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::StreamExt;
    use serde_json::Value;
    use tokio::sync::{mpsc, oneshot};
    use tokio::time::timeout;

    use super::*;
    use crate::agent::{AgentEvent, Control};
    use crate::client::Usage;
    use crate::permissions::{Offers, Policy};
    use crate::prompt::SystemPrompt;
    use crate::{profile, session};

    const WAIT: Duration = Duration::from_secs(5);

    /// A server over a session with no agent behind it, for tests that only publish.
    async fn quiet() -> (String, Arc<Session>) {
        let (tx_user, _) = mpsc::channel(1);
        let (tx_control, _) = mpsc::channel(1);
        let session = Session::new(
            "test-model".to_string(),
            "medium".to_string(),
            "router".to_string(),
            tx_user,
            tx_control,
            Arc::new(crate::agent::Cancel::default()),
            Arc::new(Policy::default()),
            None,
        );
        let listener = bind(0).await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve(listener, Arc::clone(&session), TOKEN.to_string()));
        (base, session)
    }

    fn text(s: &str) -> session::Event {
        session::Event::Text(s.to_string())
    }

    /// Read `/events` until `n` SSE events have arrived, as `(id, data)`.
    async fn read_sse(response: reqwest::Response, n: usize) -> Vec<(Option<String>, String)> {
        let mut body = String::new();
        let mut stream = response.bytes_stream();
        timeout(WAIT, async {
            while body.matches("\n\n").count() < n {
                let chunk = stream.next().await.unwrap().unwrap();
                body.push_str(&String::from_utf8_lossy(&chunk));
            }
        })
        .await
        .expect("too few events on /events");
        body.split("\n\n")
            .take(n)
            .map(|block| {
                let field = |name: &str| {
                    block
                        .lines()
                        .find_map(|l| l.strip_prefix(name))
                        .map(str::to_string)
                };
                (field("id: "), field("data: ").unwrap_or_default())
            })
            .collect()
    }

    fn events_after(http: &reqwest::Client, base: &str, id: &str) -> reqwest::RequestBuilder {
        http.get(format!("{base}/events"))
            .header(TOKEN_HEADER, TOKEN)
            .header("last-event-id", id)
    }

    /// A client that dropped reconnects with the last id it saw and gets what it missed,
    /// then the live stream, with nothing twice.
    #[tokio::test]
    async fn last_event_id_replays_what_came_after_it() {
        let (base, session) = quiet().await;
        let http = reqwest::Client::new();
        for s in ["a", "b", "c"] {
            session.publish(text(s));
        }
        assert_eq!(get_json(&http, format!("{base}/state")).await["seq"], 3);
        let response = events_after(&http, &base, "1").send().await.unwrap();
        assert_eq!(response.status().as_u16(), 200);
        session.publish(text("d"));
        let got = read_sse(response, 3).await;
        let want: Vec<_> = [("2", "b"), ("3", "c"), ("4", "d")]
            .into_iter()
            .map(|(id, data)| {
                let data = format!(r#"{{"type":"text","data":"{data}"}}"#);
                (Some(id.to_string()), data)
            })
            .collect();
        assert_eq!(got, want);
    }

    /// Without the header the stream starts at now, as it always did.
    #[tokio::test]
    async fn events_without_an_id_start_live() {
        let (base, session) = quiet().await;
        let http = reqwest::Client::new();
        session.publish(text("old"));
        let response = http
            .get(format!("{base}/events"))
            .header(TOKEN_HEADER, TOKEN)
            .send()
            .await
            .unwrap();
        session.publish(text("new"));
        let got = read_sse(response, 1).await;
        assert_eq!(
            got,
            vec![(
                Some("2".to_string()),
                r#"{"type":"text","data":"new"}"#.to_string()
            )]
        );
    }

    /// A reader further back than the log reaches is told how many it missed, rather than
    /// left with a hole in the stream that reads as nothing having happened.
    #[tokio::test]
    async fn a_reader_behind_the_log_is_told_what_it_missed() {
        let (base, session) = quiet().await;
        let http = reqwest::Client::new();
        let total = session::EVENT_LOG as u64 + 3;
        for i in 1..=total {
            session.publish(text(&i.to_string()));
        }
        let response = events_after(&http, &base, "0").send().await.unwrap();
        let got = read_sse(response, 2).await;
        assert_eq!(
            got,
            vec![
                (None, r#"{"type":"lagged","data":3}"#.to_string()),
                (
                    Some("4".to_string()),
                    r#"{"type":"text","data":"4"}"#.to_string()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn a_bad_or_foreign_last_event_id_is_refused() {
        let (base, session) = quiet().await;
        let http = reqwest::Client::new();
        session.publish(text("a"));
        let status = |id: &'static str| {
            let request = events_after(&http, &base, id);
            async move { request.send().await.unwrap().status().as_u16() }
        };
        assert_eq!(status("x").await, 400);
        // Ids restart with each run, so one past the newest is from another.
        assert_eq!(status("2").await, 409);
        assert_eq!(status("1").await, 200);
    }

    /// Start a server on an ephemeral port in front of a fake agent that says hi, asks
    /// to run one command, and reports the decision on the returned channel.
    /// The token the test server is started with.
    const TOKEN: &str = "test-token";

    async fn start() -> (String, oneshot::Receiver<Answer>) {
        let (tx_user, mut rx_user) = mpsc::channel::<crate::agent::UserInput>(1);
        let (tx_control, mut rx_control) = mpsc::channel::<Control>(1);
        let (tx_agent, rx_agent) = mpsc::unbounded_channel();
        let (tx_decision, rx_decision) = oneshot::channel();
        let session = Session::new(
            "test-model".to_string(),
            "medium".to_string(),
            "router".to_string(),
            tx_user,
            tx_control,
            Arc::new(crate::agent::Cancel::default()),
            Arc::new(Policy::default()),
            None,
        );
        tokio::spawn(session::pump(Arc::clone(&session), rx_agent));
        tokio::spawn(async move {
            while let Some(Control::Context(reply)) = rx_control.recv().await {
                let _ = reply.send(profile::build(
                    &SystemPrompt {
                        text: "sys".to_string(),
                        ..SystemPrompt::default()
                    },
                    &[],
                    &[],
                    &[],
                    &crate::tokens::ByteEstimate,
                ));
            }
        });
        tokio::spawn(async move {
            let _prompt = rx_user.recv().await;
            let _ = tx_agent.send(AgentEvent::Text("hi".to_string()));
            let (reply, wait) = oneshot::channel();
            let _ = tx_agent.send(AgentEvent::Approval {
                tool: "bash".to_string(),
                command: "ls".to_string(),
                preview: Some("@@ -1 +1 @@\n-a\n+b".to_string()),
                offers: Offers {
                    exact: Some("Bash(ls)".to_string()),
                    prefix: None,
                },
                reply,
            });
            let accepted = wait.await.unwrap_or(Answer::Reject);
            let _ = tx_decision.send(accepted);
            let _ = tx_agent.send(AgentEvent::Usage(Usage {
                input: 5,
                cached: 4,
                cache_write: 0,
                output: 2,
                reasoning: 1,
            }));
            let _ = tx_agent.send(AgentEvent::ChildUsage(Usage {
                input: 30,
                ..Usage::default()
            }));
            let _ = tx_agent.send(AgentEvent::TurnEnd);
            // The queued prompt starts as that turn ends.
            let _ = rx_user.recv().await;
            let _ = tx_agent.send(AgentEvent::TurnEnd);
            // Stay alive so the session keeps accepting messages.
            let _ = rx_user.recv().await;
        });
        let listener = bind(0).await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve(listener, session, TOKEN.to_string()));
        (base, rx_decision)
    }

    async fn get_json(http: &reqwest::Client, url: String) -> Value {
        http.get(url)
            .header(TOKEN_HEADER, TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn post(http: &reqwest::Client, url: String, body: Value) -> StatusCode {
        let status = http
            .post(url)
            .header(TOKEN_HEADER, TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status();
        StatusCode::from_u16(status.as_u16()).unwrap()
    }

    /// Poll `/state` until `check` passes.
    async fn wait_state(http: &reqwest::Client, base: &str, check: impl Fn(&Value) -> bool) {
        timeout(WAIT, async {
            loop {
                if check(&get_json(http, format!("{base}/state")).await) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("state never matched");
    }

    #[tokio::test]
    async fn drives_a_turn_over_http() {
        let (base, rx_decision) = start().await;
        let http = reqwest::Client::new();

        let state = get_json(&http, format!("{base}/state")).await;
        assert_eq!(state["model"], "test-model");
        assert_eq!(state["identity"], "router");
        assert_eq!(state["working"], false);
        assert!(state["pending"].is_null());

        let events = http
            .get(format!("{base}/events"))
            .header(TOKEN_HEADER, TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(events.status().as_u16(), 200);

        let empty = http
            .post(format!("{base}/approve"))
            .header(TOKEN_HEADER, TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(empty.status(), StatusCode::CONFLICT);
        let go = |text: &str| {
            http.post(format!("{base}/prompt"))
                .header(TOKEN_HEADER, TOKEN)
                .json(&json!({"text": text, "id": "r1"}))
                .send()
        };
        assert_eq!(go("go").await.unwrap().status().as_u16(), 200);
        // A retry with the same id is answered again and starts nothing.
        let retry = go("go").await.unwrap();
        assert_eq!(retry.status().as_u16(), 200);
        assert_eq!(retry.json::<Value>().await.unwrap(), json!({"ok": true}));
        assert_eq!(go("other").await.unwrap().status().as_u16(), 409);
        assert_eq!(
            get_json(&http, format!("{base}/state")).await["queued"],
            json!([])
        );
        // A second prompt joins the queue instead of being refused.
        assert_eq!(
            post(&http, format!("{base}/prompt"), json!({"text": "again"})).await,
            StatusCode::OK
        );
        assert_eq!(
            get_json(&http, format!("{base}/state")).await["queued"],
            json!(["again"])
        );

        wait_state(&http, &base, |s| s["pending"]["command"] == "ls").await;
        assert_eq!(
            get_json(&http, format!("{base}/state")).await["pending"]["exact"],
            "Bash(ls)"
        );
        assert_eq!(
            get_json(&http, format!("{base}/state")).await["pending"]["preview"],
            "@@ -1 +1 @@\n-a\n+b"
        );
        // An id that is not the pending one answers nothing, however it is asked.
        for path in ["approve", "reject"] {
            assert_eq!(
                post(&http, format!("{base}/{path}"), json!({"id": 99})).await,
                StatusCode::CONFLICT
            );
        }
        assert_eq!(
            get_json(&http, format!("{base}/state")).await["pending"]["id"],
            1
        );
        // Only an offered rule can be remembered, and the body must parse.
        for body in [json!({"remember": "prefix"}), json!({"remember": "all"})] {
            assert_eq!(
                post(&http, format!("{base}/approve"), body).await,
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            post(
                &http,
                format!("{base}/approve"),
                json!({"id": 1, "remember": "exact"})
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(
            timeout(WAIT, rx_decision).await.unwrap().unwrap(),
            Answer::Accept(Some(Remember::Exact))
        );

        wait_state(&http, &base, |s| s["working"] == false).await;
        let state = get_json(&http, format!("{base}/state")).await;
        assert_eq!(state["input_tokens"], 5);
        assert_eq!(state["output_tokens"], 2);
        assert_eq!(state["cached_tokens"], 4);
        assert_eq!(state["last_usage"]["reasoning"], 1);
        assert_eq!(state["children"]["input"], 30);
        assert!(state["pending"].is_null());

        // The stream saw the whole turn, approval id included.
        let mut body = String::new();
        let mut stream = events.bytes_stream();
        timeout(WAIT, async {
            while !body.contains("turn_end") {
                let chunk = stream.next().await.unwrap().unwrap();
                body.push_str(&String::from_utf8_lossy(&chunk));
            }
        })
        .await
        .expect("no turn_end on /events");
        // Each frame carries a count of the events so far as its SSE id.
        assert!(body.starts_with("id: 1\n"), "no id on the first in {body}");
        for want in [
            r#"{"type":"user","data":"go"}"#,
            r#"{"type":"text","data":"hi"}"#,
            r#"{"type":"approval","data":{"id":1,"tool":"bash","command":"ls","preview":"@@ -1 +1 @@\n-a\n+b","exact":"Bash(ls)"}}"#,
            r#"{"type":"resolved","data":{"id":1,"accepted":true,"remember":"exact"}}"#,
        ] {
            assert!(body.contains(want), "missing {want} in {body}");
        }
    }

    #[tokio::test]
    async fn state_lists_the_attributed_entries() {
        use crate::agent::fake::{Fake, say};

        let (tx_user, rx_user) = mpsc::channel(1);
        let (tx_control, rx_control) = mpsc::channel(1);
        let (tx_agent, rx_agent) = mpsc::unbounded_channel();
        let cancel = Arc::new(crate::agent::Cancel::default());
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
        tokio::spawn(session::pump(Arc::clone(&session), rx_agent));
        tokio::spawn(crate::agent::run_with(
            Arc::new(Fake::new(vec![vec![say("one")], vec![say("two")]])),
            "sess".to_string(),
            crate::prompt::system_prompt(&[], Vec::new()),
            policy,
            None,
            None,
            rx_user,
            None,
            rx_control,
            tx_agent,
            cancel,
            None,
            None,
            None,
            crate::compact::Limits::default(),
        ));
        let listener = bind(0).await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve(listener, session, TOKEN.to_string()));
        let http = reqwest::Client::new();

        for text in ["first", "second"] {
            assert_eq!(
                post(&http, format!("{base}/prompt"), json!({"text": text})).await,
                StatusCode::OK
            );
            wait_state(&http, &base, |s| s["working"] == false).await;
        }
        let state = get_json(&http, format!("{base}/state")).await;
        let entries = state["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2, "{entries:?}");
        let first = &entries[0];
        assert_eq!(
            (&first["index"], &first["kind"], &first["label"]),
            (&json!(0), &json!("user"), &json!("first"))
        );
        assert_eq!(first["method"], "estimated");
        assert!(first["input"].as_u64().unwrap() > 0);
        // The second call sent the first message again.
        assert_eq!(first["resends"], 1);
        assert_eq!(entries[1]["label"], "second");
        assert_eq!(entries[1]["resends"], 0);
    }

    #[tokio::test]
    async fn context_returns_the_breakdown() {
        let (base, _) = start().await;
        let context = get_json(&reqwest::Client::new(), format!("{base}/context")).await;
        assert_eq!(context["items"][0]["label"], "system prompt");
        assert_eq!(context["total_bytes"], 3);
    }

    #[tokio::test]
    async fn browser_and_foreign_host_requests_are_refused() {
        let (base, _) = start().await;
        let http = reqwest::Client::new();
        let from_page = http
            .post(format!("{base}/approve"))
            .header("origin", "https://example.com")
            .header(TOKEN_HEADER, TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(from_page.status().as_u16(), 403);
        let rebound = http
            .get(format!("{base}/state"))
            .header("host", "evil.example:7878")
            .header(TOKEN_HEADER, TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(rebound.status().as_u16(), 403);
    }

    /// Anything on the machine can reach the port, so the port is not the credential.
    #[tokio::test]
    async fn a_request_without_the_token_is_refused() {
        let (base, _) = start().await;
        let http = reqwest::Client::new();
        for request in [
            http.get(format!("{base}/state")),
            http.get(format!("{base}/events")),
            http.post(format!("{base}/interrupt")),
            http.get(format!("{base}/state")).header(TOKEN_HEADER, ""),
            http.get(format!("{base}/state"))
                .header(TOKEN_HEADER, "test-toke"),
            http.get(format!("{base}/state"))
                .header(TOKEN_HEADER, "test-token-"),
            http.get(format!("{base}/state"))
                .header(TOKEN_HEADER, "TEST-TOKEN"),
        ] {
            let refused = request.send().await.unwrap();
            assert_eq!(refused.status().as_u16(), 403);
        }
        // The one that was told.
        assert_eq!(
            get_json(&http, format!("{base}/state")).await["model"],
            "test-model"
        );
    }

    /// A headless run is exactly where nobody can answer a prompt, so it is where a rule
    /// has to be settable without one.
    #[tokio::test]
    async fn a_rule_can_be_added_over_http() {
        let (base, _) = start().await;
        let http = reqwest::Client::new();
        assert!(
            !get_json(&http, format!("{base}/permissions")).await["permissions"]
                .as_str()
                .unwrap()
                .contains("Bash(cargo test:*)")
        );
        assert_eq!(
            post(
                &http,
                format!("{base}/allow"),
                json!({"rule": "Bash(cargo test:*)"})
            )
            .await,
            StatusCode::OK
        );
        let shown = get_json(&http, format!("{base}/permissions")).await["permissions"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(shown.contains("Bash(cargo test:*)"), "{shown}");
        // A rule that does not parse is the caller's mistake, not a silent no-op.
        assert!(
            post(&http, format!("{base}/allow"), json!({"rule": "Bash(rm"}))
                .await
                .is_client_error()
        );
    }

    #[tokio::test]
    async fn mode_is_set_over_http() {
        let (base, _) = start().await;
        let http = reqwest::Client::new();
        assert_eq!(
            get_json(&http, format!("{base}/state")).await["mode"],
            "ask"
        );
        assert_eq!(
            post(&http, format!("{base}/mode"), json!({"mode": "bypass"})).await,
            StatusCode::OK
        );
        assert_eq!(
            get_json(&http, format!("{base}/state")).await["mode"],
            "bypass"
        );
        assert!(
            post(&http, format!("{base}/mode"), json!({"mode": "yolo"}))
                .await
                .is_client_error()
        );
    }

    #[tokio::test]
    async fn effort_is_set_over_http_keeping_the_model() {
        let (base, _) = start().await;
        let http = reqwest::Client::new();
        assert_eq!(
            post(&http, format!("{base}/model"), json!({"effort": "xhigh"})).await,
            StatusCode::OK
        );
        let state = get_json(&http, format!("{base}/state")).await;
        assert_eq!(
            (&state["model"], &state["effort"]),
            (&json!("test-model"), &json!("xhigh"))
        );
    }

    #[tokio::test]
    async fn a_goal_is_set_over_http_and_a_bad_budget_refused() {
        let (base, _) = start().await;
        let http = reqwest::Client::new();
        assert_eq!(
            post(
                &http,
                format!("{base}/goal"),
                json!({"text": "budget lots"})
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            post(
                &http,
                format!("{base}/goal"),
                json!({"text": "make it pass"})
            )
            .await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn retry_runs_a_turn_again_unless_one_is_running() {
        let (base, _) = start().await;
        let http = reqwest::Client::new();
        assert_eq!(
            post(&http, format!("{base}/retry"), json!({})).await,
            StatusCode::OK
        );
        // The session is working on it, so there is nothing to run again yet.
        assert_eq!(
            get_json(&http, format!("{base}/state")).await["working"],
            true
        );
        assert_eq!(
            post(&http, format!("{base}/retry"), json!({})).await,
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn interrupt_needs_a_running_turn() {
        let (base, _) = start().await;
        let http = reqwest::Client::new();
        assert_eq!(
            post(&http, format!("{base}/interrupt"), json!({})).await,
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn one_child_is_interrupted_over_http() {
        let cancel = Arc::new(crate::agent::Cancel::default());
        let (tx_user, _) = mpsc::channel(1);
        let (tx_control, _) = mpsc::channel(1);
        let session = Session::new(
            "test-model".to_string(),
            "medium".to_string(),
            "router".to_string(),
            tx_user,
            tx_control,
            Arc::clone(&cancel),
            Arc::new(Policy::default()),
            None,
        );
        let listener = bind(0).await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve(listener, Arc::clone(&session), TOKEN.to_string()));
        let flag = cancel.child("a1");
        let http = reqwest::Client::new();
        let url = |id: &str| format!("{base}/children/{id}/interrupt");

        assert_eq!(
            post(&http, url("zzzzzz"), json!({})).await,
            StatusCode::NOT_FOUND
        );
        assert!(!flag.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(post(&http, url("a1"), json!({})).await, StatusCode::OK);
        assert!(flag.load(std::sync::atomic::Ordering::Relaxed));
        assert!(!cancel.stopped(), "the turn was not interrupted");
    }
}
