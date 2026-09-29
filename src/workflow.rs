//! Workflows: a handful of child agent steps run in dependency order under one token
//! budget. Definitions are markdown files with frontmatter, like identities. Only the
//! user starts one, with `/workflow` or `--workflow`; the model is never offered a
//! workflow tool, so it cannot spend the budget on its own.
//!
//! A step runs once, or once per item an earlier step listed (`for_each`), which is the
//! one part of the shape the file cannot fix, since the list is runtime data. A step
//! that declares `output: json` is held to answering with one object, and the steps
//! after it read its fields and gate themselves on them (`when`).

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
/// Characters of an item that fit in the label naming one instance of a fan-out.
const ITEM_LABEL: usize = 48;
/// Instances one `for_each` step may run unless the file says otherwise. The list is
/// runtime data, so a step that answers with a hundred lines is capped rather than
/// allowed to spend the budget on its own; the report says how many of the items ran.
const DEFAULT_FANOUT: u64 = 20;
/// What `max_fanout` may be raised to. The budget is the real backstop, but a run
/// still has a bound the definition cannot talk its way out of.
const MAX_FANOUT: u64 = 100;

/// What a step does when it fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnFail {
    /// Launch no more steps.
    Stop,
    /// Carry on with the steps that do not need this one.
    Continue,
}

/// What a step says it will answer with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// Whatever the child wrote, which the next step reads as prose.
    Text,
    /// One JSON object, whose fields the steps after it can read and branch on.
    Json,
}

/// A step's `when`: a placeholder, `==` or `!=`, and what it is compared to.
#[derive(Debug, Clone, PartialEq)]
pub struct When {
    /// The `{{...}}` name, without the braces.
    pub name: String,
    /// `==` rather than `!=`.
    pub equal: bool,
    pub value: String,
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
    /// Run this step once per item of the named step's output, with `{{item}}` filled
    /// in. The size of the fan-out is known only once that step has answered.
    pub for_each: Option<String>,
    /// What the step answers with; `Json` is checked, and its fields can be read.
    pub output: Output,
    /// Run the step only when this holds. It is checked once the steps it needs have
    /// answered, so a step gated on a field is skipped rather than run on a guess.
    pub when: Option<When>,
    /// Run this step on another model, over whatever its identity would use.
    pub model: Option<String>,
    /// Run it at this reasoning effort, over whatever its identity would use.
    pub effort: Option<String>,
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
    /// Instances one `for_each` step may run.
    pub max_fanout: usize,
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
    let max_fanout = number("max_fanout", DEFAULT_FANOUT)?.clamp(1, MAX_FANOUT);
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
        // The model is checked against the catalogue when the run starts, since what
        // the backends serve is not knowable from the file. The effort is: it is the
        // fixed list the API takes.
        let effort = value("effort");
        if let Some(effort) = &effort
            && !crate::client::EFFORTS.contains(&effort.as_str())
        {
            bail!(
                "step `{id}` has a bad `effort` `{effort}`; the API takes {}",
                crate::client::EFFORTS.join(", ")
            );
        }
        let output = match value("output").as_deref() {
            None | Some("text") => Output::Text,
            Some("json") => Output::Json,
            Some(other) => bail!("step `{id}` has a bad `output` `{other}`"),
        };
        let when = match value("when") {
            Some(text) => Some(condition(&text).with_context(|| format!("step `{id}`"))?),
            None => None,
        };
        steps.push(Step {
            id,
            identity: value("identity").unwrap_or_else(|| identity::DEFAULT.to_string()),
            prompt,
            needs: frontmatter::list(&item, "needs").unwrap_or_default(),
            on_fail,
            for_each: value("for_each"),
            output,
            when,
            model: value("model"),
            effort,
        });
    }
    if steps.is_empty() {
        bail!("no `steps`");
    }
    // Fanning out over a step is needing it, so the dependency is implied rather than
    // written twice; everything after this reads one list.
    for step in &mut steps {
        if let Some(over) = step.for_each.clone()
            && !step.needs.contains(&over)
        {
            step.needs.push(over);
        }
    }
    check(&steps)?;
    Ok(Workflow {
        name,
        description: value("description").unwrap_or_default(),
        source: source.to_string(),
        budget_tokens,
        max_parallel: max_parallel as usize,
        max_fanout: max_fanout as usize,
        cache,
        steps,
        body: body.trim_end().to_string(),
    })
}

/// A `when` line: `{{name}} == value` or `{{name}} != value`, with the value quoted or
/// bare. Compared as text, so `== true` and `== 42` read as they look.
fn condition(text: &str) -> Result<When> {
    let (left, equal, right) = match text.split_once("==") {
        Some((left, right)) => (left, true, right),
        None => match text.split_once("!=") {
            Some((left, right)) => (left, false, right),
            None => bail!("`when` `{text}` is not `{{{{...}}}} == value` or `!=`"),
        },
    };
    let name = left
        .trim()
        .strip_prefix("{{")
        .and_then(|name| name.strip_suffix("}}"))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .with_context(|| format!("`when` `{text}` does not start with a `{{{{...}}}}`"))?;
    let value = right.trim();
    let value = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(value);
    Ok(When {
        name: name.to_string(),
        equal,
        value: value.to_string(),
    })
}

/// What a `{{...}}` name refers to. Only top-level fields of a step's object are
/// reachable, so a name with a second dot in it is not one of these.
enum Ref<'a> {
    Input,
    Item,
    Step(&'a str),
    Field(&'a str, &'a str),
}

fn reference(name: &str) -> Option<Ref<'_>> {
    match name.strip_prefix("steps.") {
        None if name == "input" => Some(Ref::Input),
        None if name == "item" => Some(Ref::Item),
        None => None,
        Some(rest) => match rest.split_once('.') {
            None if rest.is_empty() => None,
            None => Some(Ref::Step(rest)),
            Some((id, field)) if !id.is_empty() && !field.is_empty() && !field.contains('.') => {
                Some(Ref::Field(id, field))
            }
            Some(_) => None,
        },
    }
}

