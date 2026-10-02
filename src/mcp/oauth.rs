//! OAuth for hosted MCP servers. `bhai mcp login <server>` runs rmcp's authorization code
//! flow with PKCE: discovery, dynamic client registration, and the browser's redirect back
//! to a one-shot listener on a loopback port. The login is kept by server url in
//! `~/.config/bhai/mcp-credentials.json`, readable only by the user, and a server with one
//! sends its token, refreshed shortly before it lapses.
//!
//! The authorization server's metadata is kept with the login and used as it was rather
//! than discovered again, so a refresh token only ever goes back to the token endpoint
//! that issued it, whatever the server later says its authorization server is.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rmcp::service::ClientInitializeError;
use rmcp::transport::auth::{
    AuthClient, AuthError, AuthorizationCallback, AuthorizationManager, AuthorizationMetadata,
    CredentialStore, OAuthClientConfig, StoredCredentials,
};
use rmcp::transport::streamable_http_client::StreamableHttpError;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// Where logins are kept, under the home directory.
pub const STORE: &str = ".config/bhai/mcp-credentials.json";
/// How long the browser has to come back.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// How long one connection to the callback listener has to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The most of a request the callback listener reads. The redirect carries a code and a
/// state in its query; anything near this is not one.
const MAX_REQUEST: usize = 16 * 1024;
/// The name bhai registers as.
const CLIENT_NAME: &str = "bhai";

/// Held across each read-modify-write of the store, since two servers can refresh at once.
static WRITING: Mutex<()> = Mutex::new(());

/// One server's login. Not `Debug`: it holds the tokens.
#[derive(Clone, Serialize, Deserialize)]
struct Login {
    metadata: AuthorizationMetadata,
    client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
    redirect_uri: String,
    #[serde(default)]
    credentials: Option<StoredCredentials>,
}

/// Every login, by server url.
type Logins = BTreeMap<String, Login>;

/// The store, or none when there is none or it will not read: a server then starts with
/// no login and says it needs one.
fn read(path: &Path) -> Logins {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write(path: &Path, logins: &Logins) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut text = serde_json::to_string_pretty(logins)?;
    text.push('\n');
    crate::auth::write_atomically(path, &text)
        .with_context(|| format!("could not keep the MCP login in {}", path.display()))
}

/// rmcp's view of one server's entry in the store. During a login, `fresh` is what the
/// tokens are saved with, replacing whatever the server had; otherwise a refresh only
/// replaces the tokens.
struct FileStore {
    path: PathBuf,
    url: String,
    fresh: Option<Login>,
}

impl FileStore {
    fn update(&self, change: impl FnOnce(&mut Logins)) -> Result<(), AuthError> {
        let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
        let mut logins = read(&self.path);
        change(&mut logins);
        write(&self.path, &logins).map_err(|e| AuthError::InternalError(format!("{e:#}")))
    }
}

/// Keep the tokens out of everything bhai prints or sends the model.
fn redact(credentials: &StoredCredentials) {
    let Ok(token) = serde_json::to_value(&credentials.token_response) else {
        return;
    };
    for key in ["access_token", "refresh_token"] {
        if let Some(secret) = token.get(key).and_then(|v| v.as_str()) {
            crate::redact::register(secret);
        }
    }
}

#[async_trait::async_trait]
impl CredentialStore for FileStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let credentials = read(&self.path)
            .remove(&self.url)
            .and_then(|login| login.credentials);
        if let Some(credentials) = &credentials {
            redact(credentials);
        }
        Ok(credentials)
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        redact(&credentials);
        self.update(|logins| match &self.fresh {
            Some(fresh) => {
                let login = Login {
                    credentials: Some(credentials),
                    ..fresh.clone()
                };
                logins.insert(self.url.clone(), login);
            }
            // A login removed since this connection started stays removed.
            None => {
                if let Some(login) = logins.get_mut(&self.url) {
                    login.credentials = Some(credentials);
                }
            }
        })
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.update(|logins| {
            logins.remove(&self.url);
        })
    }
}

/// An HTTP client that sends the token of the login kept for `url`, or `None` when there
/// is none.
pub async fn authorized(
    store: &Path,
    url: &str,
    http: reqwest::Client,
) -> Result<Option<AuthClient<reqwest::Client>>> {
    let Some(login) = read(store).remove(url) else {
        return Ok(None);
    };
    if login.credentials.is_none() {
        return Ok(None);
    }
    let mut manager = AuthorizationManager::new(url).await?;
    manager.set_metadata(login.metadata);
    let mut config = OAuthClientConfig::new(login.client_id, login.redirect_uri);
    if let Some(secret) = login.client_secret {
        config = config.with_client_secret(secret);
    }
    manager.configure_client(config)?;
    manager.set_credential_store(FileStore {
        path: store.to_path_buf(),
        url: url.to_string(),
        fresh: None,
    });
    Ok(Some(AuthClient::new(http, manager)))
}

