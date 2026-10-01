//! `mcp_search` finds MCP tools and shows their schemas; `mcp_call` runs one and needs
//! approval, decided under its `mcp__server__tool` name. `tool_search`, behind
//! `[mcp] tool_search`, is the Responses API's own: it loads matches as deferred functions,
//! and a call to one runs as the `mcp_call` it stands for, so it is approved the same way.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use super::{BoxFuture, Live, Tool, string_arg, truncate};
use crate::mcp::Hub;

pub const SEARCH: &str = "mcp_search";
pub const CALL: &str = "mcp_call";
pub const TOOL_SEARCH: &str = "tool_search";

/// Longest argument summary shown in the approval prompt.
const SUMMARY_ARGS: usize = 160;
/// How soon an interrupt stops the wait for a server's answer.
const TICK: Duration = Duration::from_millis(50);

pub struct Search {
    pub hub: Arc<Hub>,
}

pub struct Call {
    pub hub: Arc<Hub>,
}

pub struct ToolSearch {
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
                Ok(q) => (truncate(&self.hub.search(q).await), true),
                Err(e) => (e, false),
            }
        })
    }
}

impl Tool for ToolSearch {
    fn name(&self) -> &str {
        TOOL_SEARCH
    }

    fn schema(&self) -> Value {
        json!({
            "type": "tool_search",
            "execution": "client",
            "description": "Search the connected MCP servers' tools by keywords or a server \
        name. The matches are loaded for your next call, and you call them directly by name. \
        Runs without approval; each call to a loaded tool is approved.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keywords, a tool name, or a server name."
                    },
                    "limit": {
                        "type": "number",
                        "description": "Most tools to load; 5 when left out."
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
        query(args).map(|q| format!("tool_search {q}"))
    }

    /// The output is the JSON of the `tools` the `tool_search_output` carries.
    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let q = match query(args) {
                Ok(q) => q,
                Err(e) => return (e, false),
            };
            let limit = args
                .get("limit")
                .and_then(Value::as_f64)
                .map(|n| n.max(0.0) as usize);
            let found = self.hub.search_native(q, limit).await;
            (Value::Array(found).to_string(), true)
        })
    }
}

/// A `tool_search_call` as the `function_call` to `tool_search` it is run as.
pub fn search_call(item: &Value) -> Option<Value> {
    if item.get("type").and_then(Value::as_str) != Some("tool_search_call") {
        return None;
    }
    let arguments = match item.get("arguments") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "{}".to_string(),
    };
    Some(json!({
        "type": "function_call",
        "name": TOOL_SEARCH,
        "call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
        "arguments": arguments,
    }))
}

/// The `tool_search_output` that answers `call_id`. A search that failed loads nothing.
pub fn search_output(call_id: &str, output: &str) -> Value {
    let tools = serde_json::from_str::<Vec<Value>>(output).unwrap_or_default();
    json!({
        "type": "tool_search_output",
        "call_id": call_id,
        "status": "completed",
        "execution": "client",
        "tools": tools,
    })
}

