//! A localhost debug server: inspect and drive a running session over HTTP.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{self, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::broadcast::error::RecvError;

use crate::permissions::{Answer, Mode, Remember};
use crate::session::{self, Session, Submitted};

/// Port `--serve` listens on when none is given.
pub const DEFAULT_PORT: u16 = 7878;

/// Bind to 127.0.0.1 only; the server can run commands, so it never faces the network.
pub async fn bind(port: u16) -> anyhow::Result<TcpListener> {
    Ok(TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?)
}

pub async fn serve(listener: TcpListener, session: Arc<Session>) -> anyhow::Result<()> {
    Ok(axum::serve(listener, router(session)).await?)
}

fn router(session: Arc<Session>) -> Router {
    Router::new()
        .route("/state", get(state))
        .route("/events", get(events))
        .route("/prompt", post(prompt))
        .route("/approve", post(approve))
        .route("/reject", post(reject))
        .route("/interrupt", post(interrupt))
        .route("/retry", post(retry))
        .route("/mode", post(mode))
        .route("/context", get(context))
        .route("/children", get(children))
        .route("/steer", post(steer))
        .layer(middleware::from_fn(local_only))
        .with_state(session)
}

/// Refuse browsers: any web page could otherwise POST `/approve` to localhost, and a
/// rebound DNS name could drive the whole session.
async fn local_only(request: Request, next: Next) -> Response {
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
    next.run(request).await
}

#[derive(Deserialize)]
struct Prompt {
    text: String,
}

#[derive(Deserialize)]
struct ModeBody {
    mode: Mode,
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

async fn events(
    State(session): State<Arc<Session>>,
) -> Sse<impl Stream<Item = Result<sse::Event, axum::Error>>> {
    let rx = session.subscribe();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        let item = sent(rx.recv().await)?;
        let event = item
            .map_err(axum::Error::new)
            .and_then(|value| sse::Event::default().json_data(value));
        Some((event, rx))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// What `/events` sends for one receive, or `None` once the session has gone away. A
/// consumer that fell behind is told how many it missed, rather than left with a hole in
/// the stream that reads as nothing having happened.
fn sent(received: Result<session::Event, RecvError>) -> Option<serde_json::Result<Value>> {
    match received {
        Ok(event) => Some(serde_json::to_value(event)),
        Err(RecvError::Lagged(n)) => Some(Ok(json!({ "type": "lagged", "data": n }))),
        Err(RecvError::Closed) => None,
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

/// Post a message to a running child agent, as typing into its pane does.
async fn steer(State(session): State<Arc<Session>>, Json(body): Json<Steer>) -> Response {
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return error(StatusCode::BAD_REQUEST, "text is empty");
    }
    match session.steer(&body.id, text) {
        Ok(()) => ok(),
        Err(_) => error(
            StatusCode::NOT_FOUND,
            "no child agent of that id is running",
        ),
    }
}

async fn prompt(State(session): State<Arc<Session>>, Json(body): Json<Prompt>) -> Response {
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return error(StatusCode::BAD_REQUEST, "text is empty");
    }
    match session.submit(text) {
        Ok(Submitted::Started) => ok(),
        Ok(Submitted::Queued { position }) => {
            Json(json!({ "ok": true, "queued": position })).into_response()
        }
        // A busy session queues instead of refusing, so the agent is gone.
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, &e.to_string()),
    }
}

#[derive(Debug, Default, Deserialize)]
struct Approve {
    /// Which offered rule to remember.
    remember: Option<Remember>,
}

/// The body is optional: `{"remember": "exact" | "prefix"}`.
async fn approve(State(session): State<Arc<Session>>, body: Bytes) -> Response {
    let approve: Approve = if body.iter().all(u8::is_ascii_whitespace) {
        Approve::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(approve) => approve,
            Err(e) => return error(StatusCode::BAD_REQUEST, &format!("bad body: {e}")),
        }
    };
    let Some(pending) = session.state().pending else {
        return error(StatusCode::CONFLICT, "no approval is pending");
    };
    if let Some(remember) = approve.remember
        && pending.offers.get(remember).is_none()
    {
        let message = format!("this approval offers no {} rule", remember.as_str());
        return error(StatusCode::BAD_REQUEST, &message);
    }
    answer(&session, Answer::Accept(approve.remember), Some(pending.id))
}

async fn reject(State(session): State<Arc<Session>>) -> Response {
    answer(&session, Answer::Reject, None)
}

fn answer(session: &Session, answer: Answer, id: Option<u64>) -> Response {
    match session.answer(answer, id) {
        Some(id) => Json(json!({ "ok": true, "id": id })).into_response(),
        None => error(StatusCode::CONFLICT, "no approval is pending"),
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

async fn mode(State(session): State<Arc<Session>>, Json(body): Json<ModeBody>) -> Response {
    // An untrusted project only has `ask`, so the mode in force may not be the one asked
    // for. The reply says which it is rather than pretending.
    let mode = session.set_mode(body.mode);
    Json(json!({ "ok": true, "mode": mode })).into_response()
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

    /// A tool that streams a lot of progress can outrun a consumer that forks per line,
    /// and the stream then jumps. It has to say so, or the gap reads as a quiet patch.
    #[tokio::test]
    async fn a_consumer_that_falls_behind_is_told_what_it_missed() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(2);
        for i in 0..5 {
            let _ = tx.send(session::Event::Text(i.to_string()));
        }
        assert_eq!(
            sent(rx.recv().await).unwrap().unwrap(),
            json!({"type": "lagged", "data": 3})
        );
        // The stream goes on from where the ring now starts.
        assert_eq!(
            sent(rx.recv().await).unwrap().unwrap(),
            json!({"type": "text", "data": "3"})
        );
        drop(tx);
        assert_eq!(
            sent(rx.recv().await).unwrap().unwrap(),
            json!({"type": "text", "data": "4"})
        );
        assert!(sent(rx.recv().await).is_none());
    }

    /// Start a server on an ephemeral port in front of a fake agent that says hi, asks
    /// to run one command, and reports the decision on the returned channel.
    async fn start() -> (String, oneshot::Receiver<Answer>) {
        let (tx_user, mut rx_user) = mpsc::channel::<String>(1);
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
        tokio::spawn(serve(listener, session));
        (base, rx_decision)
    }

    async fn get_json(http: &reqwest::Client, url: String) -> Value {
        http.get(url).send().await.unwrap().json().await.unwrap()
    }

    async fn post(http: &reqwest::Client, url: String, body: Value) -> StatusCode {
        let status = http.post(url).json(&body).send().await.unwrap().status();
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

        let events = http.get(format!("{base}/events")).send().await.unwrap();
        assert_eq!(events.status().as_u16(), 200);

        let empty = http.post(format!("{base}/approve")).send().await.unwrap();
        assert_eq!(empty.status(), StatusCode::CONFLICT);
        assert_eq!(
            post(&http, format!("{base}/prompt"), json!({"text": "go"})).await,
            StatusCode::OK
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
                json!({"remember": "exact"})
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
        for want in [
            r#"{"type":"user","data":"go"}"#,
            r#"{"type":"text","data":"hi"}"#,
            r#"{"type":"approval","data":{"id":1,"tool":"bash","command":"ls","exact":"Bash(ls)"}}"#,
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
        tokio::spawn(serve(listener, session));
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
            .send()
            .await
            .unwrap();
        assert_eq!(from_page.status().as_u16(), 403);
        let rebound = http
            .get(format!("{base}/state"))
            .header("host", "evil.example:7878")
            .send()
            .await
            .unwrap();
        assert_eq!(rebound.status().as_u16(), 403);
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
}
