//! Rendering a page for `fetch` in a headless Chrome or Chromium found on the machine,
//! spoken to over the DevTools Protocol. bhai never installs a browser. Each render gets a
//! fresh profile in a private temp directory, deleted afterwards, so the user's cookies
//! and logins are never in it.
//!
//! Every request the page makes is paused (`Fetch.requestPaused`) and bhai makes it
//! itself through the `ssrf` guard, one hop at a time, handing the answer back with
//! `Fetch.fulfillRequest`; a redirect goes back to Chrome as a redirect, so its next hop
//! is paused and checked again. What `Fetch` does not see (websockets, service workers)
//! goes to a proxy that is not there and fails.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::{Method, Url};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::cdp::{Cdp, Event};
use super::{Image, ssrf};

/// How long the whole render may take, the browser's start included.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(45);
/// How long Chrome has to write the port it listens on.
const START: Duration = Duration::from_secs(10);
/// How long the load event is waited for before the page is read as it stands.
const LOAD: Duration = Duration::from_secs(20);
/// After the load, the page is read once no request has been open for `IDLE`, or after
/// `SETTLE` at the most.
const IDLE: Duration = Duration::from_millis(500);
const SETTLE: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(50);
/// Each request bhai makes for the page.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Past these, a page's further requests fail.
const MAX_REQUESTS: usize = 200;
const MAX_BYTES: usize = 20 << 20;
const MAX_RESPONSE: usize = 8 << 20;
/// `innerText` is cut here in the page, before it crosses the socket.
const MAX_TEXT: usize = 2 << 20;
const VIEWPORT: (u32, u32) = (1280, 800);
/// A proxy no one listens on, for whatever traffic `Fetch` does not pause.
const NO_PROXY: &str = "http://127.0.0.1:9";

/// Where an installed Chrome, Chromium, Edge or Brave usually is, by name on `PATH`
/// elsewhere.
const MAC_APPS: [&str; 6] = [
    "Google Chrome.app/Contents/MacOS/Google Chrome",
    "Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
    "Chromium.app/Contents/MacOS/Chromium",
    "Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
    "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "Brave Browser.app/Contents/MacOS/Brave Browser",
];
const COMMANDS: [&str; 8] = [
    "chrome-headless-shell",
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
    "chrome",
    "microsoft-edge",
    "brave-browser",
];

/// A page as Chrome rendered it.
#[derive(Debug, Default)]
pub(crate) struct Rendered {
    /// Where the page ended up, redirects and script navigation included.
    pub url: String,
    /// The main document's status, when bhai saw it answered.
    pub status: Option<u16>,
    pub title: String,
    pub text: String,
    pub image: Option<Image>,
    /// Why requests were refused, the first few.
    pub refused: Vec<String>,
    /// How many were refused in all.
    pub refusals: usize,
}

/// The browser to render with: `BHAI_CHROME` when set, else the first in the usual
/// places.
pub(crate) fn find() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("BHAI_CHROME") {
        let path = PathBuf::from(path);
        return match path.is_file() {
            true => Ok(path),
            false => Err(format!(
                "BHAI_CHROME names {}, which is not a file",
                path.display()
            )),
        };
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let path = std::env::var_os("PATH");
    candidates(home.as_deref(), path.as_deref())
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| {
            "no Chrome or Chromium was found to render the page (bhai does not install one; \
set BHAI_CHROME to a browser's executable to use it)"
                .to_string()
        })
}

/// Every place a browser is looked for, in order.
fn candidates(home: Option<&Path>, path: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if cfg!(target_os = "macos") {
        let mut roots = vec![PathBuf::from("/Applications")];
        roots.extend(home.map(|h| h.join("Applications")));
        for root in roots {
            out.extend(MAC_APPS.iter().map(|app| root.join(app)));
        }
    }
    if let Some(path) = path {
        for dir in std::env::split_paths(path) {
            out.extend(COMMANDS.iter().map(|name| dir.join(name)));
        }
    }
    out
}

/// Every running browser's process group, by its profile, so a signal or panic that ends
/// bhai can end them too: the group is their own, so nothing else would.
static BROWSERS: Mutex<BTreeMap<PathBuf, u32>> = Mutex::new(BTreeMap::new());