/// Unique ids, dependencies that exist and are not circular, and placeholders that a
/// value will be there for.
fn check(steps: &[Step]) -> Result<()> {
    for (index, step) in steps.iter().enumerate() {
        if steps[..index].iter().any(|s| s.id == step.id) {
            bail!("two steps are called `{}`", step.id);
        }
        if let Some(over) = &step.for_each {
            let producer = steps.iter().find(|s| &s.id == over);
            match producer {
                _ if over == &step.id => bail!("step `{}` fans out over itself", step.id),
                None => bail!(
                    "step `{}` fans out over `{over}`, which is not a step",
                    step.id
                ),
                // Its output is the items and their results run together, so the lines
                // of it are not a list of anything.
                Some(producer) if producer.for_each.is_some() => bail!(
                    "step `{}` fans out over `{over}`, which fans out itself",
                    step.id
                ),
                Some(_) => {}
            }
            if !placeholders(&step.prompt).contains(&"item") {
                bail!(
                    "step `{}` has `for_each` but does not use `{{{{item}}}}`, so every \
instance would get the same prompt",
                    step.id
                );
            }
        }
        for need in &step.needs {
            if !steps.iter().any(|s| &s.id == need) {
                bail!("step `{}` needs `{need}`, which is not a step", step.id);
            }
        }
        for name in placeholders(&step.prompt) {
            reachable(step, steps, name)?;
        }
        if let Some(when) = &step.when {
            // It is read before the step's items exist, so it cannot be one of them.
            if when.name == "item" {
                bail!("step `{}` gates itself on `{{{{item}}}}`", step.id);
            }
            reachable(step, steps, &when.name)?;
        }
    }
    order(steps)?;
    Ok(())
}

