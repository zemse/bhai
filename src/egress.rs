//! The proxy bash reaches the network through once the global config has an `[egress]`
//! table. A request goes out only when an `allow` rule matches its origin, method and
//! path prefix. HTTPS is opened with a CA made for this run, so an HTTPS request's
//! method and path are checked too, and the client must trust that CA: the child's
//! `SSL_CERT_FILE` and its kin point at it.
//!
//! `[egress.secrets.NAME]` gives the child `$NAME` holding a placeholder. The proxy swaps
//! it for the real value, read from bhai's own environment or a command, in the headers
//! (a `Basic` credential decoded) and the request target, and only on an allowed request
//! to the secret's origin, so the model never sees the value. Bodies pass through as they
//! are, placeholder and all.
//!
//! Only clients that honour `HTTPS_PROXY` come here. An `[egress]` table turns the
//! sandbox on, and the sandbox is what stops the rest: on macOS bash may open an IP
//! connection to this port on loopback alone. Landlock rules name a port but no address,
//! so on Linux a command may reach this port number on any host, and UDP, DNS included,
//! still goes out. With `[bash] network = false` the proxy does not start. Upgrades
//! (WebSocket) are refused, and redirects are handed back, not followed.
//!
//! Nothing here logs: a secret's value reaches only the upstream request, and an error
//! the child sees names the origin, never the URL a value was put into.

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt as _, Empty, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use reqwest::Url;
use rustls::ServerConfig;
use serde::Deserialize;
use tokio::net::{TcpListener, TcpStream};

/// On every reply the proxy makes itself, so a refusal is not mistaken for the server's.
const REFUSED: &str = "x-bhai-egress";
/// How long a secret's `command` may take, a Touch ID prompt included.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// Days a leaf certificate is valid from yesterday; clients refuse very long ones.
const LEAF_DAYS: u64 = 30;

/// `[egress]` as the config reads it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// `[METHOD ]scheme://host[:port][/path]`: no method or `*` for any, the path a
    /// prefix by whole segments.
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretSettings>,
}

/// `[egress.secrets.NAME]`: one of `env` and `command`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretSettings {
    /// The one HTTPS origin the value goes to.
    pub origin: String,
    /// A variable in bhai's own environment.
    pub env: Option<String>,
    /// A command whose stdout is the value, run on the first request that needs it.
    pub command: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq)]
struct Rule {
    /// Upper case; `None` for any.
    method: Option<String>,
    /// As `Url::origin` serialises it: lower case, default port left out.
    origin: String,
    /// No trailing `/`; empty for every path.
    path: String,
}

impl Rule {
    fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (method, url) = match text.split_once(char::is_whitespace) {
            Some((method, url)) => (Some(method), url.trim()),
            None => (None, text),
        };
        let method = match method {
            None | Some("*") => None,
            Some(m) if m.bytes().all(|b| b.is_ascii_alphabetic()) => Some(m.to_ascii_uppercase()),
            Some(m) => return Err(format!("`{text}`: `{m}` is not a method")),
        };
        let url = Url::parse(url).map_err(|e| format!("`{text}`: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!(
                "`{text}`: only http and https go through the proxy"
            ));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(format!("`{text}`: a rule names no credentials"));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(format!("`{text}`: a rule matches a path, not a query"));
        }
        Ok(Self {
            method,
            origin: url.origin().ascii_serialization(),
            path: url.path().trim_end_matches('/').to_string(),
        })
    }

    fn allows(&self, method: &str, origin: &str, path: &str) -> bool {
        self.origin == origin
            && self.method.as_deref().is_none_or(|m| m == method)
            && (self.path.is_empty()
                || path == self.path
                || path
                    .strip_prefix(&self.path)
                    .is_some_and(|rest| rest.starts_with('/')))
    }
}

/// A path a server could resolve somewhere other than where it reads: a `.` or `..`
/// segment, spelled out or percent-encoded.
fn dotted(path: &str) -> bool {
    path.split('/').any(|segment| {
        let segment = segment.replace("%2e", ".").replace("%2E", ".");
        segment == "." || segment == ".."
    })
}

#[derive(Debug)]
struct Secret {
    name: String,
    origin: String,
    placeholder: String,
    source: Source,
    value: tokio::sync::OnceCell<String>,
}

#[derive(Debug)]
enum Source {
    Env(String),
    Command(Vec<String>),
}

