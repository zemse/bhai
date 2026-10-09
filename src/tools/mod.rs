//! The tools the model can call, and the registry the agent dispatches them through.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

pub mod agent;
pub mod aws_instance;
pub mod bash;
pub mod browser;
mod cdp;
pub mod edit;
pub mod fetch;
pub mod goal;
pub mod history;
pub mod image_gen;
pub mod mcp;
pub mod memory;
pub mod models;
pub mod monitor;
pub mod patch;
pub mod plan;
pub mod read;
pub mod schedule;
pub mod skill;
pub mod ssrf;
pub mod status;
pub mod stdin;
pub mod submit;
pub mod title;
pub mod view_image;
pub mod web;
pub mod write;

/// Every tool name, as identities refer to them.
pub const NAMES: [&str; 16] = [
    aws_instance::NAME,
    bash::NAME,
    read::NAME,
    write::NAME,
    edit::NAME,
    skill::NAME,
    agent::NAME,
    models::NAME,
    history::FIND,
    history::READ,
    memory::NAME,
    web::NAME,
    fetch::NAME,
    schedule::NAME,
    monitor::NAME,
    status::NAME,
];

/// Tool output past this is trimmed in the middle; the tail usually carries the error.
pub(crate) const MAX_OUTPUT: usize = 20_000;

/// An image past this many bytes is not sent: it would ride in every later request until
/// compaction took it out.
pub(crate) const MAX_IMAGE_BYTES: usize = 10 << 20;

/// An image past this many bytes is not taken in at all; under it, one too large to send
/// is scaled down first.
pub(crate) const MAX_IMAGE_INPUT: usize = 32 << 20;

/// The image types the Responses API reads.
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// An image a tool brings in, which goes to the model beside its text.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub mime: String,
    /// Standard base64.
    pub data: String,
}

impl Image {
    /// The image, or why it cannot be sent: a type the model cannot read, or too large.
    pub fn new(mime: &str, data: &str) -> Result<Self, String> {
        let mime = mime.trim().to_ascii_lowercase();
        if !IMAGE_TYPES.contains(&mime.as_str()) {
            return Err(format!("[image {mime}: not a type the model reads]"));
        }
        let data: String = data.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        let bytes = data.len() / 4 * 3;
        if bytes > MAX_IMAGE_INPUT {
            return Err(format!(
                "[image {mime}: {bytes} bytes, past the {MAX_IMAGE_INPUT} bhai takes in]"
            ));
        }
        Ok(Self { mime, data })
    }

    fn url(&self) -> String {
        format!("data:{};base64,{}", self.mime, self.data)
    }

    /// What stands for it where only text goes: the transcript, Ollama, a token count.
    fn placeholder(mime: &str) -> String {
        format!("[image {mime}]")
    }
}

/// `text` with a placeholder line for each image, for where only text goes.
pub fn with_images(text: &str, images: &[Image]) -> String {
    let mut out = text.to_string();
    for image in images {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&Image::placeholder(&image.mime));
    }
    out
}

/// The `function_call_output` that answers `call_id`: its output a string, or with images
/// a content array of the text and each image.
pub fn function_output(call_id: &str, text: &str, images: &[Image]) -> Value {
    let output = match images.is_empty() {
        true => Value::String(text.to_string()),
        false => parts(text, images),
    };
    serde_json::json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": output,
    })
}

/// A user message of `text` and the images attached to it, after the text.
pub fn user_message(text: &str, images: &[Image]) -> Value {
    serde_json::json!({
        "type": "message",
        "role": "user",
        "content": parts(text, images),
    })
}

/// A content array: `text`, then each image.
fn parts(text: &str, images: &[Image]) -> Value {
    let mut parts = vec![serde_json::json!({"type": "input_text", "text": text})];
    parts.extend(
        images
            .iter()
            .map(|image| serde_json::json!({"type": "input_image", "image_url": image.url()})),
    );
    Value::Array(parts)
}

/// The placeholder for an `input_image` part, its type read off the data URL.
pub fn image_part(part: &Value) -> String {
    let url = part.get("image_url").and_then(Value::as_str);
    let mime = url
        .and_then(|u| u.strip_prefix("data:"))
        .and_then(|u| u.split_once(';'))
        .map_or("", |(mime, _)| mime);
    Image::placeholder(mime)
}

