//! Remote control over an outbound laptop connection to a trusted relay.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path as RoutePath, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use futures_util::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as ClientMessage;

use crate::{server, session::Session};

const LIMIT: usize = 16 * 1024 * 1024;
const CAPACITY: usize = 4096;
const AUTH_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const NONCE_TTL: Duration = Duration::from_secs(60);
const HEARTBEAT: Duration = Duration::from_secs(5);
const DEAD: Duration = Duration::from_secs(20);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn failure(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}

/// The permanent machine key never leaves the laptop except through explicit pairing.
pub struct Identity {
    key: rcgen::KeyPair,
}

impl Identity {
    pub fn load(path: &Path) -> Result<Self> {
        use std::io::{Read as _, Write as _};
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        if !path.exists() {
            let parent = path
                .parent()
                .context("control key needs a parent directory")?;
            std::fs::create_dir_all(parent)?;
            let key = rcgen::KeyPair::generate()?;
            let temporary = parent.join(format!(".control-{}.tmp", server::mint()));
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)?;
            let result = (|| -> Result<()> {
                file.write_all(key.serialize_pem().as_bytes())?;
                file.sync_all()?;
                match std::fs::hard_link(&temporary, path) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                    Err(error) => Err(error.into()),
                }
            })();
            let _ = std::fs::remove_file(&temporary);
            result?;
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
            bail!(
                "{} must be a regular file readable only by you (chmod 600)",
                path.display()
            );
        }
        let mut pem = String::new();
        file.read_to_string(&mut pem)?;
        let key = rcgen::KeyPair::from_pem(&pem).context("cannot read the machine control key")?;
        if key.algorithm() != &rcgen::PKCS_ECDSA_P256_SHA256 {
            bail!("the machine control key must be ECDSA P-256");
        }
        crate::redact::register(&pem);
        crate::redact::register(&B64.encode(key.serialized_der()));
        Ok(Self { key })
    }

    pub fn machine() -> Result<Self> {
        let home = std::env::var_os("HOME").context("remote control needs HOME")?;
        Self::load(&PathBuf::from(home).join(".bhai/control-key.pem"))
    }

    pub fn pairing(&self, relay: &str) -> String {
        json!({"v":1,"relay":relay,"key":B64.encode(self.key.serialized_der())}).to_string()
    }

    pub fn qr(&self, relay: &str) -> Result<Vec<String>> {
        let qr = qrcodegen::QrCode::encode_text(&self.pairing(relay), qrcodegen::QrCodeEcc::Medium)
            .map_err(|_| anyhow::anyhow!("relay address is too long for the pairing QR"))?;
        let mut rows = Vec::new();
        for y in (-4..qr.size() + 4).step_by(2) {
            let row = (-4..qr.size() + 4)
                .map(|x| match (qr.get_module(x, y), qr.get_module(x, y + 1)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                })
                .collect();
            rows.push(row);
        }
        Ok(rows)
    }

    fn auth(&self, nonce: String, role: &str) -> Result<Auth> {
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let key = provider.key_provider.load_private_key(
            rustls::pki_types::PrivatePkcs8KeyDer::from(self.key.serialize_der()).into(),
        )?;
        let signer = key
            .choose_scheme(&[rustls::SignatureScheme::ECDSA_NISTP256_SHA256])
            .context("control key cannot sign ECDSA P-256")?;
        let signature = signer.sign(challenge(role, &nonce).as_bytes())?;
        Ok(Auth {
            nonce,
            role: role.to_string(),
            public_key: B64.encode(self.key.public_key_raw()),
            signature: B64.encode(signature),
        })
    }
}