impl Secret {
    fn new(name: &str, settings: &SecretSettings) -> Result<Self, String> {
        let valid_name = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid_name {
            return Err(format!("secret `{name}`: not a variable name"));
        }
        let url = Url::parse(&settings.origin).map_err(|e| format!("secret `{name}`: {e}"))?;
        if url.scheme() != "https" || url.path() != "/" || url.query().is_some() {
            return Err(format!(
                "secret `{name}`: `origin` must be an https origin with no path, like https://api.github.com"
            ));
        }
        let source = match (&settings.env, &settings.command) {
            (Some(env), None) => Source::Env(env.clone()),
            (None, Some(command)) if !command.is_empty() => Source::Command(command.clone()),
            _ => return Err(format!("secret `{name}`: give one of `env` and `command`")),
        };
        Ok(Self {
            name: name.to_string(),
            origin: url.origin().ascii_serialization(),
            placeholder: format!("bhai-egress-placeholder-{}", uuid::Uuid::new_v4().simple()),
            source,
            value: tokio::sync::OnceCell::new(),
        })
    }

    /// Read once, then kept, and registered for redaction in case a server echoes it.
    async fn value(&self) -> Result<&str, String> {
        let value = self
            .value
            .get_or_try_init(|| async {
                let value = match &self.source {
                    Source::Env(env) => std::env::var(env)
                        .map_err(|_| format!("${env} is not set in bhai's environment"))?,
                    Source::Command(argv) => run(argv).await?,
                };
                if value.is_empty() {
                    return Err("it is empty".to_string());
                }
                crate::redact::register(&value);
                Ok(value)
            })
            .await?;
        Ok(value)
    }
}

async fn run(argv: &[String]) -> Result<String, String> {
    let output = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(COMMAND_TIMEOUT, output)
        .await
        .map_err(|_| format!("`{}` took too long", argv[0]))?
        .map_err(|e| format!("`{}`: {e}", argv[0]))?;
    if !output.status.success() {
        return Err(format!("`{}` {}", argv[0], output.status));
    }
    let value = String::from_utf8(output.stdout).map_err(|_| "not UTF-8".to_string())?;
    Ok(value.trim_end_matches(['\n', '\r']).to_string())
}

/// The CA leaf certificates are signed with, made for this run; its key never leaves
/// memory.
struct Ca {
    issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
    pem: String,
}

impl Ca {
    fn new() -> Result<Self> {
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "bhai egress proxy");
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        validity(&mut params, LEAF_DAYS * 12);
        let key = rcgen::KeyPair::generate()?;
        let pem = params.self_signed(&key)?.pem();
        Ok(Self {
            issuer: rcgen::Issuer::new(params, key),
            pem,
        })
    }

    /// A server config presenting a leaf for `host`, a name or an IP address.
    fn server_config(&self, host: &str) -> Result<ServerConfig> {
        let mut params = rcgen::CertificateParams::new(vec![host.to_string()])?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, host);
        params.use_authority_key_identifier_extension = true;
        params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        validity(&mut params, LEAF_DAYS);
        let key = rcgen::KeyPair::generate()?;
        let cert = params.signed_by(&key, &self.issuer)?;
        let key = rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der())
            .map_err(|e| anyhow!("{e}"))?;
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(config)
    }
}

/// Valid from yesterday, against clock skew, for `days`.
fn validity(params: &mut rcgen::CertificateParams, days: u64) {
    use chrono::Datelike as _;
    let ymd = |date: chrono::NaiveDate| {
        rcgen::date_time_ymd(date.year(), date.month() as u8, date.day() as u8)
    };
    let from = chrono::Utc::now().date_naive() - chrono::Days::new(1);
    params.not_before = ymd(from);
    params.not_after = ymd(from + chrono::Days::new(days));
}

/// Everything a connection needs, shared by all of them.
struct State {
    rules: Vec<Rule>,
    secrets: Vec<Secret>,
    /// The `Proxy-Authorization` value a client must send.
    auth: String,
    ca: Ca,
    /// Leaf configs by host, made on the first CONNECT to each.
    leaves: Mutex<HashMap<String, Arc<ServerConfig>>>,
    upstream: reqwest::Client,
}