/// Whether a server refused to start for want of a login, or because the one it had has
/// lapsed and would not refresh.
pub fn needs_login(error: &ClientInitializeError) -> bool {
    let mut next: Option<&(dyn std::error::Error + 'static)> = match error {
        // The transport's error is a field, not the source.
        ClientInitializeError::TransportError { error, .. } => Some(error.error.as_ref()),
        other => Some(other),
    };
    while let Some(error) = next {
        if let Some(http) = error.downcast_ref::<StreamableHttpError<reqwest::Error>>() {
            return matches!(
                http,
                StreamableHttpError::AuthRequired(_)
                    | StreamableHttpError::Auth(AuthError::AuthorizationRequired)
            );
        }
        next = error.source();
    }
    false
}

/// Sign in to the HTTP server at `url` and keep the login in `store`. `open` is handed the
/// page the user signs in on.
pub async fn login(store: &Path, url: &str, open: impl FnOnce(&str)) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("could not listen on a loopback port for the sign-in")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let mut manager = AuthorizationManager::new(url).await?;
    let metadata = manager
        .discover_metadata()
        .await
        .context("could not find the server's authorization server")?;
    manager.set_metadata(metadata.clone());
    let scopes = manager.select_scopes(None, &[]);
    let scopes: Vec<&str> = scopes.iter().map(String::as_str).collect();
    let client = manager
        .register_client(CLIENT_NAME, &redirect_uri, &scopes)
        .await
        .context("could not register bhai with the authorization server")?;
    manager.set_credential_store(FileStore {
        path: store.to_path_buf(),
        url: url.to_string(),
        fresh: Some(Login {
            metadata,
            client_id: client.client_id,
            client_secret: client.client_secret,
            redirect_uri,
            credentials: None,
        }),
    });
    let page = manager.get_authorization_url(&scopes).await?;
    open(&page);
    let callback = tokio::time::timeout(LOGIN_TIMEOUT, callback(&listener, port))
        .await
        .map_err(|_| {
            anyhow!(
                "no sign-in came back within {}s",
                LOGIN_TIMEOUT.as_secs_f64()
            )
        })??;
    manager
        .exchange_code_for_token_with_issuer(
            &callback.code,
            &callback.csrf_token,
            callback.issuer.as_deref(),
        )
        .await
        .context("could not exchange the sign-in for a token")?;
    Ok(())
}

/// Forget the login kept for `url`. Returns whether there was one.
pub fn logout(store: &Path, url: &str) -> Result<bool> {
    let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let mut logins = read(store);
    if logins.remove(url).is_none() {
        return Ok(false);
    }
    write(store, &logins)?;
    Ok(true)
}

/// Wait for the browser's redirect to `/callback`. Anything else that connects is turned
/// away and the wait goes on.
async fn callback(listener: &TcpListener, port: u16) -> Result<AuthorizationCallback> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let head = tokio::time::timeout(REQUEST_TIMEOUT, read_head(&mut stream))
            .await
            .ok()
            .flatten();
        let Some(target) = head.as_deref().and_then(target) else {
            respond(
                &mut stream,
                "400 Bad Request",
                "bhai could not read that request.",
            )
            .await;
            continue;
        };
        let Ok(url) = reqwest::Url::parse(&format!("http://127.0.0.1:{port}{target}")) else {
            respond(
                &mut stream,
                "400 Bad Request",
                "bhai could not read that request.",
            )
            .await;
            continue;
        };
        if url.path() != "/callback" {
            respond(&mut stream, "404 Not Found", "Not found.").await;
            continue;
        }
        if let Some((_, error)) = url.query_pairs().find(|(key, _)| key == "error") {
            respond(&mut stream, "200 OK", "Sign-in failed. Back to bhai.").await;
            bail!("the authorization server refused: {}", printable(&error));
        }
        let callback = AuthorizationCallback::from_redirect_url(url.as_str());
        let body = match callback {
            Ok(_) => "Signed in. You can close this tab and go back to bhai.",
            Err(_) => "Sign-in failed. Back to bhai.",
        };
        respond(&mut stream, "200 OK", body).await;
        return Ok(callback?);
    }
}

/// A request's head, up to the blank line, or `None` past `MAX_REQUEST` or on a
/// connection that closes first.
async fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 || head.len() + read > MAX_REQUEST {
            return None;
        }
        head.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8(head).ok()
}