fn browsers() -> std::sync::MutexGuard<'static, BTreeMap<PathBuf, u32>> {
    BROWSERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Kill every browser a render started and delete its profile.
pub fn kill_all() {
    for (profile, group) in std::mem::take(&mut *browsers()) {
        super::bash::kill_group(Some(group));
        let _ = std::fs::remove_dir_all(profile);
    }
}

/// A headless browser bhai started; dropping it kills its process group and deletes its
/// profile.
struct Browser {
    child: tokio::process::Child,
    group: Option<u32>,
    profile: PathBuf,
}

impl Browser {
    async fn launch(exe: &Path) -> Result<(Browser, String), String> {
        let profile = std::env::temp_dir().join(format!("bhai-chrome-{}", uuid::Uuid::new_v4()));
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&profile)
                .map_err(|e| format!("could not make a profile at {}: {e}", profile.display()))?;
        }
        let mut command = tokio::process::Command::new(exe);
        command
            .args(args(&profile))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&profile);
                return Err(format!("could not start {}: {e}", exe.display()));
            }
        };
        let group = child.id();
        if let Some(group) = group {
            browsers().insert(profile.clone(), group);
        }
        let mut browser = Browser {
            child,
            group,
            profile,
        };
        let endpoint = browser.endpoint(exe).await?;
        Ok((browser, endpoint))
    }

    /// The browser's DevTools websocket, from the `DevToolsActivePort` file Chrome writes
    /// into the profile once it listens.
    async fn endpoint(&mut self, exe: &Path) -> Result<String, String> {
        let file = self.profile.join("DevToolsActivePort");
        let begun = Instant::now();
        loop {
            if let Ok(text) = std::fs::read_to_string(&file) {
                let mut lines = text.lines();
                if let (Some(port), Some(path)) = (lines.next(), lines.next())
                    && let Ok(port) = port.trim().parse::<u16>()
                {
                    return Ok(format!("ws://127.0.0.1:{port}{}", path.trim()));
                }
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(format!("{} exited as it started: {status}", exe.display()));
            }
            if begun.elapsed() > START {
                return Err(format!(
                    "{} did not open DevTools within {}s",
                    exe.display(),
                    START.as_secs()
                ));
            }
            tokio::time::sleep(TICK).await;
        }
    }

    /// Kill it, wait for it and delete its profile.
    async fn close(mut self) {
        super::bash::kill_group(self.group);
        let _ = self.child.wait().await;
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        super::bash::kill_group(self.group);
        browsers().remove(&self.profile);
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

/// How the browser is started: headless on its own profile, extensions, sync, background
/// requests and the keychain off, and a dead proxy for whatever `Fetch` does not pause.
fn args(profile: &Path) -> Vec<String> {
    let mut args: Vec<String> = [
        "--headless=new",
        "--remote-debugging-port=0",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-extensions",
        "--disable-sync",
        "--disable-background-networking",
        "--disable-component-update",
        "--disable-default-apps",
        "--disable-domain-reliability",
        "--disable-client-side-phishing-detection",
        "--dns-prefetch-disable",
        "--password-store=basic",
        "--use-mock-keychain",
        "--mute-audio",
        "--hide-scrollbars",
        // Loopback is otherwise let past the proxy.
        "--proxy-bypass-list=<-loopback>",
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
    ]
    .map(str::to_string)
    .to_vec();
    args.push(format!("--proxy-server={NO_PROXY}"));
    args.push(format!("--window-size={},{}", VIEWPORT.0, VIEWPORT.1));
    args.push(format!("--user-data-dir={}", profile.display()));
    args.push("about:blank".to_string());
    args
}

/// Render `url` in the browser at `exe`: launch it, load the page with every request
/// through the guard, read it, and kill the browser.
pub(crate) async fn render(
    exe: &Path,
    url: &Url,
    screenshot: bool,
    refusal: fn(IpAddr) -> Option<&'static str>,
) -> Result<Rendered, String> {
    let (browser, endpoint) = Browser::launch(exe).await?;
    let out = async {
        let port_path = endpoint.trim_start_matches("ws://127.0.0.1:");
        let port = port_path
            .split('/')
            .next()
            .and_then(|p| p.parse::<u16>().ok())
            .ok_or_else(|| format!("a DevTools endpoint bhai cannot read: {endpoint}"))?;
        let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(|e| format!("could not reach the browser's DevTools: {e}"))?;
        let (ws, _) = tokio_tungstenite::client_async(endpoint.as_str(), stream)
            .await
            .map_err(|e| format!("could not open the browser's DevTools: {e}"))?;
        let (cdp, events) = Cdp::start(ws);
        load(&cdp, events, url, screenshot, refusal).await
    }
    .await;
    browser.close().await;
    out
}

/// What the requests of a page have come to so far.
struct Watch {
    loaded: bool,
    open: usize,
    changed: Instant,
    requests: usize,
    bytes: usize,
    /// The main frame's last answered document: its status and URL.
    document: Option<(u16, String)>,
    refused: Vec<String>,
    refusals: usize,
}

impl Watch {
    fn new() -> Self {
        Watch {
            loaded: false,
            open: 0,
            changed: Instant::now(),
            requests: 0,
            bytes: 0,
            document: None,
            refused: Vec::new(),
            refusals: 0,
        }
    }
}

/// Load `url` in a new page of the browser on `cdp` and read it.
pub(crate) async fn load(
    cdp: &Arc<Cdp>,
    events: mpsc::UnboundedReceiver<Event>,
    url: &Url,
    screenshot: bool,
    refusal: fn(IpAddr) -> Option<&'static str>,
) -> Result<Rendered, String> {
    cdp.call(
        None,
        "Browser.setDownloadBehavior",
        json!({"behavior": "deny"}),
    )
    .await?;
    let target = cdp
        .call(None, "Target.createTarget", json!({"url": "about:blank"}))
        .await?;
    let target = string(&target, "targetId")?;
    let attached = cdp
        .call(
            None,
            "Target.attachToTarget",
            json!({"targetId": target, "flatten": true}),
        )
        .await?;
    let session = string(&attached, "sessionId")?;
    let watch = Arc::new(Mutex::new(Watch::new()));
    let intercepting = tokio::spawn(intercept(
        Arc::clone(cdp),
        events,
        session.clone(),
        target.clone(),
        Arc::clone(&watch),
        refusal,
    ));
    // Aborted however this ends, and the requests it is making with it.
    let _intercepting = AbortOnDrop(intercepting);
    let s = Some(session.as_str());
    cdp.call(
        s,
        "Fetch.enable",
        json!({"patterns": [{"urlPattern": "*", "requestStage": "Request"}]}),
    )
    .await?;
    cdp.call(s, "Page.enable", json!({})).await?;
    let navigated = cdp
        .call(s, "Page.navigate", json!({"url": url.as_str()}))
        .await?;
    if let Some(error) = navigated.get("errorText").and_then(Value::as_str) {
        let w = lock(&watch);
        return Err(match w.refused.first() {
            Some(why) => format!("{url}: {why}"),
            None => format!("{url}: the browser could not load it: {error}"),
        });
    }
    settle(&watch).await;
    let read = cdp
        .call(
            s,
            "Runtime.evaluate",
            json!({"expression": extract(), "returnByValue": true}),
        )
        .await?;
    if let Some(details) = read.get("exceptionDetails") {
        return Err(format!("reading the page failed: {details}"));
    }
    let page = &read["result"]["value"];
    let image = match screenshot {
        true => {
            let shot = cdp
                .call(
                    s,
                    "Page.captureScreenshot",
                    json!({"format": "jpeg", "quality": 80}),
                )
                .await?;
            Some(Image::new("image/jpeg", &string(&shot, "data")?)?)
        }
        false => None,
    };
    let w = lock(&watch);
    Ok(Rendered {
        url: page["url"].as_str().unwrap_or(url.as_str()).to_string(),
        status: w.document.as_ref().map(|(status, _)| *status),
        title: page["title"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string(),
        text: page["text"].as_str().unwrap_or_default().to_string(),
        image,
        refused: w.refused.clone(),
        refusals: w.refusals,
    })
}

/// The page's title, URL and visible text, the text cut in the page. A lone surrogate,
/// which the cut can make of an emoji, is replaced so the reply is valid JSON.
fn extract() -> String {
    format!(
        "((w) => ({{title: w(document.title), url: location.href, text: w((document.body || \
document.documentElement || {{innerText: ''}}).innerText.slice(0, {MAX_TEXT}))}}))\
((s) => String(s).replace(/[\\uD800-\\uDBFF](?![\\uDC00-\\uDFFF])|(?<![\\uD800-\\uDBFF])[\\uDC00-\\uDFFF]/g, '\\uFFFD'))"
    )
}

/// Wait for the load event, then for the requests to go quiet.
async fn settle(watch: &Mutex<Watch>) {
    let begun = Instant::now();
    while !lock(watch).loaded && begun.elapsed() < LOAD {
        tokio::time::sleep(TICK).await;
    }
    let loaded = Instant::now();
    while loaded.elapsed() < SETTLE {
        {
            let w = lock(watch);
            if w.open == 0 && w.changed.elapsed() >= IDLE {
                return;
            }
        }
        tokio::time::sleep(TICK).await;
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn lock(watch: &Mutex<Watch>) -> std::sync::MutexGuard<'_, Watch> {
    watch.lock().unwrap_or_else(|e| e.into_inner())
}

fn string(value: &Value, field: &str) -> Result<String, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("the browser answered without `{field}`: {value}"))
}

/// Answer the page's paused requests and note its load, until the socket closes.
async fn intercept(
    cdp: Arc<Cdp>,
    mut events: mpsc::UnboundedReceiver<Event>,
    session: String,
    frame: String,
    watch: Arc<Mutex<Watch>>,
    refusal: fn(IpAddr) -> Option<&'static str>,
) {
    let mut answering = tokio::task::JoinSet::new();
    while let Some(event) = events.recv().await {
        while answering.try_join_next().is_some() {}
        if event.session.as_deref() != Some(session.as_str()) {
            continue;
        }
        match event.method.as_str() {
            "Page.loadEventFired" => {
                let mut w = lock(&watch);
                w.loaded = true;
                w.changed = Instant::now();
            }
            "Fetch.requestPaused" => {
                {
                    let mut w = lock(&watch);
                    w.open += 1;
                    w.requests += 1;
                    w.changed = Instant::now();
                }
                answering.spawn(answer(
                    Arc::clone(&cdp),
                    session.clone(),
                    frame.clone(),
                    event.params,
                    Arc::clone(&watch),
                    refusal,
                ));
            }
            _ => {}
        }
    }
}

/// How bhai answers one paused request.
#[derive(Debug, PartialEq)]
enum Answer {
    /// Let Chrome have it: a `data:` or `blob:` URL, which reaches no network.
    Continue,
    Fulfil {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Refuse(String),
}

async fn answer(
    cdp: Arc<Cdp>,
    session: String,
    frame: String,
    params: Value,
    watch: Arc<Mutex<Watch>>,
    refusal: fn(IpAddr) -> Option<&'static str>,
) {
    let id = params["requestId"].as_str().unwrap_or_default().to_string();
    let request = &params["request"];
    let url = request["url"].as_str().unwrap_or_default().to_string();
    let over = {
        let w = lock(&watch);
        match (w.requests > MAX_REQUESTS, w.bytes >= MAX_BYTES) {
            (true, _) => Some(format!("the page passed {MAX_REQUESTS} requests")),
            (_, true) => Some(format!("the page passed {} MiB", MAX_BYTES >> 20)),
            _ => None,
        }
    };
    let answer = match over {
        Some(why) => Answer::Refuse(why),
        None => make(request, refusal, &watch).await,
    };
    let document = params["resourceType"] == "Document" && params["frameId"] == frame.as_str();
    let (method, reply) = match answer {
        Answer::Continue => ("Fetch.continueRequest", json!({"requestId": id})),
        Answer::Fulfil {
            status,
            headers,
            body,
        } => {
            let mut w = lock(&watch);
            if document {
                w.document = Some((status, url.clone()));
            }
            let headers: Vec<Value> = headers
                .into_iter()
                .map(|(name, value)| json!({"name": name, "value": value}))
                .collect();
            (
                "Fetch.fulfillRequest",
                json!({
                    "requestId": id,
                    "responseCode": status,
                    "responseHeaders": headers,
                    "body": crate::clipboard::base64(&body),
                }),
            )
        }
        Answer::Refuse(why) => {
            let mut w = lock(&watch);
            w.refusals += 1;
            if w.refused.len() < 3 {
                w.refused.push(why);
            }
            (
                "Fetch.failRequest",
                json!({"requestId": id, "errorReason": "BlockedByClient"}),
            )
        }
    };
    let _ = cdp.call(Some(&session), method, reply).await;
    let mut w = lock(&watch);
    w.open = w.open.saturating_sub(1);
    w.changed = Instant::now();
}

/// Request headers Chrome hands over that bhai's own client sets, or that would ask for
/// an encoding it does not decode.
const DROPPED_REQUEST: [&str; 9] = [
    "host",
    "connection",
    "content-length",
    "accept-encoding",
    "transfer-encoding",
    "upgrade",
    "keep-alive",
    "te",
    "trailer",
];
/// Response headers that describe the bytes on the wire rather than the body handed on.
const DROPPED_RESPONSE: [&str; 4] = [
    "content-encoding",
    "content-length",
    "transfer-encoding",
    "connection",
];

/// Make the request Chrome paused, through the guard and without following a redirect.
/// Each chunk read is counted into `watch` as it arrives, so requests in flight together
/// share the page's byte cap.
async fn make(
    request: &Value,
    refusal: fn(IpAddr) -> Option<&'static str>,
    watch: &Mutex<Watch>,
) -> Answer {
    let Some(Ok(url)) = request["url"].as_str().map(Url::parse) else {
        return Answer::Refuse(format!(
            "a request to an unreadable URL: {}",
            request["url"]
        ));
    };
    match url.scheme() {
        "data" | "blob" => return Answer::Continue,
        "http" | "https" => {}
        other => return Answer::Refuse(format!("{url}: {other} is not fetched")),
    }
    let addr = match ssrf::check_with(&url, refusal).await {
        Ok(addr) => addr,
        Err(why) => return Answer::Refuse(why),
    };
    let client = match ssrf::pinned(&url, addr, REQUEST_TIMEOUT) {
        Ok(client) => client,
        Err(why) => return Answer::Refuse(why),
    };
    let method = request["method"]
        .as_str()
        .and_then(|m| Method::from_bytes(m.as_bytes()).ok())
        .unwrap_or(Method::GET);
    let mut builder = client.request(method, url.clone());
    if let Some(headers) = request["headers"].as_object() {
        for (name, value) in headers {
            let lower = name.to_ascii_lowercase();
            if DROPPED_REQUEST.contains(&lower.as_str()) || lower.starts_with("proxy-") {
                continue;
            }
            if let Some(value) = value.as_str() {
                builder = builder.header(name.as_str(), value);
            }
        }
    }
    if let Some(body) = post_data(request) {
        builder = builder.body(body);
    }
    let mut response = match builder.send().await {
        Ok(response) => response,
        Err(e) => return Answer::Refuse(format!("{url}: {e}")),
    };
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter(|(name, _)| !DROPPED_RESPONSE.contains(&name.as_str()))
        .map(|(name, value)| {
            (
                name.to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if body.len() + chunk.len() > MAX_RESPONSE => {
                return Answer::Refuse(format!(
                    "{url}: the response passed {} MiB",
                    MAX_RESPONSE >> 20
                ));
            }
            Ok(Some(chunk)) => {
                {
                    let mut w = lock(watch);
                    if w.bytes + chunk.len() > MAX_BYTES {
                        return Answer::Refuse(format!(
                            "{url}: the page passed {} MiB",
                            MAX_BYTES >> 20
                        ));
                    }
                    w.bytes += chunk.len();
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => return Answer::Refuse(format!("{url}: reading the body: {e}")),
        }
    }
    Answer::Fulfil {
        status,
        headers,
        body,
    }
}

/// The body of a paused request: its `postDataEntries` decoded, or its `postData`.
fn post_data(request: &Value) -> Option<Vec<u8>> {
    if let Some(entries) = request["postDataEntries"].as_array() {
        let mut body = Vec::new();
        for entry in entries {
            body.extend(crate::clipboard::unbase64(entry["bytes"].as_str()?)?);
        }
        return Some(body);
    }
    request["postData"].as_str().map(|s| s.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::header;
    use axum::routing::get;
    use futures_util::{SinkExt, StreamExt};
    use std::net::Ipv4Addr;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::tungstenite::Message;

    fn but_local(ip: IpAddr) -> Option<&'static str> {
        if ip == IpAddr::V4(Ipv4Addr::LOCALHOST) {
            return None;
        }
        ssrf::refusal(ip)
    }

    async fn site() -> String {
        let app = Router::new()
            .route(
                "/app",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/html")],
                        r#"<div id="root"></div><script>root.textContent = "drawn by script"</script>"#,
                    )
                }),
            )
            .route(
                "/hop",
                get(|| async { axum::response::Redirect::temporary("/app") }),
            )
            .route("/mib", get(|| async { vec![b'x'; 1 << 20] }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn a_paused_request_is_made_through_the_guard_one_hop_at_a_time() {
        let base = site().await;
        let watch = Mutex::new(Watch::new());
        let request = |url: &str| json!({"url": url, "method": "GET", "headers": {"Accept-Encoding": "gzip", "X-Kept": "1"}});
        match make(&request(&format!("{base}/app")), but_local, &watch).await {
            Answer::Fulfil {
                status,
                headers,
                body,
            } => {
                assert_eq!(status, 200);
                assert!(String::from_utf8(body).unwrap().contains("drawn by script"));
                assert!(
                    headers
                        .iter()
                        .any(|(n, v)| n == "content-type" && v == "text/html")
                );
                assert!(!headers.iter().any(|(n, _)| n == "content-length"));
            }
            other => panic!("{other:?}"),
        }
        match make(&request(&format!("{base}/hop")), but_local, &watch).await {
            Answer::Fulfil {
                status, headers, ..
            } => {
                assert_eq!(status, 307);
                assert!(headers.iter().any(|(n, v)| n == "location" && v == "/app"));
            }
            other => panic!("{other:?}"),
        }
        for url in [
            format!("{base}/app"),
            "http://169.254.169.254/latest/".to_string(),
            "http://[::1]/".to_string(),
        ] {
            match make(&request(&url), ssrf::refusal, &watch).await {
                Answer::Refuse(why) => assert!(why.starts_with("refused:"), "{why}"),
                other => panic!("{url}: {other:?}"),
            }
        }
        assert_eq!(
            make(&request("data:text/plain,hi"), ssrf::refusal, &watch).await,
            Answer::Continue
        );
        assert!(matches!(
            make(&request("file:///etc/passwd"), ssrf::refusal, &watch).await,
            Answer::Refuse(_)
        ));
    }

    #[test]
    fn post_data_is_read_from_its_entries_or_its_string() {
        let entries = json!({"postDataEntries": [{"bytes": crate::clipboard::base64(b"a=")}, {"bytes": crate::clipboard::base64(b"1")}]});
        assert_eq!(post_data(&entries).unwrap(), b"a=1");
        assert_eq!(post_data(&json!({"postData": "x"})).unwrap(), b"x");
        assert_eq!(post_data(&json!({})), None);
    }

    #[test]
    fn the_browser_runs_on_its_own_profile_behind_a_dead_proxy() {
        let args = args(Path::new("/tmp/p"));
        for want in [
            "--headless=new",
            "--user-data-dir=/tmp/p",
            "--proxy-server=http://127.0.0.1:9",
            "--proxy-bypass-list=<-loopback>",
            "--use-mock-keychain",
        ] {
            assert!(args.iter().any(|a| a == want), "{want}: {args:?}");
        }
        assert_eq!(args.last().unwrap(), "about:blank");
    }

    #[test]
    fn browsers_are_looked_for_in_the_usual_places() {
        let found = candidates(
            Some(Path::new("/Users/u")),
            Some(std::ffi::OsStr::new("/usr/bin:/opt/bin")),
        );
        assert!(found.contains(&PathBuf::from("/usr/bin/google-chrome")));
        assert!(found.contains(&PathBuf::from("/opt/bin/chromium")));
        if cfg!(target_os = "macos") {
            assert_eq!(
                found[0],
                PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
            );
            assert!(found.contains(&PathBuf::from(
                "/Users/u/Applications/Chromium.app/Contents/MacOS/Chromium"
            )));
        }
    }

    /// What the fake browser was sent: each command's method and params, in order.
    type Sent = Arc<Mutex<Vec<(String, Value, Option<String>)>>>;

    /// A browser end that loads a page the way Chrome does over CDP: on `Page.navigate`
    /// it pauses `requests` and answers the navigation once each is fulfilled or failed.
    async fn fake_browser(
        requests: Vec<String>,
        sent: Sent,
    ) -> (Arc<Cdp>, mpsc::UnboundedReceiver<Event>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut navigation = None;
            let mut answered = 0;
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let command: Value = serde_json::from_str(text.as_str()).unwrap();
                let method = command["method"].as_str().unwrap().to_string();
                let session = command["sessionId"].as_str().map(str::to_string);
                sent.lock()
                    .unwrap()
                    .push((method.clone(), command["params"].clone(), session));
                let id = command["id"].clone();
                let mut out = Vec::new();
                let result = match method.as_str() {
                    "Target.createTarget" => json!({"targetId": "T1"}),
                    "Target.attachToTarget" => json!({"sessionId": "S1"}),
                    "Page.navigate" => {
                        navigation = Some(id.clone());
                        for (n, url) in requests.iter().enumerate() {
                            out.push(json!({"method": "Fetch.requestPaused", "sessionId": "S1", "params": {
                                "requestId": format!("r{n}"), "frameId": "T1",
                                "resourceType": if n == 0 { "Document" } else { "Script" },
                                "request": {"url": url, "method": "GET", "headers": {}}
                            }}));
                        }
                        Value::Null
                    }
                    "Fetch.fulfillRequest" | "Fetch.failRequest" | "Fetch.continueRequest" => {
                        answered += 1;
                        if answered == requests.len()
                            && let Some(nav) = navigation.take()
                        {
                            out.push(json!({"id": nav, "result": {"frameId": "T1"}}));
                            out.push(json!({"method": "Page.loadEventFired", "sessionId": "S1", "params": {}}));
                        }
                        json!({})
                    }
                    "Runtime.evaluate" => json!({"result": {"type": "object", "value": {
                        "title": " Drawn ", "url": "http://page.test/app", "text": "drawn by script"
                    }}}),
                    "Page.captureScreenshot" => {
                        json!({"data": crate::clipboard::base64(b"\xff\xd8\xffjpeg")})
                    }
                    _ => json!({}),
                };
                if !result.is_null() {
                    out.insert(0, json!({"id": id, "result": result}));
                }
                for frame in out {
                    ws.send(Message::Text(frame.to_string().into()))
                        .await
                        .unwrap();
                }
            }
        });
        let stream = TcpStream::connect(addr).await.unwrap();
        let (ws, _) =
            tokio_tungstenite::client_async(format!("ws://{addr}/devtools/browser/x"), stream)
                .await
                .unwrap();
        Cdp::start(ws)
    }

    #[tokio::test]
    async fn a_page_loads_with_each_request_answered_through_the_guard() {
        let base = site().await;
        let sent = Sent::default();
        let (cdp, events) = fake_browser(
            vec![
                format!("{base}/app"),
                "http://169.254.169.254/latest/meta-data/".to_string(),
                "data:text/javascript,1".to_string(),
            ],
            Arc::clone(&sent),
        )
        .await;
        let url = Url::parse(&format!("{base}/app")).unwrap();
        let page = load(&cdp, events, &url, true, but_local).await.unwrap();
        assert_eq!(page.title, "Drawn");
        assert_eq!(page.text, "drawn by script");
        assert_eq!(page.url, "http://page.test/app");
        assert_eq!(page.status, Some(200));
        assert_eq!(page.refusals, 1);
        assert!(page.refused[0].contains("link-local"), "{:?}", page.refused);
        assert_eq!(page.image.unwrap().mime, "image/jpeg");

        let sent = sent.lock().unwrap();
        let find = |method: &str| {
            sent.iter()
                .filter(|(m, _, _)| m == method)
                .collect::<Vec<_>>()
        };
        assert_eq!(find("Browser.setDownloadBehavior")[0].1["behavior"], "deny");
        let enable = find("Fetch.enable")[0];
        assert_eq!(enable.1["patterns"][0]["urlPattern"], "*");
        assert_eq!(enable.2.as_deref(), Some("S1"));
        let fulfilled = find("Fetch.fulfillRequest");
        assert_eq!(fulfilled.len(), 1);
        assert_eq!(fulfilled[0].1["requestId"], "r0");
        assert_eq!(fulfilled[0].1["responseCode"], 200);
        let body = crate::clipboard::unbase64(fulfilled[0].1["body"].as_str().unwrap()).unwrap();
        assert!(String::from_utf8(body).unwrap().contains("drawn by script"));
        let failed = find("Fetch.failRequest");
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].1["requestId"], "r1");
        assert_eq!(find("Fetch.continueRequest")[0].1["requestId"], "r2");
        assert_eq!(find("Page.navigate")[0].1["url"], url.as_str());
    }

    #[tokio::test]
    async fn a_refused_document_names_why() {
        let sent = Sent::default();
        let (cdp, events) =
            fake_browser(vec!["http://10.0.0.1/".to_string()], Arc::clone(&sent)).await;
        // The fake answers the navigation as loaded; Chrome would give an errorText.
        let url = Url::parse("http://10.0.0.1/").unwrap();
        let page = load(&cdp, events, &url, false, ssrf::refusal)
            .await
            .unwrap();
        assert_eq!(page.status, None);
        assert!(page.refused[0].contains("private"), "{:?}", page.refused);
    }

    #[tokio::test]
    async fn requests_in_flight_together_share_the_page_byte_cap() {
        let base = site().await;
        let sent = Sent::default();
        let count = (MAX_BYTES >> 20) + 10;
        let (cdp, events) =
            fake_browser(vec![format!("{base}/mib"); count], Arc::clone(&sent)).await;
        let url = Url::parse(&format!("{base}/mib")).unwrap();
        let page = load(&cdp, events, &url, false, but_local).await.unwrap();

        let sent = sent.lock().unwrap();
        let fulfilled: usize = sent
            .iter()
            .filter(|(m, _, _)| m == "Fetch.fulfillRequest")
            .map(|(_, p, _)| {
                crate::clipboard::unbase64(p["body"].as_str().unwrap())
                    .unwrap()
                    .len()
            })
            .sum();
        assert!(fulfilled <= MAX_BYTES, "{fulfilled} bytes handed on");
        assert!(fulfilled > 0);
        assert!(
            page.refusals >= count - (MAX_BYTES >> 20),
            "{}",
            page.refusals
        );
        assert!(page.refused[0].contains("MiB"), "{:?}", page.refused);
    }

    /// Needs a Chrome or Chromium on the machine: `cargo test -- --ignored real_chrome`.
    #[tokio::test]
    #[ignore]
    async fn real_chrome_renders_a_script_drawn_page() {
        let exe = find().expect("no browser to test with");
        let base = site().await;
        let url = Url::parse(&format!("{base}/hop")).unwrap();
        let page = render(&exe, &url, true, but_local).await.unwrap();
        assert_eq!(page.text.trim(), "drawn by script", "{page:?}");
        assert!(page.url.ends_with("/app"), "{}", page.url);
        assert_eq!(page.status, Some(200));
        assert!(page.image.is_some());
        assert!(browsers().is_empty());
    }
}