type Body = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// A reply the proxy makes itself.
fn reply(status: StatusCode, text: String) -> Response<Body> {
    let body = match text.is_empty() {
        true => Empty::new().map_err(|never| match never {}).boxed(),
        false => Full::new(Bytes::from(text + "\n"))
            .map_err(|never| match never {})
            .boxed(),
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(REFUSED, HeaderValue::from_static("1"));
    response
}

/// Headers that belong to one connection, not to the request, so never passed on.
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let listed: Vec<String> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .collect();
    for name in listed {
        headers.remove(name.as_str());
    }
    for name in [
        "connection",
        "proxy-connection",
        "keep-alive",
        "proxy-authorization",
        "proxy-authenticate",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

impl State {
    fn denied(&self, method: &str, origin: &str, path: &str) -> Option<String> {
        if dotted(path) {
            return Some(format!("bhai egress: {path} has a `.` or `..` segment"));
        }
        let allowed = self.rules.iter().any(|r| r.allows(method, origin, path));
        (!allowed).then(|| {
            format!("bhai egress: {method} {origin}{path} is not on the [egress] allowlist")
        })
    }

    /// A request on the proxy's own connection: a CONNECT, or plain HTTP in absolute form.
    async fn outer(self: Arc<Self>, mut request: Request<Incoming>) -> Response<Body> {
        let auth = request.headers().get(header::PROXY_AUTHORIZATION);
        if auth.is_none_or(|auth| auth.as_bytes() != self.auth.as_bytes()) {
            let mut response = reply(StatusCode::PROXY_AUTHENTICATION_REQUIRED, String::new());
            response.headers_mut().insert(
                header::PROXY_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"bhai\""),
            );
            return response;
        }
        if request.method() == Method::CONNECT {
            let Some(authority) = request.uri().authority().map(|a| a.to_string()) else {
                return reply(
                    StatusCode::BAD_REQUEST,
                    "bhai egress: CONNECT names no host".into(),
                );
            };
            let Ok(url) = Url::parse(&format!("https://{authority}")) else {
                return reply(
                    StatusCode::BAD_REQUEST,
                    format!("bhai egress: bad host {authority}"),
                );
            };
            let origin = url.origin().ascii_serialization();
            if !self.rules.iter().any(|rule| rule.origin == origin) {
                return reply(
                    StatusCode::FORBIDDEN,
                    format!("bhai egress: {origin} is not on the [egress] allowlist"),
                );
            }
            let host = url.host_str().unwrap_or_default();
            let host = host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string();
            let config = match self.leaf(&host) {
                Ok(config) => config,
                Err(e) => return reply(StatusCode::BAD_GATEWAY, format!("bhai egress: {e}")),
            };
            let upgrade = hyper::upgrade::on(&mut request);
            tokio::spawn(async move {
                if let Ok(upgraded) = upgrade.await {
                    self.tunnel(upgraded, origin, config).await;
                }
            });
            return reply(StatusCode::OK, String::new());
        }
        let url = Url::parse(&request.uri().to_string()).ok();
        match url.filter(|url| url.scheme() == "http") {
            Some(url) => {
                let origin = url.origin().ascii_serialization();
                self.forward(request, &origin).await
            }
            None => reply(
                StatusCode::BAD_REQUEST,
                "bhai egress: send an absolute http:// URL, or CONNECT for https".into(),
            ),
        }
    }

    fn leaf(&self, host: &str) -> Result<Arc<ServerConfig>> {
        let mut leaves = self.leaves.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(config) = leaves.get(host) {
            return Ok(config.clone());
        }
        let config = Arc::new(self.ca.server_config(host)?);
        leaves.insert(host.to_string(), config.clone());
        Ok(config)
    }

    /// Serve the requests inside a CONNECT, over TLS as `origin`.
    async fn tunnel(
        self: Arc<Self>,
        upgraded: hyper::upgrade::Upgraded,
        origin: String,
        config: Arc<ServerConfig>,
    ) {
        let acceptor = tokio_rustls::TlsAcceptor::from(config);
        let Ok(tls) = acceptor.accept(TokioIo::new(upgraded)).await else {
            return;
        };
        let service = service_fn(move |request| {
            let (state, origin) = (self.clone(), origin.clone());
            async move { Ok::<_, Infallible>(state.forward(request, &origin).await) }
        });
        let _ = http1::Builder::new()
            .serve_connection(TokioIo::new(tls), service)
            .await;
    }

    /// Check one request against the rules, put in its secrets and send it on.
    async fn forward(&self, request: Request<Incoming>, origin: &str) -> Response<Body> {
        let (mut parts, body) = request.into_parts();
        let raw = parts.uri.path();
        // The URL sent upstream is parsed again, which turns `\` into `/` and resolves
        // dot segments, so the path checked is the parsed one and it must be the raw one.
        let path = match Url::parse(&format!("{origin}{raw}")) {
            Ok(url) if url.path() == raw => url.path().to_string(),
            Ok(url) => {
                return reply(
                    StatusCode::FORBIDDEN,
                    format!("bhai egress: {raw} resolves to {}", url.path()),
                );
            }
            Err(e) => return reply(StatusCode::BAD_REQUEST, format!("bhai egress: {raw}: {e}")),
        };
        if let Some(why) = self.denied(parts.method.as_str(), origin, &path) {
            return reply(StatusCode::FORBIDDEN, why);
        }
        if parts.headers.contains_key(header::UPGRADE) {
            return reply(
                StatusCode::FORBIDDEN,
                "bhai egress: upgrades (WebSocket) are refused".into(),
            );
        }
        strip_hop_by_hop(&mut parts.headers);
        parts.headers.remove(header::HOST);
        let mut target = parts
            .uri
            .path_and_query()
            .map_or("/", |pq| pq.as_str())
            .to_string();
        for secret in &self.secrets {
            if let Err(why) = self
                .insert(secret, origin, &mut target, &mut parts.headers)
                .await
            {
                return reply(StatusCode::FORBIDDEN, why);
            }
        }
        let upstream = self
            .upstream
            .request(parts.method, format!("{origin}{target}"))
            .headers(parts.headers)
            .body(reqwest::Body::wrap(body));
        match upstream.send().await {
            Ok(response) => {
                let mut response: Response<reqwest::Body> = response.into();
                strip_hop_by_hop(response.headers_mut());
                response.map(|body| body.map_err(Into::into).boxed())
            }
            // The URL may hold a secret's value by now.
            Err(e) => reply(
                StatusCode::BAD_GATEWAY,
                format!("bhai egress: {origin}: {}", e.without_url()),
            ),
        }
    }

    /// Swap `secret`'s placeholder for its value wherever the request carries it.
    async fn insert(
        &self,
        secret: &Secret,
        origin: &str,
        target: &mut String,
        headers: &mut HeaderMap,
    ) -> Result<(), String> {
        let placeholder = secret.placeholder.as_str();
        let carried = |value: &HeaderValue| {
            let text = value.to_str().unwrap_or_default();
            text.contains(placeholder) || basic(text).is_some_and(|c| c.contains(placeholder))
        };
        if !target.contains(placeholder) && !headers.values().any(carried) {
            return Ok(());
        }
        if secret.origin != origin {
            return Err(format!(
                "bhai egress: ${} goes only to {}, not {origin}",
                secret.name, secret.origin
            ));
        }
        let value = secret
            .value()
            .await
            .map_err(|e| format!("bhai egress: could not read ${}: {e}", secret.name))?;
        *target = target.replace(placeholder, value);
        for header_value in headers.values_mut() {
            let Ok(text) = header_value.to_str() else {
                continue;
            };
            let swapped = match basic(text) {
                Some(credential) if credential.contains(placeholder) => format!(
                    "Basic {}",
                    BASE64.encode(credential.replace(placeholder, value))
                ),
                _ if text.contains(placeholder) => text.replace(placeholder, value),
                _ => continue,
            };
            *header_value = HeaderValue::from_str(&swapped)
                .map_err(|_| format!("bhai egress: ${} does not fit in a header", secret.name))?;
        }
        Ok(())
    }
}

/// The decoded `user:password` of a `Basic` credential.
fn basic(value: &str) -> Option<String> {
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    String::from_utf8(BASE64.decode(encoded.trim()).ok()?).ok()
}

/// The running proxy, as a bash child sees it.
#[derive(Debug)]
pub struct Proxy {
    port: u16,
    /// Variables set on the child.
    env: Vec<(String, String)>,
    /// Where secrets are read from in bhai's environment, removed from the child's.
    sources: Vec<String>,
    describe: String,
}

static PROXY: OnceLock<Proxy> = OnceLock::new();

/// Start the proxy for the process; every bash call after this goes through it.
pub async fn start(settings: &Settings) -> Result<&'static Proxy> {
    let upstream = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .build()?;
    let proxy = spawn(settings, upstream).await?;
    Ok(PROXY.get_or_init(|| proxy))
}

