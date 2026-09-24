//! Workflows: a handful of child agent steps run in dependency order under one token
//! budget. Definitions are markdown files with frontmatter, like identities. Only the
//! user starts one, with `/workflow` or `--workflow`; the model is never offered a
//! workflow tool, so it cannot spend the budget on its own.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use chrono::Local;
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};

use crate::agent::{self, AgentEvent, Child, Children, Delegation};
use crate::client::Usage;
use crate::frontmatter;
use crate::identity::{self, Identity};
use crate::instructions::{self, Roots};
use crate::judge::Judge;
use crate::permissions::{Offers, Policy};
use crate::tools;

/// The tool name the confirmation prompt carries.
pub const TOOL: &str = "workflow";
/// Tokens a workflow may spend before it stops launching steps.
const DEFAULT_BUDGET: u64 = 200_000;
/// Workflow files under the home directory.
const HOME_DIR: &str = ".config/bhai/workflows";
/// Workflow files under the project root; they replace a home one of the same name.
const PROJECT_DIR: &str = ".bhai/workflows";
/// Cached step results, under the `.bhai` the session transcripts are in.
const CACHE_DIR: &str = "cache/workflows";

/// What a step does when it fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnFail {
    /// Launch no more steps.
    Stop,
    /// Carry on with the steps that do not need this one.
    Continue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub id: String,
    pub identity: String,
    /// The task, with `{{input}}` and `{{steps.<id>}}` still in it.
    pub prompt: String,
    /// Step ids that must finish first.
    pub needs: Vec<String>,
    pub on_fail: OnFail,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Workflow {
    pub name: String,
    pub description: String,
    /// Where it was defined, as shown to the user.
    pub source: String,
    pub budget_tokens: u64,
    /// Steps launched at once, never more than the child agent fan-out cap.
    pub max_parallel: usize,
    /// Keep each step's result and hand it back when the step has not changed. Off
    /// unless the file asks for it: a hit answers today's run with yesterday's text.
    pub cache: bool,
    pub steps: Vec<Step>,
    /// Prose shown by `/workflows`.
    pub body: String,
}

/// Every workflow file found, and the ones that would not load.
#[derive(Debug, Default)]
pub struct Found {
    pub workflows: Vec<Arc<Workflow>>,
    /// One line per file that failed to parse.
    pub errors: Vec<String>,
}

/// The built-in workflows (there are none) and every workflow file; a later definition
/// replaces an earlier one of the same name.
pub fn discover(roots: &Roots) -> Found {
    let mut dirs = Vec::new();
    if let Some(home) = &roots.home {
        dirs.push(home.join(HOME_DIR));
    }
    dirs.push(instructions::project_root(&roots.cwd).join(PROJECT_DIR));

    let mut found = Found::default();
    for dir in dirs {
        let label = instructions::label(&dir, roots);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        files.sort();
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            match parse(&text, &label) {
                Ok(workflow) => {
                    let workflow = Arc::new(workflow);
                    match found.workflows.iter_mut().find(|w| w.name == workflow.name) {
                        Some(slot) => *slot = workflow,
                        None => found.workflows.push(workflow),
                    }
                }
                Err(e) => found.errors.push(format!(
                    "skipped {}: {e:#}",
                    instructions::label(&path, roots)
                )),
            }
        }
    }
    found
}

/// A workflow file. Everything that cannot be checked once it is running (a cycle, a
/// placeholder with no value, a step that needs a step that is not there) fails here.
pub fn parse(text: &str, source: &str) -> Result<Workflow> {
    let (front, body) = frontmatter::split(text).context("no frontmatter")?;
    let value = |key: &str| frontmatter::value(&front, key).filter(|v| !v.is_empty());
    let name = value("name").context("no `name`")?;
    let number = |key: &str, default: u64| -> Result<u64> {
        match value(key) {
            Some(text) => text
                .parse()
                .with_context(|| format!("bad `{key}` `{text}`")),
            None => Ok(default),
        }
    };
    let budget_tokens = number("budget_tokens", DEFAULT_BUDGET)?;
    let max_parallel = number("max_parallel", 1)?.clamp(1, tools::agent::MAX_RUNNING as u64);
    let cache = match value("cache").as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        Some(other) => bail!("bad `cache` `{other}`"),
    };

    let mut steps = Vec::new();
    for item in frontmatter::items(&front, "steps").unwrap_or_default() {
        let value = |key: &str| frontmatter::value(&item, key).filter(|v| !v.is_empty());
        let id = value("id").context("a step has no `id`")?;
        let prompt = value("prompt").with_context(|| format!("step `{id}` has no `prompt`"))?;
        let on_fail = match value("on_fail").as_deref() {
            None | Some("stop") => OnFail::Stop,
            Some("continue") => OnFail::Continue,
            Some(other) => bail!("step `{id}` has a bad `on_fail` `{other}`"),
        };
        steps.push(Step {
            id,
            identity: value("identity").unwrap_or_else(|| identity::DEFAULT.to_string()),
            prompt,
            needs: frontmatter::list(&item, "needs").unwrap_or_default(),
            on_fail,
        });
    }
    if steps.is_empty() {
        bail!("no `steps`");
    }
    check(&steps)?;
    Ok(Workflow {
        name,
        description: value("description").unwrap_or_default(),
        source: source.to_string(),
        budget_tokens,
        max_parallel: max_parallel as usize,
        cache,
        steps,
        body: body.trim_end().to_string(),
    })
}

