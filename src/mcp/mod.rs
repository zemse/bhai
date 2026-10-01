//! MCP client: stdio servers started once at launch. Their tool schemas stay out of the
//! tool list; the model finds them with `mcp_search` and runs them with `mcp_call`, so
//! the tool list and prompt prefix stay fixed however many servers there are. A server
//! only a child identity allows starts on that child's first MCP call and is then shared.

use std::collections::HashMap;
use std::fmt;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::{Peer, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;

use crate::config::Config;
use crate::identity::Identity;
use crate::instructions::Roots;

pub mod servers;

use servers::Server;

/// How long a server gets to start, initialize and list its tools, unless its entry sets
/// `startup_timeout_sec`.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a server gets to answer one call, unless its entry sets `tool_timeout_sec`.
/// Generous: MCP tools can legitimately run for minutes, and an interrupt already ends
/// the wait sooner.
const CALL_TIMEOUT: Duration = Duration::from_secs(600);
/// Tool names the prompt line shows per server.
const PROMPT_TOOLS: usize = 3;
/// How much of a name the listing shows. A server names its own tools, and the listing is
/// one line per server inside the cached prefix, so the text it can put there is bounded.
const PROMPT_NAME: usize = 64;
/// Results `mcp_search` returns.
const SEARCH_RESULTS: usize = 5;

/// One MCP tool, as `mcp_search` shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolInfo {
    pub server: String,
    pub name: String,
    pub description: String,
    pub schema: Value,
}

impl ToolInfo {
    #[cfg(test)]
    pub fn test(server: &str, name: &str, description: &str) -> Self {
        Self {
            server: server.to_string(),
            name: name.to_string(),
            description: description.to_string(),
            schema: serde_json::json!({"type": "object"}),
        }
    }

    /// `mcp__server__tool`.
    pub fn full_name(&self) -> String {
        format!("mcp__{}__{}", self.server, self.name)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Connected,
    Failed(String),
    Skipped(String),
}

#[derive(Debug, Clone)]
pub struct Status {
    pub name: String,
    pub source: String,
    pub state: State,
    pub tools: Vec<ToolInfo>,
}

type Service = RunningService<RoleClient, ()>;

/// A stdio server's process group, killed when dropped. `npx`, `uvx` and `docker run`
/// start the real server as a grandchild, which killing only the wrapper leaves running.
struct Group(u32);

impl Drop for Group {
    fn drop(&mut self) {
        crate::tools::bash::kill_group(Some(self.0));
    }
}

/// A connected server, and its process group when it is a stdio one.
type Running = (Service, Option<Group>);

/// What every view of a session's hub shares.
struct Shared {
    /// Taken on shutdown; `None` after it.
    services: Mutex<Option<Vec<Running>>>,
    /// Servers started at launch, with all their tools.
    launched: Vec<Status>,
    /// Servers the launch identity did not allow, which a child may start.
    deferred: Vec<Server>,
    /// Deferred servers started so far, with all their tools.
    late: tokio::sync::Mutex<Vec<(Status, Option<Peer<RoleClient>>)>>,
    log_dir: PathBuf,
    timeout: Duration,
    /// Each server's call timeout, by name.
    call_timeouts: HashMap<String, Duration>,
}

/// The servers of a session as one identity sees them: their status, tools, and the
/// connections to call them.
pub struct Hub {
    pub servers: Vec<Status>,
    peers: Vec<(String, Peer<RoleClient>)>,
    /// Deferred servers this view starts on its first search or call.
    deferred: Vec<String>,
    identity: Identity,
    /// Only the session's own hub closes the servers.
    root: bool,
    shared: Arc<Shared>,
}

impl fmt::Debug for Hub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hub")
            .field("servers", &self.servers)
            .field("deferred", &self.deferred)
            .finish()
    }
}

/// The session's hub, or `None` when MCP is off.
pub async fn start(config: &Config, roots: &Roots, identity: &Identity) -> Option<Arc<Hub>> {
    if !config.mcp {
        return None;
    }
    let servers = servers::load(roots, &config.mcp_servers);
    let log_dir = roots.cwd.join(".bhai").join("debug");
    Some(Arc::new(
        Hub::connect(servers, identity, &log_dir, START_TIMEOUT).await,
    ))
}

impl Hub {
    /// Start every allowed server at once. A server that fails is only marked failed.
    pub async fn connect(
        servers: Vec<Server>,
        identity: &Identity,
        log_dir: &Path,
        timeout: Duration,
    ) -> Self {
        let mut deferred = Vec::new();
        let mut starts = Vec::new();
        let call_timeouts = servers
            .iter()
            .map(|s| (s.name.clone(), s.tool_timeout.unwrap_or(CALL_TIMEOUT)))
            .collect();
        for server in servers {
            if server.skip.is_none() && !identity.allows_mcp_server(&server.name) {
                deferred.push(server.clone());
            }
            starts.push(async move {
                let skip = server.skip.clone().or_else(|| {
                    (!identity.allows_mcp_server(&server.name))
                        .then(|| format!("not allowed by identity {}", identity.name))
                });
                match skip {
                    Some(reason) => (skipped(&server, reason), None),
                    None => start_one(&server, log_dir, timeout).await,
                }
            });
        }
        let mut launched = Vec::new();
        let mut peers = Vec::new();
        let mut services = Vec::new();
        for (status, service) in futures_util::future::join_all(starts).await {
            if let Some(service) = service {
                peers.push((status.name.clone(), service.0.peer().clone()));
                services.push(service);
            }
            launched.push(status);
        }
        let shared = Arc::new(Shared {
            services: Mutex::new(Some(services)),
            launched,
            deferred,
            late: tokio::sync::Mutex::default(),
            log_dir: log_dir.to_path_buf(),
            timeout,
            call_timeouts,
        });
        Hub {
            peers,
            root: true,
            ..Hub::view(shared, identity)
        }
    }