pub fn active() -> Option<&'static Proxy> {
    PROXY.get()
}

/// Bind and serve on a free loopback port. `upstream` sends what is let through.
async fn spawn(settings: &Settings, upstream: reqwest::Client) -> Result<Proxy> {
    let rules = settings
        .allow
        .iter()
        .map(|rule| Rule::parse(rule))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("[egress] allow: {e}"))?;
    let secrets = settings
        .secrets
        .iter()
        .map(|(name, secret)| Secret::new(name, secret))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("[egress] {e}"))?;
    let ca = Ca::new().context("[egress] could not make the proxy's CA")?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let ca_file = write_ca(&ca.pem)?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    let proxy = Proxy {
        port,
        env: child_env(port, &token, &ca_file, &secrets),
        sources: secrets
            .iter()
            .filter_map(|s| match &s.source {
                Source::Env(env) => Some(env.clone()),
                Source::Command(_) => None,
            })
            .collect(),
        describe: describe(&settings.allow, &secrets),
    };
    let state = Arc::new(State {
        rules,
        secrets,
        auth: format!("Basic {}", BASE64.encode(format!("bhai:{token}"))),
        ca,
        leaves: Mutex::new(HashMap::new()),
        upstream,
    });
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(serve(stream, state.clone()));
        }
    });
    Ok(proxy)
}