/// Unique ids, dependencies that exist and are not circular, and placeholders that a
/// value will be there for.
fn check(steps: &[Step]) -> Result<()> {
    for (index, step) in steps.iter().enumerate() {
        if steps[..index].iter().any(|s| s.id == step.id) {
            bail!("two steps are called `{}`", step.id);
        }
        for need in &step.needs {
            if !steps.iter().any(|s| &s.id == need) {
                bail!("step `{}` needs `{need}`, which is not a step", step.id);
            }
        }
        for name in placeholders(&step.prompt) {
            match name.strip_prefix("steps.") {
                None if name == "input" => {}
                Some(id) if step.needs.iter().any(|n| n == id) => {}
                Some(id) if steps.iter().any(|s| s.id == id) => {
                    bail!(
                        "step `{}` uses `{{{{{name}}}}}` but does not need `{id}`",
                        step.id
                    )
                }
                _ => bail!("step `{}` uses unknown `{{{{{name}}}}}`", step.id),
            }
        }
    }
    order(steps)?;
    Ok(())
}

/// Step indexes in dependency order; `Err` names the steps in a cycle.
fn order(steps: &[Step]) -> Result<Vec<usize>> {
    let mut done = vec![false; steps.len()];
    let mut order = Vec::with_capacity(steps.len());
    while order.len() < steps.len() {
        let ready: Vec<usize> = steps
            .iter()
            .enumerate()
            .filter(|(index, step)| {
                !done[*index]
                    && step.needs.iter().all(|need| {
                        steps
                            .iter()
                            .position(|s| &s.id == need)
                            .is_some_and(|i| done[i])
                    })
            })
            .map(|(index, _)| index)
            .collect();
        if ready.is_empty() {
            let stuck: Vec<&str> = steps
                .iter()
                .enumerate()
                .filter(|(index, _)| !done[*index])
                .map(|(_, step)| step.id.as_str())
                .collect();
            bail!("steps need each other in a cycle: {}", stuck.join(", "));
        }
        for index in ready {
            done[index] = true;
            order.push(index);
        }
    }
    Ok(order)
}

/// The `{{...}}` names in `text`, in order.
fn placeholders(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        found.push(after[..end].trim());
        rest = &after[end + 2..];
    }
    found
}

/// `text` with `{{input}}` and `{{steps.<id>}}` filled in. A name with no value is left
/// as it is; `check` already refused the ones that could not be filled.
fn render(text: &str, input: &str, results: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        let name = after[..end].trim();
        let value = match name.strip_prefix("steps.") {
            Some(id) => results.get(id).map(String::as_str),
            None if name == "input" => Some(input),
            None => None,
        };
        out.push_str(&rest[..start]);
        match value {
            Some(value) => out.push_str(value),
            None => out.push_str(&rest[start..start + 4 + end]),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// The workflow called `name`, or an error listing the available ones.
pub fn find(workflows: &[Arc<Workflow>], name: &str) -> Result<Arc<Workflow>> {
    if let Some(workflow) = workflows.iter().find(|w| w.name == name) {
        return Ok(Arc::clone(workflow));
    }
    let names: Vec<&str> = workflows.iter().map(|w| w.name.as_str()).collect();
    if names.is_empty() {
        bail!("unknown workflow `{name}`. No workflows are defined.");
    }
    bail!(
        "unknown workflow `{name}`. Available workflows: {}",
        names.join(", ")
    )
}

/// What `/workflows` prints.
pub fn report(found: &Found) -> String {
    let mut out = String::new();
    if found.workflows.is_empty() {
        out.push_str(&format!(
            "no workflows. Define them in ~/{HOME_DIR}/*.md or <project>/{PROJECT_DIR}/*.md.\n"
        ));
    }
    for workflow in &found.workflows {
        let _ = writeln!(
            out,
            "{} ({}): {} step(s), budget {} tokens, {} at a time{}",
            workflow.name,
            workflow.source,
            workflow.steps.len(),
            workflow.budget_tokens,
            workflow.max_parallel,
            caching(workflow)
        );
        // The description and the file's prose, indented under the definition.
        for line in workflow.description.lines().chain(workflow.body.lines()) {
            let _ = match line.is_empty() {
                true => writeln!(out),
                false => writeln!(out, "  {line}"),
            };
        }
    }
    for error in &found.errors {
        let _ = writeln!(out, "{error}");
    }
    out
}

/// What the cache setting adds to a line describing a workflow.
fn caching(workflow: &Workflow) -> &'static str {
    match workflow.cache {
        true => ", unchanged steps from cache",
        false => "",
    }
}

/// How one step ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Ok,
    Failed(String),
    /// Never launched, and why.
    Skipped(String),
}

/// One step's line of the final report.
#[derive(Debug, Clone, PartialEq)]
pub struct StepReport {
    pub id: String,
    pub identity: String,
    pub status: Status,
    pub usage: Usage,
    /// Answered from the cache, so it cost nothing and ran no child.
    pub cached: bool,
}

