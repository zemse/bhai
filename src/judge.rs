//! The auto-approval judge: when the rules leave a call at `Ask` in `auto` mode in a
//! trusted project, a small model call decides whether it is a reasonable step toward the
//! task the user asked for. There are exactly two verdicts, approve and deny; an error, a
//! timeout, a malformed reply or a spent budget is not a third one, it falls back to
//! asking the user. The judge never sees what the rules already decided, and its request
//! is its own: a separate cache key, no tools, and nothing appended to the conversation.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::client::{Client, Usage};
use crate::tools::BoxFuture;

/// Transcript labels the summary carries, newest last.
const RECENT: usize = 6;
/// Verdicts the summary carries, newest last.
const VERDICTS: usize = 10;
/// Longest a label or verdict line may be in the summary, in characters.
const CLIP: usize = 200;
/// Longest the judged command or path, and the edit detail, may be, in characters. Well
/// past any real command, so the judge is never asked to rule on a fragment.
const TARGET_CLIP: usize = 2000;
/// Longest the user's task may be in the summary, in characters.
const TASK_CLIP: usize = 1200;
/// The cache key suffix of every judge call, so its prefix caches on its own.
const CACHE_KEY: &str = "judge";

/// Fixed for the life of the session, so the judge's prefix caches.
pub const SYSTEM: &str = "\
You decide whether one tool call a coding agent wants to make may run without asking the \
user. You are told the task the user gave the agent, the call, and where it would run.

Approve only if both hold: the call is a reasonable step toward the stated task, and its \
blast radius is confined to the project root.

Deny anything unrelated to the stated task, anything that reaches outside the project \
root, anything that sends data to a network endpoint the task did not ask for, and \
anything destructive beyond what the task implies. When you are unsure, deny.

A field marked truncated means you cannot see the whole command, so deny.

Answer with a strict JSON object and nothing else, no prose and no code fence:
{\"verdict\":\"approve\",\"reason\":\"<at most 12 words>\"}";

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Approve { reason: String },
    Deny { reason: String },
}

impl Verdict {
    pub fn reason(&self) -> &str {
        match self {
            Verdict::Approve { reason } | Verdict::Deny { reason } => reason,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Verdict::Approve { .. } => "approve",
            Verdict::Deny { .. } => "deny",
        }
    }
}

/// What the judge is told about one call: a compact summary, never the transcript.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JudgeRequest {
    /// The user's current task: the latest user message.
    pub task: String,
    pub tool: String,
    /// The exact command, or the exact path.
    pub target: String,
    /// A diff summary for an edit; empty for anything else.
    pub detail: String,
    pub cwd: String,
    pub root: String,
    /// The last few transcript labels, one line each.
    pub recent: Vec<String>,
    /// The verdicts already given this session.
    pub verdicts: Vec<String>,
}

impl JudgeRequest {
    /// The one message the judge reads.
    pub fn text(&self) -> String {
        let mut out = format!(
            "task: {}\ntool: {}\n",
            clip(&self.task, TASK_CLIP),
            self.tool
        );
        out.push_str(&field("target", &self.target, TARGET_CLIP));
        if !self.detail.is_empty() {
            out.push_str(&field("detail", &self.detail, TARGET_CLIP));
        }
        out.push_str(&format!("cwd: {}\nproject root: {}\n", self.cwd, self.root));
        for (header, list) in [
            ("recent calls", &self.recent),
            ("verdicts this session", &self.verdicts),
        ] {
            if list.is_empty() {
                continue;
            }
            out.push_str(&format!("{header}:\n"));
            for line in list {
                out.push_str(&format!("- {}\n", clip(line, CLIP)));
            }
        }
        out
    }
}

/// The model call behind the judge, so tests inject one that never talks to the model.
pub trait Decide: Send + Sync {
    fn decide<'a>(&'a self, request: &'a JudgeRequest) -> BoxFuture<'a, Result<(Verdict, Usage)>>;
}

