//! `bhai attach`: a session served by another bhai's `--serve`, shown here. A local
//! [`Session`] stands in for it, so the tui reads it as it reads its own: what happens
//! there arrives over `/events` and goes through [`Session::mirror`], and what the user
//! does here leaves through the session's [`Link`] as HTTP calls.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::entries::Entry;
use crate::permissions::{Answer, Policy};
use crate::server::{DEFAULT_PORT, TOKEN_ENV, TOKEN_HEADER};
use crate::session::{self, Action, Link, Session};

/// The first wait before reconnecting, doubled after each failure up to `BACKOFF_MAX`.
const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(10);
/// How long a request other than the event stream may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The server's base URL from what the user typed: nothing, a port, `host:port` or a
/// URL. Only `127.0.0.1` and `localhost` are accepted, since the server answers no
/// other name; a remote box is reached through a tunnel to one of those.
pub fn base_url(target: Option<&str>) -> Result<String> {
    let target = target.map(str::trim).unwrap_or_default();
    let rest = match target {
        "" => format!("127.0.0.1:{DEFAULT_PORT}"),
        port if port.bytes().all(|b| b.is_ascii_digit()) => format!("127.0.0.1:{port}"),
        other => match other.split_once("://") {
            Some(("http", rest)) => rest.trim_end_matches('/').to_string(),
            Some((scheme, _)) => bail!("{scheme}:// is not served; the server speaks plain http"),
            None => other.trim_end_matches('/').to_string(),
        },
    };
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) => (host, port),
        None => (rest.as_str(), ""),
    };
    if !matches!(host, "127.0.0.1" | "localhost") {
        bail!(
            "the server only answers on 127.0.0.1; tunnel to it with ssh -N -L PORT:127.0.0.1:PORT HOST and attach to the local port"
        );
    }
    let port: u16 = match port {
        "" => DEFAULT_PORT,
        port => port
            .parse()
            .with_context(|| format!("`{port}` is not a port"))?,
    };
    Ok(format!("http://{host}:{port}"))
}

/// The token from `--token`, else the file, else `BHAI_TOKEN`. Unlike `--serve`, a
/// missing file is an error: the server's file is the one that holds it.
pub fn token(
    given: Option<String>,
    file: Option<&std::path::Path>,
    env: Option<String>,
) -> Result<String> {
    let token = match (given, file, env) {
        (Some(token), ..) => token,
        (None, Some(path), _) => std::fs::read_to_string(path)
            .with_context(|| format!("cannot read the token from {}", path.display()))?,
        (None, None, Some(token)) => token,
        (None, None, None) => bail!(
            "attach needs the server's token: pass --token, --token-file PATH or set {TOKEN_ENV}"
        ),
    };
    let token = token.trim().to_string();
    if token.is_empty() {
        bail!("the token is empty");
    }
    Ok(token)
}

/// The served session's HTTP routes.
#[derive(Clone)]
pub struct Remote {
    http: reqwest::Client,
    base: String,
    token: String,
}

/// Why a request got no answer worth reading.
#[derive(Debug)]
enum Failed {
    /// The server said no, with its reason.
    Refused(u16, String),
    /// Nothing answered.
    Unreachable(String),
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failed::Refused(_, message) => f.write_str(message),
            Failed::Unreachable(e) => write!(f, "no answer: {e}"),
        }
    }
}