/// HTTPS by default; plaintext is only permitted on loopback for offline tests/tunnels.
pub fn address(given: &str) -> Result<String> {
    let text = if given.contains("://") {
        given.to_string()
    } else {
        format!("https://{given}")
    };
    let url = url::Url::parse(&text).context("invalid relay address")?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        bail!("the relay address must be an origin, without credentials, path, query or fragment");
    }
    match (url.scheme(), url.host_str()) {
        ("https", Some(_)) | ("http", Some("127.0.0.1" | "localhost" | "[::1]")) => {}
        _ => bail!("remote control requires HTTPS (HTTP is only allowed on loopback)"),
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn challenge(role: &str, nonce: &str) -> String {
    format!("bhai-rc-v1\n{role}\n{nonce}")
}

#[derive(Serialize, Deserialize)]
struct Auth {
    nonce: String,
    role: String,
    public_key: String,
    signature: String,
}

#[derive(Clone)]
struct Grant {
    machine: String,
    role: String,
    expires: Instant,
}

#[derive(Default)]
struct Hub {
    nonces: Mutex<HashMap<String, Instant>>,
    grants: Mutex<HashMap<String, Grant>>,
    sessions: Mutex<HashMap<(String, String), Arc<Link>>>,
    requests: Mutex<usize>,
}

struct Permit(Arc<Hub>);

impl Drop for Permit {
    fn drop(&mut self) {
        *lock(&self.0.requests) -= 1;
    }
}

struct Link {
    generation: String,
    title: String,
    project: String,
    sender: mpsc::Sender<WireRequest>,
    pending: Mutex<HashMap<String, oneshot::Sender<WireReply>>>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
struct WireRequest {
    id: String,
    method: String,
    path: String,
    body: String,
    content_type: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct WireReply {
    id: String,
    status: u16,
    body: String,
    content_type: String,
}

impl WireReply {
    fn error(id: String, status: u16, message: &str) -> Self {
        Self {
            id,
            status,
            body: B64.encode(json!({"error":message}).to_string()),
            content_type: "application/json".into(),
        }
    }
}

/// The public listener should sit behind a TLS-terminating reverse proxy.
pub async fn serve(bind: std::net::SocketAddr) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!(
        "bhai: relay on {} (put HTTPS in front of this listener)",
        listener.local_addr()?
    );
    axum::serve(listener, router(Arc::new(Hub::default()))).await?;
    Ok(())
}

fn router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/", get(index))
        .route(
            "/static/jsQR.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../assets/vendor/jsQR.js"),
                )
            }),
        )
        .route(
            "/static/jsQR.LICENSE",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    include_str!("../assets/vendor/jsQR.LICENSE"),
                )
            }),
        )
        .route(
            "/static/qrcode-generator.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../assets/vendor/qrcode-generator.js"),
                )
            }),
        )
        .route(
            "/static/qrcode-generator.LICENSE",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    include_str!("../assets/vendor/qrcode-generator.LICENSE"),
                )
            }),
        )
        .route("/health", get(|| async { Json(json!({"ok":true})) }))
        .route("/api/challenge", get(nonce))
        .route(
            "/api/auth",
            post(authenticate).layer(DefaultBodyLimit::max(4096)),
        )
        .route("/api/sessions", get(list))
        .route("/api/sessions/{session}/{*path}", get(proxy).post(proxy))
        .route("/connect", get(connect))
        .layer(DefaultBodyLimit::max(LIMIT))
        .layer(axum::middleware::from_fn(security))
        .with_state(hub)
}

async fn index() -> Response {
    Html(include_str!("../assets/remote.html")).into_response()
}

async fn security(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    // Browser requests are same-origin. Native laptop requests have no Origin.
    if let Some(origin) = request.headers().get(header::ORIGIN) {
        let valid = origin
            .to_str()
            .ok()
            .and_then(|origin| url::Url::parse(origin).ok())
            .is_some_and(|origin| {
                let authority =
                    origin[url::Position::BeforeHost..url::Position::AfterPort].to_string();
                request
                    .headers()
                    .get(header::HOST)
                    .and_then(|host| host.to_str().ok())
                    == Some(authority.as_str())
                    && matches!(origin.scheme(), "http" | "https")
            });
        if !valid {
            return failure(StatusCode::FORBIDDEN, "cross-origin requests are refused");
        }
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("referrer-policy", "no-referrer".parse().unwrap());
    headers.insert("content-security-policy", "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' blob: data:; media-src 'self' blob:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'".parse().unwrap());
    response
}

async fn nonce(State(hub): State<Arc<Hub>>) -> Response {
    let now = Instant::now();
    let mut nonces = lock(&hub.nonces);
    nonces.retain(|_, expires| *expires > now);
    if nonces.len() >= CAPACITY {
        return failure(
            StatusCode::TOO_MANY_REQUESTS,
            "too many authentication attempts",
        );
    }
    let nonce = format!("{}{}", server::mint(), server::mint());
    nonces.insert(nonce.clone(), now + NONCE_TTL);
    Json(json!({"nonce":nonce})).into_response()
}

async fn authenticate(State(hub): State<Arc<Hub>>, Json(auth): Json<Auth>) -> Response {
    let now = Instant::now();
    let expires = lock(&hub.nonces).remove(&auth.nonce);
    if expires.is_none_or(|expires| expires <= now)
        || !matches!(auth.role.as_str(), "laptop" | "controller")
    {
        return failure(StatusCode::UNAUTHORIZED, "expired or invalid challenge");
    }
    let Some(machine) = verified(&auth) else {
        return failure(StatusCode::UNAUTHORIZED, "invalid signature");
    };
    let token = format!("{}{}", server::mint(), server::mint());
    let mut grants = lock(&hub.grants);
    grants.retain(|_, grant| grant.expires > now);
    if grants.len() >= CAPACITY {
        return failure(
            StatusCode::TOO_MANY_REQUESTS,
            "too many authenticated connections",
        );
    }
    grants.insert(
        token.clone(),
        Grant {
            machine: machine.clone(),
            role: auth.role,
            expires: now + AUTH_TTL,
        },
    );
    Json(json!({"token":token,"machine":machine})).into_response()
}

fn verified(auth: &Auth) -> Option<String> {
    let public = B64.decode(&auth.public_key).ok()?;
    let signature = B64.decode(&auth.signature).ok()?;
    if public.len() != 65 || public[0] != 4 || signature.len() > 80 {
        return None;
    }
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let message = challenge(&auth.role, &auth.nonce);
    let valid = provider
        .signature_verification_algorithms
        .all
        .iter()
        .any(|algorithm| {
            algorithm
                .verify_signature(&public, message.as_bytes(), &signature)
                .is_ok()
        });
    valid.then(|| format!("{:x}", Sha256::digest(&public)))
}

fn authorized(hub: &Hub, headers: &HeaderMap, role: &str) -> Option<Grant> {
    let token = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    let grant = lock(&hub.grants).get(token)?.clone();
    (grant.role == role && grant.expires > Instant::now()).then_some(grant)
}

async fn list(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    let Some(grant) = authorized(&hub, &headers, "controller") else {
        return failure(StatusCode::UNAUTHORIZED, "authenticate again");
    };
    let mut rows: Vec<Value> = lock(&hub.sessions).iter()
        .filter(|((machine, _), _)| *machine == grant.machine)
        .map(|((_, id), link)| json!({"id":id,"title":link.title,"project":link.project,"connected":true})).collect();
    rows.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Json(json!({"sessions":rows})).into_response()
}

#[derive(Deserialize)]
struct Registration {
    session: String,
    title: String,
    project: String,
}

async fn connect(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Query(registration): Query<Registration>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(grant) = authorized(&hub, &headers, "laptop") else {
        return failure(StatusCode::UNAUTHORIZED, "authenticate again");
    };
    if uuid::Uuid::parse_str(&registration.session).is_err()
        || registration.title.len() > 1024
        || registration.project.len() > 4096
    {
        return failure(StatusCode::BAD_REQUEST, "invalid session registration");
    }
    if let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.strip_prefix("Bearer "))
    {
        lock(&hub.grants).remove(token);
    }
    upgrade
        .max_message_size(LIMIT * 2)
        .max_frame_size(LIMIT * 2)
        .on_upgrade(move |socket| connected(hub, grant, registration, socket))
        .into_response()
}