    /// The shared servers as `identity` sees them, with no connections yet.
    fn view(shared: Arc<Shared>, identity: &Identity) -> Hub {
        let servers = shared
            .launched
            .iter()
            .map(|server| {
                let mut server = server.clone();
                if !identity.allows_mcp_server(&server.name) {
                    server.tools.clear();
                } else {
                    server
                        .tools
                        .retain(|t| identity.allows_mcp_tool(&server.name, &t.name));
                }
                server
            })
            .collect();
        let deferred = shared
            .deferred
            .iter()
            .filter(|s| identity.allows_mcp_server(&s.name))
            .map(|s| s.name.clone())
            .collect();
        Hub {
            servers,
            peers: Vec::new(),
            deferred,
            identity: identity.clone(),
            root: false,
            shared,
        }
    }

    /// This hub as `identity` sees it: the same connections, only its servers and tools,
    /// plus the servers it may start on first use. Closing the servers stays with the
    /// session's hub.
    pub fn narrowed(&self, identity: &Identity) -> Hub {
        Hub {
            peers: self.peers.clone(),
            ..Hub::view(Arc::clone(&self.shared), identity)
        }
    }

    /// A hub of connected servers with these tools and no processes behind them.
    #[cfg(test)]
    pub fn offline(servers: Vec<(&str, Vec<ToolInfo>)>) -> Self {
        let launched = servers
            .into_iter()
            .map(|(name, tools)| Status {
                name: name.to_string(),
                source: "test".to_string(),
                state: State::Connected,
                tools,
            })
            .collect();
        let shared = Arc::new(Shared {
            services: Mutex::new(Some(Vec::new())),
            launched,
            deferred: Vec::new(),
            late: tokio::sync::Mutex::default(),
            log_dir: PathBuf::new(),
            timeout: START_TIMEOUT,
            call_timeouts: HashMap::new(),
        });
        Hub {
            root: true,
            ..Hub::view(shared, &Identity::default())
        }
    }

    /// Start this view's deferred servers that no one has started yet.
    async fn start_deferred(&self) {
        if self.deferred.is_empty() {
            return;
        }
        let mut late = self.shared.late.lock().await;
        let pending: Vec<&Server> = self
            .shared
            .deferred
            .iter()
            .filter(|s| self.deferred.contains(&s.name))
            .filter(|s| !late.iter().any(|(status, _)| status.name == s.name))
            .collect();
        let starts = pending
            .into_iter()
            .map(|server| start_one(server, &self.shared.log_dir, self.shared.timeout));
        for (mut status, service) in futures_util::future::join_all(starts).await {
            let mut peer = None;
            if let Some(service) = service {
                let mut services = self
                    .shared
                    .services
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                match services.as_mut() {
                    Some(services) => {
                        peer = Some(service.0.peer().clone());
                        services.push(service);
                    }
                    None => {
                        status.state = State::Failed("the session is closing".to_string());
                        status.tools.clear();
                    }
                }
            }
            late.push((status, peer));
        }
    }

    /// The tools of this view's deferred servers that are running, as it may see them.
    async fn late_tools(&self) -> Vec<ToolInfo> {
        self.start_deferred().await;
        let late = self.shared.late.lock().await;
        late.iter()
            .filter(|(status, _)| self.deferred.contains(&status.name))
            .flat_map(|(status, _)| status.tools.iter())
            .filter(|t| self.identity.allows_mcp_tool(&t.server, &t.name))
            .cloned()
            .collect()
    }

    /// Every tool of every connected server.
    pub fn tools(&self) -> impl Iterator<Item = &ToolInfo> {
        self.servers.iter().flat_map(|s| s.tools.iter())
    }

    /// Whether this view has tools now or may start servers that have some.
    pub fn has_tools(&self) -> bool {
        self.tools().next().is_some() || !self.deferred.is_empty()
    }

    /// The system prompt section: one line per server with tools, and one per deferred
    /// server. It depends only on the config and what started at launch.
    pub fn prompt_section(&self) -> String {
        if !self.has_tools() {
            return String::new();
        }
        let mut text = String::from(
            "\n\n# MCP\n\nMCP tools are not in your tool list. Find one with `mcp_search`, \
then run it with `mcp_call` using the exact `mcp__server__tool` name.\n",
        );
        for server in self.servers.iter().filter(|s| !s.tools.is_empty()) {
            let mut names: Vec<String> = server
                .tools
                .iter()
                .take(PROMPT_TOOLS)
                .map(|t| listed(&t.name))
                .collect();
            if server.tools.len() > PROMPT_TOOLS {
                names.push("...".to_string());
            }
            let _ = write!(
                text,
                "\nmcp server {}: {} tools ({})",
                listed(&server.name),
                server.tools.len(),
                names.join(", ")
            );
        }
        for name in &self.deferred {
            let _ = write!(text, "\nmcp server {}: starts on first use", listed(name));
        }
        text
    }

    /// The best matches for `query`, with their schemas, as `mcp_search` returns them.
    /// Starts this view's deferred servers first.
    pub async fn search(&self, query: &str) -> String {
        let mut tools: Vec<ToolInfo> = self.tools().cloned().collect();
        tools.extend(self.late_tools().await);
        search(&tools, query)
    }

    pub fn find(&self, full_name: &str) -> Option<&ToolInfo> {
        self.tools().find(|t| t.full_name() == full_name)
    }