/// What a finished run spent, step by step.
#[derive(Debug, Clone)]
pub struct Report {
    pub name: String,
    pub steps: Vec<StepReport>,
    pub usage: Usage,
    pub budget: u64,
    /// Set when the run never started, such as when the user said no.
    pub refused: Option<String>,
}

impl Report {
    /// The transcript line the run ends with.
    pub fn text(&self) -> String {
        if let Some(reason) = &self.refused {
            return format!("workflow {}: {reason}", self.name);
        }
        let mut out = format!(
            "workflow {} finished, {}/{} tokens of a {} budget",
            self.name, self.usage.input, self.usage.output, self.budget
        );
        for step in &self.steps {
            let status = match &step.status {
                Status::Ok if step.cached => "ok, from cache".to_string(),
                Status::Ok => "ok".to_string(),
                Status::Failed(e) => format!("failed: {e}"),
                Status::Skipped(reason) => format!("did not run: {reason}"),
            };
            let _ = write!(
                out,
                "\n  {} ({}) {status}, {}/{} tokens",
                step.id, step.identity, step.usage.input, step.usage.output
            );
        }
        out
    }
}

/// One workflow run: the definition, its input, and everything a child agent needs.
pub struct Run<'a> {
    pub workflow: &'a Workflow,
    pub input: &'a str,
    pub delegation: &'a Delegation,
    pub model: &'a dyn agent::Model,
    pub policy: &'a Policy,
    /// The session's events; every step's own events are forwarded into it.
    pub tx: &'a mpsc::UnboundedSender<AgentEvent>,
    pub cancel: &'a Arc<AtomicBool>,
    pub children: &'a Children,
    /// The session's judge, which each step's own starts from.
    pub judge: Option<&'a Judge>,
    /// Where the step transcripts are written.
    pub transcripts: PathBuf,
    /// `<project>/.bhai`, which a caching workflow's step results go under.
    pub cache_root: PathBuf,
}

/// Run every step in dependency order, up to `max_parallel` at a time, until the steps
/// run out, one fails with `on_fail: stop`, or the budget is spent.
pub async fn run(run: Run<'_>) -> Report {
    let workflow = run.workflow;
    let mut report = Report {
        name: workflow.name.clone(),
        steps: Vec::new(),
        usage: Usage::default(),
        budget: workflow.budget_tokens,
        refused: None,
    };
    let identities = match resolve(run.delegation, &workflow.steps) {
        Ok(identities) => identities,
        Err(e) => {
            let _ = run.tx.send(AgentEvent::Error(format!("workflow: {e:#}")));
            report.refused = Some(format!("not started: {e:#}"));
            let _ = run.tx.send(AgentEvent::Info(report.text()));
            return report;
        }
    };
    let plan = plan(workflow);
    let _ = run.tx.send(AgentEvent::Info(plan.clone()));
    if !confirm(run.tx, plan).await {
        report.refused = Some("not started".to_string());
        let _ = run.tx.send(AgentEvent::Info(report.text()));
        return report;
    }

    let mut status: Vec<Option<Status>> = vec![None; workflow.steps.len()];
    let mut results: HashMap<String, String> = HashMap::new();
    let mut usage = vec![Usage::default(); workflow.steps.len()];
    let mut cached = vec![false; workflow.steps.len()];
    let cache = Cache::open(&run);
    // Once a wave holds a step the cache does not have, every later wave runs cold and
    // is not asked about. A changed result changes the prompts rendered after it, so
    // those keys would miss anyway; not looking makes the rule the code's rather than a
    // side effect of the keys. Steps in the same wave do not feed each other, so a miss
    // says nothing about its siblings and they are still allowed to hit.
    let mut cold = false;
    // Set once no more steps are launched, with the reason the rest did not run.
    let mut stopped: Option<String> = None;

    while let Some(wave) = next_wave(&workflow.steps, &status) {
        for (index, reason) in wave.blocked {
            status[index] = Some(Status::Skipped(reason));
        }
        let mut missed = false;
        for chunk in wave.ready.chunks(workflow.max_parallel) {
            if stopped.is_none() && run.cancel.load(Ordering::Relaxed) {
                stopped = Some("the run was interrupted".to_string());
            }
            if stopped.is_none() && spent(&report.usage) >= workflow.budget_tokens {
                stopped = Some(format!(
                    "the {} token budget was spent",
                    workflow.budget_tokens
                ));
            }
            if let Some(reason) = &stopped {
                for &index in chunk {
                    status[index] = Some(Status::Skipped(reason.clone()));
                }
                continue;
            }
            // The key, kept beside each launched step so the result can be stored under
            // it. A cold run still stores what it produces; it only stops reading.
            let mut launched: Vec<(usize, String, Option<String>)> = Vec::new();
            for &index in chunk {
                let step = &workflow.steps[index];
                let prompt = render(&step.prompt, run.input, &results);
                let Some(cache) = &cache else {
                    launched.push((index, prompt, None));
                    continue;
                };
                let key = key(step, &identities[index], &prompt);
                match (!cold).then(|| cache.read(&key)).flatten() {
                    Some(output) => {
                        replay(&run, step, &identities[index], &output);
                        results.insert(step.id.clone(), output);
                        status[index] = Some(Status::Ok);
                        cached[index] = true;
                    }
                    None => {
                        missed = true;
                        launched.push((index, prompt, Some(key)));
                    }
                }
            }
            let finished = join_all(launched.iter().map(|(index, prompt, _)| {
                step(&run, &workflow.steps[*index], &identities[*index], prompt)
            }))
            .await;
            for ((index, _, key), done) in launched.iter().zip(finished) {
                let step = &workflow.steps[*index];
                usage[*index] = done.usage;
                add(&mut report.usage, done.usage);
                match done.result {
                    Ok(text) => {
                        // Only on the way out of this arm, so a failed step is not
                        // stored and the next run retries it.
                        if let (Some(cache), Some(key)) = (&cache, key) {
                            cache.write(key, step, &identities[*index], &text);
                        }
                        results.insert(step.id.clone(), text);
                        status[*index] = Some(Status::Ok);
                    }
                    Err(e) => {
                        let reason = format!("{e:#}");
                        status[*index] = Some(Status::Failed(reason.clone()));
                        if step.on_fail == OnFail::Stop && stopped.is_none() {
                            stopped = Some(format!("step `{}` failed", step.id));
                        }
                    }
                }
            }
        }
        cold |= missed;
    }

    report.steps = workflow
        .steps
        .iter()
        .zip(status)
        .zip(usage)
        .zip(cached)
        .map(|(((step, status), usage), cached)| StepReport {
            id: step.id.clone(),
            identity: step.identity.clone(),
            status: status.unwrap_or(Status::Skipped("not reached".to_string())),
            usage,
            cached,
        })
        .collect();
    let _ = run.tx.send(AgentEvent::Info(report.text()));
    report
}

