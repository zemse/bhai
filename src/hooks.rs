//! Lifecycle hook configuration and bounded command and HTTP handlers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

const OUTPUT_LIMIT: u64 = 64 * 1024;

/// Lifecycle points understood by the hook configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub enum Event {
    SessionStart,
    Setup,
    SessionEnd,
    UserPromptSubmit,
    UserPromptExpansion,
    PreToolUse,
    PermissionRequest,
    PermissionDenied,
    PostToolUse,
    PostToolUseFailure,
    PostToolBatch,
    Notification,
    MessageDisplay,
    SubagentStart,
    SubagentStop,
    TaskCreated,
    TaskCompleted,
    Stop,
    StopFailure,
    TeammateIdle,
    InstructionsLoaded,
    ConfigChange,
    CwdChanged,
    DirectoryAdded,
    FileChanged,
    WorktreeCreate,
    WorktreeRemove,
    PreCompact,
    PostCompact,
    PreModelSwitch,
    PostModelSwitch,
    Elicitation,
    ElicitationResult,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    #[serde(default)]
    hooks: BTreeMap<Event, Vec<Group>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Group {
    #[serde(default)]
    matcher: String,
    hooks: Vec<Handler>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Handler {
    Command {
        command: String,
        #[serde(default = "timeout")]
        timeout: u64,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default = "timeout")]
        timeout: u64,
    },
}

fn timeout() -> u64 {
    60
}

impl Handler {
    fn timeout(&self) -> Duration {
        Duration::from_secs(match self {
            Self::Command { timeout, .. } | Self::Http { timeout, .. } => *timeout,
        })
    }