    /// Whether `full_name` may be a tool of this view: a known one, or one of a deferred
    /// server that the identity allows.
    pub fn may_call(&self, full_name: &str) -> bool {
        if self.find(full_name).is_some() {
            return true;
        }
        split(full_name).is_some_and(|(server, tool)| {
            self.deferred.iter().any(|d| d == server) && self.identity.allows_mcp_tool(server, tool)
        })
    }

    /// Run `full_name`; returns the output and whether it succeeded. A tool of a deferred
    /// server starts that server first.
    pub async fn call(&self, full_name: &str, arguments: Value) -> (String, bool) {
        let found = match self.find(full_name) {
            Some(tool) => Some((
                tool.clone(),
                self.peers
                    .iter()
                    .find(|(name, _)| *name == tool.server)
                    .map(|(_, peer)| peer.clone()),
            )),
            None => self.find_late(full_name).await,
        };
        let Some((tool, peer)) = found else {
            return (
                format!(
                    "No MCP tool named `{full_name}`. Use `mcp_search` to find the exact name."
                ),
                false,
            );
        };
        let Some(peer) = peer else {
            return (
                format!("MCP server `{}` is not connected.", tool.server),
                false,
            );
        };
        let timeout = self
            .shared
            .call_timeouts
            .get(&tool.server)
            .copied()
            .unwrap_or(CALL_TIMEOUT);
        let mut params = CallToolRequestParams::new(tool.name.clone());
        if let Value::Object(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        match tokio::time::timeout(timeout, peer.call_tool(params)).await {
            Ok(Ok(result)) => render(&result),
            Ok(Err(e)) => (
                crate::redact::apply(&format!("MCP call failed: {e}")).into_owned(),
                false,
            ),
            Err(_) => (
                format!(
                    "MCP server `{}` did not answer in {}s.",
                    tool.server,
                    timeout.as_secs_f64()
                ),
                false,
            ),
        }
    }

    /// A tool of a deferred server and its connection, starting the server if needed.
    async fn find_late(&self, full_name: &str) -> Option<(ToolInfo, Option<Peer<RoleClient>>)> {
        if !self.may_call(full_name) {
            return None;
        }
        let tool = self
            .late_tools()
            .await
            .into_iter()
            .find(|t| t.full_name() == full_name)?;
        let late = self.shared.late.lock().await;
        let peer = late
            .iter()
            .find(|(status, _)| status.name == tool.server)
            .and_then(|(_, peer)| peer.clone());
        Some((tool, peer))
    }

    /// Close every server; each is killed if it does not exit in a few seconds, and what a
    /// stdio server left behind in its process group goes with it. Only the session's hub
    /// does this.
    pub async fn shutdown(&self) {
        if !self.root {
            return;
        }
        let services = self
            .shared
            .services
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap_or_default();
        futures_util::future::join_all(services.into_iter().map(|(service, group)| async move {
            let _ = service.cancel().await;
            drop(group);
        }))
        .await;
    }

    /// What `/mcp` prints.
    pub fn report(&self) -> String {
        let connected = self
            .servers
            .iter()
            .filter(|s| s.state == State::Connected)
            .count();
        let listing = self.prompt_section().len().div_ceil(4);
        let mut out = format!(
            "mcp: {connected} of {} servers connected, {} tools, listing ~{listing} tok \
(schemas load on demand through mcp_search)",
            self.servers.len(),
            self.tools().count()
        );
        let late = self.shared.late.try_lock().ok();
        for server in &self.servers {
            let started = late
                .iter()
                .flat_map(|late| late.iter())
                .find(|(status, _)| status.name == server.name)
                .map(|(status, _)| status);
            let (status, note) = match started {
                Some(status) => (status, ", for a child"),
                None => (server, ""),
            };
            let state = match &status.state {
                State::Connected => format!("connected  {} tools{note}", status.tools.len()),
                State::Failed(why) => format!("failed     {why}"),
                State::Skipped(why) => format!("skipped    {why}"),
            };
            let _ = write!(out, "\n  {}  ({})  {state}", server.name, server.source);
        }
        if self.servers.is_empty() {
            out.push_str("\n  no servers configured");
        }
        out
    }

    /// Startup lines for servers that failed.
    pub fn notices(&self) -> Vec<String> {
        self.servers
            .iter()
            .filter_map(|s| match &s.state {
                State::Failed(why) => Some(format!("mcp server {} failed: {why}", s.name)),
                _ => None,
            })
            .collect()
    }
}

/// `tests/fixtures/fake_mcp.py` as a server, or `None` without python3.
#[cfg(test)]
pub fn fake_server(name: &str, mode: &str) -> Option<Server> {
    let found = std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        eprintln!("skipped: python3 is not available");
        return None;
    }
    let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_mcp.py");
    Some(Server {
        name: name.to_string(),
        source: "test".to_string(),
        command: "python3".to_string(),
        args: vec![fake.to_string(), mode.to_string()],
        env: std::collections::BTreeMap::new(),
        url: None,
        headers: crate::config::Headers::default(),
        skip: None,
        startup_timeout: None,
        tool_timeout: None,
    })
}

