//! `bhai serve`: several sessions behind one server. Each session is a `bhai --serve 0
//! --headless` of its own, since what a session sets up for the process (the sandbox, the
//! working directory, `[notify]`) is one per process. This server starts them, lists and
//! stops them, and forwards `/sessions/{id}/...` to each one's server.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path as Route, Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::permissions::Mode;
use crate::server::{self, TOKEN_ENV, TOKEN_HEADER};
use crate::{sessions, shutdown};

/// How long a new session may take to print its server line: config, MCP servers and
/// the backend's preflight all come first.
const START: Duration = Duration::from_secs(60);
/// How long a stopped session gets past its own wind-down before it is killed.
const GRACE: Duration = Duration::from_secs(5);
/// How long `/sessions` waits on each session's `/state`.
const ASK: Duration = Duration::from_secs(5);

/// What a session is opened with, from `POST /sessions/new` or the command line.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Open {
    /// The project root, relative to the daemon's; the daemon's own when left out.
    pub project: Option<PathBuf>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<Mode>,
    /// A saved session's id or a prefix of it, or `latest`.
    pub resume: Option<String>,
    /// `--trust`: honour the project's permission rules as they are now.
    #[serde(default)]
    pub trust: bool,
    /// `--as`: the identity a new session runs as.
    #[serde(rename = "as")]
    pub identity: Option<String>,
}

/// The daemon's sessions, and what it starts new ones with.
pub struct Daemon {
    program: PathBuf,
    root: PathBuf,
    /// Flags every session gets, such as `--profile`.
    common: Vec<String>,
    sessions: Mutex<Vec<Arc<Served>>>,
    /// The session the unprefixed routes reach while it runs: the one `--open` started.
    default: Mutex<Option<String>>,
    /// Held while a session starts, so two requests cannot both resume one id.
    opening: tokio::sync::Mutex<()>,
    http: reqwest::Client,
}

/// One running session.
struct Served {
    id: String,
    project: PathBuf,
    base: String,
    token: String,
    created: chrono::DateTime<chrono::Local>,
    pid: i32,
    ended: watch::Receiver<bool>,
}

/// A refusal, as the route answers it.
type Refusal = (StatusCode, String);