/// The target of a `GET` request line.
fn target(head: &str) -> Option<&str> {
    let mut parts = head.lines().next()?.split(' ');
    match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) if target.starts_with('/') => Some(target),
        _ => None,
    }
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let page = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\
Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(page.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// What a server said, safe to print: one line, no control characters, and short.
fn printable(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(200).collect()
}

/// Open `page` in the user's browser, if it is a web page.
pub fn open_browser(page: &str) {
    let web = reqwest::Url::parse(page).is_ok_and(|url| matches!(url.scheme(), "http" | "https"));
    if !web {
        return;
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(page)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_store() -> PathBuf {
        std::env::temp_dir()
            .join(format!("bhai-mcp-oauth-{}", uuid::Uuid::new_v4()))
            .join("mcp-credentials.json")
    }

    fn credentials(access: &str) -> StoredCredentials {
        let token = json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": format!("{access}-refresh"),
        });
        StoredCredentials::new(
            "client".to_string(),
            Some(serde_json::from_value(token).unwrap()),
            Vec::new(),
            Some(0),
        )
    }

    fn login() -> Login {
        Login {
            metadata: AuthorizationMetadata::default(),
            client_id: "client".to_string(),
            client_secret: None,
            redirect_uri: "http://127.0.0.1:1/callback".to_string(),
            credentials: None,
        }
    }

    fn logged_in(store: &Path, url: &str) -> bool {
        read(store)
            .get(url)
            .is_some_and(|login| login.credentials.is_some())
    }

    fn access(stored: &StoredCredentials) -> String {
        let token = serde_json::to_value(&stored.token_response).unwrap();
        token["access_token"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn a_login_is_kept_by_url_and_only_the_user_can_read_it() {
        let path = temp_store();
        let url = "https://mcp.example/mcp";
        let fresh = FileStore {
            path: path.clone(),
            url: url.to_string(),
            fresh: Some(login()),
        };
        assert!(fresh.load().await.unwrap().is_none());
        fresh.save(credentials("first-token-1234")).await.unwrap();
        assert!(logged_in(&path, url));
        assert!(!logged_in(&path, "https://other.example/mcp"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A token the store hands out is never printed back.
        assert_eq!(crate::redact::apply("first-token-1234"), "[REDACTED]");

        // A refresh replaces the tokens and keeps the rest.
        let refreshing = FileStore {
            path: path.clone(),
            url: url.to_string(),
            fresh: None,
        };
        refreshing.save(credentials("second-token")).await.unwrap();
        let kept = refreshing.load().await.unwrap().unwrap();
        assert_eq!(access(&kept), "second-token");
        assert_eq!(read(&path)[url].redirect_uri, "http://127.0.0.1:1/callback");

        // Nor does a refresh that lands after a logout bring the login back.
        assert!(logout(&path, url).unwrap());
        refreshing.save(credentials("third-token")).await.unwrap();
        assert!(!logged_in(&path, url));
        assert!(!logout(&path, url).unwrap());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn no_login_or_one_without_tokens_is_no_client() {
        let path = temp_store();
        let url = "https://mcp.example/mcp";
        let none = authorized(&path, url, reqwest::Client::new())
            .await
            .unwrap();
        assert!(none.is_none());
        let mut logins = Logins::new();
        logins.insert(url.to_string(), login());
        write(&path, &logins).unwrap();
        let none = authorized(&path, url, reqwest::Client::new())
            .await
            .unwrap();
        assert!(none.is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn only_a_get_request_line_has_a_target() {
        assert_eq!(
            target("GET /callback?code=c&state=s HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some("/callback?code=c&state=s")
        );
        assert_eq!(target("POST /callback HTTP/1.1\r\n\r\n"), None);
        assert_eq!(target("GET http://evil/ HTTP/1.1\r\n\r\n"), None);
        assert_eq!(target(""), None);
        assert_eq!(printable("bad\n\x1b[31mred"), "bad[31mred");
    }

    #[tokio::test]
    async fn the_callback_listener_waits_out_strays_and_oversized_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let send = |request: Vec<u8>| async move {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let _ = stream.write_all(&request).await;
            let mut answer = String::new();
            let _ = stream.read_to_string(&mut answer).await;
            answer
        };
        let waiting = tokio::spawn(async move { callback(&listener, port).await });
        let huge = format!(
            "GET /callback?x={} HTTP/1.1\r\n\r\n",
            "a".repeat(MAX_REQUEST)
        );
        // Cut off mid-request, the client may see a reset rather than the 400.
        assert!(!send(huge.into_bytes()).await.contains("Signed in"));
        let stray = b"GET /favicon.ico HTTP/1.1\r\n\r\n".to_vec();
        assert!(send(stray).await.starts_with("HTTP/1.1 404"));
        let good = b"GET /callback?code=c0de&state=st4te HTTP/1.1\r\n\r\n".to_vec();
        assert!(send(good).await.contains("Signed in"));
        let callback = waiting.await.unwrap().unwrap();
        assert_eq!(
            (callback.code.as_str(), callback.csrf_token.as_str()),
            ("c0de", "st4te")
        );
    }
}
