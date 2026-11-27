//! Keeping history inside the context window: old tool outputs are evicted first, and
//! only when that is not enough are the earlier turns replaced by a summary, or by the
//! backend's own `compaction` item on a model that compacts on the server.

use std::collections::HashSet;
use std::time::Duration;

use serde_json::{Value, json};

use crate::tokens::{self, Tokenizer};

/// The context window of the gpt-5 family, in tokens.
pub const GPT5_WINDOW: u64 = 272_000;
/// The window the Codex CLI gives the gpt-6 family by default, in tokens.
pub const GPT6_WINDOW: u64 = 272_000;
/// What Ollama is asked to hold, and what compaction assumes it holds. Its own default
/// is 4096, which the system prompt and the tool schemas overflow between them, and it
/// truncates from the front with nothing but a line in the server's log to say so.
pub const OLLAMA_WINDOW: u64 = 32_768;
/// What a model nothing else says a window for is assumed to hold. Compaction has to
/// assume something: with no window at all it never runs, and a full context is then a
/// fatal 4xx on every turn until the user compacts by hand.
pub const DEFAULT_WINDOW: u64 = 128_000;
/// The default fraction of the window a call may read before history is compacted.
pub const COMPACT_AT: f64 = 0.8;
/// Compaction aims for this fraction of the window, or for this fraction of a lower
/// trigger: a target equal to the trigger would leave the next turn over it again.
const TARGET: f64 = 0.6;
const TARGET_OF_TRIGGER: f64 = 0.75;
/// A compacted copy of an idle conversation is made this long before its cache is
/// expected to lapse, so the summary call still reads the history from the cache.
pub const FORK_LEAD: Duration = Duration::from_secs(3 * 60);
/// A conversation whose last call read fewer tokens than this is not forked: there is
/// too little to fold away to be worth the summary call.
pub const FORK_MIN: u64 = 16_000;
/// The tokens of user messages a server compaction keeps beside its item.
pub const RETAINED: u64 = 64_000;
/// The most recent tool results are never evicted.
const KEEP_RESULTS: usize = 6;
/// A kept result longer than this, in bytes, keeps only its first and last `TRIM_KEEP`:
/// about 1,000 tokens, past which a log or a file read is mostly middle, while the start
/// says what ran and the end how it ended. OpenClaw's pruning uses the same pair.
const TRIM_OVER: usize = 4_000;
const TRIM_KEEP: usize = 1_500;
/// An old edit, write or patch body longer than this, in bytes, becomes a stub. The
/// stub is about 100 bytes, so a shorter body is cheaper to keep than to fetch back.
const STUB_OVER: usize = 256;
/// Old tool results are evicted before a call on a cold cache once the last call read
/// more than this fraction of the window; under it there is too little to save.
pub const COLD_FLOOR: f64 = 0.3;
/// How the summary starts in the compacted history.
pub const SUMMARY_PREFIX: &str = "Summary of earlier conversation:";
/// What stands in for a summary the summary call could not fit in the window.
pub const UNSUMMARISED: &str = "The earlier turns were dropped without a summary: the \
conversation no longer fit in the context window to be summarised.";
/// What the model is asked for when history is summarised. A short request gets a
/// short summary, and a short summary paraphrases less; the exact text stays in the
/// record for `recall`.
const REQUEST: &str = "Summarise the conversation so far for yourself to continue from once the \
older messages are gone. Keep it short: about 1,000 tokens, in terse bullets. Leave out what is \
restated beside the summary anyway: the first user message, the latest one and what followed it, \
the goal, the plan, your session notes, what was remembered, and the instructions. The earlier \
turns stay in the session's record, where `recall` fetches exact tool output and errors, so name \
what to recall (a command, a file, a call id) rather than copying long text; if the session is \
not saved, keep the exact error lines that matter. Keep:
- the user's requirements and corrections, and each authorization with its scope;
- decisions made, with why;
- approaches tried and rejected, with why each failed;
- unresolved blockers with their evidence; for a resource limit, the measured capacity and any \
cleanup or artifact rotation the user authorized;
- what is done, including procedures verified to work, and what is left.
Keep explicit user requirements and tool-enforced restrictions apart from your own inferences: \
label uncertainty, and do not turn an inferred restriction into a user requirement. Carry forward \
later corrections and resolved blockers rather than stale constraints. Do not include secret \
values. End with a section headed `## For future sessions`: a short list of facts about this \
project or its environment that a later session, starting without this conversation, would \
otherwise have to rediscover (setup, conventions, traps), each with why it holds; write `None.` \
under it when there are none. Do not call any tools; reply with the summary only.";

/// What follows the summary in a folded history, saying where the earlier turns went.
const POINTER: &str = "The turns before this summary are in the session's record: \
`recall` searches them, and returns earlier turns, exact tool outputs and errors.";
const POINTER_LOST: &str = "The turns before this summary are gone: this session is \
not saved, so earlier turns, exact tool outputs and errors cannot be recalled.";

/// The heading of the summary's last section, which the harness takes out of it.
const FUTURE: &str = "for future sessions";

/// `summary` without its `## For future sessions` section, and the facts that section
/// lists: one per item, an item's continuation lines joined to it.
pub fn candidates(summary: &str) -> (String, Vec<String>) {
    let heading = |line: &str| {
        let line = line.trim();
        line.starts_with('#')
            && line
                .trim_start_matches('#')
                .trim()
                .trim_end_matches(':')
                .eq_ignore_ascii_case(FUTURE)
    };
    let lines: Vec<&str> = summary.lines().collect();
    let Some(start) = lines.iter().rposition(|line| heading(line)) else {
        return (summary.to_string(), Vec::new());
    };
    let level = lines[start]
        .trim()
        .chars()
        .take_while(|&c| c == '#')
        .count();
    let end = lines[start + 1..]
        .iter()
        .position(|line| {
            let hashes = line.chars().take_while(|&c| c == '#').count();
            hashes > 0 && hashes <= level && line[hashes..].starts_with(' ')
        })
        .map_or(lines.len(), |at| start + 1 + at);
    let mut facts: Vec<String> = Vec::new();
    for line in &lines[start + 1..end] {
        let trimmed = line.trim();
        let item = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| {
                let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
                (digits > 0)
                    .then(|| trimmed[digits..].strip_prefix(". "))
                    .flatten()
            });
        match (item, facts.last_mut()) {
            (Some(item), _) => facts.push(item.trim().to_string()),
            (None, Some(last)) if !trimmed.is_empty() && line.starts_with(char::is_whitespace) => {
                last.push(' ');
                last.push_str(trimmed);
            }
            _ => {}
        }
    }
    facts.retain(|fact| {
        let bare = fact.trim_end_matches('.').trim();
        !bare.is_empty() && !bare.eq_ignore_ascii_case("none")
    });
    let mut kept: Vec<&str> = lines[..start].to_vec();
    kept.extend(&lines[end..]);
    (kept.join("\n").trim().to_string(), facts)
}