impl Daemon {
    pub fn new(program: PathBuf, root: PathBuf, common: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            program,
            root,
            common,
            sessions: Mutex::default(),
            default: Mutex::default(),
            opening: tokio::sync::Mutex::new(()),
            http: reqwest::Client::new(),
        })
    }

    fn running(&self) -> Vec<Arc<Served>> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The session `id` names, in full or by a prefix only it has.
    fn find(&self, id: &str) -> Result<Arc<Served>, Refusal> {
        let running = self.running();
        let found: Vec<_> = running.iter().filter(|s| s.id.starts_with(id)).collect();
        match found.as_slice() {
            [one] => Ok(Arc::clone(one)),
            [] => Err((
                StatusCode::NOT_FOUND,
                format!("no session `{id}` is open; GET /sessions lists them"),
            )),
            _ => Err((
                StatusCode::CONFLICT,
                format!("`{id}` matches more than one session"),
            )),
        }
    }

    /// The session an unprefixed route reaches: the default while it runs, else the
    /// only one.
    fn implied(&self) -> Result<Arc<Served>, Box<Response>> {
        let running = self.running();
        let default = self
            .default
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(found) = default.and_then(|id| running.iter().find(|s| s.id == id)) {
            return Ok(Arc::clone(found));
        }
        match running.as_slice() {
            [one] => Ok(Arc::clone(one)),
            [] => Err(Box::new(error(
                StatusCode::NOT_FOUND,
                "no session is open; POST /sessions/new opens one",
            ))),
            several => {
                let ids: Vec<_> = several.iter().map(|s| s.id.clone()).collect();
                Err(Box::new(
                    (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "several sessions are open; use /sessions/{id}/... with one of these",
                        "sessions": ids,
                    })),
                )
                    .into_response()))
            }
        }
    }

    /// Start a session and wait for its server.
    pub async fn open(self: &Arc<Self>, open: Open) -> Result<String, Refusal> {
        let bad = |message: String| (StatusCode::BAD_REQUEST, message);
        let project = match &open.project {
            Some(dir) => self.root.join(dir),
            None => self.root.clone(),
        };
        let project = project
            .canonicalize()
            .ok()
            .filter(|p| p.is_dir())
            .ok_or_else(|| bad(format!("{} is not a directory", project.display())))?;
        let _opening = self.opening.lock().await;
        // Resolved here, so a session already open is refused before a second process
        // appends to its file.
        let resume = match open.resume.as_deref() {
            None => None,
            Some(id) => {
                let id = Some(id).filter(|id| *id != "latest");
                let loaded = sessions::find(&project.join(sessions::DIR), id)
                    .map_err(|e| bad(format!("{e:#}")))?;
                Some(loaded.header.session)
            }
        };
        if let Some(id) = &resume
            && self.running().iter().any(|s| &s.id == id)
        {
            return Err((
                StatusCode::CONFLICT,
                format!("session {id} is already open"),
            ));
        }
        let token = server::mint();
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(["--serve", "0", "--headless", "-C"])
            .arg(&project)
            .args(&self.common);
        for (flag, value) in [
            ("--model", &open.model),
            ("--effort", &open.effort),
            ("--as", &open.identity),
        ] {
            if let Some(value) = value {
                command.arg(format!("{flag}={value}"));
            }
        }
        if let Some(mode) = open.mode {
            command.arg(format!("--mode={mode}"));
        }
        if open.trust {
            command.arg("--trust");
        }
        if let Some(id) = &resume {
            command.arg(format!("--resume={id}"));
        }
        let mut child = command
            .current_dir(&project)
            .env(TOKEN_ENV, &token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // Its own group, so ctrl+c in the daemon's terminal reaches only the daemon,
            // which then stops each session once.
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("cannot start {}: {e}", self.program.display()),
                )
            })?;
        let pid = child.id().and_then(|p| i32::try_from(p).ok()).unwrap_or(0);
        let mut lines = BufReader::new(child.stderr.take().expect("stderr is piped")).lines();
        let mut said = Vec::new();
        let mut id = None;
        let started = tokio::time::timeout(START, async {
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(rest) = line.strip_prefix("bhai: session ") {
                    id = Some(rest.trim().to_string());
                } else if let Some(rest) = line.strip_prefix("bhai: debug server on ") {
                    return rest.split_whitespace().next().map(str::to_string);
                } else {
                    said.push(line);
                }
            }
            None
        })
        .await;
        let (Ok(Some(base)), Some(id)) = (started, id) else {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let said = said.join("\n");
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("the session did not start: {}", said.trim()),
            ));
        };
        let short: String = id.chars().take(8).collect();
        for line in said {
            eprintln!("[{short}] {line}");
        }
        let relay = tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[{short}] {line}");
            }
        });
        let (ended_tx, ended) = watch::channel(false);
        let served = Arc::new(Served {
            id: id.clone(),
            project,
            base,
            token,
            created: chrono::Local::now(),
            pid,
            ended,
        });
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(served);
        let daemon = Arc::downgrade(self);
        let reaped = id.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            // Its last lines, the resume hint among them, before it counts as gone.
            let _ = tokio::time::timeout(Duration::from_secs(1), relay).await;
            if let Some(daemon) = daemon.upgrade() {
                daemon
                    .sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|s| s.id != reaped);
            }
            let status = status.map_or_else(|e| e.to_string(), |s| s.to_string());
            eprintln!("bhai: session {reaped} ended ({status})");
            let _ = ended_tx.send(true);
        });
        Ok(id)
    }

    /// Make `id` the session the unprefixed routes reach.
    pub fn set_default(&self, id: &str) {
        *self.default.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.to_string());
    }

    /// Stop every session as SIGTERM stops one, together.
    pub async fn close_all(&self) {
        let running = self.running();
        futures_util::future::join_all(running.iter().map(|s| stop(s))).await;
    }

    /// Each session with what `/state` says of it, in the order they were opened.
    async fn list(&self) -> Vec<Value> {
        let running = self.running();
        let rows = running.iter().map(|served| async move {
            let state = self
                .http
                .get(format!("{}/state", served.base))
                .header(TOKEN_HEADER, &served.token)
                .timeout(ASK)
                .send()
                .await
                .ok();
            let state: Value = match state {
                Some(response) => response.json().await.unwrap_or(Value::Null),
                None => Value::Null,
            };
            let file = sessions::path(&served.project.join(sessions::DIR), &served.id);
            let title = sessions::load(&file)
                .ok()
                .and_then(|loaded| loaded.items.iter().find_map(sessions::user_text))
                .map(|text| {
                    text.lines()
                        .next()
                        .unwrap_or_default()
                        .chars()
                        .take(80)
                        .collect::<String>()
                });
            let updated = std::fs::metadata(&file)
                .and_then(|m| m.modified())
                .map(|t| chrono::DateTime::<chrono::Local>::from(t).to_rfc3339())
                .unwrap_or_else(|_| served.created.to_rfc3339());
            json!({
                "id": served.id,
                "title": title,
                "project": served.project,
                "working": state["working"],
                "pending": state["pending"],
                "model": state["model"],
                "mode": state["mode"],
                "created": served.created.to_rfc3339(),
                "updated": updated,
            })
        });
        futures_util::future::join_all(rows).await
    }

    /// Send `request` on to `served` at `path`, and its answer back as it streams.
    async fn forward(&self, served: &Served, path: &str, request: Request) -> Response {
        let (parts, body) = request.into_parts();
        let query = parts
            .uri
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default();
        let url = format!("{}/{}{query}", served.base, path.trim_start_matches('/'));
        let Ok(method) = reqwest::Method::from_bytes(parts.method.as_str().as_bytes()) else {
            return error(StatusCode::METHOD_NOT_ALLOWED, "unknown method");
        };
        // One over the session's own limit, so it is the session that says too large.
        let body = match axum::body::to_bytes(body, server::FILE_LIMIT as usize + 1).await {
            Ok(body) => body,
            Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "the body is too large"),
        };
        let mut sent = self
            .http
            .request(method, url)
            .header(TOKEN_HEADER, &served.token);
        for name in [header::CONTENT_TYPE, header::ACCEPT, LAST_EVENT_ID] {
            if let Some(value) = parts.headers.get(&name) {
                sent = sent.header(name.as_str(), value.as_bytes());
            }
        }
        if !body.is_empty() {
            sent = sent.body(body);
        }
        let response = match sent.send().await {
            Ok(response) => response,
            Err(e) => {
                return error(
                    StatusCode::BAD_GATEWAY,
                    &format!("session {} did not answer: {e}", served.id),
                );
            }
        };
        let status =
            StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let mut headers = HeaderMap::new();
        for name in [header::CONTENT_TYPE, header::CACHE_CONTROL] {
            if let Some(value) = response.headers().get(name.as_str())
                && let Ok(value) = header::HeaderValue::from_bytes(value.as_bytes())
            {
                headers.insert(name, value);
            }
        }
        (status, headers, Body::from_stream(response.bytes_stream())).into_response()
    }
}

