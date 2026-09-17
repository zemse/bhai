//! `mcp_search` finds MCP tools and shows their schemas; `mcp_call` runs one and needs
//! approval, decided under its `mcp__server__tool` name.

use std::sync::Arc;

use serde_json::{Value, json};

use super::{BoxFuture, Tool, string_arg};
use crate::mcp::Hub;

pub const SEARCH: &str = "mcp_search";
pub const CALL: &str = "mcp_call";

/// Longest argument summary shown in the approval prompt.
const SUMMARY_ARGS: usize = 160;

pub struct Search {
    pub hub: Arc<Hub>,
}

pub struct Call {
    pub hub: Arc<Hub>,
}

impl Tool for Search {
    fn name(&self) -> &str {
        SEARCH
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": SEARCH,
            "description": "Search the connected MCP servers' tools by keywords or a server \
        name. Returns up to 5 `mcp__server__tool` names with their descriptions and input schemas. \
        Runs without approval.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keywords, a tool name, or a server name."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        query(args).map(|q| format!("mcp_search {q}"))
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            match query(args) {
                Ok(q) => (self.hub.search(q), true),
                Err(e) => (e, false),
            }
        })
    }
}

fn query(args: &Value) -> Result<&str, String> {
    string_arg(args, "query")
        .filter(|q| !q.trim().is_empty())
        .ok_or_else(|| "missing required string field `query`.".to_string())
}

impl Tool for Call {
    fn name(&self) -> &str {
        CALL
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": CALL,
            "description": "Run an MCP tool found with `mcp_search`. The user approves each \
        call.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The exact `mcp__server__tool` name."
                    },
                    "arguments": {
                        "type": "object",
                        "description": "Arguments matching the tool's input schema."
                    }
                },
                "required": ["name", "arguments"],
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        let name = string_arg(args, "name")
            .ok_or_else(|| "missing required string field `name`.".to_string())?;
        if self.hub.find(name).is_none() {
            return Err(format!(
                "no MCP tool named `{name}`. Use `mcp_search` to find the exact name."
            ));
        }
        let arguments = arguments(args)?;
        let mut summary = arguments.to_string();
        if summary.len() > SUMMARY_ARGS {
            let end = super::floor_boundary(&summary, SUMMARY_ARGS);
            summary.truncate(end);
            summary.push_str("...");
        }
        Ok(format!("{name} {summary}"))
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let name = string_arg(args, "name").unwrap_or_default();
            match arguments(args) {
                Ok(arguments) => self.hub.call(name, arguments).await,
                Err(e) => (e, false),
            }
        })
    }
}

/// The `arguments` object; a missing one is empty.
fn arguments(args: &Value) -> Result<Value, String> {
    match args.get("arguments") {
        None | Some(Value::Null) => Ok(json!({})),
        Some(Value::Object(map)) => Ok(Value::Object(map.clone())),
        Some(_) => Err("`arguments` must be a JSON object.".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::ToolInfo;
    use crate::tools::Registry;

    fn hub() -> Arc<Hub> {
        Arc::new(Hub::offline(vec![(
            "gh",
            vec![ToolInfo::test("gh", "get_issue", "Get an issue")],
        )]))
    }

    #[test]
    fn the_registry_adds_both_tools_only_when_there_are_mcp_tools() {
        let with = Registry::new(Vec::new()).with_mcp(Some(hub()));
        assert!(!with.get(SEARCH).unwrap().needs_approval());
        assert!(with.get(CALL).unwrap().needs_approval());
        let empty = Arc::new(Hub::offline(vec![("gh", Vec::new())]));
        let without = Registry::new(Vec::new()).with_mcp(Some(empty));
        assert!(without.get(SEARCH).is_none() && without.get(CALL).is_none());
        assert_eq!(Registry::new(Vec::new()).with_mcp(None).schemas().len(), 4);
    }

    #[test]
    fn calls_are_summarized_by_their_mcp_name() {
        let call = Call { hub: hub() };
        let describe = |args: Value| call.describe(&args);
        assert_eq!(
            describe(json!({"name": "mcp__gh__get_issue", "arguments": {"n": 1}})).unwrap(),
            r#"mcp__gh__get_issue {"n":1}"#
        );
        assert_eq!(
            describe(json!({"name": "mcp__gh__get_issue"})).unwrap(),
            "mcp__gh__get_issue {}"
        );
        let long =
            describe(json!({"name": "mcp__gh__get_issue", "arguments": {"s": "é".repeat(200)}}));
        assert!(long.unwrap().ends_with("..."));
        assert!(
            describe(json!({"name": "mcp__gh__nope"}))
                .unwrap_err()
                .contains("mcp_search")
        );
        assert!(describe(json!({"name": "mcp__gh__get_issue", "arguments": [1]})).is_err());
        assert!(describe(json!({})).is_err());

        let search = Search { hub: hub() };
        assert_eq!(
            search.describe(&json!({"query": "issue"})).unwrap(),
            "mcp_search issue"
        );
        assert!(search.describe(&json!({"query": " "})).is_err());
    }
}