impl Remote {
    pub fn new(base: String, token: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base,
            token,
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// A request carrying the token. reqwest names the host from the URL and sends no
    /// `Origin`, which is what the server asks of a client.
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base))
            .header(TOKEN_HEADER, &self.token)
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Value, Failed> {
        let response = request
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| Failed::Unreachable(e.to_string()))?;
        let status = response.status().as_u16();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        match status {
            200..=299 => Ok(body),
            _ => Err(Failed::Refused(
                status,
                body["error"]
                    .as_str()
                    .map_or_else(|| format!("status {status}"), str::to_string),
            )),
        }
    }

    async fn get(&self, path: &str) -> Result<Value, Failed> {
        self.send(self.request(reqwest::Method::GET, path)).await
    }

    /// A null `body` sends none, which the routes that take no body expect.
    async fn post(&self, path: &str, body: Value) -> Result<Value, Failed> {
        let request = self.request(reqwest::Method::POST, path);
        let request = match body.is_null() {
            true => request,
            false => request.json(&body),
        };
        self.send(request).await
    }

    /// `/state` and the transcript, read together.
    async fn snapshot(&self) -> Result<(Value, Vec<Entry>), Failed> {
        let mut body = self.get("/snapshot").await?;
        let transcript = serde_json::from_value(body["transcript"].take()).unwrap_or_default();
        Ok((body["state"].take(), transcript))
    }

    /// The event stream from after `after`.
    async fn events(&self, after: u64) -> Result<reqwest::Response, Failed> {
        let response = self
            .request(reqwest::Method::GET, &format!("/events?since={after}"))
            .send()
            .await
            .map_err(|e| Failed::Unreachable(e.to_string()))?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body: Value = response.json().await.unwrap_or(Value::Null);
            let message = body["error"].as_str().unwrap_or_default().to_string();
            return Err(Failed::Refused(status, message));
        }
        Ok(response)
    }
}

/// One thing `/events` sent.
#[derive(Debug, PartialEq)]
enum Frame {
    Event(u64, session::Event),
    /// The log had let go of this many events before the stream reached them.
    Lagged(u64),
}

/// Take every complete frame off the front of `buf`. Comments (the keep-alives) and
/// events this build cannot read are skipped.
fn frames(buf: &mut Vec<u8>) -> Vec<Frame> {
    let mut out = Vec::new();
    while let Some(end) = buf.windows(2).position(|w| w == b"\n\n") {
        let block: Vec<u8> = buf.drain(..end + 2).collect();
        let block = String::from_utf8_lossy(&block);
        let field = |name: &str| block.lines().find_map(|l| l.strip_prefix(name));
        let Some(data) = field("data: ").or_else(|| field("data:")) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if value["type"] == "lagged" {
            out.push(Frame::Lagged(value["data"].as_u64().unwrap_or_default()));
            continue;
        }
        let id = field("id: ").or_else(|| field("id:"));
        let Some(seq) = id.and_then(|id| id.trim().parse().ok()) else {
            continue;
        };
        if let Ok(event) = serde_json::from_value(value) {
            out.push(Frame::Event(seq, event));
        }
    }
    out
}

/// A session attached to a served one, and the signal that its whole state was read
/// again, which the view has to follow.
pub struct Attached {
    pub session: Arc<Session>,
    pub resync: mpsc::UnboundedReceiver<()>,
}

/// The base URL of the session to attach to. A `bhai serve` daemon answers
/// `/sessions`: `wanted` picks one by id or prefix, else the only one, else the user
/// does at a terminal. A `--serve` server has no such route and is its own session.
pub async fn choose(base: &str, token: &str, wanted: Option<&str>) -> Result<String> {
    let remote = Remote::new(base.to_string(), token.to_string());
    let listed = match remote.get("/sessions").await {
        Ok(body) => body,
        Err(Failed::Refused(404, _)) if wanted.is_some() => {
            bail!("{base} serves one session, so it takes no --session")
        }
        // A plain server, or a refusal `connect` reports in its own words.
        Err(_) => return Ok(base.to_string()),
    };
    let rows: Vec<(String, String)> = listed["sessions"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|row| {
            (
                row["id"].as_str().unwrap_or_default().to_string(),
                described(row),
            )
        })
        .collect();
    let id = match pick(&rows, wanted)? {
        Some(id) => id,
        None => ask(&rows)?,
    };
    Ok(format!("{base}/sessions/{id}"))
}

/// One line for a session in the list: its id, whether it is busy and what it is about.
fn described(row: &Value) -> String {
    let state = match (row["pending"].is_null(), row["working"].as_bool()) {
        (false, _) => "approval waiting",
        (true, Some(true)) => "working",
        _ => "idle",
    };
    let title = row["title"].as_str().unwrap_or("(no prompt yet)");
    let project = row["project"].as_str().unwrap_or_default();
    format!(
        "{}  {state:<16}  {project}  {title}",
        row["id"].as_str().unwrap_or_default()
    )
}