const LAST_EVENT_ID: HeaderName = HeaderName::from_static("last-event-id");

/// SIGTERM `served`, as `kill` would stop a lone session, and wait for it to end; one
/// that outlasts its wind-down is killed.
async fn stop(served: &Served) {
    let mut ended = served.ended.clone();
    if *ended.borrow() || served.pid <= 0 {
        return;
    }
    signal(served.pid, libc::SIGTERM);
    let wait = shutdown::SETTLE + GRACE;
    if tokio::time::timeout(wait, ended.wait_for(|e| *e))
        .await
        .is_err()
    {
        signal(served.pid, libc::SIGKILL);
        let _ = tokio::time::timeout(GRACE, ended.wait_for(|e| *e)).await;
    }
}

fn signal(pid: i32, signal: i32) {
    #[allow(unsafe_code)]
    // SAFETY: only sends a signal, to a session this daemon started and has not reaped.
    unsafe {
        libc::kill(pid, signal);
    }
}

/// Serve the daemon on `listener` until a signal stops it, then stop every session.
pub async fn run(daemon: Arc<Daemon>, listener: TcpListener, token: String) -> anyhow::Result<()> {
    let mut signals = shutdown::take_ending(true);
    tokio::select! {
        result = axum::serve(listener, router(Arc::clone(&daemon), token)) => Ok(result?),
        Some(signal) = signals.recv() => {
            let count = daemon.running().len();
            eprintln!("bhai: {} stops {count} session(s)", shutdown::name(signal));
            daemon.close_all().await;
            Ok(())
        }
    }
}

fn router(daemon: Arc<Daemon>, token: String) -> Router {
    Router::new()
        .route("/sessions", get(list))
        .route("/sessions/new", post(open))
        .route("/sessions/{id}", delete(close))
        .route("/sessions/{id}/close", post(close))
        .route("/sessions/{id}/{*rest}", any(prefixed))
        .fallback(unprefixed)
        .layer(middleware::from_fn(move |request: Request, next: Next| {
            let token = token.clone();
            async move { server::local_only(&token, request, next).await }
        }))
        .with_state(daemon)
}