/// A tool output's text: the string, or a content array's text with a placeholder for
/// each image.
pub fn output_text(output: &Value) -> String {
    match output {
        Value::String(text) => text.clone(),
        Value::Array(parts) => {
            let texts: Vec<String> = parts
                .iter()
                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("input_image") => Some(image_part(part)),
                    _ => part.get("text").and_then(Value::as_str).map(str::to_string),
                })
                .filter(|text| !text.is_empty())
                .collect();
            texts.join("\n")
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// How many images a tool output or a message's content carries.
pub fn output_images(output: &Value) -> usize {
    output.as_array().map_or(0, |parts| {
        parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("input_image"))
            .count()
    })
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    /// The Responses API tool definition: a function, or for `apply_patch` a custom tool.
    fn schema(&self) -> Value;
    fn needs_approval(&self) -> bool;
    /// Whether its calls may run alongside the others of one response: it changes
    /// nothing another call could read.
    fn parallel(&self) -> bool {
        false
    }
    /// Check the arguments and summarize the call in one line for the user.
    fn describe(&self, args: &Value) -> Result<String, String>;
    /// A unified diff of what the call would change, for the files it writes.
    fn preview(&self, _args: &Value) -> Option<String> {
        None
    }
    /// Run the call; returns the output and whether it counts as a success.
    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)>;
    /// Run the call while reporting output as it arrives; only `bash` and `write_stdin`
    /// stream.
    fn execute_live<'a>(
        &'a self,
        args: &'a Value,
        _live: Live<'a>,
    ) -> BoxFuture<'a, (String, bool)> {
        self.execute(args)
    }

    /// Run the call like `execute_live`, also returning the images it brought in. Only
    /// `view_image`, `image_gen` and `mcp_call` bring any.
    fn execute_images<'a>(
        &'a self,
        args: &'a Value,
        live: Live<'a>,
    ) -> BoxFuture<'a, (String, bool, Vec<Image>)> {
        Box::pin(async move {
            let (output, ok) = self.execute_live(args, live).await;
            (output, ok, Vec::new())
        })
    }
}

/// What a running call reports to and hears from the agent.
#[derive(Clone, Copy)]
pub struct Live<'a> {
    /// Takes output as it arrives, for the UI only; it never reaches history.
    pub progress: &'a (dyn Fn(String) + Send + Sync),
    /// Set when the user interrupts the turn.
    pub cancel: &'a AtomicBool,
    /// The conversation the call was made in; `None` outside a turn.
    pub conversation: Option<Conversation<'a>>,
}

/// What a tool that sends some of the conversation along is told about it.
#[derive(Clone, Copy)]
pub struct Conversation<'a> {
    /// The id the backend knows the conversation by.
    pub id: &'a str,
    pub model: &'a str,
    /// The history up to the call, the call's own response not yet in it.
    pub history: &'a [Value],
}

pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
    /// The hub calls to functions `tool_search` loaded are resolved against.
    native: Option<Arc<crate::mcp::Hub>>,
    /// A child's worktree, which its calls are moved into.
    rooted: Option<crate::worktrees::Rooted>,
}

impl Registry {
    /// No tools or MCP connections. Hosts add only the tools they want to expose.
    pub fn empty() -> Self {
        Self {
            tools: Vec::new(),
            native: None,
            rooted: None,
        }
    }

    /// Every built-in tool, including `register_skills` for startup and on-demand repo skills.
    pub fn new(skills: Vec<crate::skills::Skill>) -> Self {
        let mut tools: Vec<Box<dyn Tool>> = vec![
            Box::new(bash::Bash),
            Box::new(aws_instance::AwsInstance::default()),
            Box::new(read::Read),
            Box::new(write::Write),
            Box::new(edit::Edit),
            Box::new(patch::ApplyPatch),
            Box::new(view_image::ViewImage),
            Box::new(stdin::WriteStdin),
            Box::new(fetch::Fetch::default()),
        ];
        tools.push(Box::new(skill::Skill { skills }));
        Self {
            tools,
            native: None,
            rooted: None,
        }
    }