/// The session `wanted` names among `rows`, or the only one; `None` when the user must
/// choose among several.
fn pick(rows: &[(String, String)], wanted: Option<&str>) -> Result<Option<String>> {
    let listing = || {
        rows.iter()
            .map(|(_, line)| format!("\n  {line}"))
            .collect::<String>()
    };
    if let Some(wanted) = wanted {
        let found: Vec<_> = rows
            .iter()
            .filter(|(id, _)| id.starts_with(wanted))
            .collect();
        return match found.as_slice() {
            [(id, _)] => Ok(Some(id.clone())),
            [] => bail!(
                "no session `{wanted}` is open there; open ones:{}",
                listing()
            ),
            _ => bail!("`{wanted}` matches more than one session:{}", listing()),
        };
    }
    match rows {
        [] => bail!(
            "no session is open there; open one with --open on the server, or POST /sessions/new"
        ),
        [(id, _)] => Ok(Some(id.clone())),
        _ if !std::io::IsTerminal::is_terminal(&std::io::stdin()) => bail!(
            "several sessions are open; choose one with --session ID:{}",
            listing()
        ),
        _ => Ok(None),
    }
}

/// Ask at the terminal which of `rows` to attach to, by number or id.
fn ask(rows: &[(String, String)]) -> Result<String> {
    eprintln!("several sessions are open:");
    for (n, (_, line)) in rows.iter().enumerate() {
        eprintln!("  {}) {line}", n + 1);
    }
    eprint!("attach to: ");
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let answer = answer.trim();
    if let Some((id, _)) = answer
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|n| rows.get(n))
    {
        return Ok(id.clone());
    }
    match pick(rows, Some(answer).filter(|a| !a.is_empty()))? {
        Some(id) => Ok(id),
        None => bail!("no session chosen"),
    }
}

/// Read the served session and start following it.
pub async fn connect(remote: Remote) -> Result<Attached> {
    let (state, transcript) = remote.snapshot().await.map_err(|e| match e {
        Failed::Refused(403, message) => {
            anyhow::anyhow!("{} refused the token: {message}", remote.base)
        }
        Failed::Refused(_, message) => anyhow::anyhow!("{}: {message}", remote.base),
        Failed::Unreachable(e) => anyhow::anyhow!(
            "cannot reach {} ({e}); is bhai running there with --serve, and the ssh tunnel up?",
            remote.base
        ),
    })?;
    let text = |key: &str| state[key].as_str().unwrap_or_default().to_string();
    let (tx_user, rx_user) = mpsc::channel(1);
    let (tx_control, rx_control) = mpsc::channel(1);
    let session = Session::new(
        text("model"),
        text("effort"),
        text("identity"),
        tx_user,
        tx_control,
        Arc::new(crate::agent::Cancel::default()),
        Arc::new(Policy::default()),
        None,
    );
    let (tx_actions, rx_actions) = mpsc::unbounded_channel();
    let link = Arc::new(Link {
        url: remote.base.clone(),
        actions: tx_actions,
        connected: true.into(),
        permissions: Default::default(),
    });
    session.attach(Arc::clone(&link));
    let after = state["seq"].as_u64().unwrap_or_default();
    session.seed(&state, transcript);
    refresh_permissions(&remote, &link).await;
    let (tx_resync, resync) = mpsc::unbounded_channel();
    tokio::spawn(follow(
        remote.clone(),
        Arc::clone(&session),
        after,
        tx_resync,
    ));
    // Nothing is sent to an agent here; the receivers are held so a send fails quietly.
    tokio::spawn(act(
        remote,
        Arc::clone(&session),
        rx_actions,
        (rx_user, rx_control),
    ));
    Ok(Attached { session, resync })
}