async fn connected(hub: Arc<Hub>, grant: Grant, registration: Registration, mut socket: WebSocket) {
    let (sender, mut receiver) = mpsc::channel(32);
    let link = Arc::new(Link {
        generation: server::mint(),
        title: registration.title,
        project: registration.project,
        sender,
        pending: Mutex::new(HashMap::new()),
    });
    let key = (grant.machine, registration.session);
    {
        let mut sessions = lock(&hub.sessions);
        if !sessions.contains_key(&key) && sessions.len() >= 128 {
            return;
        }
        sessions.insert(key.clone(), Arc::clone(&link));
    }
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    let mut heard = Instant::now();
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if heard.elapsed() > DEAD || grant.expires <= Instant::now() || socket.send(Message::Ping(Bytes::new())).await.is_err() { break; }
            }
            request = receiver.recv() => {
                let Some(request) = request else { break; };
                let Ok(text) = serde_json::to_string(&request) else { break; };
                if socket.send(Message::Text(text.into())).await.is_err() { break; }
            }
            message = socket.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        heard = Instant::now();
                        let Ok(reply) = serde_json::from_str::<WireReply>(&text) else { break; };
                        if let Some(waiting) = lock(&link.pending).remove(&reply.id) { let _ = waiting.send(reply); }
                    }
                    Some(Ok(Message::Pong(_))) => heard = Instant::now(),
                    Some(Ok(Message::Ping(data))) => { heard = Instant::now(); if socket.send(Message::Pong(data)).await.is_err() { break; } }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => break,
                }
            }
        }
    }
    let mut sessions = lock(&hub.sessions);
    if sessions
        .get(&key)
        .is_some_and(|current| current.generation == link.generation)
    {
        sessions.remove(&key);
    }
    lock(&link.pending).clear();
}