    /// `mcp_search` and `mcp_call`, when the hub has tools to offer, and `tool_search` when
    /// the session turned it on.
    pub fn with_mcp(mut self, hub: Option<Arc<crate::mcp::Hub>>) -> Self {
        if let Some(hub) = hub.filter(|h| h.has_tools()) {
            self.tools.push(Box::new(mcp::Search {
                hub: Arc::clone(&hub),
            }));
            if hub.tool_search() {
                self.tools.push(Box::new(mcp::ToolSearch {
                    hub: Arc::clone(&hub),
                }));
                self.native = Some(Arc::clone(&hub));
            }
            self.tools.push(Box::new(mcp::Call { hub }));
        }
        self
    }

    /// A call to a function `tool_search` loaded, as the `mcp_call` that runs it; `None`
    /// for any other call.
    pub async fn resolve(&self, call: &Value) -> Option<Value> {
        mcp::resolve(self.native.as_deref()?, call).await
    }

    /// The `agent` tool and its `close_agent`, for a parent session only.
    pub fn with_agent(mut self, agent: agent::Agent) -> Self {
        self.tools.push(Box::new(agent::Close {
            cancel: Arc::clone(&agent.cancel),
        }));
        self.tools.push(Box::new(agent));
        self
    }

    /// The `models` tool, which only a session that can delegate has a use for.
    pub fn with_models(mut self, models: models::Models) -> Self {
        self.tools.push(Box::new(models));
        self
    }

    /// `goal`, for the main agent. Added past the identity's narrowing, so every
    /// persistent objective can be completed or blocked.
    pub fn with_goal(mut self, goal: goal::Goal) -> Self {
        self.tools.push(Box::new(goal));
        self
    }

    /// `update_plan`, for the main agent, past the identity's narrowing like `goal`: it
    /// touches nothing but the checklist.
    pub fn with_plan(mut self, plan: plan::UpdatePlan) -> Self {
        self.tools.push(Box::new(plan));
        self
    }

    /// `find_sessions` and `read_session` over the sessions in `dir`, for the main agent.
    /// `current` is the session itself, which a search leaves out.
    pub fn with_history(mut self, dir: std::path::PathBuf, current: String) -> Self {
        self.tools.push(Box::new(history::FindSessions {
            dir: dir.clone(),
            current,
        }));
        self.tools.push(Box::new(history::ReadSession { dir }));
        self
    }

    /// `remember`, appending to the memory file at `path`, for the main agent.
    pub fn with_memory(mut self, path: std::path::PathBuf) -> Self {
        self.tools.push(Box::new(memory::Remember { path }));
        self
    }

    /// `schedule`, for the main agent of a session that fires the schedules it sets.
    pub fn with_schedule(mut self, schedule: schedule::Schedule) -> Self {
        self.tools.push(Box::new(schedule));
        self
    }

    /// `submit_result`, for a child whose step holds it to a contract. It is added past
    /// the identity's narrowing, since without it the child has no way to answer.
    pub fn with_submit(mut self, submit: submit::Submit) -> Self {
        self.tools.push(Box::new(submit));
        self
    }