/// The `judge*` config keys.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// `judge`: never active in `ask` or `bypass`, which never reach the judge anyway.
    pub on: bool,
    /// `judge_model`: the session's model when unset.
    pub model: Option<String>,
    /// `judge_effort`: the cheapest effort the backend takes.
    pub effort: String,
    pub timeout: Duration,
    pub max_per_turn: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            on: true,
            model: None,
            effort: "low".to_string(),
            timeout: Duration::from_millis(15_000),
            max_per_turn: 20,
        }
    }
}

/// The judge, its per-session cache and its per-turn budget.
pub struct Judge {
    backend: Arc<dyn Decide>,
    root: PathBuf,
    settings: Settings,
    /// Where every verdict is appended, if anywhere.
    log: Option<PathBuf>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    task: String,
    recent: VecDeque<String>,
    verdicts: VecDeque<String>,
    /// One verdict per `tool` and target, so the same call is never judged twice.
    cache: HashMap<String, Verdict>,
    /// Calls judged in the running turn.
    spent: usize,
    total: Usage,
}

impl Judge {
    pub fn new(backend: Arc<dyn Decide>, root: PathBuf, settings: Settings) -> Self {
        Self {
            backend,
            root,
            settings,
            log: None,
            state: Mutex::default(),
        }
    }

    /// Append every verdict to `path`.
    pub fn with_log(mut self, path: PathBuf) -> Self {
        self.log = Some(path);
        self
    }

    /// A new turn on `task`, which refills the budget.
    pub fn start_turn(&self, task: &str) {
        let mut state = self.lock();
        state.task = task.to_string();
        state.spent = 0;
    }

    /// Record a finished call, so the judge sees what the agent has been doing.
    pub fn note(&self, label: &str) {
        let mut state = self.lock();
        state
            .recent
            .push_back(clip(&label.replace('\n', " "), CLIP));
        while state.recent.len() > RECENT {
            state.recent.pop_front();
        }
    }

    /// What the judge has cost so far; never part of the conversation's totals.
    pub fn total(&self) -> Usage {
        self.lock().total
    }

    /// What `/permissions` prints about the judge.
    pub fn describe(&self) -> String {
        if !self.settings.on {
            return "\njudge: off".to_string();
        }
        let state = self.lock();
        format!(
            "\njudge: on, {} of {} calls judged this turn, {} cached",
            state.spent,
            self.settings.max_per_turn,
            state.cache.len()
        )
    }

    /// Decide one call. `None` means the user must be asked, exactly as today.
    pub async fn decide(&self, tool: &str, target: &str, detail: &str) -> Option<Verdict> {
        if !self.settings.on {
            return None;
        }
        let key = format!("{tool}\u{0}{target}");
        let request = {
            let mut state = self.lock();
            if let Some(verdict) = state.cache.get(&key) {
                return Some(verdict.clone());
            }
            if state.spent >= self.settings.max_per_turn {
                return None;
            }
            state.spent += 1;
            JudgeRequest {
                task: state.task.clone(),
                tool: tool.to_string(),
                target: target.to_string(),
                detail: detail.to_string(),
                cwd: self.root.display().to_string(),
                root: self.root.display().to_string(),
                recent: state.recent.iter().cloned().collect(),
                verdicts: state.verdicts.iter().cloned().collect(),
            }
        };

        let started = Instant::now();
        let answered = tokio::time::timeout(self.settings.timeout, self.backend.decide(&request))
            .await
            .unwrap_or_else(|_| bail!("the judge did not answer in time"));
        let elapsed = started.elapsed();

        let (verdict, usage) = match answered {
            Ok(answered) => answered,
            // Fail closed: an error is not a third verdict, the user is asked.
            Err(e) => {
                self.record(&request, None, &format!("{e:#}"), Usage::default(), elapsed);
                return None;
            }
        };
        {
            let mut state = self.lock();
            add(&mut state.total, usage);
            state.cache.insert(key, verdict.clone());
            state.verdicts.push_back(format!(
                "{}: {} ({target})",
                verdict.name(),
                verdict.reason()
            ));
            while state.verdicts.len() > VERDICTS {
                state.verdicts.pop_front();
            }
        }
        self.record(&request, Some(&verdict), "", usage, elapsed);
        Some(verdict)
    }