async fn list(State(daemon): State<Arc<Daemon>>) -> Response {
    Json(json!({ "sessions": daemon.list().await })).into_response()
}

async fn open(State(daemon): State<Arc<Daemon>>, body: axum::body::Bytes) -> Response {
    // An empty body opens a session with the daemon's defaults.
    let open: Open = match body.is_empty() {
        true => Open::default(),
        false => match serde_json::from_slice(&body) {
            Ok(open) => open,
            Err(e) => return error(StatusCode::BAD_REQUEST, &e.to_string()),
        },
    };
    match daemon.open(open).await {
        Ok(id) => {
            let project = daemon.find(&id).map(|s| s.project.clone()).ok();
            Json(json!({ "id": id, "project": project, "path": format!("/sessions/{id}") }))
                .into_response()
        }
        Err((status, message)) => error(status, &message),
    }
}

async fn close(State(daemon): State<Arc<Daemon>>, Route(id): Route<String>) -> Response {
    let served = match daemon.find(&id) {
        Ok(served) => served,
        Err((status, message)) => return error(status, &message),
    };
    stop(&served).await;
    Json(json!({ "closed": served.id })).into_response()
}

async fn prefixed(
    State(daemon): State<Arc<Daemon>>,
    Route((id, rest)): Route<(String, String)>,
    request: Request,
) -> Response {
    match daemon.find(&id) {
        Ok(served) => daemon.forward(&served, &rest, request).await,
        Err((status, message)) => error(status, &message),
    }
}

async fn unprefixed(State(daemon): State<Arc<Daemon>>, request: Request) -> Response {
    let served = match daemon.implied() {
        Ok(served) => served,
        Err(response) => return *response,
    };
    let path = request.uri().path().to_string();
    daemon.forward(&served, &path, request).await
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_reads_the_fields_and_refuses_others() {
        let open: Open = serde_json::from_value(json!({
            "project": "sub", "model": "m", "mode": "auto", "resume": "latest", "trust": true, "as": "x"
        }))
        .unwrap();
        assert_eq!(open.project.as_deref(), Some(std::path::Path::new("sub")));
        assert_eq!(open.mode, Some(Mode::Auto));
        assert!(open.trust);
        assert_eq!(open.identity.as_deref(), Some("x"));
        assert!(serde_json::from_value::<Open>(json!({ "cwd": "x" })).is_err());
    }

    async fn daemon() -> (Arc<Daemon>, String, String) {
        let daemon = Daemon::new(
            PathBuf::from("/nonexistent/bhai"),
            std::env::temp_dir(),
            Vec::new(),
        );
        let listener = server::bind(0).await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let token = server::mint();
        tokio::spawn(
            axum::serve(listener, router(Arc::clone(&daemon), token.clone())).into_future(),
        );
        (daemon, base, token)
    }

    #[tokio::test]
    async fn routes_need_the_token_and_say_when_no_session_is_open() {
        let (_daemon, base, token) = daemon().await;
        let http = reqwest::Client::new();
        let refused = http.get(format!("{base}/sessions")).send().await.unwrap();
        assert_eq!(refused.status(), 403);
        let refused = http.get(format!("{base}/state")).send().await.unwrap();
        assert_eq!(
            refused.status(),
            403,
            "the fallback is behind the token too"
        );
        let listed: Value = http
            .get(format!("{base}/sessions"))
            .header(TOKEN_HEADER, &token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listed, json!({ "sessions": [] }));
        let none = http
            .get(format!("{base}/state"))
            .header(TOKEN_HEADER, &token)
            .send()
            .await
            .unwrap();
        assert_eq!(none.status(), 404);
        let none = http
            .get(format!("{base}/sessions/abc/state"))
            .header(TOKEN_HEADER, &token)
            .send()
            .await
            .unwrap();
        assert_eq!(none.status(), 404);
        let missing = http
            .post(format!("{base}/sessions/new"))
            .header(TOKEN_HEADER, &token)
            .json(&json!({ "project": "no-such-dir-for-bhai" }))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 400);
        let unstartable = http
            .post(format!("{base}/sessions/new"))
            .header(TOKEN_HEADER, &token)
            .send()
            .await
            .unwrap();
        assert_eq!(unstartable.status(), 500);
    }
}