/// A call to a function `tool_search` loaded, as the `mcp_call` it stands for, or `None`
/// for a call that names no namespace. One that matches no tool still becomes an
/// `mcp_call`, which says so.
pub async fn resolve(hub: &Hub, call: &Value) -> Option<Value> {
    let namespace = call.get("namespace").and_then(Value::as_str)?;
    let function = string_arg(call, "name").unwrap_or_default();
    let name = match hub.resolve_native(namespace, function).await {
        Some(name) => name,
        None => format!("{namespace}{function}"),
    };
    let raw = string_arg(call, "arguments").unwrap_or_default();
    let arguments = match raw.trim().is_empty() {
        true => json!({}),
        false => serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string())),
    };
    Some(json!({
        "type": "function_call",
        "name": CALL,
        "call_id": call.get("call_id").cloned().unwrap_or(Value::Null),
        "arguments": json!({"name": name, "arguments": arguments}).to_string(),
    }))
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
        if !self.hub.may_call(name) {
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
        static NEVER: AtomicBool = AtomicBool::new(false);
        let live = Live {
            progress: &|_| {},
            cancel: &NEVER,
            conversation: None,
        };
        self.execute_live(args, live)
    }

    /// Dropping the call's future is what abandons a server that stopped answering.
    fn execute_live<'a>(
        &'a self,
        args: &'a Value,
        live: Live<'a>,
    ) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let name = string_arg(args, "name").unwrap_or_default();
            let arguments = match arguments(args) {
                Ok(arguments) => arguments,
                Err(e) => return (e, false),
            };
            let call = self.hub.call(name, arguments);
            tokio::pin!(call);
            let mut tick = tokio::time::interval(TICK);
            loop {
                tokio::select! {
                    biased;
                    _ = tick.tick() => {
                        if live.cancel.load(Ordering::Relaxed) {
                            let why = format!("The user interrupted the turn; `{name}` did not finish.");
                            return (why, false);
                        }
                    }
                    out = &mut call => return out,
                }
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
        assert_eq!(Registry::new(Vec::new()).with_mcp(None).schemas().len(), 5);
    }

    #[test]
    fn tool_search_is_offered_only_when_the_session_turned_it_on() {
        let off = Registry::new(Vec::new()).with_mcp(Some(hub()));
        assert!(off.get(TOOL_SEARCH).is_none());
        let on = Arc::new(
            Hub::offline(vec![("gh", vec![ToolInfo::test("gh", "get_issue", "")])])
                .with_tool_search(),
        );
        let registry = Registry::new(Vec::new()).with_mcp(Some(on));
        let search = registry.get(TOOL_SEARCH).unwrap();
        assert!(!search.needs_approval());
        assert_eq!(search.schema()["type"], "tool_search");
        assert_eq!(search.schema()["execution"], "client");
        // `mcp_search` and `mcp_call` stay, for Ollama and for a model that uses them.
        assert!(registry.get(SEARCH).is_some() && registry.get(CALL).is_some());
    }

    #[tokio::test]
    async fn a_loaded_function_runs_as_the_mcp_call_it_stands_for() {
        let hub = hub();
        let call = json!({"type": "function_call", "call_id": "c1", "namespace": "mcp__gh__",
                          "name": "get_issue", "arguments": "{\"n\":1}"});
        let resolved = resolve(&hub, &call).await.unwrap();
        assert_eq!(resolved["name"], CALL);
        assert_eq!(resolved["call_id"], "c1");
        let args: Value = serde_json::from_str(resolved["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(
            args,
            json!({"name": "mcp__gh__get_issue", "arguments": {"n": 1}})
        );

        // An unknown one still goes to `mcp_call`, which says there is no such tool.
        let unknown = json!({"type": "function_call", "namespace": "mcp__gh__", "name": "nope",
                             "arguments": "[1]"});
        let resolved = resolve(&hub, &unknown).await.unwrap();
        let args: Value = serde_json::from_str(resolved["arguments"].as_str().unwrap()).unwrap();
        let mcp_call = Call {
            hub: Arc::clone(&hub),
        };
        assert!(
            mcp_call
                .describe(&args)
                .unwrap_err()
                .contains("no MCP tool")
        );

        let plain = json!({"type": "function_call", "name": "read", "arguments": "{}"});
        assert!(resolve(&hub, &plain).await.is_none());
    }

    #[tokio::test]
    async fn a_tool_search_call_is_run_and_answered_in_kind() {
        let item = json!({"type": "tool_search_call", "call_id": "s1", "execution": "client",
                          "arguments": {"query": "issue", "limit": 2}});
        let call = search_call(&item).unwrap();
        assert_eq!(call["name"], TOOL_SEARCH);
        let args = crate::tools::parse_arguments(call["arguments"].as_str().unwrap()).unwrap();
        let search = ToolSearch { hub: hub() };
        assert_eq!(search.describe(&args).unwrap(), "tool_search issue");
        let (out, ok) = search.execute(&args).await;
        assert!(ok, "{out}");
        let answer = search_output("s1", &out);
        assert_eq!(answer["type"], "tool_search_output");
        assert_eq!(answer["tools"][0]["tools"][0]["name"], "get_issue");
        // A failed search loads nothing rather than sending its error as tools.
        let failed = search_output("s1", "missing required string field `query`.");
        assert_eq!(failed["tools"], json!([]));
        assert!(search_call(&json!({"type": "function_call"})).is_none());
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

    #[tokio::test]
    async fn an_interrupted_call_stops_waiting_for_the_server() {
        static CANCELLED: AtomicBool = AtomicBool::new(false);
        let Some(server) = crate::mcp::fake_server("stuck", "hangcall") else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("bhai-mcp-call-{}", uuid::Uuid::new_v4()));
        let hub = Hub::connect(
            vec![server],
            &crate::identity::Identity::default(),
            &dir,
            Duration::from_secs(10),
        )
        .await;
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            CANCELLED.store(true, Ordering::Relaxed);
        });
        let call = Call { hub: Arc::new(hub) };
        let live = Live {
            progress: &|_| {},
            cancel: &CANCELLED,
            conversation: None,
        };
        let args = json!({"name": "mcp__stuck__echo", "arguments": {"message": "hi"}});
        let (out, ok) = call.execute_live(&args, live).await;
        assert!(!ok && out.contains("interrupted"), "{out}");
        call.hub.shutdown().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_known_secret_in_the_result_is_redacted() {
        static CANCELLED: AtomicBool = AtomicBool::new(false);
        let Some(server) = crate::mcp::fake_server("x", "") else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("bhai-mcp-call-{}", uuid::Uuid::new_v4()));
        let hub = Hub::connect(
            vec![server],
            &crate::identity::Identity::default(),
            &dir,
            Duration::from_secs(10),
        )
        .await;
        crate::redact::register("mcp-call-secret-6d2b");
        let call = Call { hub: Arc::new(hub) };
        let live = Live {
            progress: &|_| {},
            cancel: &CANCELLED,
            conversation: None,
        };
        let args =
            json!({"name": "mcp__x__echo", "arguments": {"message": "mcp-call-secret-6d2b"}});
        let (out, ok) = call.execute_live(&args, live).await;
        assert!(ok && out.contains("[REDACTED]"), "{out}");
        assert!(!out.contains("mcp-call-secret-6d2b"), "{out}");
        call.hub.shutdown().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_search_trims_a_server_schema_too_big_to_read() {
        let huge = ToolInfo {
            schema: json!({"pad": "x".repeat(60_000)}),
            ..ToolInfo::test("gh", "get_issue", "Get an issue")
        };
        let search = Search {
            hub: Arc::new(Hub::offline(vec![("gh", vec![huge])])),
        };
        let (out, ok) = search.execute(&json!({"query": "issue"})).await;
        assert!(ok && out.len() < 30_000, "{} bytes", out.len());
        assert!(out.contains("bytes trimmed"), "{out}");
    }
}
