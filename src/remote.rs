//! `bhai attach`: a session served by another bhai's `--serve`, shown here. A local
//! [`Session`] stands in for it, so the tui reads it as it reads its own: what happens
//! there arrives over `/events` and goes through [`Session::mirror`], and what the user
//! does here leaves through the session's [`Link`] as HTTP calls.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, watch};

use crate::entries::Entry;
use crate::permissions::{Answer, Policy};
use crate::server::{DEFAULT_PORT, KEEP_ALIVE, TOKEN_ENV, TOKEN_HEADER};
use crate::session::{self, Action, ChildLog, ChildRow, Link, Seed, Session};
use crate::tunnel;

/// The wait between attempts to reconnect. The first after a drop goes at once; a fixed
/// short wait after that, so a link that comes back is caught up within about this long.
const RETRY: Duration = Duration::from_secs(1);
/// How long a request other than the event stream may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the background list is read again while something runs there, for the
/// output and state that change without an event saying so.
const LISTS: Duration = Duration::from_secs(5);
/// How long the event stream may go without a byte before it is taken for dead. The
/// server sends a keep-alive every [`KEEP_ALIVE`], so this is several of them missed.
const IDLE: Duration = Duration::from_secs(10);
const _: () = assert!(KEEP_ALIVE.as_secs() * 2 < IDLE.as_secs());

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
    /// [`IDLE`], shorter in tests.
    idle: Duration,
    /// How the user knows the server, for notices: the URL, or the ssh host and port.
    name: String,
    /// The ssh tunnel attach runs to reach it, if it runs one.
    tunnel: Option<watch::Receiver<tunnel::State>>,
    /// Its `/snapshot` has been seen to carry the children, so `/children` need not be
    /// read around it.
    full: Arc<AtomicBool>,
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
        // Through a tunnel the connect is local, so a dead link shows as a request that
        // never answers: the timeouts below are what end it.
        let http = reqwest::Client::builder()
            .connect_timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            http,
            name: base.clone(),
            base,
            token,
            idle: IDLE,
            tunnel: None,
            full: Arc::default(),
        }
    }

    /// Reached through the tunnel whose state is `tunnel`, and named `name` for the user.
    pub fn through(self, name: String, tunnel: watch::Receiver<tunnel::State>) -> Self {
        Self {
            name,
            tunnel: Some(tunnel),
            ..self
        }
    }

    /// Why nothing answers, as far as can be told from here.
    fn why_down(&self) -> String {
        match self.tunnel.as_ref().map(|t| t.borrow().clone()) {
            None => "no answer".to_string(),
            Some(tunnel::State::Starting) => "ssh tunnel starting".to_string(),
            Some(tunnel::State::Down(why)) => format!("ssh tunnel down: {why}"),
            Some(tunnel::State::Up) => "tunnel up, server not answering".to_string(),
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

    /// The served session as of one `seq`, that `seq`, and the child events after it
    /// that the panes read with it may already hold.
    async fn snapshot(&self) -> Result<(Seed, u64, Skips), Failed> {
        let before = match self.full.load(Ordering::Relaxed) {
            true => None,
            false => Some(self.children().await?),
        };
        let mut body = self.get("/snapshot").await?;
        let transcript = serde_json::from_value(body["transcript"].take()).unwrap_or_default();
        let title = body["title"].as_str().map(str::to_string);
        let state = body["state"].take();
        let seq = state["seq"].as_u64().unwrap_or_default();
        let (children, skips) = match body["children"].as_array() {
            Some(rows) => {
                self.full.store(true, Ordering::Relaxed);
                let children = rows
                    .iter()
                    .filter_map(|c| child(c, |e| serde_json::from_value(e.clone()).ok()))
                    .collect();
                (children, Skips::default())
            }
            // An older server: its panes are read on either side of the snapshot, and
            // what moved between the two reads is taken as already held.
            None => {
                let after = self.children().await?;
                let skips = before
                    .map(|before| skips(&before, &after))
                    .unwrap_or_default();
                (after, skips)
            }
        };
        let seed = Seed {
            state,
            transcript,
            children,
            title,
        };
        Ok((seed, seq, skips))
    }

    /// `/children`, whose panes give each entry's kind and text alone.
    async fn children(&self) -> Result<Vec<ChildLog>, Failed> {
        let body = self.get("/children").await?;
        Ok(body["children"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|c| child(c, |e| Some(entry(e))))
            .collect())
    }

    /// The event stream from after `after`. Only its headers are timed: the body runs
    /// for as long as the link holds.
    async fn events(&self, after: u64) -> Result<reqwest::Response, Failed> {
        let request = self.request(reqwest::Method::GET, &format!("/events?since={after}"));
        let response = tokio::time::timeout(self.idle, request.send())
            .await
            .map_err(|_| Failed::Unreachable("no answer in time".to_string()))?
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

/// One child as `/children` or `/snapshot` lists it, its entries read by `entry`.
fn child(value: &Value, entry: impl Fn(&Value) -> Option<Entry>) -> Option<ChildLog> {
    let row = &value["child"];
    let text = |key: &str| row[key].as_str().unwrap_or_default().to_string();
    let idle = Duration::from_secs(row["idle_secs"].as_u64().unwrap_or_default());
    let row = ChildRow {
        id: row["id"].as_str()?.to_string(),
        identity: text("identity"),
        model: text("model"),
        description: text("description"),
        state: serde_json::from_value(row["state"].clone()).ok()?,
        steps: u32::try_from(row["steps"].as_u64().unwrap_or_default()).unwrap_or(u32::MAX),
        started: Instant::now()
            .checked_sub(Duration::from_secs(
                row["elapsed_secs"].as_u64().unwrap_or_default(),
            ))
            .unwrap_or_else(Instant::now),
        last: Instant::now()
            .checked_sub(idle)
            .unwrap_or_else(Instant::now),
    };
    let entries = value["entries"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(entry)
        .collect();
    Some(ChildLog { row, entries })
}

/// An entry from its kind and text, as `/children` gives it. What else it carried, a
/// command's tool or who rejected a call, is not there to read.
fn entry(value: &Value) -> Entry {
    let text = value["text"].as_str().unwrap_or_default().to_string();
    match value["kind"].as_str().unwrap_or_default() {
        "user" => Entry::User(text),
        "automatic" => Entry::Automatic(text),
        "assistant" => Entry::Assistant(text),
        "commentary" => Entry::Commentary(text),
        "thinking" => Entry::Reasoning(text),
        "command" => Entry::Command {
            tool: String::new(),
            summary: text,
        },
        "running" => Entry::Running {
            tail: text,
            lines: 0,
        },
        "output" => Entry::Output(text),
        "diff" => Entry::Diff(text),
        "rejected" => Entry::Rejected {
            by: crate::agent::Rejecter::User,
            reason: text,
        },
        "error" => Entry::Error(text),
        "failed" => Entry::Failed(text),
        "done" => Entry::Done(text),
        "summary" => Entry::Summary(text),
        _ => Entry::Info(text),
    }
}

/// Child events the stream goes on to replay that the panes may already hold, by child:
/// how many of its events, and whether its start is one.
#[derive(Debug, Default, PartialEq)]
struct Skips(HashMap<String, (u32, bool)>);

impl Skips {
    /// Whether `event` is one to drop, counting it off if so.
    fn take(&mut self, event: &session::Event) -> bool {
        let (id, start) = match event {
            session::Event::Child { id, .. } => (id, false),
            session::Event::ChildStarted { id, .. } => (id, true),
            _ => return false,
        };
        let Some((events, started)) = self.0.get_mut(id) else {
            return false;
        };
        let taken = match start {
            true => std::mem::take(started),
            false if *events > 0 => {
                *events -= 1;
                true
            }
            false => false,
        };
        if *events == 0 && !*started {
            self.0.remove(id);
        }
        taken
    }
}

/// What changed in the panes from `before` to `after`, read on either side of a
/// snapshot. All of it is skipped in the stream, so a pane may miss an event that came
/// in that window but never shows one twice.
fn skips(before: &[ChildLog], after: &[ChildLog]) -> Skips {
    let running = |log: &ChildLog| log.row.state == session::ChildState::Running;
    let mut skips = HashMap::new();
    for now in after {
        let (events, started) = match before.iter().find(|b| b.row.id == now.row.id) {
            Some(then) => (
                now.row.steps.saturating_sub(then.row.steps),
                running(now) && !running(then),
            ),
            None => (now.row.steps, true),
        };
        if events > 0 || started {
            skips.insert(now.row.id.clone(), (events, started));
        }
    }
    Skips(skips)
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
    let (seed, after, skips) = remote.snapshot().await.map_err(|e| match e {
        Failed::Refused(403, message) => {
            anyhow::anyhow!("{} refused the token: {message}", remote.base)
        }
        Failed::Refused(_, message) => anyhow::anyhow!("{}: {message}", remote.base),
        Failed::Unreachable(e) => anyhow::anyhow!(
            "cannot reach {} ({e}); is bhai running there with --serve, and the ssh tunnel up?",
            remote.name
        ),
    })?;
    let text = |key: &str| seed.state[key].as_str().unwrap_or_default().to_string();
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
    let getter = remote.clone();
    let get: session::Get = Box::new(move |path| {
        let remote = getter.clone();
        Box::pin(async move { remote.get(&path).await.map_err(|e| e.to_string()) })
    });
    let link = Arc::new(Link::new(remote.name.clone(), tx_actions, get));
    let (tx_up, up) = watch::channel(true);
    session.attach(Arc::clone(&link));
    session.seed(seed);
    refresh_permissions(&remote, &link).await;
    refresh_lists(&remote, &session).await;
    let (tx_resync, resync) = mpsc::unbounded_channel();
    let wake = Arc::new(Notify::new());
    tokio::spawn(lists(
        remote.clone(),
        Arc::clone(&session),
        Arc::clone(&wake),
        tx_resync.clone(),
    ));
    tokio::spawn(follow(
        remote.clone(),
        Arc::clone(&session),
        (after, skips),
        wake,
        tx_resync,
        tx_up,
    ));
    // Nothing is sent to an agent here; the receivers are held so a send fails quietly.
    tokio::spawn(act(
        remote,
        Arc::clone(&session),
        rx_actions,
        up,
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

/// Read again what the served session lists outside its event stream: what runs in the
/// background, its monitors and its schedules. Each keeps what it last had when its
/// route does not answer. Published as the count, which is what the view follows.
async fn refresh_lists(remote: &Remote, session: &Session) {
    let Some(link) = session.link() else {
        return;
    };
    let background = match remote.get("/background").await {
        Ok(mut body) => serde_json::from_value(body["background"].take()).ok(),
        Err(_) => None,
    };
    let monitors = match remote.get("/monitors").await {
        Ok(mut body) => serde_json::from_value(body["monitors"].take()).ok(),
        Err(_) => None,
    };
    let scheduled = match remote.get("/schedules").await {
        Ok(body) => Some(
            body["schedules"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter(|row| row["paused"] != true)
                .count(),
        ),
        // A server with no home directory keeps none.
        Err(Failed::Refused(..)) => Some(0),
        Err(Failed::Unreachable(_)) => None,
    };
    // Stored together, so the view never shows one read beside another's leftovers.
    if let Some(rows) = background {
        *link.background.lock().unwrap_or_else(|e| e.into_inner()) = rows;
    }
    if let Some(views) = monitors {
        *link.monitors.lock().unwrap_or_else(|e| e.into_inner()) = views;
    }
    if let Some(scheduled) = scheduled {
        link.scheduled.store(scheduled, Ordering::Relaxed);
    }
    let count = session.background().len();
    session.publish(session::Event::Background(count));
}

/// Keep the lists current: read again when `wake` says the served session's changed, and
/// every [`LISTS`] while something runs there.
async fn lists(
    remote: Remote,
    session: Arc<Session>,
    wake: Arc<Notify>,
    resync: mpsc::UnboundedSender<()>,
) {
    loop {
        let busy = !session.background().is_empty();
        tokio::select! {
            () = wake.notified() => {}
            () = tokio::time::sleep(LISTS), if busy => {}
            () = resync.closed() => return,
        }
        refresh_lists(&remote, &session).await;
    }
}

/// Read the served session again from scratch, returning the `seq` it leaves off at and
/// the child events after it to drop.
async fn reseed(remote: &Remote, session: &Session) -> Option<(u64, Skips)> {
    let (seed, after, skips) = remote.snapshot().await.ok()?;
    session.seed(seed);
    if let Some(link) = session.link() {
        refresh_permissions(remote, link).await;
    }
    refresh_lists(remote, session).await;
    Some((after, skips))
}

/// Follow `/events` into `session` for as long as the session lives, reconnecting every
/// [`RETRY`] and picking up after the last event seen. `up` says whether the stream is
/// open, for what [`act`] holds while it is not; `wake` has the lists read again.
async fn follow(
    remote: Remote,
    session: Arc<Session>,
    (mut after, mut skips): (u64, Skips),
    wake: Arc<Notify>,
    resync: mpsc::UnboundedSender<()>,
    up: watch::Sender<bool>,
) {
    let Some(link) = session.link().cloned() else {
        return;
    };
    let mut wait = Duration::ZERO;
    loop {
        match remote.events(after).await {
            Ok(response) => {
                if !link.connected.swap(true, Ordering::Relaxed) {
                    session.publish(session::Event::Info(format!("reconnected to {}", link.url)));
                    wake.notify_one();
                    let _ = resync.send(());
                }
                up.send_replace(true);
                wait = Duration::ZERO;
                let mut stream = response.bytes_stream();
                let mut buf = Vec::new();
                // A stream with nothing, not even a keep-alive, for `idle` is a link that
                // died without closing, so it is dropped and opened again.
                while let Ok(Some(Ok(chunk))) =
                    tokio::time::timeout(remote.idle, stream.next()).await
                {
                    buf.extend_from_slice(&chunk);
                    for frame in frames(&mut buf) {
                        match frame {
                            // A snapshot read after a lag already holds what the
                            // stream goes on to replay up to its `seq`.
                            Frame::Event(seq, _) if seq <= after => {}
                            Frame::Event(seq, event) => {
                                after = seq;
                                if skips.take(&event) {
                                    continue;
                                }
                                match event {
                                    // The count is published once the list it counts is.
                                    session::Event::Background(_) => wake.notify_one(),
                                    event => session.mirror(event),
                                }
                            }
                            // What was missed is not in the stream, so the whole state
                            // is read again rather than left with a hole in it.
                            Frame::Lagged(_) => {
                                if let Some(read) = reseed(&remote, &session).await {
                                    (after, skips) = read;
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
                if let Some(read) = reseed(&remote, &session).await {
                    (after, skips) = read;
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
        up.send_replace(false);
        *link.down.lock().unwrap_or_else(|e| e.into_inner()) = remote.why_down();
        if link.connected.swap(false, Ordering::Relaxed) {
            session.publish(session::Event::Info(format!(
                "lost {}; reconnecting. The session there keeps running. Prompts and answers given meanwhile are held and sent once it is back.",
                link.url
            )));
            let _ = resync.send(());
        }
        // A tunnel coming back up is tried at once rather than at the next tick.
        match remote.tunnel.clone() {
            Some(mut tunnel) if *tunnel.borrow() != tunnel::State::Up => {
                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    _ = tunnel.wait_for(|s| *s == tunnel::State::Up) => {}
                }
            }
            _ => tokio::time::sleep(wait).await,
        }
        wait = RETRY;
    }
}

/// An action waiting to be sent, routed when it was taken so a retry carries the same
/// prompt id.
struct Held {
    action: Action,
    path: String,
    body: Value,
    /// Taken while the link was down.
    late: bool,
}

/// Carry each action to the served session, one at a time, in the order taken. While
/// the link is down they are held and sent once it is back, except an interrupt, which
/// then could stop a turn the user never saw and so is refused at once.
async fn act<T>(
    remote: Remote,
    session: Arc<Session>,
    mut actions: mpsc::UnboundedReceiver<Action>,
    mut up: watch::Receiver<bool>,
    _keep: T,
) {
    let Some(link) = session.link().cloned() else {
        return;
    };
    let mut held = std::collections::VecDeque::new();
    loop {
        if held.is_empty() || !*up.borrow() {
            tokio::select! {
                action = actions.recv() => match action {
                    Some(action) => take(&session, &mut held, action, *up.borrow()),
                    None => return,
                },
                changed = up.changed() => if changed.is_err() {
                    return;
                },
            }
            link.held.store(held.len(), Ordering::Relaxed);
            continue;
        }
        let Some(next) = held.front() else {
            continue;
        };
        let refresh = matches!(
            next.action,
            Action::Mode(_) | Action::Allow(_) | Action::Trust(_)
        );
        match remote.post(&next.path, next.body.clone()).await {
            Ok(answer) => {
                let said = ["allowed", "trusted", "untrusted", "stopped"]
                    .iter()
                    .find_map(|key| answer[*key].as_str());
                if let Some(said) = said {
                    session.publish(session::Event::Info(said.to_string()));
                }
            }
            // A prompt carries its id and an answer names its approval, so sending
            // either again cannot do it twice; the rest are not sent blind.
            Err(Failed::Unreachable(_)) if retried(&next.action) => {
                if let Some(next) = held.front_mut().filter(|next| !next.late) {
                    next.late = true;
                    session.publish(session::Event::Info(format!(
                        "no answer from {}; {} is held and sent again once it answers",
                        link.url,
                        described_action(&next.action)
                    )));
                }
                let _ = tokio::time::timeout(RETRY, up.wait_for(|up| !*up)).await;
                continue;
            }
            Err(Failed::Unreachable(e)) => session.publish(session::Event::Error(format!(
                "{} may not have reached {}: {e}",
                described_action(&next.action),
                link.url
            ))),
            Err(Failed::Refused(409, _)) if next.late && matches!(next.action, Action::Answer { .. }) => {
                session.publish(session::Event::Error(
                    "the approval answered while disconnected was settled there before the link came back, so the answer was not used".to_string(),
                ))
            }
            Err(e) => session.publish(session::Event::Error(format!("{}: {e}", next.path))),
        }
        held.pop_front();
        link.held.store(held.len(), Ordering::Relaxed);
        if refresh {
            refresh_permissions(&remote, &link).await;
        }
    }
}

/// Queue `action` to be sent, saying so when the link is down.
fn take(session: &Session, held: &mut std::collections::VecDeque<Held>, action: Action, up: bool) {
    if !up {
        if matches!(
            action,
            Action::Interrupt | Action::InterruptTurn | Action::InterruptChild(_)
        ) {
            session.publish(session::Event::Error(format!(
                "not connected, so the {} was not sent; try again once reconnected",
                described_action(&action)
            )));
            return;
        }
        let said = match &action {
            Action::Prompt(text) => format!("held until reconnected: {text}"),
            Action::Answer { .. } => "held until reconnected: your answer to the approval, used if it is still waiting then".to_string(),
            other => format!("held until reconnected: {}", described_action(other)),
        };
        session.publish(session::Event::Info(said));
    }
    let (path, body) = route(&action);
    held.push_back(Held {
        action,
        path,
        body,
        late: !up,
    });
}

/// Whether `action` is sent again after a request that got no answer.
fn retried(action: &Action) -> bool {
    matches!(
        action,
        Action::Prompt(_)
            | Action::Answer { .. }
            | Action::Mode(_)
            | Action::Model { .. }
            | Action::Fast(_)
            | Action::Goal(_)
            | Action::Allow(_)
            | Action::Trust(_)
    )
}

/// What `action` is, in a few words for a notice.
fn described_action(action: &Action) -> String {
    match action {
        Action::Prompt(_) => "the prompt".into(),
        Action::Answer { .. } => "the answer to the approval".into(),
        Action::Interrupt => "interrupt".into(),
        Action::InterruptTurn => "parent interrupt".into(),
        Action::InterruptChild(id) => format!("interrupt of {id}"),
        Action::Steer { id, .. } => format!("message to {id}"),
        Action::Mode(mode) => format!("mode change to {}", mode.as_str()),
        Action::Model { model, .. } => format!("model change to {model}"),
        Action::Fast(_) => "fast mode change".into(),
        Action::Retry => "retry".into(),
        Action::Goal(_) => "goal".into(),
        Action::Allow(rule) => format!("rule {rule}"),
        Action::Trust(true) => "trust".into(),
        Action::Trust(false) => "untrust".into(),
        Action::StopBackground { kind, id } => format!("stop of {} {id}", kind.name()),
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
        Action::InterruptTurn => ("/interrupt/turn".into(), Value::Null),
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
        Action::StopBackground { kind, id } => {
            ("/background/kill".into(), json!({ "id": id, "kind": kind }))
        }
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
    fn child_snapshots_keep_elapsed_and_idle_time_separate() {
        let mut value = serde_json::json!({
            "child": {
                "id": "a1",
                "state": "running",
                "elapsed_secs": 125,
                "idle_secs": 3
            },
            "entries": []
        });
        let log = child(&value, |_| None).unwrap();
        assert_eq!(log.row.started.elapsed().as_secs(), 125);
        assert_eq!(log.row.idle().as_secs(), 3);
        assert_eq!(serde_json::to_value(&log.row).unwrap()["elapsed_secs"], 125);
        value["child"]
            .as_object_mut()
            .unwrap()
            .remove("elapsed_secs");
        let older = child(&value, |_| None).unwrap();
        assert_eq!(older.row.started.elapsed().as_secs(), 0);
    }

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
                child: false,
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

    /// Forwards a port and can cut every connection through it, as a dropped tunnel does,
    /// refuse new ones while it is `down`, or go quiet while it is `frozen`, holding
    /// every connection open without passing a byte, as a network cut under ssh does.
    struct Tunnel {
        port: u16,
        open: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
        down: Arc<std::sync::atomic::AtomicBool>,
        frozen: tokio::sync::watch::Sender<bool>,
    }

    impl Tunnel {
        async fn to(port: u16) -> Tunnel {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let local = listener.local_addr().unwrap().port();
            let open: Arc<Mutex<Vec<tokio::task::AbortHandle>>> = Arc::default();
            let down: Arc<std::sync::atomic::AtomicBool> = Arc::default();
            let (frozen, _) = tokio::sync::watch::channel(false);
            let (held, refusing, freezing) = (Arc::clone(&open), Arc::clone(&down), frozen.clone());
            tokio::spawn(async move {
                while let Ok((mut inbound, _)) = listener.accept().await {
                    if refusing.load(Ordering::Relaxed) {
                        continue;
                    }
                    let mut frozen = freezing.subscribe();
                    let task = tokio::spawn(async move {
                        let mut outbound = tokio::net::TcpStream::connect(("127.0.0.1", port))
                            .await
                            .unwrap();
                        let quiet = tokio::select! {
                            _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => false,
                            _ = frozen.wait_for(|f| *f) => true,
                        };
                        if quiet {
                            std::future::pending::<()>().await;
                        }
                    });
                    held.lock().unwrap().push(task.abort_handle());
                }
            });
            Tunnel {
                port: local,
                open,
                down,
                frozen,
            }
        }

        fn cut(&self) {
            for task in self.open.lock().unwrap().drain(..) {
                task.abort();
            }
        }

        /// Cut every connection and refuse new ones until `up`.
        fn down(&self) {
            self.down.store(true, Ordering::Relaxed);
            self.cut();
        }

        fn freeze(&self) {
            self.frozen.send_replace(true);
        }

        fn up(&self) {
            self.down.store(false, Ordering::Relaxed);
            self.frozen.send_replace(false);
        }
    }

    fn texts(session: &Session) -> Vec<String> {
        shown(session)
            .into_iter()
            .filter_map(|e| match e {
                Event::Text(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    /// A dropped stream is reopened from the last event seen: what was published while
    /// the link was down shows within about a [`RETRY`] of it coming back, once each and
    /// in order, and the drop is said and undone.
    #[tokio::test]
    async fn a_dropped_stream_reconnects_and_replays_what_it_missed() {
        let served = served().await;
        let tunnel = Tunnel::to(served.port).await;
        let attached = connect(attach_to(tunnel.port)).await.unwrap();
        let here = attached.session;
        served.session.publish(Event::Text("one".into()));
        until("the first event arrives", || texts(&here) == ["one"]).await;
        tunnel.down();
        until("the drop is noticed", || !here.link().unwrap().connected()).await;
        for text in ["two", "three", "four"] {
            served.session.publish(Event::Text(text.into()));
        }
        // Long enough for attempts to fail against the closed link.
        tokio::time::sleep(RETRY * 2).await;
        assert_eq!(texts(&here), ["one"]);
        tunnel.up();
        let back = std::time::Instant::now();
        until("what was missed arrives", || texts(&here).len() == 4).await;
        assert!(back.elapsed() < RETRY * 2, "{:?}", back.elapsed());
        assert_eq!(texts(&here), ["one", "two", "three", "four"]);
        assert!(here.link().unwrap().connected());
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

    /// Down for longer than the log reaches back, the attached session is read again
    /// from a snapshot rather than left with a hole.
    #[tokio::test]
    async fn a_link_down_past_the_log_reloads_the_snapshot() {
        let served = served().await;
        let tunnel = Tunnel::to(served.port).await;
        let attached = connect(attach_to(tunnel.port)).await.unwrap();
        let (here, mut resync) = (attached.session, attached.resync);
        tunnel.down();
        until("the drop is noticed", || !here.link().unwrap().connected()).await;
        served.session.publish(Event::User("long".into()));
        for n in 0..session::EVENT_LOG + 10 {
            served.session.publish(Event::Text(format!("{n} ")));
        }
        served.session.publish(Event::Text("end".into()));
        while resync.try_recv().is_ok() {}
        tunnel.up();
        let transcript = |s: &Session| serde_json::to_value(&s.entries().list).unwrap();
        until("the snapshot is read again", || {
            transcript(&here) == transcript(&served.session)
        })
        .await;
        assert!(timeout(WAIT, resync.recv()).await.unwrap().is_some());
    }

    /// A link that goes quiet without closing, as one under ssh does when the network
    /// drops, is dropped after `idle` and reopened once bytes flow again; keep-alives
    /// hold a quiet but live one open.
    #[tokio::test]
    async fn a_silent_link_is_dropped_and_reopened() {
        let served = served().await;
        let tunnel = Tunnel::to(served.port).await;
        let idle = KEEP_ALIVE + Duration::from_millis(500);
        let remote = Remote {
            idle,
            ..attach_to(tunnel.port)
        };
        let attached = connect(remote).await.unwrap();
        let here = attached.session;
        let lost = |s: &Session| {
            shown(s)
                .iter()
                .any(|e| matches!(e, Event::Info(t) if t.starts_with("lost ")))
        };
        tokio::time::sleep(idle + Duration::from_millis(500)).await;
        assert!(here.link().unwrap().connected() && !lost(&here));
        tunnel.freeze();
        served.session.publish(Event::Text("unseen".into()));
        let quiet = std::time::Instant::now();
        let wait = idle * 2;
        timeout(wait, async {
            while here.link().unwrap().connected() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the silent link is never dropped");
        // Counted from the last keep-alive, which came at most one interval before.
        assert!(
            quiet.elapsed() >= idle - KEEP_ALIVE,
            "{:?}",
            quiet.elapsed()
        );
        assert!(texts(&here).is_empty());
        tunnel.up();
        until("what was missed arrives", || texts(&here) == ["unseen"]).await;
    }

    fn errors(session: &Session) -> Vec<String> {
        shown(session)
            .into_iter()
            .filter_map(|e| match e {
                Event::Error(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    /// Start a turn there from here and park it on an approval, returning the approval's
    /// id and where its answer lands.
    async fn parked(served: &mut Served, here: &Session) -> (u64, oneshot::Receiver<Answer>) {
        here.submit("go".to_string()).unwrap();
        assert_eq!(served.prompts.recv().await.unwrap(), "go");
        let (reply, decided) = oneshot::channel();
        served
            .agent
            .send(AgentEvent::Approval {
                child: false,
                tool: "bash".into(),
                command: "touch x".into(),
                preview: None,
                offers: Offers::default(),
                reply,
            })
            .unwrap();
        until("the approval shows here", || here.state().pending.is_some()).await;
        (here.state().pending.unwrap().id, decided)
    }

    #[tokio::test]
    async fn attached_parent_interrupt_keeps_child_approvals() {
        let mut served = served().await;
        let attached = connect(attach_to(served.port)).await.unwrap();
        let here = attached.session;
        let (_, parent) = parked(&mut served, &here).await;
        let (reply, mut child) = oneshot::channel();
        served
            .agent
            .send(AgentEvent::Approval {
                child: true,
                tool: "bash".into(),
                command: "child command".into(),
                preview: None,
                offers: Offers::default(),
                reply,
            })
            .unwrap();
        assert!(here.interrupt_turn());
        assert_eq!(
            timeout(WAIT, parent).await.unwrap().unwrap(),
            Answer::Reject
        );
        until("the child approval stays", || {
            here.state()
                .pending
                .is_some_and(|a| a.command == "child command")
        })
        .await;
        assert!(matches!(
            child.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(here.answer(Answer::Accept(None), None).is_some());
        assert_eq!(
            timeout(WAIT, child).await.unwrap().unwrap(),
            Answer::Accept(None)
        );
    }

    /// While the link is down, prompts and an answer to the approval on screen are held,
    /// said to be, counted on the link, and sent in order once it is back; an interrupt
    /// is refused at once rather than sent late.
    #[tokio::test]
    async fn what_is_done_while_disconnected_is_held_or_refused() {
        let mut served = served().await;
        let tunnel = Tunnel::to(served.port).await;
        let attached = connect(attach_to(tunnel.port)).await.unwrap();
        let here = attached.session;
        let (id, decided) = parked(&mut served, &here).await;
        tunnel.down();
        until("the drop is noticed", || !here.link().unwrap().connected()).await;
        assert_eq!(here.answer(Answer::Accept(None), Some(id)), Some(id));
        here.submit("first".to_string()).unwrap();
        here.submit("second".to_string()).unwrap();
        assert!(here.interrupt());
        until("three are held", || here.link().unwrap().held() == 3).await;
        let infos: Vec<_> = shown(&here)
            .into_iter()
            .filter_map(|e| match e {
                Event::Info(t) if t.starts_with("held until reconnected") => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(infos.len(), 3, "{infos:?}");
        assert!(infos[1].ends_with("first"), "{infos:?}");
        until("the interrupt is refused", || {
            errors(&here)
                .iter()
                .any(|e| e.contains("interrupt was not sent"))
        })
        .await;
        tokio::time::sleep(RETRY).await;
        assert!(served.session.state().pending.is_some());

        tunnel.up();
        let answer = timeout(WAIT, decided).await.unwrap().unwrap();
        assert_eq!(answer, Answer::Accept(None));
        until("the prompts queue there", || {
            served.session.queued() == ["first", "second"]
        })
        .await;
        until("nothing is held", || here.link().unwrap().held() == 0).await;
        assert!(!shown(&served.session).contains(&Event::Interrupted));
        served.agent.send(AgentEvent::TurnEnd).unwrap();
        assert_eq!(served.prompts.recv().await.unwrap(), "first");
    }

    /// An answer held while the link was down, to an approval settled there meanwhile,
    /// is said not to have been used.
    #[tokio::test]
    async fn a_held_answer_to_a_settled_approval_is_reported() {
        let mut served = served().await;
        let tunnel = Tunnel::to(served.port).await;
        let attached = connect(attach_to(tunnel.port)).await.unwrap();
        let here = attached.session;
        let (id, decided) = parked(&mut served, &here).await;
        tunnel.down();
        until("the drop is noticed", || !here.link().unwrap().connected()).await;
        assert_eq!(here.answer(Answer::Reject, Some(id)), Some(id));
        assert_eq!(
            served.session.answer(Answer::Accept(None), Some(id)),
            Some(id)
        );
        assert_eq!(decided.await.unwrap(), Answer::Accept(None));
        tunnel.up();
        until("the late answer is reported", || {
            errors(&here).iter().any(|e| e.contains("was not used"))
        })
        .await;
        assert_eq!(here.link().unwrap().held(), 0);
    }

    /// Through a tunnel attach runs, the link is named for the host, and while it is
    /// down it says whether the tunnel or the server is what failed.
    #[tokio::test]
    async fn a_down_link_says_whether_the_tunnel_or_the_server_failed() {
        let served = served().await;
        let tunnel = Tunnel::to(served.port).await;
        let (state, rx) = watch::channel(tunnel::State::Up);
        let remote = attach_to(tunnel.port).through("ubuntu@host:7878".into(), rx);
        let attached = connect(remote).await.unwrap();
        let link = Arc::clone(attached.session.link().unwrap());
        assert_eq!(link.url, "ubuntu@host:7878");
        tunnel.down();
        state.send_replace(tunnel::State::Down("Connection refused".into()));
        until("the tunnel is blamed", || {
            link.down() == "ssh tunnel down: Connection refused"
        })
        .await;
        state.send_replace(tunnel::State::Up);
        until("the server is blamed", || {
            link.down() == "tunnel up, server not answering"
        })
        .await;
        tunnel.up();
        until("it is back", || link.connected()).await;
    }

    fn plan(step: &str) -> crate::plan::Plan {
        crate::plan::Plan {
            explanation: String::new(),
            steps: vec![crate::plan::Step {
                step: step.to_string(),
                status: crate::plan::Status::InProgress,
                reason: String::new(),
            }],
        }
    }

    fn usage(input: u64, output: u64) -> crate::client::Usage {
        crate::client::Usage {
            input,
            output,
            ..Default::default()
        }
    }

    fn started(id: &str, task: &str) -> AgentEvent {
        AgentEvent::ChildStarted {
            id: id.to_string(),
            identity: "general".into(),
            model: "child-model".into(),
            description: format!("{id} work"),
            task: task.to_string(),
        }
    }

    fn said(id: &str, text: &str) -> AgentEvent {
        AgentEvent::Child {
            id: id.to_string(),
            event: Box::new(AgentEvent::Text(text.to_string())),
        }
    }

    /// The served session mid-turn, with a child running and one finished, a plan, a
    /// schedule, a monitor sampled at least once, usage and a title, all before anyone
    /// attaches. Returns the schedule's id.
    async fn under_way(served: &mut Served) -> String {
        let dir = crate::tools::temp_dir();
        served
            .session
            .run_schedules(crate::schedules::Schedules::new(&dir, &dir, "attach-test"));
        let reminder = served
            .session
            .schedules()
            .unwrap()
            .remind("in 20m check CI")
            .unwrap();
        let monitors = served.session.monitors();
        monitors
            .add(crate::monitor::Spec {
                name: "bench".into(),
                command: "printf '%s' '{\"summary\":\"63 of 65\"}'".into(),
                workdir: std::env::temp_dir(),
                interval_secs: 1,
                timeout_secs: 2,
                hooks: vec![],
            })
            .unwrap();
        until("the monitor is sampled", || {
            monitors
                .views()
                .first()
                .is_some_and(|v| v.snapshot.is_some())
        })
        .await;
        served.session.submit("go".to_string()).unwrap();
        assert_eq!(served.prompts.recv().await.unwrap(), "go");
        for event in [
            AgentEvent::Titled("fixing ci".into()),
            AgentEvent::Usage(usage(1200, 300)),
            AgentEvent::Plan(Some(plan("build"))),
            started("a1", "look around"),
            said("a1", "halfway"),
            started("b2", "check it"),
            said("b2", "finished"),
            AgentEvent::ChildEnded {
                id: "b2".into(),
                ok: true,
            },
            AgentEvent::ChildUsage(usage(100, 50)),
        ] {
            served.agent.send(event).unwrap();
        }
        until("the scene is set there", || {
            served.session.children().len() == 2 && served.session.state().children.input == 100
        })
        .await;
        reminder.id
    }

    /// What one child's pane holds, each entry as its kind and text.
    fn pane(session: &Session, id: &str) -> Vec<(&'static str, String)> {
        session
            .child_entries(id)
            .map(|e| {
                let list = &e.lock().unwrap().list;
                list.iter()
                    .map(|e| (e.kind(), e.text().to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Everything the served session's own tui would show is there in the view as it
    /// attaches: the children and their panes, the plan, the monitor, the background
    /// list, the schedule, the totals, the working turn and the title.
    fn assert_shows_the_scene(app: &crate::app::App, session: &Session) {
        use crate::session::ChildState;
        let children: Vec<_> = app
            .children()
            .into_iter()
            .map(|row| (row.id, row.state))
            .collect();
        assert_eq!(
            children,
            [
                ("a1".to_string(), ChildState::Running),
                ("b2".to_string(), ChildState::Done)
            ]
        );
        assert_eq!(
            pane(session, "a1"),
            [
                ("user", "look around".to_string()),
                ("assistant", "halfway".to_string())
            ]
        );
        assert_eq!(app.plan(), Some(plan("build")));
        let monitors = app.monitors();
        assert_eq!(monitors.len(), 1);
        assert_eq!(monitors[0].name, "bench");
        let kinds: Vec<_> = session.background().into_iter().map(|r| r.kind).collect();
        use crate::background::Kind;
        assert_eq!(kinds, [Kind::Child, Kind::Schedule, Kind::Monitor]);
        assert_eq!(app.background, 3);
        assert_eq!(app.scheduled(), 1);
        assert_eq!((app.tokens_in, app.tokens_out), (1300, 350));
        assert_eq!(app.last_usage, Some(usage(1200, 300)));
        assert!(app.working);
        assert_eq!(app.title().as_deref(), Some("fixing ci"));
    }

    /// Attached to a session already under way, the view shows all of it at once; cut
    /// off while it moves on and reconnected, it shows where it got to, each thing once.
    #[tokio::test]
    async fn attach_shows_what_was_under_way_and_catches_up_after_a_drop() {
        let mut served = served().await;
        let reminder = under_way(&mut served).await;
        let tunnel = Tunnel::to(served.port).await;
        let attached = connect(attach_to(tunnel.port)).await.unwrap();
        let (here, mut resync) = (attached.session, attached.resync);
        let mut app = crate::app::App::new(Arc::clone(&here));
        assert_shows_the_scene(&app, &here);

        tunnel.down();
        until("the drop is noticed", || !here.link().unwrap().connected()).await;
        for event in [
            said("a1", " and on"),
            AgentEvent::ChildEnded {
                id: "a1".into(),
                ok: false,
            },
            started("c3", "one more"),
            AgentEvent::Usage(usage(2000, 10)),
            AgentEvent::Plan(Some(plan("ship"))),
        ] {
            served.agent.send(event).unwrap();
        }
        served
            .session
            .schedules()
            .unwrap()
            .cancel(&reminder)
            .unwrap();
        until("it moved on there", || served.session.children().len() == 3).await;
        while resync.try_recv().is_ok() {}
        tunnel.up();
        until("it caught up here", || {
            here.children().len() == 3 && here.plan() == Some(plan("ship"))
        })
        .await;
        until("the lists are read again", || {
            !here.background().iter().any(|r| r.id == reminder)
        })
        .await;
        timeout(WAIT, resync.recv()).await.unwrap();
        app.resync();
        assert_eq!(
            pane(&here, "a1"),
            [
                ("user", "look around".to_string()),
                ("assistant", "halfway and on".to_string())
            ]
        );
        assert_eq!(pane(&here, "c3"), [("user", "one more".to_string())]);
        let states: Vec<_> = here.children().into_iter().map(|r| r.state).collect();
        use crate::session::ChildState;
        assert_eq!(
            states,
            [ChildState::Failed, ChildState::Done, ChildState::Running]
        );
        assert_eq!((app.tokens_in, app.tokens_out), (3300, 360));
        assert_eq!(app.scheduled(), 0);
        let transcript = |s: &Session| serde_json::to_value(&s.entries().list).unwrap();
        // The notices said here are this view's own.
        let shown: Vec<_> = here
            .entries()
            .list
            .iter()
            .filter(|e| e.kind() != "info")
            .map(|e| e.text().to_string())
            .collect();
        assert_eq!(shown, ["go"]);

        // Down past what the log keeps, it is read again from a snapshot, the panes
        // with it.
        tunnel.down();
        until("the drop is noticed", || !here.link().unwrap().connected()).await;
        served.agent.send(said("c3", "deep")).unwrap();
        for n in 0..session::EVENT_LOG + 10 {
            served.session.publish(Event::Info(format!("{n}")));
        }
        tunnel.up();
        until("the snapshot is read again", || {
            pane(&here, "c3").len() == 2 && transcript(&here) == transcript(&served.session)
        })
        .await;
        assert_eq!(
            pane(&here, "c3"),
            [
                ("user", "one more".to_string()),
                ("assistant", "deep".to_string())
            ]
        );
        assert_eq!(pane(&here, "a1"), pane(&served.session, "a1"));
    }

    /// In front of `port`, a server as built before `/snapshot` carried the children
    /// and the title: everything else is passed through as it is.
    async fn older(port: u16) -> u16 {
        use axum::body::Body;
        let http = reqwest::Client::new();
        let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
            let http = http.clone();
            async move {
                let (parts, body) = request.into_parts();
                let path = parts
                    .uri
                    .path_and_query()
                    .map_or("/", |p| p.as_str())
                    .to_string();
                let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                let mut sent = http
                    .request(parts.method, format!("http://127.0.0.1:{port}{path}"))
                    .body(body);
                for (name, value) in &parts.headers {
                    if name != "host" {
                        sent = sent.header(name, value);
                    }
                }
                let response = sent.send().await.unwrap();
                let status = response.status();
                let reply = axum::response::Response::builder()
                    .status(status)
                    .header("content-type", response.headers()["content-type"].clone());
                if path == "/snapshot" {
                    let mut body: Value = response.json().await.unwrap();
                    let body = body.as_object_mut().unwrap();
                    body.retain(|key, _| key == "state" || key == "transcript");
                    return reply
                        .body(Body::from(Value::from(body.clone()).to_string()))
                        .unwrap();
                }
                reply
                    .body(Body::from_stream(response.bytes_stream()))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await });
        local
    }

    /// A server whose snapshot lacks the children is read through `/children` instead,
    /// and shows the same.
    #[tokio::test]
    async fn an_older_server_is_read_through_its_other_routes() {
        let mut served = served().await;
        under_way(&mut served).await;
        let older = older(served.port).await;
        let attached = connect(attach_to(older)).await.unwrap();
        let here = attached.session;
        let app = crate::app::App::new(Arc::clone(&here));
        let title = app.title();
        // The title is in no route such a server has.
        assert_eq!(title, None);
        here.publish(Event::Titled("fixing ci".into()));
        assert_shows_the_scene(&app, &here);
        served.agent.send(said("a1", " more")).unwrap();
        until("the child goes on here", || {
            pane(&here, "a1").last() == Some(&("assistant", "halfway more".to_string()))
        })
        .await;
    }

    /// The panes read before and after a snapshot bound what the stream replays: what
    /// moved between the reads is dropped from it, a start included.
    #[test]
    fn what_moved_around_the_snapshot_is_skipped_once() {
        use crate::session::ChildState;
        let log = |id: &str, state, steps| ChildLog {
            row: ChildRow {
                id: id.into(),
                identity: String::new(),
                model: String::new(),
                description: String::new(),
                state,
                steps,
                started: Instant::now(),
                last: Instant::now(),
            },
            entries: Vec::new(),
        };
        let before = [
            log("a", ChildState::Running, 4),
            log("b", ChildState::Done, 2),
        ];
        let after = [
            log("a", ChildState::Running, 6),
            log("b", ChildState::Running, 2),
            log("c", ChildState::Running, 1),
        ];
        let mut skips = skips(&before, &after);
        let child = |id: &str| Event::Child {
            id: id.into(),
            event: Box::new(Event::Text("x".into())),
        };
        let start = |id: &str| Event::ChildStarted {
            id: id.into(),
            identity: String::new(),
            model: String::new(),
            description: String::new(),
            task: String::new(),
        };
        let taken: Vec<bool> = [
            child("a"),
            child("a"),
            child("a"),
            start("b"),
            child("b"),
            start("c"),
            child("c"),
            child("c"),
        ]
        .iter()
        .map(|e| skips.take(e))
        .collect();
        assert_eq!(taken, [true, true, false, true, false, true, true, false]);
        assert_eq!(skips, Skips::default());
        assert_eq!(super::skips(&after, &after), Skips::default());
    }
}