/// A new file, so a name planted in a shared temp dir is never written through.
fn write_ca(pem: &str) -> Result<std::path::PathBuf> {
    use std::io::Write as _;
    let name = format!("bhai-egress-ca-{}.pem", uuid::Uuid::new_v4().simple());
    let path = std::env::temp_dir().join(name);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut file| file.write_all(pem.as_bytes()))
        .with_context(|| format!("[egress] could not write {}", path.display()))?;
    Ok(path)
}

async fn serve(stream: TcpStream, state: Arc<State>) {
    let service = service_fn(move |request| {
        let state = state.clone();
        async move { Ok::<_, Infallible>(state.outer(request).await) }
    });
    let _ = http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades()
        .await;
}

/// Variables that point a client at the proxy and its CA, and the placeholders.
fn child_env(
    port: u16,
    token: &str,
    ca_file: &std::path::Path,
    secrets: &[Secret],
) -> Vec<(String, String)> {
    let url = format!("http://bhai:{token}@127.0.0.1:{port}");
    let ca = ca_file.to_string_lossy().into_owned();
    let mut env = Vec::new();
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        env.push((name.to_string(), url.clone()));
    }
    // An inherited list would send its hosts around the proxy.
    env.push(("NO_PROXY".to_string(), String::new()));
    env.push(("no_proxy".to_string(), String::new()));
    for name in [
        "SSL_CERT_FILE",
        "CURL_CA_BUNDLE",
        "REQUESTS_CA_BUNDLE",
        "NODE_EXTRA_CA_CERTS",
        "GIT_SSL_CAINFO",
        "CARGO_HTTP_CAINFO",
        "PIP_CERT",
    ] {
        env.push((name.to_string(), ca.clone()));
    }
    env.push(("NODE_USE_ENV_PROXY".to_string(), "1".to_string()));
    for secret in secrets {
        env.push((secret.name.clone(), secret.placeholder.clone()));
    }
    env
}

/// What the model is told, so a 403 from the proxy reads as policy, not as the server.
fn describe(allow: &[String], secrets: &[Secret]) -> String {
    let rules = match allow.is_empty() {
        true => "nothing".to_string(),
        false => allow
            .iter()
            .map(|rule| format!("`{}`", rule.trim()))
            .collect::<Vec<_>>()
            .join(", "),
    };
    let mut text = format!(
        " HTTP and HTTPS go out only through bhai's egress proxy, which allows {rules}; \
    anything else gets a 403 from the proxy, and other connections fail."
    );
    for secret in secrets {
        text.push_str(&format!(
            " `${}` holds a placeholder the proxy swaps for the real value on requests to {}: \
    pass it as it is.",
            secret.name, secret.origin
        ));
    }
    text
}

impl Proxy {
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Point `command` at the proxy. Call it after `childenv::scrub`.
    pub fn apply(&self, command: &mut tokio::process::Command) {
        for name in &self.sources {
            command.env_remove(name);
        }
        command.envs(self.env.iter().map(|(k, v)| (k, v)));
    }