/// `fake_mcp.py http` on a free port, with the token it wants in a header.
#[cfg(test)]
pub fn fake_http_server(name: &str) -> Option<(Server, std::process::Child)> {
    use std::io::BufRead as _;

    let stdio = fake_server(name, "http")?;
    let mut child = std::process::Command::new(&stdio.command)
        .args(&stdio.args)
        .stdout(Stdio::piped())
        .spawn()
        .expect("starting the http fake");
    let mut line = String::new();
    let stdout = child.stdout.take().expect("piped stdout");
    std::io::BufReader::new(stdout)
        .read_line(&mut line)
        .expect("reading the port");
    let port = serde_json::from_str::<Value>(&line).expect("port line")["port"]
        .as_u64()
        .expect("port number");
    let headers = std::collections::BTreeMap::from([(
        "Authorization".to_string(),
        "Bearer s3cret".to_string(),
    )]);
    let server = Server {
        url: Some(format!("http://127.0.0.1:{port}/mcp")),
        headers: crate::config::Headers(headers),
        ..stdio
    };
    Some((server, child))
}

/// What `/mcp` prints for a session's hub.
pub fn report(hub: Option<&Hub>) -> String {
    match hub {
        Some(hub) => hub.report(),
        None => "mcp: off (set `mcp = true` in ~/.config/bhai/config.toml)".to_string(),
    }
}

/// A status for a server that is not started.
fn skipped(server: &Server, reason: String) -> Status {
    Status {
        name: server.name.clone(),
        source: server.source.clone(),
        state: State::Skipped(reason),
        tools: Vec::new(),
    }
}

/// Start one server within its own startup timeout, or `timeout` when it sets none; a
/// failure is only marked in its status.
async fn start_one(
    server: &Server,
    log_dir: &Path,
    timeout: Duration,
) -> (Status, Option<Running>) {
    let timeout = server.startup_timeout.unwrap_or(timeout);
    let mut status = Status {
        state: State::Connected,
        ..skipped(server, String::new())
    };
    match tokio::time::timeout(timeout, spawn(server, log_dir)).await {
        Ok(Ok((service, group, tools))) => {
            status.tools = tools;
            (status, Some((service, group)))
        }
        Ok(Err(e)) => {
            status.state = State::Failed(format!("{e:#}"));
            (status, None)
        }
        Err(_) => {
            status.state = State::Failed(format!("timed out after {}s", timeout.as_secs_f64()));
            (status, None)
        }
    }
}

/// The server and tool of `mcp__server__tool`.
/// A name as the listing shows it: one line, printable, and no longer than a name needs to
/// be. What a server calls itself and its tools is the server's to decide, and this is the
/// one place it lands in the prompt.
fn listed(name: &str) -> String {
    let clean: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(PROMPT_NAME)
        .collect();
    match clean.chars().count() < name.chars().filter(|c| !c.is_control()).count() {
        true => format!("{clean}..."),
        false => clean,
    }
}

fn split(full_name: &str) -> Option<(&str, &str)> {
    full_name.strip_prefix("mcp__")?.split_once("__")
}

/// The best matches for `query` among `tools`, with their schemas.
fn search(tools: &[ToolInfo], query: &str) -> String {
    let words: Vec<String> = query
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    let mut hits: Vec<(usize, &ToolInfo)> = tools
        .iter()
        .map(|tool| (score(tool, &words), tool))
        .filter(|(score, _)| *score > 0)
        .collect();
    hits.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.full_name().cmp(&b.1.full_name()))
    });
    if hits.is_empty() {
        let mut servers: Vec<&str> = Vec::new();
        for tool in tools {
            if !servers.contains(&tool.server.as_str()) {
                servers.push(&tool.server);
            }
        }
        return format!(
            "No MCP tools match `{query}`. Connected servers: {}. Search with other words, \
or a server name to list its tools.",
            servers.join(", ")
        );
    }
    let mut out = String::new();
    for (_, tool) in hits.iter().take(SEARCH_RESULTS) {
        let _ = writeln!(
            out,
            "{}: {}\ninput schema: {}\n",
            tool.full_name(),
            first_line(&tool.description),
            tool.schema
        );
    }
    if hits.len() > SEARCH_RESULTS {
        let _ = writeln!(
            out,
            "{} more matches; narrow the query.",
            hits.len() - SEARCH_RESULTS
        );
    }
    out.trim_end().to_string()
}

/// Connect, initialize and list tools.
async fn spawn(server: &Server, log_dir: &Path) -> Result<(Service, Option<Group>, Vec<ToolInfo>)> {
    let (service, group) = match &server.url {
        Some(url) => (http(server, url).await?, None),
        None => stdio(server, log_dir).await?,
    };
    let mut tools = service
        .list_all_tools()
        .await
        .context("tools/list failed")?
        .into_iter()
        .map(|t| ToolInfo {
            server: server.name.clone(),
            name: t.name.to_string(),
            description: t.description.as_deref().unwrap_or_default().to_string(),
            schema: Value::Object((*t.input_schema).clone()),
        })
        .collect::<Vec<_>>();
    // Sorted so the prompt's tool lines do not depend on the server's listing order.
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((service, group, tools))
}

/// A stdio server as a child process leading its own group. Its stderr goes to
/// `<log_dir>/mcp-<name>.log`.
async fn stdio(server: &Server, log_dir: &Path) -> Result<(Service, Option<Group>)> {
    std::fs::create_dir_all(log_dir).with_context(|| format!("creating {}", log_dir.display()))?;
    let log_path = log_dir.join(format!("mcp-{}.log", file_safe(&server.name)));
    let log = std::fs::File::create(&log_path)
        .with_context(|| format!("creating {}", log_path.display()))?;
    let mut command = tokio::process::Command::new(&server.command);
    crate::childenv::scrub(&mut command);
    command
        .args(&server.args)
        .envs(&server.env)
        .process_group(0)
        .kill_on_drop(true);
    let (transport, _) = TokioChildProcess::builder(command)
        .stderr(Stdio::from(log))
        .spawn()
        .with_context(|| format!("starting `{}`", server.command))?;
    let group = transport.id().map(Group);
    let service = ().serve(transport).await.context("initialize failed")?;
    Ok((service, group))
}