    /// Append a host tool. Names must be unique within the registry.
    pub fn with_tool(mut self, tool: Box<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// The built-in tools and `register_skills`, narrowed to an identity's tools.
    pub fn for_identity(
        skills: Vec<crate::skills::Skill>,
        identity: &crate::identity::Identity,
    ) -> Self {
        let mut registry = Self::new(skills);
        registry.tools.retain(|t| identity.allows_tool(t.name()));
        registry
    }

    /// The tools a session's prompt allows: its skills, MCP tools, web search and image
    /// generation, narrowed to its identity's tools. Its identity's `mcp` globs already
    /// narrowed the MCP tools.
    pub fn for_prompt(prompt: &crate::prompt::SystemPrompt) -> Self {
        let mut registry = Self::new(prompt.skills.clone()).with_mcp(prompt.mcp.clone());
        if prompt.web_search {
            registry.tools.push(Box::new(web::WebSearch::codex()));
        }
        if prompt.image_gen {
            registry.tools.push(Box::new(image_gen::ImageGen::codex()));
        }
        registry
            .tools
            .retain(|t| prompt.identity.allows_tool(t.name()));
        registry
    }

    /// Move every call into a child's worktree before it is run.
    pub fn with_workdir(mut self, rooted: crate::worktrees::Rooted) -> Self {
        self.rooted = Some(rooted);
        self
    }

    /// Where automation for this registry runs, including a child's worktree.
    pub(crate) fn workdir<'a>(&'a self, fallback: &'a std::path::Path) -> &'a std::path::Path {
        self.rooted
            .as_ref()
            .map(|rooted| rooted.workdir.as_path())
            .unwrap_or(fallback)
    }

    /// `args` for a call to `name`, moved into the worktree when there is one.
    pub fn rooted(&self, name: &str, args: Value) -> Value {
        match &self.rooted {
            Some(rooted) => rooted.args(name, args),
            None => args,
        }
    }

    /// Rooted `args` as the permission rules must see them: as the same call in the
    /// checkout when there is a worktree.
    pub fn checked(&self, name: &str, args: &Value) -> Value {
        match &self.rooted {
            Some(rooted) => rooted.checked(name, args),
            None => args.clone(),
        }
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

    /// Whether `call` may run alongside the others of its response. A call to a function
    /// `tool_search` loaded is an `mcp_call`, which may change anything.
    pub fn parallel(&self, call: &Value) -> bool {
        call.get("type").and_then(Value::as_str) == Some("function_call")
            && call
                .get("name")
                .and_then(Value::as_str)
                .and_then(|name| self.get(name))
                .is_some_and(|tool| tool.parallel())
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

/// A `custom_tool_call` as the `function_call` it is run as, its freeform input carried
/// as the one argument `input`.
pub fn custom_call(item: &Value) -> Option<Value> {
    if item.get("type").and_then(Value::as_str) != Some("custom_tool_call") {
        return None;
    }
    let input = item
        .get("input")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Some(serde_json::json!({
        "type": "function_call",
        "name": item.get("name").cloned().unwrap_or(Value::Null),
        "call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
        "arguments": serde_json::json!({ "input": input }).to_string(),
    }))
}

/// The `custom_tool_call_output` that answers `call_id`.
pub fn custom_output(call_id: &str, output: &str) -> Value {
    serde_json::json!({
        "type": "custom_tool_call_output",
        "call_id": call_id,
        "output": output,
    })
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

/// A file read for an approval preview is skipped past this.
const MAX_PREVIEW_BYTES: u64 = 1 << 20;

/// The text a write or edit preview diffs against. It runs before the user is asked, so
/// anything but a small regular file is an error: a fifo blocks and `/dev/zero` never ends.
pub(crate) fn preview_text(path: &Path) -> std::io::Result<String> {
    use std::io::{Error, ErrorKind, Read};
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() || meta.len() > MAX_PREVIEW_BYTES {
        return Err(Error::from(ErrorKind::InvalidInput));
    }
    let mut bytes = Vec::new();
    // Bounded again in case the file grew since the check.
    std::fs::File::open(path)?
        .take(MAX_PREVIEW_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PREVIEW_BYTES {
        return Err(Error::from(ErrorKind::InvalidInput));
    }
    String::from_utf8(bytes).map_err(|_| Error::from(ErrorKind::InvalidData))
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
                if s["name"] != patch::NAME {
                    assert_eq!(s["type"], "function");
                    assert_eq!(s["parameters"]["type"], "object");
                }
                s["name"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(
            names,
            [
                "bash",
                "aws_instance",
                "read",
                "write",
                "edit",
                "apply_patch",
                "view_image",
                "write_stdin",
                "fetch",
                "register_skills"
            ]
        );
    }

    #[test]
    fn only_read_view_image_skill_and_write_stdin_skip_approval() {
        let skill = crate::skills::Skill {
            name: "s".to_string(),
            description: String::new(),
            dir: "/s".into(),
            source: "~/.claude/skills".to_string(),
        };
        let registry = Registry::new(vec![skill]);
        assert_eq!(registry.schemas().len(), 10);
        for name in ["bash", "write", "edit", "apply_patch", "fetch"] {
            assert!(registry.get(name).unwrap().needs_approval(), "{name}");
        }
        assert!(!registry.get("read").unwrap().needs_approval());
        assert!(!registry.get("view_image").unwrap().needs_approval());
        assert!(!registry.get("register_skills").unwrap().needs_approval());
        // The policy asks about what it types; a poll is not asked about.
        assert!(!registry.get("write_stdin").unwrap().needs_approval());
        assert!(Registry::new(Vec::new()).get("register_skills").is_some());
    }

    #[test]
    fn image_gen_is_offered_when_the_prompt_turns_it_on_and_needs_approval() {
        let off = crate::prompt::system_prompt(&[], Vec::new());
        assert!(Registry::for_prompt(&off).get(image_gen::NAME).is_none());
        let on = crate::prompt::SystemPrompt {
            image_gen: true,
            ..crate::prompt::system_prompt(&[], Vec::new())
        };
        let registry = Registry::for_prompt(&on);
        assert!(registry.get(image_gen::NAME).unwrap().needs_approval());
        let call = serde_json::json!({"type": "function_call", "name": image_gen::NAME});
        assert!(!registry.parallel(&call));
    }

    #[test]
    fn an_unknown_tool_lists_the_available_ones() {
        let registry = Registry::new(Vec::new());
        assert!(registry.get("nope").is_none());
        let out = registry.unknown("nope");
        assert!(out.contains("`nope`"), "{out}");
        assert!(
            out.contains("`bash`, `aws_instance`, `read`, `write`, `edit`"),
            "{out}"
        );
    }

    #[test]
    fn a_custom_tool_call_runs_as_a_function_call_and_is_answered_in_kind() {
        let item = json!({"type": "custom_tool_call", "call_id": "c1", "name": "apply_patch",
            "input": "*** Begin Patch\n*** End Patch"});
        let call = custom_call(&item).unwrap();
        assert_eq!(call["type"], "function_call");
        assert_eq!(call["name"], "apply_patch");
        assert_eq!(call["call_id"], "c1");
        let args = parse_arguments(call["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["input"], "*** Begin Patch\n*** End Patch");
        assert!(custom_call(&json!({"type": "function_call"})).is_none());
        let output = custom_output("c1", "done");
        assert_eq!(output["type"], "custom_tool_call_output");
        assert_eq!(output["call_id"], "c1");
        assert_eq!(output["output"], "done");
    }

    #[test]
    fn images_ride_in_a_content_array_and_read_back_as_placeholders() {
        let plain = function_output("c1", "ok", &[]);
        assert_eq!(plain["output"], "ok");
        assert_eq!(output_images(&plain["output"]), 0);

        let image = Image::new("Image/PNG", "iVBO\nRw0K").unwrap();
        assert_eq!(image.mime, "image/png");
        assert_eq!(image.data, "iVBORw0K");
        let item = function_output("c1", "shot", &[image.clone(), image]);
        assert_eq!(item["type"], "function_call_output");
        assert_eq!(item["call_id"], "c1");
        let parts = item["output"].as_array().unwrap();
        assert_eq!(parts[0], json!({"type": "input_text", "text": "shot"}));
        assert_eq!(
            parts[1],
            json!({"type": "input_image", "image_url": "data:image/png;base64,iVBORw0K"})
        );
        assert_eq!(output_images(&item["output"]), 2);
        assert_eq!(
            output_text(&item["output"]),
            "shot\n[image image/png]\n[image image/png]"
        );
    }

    #[test]
    fn a_user_message_without_images_is_the_plain_text_one() {
        assert_eq!(user_message("hi", &[]), crate::compact::user_message("hi"));
        let image = Image::new("image/png", "iVBORw0K").unwrap();
        let item = user_message("see [image #1]", &[image]);
        assert_eq!(item["content"][0]["text"], "see [image #1]");
        assert_eq!(output_images(&item["content"]), 1);
        assert_eq!(image_part(&item["content"][1]), "[image image/png]");
    }

    #[test]
    fn an_image_the_model_cannot_take_is_refused_with_a_reason() {
        let svg = Image::new("image/svg+xml", "PHN2Zz4=").unwrap_err();
        assert!(svg.contains("not a type the model reads"), "{svg}");
        let huge = "A".repeat(MAX_IMAGE_INPUT / 3 * 4 + 8);
        let large = Image::new("image/png", &huge).unwrap_err();
        assert!(large.contains("past the"), "{large}");
        assert_eq!(with_images("", &[]), "");
        let image = Image::new("image/jpeg", "/9j/").unwrap();
        assert_eq!(with_images("", &[image]), "[image image/jpeg]");
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
