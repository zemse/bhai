//! MCP client: stdio servers started once at launch. Their tool schemas stay out of the
//! tool list; the model finds them with `mcp_search` and runs them with `mcp_call`, so
//! the tool list and prompt prefix stay fixed however many servers there are.

use std::fmt;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::{Peer, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;

use crate::config::Config;
use crate::identity::Identity;
use crate::instructions::Roots;

pub mod servers;

use servers::Server;

/// How long a server gets to start, initialize and list its tools.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// Tool names the prompt line shows per server.
const PROMPT_TOOLS: usize = 3;
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

/// The servers of a session: their status, tools, and the connections to call them.
pub struct Hub {
    pub servers: Vec<Status>,
    peers: Vec<(String, Peer<RoleClient>)>,
    /// Taken on shutdown.
    services: Mutex<Vec<Service>>,
}

impl fmt::Debug for Hub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hub")
            .field("servers", &self.servers)
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
        let starts = servers.into_iter().map(|server| async move {
            let skip = server.skip.clone().or_else(|| {
                (!identity.allows_mcp_server(&server.name))
                    .then(|| format!("not allowed by identity {}", identity.name))
            });
            let mut status = Status {
                name: server.name.clone(),
                source: server.source.clone(),
                state: State::Connected,
                tools: Vec::new(),
            };
            if let Some(reason) = skip {
                status.state = State::Skipped(reason);
                return (status, None);
            }
            let started = tokio::time::timeout(timeout, spawn(&server, log_dir)).await;
            match started {
                Ok(Ok((service, tools))) => {
                    status.tools = tools
                        .into_iter()
                        .filter(|t| identity.allows_mcp_tool(&server.name, &t.name))
                        .collect();
                    (status, Some(service))
                }
                Ok(Err(e)) => {
                    status.state = State::Failed(format!("{e:#}"));
                    (status, None)
                }
                Err(_) => {
                    status.state = State::Failed(format!("timed out after {}s", timeout.as_secs()));
                    (status, None)
                }
            }
        });
        let mut hub = Hub {
            servers: Vec::new(),
            peers: Vec::new(),
            services: Mutex::new(Vec::new()),
        };
        let services = hub.services.get_mut().unwrap_or_else(|e| e.into_inner());
        for (status, service) in futures_util::future::join_all(starts).await {
            if let Some(service) = service {
                hub.peers
                    .push((status.name.clone(), service.peer().clone()));
                services.push(service);
            }
            hub.servers.push(status);
        }
        hub
    }

    /// This hub as `identity` sees it: the same connections, only its servers and tools.
    /// Closing the servers stays with this hub.
    pub fn narrowed(&self, identity: &Identity) -> Hub {
        let servers = self
            .servers
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
        Hub {
            servers,
            peers: self.peers.clone(),
            services: Mutex::new(Vec::new()),
        }
    }

    /// A hub of connected servers with these tools and no processes behind them.
    #[cfg(test)]
    pub fn offline(servers: Vec<(&str, Vec<ToolInfo>)>) -> Self {
        Hub {
            servers: servers
                .into_iter()
                .map(|(name, tools)| Status {
                    name: name.to_string(),
                    source: "test".to_string(),
                    state: State::Connected,
                    tools,
                })
                .collect(),
            peers: Vec::new(),
            services: Mutex::new(Vec::new()),
        }
    }

    /// Every tool of every connected server.
    pub fn tools(&self) -> impl Iterator<Item = &ToolInfo> {
        self.servers.iter().flat_map(|s| s.tools.iter())
    }

    pub fn has_tools(&self) -> bool {
        self.tools().next().is_some()
    }

    /// The system prompt section: one line per server with tools.
    pub fn prompt_section(&self) -> String {
        if !self.has_tools() {
            return String::new();
        }
        let mut text = String::from(
            "\n\n# MCP\n\nMCP tools are not in your tool list. Find one with `mcp_search`, \
then run it with `mcp_call` using the exact `mcp__server__tool` name.\n",
        );
        for server in self.servers.iter().filter(|s| !s.tools.is_empty()) {
            let mut names: Vec<&str> = server
                .tools
                .iter()
                .take(PROMPT_TOOLS)
                .map(|t| t.name.as_str())
                .collect();
            if server.tools.len() > PROMPT_TOOLS {
                names.push("...");
            }
            let _ = write!(
                text,
                "\nmcp server {}: {} tools ({})",
                server.name,
                server.tools.len(),
                names.join(", ")
            );
        }
        text
    }

    /// The best matches for `query`, with their schemas, as `mcp_search` returns them.
    pub fn search(&self, query: &str) -> String {
        let words: Vec<String> = query
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect();
        let mut hits: Vec<(usize, &ToolInfo)> = self
            .tools()
            .map(|tool| (score(tool, &words), tool))
            .filter(|(score, _)| *score > 0)
            .collect();
        hits.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.full_name().cmp(&b.1.full_name()))
        });
        if hits.is_empty() {
            let servers: Vec<_> = self
                .servers
                .iter()
                .filter(|s| !s.tools.is_empty())
                .map(|s| s.name.as_str())
                .collect();
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

    pub fn find(&self, full_name: &str) -> Option<&ToolInfo> {
        self.tools().find(|t| t.full_name() == full_name)
    }

    /// Run `full_name`; returns the output and whether it succeeded.
    pub async fn call(&self, full_name: &str, arguments: Value) -> (String, bool) {
        let Some(tool) = self.find(full_name) else {
            return (
                format!(
                    "No MCP tool named `{full_name}`. Use `mcp_search` to find the exact name."
                ),
                false,
            );
        };
        let Some((_, peer)) = self.peers.iter().find(|(name, _)| *name == tool.server) else {
            return (
                format!("MCP server `{}` is not connected.", tool.server),
                false,
            );
        };
        let mut params = CallToolRequestParams::new(tool.name.clone());
        if let Value::Object(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        match peer.call_tool(params).await {
            Ok(result) => render(&result),
            Err(e) => (format!("MCP call failed: {e}"), false),
        }
    }

    /// Close every server; each is killed if it does not exit in a few seconds.
    pub async fn shutdown(&self) {
        let services =
            std::mem::take(&mut *self.services.lock().unwrap_or_else(|e| e.into_inner()));
        futures_util::future::join_all(services.into_iter().map(|s| s.cancel())).await;
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
        for server in &self.servers {
            let state = match &server.state {
                State::Connected => format!("connected  {} tools", server.tools.len()),
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

/// What `/mcp` prints for a session's hub.
pub fn report(hub: Option<&Hub>) -> String {
    match hub {
        Some(hub) => hub.report(),
        None => "mcp: off (set `mcp = true` in ~/.config/bhai/config.toml)".to_string(),
    }
}

/// Spawn, initialize and list tools. Stderr goes to `<log_dir>/mcp-<name>.log`.
async fn spawn(server: &Server, log_dir: &Path) -> Result<(Service, Vec<ToolInfo>)> {
    std::fs::create_dir_all(log_dir).with_context(|| format!("creating {}", log_dir.display()))?;
    let log_path = log_dir.join(format!("mcp-{}.log", file_safe(&server.name)));
    let log = std::fs::File::create(&log_path)
        .with_context(|| format!("creating {}", log_path.display()))?;
    let mut command = tokio::process::Command::new(&server.command);
    command
        .args(&server.args)
        .envs(&server.env)
        .kill_on_drop(true);
    let (transport, _) = TokioChildProcess::builder(command)
        .stderr(Stdio::from(log))
        .spawn()
        .with_context(|| format!("starting `{}`", server.command))?;
    let service = ().serve(transport).await.context("initialize failed")?;
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
    Ok((service, tools))
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
    (crate::tools::truncate(&parts.join("\n")), ok)
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
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_mcp.py");

    fn python() -> bool {
        let found = std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !found {
            eprintln!("skipped: python3 is not available");
        }
        found
    }

    fn fake(name: &str, mode: &str) -> Server {
        Server {
            name: name.to_string(),
            source: "test".to_string(),
            command: "python3".to_string(),
            args: vec![FAKE.to_string(), mode.to_string()],
            env: BTreeMap::new(),
            skip: None,
        }
    }

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("bhai-mcp-hub-{}", uuid::Uuid::new_v4()))
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
    async fn failed_servers_are_marked_and_skipped() {
        if !python() {
            return;
        }
        let dir = temp_dir();
        let mut skipped = fake("skipped", "");
        skipped.skip = Some("http servers are not supported yet".to_string());
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
        assert!(state("skipped").contains("http"));
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

    #[test]
    fn search_ranks_names_over_descriptions() {
        let hub = Hub::offline(vec![
            (
                "github",
                vec![
                    ToolInfo::test("github", "create_issue", "Open an issue"),
                    ToolInfo::test("github", "search_code", "Search code"),
                    ToolInfo::test("github", "list_prs", "List pull requests about an issue"),
                ],
            ),
            (
                "fs",
                vec![ToolInfo::test("fs", "issue", "not what it seems")],
            ),
        ]);
        let order = |query: &str| -> Vec<String> {
            hub.search(query)
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
        let out = hub.search("create issue");
        assert!(out.starts_with("mcp__github__create_issue: Open an issue\ninput schema: {"));
        assert!(hub.search("zzz").contains("Connected servers: github, fs"));

        let many = Hub::offline(vec![(
            "x",
            (0..7)
                .map(|i| ToolInfo::test("x", &format!("t{i}"), ""))
                .collect(),
        )]);
        let out = many.search("x");
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