async fn proxy(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    RoutePath((session, path)): RoutePath<(String, String)>,
    method: Method,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let Some(grant) = authorized(&hub, &headers, "controller") else {
        return failure(StatusCode::UNAUTHORIZED, "authenticate again");
    };
    let _permit = {
        let mut requests = lock(&hub.requests);
        if *requests >= 32 {
            return failure(StatusCode::TOO_MANY_REQUESTS, "relay is busy");
        }
        *requests += 1;
        Permit(Arc::clone(&hub))
    };
    let link = lock(&hub.sessions).get(&(grant.machine, session)).cloned();
    let Some(link) = link else {
        return failure(StatusCode::NOT_FOUND, "session is offline");
    };
    if !allowed(&path, method.as_str()) {
        return failure(StatusCode::NOT_FOUND, "unknown remote-control route");
    }
    let id = headers
        .get("x-bhai-request-id")
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    let id = if id.is_empty() {
        server::mint()
    } else {
        id.to_string()
    };
    if uuid::Uuid::parse_str(&id).is_err() {
        return failure(StatusCode::BAD_REQUEST, "request id must be a UUID");
    }
    let path = match uri.query() {
        Some(query) => format!("/{path}?{query}"),
        None => format!("/{path}"),
    };
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let request = WireRequest {
        id: id.clone(),
        method: method.to_string(),
        path,
        body: B64.encode(&body),
        content_type,
    };
    let (sender, receiver) = oneshot::channel();
    {
        let mut pending = lock(&link.pending);
        if pending.len() >= 32 || pending.contains_key(&id) {
            return failure(
                StatusCode::CONFLICT,
                "request is already pending or session is busy",
            );
        }
        pending.insert(id.clone(), sender);
    }
    let response = async {
        link.sender.send(request).await.ok()?;
        receiver.await.ok()
    };
    let result = tokio::time::timeout(Duration::from_secs(30), response).await;
    lock(&link.pending).remove(&id);
    match result {
        Ok(Some(reply)) => {
            let Ok(body) = B64.decode(&reply.body) else {
                return failure(StatusCode::BAD_GATEWAY, "bad laptop response");
            };
            let status = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut response = (status, body).into_response();
            if let Ok(content_type) = reply.content_type.parse() {
                response
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, content_type);
            }
            response
        }
        _ => failure(
            StatusCode::GATEWAY_TIMEOUT,
            "laptop disconnected or response timed out; check session state before retrying",
        ),
    }
}

fn allowed(path: &str, method: &str) -> bool {
    if path.contains(['?', '#', '%', '\\'])
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        return false;
    }
    match method {
        "GET" => matches!(
            path,
            "state"
                | "snapshot"
                | "context"
                | "permissions"
                | "children"
                | "schedules"
                | "monitors"
                | "background"
                | "file"
        ),
        "POST" => {
            matches!(
                path,
                "prompt"
                    | "shell"
                    | "shell/background"
                    | "queue/clear"
                    | "approve"
                    | "reject"
                    | "interrupt"
                    | "interrupt/turn"
                    | "retry"
                    | "resume"
                    | "goal"
                    | "mode"
                    | "model"
                    | "fast"
                    | "allow"
                    | "trust"
                    | "untrust"
                    | "steer"
                    | "schedule"
                    | "schedule/cancel"
                    | "background/kill"
                    | "upload"
            ) || path
                .strip_prefix("children/")
                .and_then(|path| path.strip_suffix("/interrupt"))
                .is_some_and(|id| !id.is_empty() && !id.contains('/'))
        }
        _ => false,
    }
}

pub enum Change {
    Enable(String),
    Disable,
}

/// Relay tasks belong to the worker, not to a terminal attachment.
#[derive(Default)]
pub struct Controller {
    address: Option<String>,
    control: Option<Control>,
}

impl Controller {
    pub fn new(address: Option<String>) -> Self {
        Self {
            address,
            control: None,
        }
    }

    pub fn address(&self) -> Option<&str> {
        self.address.as_deref()
    }

    pub fn active(&self) -> bool {
        self.control.is_some()
    }

    pub fn disable(&mut self) {
        self.control = None;
    }

    pub async fn enable(&mut self, given: &str, session: Arc<Session>, id: String) -> Result<()> {
        let relay = address(given)?;
        if self.active() && self.address.as_deref() == Some(&relay) {
            return Ok(());
        }
        self.open(relay, Identity::machine()?, session, id).await
    }

    async fn open(
        &mut self,
        relay: String,
        identity: Identity,
        session: Arc<Session>,
        id: String,
    ) -> Result<()> {
        let control = Control::start(relay.clone(), identity, session, id).await?;
        self.control = Some(control);
        self.address = Some(relay);
        Ok(())
    }
}

/// Owns both the loopback API and the reconnecting outbound connection.
pub struct Control {
    local: tokio::task::JoinHandle<Result<()>>,
    remote: tokio::task::JoinHandle<()>,
}

impl Drop for Control {
    fn drop(&mut self) {
        self.remote.abort();
        self.local.abort();
    }
}