async fn refresh_permissions(remote: &Remote, link: &Link) {
    if let Ok(body) = remote.get("/permissions").await
        && let Some(text) = body["permissions"].as_str()
    {
        *link.permissions.lock().unwrap_or_else(|e| e.into_inner()) = text.to_string();
    }
}

/// Read the served session again from scratch, returning the `seq` it leaves off at.
async fn reseed(remote: &Remote, session: &Session) -> Option<u64> {
    let (state, transcript) = remote.snapshot().await.ok()?;
    let after = state["seq"].as_u64().unwrap_or_default();
    session.seed(&state, transcript);
    Some(after)
}

/// Follow `/events` into `session` for as long as the session lives, reconnecting with
/// backoff and picking up after the last event seen.
async fn follow(
    remote: Remote,
    session: Arc<Session>,
    mut after: u64,
    resync: mpsc::UnboundedSender<()>,
) {
    let Some(link) = session.link().cloned() else {
        return;
    };
    let mut wait = BACKOFF_MIN;
    loop {
        match remote.events(after).await {
            Ok(response) => {
                if !link.connected.swap(true, Ordering::Relaxed) {
                    session.publish(session::Event::Info(format!("reconnected to {}", link.url)));
                    let _ = resync.send(());
                }
                wait = BACKOFF_MIN;
                let mut stream = response.bytes_stream();
                let mut buf = Vec::new();
                while let Some(Ok(chunk)) = stream.next().await {
                    buf.extend_from_slice(&chunk);
                    for frame in frames(&mut buf) {
                        match frame {
                            Frame::Event(seq, event) => {
                                after = seq;
                                session.mirror(event);
                            }
                            // What was missed is not in the stream, so the whole state
                            // is read again rather than left with a hole in it.
                            Frame::Lagged(_) => {
                                if let Some(seq) = reseed(&remote, &session).await {
                                    after = seq;
                                    let _ = resync.send(());
                                }
                            }
                        }
                    }
                    if resync.is_closed() {
                        return;
                    }
                }
            }
            // An id newer than the server's latest: it restarted, and this run's ids
            // start again from 1.
            Err(Failed::Refused(409, _)) => {
                if let Some(seq) = reseed(&remote, &session).await {
                    after = seq;
                    let _ = resync.send(());
                    session.publish(session::Event::Info(
                        "the served session restarted; read it again".to_string(),
                    ));
                    continue;
                }
            }
            Err(_) => {}
        }
        if resync.is_closed() {
            return;
        }
        if link.connected.swap(false, Ordering::Relaxed) {
            session.publish(session::Event::Info(format!(
                "lost {}; reconnecting. The session there keeps running.",
                link.url
            )));
            let _ = resync.send(());
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(BACKOFF_MAX);
    }
}

/// Carry each action to the served session, one at a time, in the order taken.
async fn act<T>(
    remote: Remote,
    session: Arc<Session>,
    mut actions: mpsc::UnboundedReceiver<Action>,
    _held: T,
) {
    while let Some(action) = actions.recv().await {
        let refresh = matches!(
            action,
            Action::Mode(_) | Action::Allow(_) | Action::Trust(_)
        );
        let (path, body) = route(&action);
        match remote.post(&path, body).await {
            Ok(answer) => {
                let said = ["allowed", "trusted", "untrusted"]
                    .iter()
                    .find_map(|key| answer[*key].as_str());
                if let Some(said) = said {
                    session.publish(session::Event::Info(said.to_string()));
                }
            }
            Err(e) => session.publish(session::Event::Error(format!("{path}: {e}"))),
        }
        if refresh && let Some(link) = session.link() {
            refresh_permissions(&remote, link).await;
        }
    }
}

/// The route and body that do `action` on the served session.
fn route(action: &Action) -> (String, Value) {
    match action {
        // The id makes a retried request harmless: the server answers it once.
        Action::Prompt(text) => (
            "/prompt".into(),
            json!({ "text": text, "id": uuid::Uuid::new_v4().to_string() }),
        ),
        Action::Answer {
            id,
            answer: Answer::Accept(remember),
        } => ("/approve".into(), json!({ "id": id, "remember": remember })),
        Action::Answer {
            id,
            answer: Answer::Reject,
        } => ("/reject".into(), json!({ "id": id })),
        Action::Interrupt => ("/interrupt".into(), Value::Null),
        Action::InterruptChild(id) => (format!("/children/{id}/interrupt"), Value::Null),
        Action::Steer { id, text } => ("/steer".into(), json!({ "id": id, "text": text })),
        Action::Mode(mode) => ("/mode".into(), json!({ "mode": mode })),
        Action::Model { model, effort } => {
            ("/model".into(), json!({ "model": model, "effort": effort }))
        }
        Action::Fast(on) => ("/fast".into(), json!({ "on": on })),
        Action::Retry => ("/retry".into(), Value::Null),
        Action::Goal(text) => ("/goal".into(), json!({ "text": text })),
        Action::Allow(rule) => ("/allow".into(), json!({ "rule": rule })),
        Action::Trust(true) => ("/trust".into(), Value::Null),
        Action::Trust(false) => ("/untrust".into(), Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::*;
    use crate::agent::{AgentEvent, UserInput};
    use crate::permissions::{Offers, Remember};
    use crate::session::Event;

    const WAIT: Duration = Duration::from_secs(5);
    const TOKEN: &str = "attach-test-token-0123";

    #[test]
    fn a_session_is_picked_by_prefix_or_by_being_the_only_one() {
        let rows = |ids: &[&str]| -> Vec<(String, String)> {
            ids.iter()
                .map(|id| (id.to_string(), id.to_string()))
                .collect()
        };
        let two = rows(&["ab12", "cd34"]);
        assert_eq!(pick(&two, Some("cd")).unwrap().as_deref(), Some("cd34"));
        assert!(
            pick(&two, Some("ef"))
                .unwrap_err()
                .to_string()
                .contains("ab12")
        );
        assert!(pick(&rows(&["ab1", "ab2"]), Some("ab")).is_err());
        assert_eq!(
            pick(&rows(&["ab12"]), None).unwrap().as_deref(),
            Some("ab12")
        );
        assert!(pick(&[], None).is_err());
        let line = described(&json!({
            "id": "ab12", "working": true, "pending": null, "project": "/p", "title": "fix it"
        }));
        assert_eq!(line, "ab12  working           /p  fix it");
    }

    #[test]
    fn targets_are_local_urls_and_nothing_else() {
        let ok = |t: Option<&str>| base_url(t).unwrap();
        assert_eq!(ok(None), "http://127.0.0.1:7878");
        assert_eq!(ok(Some("8791")), "http://127.0.0.1:8791");
        assert_eq!(ok(Some("localhost:9")), "http://localhost:9");
        assert_eq!(ok(Some("http://127.0.0.1:5/")), "http://127.0.0.1:5");
        assert_eq!(ok(Some("127.0.0.1")), "http://127.0.0.1:7878");
        for bad in [
            "example.com:7878",
            "https://127.0.0.1:1",
            "10.0.0.2:7878",
            "localhost:x",
        ] {
            assert!(base_url(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_token_flag_wins_then_the_file_then_the_env() {
        let dir = crate::tools::temp_dir();
        let file = dir.join("token");
        std::fs::write(&file, "from-file\n").unwrap();
        let env = Some("from-env".to_string());
        assert_eq!(
            token(Some("given".into()), Some(&file), env.clone()).unwrap(),
            "given"
        );
        assert_eq!(token(None, Some(&file), env.clone()).unwrap(), "from-file");
        assert_eq!(token(None, None, env).unwrap(), "from-env");
        assert!(token(None, None, None).is_err());
        assert!(token(None, Some(&dir.join("missing")), None).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn frames_are_read_whole_across_chunks_and_keepalives_skipped() {
        let mut buf = b": keep-alive\n\nid: 4\ndata: {\"type\":\"text\",\"data\":\"h".to_vec();
        assert_eq!(frames(&mut buf), vec![]);
        buf.extend_from_slice(b"i\"}\n\ndata: {\"type\":\"lagged\",\"data\":7}\n\nid: 5\n");
        assert_eq!(
            frames(&mut buf),
            vec![Frame::Event(4, Event::Text("hi".into())), Frame::Lagged(7)]
        );
        assert_eq!(buf, b"id: 5\n");
    }

    /// The other end: a session served on 127.0.0.1 in front of a fake agent, which
    /// hands each prompt it is sent to `prompts` and plays whatever `script` is given.
    struct Served {
        session: Arc<Session>,
        port: u16,
        agent: mpsc::UnboundedSender<AgentEvent>,
        prompts: mpsc::UnboundedReceiver<String>,
    }

    async fn served() -> Served {
        let (tx_user, mut rx_user) = mpsc::channel::<UserInput>(8);
        let (tx_control, _rx_control) = mpsc::channel(8);
        let session = Session::new(
            "remote-model".into(),
            "high".into(),
            "router".into(),
            tx_user,
            tx_control,
            Arc::new(crate::agent::Cancel::default()),
            Arc::new(Policy::default()),
            None,
        );
        let (agent, rx_agent) = mpsc::unbounded_channel();
        tokio::spawn(session::pump(Arc::clone(&session), rx_agent));
        let (tx_prompts, prompts) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let _keep = _rx_control;
            while let Some(input) = rx_user.recv().await {
                let _ = tx_prompts.send(input.text);
            }
        });
        let listener = crate::server::bind(0).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(crate::server::serve(
            listener,
            Arc::clone(&session),
            TOKEN.to_string(),
        ));
        Served {
            session,
            port,
            agent,
            prompts,
        }
    }

    /// Wait until `check` holds of the attached session.
    async fn until(what: &str, check: impl Fn() -> bool) {
        let waited = timeout(WAIT, async {
            while !check() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(waited.is_ok(), "never: {what}");
    }

    fn shown(session: &Session) -> Vec<Event> {
        session
            .since(0)
            .events
            .into_iter()
            .map(|(_, e)| e)
            .collect()
    }

    fn attach_to(port: u16) -> Remote {
        Remote::new(format!("http://127.0.0.1:{port}"), TOKEN.to_string())
    }

    /// Attached, the transcript so far is there, a prompt reaches the served agent, its
    /// approval shows here and is answered there, and the turn ends here as there.
    #[tokio::test]
    async fn a_turn_is_driven_from_the_attached_session() {
        let mut served = served().await;
        served.session.submit("earlier".to_string()).unwrap();
        assert_eq!(served.prompts.recv().await.unwrap(), "earlier");
        served
            .agent
            .send(AgentEvent::Text("before".into()))
            .unwrap();
        served.agent.send(AgentEvent::TurnEnd).unwrap();
        until("the first turn ends", || !served.session.working()).await;

        let wrong = Remote::new(format!("http://127.0.0.1:{}", served.port), "x".repeat(20));
        let refused = connect(wrong).await.err().unwrap().to_string();
        assert!(refused.contains("refused the token"), "{refused}");

        let attached = connect(attach_to(served.port)).await.unwrap();
        let here = attached.session;
        assert_eq!(here.model(), ("remote-model".into(), "high".into()));
        let list = here.entries().list.clone();
        assert!(
            matches!(&list[..], [Entry::User(u), Entry::Assistant(a), ..] if u == "earlier" && a == "before"),
            "{list:?}"
        );

        assert_eq!(
            here.submit("make it".to_string()),
            Ok(session::Submitted::Started)
        );
        assert_eq!(served.prompts.recv().await.unwrap(), "make it");
        until("the turn shows here", || here.working()).await;
        let (reply, decided) = oneshot::channel();
        served
            .agent
            .send(AgentEvent::Approval {
                tool: "bash".into(),
                command: "touch x".into(),
                preview: None,
                offers: Offers {
                    exact: Some("Bash(touch x)".into()),
                    prefix: None,
                },
                reply,
            })
            .unwrap();
        until("the approval shows here", || here.state().pending.is_some()).await;
        let id = here.state().pending.unwrap().id;
        assert_eq!(
            here.answer(Answer::Accept(Some(Remember::Exact)), Some(id)),
            Some(id)
        );
        let answer = timeout(WAIT, decided).await.unwrap().unwrap();
        assert_eq!(answer, Answer::Accept(Some(Remember::Exact)));
        until("the answer shows here", || here.state().pending.is_none()).await;

        // A prompt sent while the turn runs queues there, and here.
        assert!(matches!(
            here.submit("next".to_string()),
            Ok(session::Submitted::Queued { position: 1 })
        ));
        until("the queue shows here", || here.queued() == ["next"]).await;
        served.agent.send(AgentEvent::Text("done".into())).unwrap();
        served.agent.send(AgentEvent::TurnEnd).unwrap();
        assert_eq!(served.prompts.recv().await.unwrap(), "next");
        until("the queued turn starts here", || {
            here.queued().is_empty() && here.working()
        })
        .await;

        // An interrupt stops the turn there; steering a child that is not there is
        // refused there, and the refusal is said here.
        assert!(here.interrupt());
        until("the interrupt shows here", || {
            shown(&here).contains(&Event::Interrupted)
        })
        .await;
        here.steer("nobody", "hello".into()).unwrap();
        until("the refusal shows here", || {
            shown(&here)
                .iter()
                .any(|e| matches!(e, Event::Error(m) if m.contains("no child agent")))
        })
        .await;
        here.set_mode(crate::permissions::Mode::Bypass);
        until("the mode changes there", || {
            served.session.state().mode == crate::permissions::Mode::Bypass
        })
        .await;
        until("and here", || {
            here.state().mode == crate::permissions::Mode::Bypass
        })
        .await;
    }

    /// Forwards a port and can cut every connection through it, as a dropped tunnel does.
    struct Tunnel {
        port: u16,
        open: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
    }

    impl Tunnel {
        async fn to(port: u16) -> Tunnel {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let local = listener.local_addr().unwrap().port();
            let open: Arc<Mutex<Vec<tokio::task::AbortHandle>>> = Arc::default();
            let held = Arc::clone(&open);
            tokio::spawn(async move {
                while let Ok((mut inbound, _)) = listener.accept().await {
                    let task = tokio::spawn(async move {
                        let mut outbound = tokio::net::TcpStream::connect(("127.0.0.1", port))
                            .await
                            .unwrap();
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    });
                    held.lock().unwrap().push(task.abort_handle());
                }
            });
            Tunnel { port: local, open }
        }

        fn cut(&self) {
            for task in self.open.lock().unwrap().drain(..) {
                task.abort();
            }
        }
    }

    /// A dropped stream is reopened from the last event seen: what was published while
    /// it was down arrives once each, in order, and the drop is said and undone.
    #[tokio::test]
    async fn a_dropped_stream_reconnects_and_replays_what_it_missed() {
        let served = served().await;
        let tunnel = Tunnel::to(served.port).await;
        let attached = connect(attach_to(tunnel.port)).await.unwrap();
        let here = attached.session;
        served.session.publish(Event::Text("one".into()));
        until("the first event arrives", || {
            shown(&here).contains(&Event::Text("one".into()))
        })
        .await;
        tunnel.cut();
        until("the drop is noticed", || !here.link().unwrap().connected()).await;
        for text in ["two", "three"] {
            served.session.publish(Event::Text(text.into()));
        }
        until("the stream is back", || here.link().unwrap().connected()).await;
        until("what was missed arrives", || {
            shown(&here).contains(&Event::Text("three".into()))
        })
        .await;
        let texts: Vec<_> = shown(&here)
            .into_iter()
            .filter_map(|e| match e {
                Event::Text(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["one", "two", "three"]);
        let said: Vec<_> = shown(&here)
            .into_iter()
            .filter_map(|e| match e {
                Event::Info(t) => Some(t),
                _ => None,
            })
            .collect();
        assert!(said.iter().any(|s| s.starts_with("lost ")), "{said:?}");
        assert!(
            said.iter().any(|s| s.starts_with("reconnected")),
            "{said:?}"
        );
    }
}