    pub fn describe(&self) -> &str {
        &self.describe
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(text: &str) -> Rule {
        Rule::parse(text).unwrap()
    }

    #[test]
    fn a_rule_matches_its_origin_method_and_whole_path_segments() {
        let repos = rule("get https://API.github.com/repos/zemse/");
        assert_eq!(repos.origin, "https://api.github.com");
        assert_eq!(repos.path, "/repos/zemse");
        let origin = "https://api.github.com";
        assert!(repos.allows("GET", origin, "/repos/zemse"));
        assert!(repos.allows("GET", origin, "/repos/zemse/bhai/issues"));
        assert!(!repos.allows("GET", origin, "/repos/zemsex"));
        assert!(!repos.allows("POST", origin, "/repos/zemse"));
        assert!(!repos.allows("GET", "https://api.github.com:8443", "/repos/zemse"));
        assert!(!repos.allows("GET", "http://api.github.com", "/repos/zemse"));
        let any = rule("https://crates.io:443");
        assert_eq!(any.origin, "https://crates.io");
        assert!(any.allows("DELETE", "https://crates.io", "/"));
        assert!(rule("* https://crates.io/api").allows("PUT", "https://crates.io", "/api/v1"));
    }

    #[test]
    fn a_malformed_rule_is_refused() {
        for text in [
            "ftp://example.com",
            "GET example.com",
            "https://user:pw@example.com",
            "https://example.com/a?b=c",
            "G-T https://example.com",
        ] {
            assert!(Rule::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn dot_segments_are_caught_encoded_or_not() {
        assert!(dotted("/repos/zemse/../other"));
        assert!(dotted("/repos/zemse/%2e%2E/other"));
        assert!(dotted("/./x"));
        assert!(!dotted("/repos/zemse/..x/.y"));
    }

    #[test]
    fn a_secret_needs_a_name_an_https_origin_and_one_source() {
        let settings =
            |origin: &str, env: Option<&str>, command: Option<Vec<String>>| SecretSettings {
                origin: origin.to_string(),
                env: env.map(str::to_string),
                command,
            };
        let ok = settings("https://api.github.com", Some("GITHUB_TOKEN"), None);
        let secret = Secret::new("GITHUB_TOKEN", &ok).unwrap();
        assert_eq!(secret.origin, "https://api.github.com");
        assert!(Secret::new("GH-TOKEN", &ok).is_err());
        assert!(Secret::new("1TOKEN", &ok).is_err());
        let plain = settings("http://api.github.com", Some("T"), None);
        assert!(Secret::new("T", &plain).is_err());
        let path = settings("https://api.github.com/repos", Some("T"), None);
        assert!(Secret::new("T", &path).is_err());
        let both = settings("https://a.b", Some("T"), Some(vec!["echo".into()]));
        assert!(Secret::new("T", &both).is_err());
        let neither = settings("https://a.b", None, None);
        assert!(Secret::new("T", &neither).is_err());
        let empty = settings("https://a.b", None, Some(Vec::new()));
        assert!(Secret::new("T", &empty).is_err());
    }

    #[test]
    fn basic_credentials_are_decoded() {
        let header = format!("Basic {}", BASE64.encode("x:abc"));
        assert_eq!(basic(&header).as_deref(), Some("x:abc"));
        assert_eq!(basic("Bearer abc"), None);
    }

    /// A server that answers each request with its method, target and headers.
    async fn echo<S>(stream: S)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let service = service_fn(|request: Request<Incoming>| async move {
            let mut text = format!("{} {}\n", request.method(), request.uri());
            for (name, value) in request.headers() {
                text.push_str(&format!("{name}: {}\n", value.to_str().unwrap_or("?")));
            }
            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(text))))
        });
        let _ = http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .await;
    }