impl Control {
    pub async fn start(
        relay: String,
        identity: Identity,
        session: Arc<Session>,
        id: String,
    ) -> Result<Self> {
        let listener = server::bind(0).await?;
        let local = format!("http://{}", listener.local_addr()?);
        let token = server::mint();
        crate::redact::register(&token);
        let registration = Registration {
            session: id,
            title: session.title().unwrap_or_else(|| "bhai".into()),
            project: session.roots().0.display().to_string(),
        };
        let local_task = tokio::spawn(server::serve(listener, session, token.clone()));
        let remote = tokio::spawn(async move {
            let client = reqwest::Client::builder()
                .http1_only()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .build();
            let Ok(client) = client else {
                return;
            };
            let mut seen = Replies::default();
            loop {
                let _ = carry(
                    &client,
                    &relay,
                    &identity,
                    &registration,
                    &local,
                    &token,
                    &mut seen,
                )
                .await;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        Ok(Self {
            local: local_task,
            remote,
        })
    }
}

#[derive(Default)]
struct Replies {
    rows: VecDeque<(String, [u8; 32], WireReply)>,
}

fn fingerprint(request: &WireRequest) -> [u8; 32] {
    let mut hash = Sha256::new();
    for field in [
        &request.method,
        &request.path,
        &request.content_type,
        &request.body,
    ] {
        hash.update((field.len() as u64).to_le_bytes());
        hash.update(field.as_bytes());
    }
    hash.finalize().into()
}

impl Replies {
    fn get(&self, request: &WireRequest) -> Option<WireReply> {
        self.rows
            .iter()
            .find(|(id, _, _)| *id == request.id)
            .map(|(_, hash, reply)| {
                if *hash == fingerprint(request) {
                    reply.clone()
                } else {
                    WireReply::error(
                        request.id.clone(),
                        409,
                        "request id was used for a different action",
                    )
                }
            })
    }
    fn insert(&mut self, request: WireRequest, reply: WireReply) {
        if self.rows.len() >= 512 {
            self.rows.pop_front();
        }
        let hash = fingerprint(&request);
        let reply = if reply.body.len() > 64 * 1024 {
            WireReply::error(
                request.id.clone(),
                409,
                "action already completed; refetch session state for its result",
            )
        } else {
            reply
        };
        self.rows.push_back((request.id, hash, reply));
    }
}

async fn carry(
    client: &reqwest::Client,
    relay: &str,
    identity: &Identity,
    registration: &Registration,
    local: &str,
    token: &str,
    seen: &mut Replies,
) -> Result<()> {
    let nonce: Value = client
        .get(format!("{relay}/api/challenge"))
        .timeout(Duration::from_secs(15))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let auth = identity.auth(
        nonce["nonce"]
            .as_str()
            .context("relay returned no challenge")?
            .to_string(),
        "laptop",
    )?;
    let grant: Value = client
        .post(format!("{relay}/api/auth"))
        .json(&auth)
        .timeout(Duration::from_secs(15))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let bearer = grant["token"]
        .as_str()
        .context("relay returned no authorization")?;
    let (key, accept) = crate::websocket::key();
    let mut url = url::Url::parse(&format!("{relay}/connect"))?;
    url.query_pairs_mut().extend_pairs([
        ("session", &registration.session),
        ("title", &registration.title),
        ("project", &registration.project),
    ]);
    let response = client
        .get(url)
        .bearer_auth(bearer)
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", key)
        .timeout(Duration::from_secs(15))
        .send()
        .await?;
    let mut connection = crate::websocket::open(response, &accept)
        .await
        .map_err(|_| anyhow::anyhow!("relay WebSocket upgrade failed"))?;
    loop {
        let frame = tokio::time::timeout(DEAD, connection.ws.next())
            .await?
            .context("relay disconnected")??;
        match frame {
            ClientMessage::Text(text) => {
                let request: WireRequest = serde_json::from_str(&text)?;
                let reply = match seen.get(&request) {
                    Some(reply) => reply,
                    None => {
                        let reply = dispatch(client, local, token, &request).await;
                        // Reads do not evict action receipts on a busy polling controller.
                        if request.method == "POST" {
                            seen.insert(request, reply.clone());
                        }
                        reply
                    }
                };
                connection
                    .ws
                    .send(ClientMessage::Text(serde_json::to_string(&reply)?.into()))
                    .await?;
            }
            ClientMessage::Ping(data) => connection.ws.send(ClientMessage::Pong(data)).await?,
            ClientMessage::Pong(_) => {}
            ClientMessage::Close(_) => break,
            _ => bail!("unexpected relay frame"),
        }
    }
    Ok(())
}

async fn dispatch(
    client: &reqwest::Client,
    local: &str,
    token: &str,
    request: &WireRequest,
) -> WireReply {
    let route = request
        .path
        .strip_prefix('/')
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default();
    if !allowed(route, &request.method) || uuid::Uuid::parse_str(&request.id).is_err() {
        return WireReply::error(request.id.clone(), 400, "invalid remote-control request");
    }
    let result = async {
        let body = B64.decode(&request.body)?;
        if body.len() > LIMIT {
            bail!("request too large");
        }
        let method = reqwest::Method::from_bytes(request.method.as_bytes())?;
        let response = client
            .request(method, format!("{local}{}", request.path))
            .header(server::TOKEN_HEADER, token)
            .header(header::CONTENT_TYPE, &request.content_type)
            .body(body)
            .timeout(Duration::from_secs(20))
            .send()
            .await?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len() + chunk.len() > LIMIT {
                bail!("response too large");
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok::<_, anyhow::Error>(WireReply {
            id: request.id.clone(),
            status,
            body: B64.encode(&bytes),
            content_type,
        })
    }
    .await;
    result.unwrap_or_else(|_| {
        WireReply::error(request.id.clone(), 502, "local session request failed")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        Identity {
            key: rcgen::KeyPair::generate().unwrap(),
        }
    }

    #[tokio::test]
    async fn runtime_controller_starts_once_and_stops_without_ending_the_session() {
        let (tx_user, mut user) = tokio::sync::mpsc::channel(1);
        let (tx_control, mut controls) = tokio::sync::mpsc::channel(1);
        let session = Session::new(
            "test".into(),
            "low".into(),
            "general".into(),
            tx_user,
            tx_control,
            Arc::default(),
            Arc::default(),
            None,
        );
        let listener = server::bind(0).await.unwrap();
        let relay = format!("http://{}", listener.local_addr().unwrap());
        let hub = Arc::new(Hub::default());
        let task = tokio::spawn(async move {
            axum::serve(listener, router(hub)).await.unwrap();
        });
        let mut controller = Controller::new(Some(relay.clone()));
        assert!(!controller.active());
        controller
            .open(
                relay.clone(),
                identity(),
                Arc::clone(&session),
                "runtime-session".into(),
            )
            .await
            .unwrap();
        assert!(controller.active());
        let local = controller.control.as_ref().unwrap().local.abort_handle();
        let remote = controller.control.as_ref().unwrap().remote.abort_handle();
        let task_id = controller.control.as_ref().unwrap().remote.id();
        controller
            .enable(&relay, Arc::clone(&session), "runtime-session".into())
            .await
            .unwrap();
        assert_eq!(controller.control.as_ref().unwrap().remote.id(), task_id);
        assert!(
            controller
                .enable(
                    "http://unsafe.example.com",
                    Arc::clone(&session),
                    "runtime-session".into()
                )
                .await
                .is_err()
        );
        assert!(controller.active());
        controller.disable();
        tokio::task::yield_now().await;
        assert!(!controller.active());
        assert_eq!(controller.address(), Some(relay.as_str()));
        assert!(local.is_finished() && remote.is_finished());
        assert!(user.try_recv().is_err());
        assert!(controls.try_recv().is_err());
        task.abort();
    }

    #[test]
    fn signature_binds_role_nonce_and_machine() {
        let first = identity();
        let mut auth = first.auth("nonce".into(), "controller").unwrap();
        assert!(verified(&auth).is_some());
        let other = identity().auth("nonce".into(), "controller").unwrap();
        assert_ne!(verified(&auth), verified(&other));
        auth.role = "laptop".into();
        assert!(verified(&auth).is_none());
        auth.role = "controller".into();
        auth.nonce = "another".into();
        assert!(verified(&auth).is_none());
    }

    #[test]
    fn pairing_qr_preserves_modules_and_a_four_module_quiet_zone() {
        let identity = identity();
        let relay = "https://relay.example.com";
        let pairing: Value = serde_json::from_str(&identity.pairing(relay)).unwrap();
        assert_eq!(pairing["v"], 1);
        assert_eq!(pairing["relay"], relay);
        assert!(pairing.get("public_key").is_none());
        assert_eq!(
            B64.decode(pairing["key"].as_str().unwrap()).unwrap(),
            identity.key.serialize_der()
        );
        let expected =
            qrcodegen::QrCode::encode_text(&identity.pairing(relay), qrcodegen::QrCodeEcc::Medium)
                .unwrap();
        let rows = identity.qr(relay).unwrap();
        assert!(rows[0].chars().all(|cell| cell == ' '));
        for (row, line) in rows.iter().enumerate() {
            assert_eq!(line.chars().count(), expected.size() as usize + 8);
            for (column, cell) in line.chars().enumerate() {
                let bits = match cell {
                    '█' => (true, true),
                    '▀' => (true, false),
                    '▄' => (false, true),
                    ' ' => (false, false),
                    _ => panic!("invalid QR cell"),
                };
                let x = column as i32 - 4;
                let y = row as i32 * 2 - 4;
                assert_eq!(
                    bits,
                    (expected.get_module(x, y), expected.get_module(x, y + 1))
                );
            }
        }
    }

    #[test]
    fn machine_keys_are_atomic_stable_and_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = std::env::temp_dir().join(format!("relay-key-{}", server::mint()));
        let path = root.join("control-key.pem");
        let public: Vec<String> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..4)
                .map(|_| {
                    let path = &path;
                    scope.spawn(move || {
                        B64.encode(Identity::load(path).unwrap().key.public_key_raw())
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect()
        });
        assert!(public.iter().all(|key| key == &public[0]));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let linked = root.join("linked.pem");
        std::os::unix::fs::symlink(&path, &linked).unwrap();
        assert!(Identity::load(&linked).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Identity::load(&path).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn addresses_require_tls_except_on_loopback() {
        assert_eq!(
            address("relay.example.com").unwrap(),
            "https://relay.example.com"
        );
        assert!(address("http://127.0.0.1:8080").is_ok());
        for bad in [
            "http://example.com",
            "https://user:password@example.com",
            "https://example.com/path",
            "https://example.com?key=secret",
        ] {
            assert!(address(bad).is_err());
        }
    }

    #[test]
    fn receipts_deduplicate_actions_and_refuse_changed_requests() {
        let request = WireRequest {
            id: server::mint(),
            method: "POST".into(),
            path: "/prompt".into(),
            body: B64.encode("{}"),
            content_type: "application/json".into(),
        };
        let reply = WireReply::error(request.id.clone(), 200, "accepted");
        let mut seen = Replies::default();
        seen.insert(request.clone(), reply.clone());
        assert_eq!(seen.get(&request).unwrap().body, reply.body);
        let mut changed = request;
        changed.path = "/approve".into();
        assert_eq!(seen.get(&changed).unwrap().status, 409);
    }

    #[test]
    fn routes_cannot_escape_the_session_api() {
        assert!(allowed("prompt", "POST"));
        assert!(allowed("children/abc/interrupt", "POST"));
        for path in [
            "../health",
            "prompt?anything",
            "//example.com",
            "children/../../interrupt",
            "%2e%2e/prompt",
            "events",
        ] {
            assert!(!allowed(path, "GET"));
            assert!(!allowed(path, "POST"));
        }
    }

    async fn login(client: &reqwest::Client, base: &str, key: &Identity, role: &str) -> String {
        let nonce: Value = client
            .get(format!("{base}/api/challenge"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let auth = key
            .auth(nonce["nonce"].as_str().unwrap().into(), role)
            .unwrap();
        let response: Value = client
            .post(format!("{base}/api/auth"))
            .json(&auth)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        response["token"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn real_transport_scopes_sessions_reconnects_and_deduplicates_actions() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let relay_listener = server::bind(0).await.unwrap();
        let base = format!("http://{}", relay_listener.local_addr().unwrap());
        let hub = Arc::new(Hub::default());
        let relay_task = tokio::spawn(async move {
            axum::serve(relay_listener, router(hub)).await.unwrap();
        });
        let count = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&count);
        let local = Router::new()
            .route(
                "/snapshot",
                get(|| async { Json(json!({"state":{"working":false},"transcript":[]})) }),
            )
            .route(
                "/prompt",
                post(move || {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Json(json!({"ok":true}))
                    }
                }),
            );
        let local_listener = server::bind(0).await.unwrap();
        let local_base = format!("http://{}", local_listener.local_addr().unwrap());
        let local_task = tokio::spawn(async move {
            axum::serve(local_listener, local).await.unwrap();
        });
        let key = identity();
        let controller = Identity {
            key: rcgen::KeyPair::from_pem(&key.key.serialize_pem()).unwrap(),
        };
        let session = uuid::Uuid::new_v4().to_string();
        let registration = Registration {
            session: session.clone(),
            title: "test".into(),
            project: "/test".into(),
        };
        let (disconnect, mut disconnected) = mpsc::channel(1);
        let relay_base = base.clone();
        let laptop = tokio::spawn(async move {
            let client = reqwest::Client::builder().http1_only().build().unwrap();
            let mut seen = Replies::default();
            loop {
                tokio::select! {
                    _ = carry(&client, &relay_base, &key, &registration, &local_base, "local-token", &mut seen) => {},
                    _ = disconnected.recv() => {},
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let client = reqwest::Client::new();
        let token = login(&client, &base, &controller, "controller").await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let list: Value = client
                    .get(format!("{base}/api/sessions"))
                    .bearer_auth(&token)
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if list["sessions"].as_array().unwrap().len() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let route = format!("{base}/api/sessions/{session}");
        let snapshot: Value = client
            .get(format!("{route}/snapshot"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(snapshot["state"]["working"], false);
        let stranger = login(&client, &base, &identity(), "controller").await;
        let hidden: Value = client
            .get(format!("{base}/api/sessions"))
            .bearer_auth(&stranger)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(hidden["sessions"].as_array().unwrap().is_empty());
        assert_eq!(
            client
                .get(format!("{route}/snapshot"))
                .bearer_auth(&stranger)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        let id = uuid::Uuid::new_v4().to_string();
        let send = || {
            client
                .post(format!("{route}/prompt"))
                .bearer_auth(&token)
                .header("x-bhai-request-id", &id)
                .json(&json!({"text":"hello","id":id}))
        };
        assert_eq!(send().send().await.unwrap().status(), StatusCode::OK);
        assert_eq!(send().send().await.unwrap().status(), StatusCode::OK);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        disconnect.send(()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(send().send().await.unwrap().status(), StatusCode::OK);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(
            client
                .post(format!("{route}/prompt"))
                .bearer_auth(&token)
                .header("x-bhai-request-id", &id)
                .json(&json!({"text":"changed"}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        laptop.abort();
        relay_task.abort();
        local_task.abort();
    }

    #[tokio::test]
    async fn controller_drives_real_session_prompts_and_permission_decisions() {
        use crate::agent::{AgentEvent, Cancel};
        use crate::permissions::{Answer, Offers, Policy};
        let listener = server::bind(0).await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let relay_task = tokio::spawn(async move {
            axum::serve(listener, router(Arc::new(Hub::default())))
                .await
                .unwrap();
        });
        let (tx_user, mut user) = mpsc::channel(1);
        let (tx_control, _control) = mpsc::channel(1);
        let (agent, rx_agent) = mpsc::unbounded_channel();
        let session = Session::new(
            "test-model".into(),
            "medium".into(),
            "general".into(),
            tx_user,
            tx_control,
            Arc::new(Cancel::default()),
            Arc::new(Policy::default()),
            None,
        );
        let pump = tokio::spawn(crate::session::pump(Arc::clone(&session), rx_agent));
        let laptop_key = identity();
        let controller_key = Identity {
            key: rcgen::KeyPair::from_pem(&laptop_key.key.serialize_pem()).unwrap(),
        };
        let id = uuid::Uuid::new_v4().to_string();
        let remote = Control::start(base.clone(), laptop_key, Arc::clone(&session), id.clone())
            .await
            .unwrap();
        let client = reqwest::Client::new();
        let token = login(&client, &base, &controller_key, "controller").await;
        let route = format!("{base}/api/sessions/{id}");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let response = client
                    .get(format!("{route}/state"))
                    .bearer_auth(&token)
                    .send()
                    .await
                    .unwrap();
                if response.status() == StatusCode::OK {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let prompt_id = uuid::Uuid::new_v4().to_string();
        let response = client
            .post(format!("{route}/prompt"))
            .bearer_auth(&token)
            .header("x-bhai-request-id", &prompt_id)
            .json(&json!({"text":"do work","id":prompt_id}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), user.recv())
                .await
                .unwrap()
                .is_some()
        );
        for approve in [false, true] {
            let (reply, decision) = oneshot::channel();
            agent
                .send(AgentEvent::Approval {
                    child: false,
                    tool: "bash".into(),
                    command: "echo test".into(),
                    preview: None,
                    offers: Offers::default(),
                    reply,
                })
                .unwrap();
            let approval = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(pending) = session.state().pending {
                        return pending.id;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let action_id = uuid::Uuid::new_v4().to_string();
            let action = if approve { "approve" } else { "reject" };
            let response = client
                .post(format!("{route}/{action}"))
                .bearer_auth(&token)
                .header("x-bhai-request-id", &action_id)
                .json(&json!({"id":approval}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let answered = tokio::time::timeout(Duration::from_secs(2), decision)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(matches!(answered, Answer::Reject), !approve);
            let duplicate = client
                .post(format!("{route}/{action}"))
                .bearer_auth(&token)
                .header("x-bhai-request-id", &action_id)
                .json(&json!({"id":approval}))
                .send()
                .await
                .unwrap();
            assert_eq!(duplicate.status(), StatusCode::OK);
        }
        drop(remote);
        relay_task.abort();
        pump.abort();
    }

    #[tokio::test]
    async fn challenge_is_single_use_and_grants_are_role_scoped() {
        let hub = Arc::new(Hub::default());
        let nonce = "test-nonce".to_string();
        lock(&hub.nonces).insert(nonce.clone(), Instant::now() + NONCE_TTL);
        let identity = identity();
        let auth = identity.auth(nonce.clone(), "controller").unwrap();
        let response = authenticate(State(Arc::clone(&hub)), Json(auth)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let again = authenticate(
            State(Arc::clone(&hub)),
            Json(identity.auth(nonce, "controller").unwrap()),
        )
        .await;
        assert_eq!(again.status(), StatusCode::UNAUTHORIZED);
        let (token, _) = lock(&hub.grants)
            .iter()
            .next()
            .map(|(token, grant)| (token.clone(), grant.clone()))
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        assert!(authorized(&hub, &headers, "controller").is_some());
        assert!(authorized(&hub, &headers, "laptop").is_none());
    }
}