/// Whether `name` is a placeholder this step will have a value for by the time it runs.
fn reachable(step: &Step, steps: &[Step], name: &str) -> Result<()> {
    let id = step.id.as_str();
    let named = |wanted: &str| steps.iter().find(|s| s.id == wanted);
    match reference(name) {
        Some(Ref::Input) => Ok(()),
        Some(Ref::Item) if step.for_each.is_some() => Ok(()),
        Some(Ref::Item) => bail!("step `{id}` uses `{{{{item}}}}` but has no `for_each`"),
        Some(Ref::Step(need)) | Some(Ref::Field(need, _))
            if !step.needs.iter().any(|n| n == need) =>
        {
            match named(need) {
                Some(_) => bail!("step `{id}` uses `{{{{{name}}}}}` but does not need `{need}`"),
                None => bail!("step `{id}` uses unknown `{{{{{name}}}}}`"),
            }
        }
        Some(Ref::Step(_)) => Ok(()),
        // A field is read off the one object a step answered with, so the step has to
        // have said it answers with one, and has to be a step that answers once.
        Some(Ref::Field(need, _)) => match named(need) {
            Some(producer) if producer.output != Output::Json => {
                bail!("step `{id}` uses `{{{{{name}}}}}` but `{need}` has no `output: json`")
            }
            Some(producer) if producer.for_each.is_some() => bail!(
                "step `{id}` uses `{{{{{name}}}}}` but `{need}` fans out, so it has no one object"
            ),
            _ => Ok(()),
        },
        None => bail!("step `{id}` uses unknown `{{{{{name}}}}}`"),
    }
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

/// What the steps that have finished left for the ones still to run.
#[derive(Default)]
struct Done {
    /// Each step's answer as the next prompt reads it.
    text: HashMap<String, String>,
    /// The object a step that declares `output: json` answered with.
    json: HashMap<String, serde_json::Map<String, serde_json::Value>>,
}

/// What `{{name}}` stands for now, or `None` when nothing has filled it: a step that
/// did not run, or a field its object does not have.
fn lookup(name: &str, input: &str, done: &Done, item: Option<&str>) -> Option<String> {
    match reference(name)? {
        Ref::Input => Some(input.to_string()),
        Ref::Item => item.map(str::to_string),
        Ref::Step(id) => done.text.get(id).cloned(),
        Ref::Field(id, field) => done.json.get(id)?.get(field).map(scalar),
    }
}

/// A JSON value as text: a string as itself, anything else as it is written, so a
/// `when` reads `== true` and `== 42` the way it looks.
fn scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// `text` with its placeholders filled in. A name with no value is left as it is;
/// `check` already refused the ones that could never be filled.
fn render(text: &str, input: &str, done: &Done, item: Option<&str>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        let name = after[..end].trim();
        out.push_str(&rest[..start]);
        match lookup(name, input, done, item) {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[start..start + 4 + end]),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Whether a step's `when` holds. `Err` names what was missing, for a step that is
/// skipped because the value it is gated on never arrived.
fn holds(when: &When, input: &str, done: &Done) -> Result<bool> {
    let found = lookup(&when.name, input, done, None).with_context(|| {
        format!(
            "`when` reads `{{{{{}}}}}`, which the step it needs did not answer with",
            when.name
        )
    })?;
    Ok((found == when.value) == when.equal)
}

/// The contents of a ``` fence when the whole text is one, since a model asked for a
/// list or an object tends to wrap it. Otherwise the text as it stands.
fn unfenced(text: &str) -> &str {
    let text = text.trim();
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let Some(body) = rest.split_once('\n').map(|(_, body)| body) else {
        return text;
    };
    match body.trim_end().strip_suffix("```") {
        Some(inner) => inner.trim(),
        None => text,
    }
}

/// The items a `for_each` step runs on. A JSON array is the list when the output is
/// one, so a step with nothing to list can say `[]`; otherwise the non-blank lines are
/// it, with a bullet or a number in front of one dropped, since that is how a model
/// writes a list even when the prompt asked for one item per line.
fn items(text: &str) -> Vec<String> {
    let text = unfenced(text);
    if let Ok(serde_json::Value::Array(values)) = serde_json::from_str::<serde_json::Value>(text) {
        return values
            .iter()
            .map(scalar)
            .filter(|item| !item.trim().is_empty())
            .collect();
    }
    text.lines()
        .map(|line| {
            let line = line.trim();
            let line = ["- ", "* ", "\u{2022} "]
                .iter()
                .find_map(|bullet| line.strip_prefix(bullet))
                .unwrap_or(line);
            let numbered = line
                .split_once(['.', ')'])
                .filter(|(head, _)| !head.is_empty() && head.chars().all(|c| c.is_ascii_digit()));
            match numbered {
                Some((_, rest)) => rest.trim().to_string(),
                None => line.to_string(),
            }
        })
        .filter(|line| !line.is_empty())
        .collect()
}

/// What a fan-out step leaves for the steps that need it: each item and what its
/// instance answered, so a step reducing them can tell which output came from which.
fn joined(outputs: &[(String, String)]) -> String {
    outputs
        .iter()
        .map(|(item, output)| format!("item: {item}\n{output}"))
        .collect::<Vec<String>>()
        .join("\n\n")
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
            "{} ({}): {} step(s), budget {} tokens, {} at a time{}{}",
            workflow.name,
            workflow.source,
            workflow.steps.len(),
            workflow.budget_tokens,
            workflow.max_parallel,
            match workflow.steps.iter().any(|s| s.for_each.is_some()) {
                true => format!(", up to {} items per fan-out", workflow.max_fanout),
                false => String::new(),
            },
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

/// One launch: a whole step, or one item of a `for_each` step.
struct Task {
    /// Its index in the workflow's steps.
    step: usize,
    item: Option<String>,
}

/// How one launch ended, before the launches of a step are folded into its `Status`.
#[derive(Clone)]
enum Outcome {
    Ok {
        output: String,
        /// The object it answered with, for a step that declares `output: json`.
        json: Option<serde_json::Map<String, serde_json::Value>>,
        cached: bool,
    },
    Failed(String),
    Skipped(String),
}

/// What a `for_each` step ran on: the instances that answered, of the items the step it
/// fans out over listed. They differ when an instance failed or the cap cut the list.
#[derive(Debug, Clone, PartialEq)]
pub struct Items {
    pub ok: usize,
    pub found: usize,
}

/// One step's line of the final report.
#[derive(Debug, Clone, PartialEq)]
pub struct StepReport {
    pub id: String,
    /// The identity, and the model when it is not the session's, as the plan named it.
    pub identity: String,
    pub status: Status,
    pub usage: Usage,
    /// Answered from the cache, so it cost nothing and ran no child.
    pub cached: bool,
    /// The step spent its whole budget, so its result is partial.
    pub truncated: bool,
    /// What a `for_each` step ran on; `None` for a step that runs once.
    pub items: Option<Items>,
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
                Status::Ok if step.truncated => "ok, step budget spent".to_string(),
                Status::Ok => "ok".to_string(),
                Status::Failed(e) => format!("failed: {e}"),
                Status::Skipped(reason) => format!("did not run: {reason}"),
            };
            let items = match &step.items {
                Some(Items { ok, found }) if ok == found => format!(", {found} items"),
                Some(Items { ok, found }) => format!(", {ok} of {found} items"),
                None => String::new(),
            };
            let _ = write!(
                out,
                "\n  {} ({}) {status}{items}, {}/{} tokens",
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
    // Only when a step names one: a run whose steps all take the session's model has
    // nothing to check, and asking the backends would cost it a round trip.
    if identities.iter().any(|i| i.model.is_some()) {
        let found = crate::models::cached(run.model.ollama_url(), run.model.name()).await;
        let bad = unserved(&found, &workflow.steps, &identities);
        if !bad.is_empty() {
            // The error line carries what the backends do serve; the report only says
            // which step named what, so a run refused over three of them stays short.
            for (id, model) in &bad {
                let _ = run.tx.send(AgentEvent::Error(format!(
                    "workflow: step `{id}`: {}",
                    crate::models::unknown(&found, model)
                )));
            }
            let named: Vec<String> = bad
                .iter()
                .map(|(id, model)| format!("step `{id}` names `{model}`"))
                .collect();
            report.refused = Some(format!(
                "not started: {}, which no backend here serves",
                named.join(", ")
            ));
            let _ = run.tx.send(AgentEvent::Info(report.text()));
            return report;
        }
    }
    let plan = plan(workflow, &identities);
    let _ = run.tx.send(AgentEvent::Info(plan.clone()));
    if !confirm(run.tx, plan).await {
        report.refused = Some("not started".to_string());
        let _ = run.tx.send(AgentEvent::Info(report.text()));
        return report;
    }

    let mut status: Vec<Option<Status>> = vec![None; workflow.steps.len()];
    let mut done = Done::default();
    let mut usage = vec![Usage::default(); workflow.steps.len()];
    let mut cached = vec![true; workflow.steps.len()];
    let mut truncated = vec![false; workflow.steps.len()];
    let mut counts: Vec<Option<Items>> = vec![None; workflow.steps.len()];
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
        // A `when` is read here, once the steps it names have answered and before
        // anything is launched for this one.
        let ready: Vec<usize> = wave
            .ready
            .into_iter()
            .filter(|&index| match &workflow.steps[index].when {
                None => true,
                Some(when) => match holds(when, run.input, &done) {
                    Ok(true) => true,
                    Ok(false) => {
                        status[index] = Some(Status::Skipped(format!(
                            "`when` did not hold: {{{{{}}}}} {} {}",
                            when.name,
                            match when.equal {
                                true => "==",
                                false => "!=",
                            },
                            when.value
                        )));
                        false
                    }
                    Err(e) => {
                        status[index] = Some(Status::Skipped(format!("{e:#}")));
                        false
                    }
                },
            })
            .collect();
        // A `for_each` step becomes one task per item of the step it fans out over.
        // That step is a dependency, so it has already answered: this is the first
        // point in the run where the size of the fan-out is known.
        let mut tasks: Vec<Task> = Vec::new();
        for &index in &ready {
            let Some(over) = &workflow.steps[index].for_each else {
                tasks.push(Task {
                    step: index,
                    item: None,
                });
                continue;
            };
            let found = items(done.text.get(over).map(String::as_str).unwrap_or_default());
            counts[index] = Some(Items {
                ok: 0,
                found: found.len(),
            });
            tasks.extend(
                found
                    .into_iter()
                    .take(workflow.max_fanout)
                    .map(|item| Task {
                        step: index,
                        item: Some(item),
                    }),
            );
        }
        let mut outcomes: Vec<Option<Outcome>> = vec![None; tasks.len()];
        let mut missed = false;
        let order: Vec<usize> = (0..tasks.len()).collect();
        for chunk in order.chunks(workflow.max_parallel) {
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
                for &task in chunk {
                    outcomes[task] = Some(Outcome::Skipped(reason.clone()));
                }
                continue;
            }
            // The key, kept beside each launched task so the result can be stored under
            // it. A cold run still stores what it produces; it only stops reading.
            let mut launched: Vec<(usize, String, Option<String>)> = Vec::new();
            for &task in chunk {
                let Task { step: index, item } = &tasks[task];
                let step = &workflow.steps[*index];
                let prompt = asked(
                    step,
                    render(&step.prompt, run.input, &done, item.as_deref()),
                );
                let Some(cache) = &cache else {
                    launched.push((task, prompt, None));
                    continue;
                };
                let key = key(step, &identities[*index], &prompt);
                match (!cold).then(|| cache.read(&key)).flatten() {
                    Some(output) => {
                        replay(&run, step, &identities[*index], item.as_deref(), &output);
                        outcomes[task] = Some(answered(step, output, true));
                    }
                    None => {
                        missed = true;
                        launched.push((task, prompt, Some(key)));
                    }
                }
            }
            let finished = join_all(launched.iter().map(|(task, prompt, _)| {
                let Task { step: index, item } = &tasks[*task];
                step(
                    &run,
                    &workflow.steps[*index],
                    &identities[*index],
                    item.as_deref(),
                    prompt,
                )
            }))
            .await;
            for ((task, _, key), answer) in launched.iter().zip(finished) {
                let index = tasks[*task].step;
                let step = &workflow.steps[index];
                add(&mut usage[index], answer.usage);
                truncated[index] |= answer.truncated;
                add(&mut report.usage, answer.usage);
                let outcome = match answer.result {
                    Ok(text) => answered(step, text, false),
                    Err(e) => Outcome::Failed(format!("{e:#}")),
                };
                // Only what the step said it would answer with, so neither a failure
                // nor an answer of the wrong shape is stored and the next run asks
                // again. A step that spent its budget is not stored either: what it
                // answered with is what it had when it was cut off, and a cache would
                // hand that back for good.
                if let (Some(cache), Some(key), false, Outcome::Ok { output, .. }) =
                    (&cache, key, answer.truncated, &outcome)
                {
                    cache.write(key, step, &identities[index], output);
                }
                if matches!(outcome, Outcome::Failed(_))
                    && step.on_fail == OnFail::Stop
                    && stopped.is_none()
                {
                    stopped = Some(format!("step `{}` failed", step.id));
                }
                outcomes[*task] = Some(outcome);
            }
        }
        cold |= missed;
        // A step's instances are all in this one wave, so its result is whole here and
        // nowhere earlier. Folding them here is also what keeps a fan-out split across
        // chunks from settling before its later chunks have run.
        for &index in &ready {
            let step = &workflow.steps[index];
            let mine: Vec<&Outcome> = tasks
                .iter()
                .zip(&outcomes)
                .filter(|(task, _)| task.step == index)
                .filter_map(|(_, outcome)| outcome.as_ref())
                .collect();
            let ok: Vec<(String, String)> = tasks
                .iter()
                .zip(&outcomes)
                .filter(|(task, _)| task.step == index)
                .filter_map(|(task, outcome)| match outcome {
                    Some(Outcome::Ok { output, .. }) => Some((
                        task.item.clone().unwrap_or_else(|| step.id.clone()),
                        output.clone(),
                    )),
                    _ => None,
                })
                .collect();
            if let Some(count) = &mut counts[index] {
                count.ok = ok.len();
            }
            // Only a step every instance of which was answered from the cache is
            // reported as cached: one that ran even a single child did not come free.
            cached[index] = !mine.is_empty()
                && mine
                    .iter()
                    .all(|o| matches!(o, Outcome::Ok { cached: true, .. }));
            let failed = mine.iter().find_map(|o| match o {
                Outcome::Failed(reason) => Some(reason.clone()),
                _ => None,
            });
            let skipped = mine.iter().find_map(|o| match o {
                Outcome::Skipped(reason) => Some(reason.clone()),
                _ => None,
            });
            status[index] = Some(match (ok.is_empty(), failed, skipped) {
                // A fan-out over a step that listed nothing. Nothing ran and nothing
                // failed, so the steps that need it read an empty result and carry on.
                (true, None, None) => {
                    done.text.insert(step.id.clone(), String::new());
                    Status::Ok
                }
                (true, None, Some(reason)) => Status::Skipped(reason),
                (true, Some(reason), _) => Status::Failed(reason),
                // At least one instance answered. Under `on_fail: continue` the ones
                // that did not are simply missing from what the next step reads; under
                // `stop` nothing more was launched anyway.
                (false, _, _) => {
                    let output = match step.for_each.is_some() {
                        true => joined(&ok),
                        false => ok[0].1.clone(),
                    };
                    // A step that answers once and with an object leaves its fields for
                    // the steps after it to read and to gate themselves on; a fan-out
                    // has one object per item, so it leaves only the text.
                    if step.for_each.is_none()
                        && let Some(Outcome::Ok {
                            json: Some(json), ..
                        }) = mine.first()
                    {
                        done.json.insert(step.id.clone(), json.clone());
                    }
                    done.text.insert(step.id.clone(), output);
                    Status::Ok
                }
            });
        }
    }

    report.steps = workflow
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| StepReport {
            id: step.id.clone(),
            identity: running_as(&step.identity, &identities[index]),
            status: status[index]
                .clone()
                .unwrap_or(Status::Skipped("not reached".to_string())),
            usage: usage[index],
            cached: cached[index],
            truncated: truncated[index],
            items: counts[index].clone(),
        })
        .collect();
    let _ = run.tx.send(AgentEvent::Info(report.text()));
    report
}

