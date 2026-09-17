//! The tools the model can call, and the registry the agent dispatches them through.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

pub mod agent;
pub mod bash;
pub mod edit;
pub mod mcp;
pub mod read;
pub mod skill;
pub mod write;

/// Every tool name, as identities refer to them.
pub const NAMES: [&str; 6] = [
    bash::NAME,
    read::NAME,
    write::NAME,
    edit::NAME,
    skill::NAME,
    agent::NAME,
];

/// Tool output past this is trimmed in the middle; the tail usually carries the error.
const MAX_OUTPUT: usize = 20_000;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    /// The Responses API function tool definition.
    fn schema(&self) -> Value;
    fn needs_approval(&self) -> bool;
    /// Check the arguments and summarize the call in one line for the user.
    fn describe(&self, args: &Value) -> Result<String, String>;
    /// Run the call; returns the output and whether it counts as a success.
    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)>;
}

pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
}

impl Registry {
    /// Every built-in tool, plus `skill` when there are skills to load.
    pub fn new(skills: Vec<crate::skills::Skill>) -> Self {
        let mut tools: Vec<Box<dyn Tool>> = vec![
            Box::new(bash::Bash),
            Box::new(read::Read),
            Box::new(write::Write),
            Box::new(edit::Edit),
        ];
        if !skills.is_empty() {
            tools.push(Box::new(skill::Skill { skills }));
        }
        Self { tools }
    }

    /// `mcp_search` and `mcp_call`, when the hub has tools to offer.
    pub fn with_mcp(mut self, hub: Option<Arc<crate::mcp::Hub>>) -> Self {
        if let Some(hub) = hub.filter(|h| h.has_tools()) {
            self.tools.push(Box::new(mcp::Search {
                hub: Arc::clone(&hub),
            }));
            self.tools.push(Box::new(mcp::Call { hub }));
        }
        self
    }

    /// The `agent` tool, for a parent session only.
    pub fn with_agent(mut self, agent: agent::Agent) -> Self {
        self.tools.push(Box::new(agent));
        self
    }

    /// The built-in tools and `skill`, narrowed to an identity's tools.
    pub fn for_identity(
        skills: Vec<crate::skills::Skill>,
        identity: &crate::identity::Identity,
    ) -> Self {
        let mut registry = Self::new(skills);
        registry.tools.retain(|t| identity.allows_tool(t.name()));
        registry
    }

    /// The tools a session's prompt allows: its skills, narrowed to its identity's tools,
    /// and the MCP tools, which its identity's `mcp` globs already narrowed.
    pub fn for_prompt(prompt: &crate::prompt::SystemPrompt) -> Self {
        Self::for_identity(prompt.skills.clone(), &prompt.identity).with_mcp(prompt.mcp.clone())
    }

    /// Tool names in registration order.
    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    pub fn schemas(&self) -> Vec<Value> {
        self.tools.iter().map(|t| t.schema()).collect()
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }

    /// The output for a call to a tool that does not exist.
    pub fn unknown(&self, name: &str) -> String {
        let names: Vec<_> = self
            .tools
            .iter()
            .map(|t| format!("`{}`", t.name()))
            .collect();
        format!(
            "Unknown tool `{name}`. Available tools: {}.",
            names.join(", ")
        )
    }
}

/// Parse a `function_call`'s JSON-string arguments into an object.
pub fn parse_arguments(arguments: &str) -> Result<Value, String> {
    let parsed: Value = serde_json::from_str(arguments)
        .map_err(|e| format!("arguments were not valid JSON: {e}. Send a JSON object."))?;
    if parsed.is_object() {
        Ok(parsed)
    } else {
        Err("arguments must be a JSON object.".to_string())
    }
}

fn string_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// The required absolute path argument `path`.
fn path_arg(args: &Value) -> Result<&Path, String> {
    let path = string_arg(args, "path")
        .filter(|p| !p.is_empty())
        .ok_or_else(|| "missing required string field `path`.".to_string())?;
    if !Path::new(path).is_absolute() {
        return Err(format!(
            "`path` must be absolute, got `{path}`. Prefix it with the working directory."
        ));
    }
    Ok(Path::new(path))
}

pub fn truncate(s: &str) -> String {
    if s.len() <= MAX_OUTPUT {
        return s.to_string();
    }
    let head = floor_boundary(s, MAX_OUTPUT / 2);
    let tail = ceil_boundary(s, s.len() - MAX_OUTPUT / 2);
    format!(
        "{}\n\n[... {} bytes trimmed ...]\n\n{}",
        &s[..head],
        tail - head,
        &s[tail..]
    )
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// A fresh empty directory for a test.
#[cfg(test)]
pub fn temp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("bhai-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schemas_serialize_with_the_expected_names() {
        let names: Vec<_> = Registry::new(Vec::new())
            .schemas()
            .iter()
            .map(|s| {
                assert_eq!(s["type"], "function");
                assert_eq!(s["parameters"]["type"], "object");
                s["name"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(names, ["bash", "read", "write", "edit"]);
    }

    #[test]
    fn only_read_and_skill_skip_approval() {
        let skill = crate::skills::Skill {
            name: "s".to_string(),
            description: String::new(),
            dir: "/s".into(),
            source: "~/.claude/skills".to_string(),
        };
        let registry = Registry::new(vec![skill]);
        assert_eq!(registry.schemas().len(), 5);
        for name in ["bash", "write", "edit"] {
            assert!(registry.get(name).unwrap().needs_approval(), "{name}");
        }
        assert!(!registry.get("read").unwrap().needs_approval());
        assert!(!registry.get("skill").unwrap().needs_approval());
        assert!(Registry::new(Vec::new()).get("skill").is_none());
    }

    #[test]
    fn an_unknown_tool_lists_the_available_ones() {
        let registry = Registry::new(Vec::new());
        assert!(registry.get("nope").is_none());
        let out = registry.unknown("nope");
        assert!(out.contains("`nope`"), "{out}");
        assert!(out.contains("`bash`, `read`, `write`, `edit`"), "{out}");
    }

    #[test]
    fn arguments_must_be_a_json_object() {
        assert!(parse_arguments(r#"{"a":1}"#).is_ok());
        assert!(parse_arguments("[1]").is_err());
        assert!(parse_arguments("not json").is_err());
    }

    #[test]
    fn relative_paths_are_rejected() {
        let err = path_arg(&json!({"path": "src/main.rs"})).unwrap_err();
        assert!(err.contains("absolute"), "{err}");
        assert!(path_arg(&json!({})).is_err());
        assert!(path_arg(&json!({"path": "/tmp/x"})).is_ok());
    }

    #[test]
    fn truncate_keeps_both_ends_and_stays_valid_utf8() {
        let long = "é".repeat(MAX_OUTPUT);
        let out = truncate(&long);
        assert!(out.len() < long.len());
        assert!(out.contains("bytes trimmed"));
        assert!(out.starts_with('é') && out.ends_with('é'));
    }
}