/// The identity each step runs as; `Err` when a step names one that is not defined.
fn resolve(delegation: &Delegation, steps: &[Step]) -> Result<Vec<Identity>> {
    let choices: Vec<Identity> = delegation
        .identities
        .iter()
        .filter(|i| i.name != identity::ROUTER)
        .cloned()
        .collect();
    steps
        .iter()
        .map(|step| {
            identity::find(&choices, &step.identity).with_context(|| format!("step `{}`", step.id))
        })
        .collect()
}

/// What the confirmation prompt shows before anything runs.
fn plan(workflow: &Workflow) -> String {
    let steps: Vec<String> = workflow
        .steps
        .iter()
        .map(|s| format!("{} ({})", s.id, s.identity))
        .collect();
    format!(
        "workflow {}: {} step(s) [{}], budget {} tokens, {} at a time{}",
        workflow.name,
        workflow.steps.len(),
        steps.join(", "),
        workflow.budget_tokens,
        workflow.max_parallel,
        caching(workflow)
    )
}

/// Ask the user through the session's approval path.
async fn confirm(tx: &mpsc::UnboundedSender<AgentEvent>, plan: String) -> bool {
    let (reply, wait) = oneshot::channel();
    let sent = tx.send(AgentEvent::Approval {
        tool: TOOL.to_string(),
        command: plan,
        offers: Offers::default(),
        reply,
    });
    sent.is_ok() && wait.await.is_ok_and(|answer| answer.accepted())
}

/// Steps that can be launched now, and steps whose dependencies did not finish.
struct Wave {
    ready: Vec<usize>,
    blocked: Vec<(usize, String)>,
}

/// The next wave, or `None` once every step has a status.
fn next_wave(steps: &[Step], status: &[Option<Status>]) -> Option<Wave> {
    let settled = |id: &str| {
        steps
            .iter()
            .position(|s| s.id == id)
            .and_then(|i| status[i].as_ref())
    };
    let mut wave = Wave {
        ready: Vec::new(),
        blocked: Vec::new(),
    };
    for (index, step) in steps.iter().enumerate() {
        if status[index].is_some() {
            continue;
        }
        let mut blocked = None;
        let mut waiting = false;
        for need in &step.needs {
            match settled(need) {
                Some(Status::Ok) => {}
                Some(_) => blocked = Some(format!("`{need}` did not finish")),
                None => waiting = true,
            }
        }
        match (blocked, waiting) {
            (Some(reason), _) => wave.blocked.push((index, reason)),
            (None, true) => {}
            (None, false) => wave.ready.push(index),
        }
    }
    (!wave.ready.is_empty() || !wave.blocked.is_empty()).then_some(wave)
}

/// Run one step as a child agent, with its own transcript entry.
async fn step(run: &Run<'_>, step: &Step, identity: &Identity, prompt: &str) -> agent::Finished {
    let id = uuid::Uuid::new_v4().simple().to_string()[..6].to_string();
    let _ = run.tx.send(AgentEvent::ToolStart {
        tool: crate::tools::agent::NAME.to_string(),
        summary: format!(
            "workflow {} step {} ({})",
            run.workflow.name, step.id, identity.name
        ),
    });
    let model = run.model.child(identity);
    let (_mailbox, steer) = agent::Mailbox::open(&run.delegation.mailboxes, &id);
    let finished = agent::run_child(Child {
        id: &id,
        description: &step.id,
        task: prompt,
        prompt: (run.delegation.prompt)(identity),
        model: model.as_ref(),
        policy: run.policy,
        tx: run.tx,
        cancel: run.cancel,
        transcript: Some(&run.transcripts.join(format!("child-{id}.jsonl"))),
        children: run.children,
        steer: Some(steer),
        judge: run.judge.map(|judge| judge.child(&id, prompt)),
    })
    .await;
    let output = match &finished.result {
        Ok(text) => tools::truncate(&tools::agent::sanitize(text)),
        Err(e) => format!("step {} failed: {e:#}", step.id),
    };
    let _ = run.tx.send(AgentEvent::ToolOutput(output));
    finished
}

