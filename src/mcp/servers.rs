//! Where MCP servers are configured: `~/.claude.json` (top level, then the project's
//! entry), the repo's `.mcp.json`, then bhai's global config. A later definition
//! replaces an earlier one of the same name. A repo's `.mcp.json` servers only start
//! once approved in `~/.claude.json`, as Claude Code asks for. `${VAR}` in HTTP header
//! values is expanded from the environment.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use crate::config::{Headers, McpServer};
use crate::instructions::{self, Roots};

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

/// Every configured server, merged by name in source order.
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
        for mut server in parse(&mcp_json, ".mcp.json") {
            if server.skip.is_none() {
                server.skip = unapproved(project, &server.name);
            }
            // An unapproved repo entry must not shadow the user's own server.
            if server.skip.is_none() || !found.iter().any(|s| s.name == server.name) {
                add(&mut found, server);
            }
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

/// A name with `__` would make `mcp__server__tool` ambiguous, so it never starts.
fn add(found: &mut Vec<Server>, mut server: Server) {
    if server.name.contains("__") {
        server.skip = Some("invalid name (contains __)".to_string());
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
                "a", "b", "new", "nourl", "off", "ok", "p__q", "sse", "unset", "untyped", "web",
                "x__y"
            ]
        );
        let invalid = Some("invalid name (contains __)".to_string());
        assert_eq!(get("p__q").skip, invalid);
        assert_eq!(get("x__y").skip, invalid);
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