/// A streamable HTTP server. Header values are never named in an error: they carry tokens.
async fn http(server: &Server, url: &str) -> Result<Service> {
    let mut headers = HashMap::new();
    for (name, value) in &server.headers.0 {
        let name = HeaderName::try_from(name).with_context(|| format!("bad header `{name}`"))?;
        crate::redact::register_header(value);
        let value = HeaderValue::from_str(value)
            .with_context(|| format!("bad value for header `{name}`"))?;
        headers.insert(name, value);
    }
    let config = StreamableHttpClientTransportConfig::with_uri(url).custom_headers(headers);
    let transport = StreamableHttpClientTransport::with_client(http_client()?, config);
    ().serve(transport).await.context("initialize failed")
}

/// Redirects an HTTP server may send before the request fails.
const MAX_REDIRECTS: usize = 10;

/// The client for HTTP servers. rmcp's default client follows no redirect at all, so a
/// server that moved `/mcp` to `/mcp/` could not be reached; this one follows the ones
/// `redirect_allowed` passes. reqwest strips only its own credential headers across hosts,
/// so a looser policy would carry a configured token header anywhere. Idle pooling is off
/// as in rmcp's default client: reusing a connection whose body was not drained stalls on
/// delayed ACK.
fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .redirect(reqwest::redirect::Policy::custom(
            |attempt| match redirect_allowed(attempt.url(), attempt.previous()) {
                Ok(()) => attempt.follow(),
                Err(why) => attempt.error(why),
            },
        ))
        .build()
        .context("building the HTTP client")
}

/// A redirect stays on the first URL's origin, goes over HTTPS unless the host is
/// loopback, and the chain stays short. The refusal names only the target's origin: a
/// path or query can carry a token.
fn redirect_allowed(next: &reqwest::Url, previous: &[reqwest::Url]) -> Result<(), String> {
    let Some(first) = previous.first() else {
        return Ok(());
    };
    let origin = next.origin().ascii_serialization();
    // `previous` starts with the original request, which is not a redirect.
    if previous.len() > MAX_REDIRECTS {
        return Err(format!("more than {MAX_REDIRECTS} redirects"));
    }
    if next.origin() != first.origin() {
        return Err(format!("refused a redirect to another origin ({origin})"));
    }
    if next.scheme() != "https" && !loopback(next) {
        return Err(format!("refused a redirect over plain HTTP ({origin})"));
    }
    Ok(())
}

fn loopback(url: &reqwest::Url) -> bool {
    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        None => false,
    }
}

/// Higher for a closer match: whole-name hits beat name substrings beat descriptions,
/// and a tool every word hits gets a bonus.
fn score(tool: &ToolInfo, words: &[String]) -> usize {
    let full = tool.full_name().to_lowercase();
    let name = tool.name.to_lowercase();
    let description = tool.description.to_lowercase();
    let scores: Vec<usize> = words
        .iter()
        .map(|w| {
            if *w == name || *w == full {
                10
            } else if name.contains(w.as_str()) {
                4
            } else if full.contains(w.as_str()) {
                2
            } else if description.contains(w.as_str()) {
                1
            } else {
                0
            }
        })
        .collect();
    let bonus = if words.len() > 1 && scores.iter().all(|&s| s > 0) {
        5
    } else {
        0
    };
    scores.iter().sum::<usize>() + bonus
}

/// The text of a tool result; other content is summarized.
fn render(result: &CallToolResult) -> (String, bool) {
    let mut parts: Vec<String> = result
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.clone(),
            ContentBlock::Image(image) => {
                format!(
                    "[image {}, {} bytes base64]",
                    image.mime_type,
                    image.data.len()
                )
            }
            other => serde_json::to_string(other).unwrap_or_default(),
        })
        .collect();
    if parts.is_empty()
        && let Some(structured) = &result.structured_content
    {
        parts.push(structured.to_string());
    }
    let ok = result.is_error != Some(true);
    let text = crate::redact::apply(&parts.join("\n")).into_owned();
    (crate::tools::truncate(&text), ok)
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default().trim()
}