/// When to compact, from the config.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// The context window in tokens; unset means the model's own, or what it is assumed
    /// to hold when nothing reports one.
    pub window: Option<u64>,
    pub compact_at: f64,
    /// The model's window as its backend's list reports it, under a configured one.
    pub reported: Option<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            window: None,
            compact_at: COMPACT_AT,
            reported: None,
        }
    }
}

impl Limits {
    /// The window for `model`: the configured one, else the reported one, else what it
    /// is assumed to hold.
    pub fn window(&self, model: &str) -> u64 {
        self.window
            .or(self.reported)
            .unwrap_or_else(|| default_window(model))
    }

    /// Whether a call that read `input` tokens filled the window past `compact_at`.
    pub fn over(&self, model: &str, input: u64) -> bool {
        input as f64 > self.compact_at * self.window(model) as f64
    }

    /// The size compaction aims for, in tokens, always below `compact_at`.
    pub fn target(&self, model: &str) -> u64 {
        let fraction = TARGET.min(self.compact_at * TARGET_OF_TRIGGER);
        // Rounded, so a trigger that is exact in decimal does not lose a token to binary.
        (fraction * self.window(model) as f64).round() as u64
    }
}

/// What `model` is assumed to hold when nothing reports its window.
pub fn default_window(model: &str) -> u64 {
    if model.starts_with("gpt-5") {
        GPT5_WINDOW
    } else if model.starts_with("gpt-6") {
        GPT6_WINDOW
    } else if model.starts_with(crate::ollama::PREFIX) {
        OLLAMA_WINDOW
    } else {
        DEFAULT_WINDOW
    }
}

/// A user message item.
pub fn user_message(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "user",
        "content": [{ "type": "input_text", "text": text }],
    })
}

/// The request appended to history for the summary call. What the user asked for goes
/// last, so it steers a summary that still has to carry the conversation.
pub fn request(asked: Option<&str>) -> Value {
    match asked.map(str::trim).filter(|asked| !asked.is_empty()) {
        Some(asked) => user_message(&format!(
            "{REQUEST}\n\nThe user asked the summary to keep this in particular: {asked}"
        )),
        None => user_message(REQUEST),
    }
}

/// Tokens the model reads in `items`, encrypted reasoning not counted.
pub fn estimate(items: &[Value], tokenizer: &dyn Tokenizer) -> u64 {
    items
        .iter()
        .filter_map(|item| tokens::item_tokens(item, tokenizer))
        .sum()
}

/// Evict every old tool result, never the last few or one the model has not read yet:
/// the output becomes a placeholder naming the `recall` that gets it back, and the bulky
/// arguments of an old edit, write or patch become a stub. A kept output the model has
/// read keeps only its start and end when it is long. The call items stay, so every call
/// keeps its output. Returns the tokens freed.
///
/// `saved` says whether a record of the session keeps the originals for `recall`; when
/// none does, the texts say what was removed cannot be recovered instead.
///
/// Everything old goes at once, not just enough to get under the target: the request
/// misses the cache from the first item changed, and the oldest output sits near the
/// front, so evicting the rest after it costs nothing more and leaves less to resend.
pub fn evict(history: &mut [Value], tokenizer: &dyn Tokenizer, saved: bool) -> u64 {
    let results: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, item)| is_output(item))
        .map(|(index, _)| index)
        .collect();
    // Outputs after the model's last item land after the call that reads them.
    let read = history.iter().rposition(from_model);
    let unread = |index: usize| read.is_none_or(|read| index > read);
    let old = results.len().saturating_sub(KEEP_RESULTS);
    let kept: HashSet<String> = results
        .iter()
        .enumerate()
        .filter(|&(n, &index)| n >= old || unread(index))
        .filter_map(|(_, &index)| history[index]["call_id"].as_str().map(str::to_string))
        .collect();
    let mut freed = 0;
    for (n, &index) in results.iter().enumerate() {
        if unread(index) {
            continue;
        }
        let item = &history[index];
        let id = item["call_id"].as_str().unwrap_or_default();
        let next = match n >= old {
            true => trimmed(item, id, saved),
            false => placeholder(item, id, saved),
        };
        if let Some(next) = next {
            freed += replace(&mut history[index], "output", next, tokenizer);
        }
    }
    for item in history.iter_mut() {
        let Some(id) = item["call_id"].as_str().map(str::to_string) else {
            continue;
        };
        if kept.contains(&id) {
            continue;
        }
        let (field, next) = match item["type"].as_str() {
            Some("function_call") => ("arguments", stubbed_arguments(item, &id, saved)),
            Some("custom_tool_call") => ("input", stubbed_patch(item, &id, saved)),
            _ => continue,
        };
        if let Some(next) = next {
            freed += replace(item, field, next, tokenizer);
        }
    }
    freed
}

/// Whether `text` is the placeholder of an evicted output.
pub fn evicted(text: &str) -> bool {
    // The first form is what sessions saved before placeholders named `recall`.
    text.starts_with("[output removed")
        || text.starts_with("[output of ")
            && (text.ends_with(" returns it]") || text.ends_with(LOST))
}

const REMOVED: &str = " removed from context";
/// What a trimmed output carries where its middle was.
const TRIMMED: &str = " bytes of this output removed from context; ";
/// How the texts end when nothing kept the originals.
const UNSAVED: &str = "this session is not saved, so";
const LOST: &str = "cannot be recovered]";

/// `item[field]` set to `next` when that reads fewer tokens; the tokens freed.
fn replace(item: &mut Value, field: &str, next: String, tokenizer: &dyn Tokenizer) -> u64 {
    let before = tokens::item_tokens(item, tokenizer).unwrap_or_default();
    let was = std::mem::replace(&mut item[field], json!(next));
    let after = tokens::item_tokens(item, tokenizer).unwrap_or_default();
    if after >= before {
        item[field] = was;
        return 0;
    }
    before - after
}

/// How the model is told to get back what is gone.
fn recall(id: &str) -> String {
    format!("recall {}", json!({ "call_id": id }))
}

/// The placeholder for an old output, `None` when it already is one.
fn placeholder(item: &Value, id: &str, saved: bool) -> Option<String> {
    let text = tokens::item_text(item).unwrap_or_default();
    if evicted(&text) {
        return None;
    }
    let size = match tokens::images(item) {
        0 => format!("{} bytes", text.len()),
        n => format!("{} bytes and {n} image(s)", text.len()),
    };
    let digest = digest(&text).map(|d| format!("{d}; ")).unwrap_or_default();
    let after = match saved {
        true => format!("{} returns it]", recall(id)),
        false => format!("{UNSAVED} it {LOST}"),
    };
    Some(format!(
        "[output of {id}{REMOVED} ({size}); {digest}{after}"
    ))
}

/// The most a digest may be, in bytes.
const DIGEST: usize = 200;