/// The identity each step runs as; `Err` when a step names one that is not defined. A
/// step's own `model` and `effort` win over the identity's, which are only its defaults,
/// so one identity can serve steps that want different models.
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
            let mut identity = identity::find(&choices, &step.identity)
                .with_context(|| format!("step `{}`", step.id))?;
            identity.model = step.model.clone().or(identity.model);
            identity.effort = step.effort.clone().or(identity.effort);
            Ok(identity)
        })
        .collect()
}

/// Each step whose model no backend here serves, with that model. A backend that could
/// not be asked answers for nothing, so a run is never refused on a list that never
/// loaded.
fn unserved<'a>(
    found: &crate::models::Catalogue,
    steps: &'a [Step],
    identities: &'a [Identity],
) -> Vec<(&'a str, &'a str)> {
    steps
        .iter()
        .zip(identities)
        .filter_map(|(step, identity)| {
            let model = identity.model.as_deref()?;
            (!crate::models::serves(found, model)).then_some((step.id.as_str(), model))
        })
        .collect()
}

/// What the confirmation prompt shows before anything runs.
fn plan(workflow: &Workflow, identities: &[Identity]) -> String {
    let steps: Vec<String> = workflow
        .steps
        .iter()
        .zip(identities)
        .map(|(s, i)| {
            let over = match &s.for_each {
                Some(over) => format!(", one per item of {over}"),
                None => String::new(),
            };
            let when = match &s.when {
                Some(when) => format!(
                    ", when {{{{{}}}}} {} {}",
                    when.name,
                    match when.equal {
                        true => "==",
                        false => "!=",
                    },
                    when.value
                ),
                None => String::new(),
            };
            format!("{} ({}{over}{when})", s.id, running_as(&s.identity, i))
        })
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

/// How a step is named in the plan and the report: its identity, and the model it runs
/// on when that is not simply the session's.
fn running_as(identity: &str, resolved: &Identity) -> String {
    match &resolved.model {
        Some(model) => format!("{identity} on {model}"),
        None => identity.to_string(),
    }
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

/// The prompt as the child is given it: a step that declares `output: json` asks for
/// one, since the run refuses an answer that is not one and would waste the call.
fn asked(step: &Step, prompt: String) -> String {
    match step.output {
        Output::Text => prompt,
        Output::Json => format!("{prompt}\n\nAnswer with one JSON object and nothing else."),
    }
}

/// One launch's answer, held against what the step said it would answer with.
fn answered(step: &Step, output: String, cached: bool) -> Outcome {
    if step.output == Output::Text {
        return Outcome::Ok {
            output,
            json: None,
            cached,
        };
    }
    match serde_json::from_str(unfenced(&output)) {
        Ok(serde_json::Value::Object(json)) => Outcome::Ok {
            output,
            json: Some(json),
            cached,
        },
        _ => Outcome::Failed("did not answer with one JSON object".to_string()),
    }
}

/// Run one step, or one instance of a fan-out, as a child agent with its own
/// transcript entry.
async fn step(
    run: &Run<'_>,
    step: &Step,
    identity: &Identity,
    item: Option<&str>,
    prompt: &str,
) -> agent::Finished {
    let id = uuid::Uuid::new_v4().simple().to_string()[..6].to_string();
    let _ = run.tx.send(AgentEvent::ToolStart {
        tool: crate::tools::agent::NAME.to_string(),
        summary: format!(
            "workflow {} step {} ({})",
            run.workflow.name,
            label(&step.id, item),
            identity.name
        ),
    });
    let model = run.model.child(identity);
    let (_mailbox, steer) = agent::Mailbox::open(&run.delegation.mailboxes, &id);
    let finished = agent::run_child(Child {
        id: &id,
        description: &label(&step.id, item),
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
    let _ = run
        .tx
        .send(AgentEvent::ToolOutput(attributed(item, output)));
    finished
}

/// How a step is named while it runs: its id, and the item when it is one instance of
/// a fan-out, cut short so a long item does not take the line over.
fn label(id: &str, item: Option<&str>) -> String {
    match item {
        Some(item) => format!("{id} [{}]", short(item)),
        None => id.to_string(),
    }
}

/// An item on one line and short enough to read: cut in the middle, not at the end,
/// since what tells two items apart is as often their tail as their head. Four paths
/// under one directory are four of the same label when the tail is what goes.
fn short(item: &str) -> String {
    let flat: String = item
        .chars()
        .map(|c| match c.is_control() {
            true => ' ',
            false => c,
        })
        .collect();
    let chars: Vec<char> = flat.trim().chars().collect();
    if chars.len() <= ITEM_LABEL {
        return chars.into_iter().collect();
    }
    let head = (ITEM_LABEL - 1) / 2;
    let tail = ITEM_LABEL - 1 - head;
    format!(
        "{}…{}",
        chars[..head].iter().collect::<String>(),
        chars[chars.len() - tail..].iter().collect::<String>()
    )
}

/// An instance's answer with the item it is for at the head of it. Instances finish in
/// whatever order they finish and the event pair carries no id, so without this the
/// answers of a fan-out arrive under one another's headers.
fn attributed(item: Option<&str>, output: String) -> String {
    match item {
        Some(item) => format!("[{}]\n{output}", short(item)),
        None => output,
    }
}

/// Put a cached step through the transcript the way a run one goes, so a re-run reads
/// as the first one did.
fn replay(run: &Run<'_>, step: &Step, identity: &Identity, item: Option<&str>, output: &str) {
    let _ = run.tx.send(AgentEvent::ToolStart {
        tool: crate::tools::agent::NAME.to_string(),
        summary: format!(
            "workflow {} step {} ({}), from cache",
            run.workflow.name,
            label(&step.id, item),
            identity.name
        ),
    });
    let _ = run.tx.send(AgentEvent::ToolOutput(attributed(
        item,
        tools::truncate(&tools::agent::sanitize(output)),
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
        match dir.filter(|dir| crate::sessions::private_dir(dir).is_ok()) {
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
            let _ = crate::sessions::private_write(&self.dir.join(format!("{key}.json")), &text);
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
    async fn a_step_that_spends_its_budget_says_so_and_is_not_cached() {
        let workflow = workflow("cache: true\nsteps:\n  - id: a\n    prompt: one\n");
        let root = tools::temp_dir();
        let mut script = vec![
            vec![fake::call(
                "read",
                serde_json::json!({"path": "/etc/hosts"})
            )];
            agent::CHILD_STEPS - 1
        ];
        script.push(vec![say("what I had so far")]);
        let fake = Fake::new(script);
        let (report, _) = go_in(&root, &workflow, "", &fake).await;
        assert_eq!(report.steps[0].status, Status::Ok);
        assert!(report.steps[0].truncated);
        assert!(
            report.text().contains("a (general) ok, step budget spent"),
            "{}",
            report.text()
        );

        // A partial result is not what a re-run hands back.
        let fake = Fake::new(vec![vec![say("the whole answer")]]);
        let (again, _) = go_in(&root, &workflow, "", &fake).await;
        assert_eq!(again.steps[0].status, Status::Ok);
        assert!(!again.steps[0].cached);
        assert!(!again.steps[0].truncated);
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
        // Both caps are clamped, so a definition cannot talk its way past either.
        assert_eq!(workflow.max_fanout, DEFAULT_FANOUT as usize);
        let greedy = parse(
            "---\nname: w\nmax_fanout: 5000\nsteps:\n  - id: a\n    prompt: one\n---\n",
            "./test",
        )
        .unwrap();
        assert_eq!(greedy.max_fanout, MAX_FANOUT as usize);
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
    fn the_shipped_examples_load() {
        let read = |name: &str| {
            let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/workflows/");
            std::fs::read_to_string(format!("{dir}{name}"))
        };
        let Ok(text) = read("review.md") else {
            return;
        };
        let workflow = parse(&text, "./examples/workflows").unwrap();
        assert_eq!(workflow.name, "review");
        assert_eq!(workflow.max_parallel, 2);
        assert_eq!(workflow.steps[1].on_fail, OnFail::Continue);
        assert!(workflow.steps[2].prompt.contains("{{steps.diff}}"));

        let workflow = parse(&read("review-each.md").unwrap(), "./examples/workflows").unwrap();
        assert_eq!(workflow.name, "review-each");
        assert_eq!(workflow.steps[0].effort.as_deref(), Some("low"));
        assert_eq!(workflow.steps[1].for_each.as_deref(), Some("files"));
        assert_eq!(workflow.steps[1].needs, ["files"]);
        assert_eq!(workflow.steps[0].identity, "worker");
        assert_eq!(workflow.steps[2].output, Output::Json);
        // The lean identity the example points at, which is what keeps a fan-out from
        // paying for the instruction files and skills once per item.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/agents/worker.md");
        let text = std::fs::read_to_string(path).unwrap();
        let worker = identity::parse(&text, Path::new(path), "./examples/agents").unwrap();
        assert_eq!(worker.instructions, Some(Vec::new()));
        assert_eq!(worker.skills, ["!*"]);
        assert_eq!(worker.tools.unwrap(), ["bash", "read"]);
        assert_eq!(
            workflow.steps[3].when.as_ref().map(|w| w.name.as_str()),
            Some("steps.verdict.blocking")
        );
    }

    /// A step's own model and effort beat the identity's, which are only its defaults,
    /// and the plan says what each step will run on before the user approves it.
    #[test]
    fn a_step_can_run_on_a_model_of_its_own() {
        let workflow = workflow(
            "steps:\n  - id: a\n    prompt: one\n  - id: b\n    model: ollama:gemma4:e4b\n    \
effort: low\n    prompt: two\n",
        );
        assert_eq!(workflow.steps[0].model, None);
        assert_eq!(
            workflow.steps[1].model.as_deref(),
            Some("ollama:gemma4:e4b")
        );
        assert_eq!(workflow.steps[1].effort.as_deref(), Some("low"));
        let identities = resolve(&delegation(), &workflow.steps).unwrap();
        assert_eq!(identities[0].model, None);
        assert_eq!(identities[1].model.as_deref(), Some("ollama:gemma4:e4b"));
        assert_eq!(identities[1].effort.as_deref(), Some("low"));
        let plan = plan(&workflow, &identities);
        assert!(
            plan.contains("[a (general), b (general on ollama:gemma4:e4b)]"),
            "{plan}"
        );

        let bad = parse(
            "---\nname: w\nsteps:\n  - id: a\n    effort: whenever\n    prompt: one\n---\n",
            "./test",
        )
        .unwrap_err();
        assert!(
            format!("{bad:#}").contains("step `a` has a bad `effort` `whenever`"),
            "{bad:#}"
        );
    }

    /// The catalogue is asked once, before the confirmation. A backend that could not be
    /// asked answers for nothing, so its models are not refused on a list it is not on.
    #[test]
    fn a_model_no_backend_serves_is_named_before_the_run_starts() {
        use crate::models::{Catalogue, Model as Listed};
        let listed = |id: &str| Listed {
            id: id.to_string(),
            label: id.to_string(),
            detail: String::new(),
            efforts: Vec::new(),
            default_effort: None,
            window: None,
        };
        let workflow = workflow(
            "steps:\n  - id: a\n    model: gpt-5.6-sol\n    prompt: one\n  - id: b\n    \
model: ollama:nope\n    prompt: two\n",
        );
        let identities = resolve(&delegation(), &workflow.steps).unwrap();
        let both = Catalogue {
            models: vec![listed("gpt-5.6-sol")],
            notes: Vec::new(),
        };
        assert_eq!(
            unserved(&both, &workflow.steps, &identities),
            [("b", "ollama:nope")]
        );

        let ollama_down = Catalogue {
            models: vec![listed("gpt-5.6-sol")],
            notes: vec!["ollama: connection refused".to_string()],
        };
        assert!(unserved(&ollama_down, &workflow.steps, &identities).is_empty());
    }

    /// The size of the fan-out is the earlier step's answer, so it is known only once
    /// that step has run. Each instance is its own child, and the step that reduces
    /// them reads every output labelled with the item it came from.
    #[tokio::test]
    async fn a_step_runs_once_per_item_of_the_step_it_fans_out_over() {
        let workflow = workflow(
            "steps:\n  - id: find\n    prompt: list the files in {{input}}\n  - id: review\n    \
for_each: find\n    prompt: review {{item}}\n  - id: sum\n    needs: [review]\n    \
prompt: summarise {{steps.review}}\n",
        );
        // `for_each` is a dependency, so it does not have to be written twice.
        assert_eq!(workflow.steps[1].needs, ["find"]);
        let fake = Fake::new(vec![
            vec![say("- a.rs\n2. b.rs")],
            vec![say("a.rs is fine")],
            vec![say("b.rs is not")],
            vec![say("one of two is fine")],
        ]);
        let (report, _, dir) = go(&workflow, "the repo", &fake).await;
        assert_eq!(
            statuses(&report),
            [
                ("find", &Status::Ok),
                ("review", &Status::Ok),
                ("sum", &Status::Ok)
            ]
        );
        assert_eq!(report.steps[1].items, Some(Items { ok: 2, found: 2 }));
        assert_eq!(report.steps[0].items, None);
        assert!(
            report.text().contains("review (general) ok, 2 items"),
            "{}",
            report.text()
        );
        let bodies = fake.bodies.lock().unwrap().clone();
        let inputs: Vec<String> = bodies.iter().map(|(_, b)| b["input"].to_string()).collect();
        // A bullet and a number in front of an item are how a model writes a list.
        assert!(inputs[1].contains("review a.rs"), "{:?}", inputs[1]);
        assert!(inputs[2].contains("review b.rs"), "{:?}", inputs[2]);
        // The reducer reads every instance, each under the item it ran on.
        let last = &inputs[3];
        assert!(last.contains("item: a.rs\\na.rs is fine"), "{last}");
        assert!(last.contains("item: b.rs\\nb.rs is not"), "{last}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A step with nothing to list says so with `[]`. Nothing ran and nothing failed,
    /// so the steps that need it read an empty result rather than being blocked.
    #[tokio::test]
    async fn a_fan_out_over_an_empty_list_runs_nothing_and_blocks_nothing() {
        let workflow = workflow(
            "steps:\n  - id: find\n    prompt: list what changed\n  - id: review\n    \
for_each: find\n    prompt: review {{item}}\n  - id: sum\n    needs: [review]\n    \
prompt: summarise {{steps.review}}\n",
        );
        let fake = Fake::new(vec![
            vec![say("```json\n[]\n```")],
            vec![say("nothing to do")],
        ]);
        let (report, _, dir) = go(&workflow, "", &fake).await;
        assert_eq!(
            statuses(&report),
            [
                ("find", &Status::Ok),
                ("review", &Status::Ok),
                ("sum", &Status::Ok)
            ]
        );
        assert_eq!(report.steps[1].items, Some(Items { ok: 0, found: 0 }));
        assert!(
            report.text().contains("review (general) ok, 0 items"),
            "{}",
            report.text()
        );
        // Two calls, not three: the fan-out ran no child at all.
        assert_eq!(fake.bodies.lock().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The list is runtime data, so the cap is what stops a step that answers with a
    /// hundred lines from spending the budget on its own. The report says it was cut,
    /// and a workflow that wants more of them says so.
    #[tokio::test]
    async fn a_fan_out_is_capped_and_the_report_says_how_many_ran() {
        let workflow = workflow(
            "max_fanout: 3\nsteps:\n  - id: find\n    prompt: list them\n  - id: review\n    \
for_each: find\n    prompt: review {{item}}\n",
        );
        assert_eq!(workflow.max_fanout, 3);
        let listed: Vec<String> = (0..5).map(|n| format!("file{n}.rs")).collect();
        let mut script = vec![vec![say(&listed.join("\n"))]];
        script.extend(vec![vec![say("looked")]; 3]);
        let fake = Fake::new(script);
        let (report, _, dir) = go(&workflow, "", &fake).await;
        assert_eq!(report.steps[1].items, Some(Items { ok: 3, found: 5 }));
        assert!(
            report.text().contains("review (general) ok, 3 of 5 items"),
            "{}",
            report.text()
        );
        assert_eq!(fake.bodies.lock().unwrap().len(), 4);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// One instance failing is the step failing, under the step's own `on_fail`: with
    /// `continue` the run carries on and the item is simply missing from what the next
    /// step reads.
    #[tokio::test]
    async fn a_failed_instance_leaves_the_rest_of_the_fan_out_standing() {
        let workflow = workflow(
            "steps:\n  - id: find\n    prompt: list them\n  - id: review\n    for_each: find\n    \
on_fail: continue\n    prompt: review {{item}}\n  - id: sum\n    needs: [review]\n    \
prompt: summarise {{steps.review}}\n",
        );
        let fake = Fake::new(vec![
            vec![say("a.rs\nb.rs")],
            fake::step(fake::FAIL),
            vec![say("b.rs is fine")],
            vec![say("one of two")],
        ]);
        let (report, _, dir) = go(&workflow, "", &fake).await;
        assert_eq!(
            statuses(&report),
            [
                ("find", &Status::Ok),
                ("review", &Status::Ok),
                ("sum", &Status::Ok)
            ]
        );
        assert_eq!(report.steps[1].items, Some(Items { ok: 1, found: 2 }));
        let bodies = fake.bodies.lock().unwrap().clone();
        let last = bodies.last().unwrap().1["input"].to_string();
        assert!(last.contains("item: b.rs"), "{last}");
        assert!(!last.contains("item: a.rs"), "{last}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_fan_out_that_could_not_work_is_a_load_error() {
        let bad = |steps: &str| {
            format!(
                "{:#}",
                parse(&format!("---\nname: w\n{steps}---\n"), "./t").unwrap_err()
            )
        };
        assert!(
            bad("steps:\n  - id: a\n    for_each: nope\n    prompt: do {{item}}\n")
                .contains("step `a` fans out over `nope`, which is not a step"),
        );
        assert!(
            bad("steps:\n  - id: a\n    for_each: a\n    prompt: do {{item}}\n")
                .contains("step `a` fans out over itself"),
        );
        assert!(
            bad(
                "steps:\n  - id: a\n    prompt: one\n  - id: b\n    for_each: a\n    prompt: two\n"
            )
            .contains("step `b` has `for_each` but does not use `{{item}}`"),
        );
        assert!(
            bad("steps:\n  - id: a\n    prompt: do {{item}}\n")
                .contains("step `a` uses `{{item}}` but has no `for_each`"),
        );
        assert!(
            bad(
                "steps:\n  - id: a\n    prompt: one\n  - id: b\n    for_each: a\n    \
prompt: {{item}}\n  - id: c\n    for_each: b\n    prompt: {{item}}\n"
            )
            .contains("step `c` fans out over `b`, which fans out itself"),
        );
    }

    /// Four paths under one directory are four of the same label if the tail is what
    /// goes, and the answers of a fan-out arrive under one another's headers without
    /// the item on them.
    #[test]
    fn an_instance_is_named_by_what_tells_it_apart() {
        assert_eq!(label("review", None), "review");
        assert_eq!(label("review", Some("src/ui.rs")), "review [src/ui.rs]");
        let one = "/Users/me/workspace/opensource/personal/bhai/src/ui.rs";
        let two = "/Users/me/workspace/opensource/personal/bhai/src/agent.rs";
        assert_ne!(label("review", Some(one)), label("review", Some(two)));
        assert!(label("review", Some(one)).ends_with("src/ui.rs]"));
        assert_eq!(short(one).chars().count(), ITEM_LABEL);
        // An item out of a JSON array can carry a line break; a label is one line.
        assert_eq!(short("a\nb"), "a b");
        assert_eq!(attributed(None, "done".to_string()), "done");
        assert_eq!(
            attributed(Some("src/ui.rs"), "done".to_string()),
            "[src/ui.rs]\ndone"
        );
    }

    /// A list is a list however the model wrote it, but a JSON array is taken whole, so
    /// an item with a line break or a bullet in it survives.
    #[test]
    fn a_list_is_read_as_lines_unless_it_is_json() {
        assert_eq!(items("a.rs\n\n  b.rs  \n"), ["a.rs", "b.rs"]);
        assert_eq!(
            items("- a.rs\n* b.rs\n1. c.rs\n2) d.rs"),
            ["a.rs", "b.rs", "c.rs", "d.rs"]
        );
        // Not a bullet and not a number: a flag and a file keep their punctuation.
        assert_eq!(items("-Xmx2g\nsrc/main.rs"), ["-Xmx2g", "src/main.rs"]);
        assert_eq!(items("[\"a b\", \"c\"]"), ["a b", "c"]);
        assert_eq!(items("```json\n[]\n```"), Vec::<String>::new());
        assert!(items("   \n  \n").is_empty());
    }

    /// A step that says it answers with an object has its answer held to that, and the
    /// steps after it read its fields and gate themselves on them.
    #[tokio::test]
    async fn a_step_can_read_and_gate_on_a_field_of_an_earlier_answer() {
        let workflow = workflow(
            "steps:\n  - id: triage\n    output: json\n    prompt: triage {{input}}\n  \
- id: fix\n    needs: [triage]\n    when: \"{{steps.triage.risky}} == true\"\n    \
prompt: fix the {{steps.triage.area}}\n",
        );
        assert_eq!(workflow.steps[0].output, Output::Json);
        assert_eq!(
            workflow.steps[1].when,
            Some(When {
                name: "steps.triage.risky".to_string(),
                equal: true,
                value: "true".to_string()
            })
        );
        let identities = resolve(&delegation(), &workflow.steps).unwrap();
        let plan = plan(&workflow, &identities);
        assert!(
            plan.contains("fix (general, when {{steps.triage.risky}} == true)"),
            "{plan}"
        );

        let fake = Fake::new(vec![
            vec![say(
                "```json\n{\"risky\": true, \"area\": \"the parser\"}\n```",
            )],
            vec![say("fixed it")],
        ]);
        let (report, _, dir) = go(&workflow, "the diff", &fake).await;
        assert_eq!(
            statuses(&report),
            [("triage", &Status::Ok), ("fix", &Status::Ok)]
        );
        let bodies = fake.bodies.lock().unwrap().clone();
        let inputs: Vec<String> = bodies.iter().map(|(_, b)| b["input"].to_string()).collect();
        // The step asks for what it says it answers with, so the call is not wasted.
        assert!(
            inputs[0].contains("Answer with one JSON object and nothing else."),
            "{:?}",
            inputs[0]
        );
        assert!(inputs[1].contains("fix the the parser"), "{:?}", inputs[1]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The gate is read once the step it names has answered. A step it does not hold
    /// for is skipped, and the run carries on rather than stopping.
    #[tokio::test]
    async fn a_gate_that_does_not_hold_skips_only_its_own_step() {
        let workflow = workflow(
            "steps:\n  - id: triage\n    output: json\n    prompt: triage it\n  - id: fix\n    \
needs: [triage]\n    when: {{steps.triage.risky}} == true\n    prompt: fix {{steps.triage.area}}\n  \
- id: note\n    needs: [triage]\n    prompt: write it down\n",
        );
        let fake = Fake::new(vec![
            vec![say("{\"risky\": false, \"area\": \"none\"}")],
            vec![say("written")],
        ]);
        let (report, _, dir) = go(&workflow, "", &fake).await;
        assert_eq!(report.steps[0].status, Status::Ok);
        assert_eq!(
            report.steps[1].status,
            Status::Skipped("`when` did not hold: {{steps.triage.risky}} == true".to_string())
        );
        assert_eq!(report.steps[2].status, Status::Ok);
        // Two calls: the gated step ran no child.
        assert_eq!(fake.bodies.lock().unwrap().len(), 2);

        // A field the object does not have is not a false gate, it is a step that
        // cannot be judged, and it says so.
        let fake = Fake::new(vec![
            vec![say("{\"area\": \"none\"}")],
            vec![say("written")],
        ]);
        let (report, _, second) = go(&workflow, "", &fake).await;
        let Status::Skipped(reason) = &report.steps[1].status else {
            panic!("{:?}", report.steps[1].status);
        };
        assert!(
            reason.contains("`when` reads `{{steps.triage.risky}}`"),
            "{reason}"
        );
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(second);
    }

    /// An answer that is not the shape the step promised is a failed step, not a value
    /// the steps after it have to guess at.
    #[tokio::test]
    async fn an_answer_that_is_not_an_object_fails_the_step() {
        let workflow = workflow(
            "cache: true\nsteps:\n  - id: triage\n    output: json\n    prompt: triage it\n  \
- id: fix\n    needs: [triage]\n    prompt: fix {{steps.triage}}\n",
        );
        let fake = Fake::new(vec![vec![say("it looks risky to me")]]);
        let (report, _, dir) = go(&workflow, "", &fake).await;
        assert_eq!(
            statuses(&report),
            [
                (
                    "triage",
                    &Status::Failed("did not answer with one JSON object".to_string())
                ),
                (
                    "fix",
                    &Status::Skipped("`triage` did not finish".to_string())
                )
            ]
        );
        // Nothing of it was stored, so the next run asks again rather than being handed
        // back an answer the step already refused.
        let fake = Fake::new(vec![vec![say("{\"risky\": true}")], vec![say("fixed")]]);
        let (report, _) = go_in(&dir, &workflow, "", &fake).await;
        assert_eq!(
            statuses(&report),
            [("triage", &Status::Ok), ("fix", &Status::Ok)]
        );
        assert_eq!(fake.bodies.lock().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_output_or_a_gate_that_could_not_work_is_a_load_error() {
        let bad = |steps: &str| {
            format!(
                "{:#}",
                parse(&format!("---\nname: w\n{steps}---\n"), "./t").unwrap_err()
            )
        };
        assert!(
            bad("steps:\n  - id: a\n    output: yaml\n    prompt: one\n")
                .contains("step `a` has a bad `output` `yaml`"),
        );
        assert!(
            bad("steps:\n  - id: a\n    when: it feels right\n    prompt: one\n")
                .contains("is not `{{...}} == value` or `!=`"),
        );
        assert!(
            bad("steps:\n  - id: a\n    when: nope == true\n    prompt: one\n")
                .contains("does not start with a `{{...}}`"),
        );
        // A field is read off one object, so the step it comes from has to answer with
        // one, and has to answer once.
        assert!(
            bad(
                "steps:\n  - id: a\n    prompt: one\n  - id: b\n    needs: [a]\n    \
prompt: {{steps.a.risky}}\n"
            )
            .contains("but `a` has no `output: json`"),
        );
        assert!(
            bad(
                "steps:\n  - id: a\n    prompt: one\n  - id: b\n    for_each: a\n    \
output: json\n    prompt: {{item}}\n  - id: c\n    needs: [b]\n    prompt: {{steps.b.risky}}\n"
            )
            .contains("but `b` fans out, so it has no one object"),
        );
        assert!(
            bad(
                "steps:\n  - id: a\n    output: json\n    prompt: one\n  - id: b\n    \
needs: [a]\n    when: {{steps.a.risky}} == true\n    prompt: {{steps.a.deep.field}}\n"
            )
            .contains("uses unknown `{{steps.a.deep.field}}`"),
        );
        assert!(
            bad(
                "steps:\n  - id: a\n    prompt: one\n  - id: b\n    for_each: a\n    \
when: {{item}} == x\n    prompt: {{item}}\n"
            )
            .contains("step `b` gates itself on `{{item}}`"),
        );
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