fn file_safe(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake(name: &str, mode: &str) -> Server {
        fake_server(name, mode).unwrap()
    }

    fn python() -> bool {
        fake_server("probe", "").is_some()
    }

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("bhai-mcp-hub-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn a_secret_across_the_truncation_cut_is_redacted_whole() {
        crate::redact::register("mcp-render-secret-0f9e2d");
        let half = crate::tools::MAX_OUTPUT / 2;
        // The cut falls at `half`, five bytes into the secret.
        let text = format!(
            "{}mcp-render-secret-0f9e2d{}",
            "a".repeat(half - 5),
            "b".repeat(half * 2)
        );
        let result = CallToolResult::success(vec![ContentBlock::text(text)]);
        let (out, ok) = render(&result);
        let around = &out[half - 20..half + 40];
        assert!(ok && out.contains("bytes trimmed"), "{around}");
        assert!(out.contains("a[REDA"), "{around}");
        assert!(
            !out.contains("mcp-r") && !out.contains("0f9e2d"),
            "{around}"
        );
    }

    #[test]
    fn redirects_stay_on_the_origin_and_off_plain_http() {
        let url = |s: &str| reqwest::Url::parse(s).unwrap();
        let check = |from: &str, to: &str| redirect_allowed(&url(to), &[url(from)]);
        assert_eq!(
            check("https://a.example/mcp", "https://a.example/mcp/"),
            Ok(())
        );
        assert_eq!(
            check("http://127.0.0.1:9/mcp", "http://127.0.0.1:9/x"),
            Ok(())
        );
        assert_eq!(
            check("http://localhost:9/mcp", "http://localhost:9/x"),
            Ok(())
        );
        assert_eq!(check("http://[::1]:9/mcp", "http://[::1]:9/x"), Ok(()));

        let other = check("https://a.example/mcp", "https://b.example/mcp?token=t").unwrap_err();
        assert!(
            other.contains("https://b.example") && !other.contains("token"),
            "{other}"
        );
        assert!(check("https://a.example/mcp", "https://a.example:8443/mcp").is_err());
        assert!(check("https://a.example/mcp", "http://a.example/mcp").is_err());
        assert!(check("http://127.0.0.1:9/mcp", "http://127.0.0.1:10/mcp").is_err());
        // Same origin, but the headers would cross the network in the clear.
        let plain = check("http://a.example/mcp", "http://a.example/x").unwrap_err();
        assert!(plain.contains("plain HTTP"), "{plain}");

        let chain = vec![url("https://a.example/0"); MAX_REDIRECTS];
        assert_eq!(
            redirect_allowed(&url("https://a.example/1"), &chain),
            Ok(())
        );
        let chain = vec![url("https://a.example/0"); MAX_REDIRECTS + 1];
        assert!(redirect_allowed(&url("https://a.example/1"), &chain).is_err());
    }

    /// Answers one request per connection: `/start` redirects to `location`, anything
    /// else is a 200. Each request's head goes to the channel.
    async fn redirecting_server(
        location: Option<String>,
    ) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let own = format!("{base}/end");
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let first = head.lines().next().unwrap_or_default();
                let response = if first.contains(" /start ") {
                    let to = location.clone().unwrap_or(own.clone());
                    format!(
                        "HTTP/1.1 307 Temporary Redirect\r\nlocation: {to}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    )
                } else {
                    "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok"
                        .to_string()
                };
                let _ = tx.send(head);
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (base, rx)
    }

    #[tokio::test]
    async fn a_token_header_does_not_follow_a_redirect_to_another_origin() {
        let (elsewhere, mut seen_elsewhere) = redirecting_server(None).await;
        let (base, mut seen) = redirecting_server(Some(format!("{elsewhere}/steal"))).await;
        let url = format!("{base}/start");
        let server = Server {
            name: "moved".to_string(),
            source: "test".to_string(),
            command: String::new(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            url: Some(url.clone()),
            headers: crate::config::Headers(std::collections::BTreeMap::from([(
                "X-API-Key".to_string(),
                "s3cret".to_string(),
            )])),
            skip: None,
            startup_timeout: None,
            tool_timeout: None,
        };
        let Err(why) = http(&server, &url).await else {
            panic!("a cross-origin redirect was followed");
        };
        let why = format!("{why:#}");
        assert!(!why.contains("s3cret"), "{why}");
        let first = seen.recv().await.unwrap();
        assert!(first.starts_with("POST /start "), "{first}");
        assert!(
            seen_elsewhere.try_recv().is_err(),
            "the other origin was contacted"
        );
    }

    #[tokio::test]
    async fn a_server_that_redirects_within_its_origin_is_reached() {
        let Some((server, mut child)) = fake_http_server("moved") else {
            return;
        };
        let url = server.url.as_deref().unwrap().replace("/mcp", "/old");
        let server = Server {
            url: Some(url),
            ..server
        };
        let dir = temp_dir();
        let hub = Hub::connect(
            vec![server],
            &Identity::default(),
            &dir,
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(hub.servers[0].state, State::Connected, "{:?}", hub.servers);
        let args = serde_json::json!({"message": "hi"});
        let (out, ok) = hub.call("mcp__moved__echo", args).await;
        assert_eq!((out.as_str(), ok), ("echo: hi", true));
        hub.shutdown().await;
        child.kill().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_call_round_trips_through_a_stdio_server() {
        if !python() {
            return;
        }
        let dir = temp_dir();
        let hub = Hub::connect(
            vec![fake("fake", "")],
            &Identity::default(),
            &dir,
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(hub.servers[0].state, State::Connected, "{:?}", hub.servers);
        let names: Vec<_> = hub.tools().map(ToolInfo::full_name).collect();
        assert_eq!(names, ["mcp__fake__echo", "mcp__fake__fail"]);
        assert_eq!(
            hub.find("mcp__fake__echo").unwrap().schema["properties"]["message"]["type"],
            "string"
        );

        let args = serde_json::json!({"message": "hi"});
        let (out, ok) = hub.call("mcp__fake__echo", args).await;
        assert_eq!((out.as_str(), ok), ("echo: hi", true));
        let (out, ok) = hub.call("mcp__fake__fail", serde_json::json!({})).await;
        assert_eq!((out.as_str(), ok), ("it failed", false));
        let (out, ok) = hub.call("mcp__fake__nope", serde_json::json!({})).await;
        assert!(!ok && out.contains("mcp_search"), "{out}");

        let log = std::fs::read_to_string(dir.join("mcp-fake.log")).unwrap();
        assert!(log.contains("fake_mcp started"), "{log}");
        hub.shutdown().await;
        let (out, ok) = hub.call("mcp__fake__echo", serde_json::json!({})).await;
        assert!(!ok, "{out}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn shutdown_kills_what_a_stdio_server_left_in_its_group() {
        if !python() {
            return;
        }
        let alive = |pid: &str| {
            std::process::Command::new("kill")
                .args(["-0", pid])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        let dir = temp_dir();
        let hub = Hub::connect(
            vec![fake("fake", "grandchild")],
            &Identity::default(),
            &dir,
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(hub.servers[0].state, State::Connected, "{:?}", hub.servers);
        let log = std::fs::read_to_string(dir.join("mcp-fake.log")).unwrap();
        let pid = log
            .lines()
            .find_map(|l| l.strip_prefix("grandchild "))
            .unwrap_or_else(|| panic!("{log}"))
            .to_string();
        assert!(alive(&pid));

        hub.shutdown().await;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while alive(&pid) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!alive(&pid), "grandchild {pid} outlived shutdown");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_call_a_server_never_answers_fails_with_the_server_named() {
        if !python() {
            return;
        }
        let dir = temp_dir();
        let stuck = Server {
            tool_timeout: Some(Duration::from_millis(200)),
            ..fake("stuck", "hangcall")
        };
        let hub = Hub::connect(
            vec![stuck],
            &Identity::default(),
            &dir,
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(hub.servers[0].state, State::Connected, "{:?}", hub.servers);
        let started = std::time::Instant::now();
        let (out, ok) = hub
            .call("mcp__stuck__echo", serde_json::json!({"message": "hi"}))
            .await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(out, "MCP server `stuck` did not answer in 0.2s.");
        assert!(!ok);
        hub.shutdown().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_server_with_its_own_startup_timeout_gets_that_one() {
        if !python() {
            return;
        }
        let dir = temp_dir();
        let hangs = Server {
            startup_timeout: Some(Duration::from_millis(300)),
            ..fake("hangs", "hang")
        };
        let started = std::time::Instant::now();
        let hub = Hub::connect(
            vec![hangs],
            &Identity::default(),
            &dir,
            Duration::from_secs(30),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(
            hub.servers[0].state,
            State::Failed("timed out after 0.3s".to_string())
        );
        hub.shutdown().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_call_round_trips_through_a_streamable_http_server() {
        let Some((server, mut child)) = fake_http_server("web") else {
            return;
        };
        let dir = temp_dir();
        let wrong = Server {
            name: "wrong".to_string(),
            headers: crate::config::Headers(std::collections::BTreeMap::from([(
                "Authorization".to_string(),
                "Bearer nope".to_string(),
            )])),
            ..server.clone()
        };
        let hub = Hub::connect(
            vec![server, wrong],
            &Identity::default(),
            &dir,
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(hub.servers[0].state, State::Connected, "{:?}", hub.servers);
        let names: Vec<_> = hub.tools().map(ToolInfo::full_name).collect();
        assert_eq!(names, ["mcp__web__echo", "mcp__web__fail"]);
        let args = serde_json::json!({"message": "hi"});
        let (out, ok) = hub.call("mcp__web__echo", args).await;
        assert_eq!((out.as_str(), ok), ("echo: hi", true));
        assert_eq!(crate::redact::apply("Bearer s3cret"), "[REDACTED]");

        // A rejected header must not be named in the failure.
        let State::Failed(why) = &hub.servers[1].state else {
            panic!("{:?}", hub.servers[1]);
        };
        assert!(!why.contains("nope"), "{why}");
        hub.shutdown().await;
        child.kill().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn failed_servers_are_marked_and_skipped() {
        if !python() {
            return;
        }
        let dir = temp_dir();
        let mut skipped = fake("skipped", "");
        skipped.skip = Some("legacy sse transport not supported".to_string());
        let missing = Server {
            command: "/nonexistent/bhai-mcp".to_string(),
            ..fake("missing", "")
        };
        let identity = Identity {
            mcp: vec![
                "*".to_string(),
                "!hidden".to_string(),
                "!good__fail".to_string(),
            ],
            ..Identity::default()
        };
        let hub = Hub::connect(
            vec![
                fake("good", ""),
                fake("exits", "exit"),
                fake("hangs", "hang"),
                missing,
                skipped,
                fake("hidden", ""),
            ],
            &identity,
            &dir,
            Duration::from_secs(2),
        )
        .await;
        let state = |name: &str| {
            let server = hub.servers.iter().find(|s| s.name == name).unwrap();
            match &server.state {
                State::Connected => "connected".to_string(),
                State::Failed(why) => format!("failed: {why}"),
                State::Skipped(why) => format!("skipped: {why}"),
            }
        };
        assert_eq!(state("good"), "connected");
        assert!(state("exits").starts_with("failed"), "{}", state("exits"));
        assert_eq!(state("hangs"), "failed: timed out after 2s");
        assert!(
            state("missing").contains("starting"),
            "{}",
            state("missing")
        );
        assert!(state("skipped").contains("legacy sse"));
        assert!(state("hidden").contains("identity general"));
        let names: Vec<_> = hub.tools().map(ToolInfo::full_name).collect();
        assert_eq!(names, ["mcp__good__echo"]);
        assert_eq!(hub.notices().len(), 3);
        let report = hub.report();
        assert!(
            report.starts_with("mcp: 1 of 6 servers connected, 1 tools"),
            "{report}"
        );
        assert!(
            report.contains("hangs  (test)  failed     timed out"),
            "{report}"
        );
        hub.shutdown().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_child_starts_a_server_its_parent_did_not_on_first_use() {
        if !python() {
            return;
        }
        let dir = temp_dir();
        let parent = Identity {
            mcp: vec!["!*".to_string()],
            ..Identity::default()
        };
        let hub = Hub::connect(
            vec![fake("fake", "")],
            &parent,
            &dir,
            Duration::from_secs(10),
        )
        .await;
        assert!(!hub.has_tools());
        assert_eq!(hub.prompt_section(), "");
        let child = hub.narrowed(&Identity {
            mcp: vec!["fake__echo".to_string()],
            ..Identity::default()
        });
        let section = child.prompt_section();
        assert!(
            section.ends_with("\nmcp server fake: starts on first use"),
            "{section}"
        );
        assert!(child.has_tools() && child.may_call("mcp__fake__echo"));
        assert!(!child.may_call("mcp__fake__fail") && !child.may_call("mcp__other__x"));
        assert!(!dir.join("mcp-fake.log").exists());

        let out = child.search("fake").await;
        assert!(out.starts_with("mcp__fake__echo: "), "{out}");
        assert!(!out.contains("mcp__fake__fail"), "{out}");
        let (out, ok) = child
            .call("mcp__fake__echo", serde_json::json!({"message": "hi"}))
            .await;
        assert_eq!((out.as_str(), ok), ("echo: hi", true));
        let (_, ok) = child.call("mcp__fake__fail", serde_json::json!({})).await;
        assert!(!ok);

        // A later child reuses the running server, and the child cannot close it.
        child.shutdown().await;
        let other = hub.narrowed(&Identity::default());
        let (out, ok) = other.call("mcp__fake__fail", serde_json::json!({})).await;
        assert_eq!((out.as_str(), ok), ("it failed", false));
        assert_eq!(hub.shared.late.lock().await.len(), 1);
        assert_eq!(child.prompt_section(), section);
        assert!(
            hub.report().contains("connected  2 tools, for a child"),
            "{}",
            hub.report()
        );

        hub.shutdown().await;
        let (out, ok) = other.call("mcp__fake__echo", serde_json::json!({})).await;
        assert!(!ok, "{out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn search_ranks_names_over_descriptions() {
        let tools = [
            ToolInfo::test("github", "create_issue", "Open an issue"),
            ToolInfo::test("github", "search_code", "Search code"),
            ToolInfo::test("github", "list_prs", "List pull requests about an issue"),
            ToolInfo::test("fs", "issue", "not what it seems"),
        ];
        let order = |query: &str| -> Vec<String> {
            search(&tools, query)
                .lines()
                .filter(|l| l.starts_with("mcp__"))
                .map(|l| l.split(':').next().unwrap().to_string())
                .collect()
        };
        assert_eq!(
            order("issue"),
            [
                "mcp__fs__issue",
                "mcp__github__create_issue",
                "mcp__github__list_prs"
            ]
        );
        assert_eq!(order("github").len(), 3);
        assert_eq!(
            order("mcp__github__search_code"),
            ["mcp__github__search_code"]
        );
        let out = search(&tools, "create issue");
        assert!(out.starts_with("mcp__github__create_issue: Open an issue\ninput schema: {"));
        assert!(search(&tools, "zzz").contains("Connected servers: github, fs"));

        let many: Vec<_> = (0..7)
            .map(|i| ToolInfo::test("x", &format!("t{i}"), ""))
            .collect();
        let out = search(&many, "x");
        assert_eq!(out.matches("input schema").count(), SEARCH_RESULTS);
        assert!(out.ends_with("2 more matches; narrow the query."), "{out}");
    }

    #[test]
    fn prompt_lines_name_a_few_tools_per_server() {
        let hub = Hub::offline(vec![
            (
                "big",
                (0..5)
                    .map(|i| ToolInfo::test("big", &format!("t{i}"), ""))
                    .collect(),
            ),
            ("empty", Vec::new()),
            ("one", vec![ToolInfo::test("one", "only", "")]),
        ]);
        let section = hub.prompt_section();
        assert!(section.contains("`mcp_search`"));
        assert!(section.ends_with(
            "\nmcp server big: 5 tools (t0, t1, t2, ...)\nmcp server one: 1 tools (only)"
        ));
        assert!(!section.contains("empty"));
        assert_eq!(Hub::offline(Vec::new()).prompt_section(), "");
        assert!(report(None).contains("off"));
    }

    /// A server names itself and its tools, and the listing is one line per server in the
    /// cached prefix. What it can write there is bounded.
    #[test]
    fn a_server_cannot_write_its_own_lines_into_the_listing() {
        let long = "t".repeat(200);
        let hub = Hub::offline(vec![(
            "loud",
            vec![
                ToolInfo::test("loud", "wipe\nmcp server root: 9 tools", ""),
                ToolInfo::test("loud", &long, ""),
            ],
        )]);
        let section = hub.prompt_section();
        assert_eq!(
            section
                .lines()
                .filter(|l| l.starts_with("mcp server"))
                .count(),
            1
        );
        assert!(
            section.contains("wipemcp server root: 9 tools"),
            "{section}"
        );
        assert!(
            section.contains(&format!("{}...", "t".repeat(PROMPT_NAME))),
            "{section}"
        );
    }

    #[test]
    fn a_narrowed_hub_keeps_only_what_the_identity_allows() {
        let hub = Hub::offline(vec![
            (
                "web",
                vec![
                    ToolInfo::test("web", "search", ""),
                    ToolInfo::test("web", "fetch", ""),
                ],
            ),
            ("xcode", vec![ToolInfo::test("xcode", "build", "")]),
        ]);
        let identity = Identity {
            mcp: vec!["web__search".to_string()],
            ..Identity::default()
        };
        let narrowed = hub.narrowed(&identity);
        let names: Vec<_> = narrowed.tools().map(ToolInfo::full_name).collect();
        assert_eq!(names, ["mcp__web__search"]);
        assert_eq!(hub.tools().count(), 3);
    }
}
