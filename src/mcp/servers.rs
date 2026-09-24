//! Where MCP servers are configured: `~/.claude.json` (top level, then the project's
//! entry), the repo's `.mcp.json`, then bhai's global config. A later definition
//! replaces an earlier one of the same name. A repo's `.mcp.json` servers only start
//! once approved in `~/.claude.json`, as Claude Code asks for. `${VAR}` in HTTP header
//! values is expanded from the environment.
//!
//! The approval in `~/.claude.json` is by name only, so a repo that keeps the name and
//! changes the command starts under an approval given to a different program. bhai keeps a
//! fingerprint of each approved `.mcp.json` server beside its own config and skips one that
//! has changed since; `bhai mcp approve <name>` accepts it as it is now.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::{Headers, McpServer};
use crate::instructions::{self, Roots};

/// Where the fingerprints of approved `.mcp.json` servers are kept, under bhai's own
/// config directory: the approval itself is Claude Code's, but what was approved is bhai's
/// to remember.
const APPROVALS: &str = ".config/bhai/mcp-approvals.json";

#[derive(Debug, Clone, PartialEq)]
pub struct Server {
    pub name: String,
    /// Where it was defined, as shown to the user.
    pub source: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// The streamable HTTP endpoint; `None` for a stdio server.
    pub url: Option<String>,
    /// Sent with every HTTP request, already expanded.
    pub headers: Headers,
    /// Why it is not started, if it is not.
    pub skip: Option<String>,
}

/// Every configured server, merged by name in source order. A `.mcp.json` server whose
/// definition has changed since it was approved is skipped rather than started.
pub fn load(roots: &Roots, bhai: &BTreeMap<String, McpServer>) -> Vec<Server> {
    let mut found: Vec<Server> = Vec::new();

    let claude = roots
        .home
        .as_ref()
        .and_then(|home| read(&home.join(".claude.json")));
    let project_root = instructions::project_root(&roots.cwd);
    let project = claude.as_ref().and_then(|c| {
        [roots.cwd.as_path(), project_root]
            .iter()
            .find_map(|dir| c.get("projects")?.get(dir.to_str()?))
    });
    if let Some(claude) = &claude {
        for server in parse(claude, "~/.claude.json") {
            add(&mut found, server);
        }
    }
    if let Some(project) = project {
        for server in parse(project, "~/.claude.json (project)") {
            add(&mut found, server);
        }
    }
    if let Some(mcp_json) = read(&project_root.join(".mcp.json")) {
        let store = approvals_path(roots);
        let mut recorded = store.as_deref().map(read_approvals).unwrap_or_default();
        let key = project_root.display().to_string();
        let mut added = false;
        for mut server in parse(&mcp_json, ".mcp.json") {
            if server.skip.is_none() {
                server.skip = unapproved(project, &server.name);
            }
            // Approved by name, and nothing else against it: the definition it was approved
            // as is what it has to keep. First sight is the baseline, which is all the
            // approval ever had.
            if server.skip.is_none() {
                let now = fingerprint(&server);
                let entry = recorded.entry(key.clone()).or_default();
                match entry.get(&server.name) {
                    Some(approved) if *approved != now => {
                        server.skip = Some(format!(
                            "changed since it was approved; `bhai mcp approve {}` accepts it as it is now",
                            server.name
                        ));
                    }
                    Some(_) => {}
                    None => {
                        entry.insert(server.name.clone(), now);
                        added = true;
                    }
                }
            }
            // An unapproved repo entry must not shadow the user's own server.
            if server.skip.is_none() || !found.iter().any(|s| s.name == server.name) {
                add(&mut found, server);
            }
        }
        if added
            && let Some(store) = &store
            && let Err(e) = write_approvals(store, &recorded)
        {
            eprintln!("bhai: {e:#}");
        }
    }
    for (name, server) in bhai {
        let (headers, unset) = expand_headers(&server.headers.0);
        let skip = if server.url.is_none() && server.command.is_empty() {
            Some("no command".to_string())
        } else {
            unset
        };
        add(
            &mut found,
            Server {
                name: name.clone(),
                source: "~/.config/bhai/config.toml".to_string(),
                command: server.command.clone(),
                args: server.args.clone(),
                env: server.env.clone(),
                url: server.url.clone(),
                headers,
                skip,
            },
        );
    }
    found
}