/// Put a cached step through the transcript the way a run one goes, so a re-run reads
/// as the first one did.
fn replay(run: &Run<'_>, step: &Step, identity: &Identity, output: &str) {
    let _ = run.tx.send(AgentEvent::ToolStart {
        tool: crate::tools::agent::NAME.to_string(),
        summary: format!(
            "workflow {} step {} ({}), from cache",
            run.workflow.name, step.id, identity.name
        ),
    });
    let _ = run.tx.send(AgentEvent::ToolOutput(tools::truncate(
        &tools::agent::sanitize(output),
    )));
}

/// One step's stored result. The key is the file name; the rest is here so the file
/// says what it belongs to.
#[derive(Serialize, Deserialize)]
struct Cached {
    step: String,
    identity: String,
    saved: String,
    output: String,
}

/// A workflow's step results on disk, one file per key.
struct Cache {
    dir: PathBuf,
}

impl Cache {
    /// `None` when the workflow does not ask for a cache, or when the directory will
    /// not open. An empty `cache_root` is a caller that has none to give, and would
    /// otherwise scatter the cache through whatever the working directory happens to be.
    fn open(run: &Run<'_>) -> Option<Self> {
        if !run.workflow.cache {
            return None;
        }
        let dir = (!run.cache_root.as_os_str().is_empty()).then(|| {
            run.cache_root
                .join(CACHE_DIR)
                .join(slug(&run.workflow.name))
        });
        match dir.filter(|dir| std::fs::create_dir_all(dir).is_ok()) {
            Some(dir) => Some(Self { dir }),
            None => {
                let _ = run.tx.send(AgentEvent::Error(
                    "workflow: no directory to cache steps in, so every step will run".to_string(),
                ));
                None
            }
        }
    }

    /// What was stored under `key`, or `None` for a miss and for a file that will not
    /// read: an unusable cache runs the step.
    fn read(&self, key: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.dir.join(format!("{key}.json"))).ok()?;
        serde_json::from_str::<Cached>(&text).ok().map(|c| c.output)
    }

    fn write(&self, key: &str, step: &Step, identity: &Identity, output: &str) {
        let entry = Cached {
            step: step.id.clone(),
            identity: identity.name.clone(),
            saved: Local::now().to_rfc3339(),
            output: output.to_string(),
        };
        if let Ok(text) = serde_json::to_string_pretty(&entry) {
            let _ = std::fs::write(self.dir.join(format!("{key}.json")), text);
        }
    }
}

/// What a cached result is keyed on: the step's id, the prompt as it will be sent, and
/// the whole identity definition, so an edited agent file runs the step again.
fn key(step: &Step, identity: &Identity, prompt: &str) -> String {
    let identity = format!("{identity:?}");
    let mut hasher = Sha256::new();
    for part in [step.id.as_str(), prompt, identity.as_str()] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// A workflow name as one path component.
fn slug(name: &str) -> String {
    name.chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                true => c,
                false => '-',
            },
        )
        .collect()
}

/// Tokens a run has spent: what was sent plus what came back.
fn spent(usage: &Usage) -> u64 {
    usage.input + usage.output
}