    async fn plain_upstream() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(echo(stream));
            }
        });
        port
    }

    /// An HTTPS upstream on 127.0.0.1, and the CA its certificate is signed by.
    async fn tls_upstream() -> (u16, String) {
        let ca = Ca::new().unwrap();
        let config = Arc::new(ca.server_config("127.0.0.1").unwrap());
        let acceptor = tokio_rustls::TlsAcceptor::from(config);
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(tls) = acceptor.accept(stream).await {
                        echo(tls).await;
                    }
                });
            }
        });
        (port, ca.pem)
    }

    fn trusting(pem: &str) -> reqwest::Client {
        reqwest::Client::builder()
            .tls_certs_only([reqwest::Certificate::from_pem(pem.as_bytes()).unwrap()])
            .build()
            .unwrap()
    }

    fn var<'a>(proxy: &'a Proxy, name: &str) -> &'a str {
        &proxy.env.iter().find(|(k, _)| k == name).unwrap().1
    }

    /// A client that goes through `proxy` and trusts its CA alone.
    fn client(proxy: &Proxy) -> reqwest::Client {
        let ca = std::fs::read(var(proxy, "SSL_CERT_FILE")).unwrap();
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(var(proxy, "HTTPS_PROXY")).unwrap())
            .tls_certs_only([reqwest::Certificate::from_pem(&ca).unwrap()])
            .build()
            .unwrap()
    }

    /// Each secret's value is `real-<name>-value`, from a command.
    fn settings(allow: &[String], secrets: &[(&str, &str)]) -> Settings {
        Settings {
            allow: allow.to_vec(),
            secrets: secrets
                .iter()
                .map(|(name, origin)| {
                    let command = vec!["printf".to_string(), format!("real-{name}-value")];
                    let secret = SecretSettings {
                        origin: origin.to_string(),
                        env: None,
                        command: Some(command),
                    };
                    (name.to_string(), secret)
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn plain_http_goes_through_only_when_a_rule_allows_it() {
        let port = plain_upstream().await;
        let origin = format!("http://127.0.0.1:{port}");
        let allow = [format!("GET {origin}/ok")];
        let proxy = spawn(&settings(&allow, &[]), reqwest::Client::new())
            .await
            .unwrap();
        let client = client(&proxy);

        let response = client
            .get(format!("{origin}/ok/a?q=1"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        assert!(text.starts_with("GET /ok/a?q=1\n"), "{text}");
        assert!(!text.contains("proxy-authorization"), "{text}");

        for (method, path) in [
            ("GET", "/okx"),
            ("POST", "/ok"),
            ("GET", "/ok/%2e%2e/admin"),
        ] {
            let response = client
                .request(method.parse().unwrap(), format!("{origin}{path}"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 403, "{method} {path}");
            assert!(response.headers().contains_key(REFUSED));
        }

        let anonymous = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(format!("http://127.0.0.1:{}", proxy.port)).unwrap())
            .build()
            .unwrap();
        let response = anonymous.get(format!("{origin}/ok")).send().await.unwrap();
        assert_eq!(response.status(), 407);
    }

    /// Send `target` to the proxy byte for byte, since reqwest and curl normalise it.
    async fn raw_get(proxy: &Proxy, target: &str) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let url = Url::parse(var(proxy, "HTTPS_PROXY")).unwrap();
        let auth = BASE64.encode(format!("{}:{}", url.username(), url.password().unwrap()));
        let mut stream = TcpStream::connect(("127.0.0.1", proxy.port)).await.unwrap();
        let request = format!(
            "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             Proxy-Authorization: Basic {auth}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn a_path_that_parses_somewhere_else_is_refused_as_sent() {
        let port = plain_upstream().await;
        let origin = format!("http://127.0.0.1:{port}");
        let allow = [format!("GET {origin}/ok")];
        let proxy = spawn(&settings(&allow, &[]), reqwest::Client::new())
            .await
            .unwrap();

        let response = raw_get(&proxy, &format!("{origin}/ok/a")).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        for path in [r"/ok/x\..\..\admin", r"/ok\..\admin", "/ok/%2e%2e/admin"] {
            let response = raw_get(&proxy, &format!("{origin}{path}")).await;
            assert!(response.starts_with("HTTP/1.1 403"), "{path}: {response}");
            assert!(!response.contains("GET /admin"), "{path}: {response}");
        }
    }

    #[tokio::test]
    async fn https_is_checked_by_path_and_gets_its_secrets_on_its_own_origin_alone() {
        let (port, upstream_ca) = tls_upstream().await;
        let origin = format!("https://127.0.0.1:{port}");
        let other = format!("http://127.0.0.1:{}", plain_upstream().await);
        let allow = [format!("GET {origin}/repos"), format!("{other}/")];
        let secrets = [("API_TOKEN", origin.as_str())];
        let proxy = spawn(&settings(&allow, &secrets), trusting(&upstream_ca))
            .await
            .unwrap();
        let client = client(&proxy);
        let placeholder = var(&proxy, "API_TOKEN").to_string();
        assert!(placeholder.starts_with("bhai-egress-placeholder-"));

        let response = client
            .get(format!("{origin}/repos/x?token={placeholder}"))
            .bearer_auth(&placeholder)
            .header("x-plain", "kept")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        assert!(
            text.starts_with("GET /repos/x?token=real-API_TOKEN-value\n"),
            "{text}"
        );
        assert!(
            text.contains("authorization: Bearer real-API_TOKEN-value"),
            "{text}"
        );
        assert!(text.contains("x-plain: kept"), "{text}");
        assert!(!text.contains(&placeholder), "{text}");

        let response = client
            .get(format!("{origin}/repos/x"))
            .basic_auth("me", Some(&placeholder))
            .send()
            .await
            .unwrap();
        let text = response.text().await.unwrap();
        let swapped = BASE64.encode("me:real-API_TOKEN-value");
        assert!(
            text.contains(&format!("authorization: Basic {swapped}")),
            "{text}"
        );

        let response = client
            .post(format!("{origin}/repos/x"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
        let text = response.text().await.unwrap();
        assert!(text.contains("not on the [egress] allowlist"), "{text}");

        // Allowed there, but the placeholder is not that origin's.
        let response = client
            .get(format!("{other}/leak"))
            .bearer_auth(&placeholder)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
        assert!(response.text().await.unwrap().contains("goes only to"));

        // An origin no rule names is refused at CONNECT.
        let (unlisted, _) = tls_upstream().await;
        let refused = client
            .get(format!("https://127.0.0.1:{unlisted}/repos"))
            .send()
            .await;
        assert!(refused.is_err(), "{refused:?}");
    }

    #[tokio::test]
    async fn a_failed_upstream_names_the_origin_not_the_url_a_secret_went_into() {
        let closed = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let origin = format!("https://127.0.0.1:{}", closed.local_addr().unwrap().port());
        drop(closed);
        let allow = [format!("GET {origin}/")];
        let secrets = [("API_TOKEN", origin.as_str())];
        let proxy = spawn(&settings(&allow, &secrets), reqwest::Client::new())
            .await
            .unwrap();
        let placeholder = var(&proxy, "API_TOKEN").to_string();
        let response = client(&proxy)
            .get(format!("{origin}/x?token={placeholder}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 502);
        let text = response.text().await.unwrap();
        assert!(text.contains(&origin), "{text}");
        assert!(!text.contains("real-API_TOKEN-value"), "{text}");
        assert!(!text.contains("/x?token="), "{text}");
    }

    #[tokio::test]
    async fn the_child_env_points_at_the_proxy_and_holds_placeholders_only() {
        let mut settings = settings(&[], &[("GH_TOKEN", "https://api.github.com")]);
        settings.secrets.insert(
            "FROM_ENV".to_string(),
            SecretSettings {
                origin: "https://api.github.com".to_string(),
                env: Some("BHAI_TEST_EGRESS_SOURCE".to_string()),
                command: None,
            },
        );
        let proxy = spawn(&settings, reqwest::Client::new()).await.unwrap();
        let url = var(&proxy, "HTTPS_PROXY");
        assert!(url.starts_with("http://bhai:"), "{url}");
        assert!(
            url.ends_with(&format!("@127.0.0.1:{}", proxy.port)),
            "{url}"
        );
        assert_eq!(var(&proxy, "NO_PROXY"), "");
        assert!(var(&proxy, "GH_TOKEN").starts_with("bhai-egress-placeholder-"));
        assert_eq!(proxy.sources, ["BHAI_TEST_EGRESS_SOURCE"]);
        assert!(proxy.describe().contains("`$GH_TOKEN` holds a placeholder"));
        let ca = std::fs::read_to_string(var(&proxy, "SSL_CERT_FILE")).unwrap();
        assert!(ca.starts_with("-----BEGIN CERTIFICATE-----"), "{ca}");
    }

    /// curl is the client most commands use, and it reads the proxy and CA from the env.
    #[tokio::test]
    async fn curl_goes_through_on_the_env_alone() {
        let (port, upstream_ca) = tls_upstream().await;
        let origin = format!("https://127.0.0.1:{port}");
        let allow = [format!("GET {origin}/")];
        let secrets = [("API_TOKEN", origin.as_str())];
        let proxy = spawn(&settings(&allow, &secrets), trusting(&upstream_ca))
            .await
            .unwrap();
        let mut command = tokio::process::Command::new("bash");
        command
            .arg("-c")
            .arg(format!(
                "command -v curl >/dev/null || exit 0; \
                 curl -sS -H \"Authorization: token $API_TOKEN\" {origin}/x; \
                 curl -sS -o /dev/null -w '%{{http_code}}' -X POST {origin}/x"
            ))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default());
        proxy.apply(&mut command);
        let out = command.output().await.unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout.is_empty() && out.status.success() {
            return;
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stdout.starts_with("GET /x\n"), "{stdout}{stderr}");
        assert!(
            stdout.contains("authorization: token real-API_TOKEN-value"),
            "{stdout}"
        );
        assert!(stdout.ends_with("403"), "{stdout}");
    }
}