    /// Append one JSONL line; a failed write must never fail the call.
    fn record(
        &self,
        request: &JudgeRequest,
        verdict: Option<&Verdict>,
        error: &str,
        usage: Usage,
        elapsed: Duration,
    ) {
        let Some(path) = &self.log else {
            return;
        };
        let line = json!({
            "timestamp": chrono::Local::now().to_rfc3339(),
            "summary": request.text(),
            "verdict": verdict.map(Verdict::name),
            "reason": verdict.map(Verdict::reason),
            "error": (!error.is_empty()).then_some(error),
            "input": usage.input,
            "cached": usage.cached,
            "output": usage.output,
            "reasoning": usage.reasoning,
            "latency_ms": elapsed.as_millis(),
        });
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            use std::io::Write;
            let _ = writeln!(file, "{line}");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The judge backed by the session's client, on a cache key of its own so its request
/// never disturbs the conversation's cached prefix.
pub struct ModelJudge {
    client: Client,
    model: String,
    effort: String,
}

impl ModelJudge {
    pub fn new(client: Client, settings: &Settings) -> Self {
        let model = settings
            .model
            .clone()
            .unwrap_or_else(|| client.model().to_string());
        Self {
            client,
            model,
            effort: settings.effort.clone(),
        }
    }
}

impl Decide for ModelJudge {
    fn decide<'a>(&'a self, request: &'a JudgeRequest) -> BoxFuture<'a, Result<(Verdict, Usage)>> {
        Box::pin(async move {
            let (reply, usage) = self
                .client
                .aside(
                    CACHE_KEY,
                    &self.model,
                    &self.effort,
                    SYSTEM,
                    &request.text(),
                )
                .await?;
            Ok((parse_verdict(&reply)?, usage))
        })
    }
}

/// Read the judge's reply. Anything that is not the agreed object is an error, so the
/// call falls back to the user rather than to a guess.
pub fn parse_verdict(reply: &str) -> Result<Verdict> {
    let start = reply
        .find('{')
        .context("the judge answered no JSON object")?;
    let end = reply
        .rfind('}')
        .context("the judge answered no JSON object")?;
    if end < start {
        bail!("the judge answered no JSON object");
    }
    let value: Value =
        serde_json::from_str(&reply[start..=end]).context("the judge answered malformed JSON")?;
    let reason = clip(
        value
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        CLIP,
    );
    match value.get("verdict").and_then(Value::as_str) {
        Some("approve") => Ok(Verdict::Approve { reason }),
        Some("deny") => Ok(Verdict::Deny { reason }),
        other => bail!(
            "the judge answered `{}`, not approve or deny",
            other.unwrap_or("nothing")
        ),
    }
}

/// The exact command or path the judge is asked about, and the extra detail that goes
/// with it. `summary` is the tool's own one-line description, for everything else.
pub fn target(tool: &str, args: &Value, summary: &str) -> (String, String) {
    let text = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let first = |s: &str| clip(&s.lines().next().unwrap_or_default().replace('\t', " "), 80);
    match tool {
        "bash" => (text("command"), String::new()),
        "write" => (
            text("path"),
            format!("{} bytes written", text("content").len()),
        ),
        "edit" => (
            text("path"),
            format!(
                "- {}\n+ {}",
                first(&text("old_string")),
                first(&text("new_string"))
            ),
        ),
        _ => (summary.to_string(), String::new()),
    }
}

/// One `label: value` line, the label saying so when the value had to be cut, so a
/// truncated command never reads as a whole one.
fn field(label: &str, text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{label} (truncated at {max} chars): {}\n", &text[..at]),
        None => format!("{label}: {text}\n"),
    }
}

/// `text` cut to `max` characters, with an ellipsis when it was longer.
fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}...", &text[..at]),
        None => text.to_string(),
    }
}

fn add(total: &mut Usage, usage: Usage) {
    total.input += usage.input;
    total.cached += usage.cached;
    total.output += usage.output;
    total.reasoning += usage.reasoning;
}

/// A scripted judge for tests.
#[cfg(test)]
pub mod fake {
    use std::path::Path;
    use std::sync::Mutex;

    use super::*;