fn add(total: &mut Usage, usage: Usage) {
    total.input += usage.input;
    total.cached += usage.cached;
    total.output += usage.output;
    total.reasoning += usage.reasoning;
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::agent::fake::{self, Fake, say};
    use crate::permissions::Answer;
    use crate::prompt::SystemPrompt;

    fn workflow(steps: &str) -> Workflow {
        parse(&format!("---\nname: w\n{steps}---\nprose"), "./test").unwrap()
    }

    fn delegation() -> Delegation {
        Delegation {
            identities: vec![Identity::default()],
            prompt: Arc::new(|identity: &Identity| SystemPrompt {
                identity: identity.clone(),
                ..SystemPrompt::default()
            }),
            sessions: PathBuf::new(),
            cache_root: PathBuf::new(),
            mailboxes: Default::default(),
        }
    }

    /// Accept every approval and collect the transcript events.
    async fn accept(mut rx: mpsc::UnboundedReceiver<AgentEvent>) -> Vec<String> {
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::Approval { command, reply, .. } => {
                    seen.push(format!("approval: {command}"));
                    let _ = reply.send(Answer::Accept(None));
                }
                AgentEvent::ToolStart { summary, .. } => seen.push(format!("start: {summary}")),
                AgentEvent::ToolOutput(s) => seen.push(format!("output: {s}")),
                AgentEvent::Info(s) => seen.push(format!("info: {s}")),
                AgentEvent::Error(s) => seen.push(format!("error: {s}")),
                _ => {}
            }
        }
        seen
    }

    async fn go(workflow: &Workflow, input: &str, fake: &Fake) -> (Report, Vec<String>, PathBuf) {
        let root = tools::temp_dir();
        let (report, seen) = go_in(&root, workflow, input, fake).await;
        (report, seen, root)
    }

    /// One run with its project directory given, so two runs share one cache.
    async fn go_in(
        root: &Path,
        workflow: &Workflow,
        input: &str,
        fake: &Fake,
    ) -> (Report, Vec<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let seen = tokio::spawn(accept(rx));
        let report = run(Run {
            workflow,
            input,
            delegation: &delegation(),
            model: fake,
            policy: &Policy::default(),
            tx: &tx,
            cancel: &Arc::new(AtomicBool::new(false)),
            children: &Children::default(),
            judge: None,
            transcripts: root.join("transcripts"),
            cache_root: root.to_path_buf(),
        })
        .await;
        drop(tx);
        (report, seen.await.unwrap())
    }

    fn statuses(report: &Report) -> Vec<(&str, &Status)> {
        report
            .steps
            .iter()
            .map(|s| (s.id.as_str(), &s.status))
            .collect()
    }

    #[tokio::test]
    async fn a_later_step_gets_the_earlier_step_output() {
        let workflow = workflow(
            "steps:\n  - id: a\n    prompt: look at {{input}}\n  - id: b\n    needs: [a]\n    \
prompt: review {{steps.a}}\n",
        );
        let fake = Fake::new(vec![vec![say("a said this")], vec![say("b done")]]);
        let (report, seen, dir) = go(&workflow, "the repo", &fake).await;
        assert_eq!(statuses(&report), [("a", &Status::Ok), ("b", &Status::Ok)]);
        let bodies = fake.bodies.lock().unwrap().clone();
        let inputs: Vec<String> = bodies.iter().map(|(_, b)| b["input"].to_string()).collect();
        assert!(inputs[0].contains("look at the repo"), "{inputs:?}");
        assert!(inputs[1].contains("review a said this"), "{inputs:?}");
        // Each step is its own pair of transcript entries, under one confirmation.
        assert_eq!(
            seen.iter().filter(|s| s.starts_with("approval: ")).count(),
            1
        );
        assert!(seen.contains(&"start: workflow w step b (general)".to_string()));
        assert!(seen.iter().any(|s| s == "output: a said this"));
        assert!(
            report.text().contains("a (general) ok"),
            "{}",
            report.text()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn independent_steps_run_together() {
        let workflow = workflow(
            "max_parallel: 2\nsteps:\n  - id: a\n    prompt: one\n  - id: b\n    prompt: two\n",
        );
        assert_eq!(workflow.max_parallel, 2);
        let fake = Fake::new(vec![vec![say("first")], vec![say("second")]]);
        let (report, _, dir) = go(&workflow, "", &fake).await;
        assert_eq!(statuses(&report), [("a", &Status::Ok), ("b", &Status::Ok)]);
        // Two children, each on its own conversation and cache key.
        let bodies = fake.bodies.lock().unwrap().clone();
        let mut keys: Vec<String> = bodies
            .iter()
            .map(|(c, b)| format!("{c} {}", b["prompt_cache_key"]))
            .collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), 2, "{keys:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_failing_step_stops_the_run_unless_it_says_continue() {
        let steps = |on_fail: &str| {
            format!(
                "steps:\n  - id: a\n    prompt: one\n    on_fail: {on_fail}\n  - id: b\n    prompt: two\n"
            )
        };
        let stop = workflow(&steps("stop"));
        let fake = Fake::new(vec![fake::step(fake::FAIL), vec![say("second")]]);
        let (report, _, dir) = go(&stop, "", &fake).await;
        assert_eq!(
            report.steps[0].status,
            Status::Failed("scripted failure".to_string())
        );
        assert_eq!(
            report.steps[1].status,
            Status::Skipped("step `a` failed".to_string())
        );
        let _ = std::fs::remove_dir_all(dir);

        let carry_on = workflow(&steps("continue"));
        let fake = Fake::new(vec![fake::step(fake::FAIL), vec![say("second")]]);
        let (report, _, dir) = go(&carry_on, "", &fake).await;
        assert!(matches!(report.steps[0].status, Status::Failed(_)));
        assert_eq!(report.steps[1].status, Status::Ok);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_step_that_needs_a_failed_step_never_runs() {
        let workflow = workflow(
            "steps:\n  - id: a\n    prompt: one\n    on_fail: continue\n  - id: b\n    \
needs: [a]\n    prompt: two {{steps.a}}\n",
        );
        let fake = Fake::new(vec![fake::step(fake::FAIL)]);
        let (report, _, dir) = go(&workflow, "", &fake).await;
        assert_eq!(
            report.steps[1].status,
            Status::Skipped("`a` did not finish".to_string())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_budget_stops_launching_steps() {
        // One step costs 12 tokens, so the second one is over the budget.
        let workflow = workflow(
            "budget_tokens: 11\nsteps:\n  - id: a\n    prompt: one\n  - id: b\n    prompt: two\n",
        );
        let fake = Fake::new(vec![vec![say("first")], vec![say("second")]]);
        let (report, _, dir) = go(&workflow, "", &fake).await;
        assert_eq!(report.steps[0].status, Status::Ok);
        assert_eq!(
            report.steps[1].status,
            Status::Skipped("the 11 token budget was spent".to_string())
        );
        assert_eq!(spent(&report.usage), 12);
        assert!(report.text().contains("10/2 tokens of a 11 budget"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_second_run_of_an_unchanged_workflow_calls_no_model() {
        let workflow = workflow(
            "cache: true\nsteps:\n  - id: a\n    prompt: look at {{input}}\n  - id: b\n    \
needs: [a]\n    prompt: review {{steps.a}}\n",
        );
        assert!(workflow.cache);
        let root = tools::temp_dir();
        let fake = Fake::new(vec![vec![say("a said this")], vec![say("b done")]]);
        let (first, _) = go_in(&root, &workflow, "the repo", &fake).await;
        assert_eq!(statuses(&first), [("a", &Status::Ok), ("b", &Status::Ok)]);
        assert!(first.steps.iter().all(|s| !s.cached));

        // An empty script, so any call the second run makes fails the step.
        let fake = Fake::new(Vec::new());
        let (again, seen) = go_in(&root, &workflow, "the repo", &fake).await;
        assert_eq!(statuses(&again), [("a", &Status::Ok), ("b", &Status::Ok)]);
        assert!(again.steps.iter().all(|s| s.cached));
        assert_eq!(spent(&again.usage), 0);
        assert!(fake.bodies.lock().unwrap().is_empty());
        assert!(
            seen.contains(&"start: workflow w step b (general), from cache".to_string()),
            "{seen:?}"
        );
        assert!(
            again.text().contains("a (general) ok, from cache"),
            "{}",
            again.text()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn without_a_cache_root_every_step_runs_and_the_run_says_so() {
        let workflow = workflow("cache: true\nsteps:\n  - id: a\n    prompt: one\n");
        let cwd = std::env::current_dir().unwrap();
        let root = tools::temp_dir();
        let (tx, rx) = mpsc::unbounded_channel();
        let seen = tokio::spawn(accept(rx));
        let fake = Fake::new(vec![vec![say("ran")]]);
        let report = run(Run {
            workflow: &workflow,
            input: "",
            delegation: &delegation(),
            model: &fake,
            policy: &Policy::default(),
            tx: &tx,
            cancel: &Arc::new(AtomicBool::new(false)),
            children: &Children::default(),
            judge: None,
            transcripts: root.join("transcripts"),
            cache_root: PathBuf::new(),
        })
        .await;
        drop(tx);
        assert_eq!(report.steps[0].status, Status::Ok);
        assert!(!report.steps[0].cached);
        assert_eq!(fake.bodies.lock().unwrap().len(), 1);
        let seen = seen.await.unwrap();
        assert!(
            seen.iter()
                .any(|line| line.contains("no directory to cache steps in")),
            "{seen:?}"
        );
        // Nothing was written beside the working directory either.
        assert!(!cwd.join(CACHE_DIR).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_changed_step_re_runs_every_step_after_it() {
        // `b`'s own prompt never changes, so only the rule keeps it from hitting.
        let workflow = workflow(
            "cache: true\nsteps:\n  - id: a\n    prompt: look at {{input}}\n  - id: b\n    \
needs: [a]\n    prompt: review it\n",
        );
        let root = tools::temp_dir();
        let fake = Fake::new(vec![vec![say("first a")], vec![say("first b")]]);
        go_in(&root, &workflow, "one repo", &fake).await;

        let fake = Fake::new(vec![vec![say("second a")], vec![say("second b")]]);
        let (again, _) = go_in(&root, &workflow, "another repo", &fake).await;
        assert!(again.steps.iter().all(|s| !s.cached), "{:?}", again.steps);
        let bodies = fake.bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2, "{bodies:?}");

        // The changed input is now the cached one, so running it again hits both steps.
        let fake = Fake::new(Vec::new());
        let (third, _) = go_in(&root, &workflow, "another repo", &fake).await;
        assert!(third.steps.iter().all(|s| s.cached), "{:?}", third.steps);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_failed_step_is_not_cached() {
        let workflow = workflow("cache: true\nsteps:\n  - id: a\n    prompt: one\n");
        let root = tools::temp_dir();
        let fake = Fake::new(vec![fake::step(fake::FAIL)]);
        let (first, _) = go_in(&root, &workflow, "", &fake).await;
        assert!(matches!(first.steps[0].status, Status::Failed(_)));

        let fake = Fake::new(vec![vec![say("worked this time")]]);
        let (again, _) = go_in(&root, &workflow, "", &fake).await;
        assert_eq!(again.steps[0].status, Status::Ok);
        assert!(!again.steps[0].cached);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn without_the_frontmatter_key_nothing_is_cached() {
        let workflow = workflow("steps:\n  - id: a\n    prompt: one\n");
        assert!(!workflow.cache);
        let root = tools::temp_dir();
        for _ in 0..2 {
            let fake = Fake::new(vec![vec![say("ran")]]);
            let (report, _) = go_in(&root, &workflow, "", &fake).await;
            assert_eq!(report.steps[0].status, Status::Ok);
            assert!(!report.steps[0].cached);
            assert_eq!(fake.bodies.lock().unwrap().len(), 1);
        }
        assert!(!root.join(CACHE_DIR).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_refused_confirmation_runs_nothing() {
        let workflow = workflow("steps:\n  - id: a\n    prompt: one\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let answer = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if let AgentEvent::Approval { reply, .. } = event {
                    let _ = reply.send(Answer::Reject);
                }
            }
        });
        let fake = Fake::default();
        let report = run(Run {
            workflow: &workflow,
            input: "",
            delegation: &delegation(),
            model: &fake,
            policy: &Policy::default(),
            tx: &tx,
            cancel: &Arc::new(AtomicBool::new(false)),
            children: &Children::default(),
            judge: None,
            transcripts: PathBuf::new(),
            cache_root: PathBuf::new(),
        })
        .await;
        drop(tx);
        answer.await.unwrap();
        assert_eq!(report.refused.as_deref(), Some("not started"));
        assert!(report.steps.is_empty());
        assert!(fake.bodies.lock().unwrap().is_empty());
    }

    #[test]
    fn a_cycle_and_an_unknown_placeholder_are_load_errors() {
        let cycle = parse(
            "---\nname: w\nsteps:\n  - id: a\n    needs: [b]\n    prompt: one\n  - id: b\n    \
needs: [a]\n    prompt: two\n---\n",
            "./test",
        )
        .unwrap_err();
        assert!(format!("{cycle:#}").contains("cycle: a, b"), "{cycle:#}");

        let unknown = parse(
            "---\nname: w\nsteps:\n  - id: a\n    prompt: one {{nope}}\n---\n",
            "./test",
        )
        .unwrap_err();
        assert!(
            format!("{unknown:#}").contains("step `a` uses unknown `{{nope}}`"),
            "{unknown:#}"
        );

        let undeclared = parse(
            "---\nname: w\nsteps:\n  - id: a\n    prompt: one\n  - id: b\n    prompt: {{steps.a}}\n---\n",
            "./test",
        )
        .unwrap_err();
        assert!(
            format!("{undeclared:#}").contains("does not need `a`"),
            "{undeclared:#}"
        );

        let missing = parse("---\nname: w\n---\n", "./test").unwrap_err();
        assert!(format!("{missing:#}").contains("no `steps`"), "{missing:#}");

        let cache = parse(
            "---\nname: w\ncache: sometimes\nsteps:\n  - id: a\n    prompt: one\n---\n",
            "./test",
        )
        .unwrap_err();
        assert!(
            format!("{cache:#}").contains("bad `cache` `sometimes`"),
            "{cache:#}"
        );
    }

    #[test]
    fn a_definition_takes_the_defaults_and_the_fan_out_cap() {
        let workflow = parse(
            "---\nname: review\ndescription: Look it over\nmax_parallel: 9\nsteps:\n  - id: a\n \
   identity: router\n    prompt: |\n      read {{input}}\n      then stop\n---\nprose here\n",
            "~/.config/bhai/workflows",
        )
        .unwrap();
        assert_eq!(workflow.budget_tokens, DEFAULT_BUDGET);
        assert_eq!(workflow.max_parallel, tools::agent::MAX_RUNNING);
        assert!(!workflow.cache);
        assert_eq!(workflow.steps[0].identity, "router");
        assert_eq!(workflow.steps[0].prompt, "read {{input}}\nthen stop");
        assert_eq!(workflow.steps[0].on_fail, OnFail::Stop);
        assert_eq!(workflow.body, "prose here");
        let found = Found {
            workflows: vec![Arc::new(workflow)],
            errors: vec!["skipped ./.bhai/workflows/bad.md: no `name`".to_string()],
        };
        let report = report(&found);
        assert!(
            report.contains("review (~/.config/bhai/workflows): 1 step(s)"),
            "{report}"
        );
        assert!(report.contains("Look it over"), "{report}");
        assert!(report.contains("  prose here"), "{report}");
        assert!(
            report.contains("skipped ./.bhai/workflows/bad.md"),
            "{report}"
        );
        assert!(find(&found.workflows, "nope").is_err());
        assert_eq!(find(&found.workflows, "review").unwrap().name, "review");
    }

    #[test]
    fn the_shipped_example_loads() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/workflows/review.md");
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let workflow = parse(&text, "./examples/workflows").unwrap();
        assert_eq!(workflow.name, "review");
        assert_eq!(workflow.max_parallel, 2);
        assert_eq!(workflow.steps[1].on_fail, OnFail::Continue);
        assert!(workflow.steps[2].prompt.contains("{{steps.diff}}"));
    }

    #[test]
    fn a_step_that_names_an_unknown_identity_stops_the_run() {
        let workflow = workflow("steps:\n  - id: a\n    identity: nope\n    prompt: one\n");
        let error = resolve(&delegation(), &workflow.steps).unwrap_err();
        assert!(
            format!("{error:#}").contains("step `a`: unknown identity `nope`"),
            "{error:#}"
        );
    }
}