    fn validate(&self) -> Result<()> {
        if self.timeout().is_zero() || self.timeout() > Duration::from_secs(600) {
            bail!("hook timeout must be between 1 and 600 seconds");
        }
        match self {
            Self::Command { command, .. } if command.trim().is_empty() => {
                bail!("hook command is empty");
            }
            Self::Http { url, .. } => {
                let url = reqwest::Url::parse(url)?;
                if !matches!(url.scheme(), "http" | "https")
                    || !url.username().is_empty()
                    || url.password().is_some()
                {
                    bail!("HTTP hooks require an http or https URL without embedded credentials");
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Loaded handlers. Project files are only read when the caller grants project trust.
#[derive(Debug, Default)]
pub struct Hooks {
    settings: Vec<Settings>,
    cwd: PathBuf,
}

/// Effects returned to the lifecycle owner; errors are never permission approvals.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    pub blocked: Option<String>,
    pub ask: bool,
    pub updated_input: Option<Value>,
    pub context: Vec<String>,
    pub notices: Vec<String>,
}

impl Hooks {
    /// Load bhai's dedicated hook files, not Claude Code's executable configuration.
    pub async fn load(home: Option<&Path>, cwd: &Path, trusted: bool) -> (Self, Vec<String>) {
        let mut paths = Vec::new();
        if let Some(home) = home {
            paths.push(home.join(".config/bhai/hooks.json"));
        }
        if trusted {
            paths.push(cwd.join(".bhai/hooks.json"));
        }
        let mut loaded = Self {
            settings: Vec::new(),
            cwd: cwd.to_path_buf(),
        };
        let mut notices = Vec::new();
        for path in paths {
            match Self::read(&path).await {
                Ok(Some(settings)) => {
                    for event in settings.hooks.keys() {
                        if !matches!(
                            event,
                            Event::PreToolUse | Event::PostToolUse | Event::PostToolUseFailure
                        ) {
                            notices.push(format!(
                                "hooks in {}: {event:?} is not yet wired into the built-in runtime",
                                path.display()
                            ));
                        }
                    }
                    loaded.settings.push(settings);
                }
                Ok(None) => {}
                Err(error) => notices.push(format!("hooks in {}: {error:#}", path.display())),
            }
        }
        (loaded, notices)
    }

    /// Run handlers in the registry's worktree while retaining the configuration roots.
    pub fn with_workdir(mut self, cwd: &Path) -> Self {
        self.cwd = cwd.to_path_buf();
        self
    }

    async fn read(path: &Path) -> Result<Option<Settings>> {
        let file = match tokio::fs::File::open(path).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let bytes = bounded(file).await?;
        let settings: Settings = serde_json::from_slice(&bytes)?;
        for groups in settings.hooks.values() {
            for group in groups {
                validate_matcher(&group.matcher)?;
                for handler in &group.hooks {
                    handler.validate()?;
                }
            }
        }
        Ok(Some(settings))
    }

    /// Run matching handlers in configuration order with a deadline on each handler.
    pub fn fire<'a>(
        &'a self,
        event: Event,
        matcher: &'a str,
        mut input: Value,
    ) -> futures_util::future::BoxFuture<'a, Outcome> {
        Box::pin(async move {
            let mut outcome = Outcome::default();
            let Some(object) = input.as_object_mut() else {
                outcome.notices.push("hook input must be an object".into());
                return outcome;
            };
            object.insert("hook_event_name".into(), json!(event));
            object.insert("cwd".into(), json!(self.cwd));
            let mut groups: Vec<&Group> = Vec::new();
            for settings in &self.settings {
                if let Some(found) = settings.hooks.get(&event) {
                    groups.extend(found);
                }
            }
            for group in groups {
                if !matches(&group.matcher, matcher) {
                    continue;
                }
                for handler in &group.hooks {
                    let result =
                        tokio::time::timeout(handler.timeout(), self.run(handler, &input)).await;
                    match result {
                        Ok(Ok((code, stdout, stderr))) => {
                            if code == 2 {
                                if blocks(event) {
                                    outcome.blocked = Some(if stderr.trim().is_empty() {
                                        "Blocked by a lifecycle hook".into()
                                    } else {
                                        stderr.trim().to_string()
                                    });
                                } else {
                                    outcome.notices.push(stderr);
                                }
                            } else if code != 0 {
                                outcome
                                    .notices
                                    .push(format!("hook exited with {code}: {}", stderr.trim()));
                            } else if !stdout.trim().is_empty() {
                                match serde_json::from_str::<Value>(&stdout) {
                                    Ok(value) => apply(event, &value, &mut outcome),
                                    Err(error) => {
                                        outcome.notices.push(format!("invalid hook JSON: {error}"))
                                    }
                                }
                            }
                        }
                        Ok(Err(error)) => outcome.notices.push(format!("hook failed: {error:#}")),
                        Err(_) => outcome.notices.push("hook timed out".into()),
                    }
                }
            }
            outcome
        })
    }

    async fn run(&self, handler: &Handler, input: &Value) -> Result<(i32, String, String)> {
        match handler {
            Handler::Command { command, .. } => {
                let mut process = tokio::process::Command::new("/bin/bash");
                process
                    .args(["-c", command])
                    .current_dir(&self.cwd)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .env("BHAI_PROJECT_DIR", &self.cwd)
                    .env("CLAUDE_PROJECT_DIR", &self.cwd);
                crate::childenv::scrub(&mut process);
                crate::childenv::non_interactive(&mut process);
                #[cfg(unix)]
                process.process_group(0);
                let mut child = process.spawn().context("starting hook command")?;
                #[cfg(unix)]
                let _group = ProcessGroup(child.id().context("hook process id unavailable")?);
                let mut stdin = child.stdin.take().context("hook stdin unavailable")?;
                let stdout = child.stdout.take().context("hook stdout unavailable")?;
                let stderr = child.stderr.take().context("hook stderr unavailable")?;
                let bytes = serde_json::to_vec(input)?;
                let write = async move {
                    let result = async {
                        stdin.write_all(&bytes).await?;
                        stdin.shutdown().await
                    }
                    .await;
                    match result {
                        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                        other => other,
                    }
                };
                let ((), stdout, stderr) =
                    tokio::try_join!(write, bounded(stdout), bounded(stderr))?;
                let status = child.wait().await?;
                Ok((
                    status.code().unwrap_or(1),
                    String::from_utf8(stdout)?,
                    String::from_utf8(stderr)?,
                ))
            }
            Handler::Http { url, headers, .. } => {
                let client = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()?;
                let mut request = client.post(url).json(input);
                for (name, value) in headers {
                    request = request.header(name, value);
                }
                let mut response = request.send().await?;
                if !response.status().is_success() {
                    bail!("HTTP hook returned {}", response.status());
                }
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await? {
                    if bytes.len() + chunk.len() > OUTPUT_LIMIT as usize {
                        bail!("hook output exceeds 64 KiB");
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok((0, String::from_utf8(bytes)?, String::new()))
            }
        }
    }
}

#[cfg(unix)]
struct ProcessGroup(u32);

#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // The group is created for this hook alone, including its descendants.
        #[allow(unsafe_code)]
        unsafe {
            libc::killpg(self.0 as libc::pid_t, libc::SIGKILL);
        }
    }
}

async fn bounded(reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() as u64 > OUTPUT_LIMIT {
        return Err(std::io::Error::other("hook output exceeds 64 KiB"));
    }
    Ok(bytes)
}

fn validate_matcher(pattern: &str) -> Result<()> {
    if pattern
        .chars()
        .any(|c| matches!(c, '[' | ']' | '(' | ')' | '\\' | '+' | '?' | '^' | '$'))
    {
        bail!(
            "hook matchers currently support exact names, * and | alternatives, not regular expressions"
        );
    }
    if pattern.split('|').any(|p| p.contains('*') && p != "*") {
        bail!("hook matcher * must stand alone");
    }
    Ok(())
}

fn matches(pattern: &str, name: &str) -> bool {
    pattern.is_empty() || pattern.split('|').any(|p| p == "*" || p == name)
}

fn blocks(event: Event) -> bool {
    matches!(
        event,
        Event::PreToolUse
            | Event::PermissionRequest
            | Event::UserPromptSubmit
            | Event::UserPromptExpansion
            | Event::Stop
            | Event::SubagentStop
            | Event::TaskCreated
            | Event::TaskCompleted
            | Event::TeammateIdle
            | Event::ConfigChange
            | Event::PreModelSwitch
    )
}

fn apply(event: Event, value: &Value, outcome: &mut Outcome) {
    let reason = || {
        value
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("Blocked by a lifecycle hook")
            .to_string()
    };
    if blocks(event)
        && (value.get("continue") == Some(&json!(false))
            || value.get("decision").and_then(Value::as_str) == Some("block"))
    {
        outcome.blocked = Some(
            value
                .get("stopReason")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(reason),
        );
    }
    let Some(specific) = value.get("hookSpecificOutput") else {
        return;
    };
    if specific.get("hookEventName") != Some(&json!(event)) {
        outcome
            .notices
            .push("hookSpecificOutput event does not match the fired event".into());
        return;
    }
    if let Some(context) = specific.get("additionalContext").and_then(Value::as_str) {
        outcome.context.push(context.to_string());
    }
    if matches!(event, Event::PreToolUse | Event::PermissionRequest) {
        match specific.get("permissionDecision").and_then(Value::as_str) {
            Some("deny") => {
                outcome.blocked = Some(
                    specific
                        .get("permissionDecisionReason")
                        .and_then(Value::as_str)
                        .unwrap_or("Denied by a lifecycle hook")
                        .to_string(),
                )
            }
            Some("ask") => outcome.ask = true,
            Some("allow") => outcome
                .notices
                .push("hook allow does not override bhai's permission policy".into()),
            _ => {}
        }
        if event == Event::PreToolUse
            && let Some(updated) = specific.get("updatedInput")
        {
            if updated.is_object() {
                outcome.updated_input = Some(updated.clone());
            } else {
                outcome
                    .notices
                    .push("hook updatedInput must be an object".into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hooks(command: &str) -> Hooks {
        Hooks { settings: vec![serde_json::from_value(json!({"hooks":{"PreToolUse":[{"matcher":"bash","hooks":[{"type":"command","command":command,"timeout":1}]}]}})).unwrap()], cwd: std::env::temp_dir() }
    }

    #[test]
    fn matchers_are_explicit() {
        assert!(matches("bash|edit", "edit"));
        assert!(matches("*", "read"));
        assert!(!matches("bash", "bash_extra"));
        assert!(validate_matcher("Bash|Edit").is_ok());
        assert!(validate_matcher(".*").is_err());
        assert!(validate_matcher("[a-z]+").is_err());
    }

    #[test]
    fn unknown_handlers_and_events_are_errors() {
        assert!(serde_json::from_value::<Settings>(json!({"hooks":{"Typo":[]}})).is_err());
        assert!(serde_json::from_value::<Handler>(json!({"type":"prompt","prompt":"ok"})).is_err());
    }

    #[test]
    fn decisions_do_not_override_policy_or_cross_events() {
        let mut result = Outcome::default();
        apply(
            Event::PreToolUse,
            &json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}),
            &mut result,
        );
        assert_eq!(result.notices.len(), 1);
        apply(
            Event::PreToolUse,
            &json!({"hookSpecificOutput":{"hookEventName":"Stop","permissionDecision":"deny"}}),
            &mut result,
        );
        assert!(result.blocked.is_none());
        apply(
            Event::PostToolUse,
            &json!({"decision":"block"}),
            &mut result,
        );
        assert!(result.blocked.is_none());
    }

    #[tokio::test]
    async fn command_receives_json_and_can_deny() {
        let result = hooks("cat >/dev/null; printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"no\"}}'")
            .fire(Event::PreToolUse, "bash", json!({"tool_name":"bash"})).await;
        assert_eq!(result.blocked.as_deref(), Some("no"));
        assert!(result.notices.is_empty());
    }

    #[tokio::test]
    async fn exit_two_blocks_and_mismatched_tools_do_not_run() {
        let engine = hooks("cat >/dev/null; echo blocked >&2; exit 2");
        assert_eq!(
            engine
                .fire(Event::PreToolUse, "bash", json!({}))
                .await
                .blocked
                .as_deref(),
            Some("blocked")
        );
        assert_eq!(
            engine.fire(Event::PreToolUse, "read", json!({})).await,
            Outcome::default()
        );
    }

    #[tokio::test]
    async fn errors_and_timeouts_are_not_decisions() {
        let result = hooks("cat >/dev/null; printf 'invalid'")
            .fire(Event::PreToolUse, "bash", json!({}))
            .await;
        assert_eq!(result.notices.len(), 1);
        assert!(result.blocked.is_none());
        let result = hooks("exec sleep 5")
            .fire(Event::PreToolUse, "bash", json!({}))
            .await;
        assert_eq!(result.notices, ["hook timed out"]);
    }

    #[tokio::test]
    async fn http_receives_event_and_returns_context() {
        use axum::{Json, Router, routing::post};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route("/hook", post(|Json(input): Json<Value>| async move {
            assert_eq!(input["hook_event_name"], "PreToolUse");
            assert_eq!(input["tool_name"], "bash");
            Json(json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"checked"}}))
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let engine = Hooks {
            settings: vec![serde_json::from_value(json!({"hooks":{"PreToolUse":[{"hooks":[{"type":"http","url":format!("http://{address}/hook")}]}]}})).unwrap()],
            cwd: std::env::temp_dir(),
        };
        let result = engine
            .fire(Event::PreToolUse, "bash", json!({"tool_name":"bash"}))
            .await;
        server.abort();
        assert_eq!(result.context, ["checked"]);
        assert!(result.notices.is_empty());
    }

    #[tokio::test]
    async fn handlers_run_in_the_registry_workdir() {
        let dir = crate::tools::temp_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let engine = hooks("cat >/dev/null; printf '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"additionalContext\":\"%s\"}}' \"$PWD\"")
            .with_workdir(&dir);
        let result = engine.fire(Event::PreToolUse, "bash", json!({})).await;
        assert_eq!(result.context.len(), 1);
        assert_eq!(
            std::fs::canonicalize(&result.context[0]).unwrap(),
            std::fs::canonicalize(&dir).unwrap()
        );
        tokio::fs::remove_dir_all(dir).await.unwrap();
    }

    #[tokio::test]
    async fn hook_need_not_consume_stdin() {
        let result = hooks("echo denied >&2; exit 2")
            .fire(
                Event::PreToolUse,
                "bash",
                json!({"large": "x".repeat(32768)}),
            )
            .await;
        assert_eq!(result.blocked.as_deref(), Some("denied"));
        assert!(result.notices.is_empty());
    }

    #[tokio::test]
    async fn output_is_bounded() {
        let bytes = vec![b'x'; OUTPUT_LIMIT as usize + 1];
        assert!(bounded(bytes.as_slice()).await.is_err());
    }

    #[tokio::test]
    async fn untrusted_project_configuration_is_not_read() {
        let dir = crate::tools::temp_dir();
        tokio::fs::create_dir_all(dir.join(".bhai")).await.unwrap();
        tokio::fs::write(dir.join(".bhai/hooks.json"), b"not json")
            .await
            .unwrap();
        let (engine, notices) = Hooks::load(None, &dir, false).await;
        assert!(engine.settings.is_empty());
        assert!(notices.is_empty());
        let (_, notices) = Hooks::load(None, &dir, true).await;
        assert_eq!(notices.len(), 1);
        tokio::fs::remove_dir_all(dir).await.unwrap();
    }
}