    /// What the fake backend does with the call it is given.
    pub enum Answers {
        Verdict(Verdict),
        /// A reply text, parsed exactly as the real one is.
        Reply(String),
        Error(String),
        /// Never answers, so the judge's timeout fires.
        Hang,
        /// Approves, having first set `cancel`, so the verdict races an interrupt.
        Interrupted(Arc<std::sync::atomic::AtomicBool>),
    }

    pub struct Backend {
        answers: Answers,
        pub calls: Mutex<Vec<JudgeRequest>>,
    }

    impl Backend {
        pub fn new(answers: Answers) -> Arc<Self> {
            Arc::new(Self {
                answers,
                calls: Mutex::default(),
            })
        }
    }

    impl Decide for Backend {
        fn decide<'a>(
            &'a self,
            request: &'a JudgeRequest,
        ) -> BoxFuture<'a, Result<(Verdict, Usage)>> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(request.clone());
                let usage = Usage {
                    input: 700,
                    cached: 600,
                    output: 12,
                    reasoning: 0,
                };
                match &self.answers {
                    Answers::Verdict(verdict) => Ok((verdict.clone(), usage)),
                    Answers::Reply(reply) => Ok((parse_verdict(reply)?, usage)),
                    Answers::Error(e) => bail!("{e}"),
                    Answers::Hang => {
                        std::future::pending::<()>().await;
                        unreachable!()
                    }
                    Answers::Interrupted(cancel) => {
                        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                        Ok((
                            Verdict::Approve {
                                reason: "a step toward the task".to_string(),
                            },
                            usage,
                        ))
                    }
                }
            })
        }
    }

    /// A judge over `answers`, rooted at `root`, on a turn that has a task.
    pub fn judge(answers: Answers, root: &Path) -> (Judge, Arc<Backend>) {
        judge_with(answers, root, Settings::default())
    }

    /// `judge` with `settings`, whose timeout is shortened so a hang is quick.
    pub fn judge_with(answers: Answers, root: &Path, settings: Settings) -> (Judge, Arc<Backend>) {
        let backend = Backend::new(answers);
        let judge = Judge::new(
            backend.clone(),
            root.to_path_buf(),
            Settings {
                timeout: Duration::from_millis(50),
                ..settings
            },
        );
        judge.start_turn("add a unit test for the parser");
        (judge, backend)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::fake::{Answers, judge};
    use super::*;

    fn approve(reason: &str) -> Verdict {
        Verdict::Approve {
            reason: reason.to_string(),
        }
    }

    #[tokio::test]
    async fn a_verdict_is_cached_and_the_budget_is_per_turn() {
        let (judge, backend) = judge(Answers::Verdict(approve("in the project")), Path::new("/p"));
        assert_eq!(
            judge.decide("bash", "cargo test", "").await,
            Some(approve("in the project"))
        );
        // The same call again is answered from the cache, not by a second model call.
        assert_eq!(
            judge.decide("bash", "cargo test", "").await,
            Some(approve("in the project"))
        );
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
        assert_eq!(judge.total().input, 700);

        // The second call carries the first verdict, and the budget is what is left.
        judge.decide("bash", "cargo build", "").await;
        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls[1].verdicts, ["approve: in the project (cargo test)"]);
        assert_eq!(calls[1].task, "add a unit test for the parser");
    }

    #[tokio::test]
    async fn the_budget_falls_back_to_asking_and_the_next_turn_refills_it() {
        let (judge, backend) = super::fake::judge_with(
            Answers::Verdict(approve("fine")),
            Path::new("/p"),
            Settings {
                max_per_turn: 1,
                ..Settings::default()
            },
        );
        assert_eq!(
            judge.decide("bash", "cargo build", "").await,
            Some(approve("fine"))
        );
        assert_eq!(judge.decide("bash", "cargo doc", "").await, None);
        judge.start_turn("now document it");
        assert_eq!(
            judge.decide("bash", "cargo doc", "").await,
            Some(approve("fine"))
        );
        assert_eq!(backend.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_error_a_timeout_and_a_malformed_reply_all_fall_back_to_asking() {
        for answers in [
            Answers::Error("429".to_string()),
            Answers::Hang,
            Answers::Reply("sure, go ahead".to_string()),
            Answers::Reply(r#"{"verdict":"maybe","reason":"x"}"#.to_string()),
        ] {
            let (judge, _) = judge(answers, Path::new("/p"));
            assert_eq!(judge.decide("bash", "cargo test", "").await, None);
            assert_eq!(judge.total(), Usage::default());
        }
    }

    #[tokio::test]
    async fn off_never_calls_the_model() {
        let backend = super::fake::Backend::new(Answers::Verdict(approve("fine")));
        let judge = Judge::new(
            backend.clone(),
            "/p".into(),
            Settings {
                on: false,
                ..Settings::default()
            },
        );
        assert_eq!(judge.decide("bash", "cargo test", "").await, None);
        assert!(backend.calls.lock().unwrap().is_empty());
        assert_eq!(judge.describe(), "\njudge: off");
    }

    #[test]
    fn a_reply_is_read_strictly() {
        assert_eq!(
            parse_verdict(r#"```json{"verdict":"deny","reason":"unrelated to the task"}```"#)
                .unwrap(),
            Verdict::Deny {
                reason: "unrelated to the task".to_string()
            }
        );
        for reply in ["", "{}", "{\"verdict\":\"ask\"}", "{oops}"] {
            assert!(parse_verdict(reply).is_err(), "{reply}");
        }
    }

    /// A full summary around `target`: every list at its limit, a real task.
    fn realistic(target: &str) -> JudgeRequest {
        JudgeRequest {
            task: "the write tool truncates files over 64k, find out why and add a \
regression test for it in src/tools/write.rs"
                .to_string(),
            tool: "bash".to_string(),
            target: target.to_string(),
            detail: String::new(),
            cwd: "/home/u/workspace/bhai".to_string(),
            root: "/home/u/workspace/bhai".to_string(),
            recent: (0..RECENT)
                .map(|i| format!("read /home/u/workspace/bhai/src/tools/write.rs -> {i} lines"))
                .collect(),
            verdicts: (0..VERDICTS)
                .map(|i| format!("approve: reads a project file ({i})"))
                .collect(),
        }
    }

    fn tokens(request: &JudgeRequest) -> usize {
        let tokenizer = crate::tokens::for_model("gpt-5.5");
        tokenizer.count(SYSTEM) + tokenizer.count(&request.text())
    }

    #[test]
    fn a_realistic_summary_stays_under_a_thousand_tokens() {
        let count = tokens(&realistic("cargo test --all-features -- --nocapture write"));
        assert!(count < 1000, "{count} tokens");
    }

    #[test]
    fn the_worst_case_summary_stays_under_two_and_a_half_thousand_tokens() {
        let mut command = "cd /h/w/bhai/src && grep -n 'fn x' judge.rs && ".repeat(50);
        command.truncate(TARGET_CLIP);
        let count = tokens(&realistic(&command));
        assert!(count < 2500, "{count} tokens");
    }

    #[test]
    fn a_long_command_reaches_the_judge_whole() {
        let prefix = "git clone --depth 1 https://example.test/";
        let command = format!("{prefix}{}", "a".repeat(500 - prefix.len()));
        assert_eq!(command.len(), 500);
        let text = realistic(&command).text();
        assert!(text.contains(&format!("target: {command}\n")), "{text}");
        assert!(!text.contains("..."), "{text}");
        assert!(!text.contains("truncated"), "{text}");
    }

    #[test]
    fn a_command_past_the_limit_is_marked_truncated() {
        let text = realistic(&"a".repeat(3000)).text();
        assert!(
            text.contains(&format!(
                "target (truncated at 2000 chars): {}\n",
                "a".repeat(2000)
            )),
            "{text}"
        );
    }

    #[test]
    fn an_edit_is_summarized_as_a_diff() {
        let args = json!({"path": "/p/src/lib.rs", "old_string": "a\nb", "new_string": "c"});
        assert_eq!(
            target("edit", &args, "edit /p/src/lib.rs"),
            ("/p/src/lib.rs".to_string(), "- a\n+ c".to_string())
        );
        let args = json!({"command": "cargo test"});
        assert_eq!(
            target("bash", &args, "cargo test"),
            ("cargo test".to_string(), String::new())
        );
    }
}