/// The store of what each approved `.mcp.json` server looked like, by project root.
type Approvals = BTreeMap<String, BTreeMap<String, String>>;

fn approvals_path(roots: &Roots) -> Option<PathBuf> {
    roots.home.as_ref().map(|home| home.join(APPROVALS))
}

/// The store, or an empty one when there is none or it will not read: a store that cannot
/// be read records every server afresh rather than refusing them all.
fn read_approvals(path: &Path) -> Approvals {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_approvals(path: &Path, approvals: &Approvals) -> Result<()> {
    let mut text = serde_json::to_string_pretty(approvals)?;
    text.push('\n');
    crate::permissions::settings::write_atomic(path, text.as_bytes())
        .with_context(|| format!("could not record MCP approvals in {}", path.display()))
}

/// What a server is approved as: the program it runs and where it connects. Header values
/// are left out, since they hold `${VAR}` expansions and tokens that rotate; what decides
/// which code runs is the command, its arguments, the environment and the url.
fn fingerprint(server: &Server) -> String {
    let mut hasher = Sha256::new();
    let mut part = |bytes: &[u8]| {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    part(server.command.as_bytes());
    for arg in &server.args {
        part(arg.as_bytes());
    }
    for (key, value) in &server.env {
        part(key.as_bytes());
        part(value.as_bytes());
    }
    part(server.url.as_deref().unwrap_or_default().as_bytes());
    for key in server.headers.0.keys() {
        part(key.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Record `name` as it is defined now, so the next run starts it. Returns what was
/// recorded, for the line that says what was accepted.
pub fn approve(roots: &Roots, bhai: &BTreeMap<String, McpServer>, name: &str) -> Result<Server> {
    let project_root = instructions::project_root(&roots.cwd);
    let store = approvals_path(roots).context("no home directory to record the approval in")?;
    let server = load(roots, bhai)
        .into_iter()
        .find(|s| s.name == name)
        .with_context(|| format!("no MCP server `{name}` is configured"))?;
    if server.source != ".mcp.json" {
        anyhow::bail!(
            "`{name}` comes from {}, which is your own file, so there is nothing to approve",
            server.source
        );
    }
    let mut approvals = read_approvals(&store);
    approvals
        .entry(project_root.display().to_string())
        .or_default()
        .insert(name.to_string(), fingerprint(&server));
    write_approvals(&store, &approvals)?;
    Ok(server)
}

/// A name with `__` would make `mcp__server__tool` ambiguous, so it never starts.
fn add(found: &mut Vec<Server>, mut server: Server) {
    // `mcp__server__tool` is split on the first `__` after the prefix, so a name holding
    // one, or ending in the `_` the separator would complete, names the wrong tool.
    if server.name.contains("__") {
        server.skip = Some("invalid name (contains __)".to_string());
    } else if server.name.ends_with('_') {
        server.skip = Some("invalid name (ends with _)".to_string());
    }
    match found.iter_mut().find(|s| s.name == server.name) {
        Some(slot) => *slot = server,
        None => found.push(server),
    }
}

/// Why a `.mcp.json` server may not start, judged by the project's `~/.claude.json` entry.
fn unapproved(project: Option<&Value>, name: &str) -> Option<String> {
    let listed = |key: &str| {
        project
            .and_then(|p| p.get(key))
            .and_then(Value::as_array)
            .is_some_and(|list| list.iter().any(|n| n.as_str() == Some(name)))
    };
    let all = project
        .and_then(|p| p.get("enableAllProjectMcpServers"))
        .and_then(Value::as_bool)
        == Some(true);
    if listed("disabledMcpjsonServers") {
        Some("disabled for this project in ~/.claude.json".to_string())
    } else if all || listed("enabledMcpjsonServers") {
        None
    } else {
        Some("repo server not approved; approve it in Claude Code first".to_string())
    }
}

/// The `mcpServers` object of a JSON config.
fn parse(config: &Value, source: &str) -> Vec<Server> {
    let Some(servers) = config.get("mcpServers").and_then(Value::as_object) else {
        return Vec::new();
    };
    servers
        .iter()
        .map(|(name, entry)| server(name, entry, source))
        .collect()
}

fn server(name: &str, entry: &Value, source: &str) -> Server {
    let text = |key: &str| entry.get(key).and_then(Value::as_str);
    let kind = text("type").unwrap_or(if entry.get("url").is_some() {
        "http"
    } else {
        "stdio"
    });
    let command = text("command").unwrap_or_default().to_string();
    let url = text("url").map(str::to_string);
    let raw: BTreeMap<String, String> = entry
        .get("headers")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
        .collect();
    let (headers, unset) = expand_headers(&raw);
    let skip = match kind {
        "stdio" if command.is_empty() => Some("no command".to_string()),
        "stdio" => None,
        "http" if url.is_none() => Some("no url".to_string()),
        "http" => unset,
        "sse" => Some("legacy sse transport not supported".to_string()),
        _ => Some(format!("{kind} servers are not supported")),
    };
    let strings = |key: &str| -> Vec<String> {
        entry
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    };
    let env = entry
        .get("env")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
        .collect();
    Server {
        name: name.to_string(),
        source: source.to_string(),
        command,
        args: strings("args"),
        env,
        url,
        headers,
        skip,
    }
}

/// Header values with `${VAR}` expanded, and why the server cannot start if a var is unset.
fn expand_headers(raw: &BTreeMap<String, String>) -> (Headers, Option<String>) {
    let mut unset = None;
    let mut headers = BTreeMap::new();
    for (name, value) in raw {
        match expand(value, &|var| std::env::var(var).ok()) {
            Ok(value) => {
                headers.insert(name.clone(), value);
            }
            Err(var) => {
                unset.get_or_insert(format!("header {name} needs unset env var {var}"));
            }
        }
    }
    (Headers(headers), unset)
}

/// `value` with each `${VAR}` or `${VAR:-default}` replaced, or the first unset `VAR`.
fn expand(value: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        let Some(len) = rest[start..].find('}') else {
            break;
        };
        out.push_str(&rest[..start]);
        let inner = &rest[start + 2..start + len];
        let (var, default) = match inner.split_once(":-") {
            Some((var, default)) => (var, Some(default)),
            None => (inner, None),
        };
        match lookup(var).or(default.map(str::to_string)) {
            Some(found) => out.push_str(&found),
            None => return Err(var.to_string()),
        }
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn read(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(path: &Path, value: Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, value.to_string()).unwrap();
    }

    #[test]
    fn sources_merge_by_name_and_repo_servers_need_approval() {
        let dir = std::env::temp_dir().join(format!("bhai-mcp-{}", uuid::Uuid::new_v4()));
        let (home, cwd) = (dir.join("home"), dir.join("repo"));
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        let project_key = cwd.display().to_string();
        write(
            &home.join(".claude.json"),
            json!({
                "mcpServers": {
                    "a": {"type": "stdio", "command": "a-global", "args": ["x"], "env": {"K": "v"}},
                    "b": {"command": "b-global"},
                    "web": {"type": "http", "url": "https://x", "headers": {
                        "Authorization": "Bearer ${BHAI_TEST_UNSET_VAR:-s3cret}",
                    }},
                    "untyped": {"url": "https://y"},
                    "sse": {"type": "sse", "url": "https://z"},
                    "nourl": {"type": "http"},
                    "unset": {"type": "http", "url": "https://w", "headers": {
                        "X-Key": "${BHAI_TEST_UNSET_VAR}",
                    }},
                },
                "projects": {
                    project_key: {
                        "mcpServers": {"b": {"command": "b-project"}},
                        "enabledMcpjsonServers": ["ok"],
                        "disabledMcpjsonServers": ["off"],
                    }
                }
            }),
        );
        write(
            &cwd.join(".mcp.json"),
            json!({"mcpServers": {
                "ok": {"command": "ok"},
                "off": {"command": "off"},
                "new": {"command": "new"},
                "a": {"command": "a-repo"},
                "x__y": {"command": "xy"},
            }}),
        );
        let bhai_server = |command: &str| McpServer {
            command: command.to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            url: None,
            headers: Headers::default(),
        };
        let bhai = BTreeMap::from([
            ("b".to_string(), bhai_server("b-bhai")),
            ("p__q".to_string(), bhai_server("pq")),
            ("trailing_".to_string(), bhai_server("trailing")),
        ]);
        let roots = Roots {
            home: Some(home),
            codex_home: None,
            cwd,
        };
        let servers = load(&roots, &bhai);
        let get = |name: &str| servers.iter().find(|s| s.name == name).unwrap();
        let mut names: Vec<_> = servers.iter().map(|s| s.name.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "a",
                "b",
                "new",
                "nourl",
                "off",
                "ok",
                "p__q",
                "sse",
                "trailing_",
                "unset",
                "untyped",
                "web",
                "x__y"
            ]
        );
        let invalid = Some("invalid name (contains __)".to_string());
        assert_eq!(get("p__q").skip, invalid);
        assert_eq!(get("x__y").skip, invalid);
        // `mcp__trailing___tool` would split as server `trailing`, tool `_tool`.
        assert_eq!(
            get("trailing_").skip,
            Some("invalid name (ends with _)".to_string())
        );
        assert_eq!(get("b").command, "b-bhai");
        assert_eq!(get("b").source, "~/.config/bhai/config.toml");
        assert!(get("ok").skip.is_none());
        assert!(get("off").skip.as_ref().unwrap().contains("disabled"));
        assert!(get("new").skip.as_ref().unwrap().contains("not approved"));
        assert_eq!(get("a").command, "a-global");
        assert!(get("a").skip.is_none());
        assert_eq!(get("web").skip, None);
        assert_eq!(get("web").url.as_deref(), Some("https://x"));
        assert_eq!(get("web").headers.0["Authorization"], "Bearer s3cret");
        assert!(!format!("{:?}", get("web")).contains("s3cret"));
        assert_eq!(get("untyped").skip, None);
        assert_eq!(get("untyped").url.as_deref(), Some("https://y"));
        let skip = |name: &str| get(name).skip.clone().unwrap();
        assert_eq!(skip("sse"), "legacy sse transport not supported");
        assert_eq!(skip("nourl"), "no url");
        assert_eq!(
            skip("unset"),
            "header X-Key needs unset env var BHAI_TEST_UNSET_VAR"
        );
        assert!(get("a").url.is_none());

        assert_eq!(get("a").args, ["x"]);
        assert_eq!(get("a").env["K"], "v");
        let servers = load(&roots, &BTreeMap::new());
        let b = servers.iter().find(|s| s.name == "b").unwrap();
        assert_eq!(b.command, "b-project");

        // The approval is by name, so what `ok` was approved as is recorded on first sight
        // and a repo that changes the command behind the name has to be approved again.
        let store = roots.home.as_ref().unwrap().join(APPROVALS);
        let key = roots.cwd.display().to_string();
        assert!(read_approvals(&store)[&key].contains_key("ok"));
        write(
            &roots.cwd.join(".mcp.json"),
            json!({"mcpServers": {"ok": {"command": "ok", "args": ["--now-with-this"]}}}),
        );
        let changed = load(&roots, &BTreeMap::new());
        let ok = changed.iter().find(|s| s.name == "ok").unwrap();
        assert!(
            ok.skip
                .as_ref()
                .unwrap()
                .starts_with("changed since it was approved"),
            "{:?}",
            ok.skip
        );
        let approved = approve(&roots, &BTreeMap::new(), "ok").unwrap();
        assert_eq!(approved.args, ["--now-with-this"]);
        let after = load(&roots, &BTreeMap::new());
        assert_eq!(after.iter().find(|s| s.name == "ok").unwrap().skip, None);
        // A server from the user's own files is not the repo's to change.
        assert!(approve(&roots, &BTreeMap::new(), "web").is_err());
        assert!(approve(&roots, &BTreeMap::new(), "nothing").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn header_values_expand_env_vars() {
        let lookup = |var: &str| (var == "TOKEN").then(|| "abc".to_string());
        assert_eq!(expand("Bearer ${TOKEN}", &lookup).unwrap(), "Bearer abc");
        assert_eq!(expand("${TOKEN}-${NOPE:-x}", &lookup).unwrap(), "abc-x");
        assert_eq!(expand("${TOKEN:-x}", &lookup).unwrap(), "abc");
        assert_eq!(expand("plain ${open", &lookup).unwrap(), "plain ${open");
        assert_eq!(expand("a ${NOPE} b", &lookup).unwrap_err(), "NOPE");
    }
}