/// What a command's result said, for its placeholder: how it ended, its first line that
/// looks like a failure when it failed, and the file its whole output is saved in. A
/// command may not be cheap or safe to run again, as a read or a search is; those, and
/// anything else that is not a command's result, get none.
fn digest(output: &str) -> Option<String> {
    use crate::tools::bash::{outcome, spill};
    let outcome = outcome(output)?;
    let mut digest = outcome.label();
    let path = spill::saved(output)
        .map(|path| format!("; saved in {path}"))
        .filter(|path| digest.len() + path.len() <= DIGEST)
        .unwrap_or_default();
    let failure = (!outcome.ok())
        .then(|| {
            output
                .lines()
                .skip(1)
                .map(str::trim)
                .filter(|line| !line.starts_with(spill::FAILURES))
                .find(|line| spill::looks_failed(line))
        })
        .flatten();
    if let Some(line) = failure {
        let room = DIGEST.saturating_sub(digest.len() + path.len() + "; first failure: ".len());
        let line = match line.len() <= room {
            true => line.to_string(),
            false => format!(
                "{}...",
                &line[..line.floor_char_boundary(room.saturating_sub(3))]
            ),
        };
        if line.len() > 3 {
            digest.push_str(&format!("; first failure: {line}"));
        }
    }
    digest.push_str(&path);
    Some(digest)
}

/// A kept output over `TRIM_OVER` bytes cut to its first and last `TRIM_KEEP`, with the
/// `recall` that pages in the middle. Only a plain text output: one with images is left.
fn trimmed(item: &Value, id: &str, saved: bool) -> Option<String> {
    let text = item["output"].as_str()?;
    if text.len() <= TRIM_OVER || evicted(text) || text.contains(TRIMMED) {
        return None;
    }
    let head = text.floor_char_boundary(TRIM_KEEP);
    let tail = text.ceil_char_boundary(text.len() - TRIM_KEEP);
    let call = json!({ "call_id": id, "offset": head });
    let after = match saved {
        true => format!("recall {call} returns them"),
        false => format!("{UNSAVED} they {}", LOST.trim_end_matches(']')),
    };
    Some(format!(
        "{}\n[... {}{TRIMMED}{after} ...]\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    ))
}

/// An old `edit` or `write` with each long field but its path cut to a stub, still a
/// JSON object as `arguments` has to be.
fn stubbed_arguments(item: &Value, id: &str, saved: bool) -> Option<String> {
    let name = item["name"].as_str()?;
    if name != crate::tools::edit::NAME && name != crate::tools::write::NAME {
        return None;
    }
    let mut args: Value = serde_json::from_str(item["arguments"].as_str()?).ok()?;
    let mut changed = false;
    for (key, value) in args.as_object_mut()? {
        let Some(text) = value.as_str().filter(|_| key != "path") else {
            continue;
        };
        if text.len() <= STUB_OVER || stubbed(text) {
            continue;
        }
        *value = json!(stub(text.len(), id, saved));
        changed = true;
    }
    changed.then(|| args.to_string())
}

/// An old `apply_patch` with the body of each file's hunk cut to one stub line, so it
/// still parses: an added line for a file added, a context line for one updated.
fn stubbed_patch(item: &Value, id: &str, saved: bool) -> Option<String> {
    if item["name"].as_str()? != crate::tools::patch::NAME {
        return None;
    }
    let patch = item["input"].as_str()?;
    if patch.lines().any(stubbed) {
        return None;
    }
    let header = |line: &str| line.starts_with("*** ") && !line.starts_with("*** End of File");
    let body: usize = patch
        .lines()
        .filter(|line| !header(line))
        .map(|line| line.len() + 1)
        .sum();
    if body <= STUB_OVER {
        return None;
    }
    let mut out = String::new();
    // The current hunk's stub prefix, and the bytes of its body.
    let mut hunk: Option<(&str, usize)> = None;
    let flush = |out: &mut String, hunk: &mut Option<(&str, usize)>| {
        if let Some((prefix, bytes)) = hunk.take().filter(|(_, bytes)| *bytes > 0) {
            out.push_str(&format!("{prefix}{}\n", stub(bytes, id, saved)));
        }
    };
    for line in patch.lines() {
        if !header(line) {
            if let Some((_, bytes)) = &mut hunk {
                *bytes += line.len() + 1;
            }
            continue;
        }
        if !line.starts_with("*** Move to: ") {
            flush(&mut out, &mut hunk);
            hunk = if line.starts_with("*** Add File: ") {
                Some(("+", 0))
            } else if line.starts_with("*** Update File: ") {
                Some((" ", 0))
            } else {
                None
            };
        }
        out.push_str(line);
        out.push('\n');
    }
    flush(&mut out, &mut hunk);
    Some(out)
}

fn stub(bytes: usize, id: &str, saved: bool) -> String {
    match saved {
        true => format!("[{bytes} bytes{REMOVED}; {} {STUBBED}", recall(id)),
        false => format!("[{bytes} bytes{REMOVED}; {UNSAVED} they {LOST}"),
    }
}

const STUBBED: &str = "gets the call back]";

/// Whether `text` is a stub, or a patch line ending in one.
fn stubbed(text: &str) -> bool {
    (text.ends_with(STUBBED) || text.ends_with(LOST)) && text.contains(REMOVED)
}

fn is_output(item: &Value) -> bool {
    is(item, "function_call_output") || is(item, "custom_tool_call_output")
}

/// What the model wrote, as against what was sent to it.
fn from_model(item: &Value) -> bool {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call" | "custom_tool_call" | "tool_search_call" | "reasoning") => true,
        Some("message") => item["role"] == "assistant",
        _ => false,
    }
}

/// The history after a summary: the environment and instruction updates it told the
/// model, the first user message, the summary with where the turns it stands for went,
/// and the last user turn onward. `saved` says whether a record keeps those turns for
/// `recall`. `None` when there is no earlier turn to fold.
pub fn fold(history: &[Value], summary: &str, saved: bool) -> Option<Vec<Value>> {
    let mut users = history
        .iter()
        .enumerate()
        .filter(|(_, item)| is(item, "message") && item["role"] == "user")
        .map(|(index, _)| index);
    let first = users.next()?;
    let last = users.next_back().filter(|&last| last > first)?;
    let mut folded: Vec<Value> = crate::environment::restated(history).into_iter().collect();
    folded.extend(crate::instructions::restated(history));
    folded.push(history[first].clone());
    folded.push(user_message(&format!(
        "{SUMMARY_PREFIX}\n{}",
        summary.trim()
    )));
    folded.push(pointer(saved));
    folded.extend(crate::goal::restated(history));
    folded.extend(crate::goal::restated_lifecycle(history));
    folded.extend(crate::plan::restated_context(history));
    folded.extend(crate::notes::restated(history));
    folded.extend(
        history[last..]
            .iter()
            .filter(|item| {
                !crate::goal::is_context(item)
                    && !crate::goal::is_lifecycle(item)
                    && !crate::plan::is_context(item)
                    && !crate::notes::is_context(item)
            })
            .cloned(),
    );
    Some(folded)
}

/// The developer item after a summary, saying whether `recall` can get the folded turns.
pub fn pointer(saved: bool) -> Value {
    let text = if saved { POINTER } else { POINTER_LOST };
    json!({"type": "message", "role": "developer", "content": [{
        "type": "input_text", "text": text
    }]})
}

/// The history after a server compaction: the environment and instruction updates it
/// told the model, the user messages that fit in `budget` tokens, newest first and the one
/// that crosses it cut in the middle, and last the `compaction` item that stands for the
/// rest.
pub fn install(
    history: &[Value],
    compaction: Value,
    budget: u64,
    tokenizer: &dyn Tokenizer,
) -> Vec<Value> {
    let mut kept = Vec::new();
    let mut left = budget;
    let users = history
        .iter()
        .rev()
        .filter(|item| is(item, "message") && item["role"] == "user");
    for item in users {
        if left == 0 {
            break;
        }
        let text = tokens::item_text(item).unwrap_or_default();
        let count = tokens::item_tokens(item, tokenizer).unwrap_or_default();
        if count <= left {
            left -= count;
            kept.push(item.clone());
            continue;
        }
        kept.push(user_message(&clip(&text, left)));
        left = 0;
    }
    kept.reverse();
    let mut installed: Vec<Value> = crate::environment::restated(history).into_iter().collect();
    installed.extend(crate::instructions::restated(history));
    installed.extend(kept);
    installed.push(compaction);
    installed.extend(crate::goal::restated(history));
    installed.extend(crate::goal::restated_lifecycle(history));
    installed.extend(crate::plan::restated_context(history));
    installed.extend(crate::notes::restated(history));
    installed
}

/// `text` cut to about `tokens` tokens, its start and end kept.
fn clip(text: &str, tokens: u64) -> String {
    let half = (tokens as usize).saturating_mul(4) / 2;
    let head = text.floor_char_boundary(half);
    let tail = text.ceil_char_boundary(text.len().saturating_sub(half).max(head));
    format!(
        "{}\n[... {} bytes cut to fit the compacted history ...]\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

fn is(item: &Value, kind: &str) -> bool {
    item.get("type").and_then(Value::as_str) == Some(kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::ByteEstimate;

    fn call(id: usize) -> Value {
        json!({"type": "function_call", "call_id": format!("c{id}"), "name": "bash", "arguments": "{}"})
    }

    fn output(id: usize, text: &str) -> Value {
        json!({"type": "function_call_output", "call_id": format!("c{id}"), "output": text})
    }

    /// A turn with `results` tool calls of 400 bytes each.
    fn turn(history: &mut Vec<Value>, text: &str, results: usize) {
        history.push(user_message(text));
        history.push(json!({"type": "reasoning", "encrypted_content": "gAAA"}));
        for _ in 0..results {
            let id = history.len();
            history.push(call(id));
            history.push(output(id, &"x".repeat(400)));
        }
        history.push(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]}));
    }

    #[test]
    fn summaries_preserve_authorization_without_hardening_inferred_blockers() {
        for asked in [None, Some("keep the benchmark progress")] {
            let message = request(asked);
            let text = message["content"][0]["text"].as_str().unwrap();
            for instruction in [
                "each authorization with its scope",
                "procedures verified to work",
                "approaches tried and rejected, with why each failed",
                "unresolved blockers with their evidence",
                "label uncertainty",
                "do not turn an inferred restriction into a user requirement",
                "later corrections and resolved blockers rather than stale constraints",
                "cleanup or artifact rotation the user authorized",
                "Do not include secret values",
            ] {
                assert!(text.contains(instruction), "{instruction}");
            }
            if let Some(asked) = asked {
                assert!(text.ends_with(asked));
            }
        }
    }

    #[test]
    fn the_summary_request_is_short_and_skips_what_is_restated() {
        let text = request(None)["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        for kept in [
            "about 1,000 tokens",
            "the user's requirements and corrections",
            "decisions made, with why",
            "what is done",
            "what is left",
            "`recall` fetches exact tool output and errors",
            "`## For future sessions`",
        ] {
            assert!(text.contains(kept), "{kept}");
        }
        // Restated after the fold anyway, so not asked for again.
        assert!(text.contains(
            "the goal, the plan, your session notes, what was remembered, and the instructions"
        ));
        assert!(!text.contains("files touched"), "{text}");
        assert!(REQUEST.len() < 2_000, "{}", REQUEST.len());
    }

    #[test]
    fn a_fold_says_where_the_earlier_turns_went() {
        let mut history = Vec::new();
        turn(&mut history, "one", 1);
        turn(&mut history, "two", 1);
        let saved = fold(&history, "s", true).unwrap();
        let text = saved[2]["content"][0]["text"].as_str().unwrap();
        assert_eq!(saved[2]["role"], "developer");
        assert!(text.contains("`recall` searches them"), "{text}");
        assert!(text.contains("exact tool outputs and errors"), "{text}");
        let unsaved = fold(&history, "s", false).unwrap();
        let text = unsaved[2]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("are gone: this session is not saved"),
            "{text}"
        );
        assert!(!text.contains("`recall` searches"), "{text}");
        assert_eq!(saved.len(), unsaved.len());
        // A server compaction stands in for the turns without a summary to follow.
        let item = json!({"type": "compaction", "encrypted_content": "opaque"});
        let installed = install(&history, item, u64::MAX, &ByteEstimate);
        assert!(!installed.contains(&pointer(true)));
    }

    /// A history whose first output is `text`, old enough to be evicted.
    fn evicted_output(text: &str) -> String {
        let mut history = vec![call(0), output(0, text)];
        turn(&mut history, "next", KEEP_RESULTS);
        evict(&mut history, &ByteEstimate, true);
        let placeholder = history[1]["output"].as_str().unwrap().to_string();
        assert!(evicted(&placeholder), "{placeholder}");
        placeholder
    }

    #[test]
    fn a_command_placeholder_keeps_how_it_ended_and_its_first_failure() {
        let failed = format!(
            "exit code: 101\n   Compiling a v0.1.0\nwarning: unused\n\
error[E0425]: cannot find value `x` in this scope\nerror: aborting\n{}\n[output: 9 lines, 3.2s]",
            "x".repeat(400)
        );
        let text = evicted_output(&failed);
        let digest =
            "failed, exit 101; first failure: error[E0425]: cannot find value `x` in this scope";
        assert_eq!(
            text,
            format!(
                "[output of c0 removed from context ({} bytes); {digest}; recall {{\"call_id\":\"c0\"}} returns it]",
                failed.len()
            )
        );
        // A success, or one still running, says only that.
        let ok = format!(
            "exit code: 0\nerror: this is just output\n{}",
            "x".repeat(400)
        );
        assert!(evicted_output(&ok).contains("bytes); succeeded; recall"));
        let running = format!("Process running with session ID 3\n{}", "x".repeat(400));
        assert!(evicted_output(&running).contains("; still running as session 3; recall"));
    }

    #[test]
    fn a_spilled_command_placeholder_keeps_its_file_and_stays_capped() {
        let path = "/w/.bhai/sessions/s/scratch/output/bash-20261127-101010-42-1.log";
        let spilled = format!(
            "exit code: 1\nThe output was 90000 bytes over 2000 lines, too long to show here. \
All of it is saved in {path}. In bash it is $BHAI_SCRATCH_DIR/output/bash-1.log.\n\n\
Lines that look like failures, outside those shown below, 1 of 1:\n\
700: error: {}\n\nFirst lines:\n1: a\n[output: 2000 lines, 9.0s]",
            "y".repeat(300)
        );
        let text = evicted_output(&spilled);
        let digest = text
            .split_once("bytes); ")
            .unwrap()
            .1
            .split_once("; recall ")
            .unwrap()
            .0;
        assert!(
            digest.starts_with("failed, exit 1; first failure: 700: error: yyy"),
            "{digest}"
        );
        assert!(
            digest.ends_with(&format!("...; saved in {path}")),
            "{digest}"
        );
        assert!(digest.len() <= DIGEST, "{}", digest.len());
        // Not the heading over the failure lines, which reads like one itself.
        assert!(!digest.contains("Lines that look"), "{digest}");
        // Unsaved, the digest stays and only the way back changes.
        let mut history = vec![call(0), output(0, &spilled)];
        turn(&mut history, "next", KEEP_RESULTS);
        evict(&mut history, &ByteEstimate, false);
        let unsaved = history[1]["output"].as_str().unwrap();
        assert!(unsaved.contains(digest), "{unsaved}");
        assert!(unsaved.ends_with("this session is not saved, so it cannot be recovered]"));
        assert!(evicted(unsaved));
    }

    #[test]
    fn read_and_search_placeholders_have_no_digest() {
        let read = format!(
            "1\tfn main() {{\n2\t    panic!(\"error\");\n{}",
            "x".repeat(400)
        );
        let search = format!("src/a.rs:3: error handling\n{}", "x".repeat(400));
        for text in [read, search] {
            assert_eq!(
                evicted_output(&text),
                format!(
                    "[output of c0 removed from context ({} bytes); recall {{\"call_id\":\"c0\"}} returns it]",
                    text.len()
                )
            );
        }
    }

    #[test]
    fn limits_know_the_gpt5_window() {
        let limits = Limits::default();
        assert_eq!(limits.window("gpt-5.1-codex"), GPT5_WINDOW);
        assert_eq!(limits.window("gpt-6-astra"), GPT6_WINDOW);
        assert!(limits.over("gpt-5", 220_000));
        assert!(!limits.over("gpt-5", 210_000));
        // A model nothing reports a window for still compacts, on an assumed one.
        assert_eq!(limits.window("other"), DEFAULT_WINDOW);
        assert_eq!(limits.window("ollama:gemma4:e2b"), OLLAMA_WINDOW);
        assert!(limits.over("other", u64::MAX));
        assert!(!limits.over("other", 100_000));
        let set = Limits {
            window: Some(1000),
            compact_at: 0.7,
            reported: None,
        };
        assert!(set.over("other", 701));
        assert_eq!(set.target("other"), 525);
        // A trigger at or under the flat target: aiming for it would compact into a
        // history that is over the trigger again, so the target follows the trigger down.
        let low = Limits {
            compact_at: 0.5,
            ..set
        };
        assert_eq!(low.target("other"), 375);
        for compact_at in [0.1, 0.3, 0.5, 0.6, 0.8, 0.95, 1.0] {
            let limits = Limits { compact_at, ..set };
            let window = limits.window("other") as f64;
            assert!(
                (limits.target("other") as f64) < compact_at * window,
                "target at compact_at {compact_at}"
            );
        }
    }

    #[test]
    fn a_reported_window_sits_between_the_configured_and_the_assumed_one() {
        let reported = Limits {
            reported: Some(400_000),
            ..Limits::default()
        };
        assert_eq!(reported.window("gpt-6-astra"), 400_000);
        assert!(!reported.over("gpt-6-astra", 300_000));
        let configured = Limits {
            window: Some(100_000),
            ..reported
        };
        assert_eq!(configured.window("gpt-6-astra"), 100_000);
    }

    #[test]
    fn eviction_keeps_pairs_and_the_last_six_results() {
        let mut history = Vec::new();
        turn(&mut history, "one", 5);
        turn(&mut history, "two", 5);
        turn(&mut history, "three", 5);
        let before = history.clone();

        // Every old output goes in one pass, not only enough to get under a target.
        let freed = evict(&mut history, &ByteEstimate, true);
        assert_eq!(history.len(), before.len());
        assert_eq!(
            estimate(&history, &ByteEstimate),
            estimate(&before, &ByteEstimate) - freed
        );
        let outputs: Vec<&Value> = history
            .iter()
            .filter(|i| is(i, "function_call_output"))
            .collect();
        for output in &outputs[..9] {
            let id = output["call_id"].as_str().unwrap();
            assert_eq!(
                output["output"],
                format!(
                    "[output of {id} removed from context (400 bytes); recall {{\"call_id\":\"{id}\"}} returns it]"
                )
            );
            assert!(evicted(output["output"].as_str().unwrap()));
        }
        assert!(
            outputs[9..]
                .iter()
                .all(|o| o["output"].as_str().unwrap().len() == 400)
        );
        // Only outputs changed; calls and their ids are untouched.
        for (old, new) in before.iter().zip(&history) {
            assert_eq!(old["call_id"], new["call_id"]);
            if !is(old, "function_call_output") {
                assert_eq!(old, new);
            }
        }
        // Nothing is evicted twice, so the history it left is the prefix from now on.
        let again = history.clone();
        assert_eq!(evict(&mut history, &ByteEstimate, true), 0);
        assert_eq!(history, again);
        // A placeholder from before they named `recall` is not replaced either.
        let mut old = vec![
            call(0),
            output(0, "[output removed to save context: 400 bytes]"),
        ];
        turn(&mut old, "next", KEEP_RESULTS);
        assert_eq!(evict(&mut old, &ByteEstimate, true), 0);
    }

    #[test]
    fn eviction_never_touches_an_output_the_model_has_not_read() {
        let mut history = Vec::new();
        turn(&mut history, "one", 3);
        // One step called ten tools in parallel, and their outputs are not read yet.
        history.push(json!({"type": "reasoning", "encrypted_content": "gAAA"}));
        let first = history.len();
        for id in first..first + 10 {
            history.push(call(id));
        }
        for id in first..first + 10 {
            history.push(output(id, &"y".repeat(10_000)));
        }
        let unread = history[first + 10..].to_vec();

        evict(&mut history, &ByteEstimate, true);
        assert_eq!(history[first + 10..], unread[..]);
        // The earlier turn's outputs are old all the same.
        let early: Vec<&Value> = history[..first]
            .iter()
            .filter(|i| is(i, "function_call_output"))
            .collect();
        assert!(early.iter().all(|o| evicted(o["output"].as_str().unwrap())));
        // Once the model has read them, all but the last six go.
        history.push(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]}));
        evict(&mut history, &ByteEstimate, true);
        let outputs: Vec<&str> = history[first + 10..first + 20]
            .iter()
            .map(|o| o["output"].as_str().unwrap())
            .collect();
        assert!(outputs[..4].iter().all(|o| evicted(o)));
        assert!(outputs[4..].iter().all(|o| !evicted(o)));
    }

    #[test]
    fn a_long_kept_output_keeps_its_start_and_end() {
        let mut history = Vec::new();
        turn(&mut history, "one", KEEP_RESULTS);
        let at = history.len() - 2;
        let id = history[at]["call_id"].as_str().unwrap().to_string();
        let long = format!(
            "{}{}{}",
            "a".repeat(1_500),
            "m".repeat(7_000),
            "z".repeat(1_500)
        );
        history[at]["output"] = json!(long);
        let short = history[at - 2].clone();

        let freed = evict(&mut history, &ByteEstimate, true);
        let text = history[at]["output"].as_str().unwrap();
        assert_eq!(
            text,
            format!(
                "{}\n[... 7000 bytes of this output removed from context; recall {{\"call_id\":\"{id}\",\"offset\":1500}} returns them ...]\n{}",
                "a".repeat(1_500),
                "z".repeat(1_500)
            )
        );
        assert!(freed > 1_500, "{freed}");
        // Under the threshold an output stays whole, and a trimmed one is not trimmed again.
        assert_eq!(history[at - 2], short);
        let trimmed = history.clone();
        assert_eq!(evict(&mut history, &ByteEstimate, true), 0);
        assert_eq!(history, trimmed);
    }

    #[test]
    fn old_edit_write_and_patch_bodies_become_stubs_that_still_parse() {
        let body = "line of the file\n".repeat(40);
        let function = |id: &str, name: &str, args: Value| json!({"type": "function_call", "call_id": id, "name": name, "arguments": args.to_string()});
        let edit = function(
            "e1",
            "edit",
            json!({"path": "/w/a.rs", "old_string": body, "new_string": body, "replace_all": false}),
        );
        let small = function(
            "e2",
            "edit",
            json!({"path": "/w/a.rs", "old_string": "x", "new_string": "y"}),
        );
        let write = function("w1", "write", json!({"path": "/w/b.rs", "content": body}));
        let bash = function("b1", "bash", json!({"command": body}));
        let added: String = body.lines().map(|l| format!("+{l}\n")).collect();
        let changed: String = body.lines().map(|l| format!("-{l}\n+{l}!\n")).collect();
        let patch = format!(
            "*** Begin Patch\n*** Add File: c.rs\n{added}*** Update File: d.rs\n*** Move to: e.rs\n@@ fn main\n{changed}*** End of File\n*** Delete File: f.rs\n*** End Patch"
        );
        let custom = json!({"type": "custom_tool_call", "call_id": "p1", "name": "apply_patch", "input": patch});
        // A recent edit is kept as it is.
        let recent = function(
            "e9",
            "edit",
            json!({"path": "/w/a.rs", "old_string": body, "new_string": body}),
        );
        let mut history = vec![user_message("go")];
        for call in [&edit, &small, &write, &bash, &custom, &recent] {
            let kind = match call["type"] == "custom_tool_call" {
                true => "custom_tool_call_output",
                false => "function_call_output",
            };
            history.push(call.clone());
            history.push(json!({"type": kind, "call_id": call["call_id"], "output": "done"}));
        }
        turn(&mut history, "next", KEEP_RESULTS - 1);

        assert!(evict(&mut history, &ByteEstimate, true) > 0);
        let stub = |id: &str, bytes: usize| {
            format!(
                "[{bytes} bytes removed from context; recall {{\"call_id\":\"{id}\"}} gets the call back]"
            )
        };
        let args = |i: usize| -> Value {
            serde_json::from_str(history[i]["arguments"].as_str().unwrap()).unwrap()
        };
        assert_eq!(
            args(1),
            json!({"path": "/w/a.rs", "old_string": stub("e1", body.len()),
                "new_string": stub("e1", body.len()), "replace_all": false})
        );
        assert_eq!(history[3], small);
        assert_eq!(
            args(5),
            json!({"path": "/w/b.rs", "content": stub("w1", body.len())})
        );
        assert_eq!(history[7], bash);
        assert_eq!(history[11], recent);
        let input = history[9]["input"].as_str().unwrap();
        let update = "@@ fn main\n".len() + changed.len() + "*** End of File\n".len();
        assert_eq!(
            input,
            format!(
                "*** Begin Patch\n*** Add File: c.rs\n+{}\n*** Update File: d.rs\n*** Move to: e.rs\n {}\n*** Delete File: f.rs\n*** End Patch\n",
                stub("p1", added.len()),
                stub("p1", update)
            )
        );
        // The stubbed patch still reads as the grammar has it.
        let cwd = std::env::temp_dir();
        let paths = crate::tools::patch::paths(&json!({ "input": input }), &cwd).unwrap();
        assert_eq!(paths.len(), 4, "{paths:?}");
        // Stubs are not stubbed again.
        let stubbed = history.clone();
        assert_eq!(evict(&mut history, &ByteEstimate, true), 0);
        assert_eq!(history, stubbed);
    }

    #[test]
    fn an_evicted_image_frees_its_fixed_cost_and_leaves_text() {
        let image = crate::tools::Image::new("image/png", "iVBORw0K").unwrap();
        let mut history = vec![
            call(0),
            crate::tools::function_output("c0", "shot", &[image]),
        ];
        turn(&mut history, "next", KEEP_RESULTS);
        let before = estimate(&history, &ByteEstimate);
        assert!(before > crate::tokens::IMAGE_TOKENS);

        let freed = evict(&mut history, &ByteEstimate, true);
        assert_eq!(
            history[1]["output"],
            "[output of c0 removed from context (22 bytes and 1 image(s)); recall {\"call_id\":\"c0\"} returns it]"
        );
        assert!(freed > crate::tokens::IMAGE_TOKENS - 30, "{freed}");
        assert_eq!(estimate(&history, &ByteEstimate), before - freed);
    }

    #[test]
    fn with_nothing_saved_the_texts_say_what_was_removed_cannot_be_recovered() {
        let body = "line of the file\n".repeat(40);
        let edit = json!({"type": "function_call", "call_id": "e1", "name": "edit",
            "arguments": json!({"path": "/w/a.rs", "old_string": body, "new_string": "y"}).to_string()});
        let patch = format!(
            "*** Begin Patch\n*** Add File: c.rs\n{}*** End Patch",
            "+x\n".repeat(200)
        );
        let custom = json!({"type": "custom_tool_call", "call_id": "p1", "name": "apply_patch", "input": patch});
        let mut history = vec![
            user_message("go"),
            edit,
            json!({"type": "function_call_output", "call_id": "e1", "output": "done"}),
            custom,
            json!({"type": "custom_tool_call_output", "call_id": "p1", "output": "done"}),
            call(0),
            output(0, &"x".repeat(400)),
        ];
        turn(&mut history, "next", KEEP_RESULTS);
        let at = history.len() - 2;
        history[at]["output"] = json!(format!("{}{}", "a".repeat(3_000), "z".repeat(3_000)));

        assert!(evict(&mut history, &ByteEstimate, false) > 0);
        assert_eq!(
            history[6]["output"],
            "[output of c0 removed from context (400 bytes); this session is not saved, so it cannot be recovered]"
        );
        let stub = format!(
            "[{} bytes removed from context; this session is not saved, so they cannot be recovered]",
            body.len()
        );
        let args: Value = serde_json::from_str(history[1]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["old_string"], stub);
        let input = history[3]["input"].as_str().unwrap();
        assert!(input.contains("+[600 bytes removed from context; this session is not saved, so they cannot be recovered]\n"), "{input}");
        let trimmed = history[at]["output"].as_str().unwrap();
        assert!(
            trimmed.contains("\n[... 3000 bytes of this output removed from context; this session is not saved, so they cannot be recovered ...]\n"),
            "{trimmed}"
        );
        // None of them names `recall`, all are recognised, and none is redone, saved or not.
        let text = Value::from(history.clone()).to_string();
        assert!(!text.contains("recall"), "{text}");
        for output in [&history[2], &history[4], &history[6]] {
            let output = output["output"].as_str().unwrap();
            assert_eq!(evicted(output), output != "done", "{output}");
        }
        let again = history.clone();
        assert_eq!(evict(&mut history, &ByteEstimate, true), 0);
        assert_eq!(evict(&mut history, &ByteEstimate, false), 0);
        assert_eq!(history, again);
    }

    #[test]
    fn fold_keeps_the_first_message_the_summary_and_the_last_turn() {
        let mut history = Vec::new();
        turn(&mut history, "one", 1);
        turn(&mut history, "two", 2);
        turn(&mut history, "three", 1);
        let folded = fold(&history, " did things \n", true).unwrap();
        assert_eq!(folded[0], history[0]);
        assert_eq!(
            folded[1]["content"][0]["text"],
            "Summary of earlier conversation:\ndid things"
        );
        assert_eq!(folded[2], pointer(true));
        let last = history.len() - 5;
        assert_eq!(folded[3], user_message("three"));
        assert_eq!(&folded[3..], &history[last..]);

        let dropped = fold(&history, UNSUMMARISED, true).unwrap();
        assert_eq!(&dropped[3..], &history[last..]);
        // Folding a folded history again keeps one pointer, after the new summary.
        let mut again = folded;
        turn(&mut again, "four", 1);
        let twice = fold(&again, "more", true).unwrap();
        let pointers: Vec<usize> = (0..twice.len())
            .filter(|&i| twice[i] == pointer(true))
            .collect();
        assert_eq!(pointers, [2]);

        // A single turn has nothing earlier to fold.
        assert_eq!(fold(&history[..5], "s", true), None);
        assert_eq!(fold(&[], "s", true), None);
    }

    #[test]
    fn install_keeps_the_newest_user_messages_then_the_item() {
        use crate::environment::{Environment, update};

        let env = Environment {
            cwd: "/w".to_string(),
            shell: "bash".to_string(),
            current_date: "2026-10-01".to_string(),
            timezone: "UTC".to_string(),
        };
        let mut history = vec![update(&[], &env).unwrap()];
        turn(&mut history, &"a".repeat(400), 1);
        turn(&mut history, &"b".repeat(400), 2);
        turn(&mut history, "three", 1);
        let item = json!({"type": "compaction", "encrypted_content": "opaque"});

        let all = install(&history, item.clone(), u64::MAX, &ByteEstimate);
        assert_eq!(all[0], history[0]);
        assert_eq!(all[1], user_message(&"a".repeat(400)));
        assert_eq!(all[2], user_message(&"b".repeat(400)));
        assert_eq!(all[3], user_message("three"));
        assert_eq!(all[4], item);
        assert_eq!(all.len(), 5, "no tool calls, outputs or answers: {all:?}");

        // 2 tokens for "three", the next 100-token message is cut to the 50 left, and
        // nothing older is kept.
        let some = install(&history, item.clone(), 52, &ByteEstimate);
        assert_eq!(some.len(), 4, "{some:?}");
        let cut = some[1]["content"][0]["text"].as_str().unwrap();
        assert!(cut.starts_with(&"b".repeat(100)), "{cut}");
        assert!(cut.ends_with(&"b".repeat(100)), "{cut}");
        assert!(cut.contains("[... 200 bytes cut"), "{cut}");
        assert_eq!(some[2], user_message("three"));
        assert_eq!(some[3], item);
    }

    #[test]
    fn fold_restates_the_environment_it_folds_away() {
        use crate::environment::{Environment, restated, update};

        let env = |date: &str| Environment {
            cwd: "/w".to_string(),
            shell: "bash".to_string(),
            current_date: date.to_string(),
            timezone: "UTC".to_string(),
        };
        let mut history = vec![update(&[], &env("2026-10-01")).unwrap()];
        turn(&mut history, "one", 1);
        history.extend(update(&history, &env("2026-10-02")));
        turn(&mut history, "two", 1);
        history.extend(update(&history, &env("2026-10-03")));
        turn(&mut history, "three", 1);
        let folded = fold(&history, "s", true).unwrap();
        // One whole item in front, as current as the last diff, and nothing left to send.
        assert_eq!(folded[0], update(&[], &env("2026-10-03")).unwrap());
        assert_eq!(folded[0], restated(&history).unwrap());
        assert_eq!(folded[1], user_message("one"));
        assert_eq!(update(&folded, &env("2026-10-03")), None);
    }

    #[test]
    fn both_compaction_paths_keep_the_latest_focused_progress() {
        let mut plan = crate::plan::Plan::parse(&json!({"plan": [
            {"step": "inspect", "status": "completed", "reason": "retrievable check: offline inspection passed"}, {"step": "verify", "status": "pending"}
        ]}))
        .unwrap()
        .unwrap();
        let mut history = vec![crate::plan::context(&[], Some(&plan)).unwrap()];
        turn(&mut history, "work", 1);
        turn(&mut history, "also verify", 1);
        plan.steps[1].status = crate::plan::Status::InProgress;
        history.extend(crate::plan::context(&history, Some(&plan)));
        let expected = crate::plan::restated_context(&history).unwrap();
        assert!(
            expected
                .to_string()
                .contains("retrievable check: offline inspection passed")
        );
        let folded = fold(&history, "summary", true).unwrap();
        let installed = install(
            &history,
            json!({"type": "compaction", "encrypted_content": "opaque"}),
            0,
            &ByteEstimate,
        );
        for kept in [folded, installed] {
            assert_eq!(
                kept.iter()
                    .filter(|item| crate::plan::is_context(item))
                    .collect::<Vec<_>>(),
                [&expected]
            );
            assert!(crate::plan::context(&kept, Some(&plan)).is_none());
        }
    }

    #[test]
    fn both_compaction_paths_keep_only_the_latest_goal_lifecycle() {
        use crate::goal::{Goal, is_lifecycle, lifecycle, restated_lifecycle};

        let mut goal = Goal::new("build");
        let mut history = vec![lifecycle(&[], Some(&goal)).unwrap()];
        turn(&mut history, "build it", 1);
        goal.pause("interrupted");
        history.push(lifecycle(&history, Some(&goal)).unwrap());
        turn(&mut history, "what happened?", 2);
        goal.state = crate::goal::State::Active;
        goal.note.clear();
        history.push(lifecycle(&history, Some(&goal)).unwrap());
        let expected = restated_lifecycle(&history).unwrap();
        let folded = fold(&history, "the goal was resumed", true).unwrap();
        let installed = install(
            &history,
            json!({"type": "compaction", "encrypted_content": "opaque"}),
            1000,
            &ByteEstimate,
        );
        for kept in [folded, installed] {
            assert_eq!(
                kept.iter()
                    .filter(|item| is_lifecycle(item))
                    .collect::<Vec<_>>(),
                [&expected]
            );
            assert!(lifecycle(&kept, Some(&goal)).is_none());
        }
    }

    #[test]
    fn both_compaction_paths_keep_only_the_latest_goal_specification() {
        use crate::goal::{Goal, Specification, context, is_context};

        let mut goal = Goal::new("build a generator");
        goal.update(
            Specification::parse(&json!({
                "objective": "build a generator",
                "requirements": ["Yul and Huff"],
                "verification": ["run tests"]
            }))
            .unwrap(),
        );
        let mut history = vec![context(&[], Some(&goal)).unwrap()];
        turn(&mut history, "build it", 1);
        goal.requirements
            .push("use state_4 directories".to_string());
        history.extend(context(&history, Some(&goal)));
        turn(&mut history, "also compare the outputs", 1);
        goal.verification
            .push("compare state_4 outputs".to_string());
        history.extend(context(&history, Some(&goal)));
        let expected = crate::goal::restated(&history).unwrap();
        let folded = fold(&history, "summary omitted all goal details", true).unwrap();
        let installed = install(
            &history,
            json!({"type": "compaction", "encrypted_content": "opaque"}),
            0,
            &ByteEstimate,
        );
        for kept in [folded, installed] {
            let specs: Vec<_> = kept.iter().filter(|item| is_context(item)).collect();
            assert_eq!(specs, [&expected]);
            assert!(context(&kept, Some(&goal)).is_none());
            let text = specs[0]["content"][0]["text"].as_str().unwrap();
            assert!(text.contains("state_4 directories"));
            assert!(text.contains("compare state_4 outputs"));
        }
    }

    #[test]
    fn the_summary_proposes_facts_for_later_sessions_apart_from_itself() {
        let text = request(None)["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(text.contains("`## For future sessions`"), "{text}");

        let summary = "Did things.\n\n## Left\nthe tests\n\n## For future sessions\n\
- `cargo test` needs `--features x`: the default build skips them\n  since 0.3.\n\
* uses pnpm, not npm\n2. the CI cache is per branch\n";
        let (kept, facts) = candidates(summary);
        assert_eq!(kept, "Did things.\n\n## Left\nthe tests");
        assert_eq!(
            facts,
            [
                "`cargo test` needs `--features x`: the default build skips them since 0.3.",
                "uses pnpm, not npm",
                "the CI cache is per branch",
            ]
        );
        // A later section of the same level ends it; `None.` proposes nothing.
        let (kept, facts) = candidates("s\n### For future sessions:\nNone.\n## After\nmore");
        assert_eq!((kept.as_str(), facts.len()), ("s\n## After\nmore", 0));
        let (kept, facts) = candidates("no section\n- a list");
        assert_eq!((kept.as_str(), facts.len()), ("no section\n- a list", 0));
    }

    /// A note saved mid-session and the session notes outlive eviction of the calls that
    /// set them and a fold or a server compaction of the turns they were in.
    #[test]
    fn both_compaction_paths_keep_the_notes_and_what_was_remembered() {
        use crate::notes::{State, context, is_context};

        let mut history = Vec::new();
        turn(&mut history, "one", 1);
        let remember = json!({"type": "function_call", "call_id": "r1", "name": "remember",
            "arguments": json!({"note": "uses pnpm"}).to_string()});
        history.push(remember);
        history.push(json!({"type": "function_call_output", "call_id": "r1",
            "output": format!("Saved to {}/.bhai/MEMORY.md.", "/w".repeat(200))}));
        let mut state = State {
            text: Some("## Rejected\nredis: no persistence".to_string()),
            remembered: vec!["- 2026-11-27: uses pnpm".to_string()],
        };
        history.extend(context(&history, &state));
        turn(&mut history, "two", KEEP_RESULTS);
        state.text = Some("## Rejected\nredis, then memcached".to_string());
        history.extend(context(&history, &state));
        turn(&mut history, "three", 1);

        evict(&mut history, &ByteEstimate, true);
        let output = history
            .iter()
            .find(|i| i["call_id"] == "r1" && is_output(i));
        assert!(evicted(output.unwrap()["output"].as_str().unwrap()));
        let item = json!({"type": "compaction", "encrypted_content": "opaque"});
        for kept in [
            fold(&history, "s", true).unwrap(),
            install(&history, item, 0, &ByteEstimate),
        ] {
            let restated: Vec<&Value> = kept.iter().filter(|i| is_context(i)).collect();
            assert_eq!(restated.len(), 2, "{restated:?}");
            let text = Value::from(kept.clone()).to_string();
            assert!(text.contains("redis, then memcached"));
            assert!(!text.contains("no persistence"));
            assert!(text.contains("2026-11-27: uses pnpm"));
            assert!(context(&kept, &state).is_empty());
        }
    }

    #[test]
    fn fold_and_install_keep_the_instruction_updates() {
        use crate::instructions::{File, updates};

        let file = |content: &str| File {
            path: "CLAUDE.md".into(),
            label: "./CLAUDE.md".to_string(),
            content: content.to_string(),
        };
        let prompt = [file("be terse")];
        let mut history = Vec::new();
        turn(&mut history, "one", 1);
        history.extend(updates(&history, &prompt, &[file("be verbose")]));
        turn(&mut history, "two", 1);
        turn(&mut history, "three", 1);
        let item = json!({"type": "compaction", "encrypted_content": "opaque"});
        for kept in [
            fold(&history, "s", true).unwrap(),
            install(&history, item, u64::MAX, &ByteEstimate),
        ] {
            assert_eq!(
                crate::instructions::note(&kept[0]).unwrap(),
                "instructions changed: ./CLAUDE.md"
            );
            assert!(updates(&kept, &prompt, &[file("be verbose")]).is_empty());
        }
    }
}
