//! The transcript entries and their per-entry token attribution, kept by the session so
//! the TUI badges, `/context` and `GET /state` all read the same numbers.

use std::collections::{BTreeMap, HashMap, VecDeque};

use serde_json::Value;

use crate::agent::Rejecter;
use crate::client::Usage;
use crate::profile::{self, CallTokens, EntryTokens, Tokens};
use crate::session::Event;

/// Characters of an entry the `/context` transcript table shows.
const LABEL_CHARS: usize = 40;
/// Bytes of a running command's output kept for the transcript; older output is dropped.
const LIVE_BYTES: usize = 16_000;

#[derive(Debug, Clone, Hash)]
pub enum Entry {
    User(String),
    Assistant(String),
    /// What the model said before carrying on, as opposed to its answer.
    Commentary(String),
    Reasoning(String),
    /// A tool call, kept with the tool that ran it so the transcript can draw a shell
    /// command as one and everything else as what it is.
    Command {
        tool: String,
        summary: String,
    },
    /// Output of a command still running: its latest bytes and its complete lines so far.
    Running {
        tail: String,
        lines: usize,
    },
    Output(String),
    /// The diff an edit or write made, under its command; folded until opened.
    Diff(String),
    /// A call that did not run, under the command it would have been. `reason` is empty
    /// when the user gave none.
    Rejected {
        by: Rejecter,
        reason: String,
    },
    Error(String),
    /// A turn that failed rather than answering. Unlike an `Error`, the history still
    /// stands behind it, so the transcript offers to run the turn again.
    Failed(String),
    Info(String),
    /// How long the turn above it ran and when it ended.
    Done(String),
    /// The summary a compaction folded the earlier turns into. It is the context the
    /// conversation carries from here, so the transcript shows it where it happened.
    Summary(String),
}

/// Ties history items and calls to the entries that show them.
#[derive(Default)]
struct Attribution {
    /// History index to entry, for user messages and tool results.
    items: BTreeMap<usize, usize>,
    /// Entries from here on are not yet tied to a call or item.
    mark: usize,
    /// Output tokens of each function call still waiting for its result.
    calls: VecDeque<u64>,
    /// Totals of a call that wrote no text, for its first function call entry.
    totals: Option<Usage>,
    /// Child agent usage since the last tool result.
    child: Option<Usage>,
}

/// The transcript as shown, with token attribution by entry index.
#[derive(Default)]
pub struct Entries {
    pub list: Vec<Entry>,
    pub tokens: HashMap<usize, Tokens>,
    attribution: Attribution,
}

impl Entries {
    /// Add an entry that no call or item accounts for.
    pub fn push(&mut self, entry: Entry) {
        self.list.push(entry);
    }

    /// Update the transcript for a published event.
    pub fn apply(&mut self, event: &Event) {
        match event {
            // The transcript is the conversation the model is having, so a message
            // only joins it when the turn it belongs to starts. A prompt still waiting
            // in the queue is shown above the prompt box instead, where the eye that
            // typed it already is, and where it is plainly not part of the history yet.
            Event::User(message) | Event::Steered(message) => {
                self.push(Entry::User(message.clone()))
            }
            // Nothing was typed: the reports themselves follow, as the tool results
            // they are, so this only says why the agent started working again.
            Event::Resumed(what) => self.push(Entry::Info(format!("resumed {what}"))),
            Event::Queued { .. } => {}
            Event::Text(delta) => self.append(delta, Stream::Assistant),
            Event::Commentary(delta) => self.append(delta, Stream::Commentary),
            Event::Reasoning(delta) => self.append(delta, Stream::Reasoning),
            Event::ToolStart {
                tool,
                summary,
                preview,
            } => {
                self.push(Entry::Command {
                    tool: tool.clone(),
                    summary: summary.clone(),
                });
                if let Some(diff) = preview {
                    self.push(Entry::Diff(diff.clone()));
                }
            }
            Event::ToolProgress(chunk) => self.progress(chunk),
            Event::ToolOutput(output) => {
                let entry = Entry::Output(output.trim_end().to_string());
                // The result takes the running entry's place.
                match self.running() {
                    Some(index) => self.list[index] = entry,
                    None => self.push(entry),
                }
            }
            Event::ToolRejected {
                tool,
                summary,
                by,
                reason,
            } => {
                self.push(Entry::Command {
                    tool: tool.clone(),
                    summary: summary.clone(),
                });
                self.push(Entry::Rejected {
                    by: *by,
                    reason: reason.clone(),
                });
            }
            Event::ChildUsage(usage) => {
                let child = self.attribution.child.get_or_insert_default();
                child.input += usage.input;
                child.cached += usage.cached;
                child.output += usage.output;
                child.reasoning += usage.reasoning;
            }
            Event::Call(call) => self.on_call(call),
            Event::Item(index) => self.on_item(*index),
            Event::Cache(Some(found)) => self.push(Entry::Error(format!(
                "cache break: {}: {}",
                found.field, found.detail
            ))),
            Event::CacheStalled(misses) => self.push(Entry::Info(format!(
                "cache missed {misses} calls in a row; the prefix may no longer be served"
            ))),
            Event::Info(message) | Event::Compacting(message) => {
                self.push(Entry::Info(message.clone()))
            }
            Event::Retrying(message) => {
                self.unstream();
                self.push(Entry::Info(message.clone()));
            }
            Event::Compacted {
                notice, summary, ..
            } => {
                self.attribution.items.clear();
                self.push(Entry::Info(notice.clone()));
                if let Some(summary) = summary {
                    self.push(Entry::Summary(summary.clone()));
                }
            }
            // The conversation is gone, so the transcript of it goes with it.
            Event::Cleared => {
                self.list.clear();
                self.tokens.clear();
                self.attribution = Attribution::default();
                self.push(Entry::Info("conversation cleared".to_string()));
            }
            Event::Error(message) => self.push(Entry::Error(message.clone())),
            Event::TurnFailed(message) => self.push(Entry::Failed(message.clone())),
            Event::Interrupted => self.push(Entry::Info("interrupted".to_string())),
            Event::Done { seconds, at, verb } => self.push(Entry::Done(format!(
                "{} {verb} \u{b7} {at} khatam",
                took(*seconds)
            ))),
            _ => {}
        }
    }

    /// Show a history resumed from disk. Encrypted reasoning without a summary is skipped.
    pub fn restore(&mut self, history: &[Value]) {
        for (index, item) in history.iter().enumerate() {
            let text = |key: &str| {
                item[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let entry = match item["type"].as_str() {
                Some("message") if item["role"] == "user" => match summarised(&text("content")) {
                    Some(summary) => Entry::Summary(summary),
                    // A child's report is carried in a user message, since the call that
                    // started the child was answered long before. It is not something
                    // the user said, so it is not shown as one.
                    None => match reported(&text("content")) {
                        Some(report) => Entry::Output(report),
                        // A goal turn's opening is the harness's, not the user's.
                        None if text("content").starts_with(crate::goal::CONTINUE) => {
                            Entry::Info("resumed on the goal".to_string())
                        }
                        None if text("content") == crate::agent::TURN_ABORTED => {
                            Entry::Info("interrupted".to_string())
                        }
                        None => Entry::User(text("content")),
                    },
                },
                Some("message") if crate::environment::is_context(item) => continue,
                Some("message") if crate::client::is_commentary(item) => {
                    Entry::Commentary(text("content"))
                }
                Some("message") => Entry::Assistant(text("content")),
                Some("reasoning") if !text("summary").is_empty() => {
                    Entry::Reasoning(text("summary"))
                }
                Some("function_call") => {
                    // A function `tool_search` loaded is named with its namespace.
                    let name = format!(
                        "{}{}",
                        item["namespace"].as_str().unwrap_or_default(),
                        item["name"].as_str().unwrap_or_default()
                    );
                    Entry::Command {
                        summary: format!(
                            "{name} {}",
                            item["arguments"].as_str().unwrap_or_default()
                        ),
                        tool: name,
                    }
                }
                Some("custom_tool_call") => {
                    let name = item["name"].as_str().unwrap_or_default();
                    let input = item["input"].as_str().unwrap_or_default();
                    Entry::Command {
                        summary: crate::tools::patch::summarize(input)
                            .filter(|_| name == crate::tools::patch::NAME)
                            .unwrap_or_else(|| format!("{name} {input}")),
                        tool: name.to_string(),
                    }
                }
                Some("function_call_output" | "custom_tool_call_output") => Entry::Output(
                    item["output"]
                        .as_str()
                        .unwrap_or_default()
                        .trim_end()
                        .to_string(),
                ),
                _ => continue,
            };
            if matches!(entry, Entry::User(_) | Entry::Summary(_) | Entry::Output(_)) {
                self.attribution.items.insert(index, self.list.len());
            }
            self.list.push(entry);
        }
        self.attribution.mark = self.list.len();
    }

    /// The badge numbers of every attributed entry, in transcript order.
    pub fn attributed(&self) -> Vec<EntryTokens> {
        let mut entries: Vec<EntryTokens> = self
            .tokens
            .iter()
            .map(|(&index, &tokens)| {
                let entry = &self.list[index];
                EntryTokens {
                    index,
                    kind: entry.kind(),
                    label: entry.label(),
                    tokens,
                }
            })
            .collect();
        entries.sort_by_key(|e| e.index);
        entries
    }

    /// Tie a finished call to the entries it read and wrote.
    fn on_call(&mut self, call: &CallTokens) {
        let first = call.sent - call.inputs.len();
        for (offset, &input) in call.inputs.iter().enumerate() {
            if let Some(&entry) = self.attribution.items.get(&(first + offset)) {
                let tokens = self.tokens.entry(entry).or_default();
                tokens.input = Some(input);
                tokens.method = call.method;
            }
        }
        // The usage only says how much of the whole call was cached, so each resent
        // entry takes the call's cached ratio: the per-entry split is proportional, not
        // exact.
        let ratio = match call.usage.input {
            0 => 0.0,
            input => call.usage.cached as f64 / input as f64,
        };
        for entry in self.attribution.items.range(..first).map(|(_, e)| *e) {
            let tokens = self.tokens.entry(entry).or_default();
            if let Some(input) = tokens.input {
                tokens.resends += 1;
                tokens.cached += (input as f64 * ratio).round() as u64;
            }
        }

        let fresh = self.attribution.mark..self.list.len();
        let of = |want: fn(&Entry) -> bool| -> Vec<usize> {
            fresh.clone().filter(|&i| want(&self.list[i])).collect()
        };
        let text = of(|e| matches!(e, Entry::Assistant(_) | Entry::Commentary(_)));
        let thinking = of(|e| matches!(e, Entry::Reasoning(_)));
        self.share(&text, call.text, |tokens, part| tokens.output = Some(part));
        self.share(&thinking, call.usage.reasoning, |tokens, part| {
            tokens.reasoning = Some(part)
        });
        match (text.last(), thinking.last()) {
            (Some(&entry), _) => self.tokens.entry(entry).or_default().call = Some(call.usage),
            _ if !call.calls.is_empty() => self.attribution.totals = Some(call.usage),
            (None, Some(&entry)) => self.tokens.entry(entry).or_default().call = Some(call.usage),
            (None, None) => {}
        }
        self.attribution.calls.extend(call.calls.iter().copied());
        self.attribution.mark = self.list.len();
    }

    /// Split `total` over `entries` by their text length.
    fn share(&mut self, entries: &[usize], total: u64, set: impl Fn(&mut Tokens, u64)) {
        let weights: Vec<u64> = entries
            .iter()
            .map(|&i| self.list[i].text().len() as u64)
            .collect();
        for (&entry, part) in entries.iter().zip(profile::split(total, &weights)) {
            set(self.tokens.entry(entry).or_default(), part);
        }
    }

    /// Tie history item `index` to the entry just shown: a tool result when a function
    /// call is waiting for one, else the user message.
    fn on_item(&mut self, index: usize) {
        let fresh = self.attribution.mark..self.list.len();
        let last = |want: fn(&Entry) -> bool| fresh.clone().rev().find(|&i| want(&self.list[i]));
        match self.attribution.calls.pop_front() {
            Some(output) => {
                // A child's commands come after the parent's own, and its results before.
                let command = fresh
                    .clone()
                    .find(|&i| matches!(self.list[i], Entry::Command { .. }));
                let result = last(|e| matches!(e, Entry::Output(_) | Entry::Rejected { .. }));
                if let Some(entry) = command.or(result) {
                    let tokens = self.tokens.entry(entry).or_default();
                    tokens.output = Some(output);
                    tokens.call = self.attribution.totals.take().or(tokens.call);
                }
                let child = self.attribution.child.take();
                if let Some(entry) = result {
                    self.attribution.items.insert(index, entry);
                    if child.is_some() {
                        self.tokens.entry(entry).or_default().child = child;
                    }
                }
            }
            None => {
                if let Some(entry) = last(|e| matches!(e, Entry::User(_))) {
                    self.attribution.items.insert(index, entry);
                }
            }
        }
        self.attribution.mark = self.list.len();
    }

    /// Drop what the failed attempt of a call streamed: the run of text and reasoning at
    /// the end, back to the last entry already accounted for. Anything else ends the run,
    /// so an earlier attempt's retry notice stays.
    fn unstream(&mut self) {
        while self.list.len() > self.attribution.mark
            && matches!(
                self.list.last(),
                Some(Entry::Assistant(_) | Entry::Commentary(_) | Entry::Reasoning(_))
            )
        {
            self.list.pop();
        }
    }

    /// The running command entry, if a call is still in flight.
    fn running(&self) -> Option<usize> {
        (self.attribution.mark..self.list.len())
            .rev()
            .find(|&i| matches!(self.list[i], Entry::Running { .. }))
    }

    /// Add live output to the running entry, starting one on the first chunk.
    fn progress(&mut self, chunk: &str) {
        let index = self.running().unwrap_or_else(|| {
            self.list.push(Entry::Running {
                tail: String::new(),
                lines: 0,
            });
            self.list.len() - 1
        });
        let Entry::Running { tail, lines } = &mut self.list[index] else {
            return;
        };
        *lines += chunk.matches('\n').count();
        tail.push_str(chunk);
        if tail.len() > LIVE_BYTES {
            let mut cut = tail.len() - LIVE_BYTES;
            while !tail.is_char_boundary(cut) {
                cut += 1;
            }
            tail.drain(..cut);
        }
    }

    /// Append a streaming delta to the last entry of the same kind, or start a new one.
    /// Entries of a call already accounted for are never extended.
    fn append(&mut self, delta: &str, kind: Stream) {
        let open = self.list.len() > self.attribution.mark;
        match (self.list.last_mut().filter(|_| open), kind) {
            (Some(Entry::Assistant(text)), Stream::Assistant)
            | (Some(Entry::Commentary(text)), Stream::Commentary)
            | (Some(Entry::Reasoning(text)), Stream::Reasoning) => {
                text.push_str(delta);
                return;
            }
            _ => {}
        }
        // Drop the leading whitespace a new block often starts with.
        let text = delta.trim_start().to_string();
        if text.is_empty() {
            return;
        }
        self.list.push(match kind {
            Stream::Assistant => Entry::Assistant(text),
            Stream::Commentary => Entry::Commentary(text),
            Stream::Reasoning => Entry::Reasoning(text),
        });
    }
}

impl Entry {
    pub fn text(&self) -> &str {
        match self {
            Entry::User(t)
            | Entry::Assistant(t)
            | Entry::Commentary(t)
            | Entry::Reasoning(t)
            | Entry::Command { summary: t, .. }
            | Entry::Running { tail: t, .. }
            | Entry::Output(t)
            | Entry::Diff(t)
            | Entry::Rejected { reason: t, .. }
            | Entry::Error(t)
            | Entry::Failed(t)
            | Entry::Info(t)
            | Entry::Done(t)
            | Entry::Summary(t) => t,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Entry::User(_) => "user",
            Entry::Assistant(_) => "assistant",
            Entry::Commentary(_) => "commentary",
            Entry::Reasoning(_) => "thinking",
            Entry::Command { .. } => "command",
            Entry::Running { .. } => "running",
            Entry::Output(_) => "output",
            Entry::Diff(_) => "diff",
            Entry::Rejected { .. } => "rejected",
            Entry::Error(_) => "error",
            Entry::Failed(_) => "failed",
            Entry::Info(_) => "info",
            Entry::Done(_) => "done",
            Entry::Summary(_) => "summary",
        }
    }

    /// The first line, cut to `LABEL_CHARS`.
    pub fn label(&self) -> String {
        let line = self.text().trim().lines().next().unwrap_or_default();
        match line.char_indices().nth(LABEL_CHARS) {
            Some((end, _)) => format!("{}...", &line[..end]),
            None => line.to_string(),
        }
    }
}

/// The report inside a message carrying a finished child's result, without the line
/// that marks it as one.
fn reported(text: &str) -> Option<String> {
    let rest = text.strip_prefix(crate::agent::CHILD_RESULT)?;
    // What follows the marker on that first line is for the model, not the transcript.
    let body = rest.split_once("\n\n").map_or(rest, |(_, body)| body);
    Some(body.trim_start().to_string())
}

/// The summary inside a folded user message, without the line that marks it as one.
fn summarised(text: &str) -> Option<String> {
    let rest = text.strip_prefix(crate::compact::SUMMARY_PREFIX)?;
    Some(rest.trim_start().to_string())
}

#[derive(Clone, Copy)]
enum Stream {
    Assistant,
    Commentary,
    Reasoning,
}

/// What a running turn is said to be doing and what it is said to have done once it
/// ends, one pair picked at random for each turn. The second reads after the duration,
/// as in `9m 54s chabāyā`.
pub const VERBS: &[(&str, &str)] = &[
    ("chabārau", "chabāyā"),
    ("pakārau", "pakāyā"),
    ("pachārau", "pachāyā"),
    ("ghisārau", "ghisā"),
    ("ragḍārau", "ragḍā"),
    ("pīsārau", "pīsā"),
    ("nichoḍārau", "nichoḍā"),
    ("khodārau", "khodā"),
    ("ṭhokārau", "ṭhokā"),
    ("ubālārau", "ubālā"),
    ("bhūnārau", "bhūnā"),
    ("chhānārau", "chhānā"),
    ("mānjārau", "mānjā"),
    ("suljhārau", "suljhāyā"),
    ("joḍārau", "joḍā"),
    ("khīnchārau", "khīnchā"),
    ("machudārau", "machudāyā"),
    ("gāṇḍ marārau", "gāṇḍ marāī"),
    ("pelārau", "pelā"),
    ("kūṭārau", "kūṭā"),
    ("phoḍārau", "phoḍā"),
    ("bajārau", "bajāyā"),
    ("chhīlārau", "chhīlā"),
    ("jhāḍārau", "jhāḍā"),
    ("nipṭārau", "nipṭāyā"),
    ("pheṇṭārau", "pheṇṭā"),
    ("ghumārau", "ghumāyā"),
    ("jhelārau", "jhelā"),
    ("ukhāḍārau", "ukhāḍā"),
    ("fāḍārau", "fāḍā"),
    ("talārau", "talā"),
    ("bunārau", "bunā"),
    ("tapkārau", "tapkāyā"),
    ("uḍārau", "uḍāyā"),
    ("chepārau", "chepā"),
    ("patārau", "patāyā"),
    ("ghusārau", "ghusāyā"),
    ("vāṭ lagārau", "vāṭ lagā dī"),
    ("jugāḍ lagārau", "jugāḍ lagāyā"),
    ("setting baiṭhārau", "setting baiṭhāī"),
    ("bawāl machārau", "bawāl machā diyā"),
    ("katl-e-ām machārau", "katl-e-ām machā diyā"),
    ("gadar machārau", "gadar machā diyā"),
    ("lankā lagārau", "lankā lagā dī"),
    ("dhuāṃ uḍārau", "dhuāṃ uḍā diyā"),
    ("tabāhī machārau", "tabāhī machā dī"),
    ("bhasaḍ machārau", "bhasaḍ machā dī"),
    ("kāṇḍ kar rau", "kāṇḍ kar diyā"),
    ("lafḍā suljhārau", "lafḍā suljhāyā"),
    ("rāḍā kar rau", "rāḍā kiyā"),
    ("phaṭkā mārārau", "phaṭkā mārā"),
    ("chālu kar rau", "chālu kiyā"),
];

/// A pair from `VERBS`, at random.
pub fn verb() -> (&'static str, &'static str) {
    VERBS[(uuid::Uuid::new_v4().as_u128() % VERBS.len() as u128) as usize]
}

/// A turn's length as `54s`, `9m 54s` or `1h 3m`.
fn took(seconds: u64) -> String {
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{}h {}m", s / 3600, s % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Method;

    fn usage(input: u64, cached: u64, output: u64, reasoning: u64) -> Usage {
        Usage {
            input,
            cached,
            cache_write: 0,
            output,
            reasoning,
        }
    }

    fn start(tool: &str, summary: &str) -> Event {
        Event::ToolStart {
            tool: tool.to_string(),
            summary: summary.to_string(),
            preview: None,
        }
    }

    /// Entries that start with the TUI's intro, as the app shows them.
    fn intro() -> Entries {
        let mut entries = Entries::default();
        entries.push(Entry::Info("intro".to_string()));
        entries
    }

    #[test]
    fn a_finished_turn_shows_how_long_it_took_and_when() {
        assert_eq!(took(54), "54s");
        assert_eq!(took(594), "9m 54s");
        assert_eq!(took(3780), "1h 3m");
        let mut entries = Entries::default();
        entries.apply(&Event::Done {
            seconds: 594,
            at: "12:58 PM".to_string(),
            verb: "chabāyā".to_string(),
        });
        assert!(matches!(&entries.list[..],
            [Entry::Done(t)] if t == "9m 54s chabāyā \u{b7} 12:58 PM khatam"));
    }

    #[test]
    fn clearing_takes_the_transcript_with_the_conversation() {
        let mut app = intro();
        app.apply(&Event::User("hi".to_string()));
        app.apply(&Event::Item(0));
        app.apply(&Event::Text("hello".to_string()));
        app.apply(&Event::Cleared);
        assert!(
            matches!(app.list.as_slice(), [Entry::Info(t)] if t == "conversation cleared"),
            "{:?}",
            app.list
        );
        assert!(app.tokens.is_empty());
        // The next turn's items index the history that starts here.
        app.apply(&Event::User("again".to_string()));
        app.apply(&Event::Item(0));
        assert_eq!(app.attribution.items.get(&0), Some(&1));
    }

    #[test]
    fn calls_are_attributed_to_their_entries() {
        let mut app = intro();
        let text = |s: &str| s.to_string();
        app.apply(&Event::User(text("hi")));
        app.apply(&Event::Item(0));
        app.apply(&Event::Reasoning(text("think")));
        app.apply(&Event::Text(text("hello")));
        app.apply(&Event::Call(CallTokens {
            usage: usage(100, 0, 30, 10),
            sent: 1,
            outputs: 3,
            inputs: vec![5],
            method: Method::Tokenized,
            text: 12,
            calls: vec![8],
        }));
        app.apply(&start("agent", "agent worker: go"));
        app.apply(&Event::ChildUsage(usage(7, 2, 1, 0)));
        app.apply(&start("bash", "[child a worker] ls"));
        app.apply(&Event::ToolOutput(text("[child a worker] x")));
        app.apply(&Event::ToolOutput(text("child done")));
        app.apply(&Event::Item(4));
        // The next call's text starts an entry of its own.
        app.apply(&Event::Reasoning(text("more")));
        app.apply(&Event::Text(text("done")));
        app.apply(&Event::Call(CallTokens {
            usage: usage(150, 100, 5, 1),
            sent: 5,
            outputs: 2,
            inputs: vec![20],
            method: Method::Exact,
            text: 4,
            calls: vec![],
        }));
        assert_eq!(app.list.len(), 10);

        let get = |i: usize| app.tokens.get(&i).copied().unwrap_or_default();
        // The user message was tokenized, then resent once at the call's cached ratio.
        assert_eq!(
            get(1),
            Tokens {
                input: Some(5),
                method: Method::Tokenized,
                resends: 1,
                cached: 3,
                ..Tokens::default()
            }
        );
        assert_eq!(get(2).reasoning, Some(10));
        assert_eq!(get(3).output, Some(12));
        assert_eq!(get(3).call, Some(usage(100, 0, 30, 10)));
        assert_eq!(get(4).output, Some(8));
        assert_eq!(
            get(5),
            Tokens::default(),
            "child entries are not the parent's"
        );
        let result = get(7);
        assert_eq!((result.input, result.method), (Some(20), Method::Exact));
        assert_eq!(result.resends, 0);
        assert_eq!(result.child, Some(usage(7, 2, 1, 0)));
        assert_eq!(get(8).reasoning, Some(1));
        assert_eq!(get(9).output, Some(4));
        assert_eq!(get(9).call, Some(usage(150, 100, 5, 1)));
    }

    #[test]
    fn a_call_without_text_puts_its_totals_on_the_command() {
        let mut app = intro();
        app.apply(&Event::User("hi".to_string()));
        app.apply(&Event::Item(0));
        app.apply(&Event::Call(CallTokens {
            usage: usage(10, 0, 3, 0),
            sent: 1,
            outputs: 1,
            inputs: vec![1],
            calls: vec![3],
            ..CallTokens::default()
        }));
        app.apply(&Event::ToolRejected {
            tool: "bash".to_string(),
            summary: "ls".to_string(),
            by: Rejecter::User,
            reason: String::new(),
        });
        app.apply(&Event::Item(2));
        // A rejected call is drawn as its command too, and the call is put on that.
        let command = app.tokens[&2];
        assert_eq!(command.output, Some(3));
        assert_eq!(command.call, Some(usage(10, 0, 3, 0)));
        assert!(matches!(app.list[3], Entry::Rejected { .. }));
    }

    #[test]
    fn live_output_is_capped_and_gives_way_to_the_result() {
        let mut app = intro();
        app.apply(&Event::User("hi".to_string()));
        app.apply(&Event::Item(0));
        app.apply(&Event::Call(CallTokens {
            usage: usage(10, 0, 3, 0),
            sent: 1,
            outputs: 1,
            inputs: vec![1],
            calls: vec![3],
            ..CallTokens::default()
        }));
        app.apply(&start("bash", "yes"));
        for _ in 0..LIVE_BYTES {
            app.apply(&Event::ToolProgress("é\n".to_string()));
        }
        let Entry::Running { tail, lines } = &app.list[3] else {
            panic!("{:?}", app.list);
        };
        assert!(
            (LIVE_BYTES - 2..=LIVE_BYTES).contains(&tail.len()),
            "{}",
            tail.len()
        );
        assert_eq!(*lines, LIVE_BYTES);
        assert_eq!(app.list.len(), 4);

        app.apply(&Event::ToolOutput("exit code: 0\n".to_string()));
        app.apply(&Event::Item(2));
        assert_eq!(app.list.len(), 4);
        assert_eq!(app.list[3].text(), "exit code: 0");
        assert_eq!(app.attribution.items[&2], 3);
        assert_eq!(app.tokens[&2].output, Some(3));
    }

    #[test]
    fn an_edit_shows_its_diff_between_the_command_and_its_result() {
        let mut app = intro();
        app.apply(&Event::User("hi".to_string()));
        app.apply(&Event::Item(0));
        app.apply(&Event::Call(CallTokens {
            usage: usage(10, 0, 3, 0),
            sent: 1,
            outputs: 1,
            inputs: vec![1],
            calls: vec![3],
            ..CallTokens::default()
        }));
        app.apply(&Event::ToolStart {
            tool: "edit".to_string(),
            summary: "edit /f".to_string(),
            preview: Some("@@ -1 +1 @@\n-a\n+b".to_string()),
        });
        app.apply(&Event::ToolOutput("Edited /f: 1 exact match.".to_string()));
        app.apply(&Event::Item(2));
        assert!(matches!(&app.list[3], Entry::Diff(d) if d.ends_with("+b")));
        assert_eq!(app.list[3].kind(), "diff");
        // The call and its result are still tied to the command and the output.
        assert_eq!(app.tokens[&2].output, Some(3));
        assert_eq!(app.attribution.items[&2], 4);
    }

    #[test]
    fn a_restored_history_shows_as_entries() {
        let mut app = intro();
        let history = [
            serde_json::json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}),
            serde_json::json!({"type": "reasoning", "encrypted_content": "x", "summary": []}),
            serde_json::json!({"type": "function_call", "call_id": "c", "name": "bash", "arguments": "{}"}),
            serde_json::json!({"type": "function_call_output", "call_id": "c", "output": "ok\n"}),
            serde_json::json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]}),
        ];
        app.restore(&history);
        let kinds: Vec<_> = app.list[1..].iter().map(Entry::kind).collect();
        assert_eq!(kinds, ["user", "command", "output", "assistant"]);
        assert_eq!(app.list[3].text(), "ok");
        assert_eq!(app.attribution.items.get(&3), Some(&3));
    }

    #[test]
    fn commentary_is_kept_apart_from_the_answer_it_leads_to() {
        let mut app = intro();
        app.apply(&Event::Commentary("checking ".to_string()));
        app.apply(&Event::Commentary("first".to_string()));
        app.apply(&Event::Text("found it".to_string()));
        let kinds: Vec<_> = app.list[1..].iter().map(Entry::kind).collect();
        assert_eq!(kinds, ["commentary", "assistant"]);
        assert_eq!(app.list[1].text(), "checking first");

        let mut restored = intro();
        let message = |text: &str| serde_json::json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]});
        let mut preamble = message("checking first");
        preamble["phase"] = serde_json::json!("commentary");
        restored.restore(&[preamble, message("found it")]);
        let kinds: Vec<_> = restored.list[1..].iter().map(Entry::kind).collect();
        assert_eq!(kinds, ["commentary", "assistant"]);
    }

    #[test]
    fn a_retry_drops_what_the_failed_attempt_streamed() {
        let mut app = intro();
        app.apply(&Event::Text("earlier answer".to_string()));
        app.apply(&Event::Call(CallTokens {
            usage: usage(10, 0, 3, 0),
            sent: 1,
            outputs: 1,
            text: 3,
            ..CallTokens::default()
        }));
        app.apply(&Event::Reasoning("thinking".to_string()));
        app.apply(&Event::Text("half an ans".to_string()));
        app.apply(&Event::Retrying("retrying (2/3) in 0.5s: boom".to_string()));
        app.apply(&Event::Commentary("again".to_string()));
        app.apply(&Event::Retrying("retrying (3/3) in 1.5s: boom".to_string()));
        app.apply(&Event::Text("the answer".to_string()));
        let texts: Vec<_> = app.list[1..].iter().map(Entry::text).collect();
        assert_eq!(
            texts,
            [
                "earlier answer",
                "retrying (2/3) in 0.5s: boom",
                "retrying (3/3) in 1.5s: boom",
                "the answer"
            ]
        );
        assert_eq!(app.tokens[&1].output, Some(3));
    }

    #[test]
    fn a_compaction_shows_the_summary_it_folded_the_turns_into() {
        let mut app = intro();
        app.apply(&Event::User("hi".to_string()));
        app.apply(&Event::Item(0));
        app.apply(&Event::Compacted {
            notice: "compacted history (summarised earlier turns): ~9 -> ~4 tokens".to_string(),
            summary: Some("did things".to_string()),
            freed: 5,
        });
        let kinds: Vec<_> = app.list[1..].iter().map(Entry::kind).collect();
        assert_eq!(kinds, ["user", "info", "summary"]);
        assert_eq!(app.list[3].text(), "did things");
        // The indexes the entries were tied to no longer hold.
        assert!(app.attribution.items.is_empty());

        // Evicting says nothing about the conversation, so there is nothing to show.
        app.apply(&Event::Compacted {
            notice: "compacted history (evicted old tool outputs): ~4 -> ~2 tokens".to_string(),
            summary: None,
            freed: 2,
        });
        assert_eq!(app.list.len(), 5);
        assert_eq!(app.list[4].kind(), "info");
    }

    #[test]
    fn a_restored_summary_is_not_something_the_user_said() {
        let mut app = intro();
        let message = |text: &str| serde_json::json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]});
        let history = [
            message("hi"),
            message(&format!("{}\ndid things", crate::compact::SUMMARY_PREFIX)),
            message("carry on"),
        ];
        app.restore(&history);
        let kinds: Vec<_> = app.list[1..].iter().map(Entry::kind).collect();
        assert_eq!(kinds, ["user", "summary", "user"]);
        // The summary costs tokens like any other item, so it is still attributed.
        assert_eq!(app.list[2].text(), "did things");
        assert_eq!(app.attribution.items.get(&1), Some(&2));
    }

    #[test]
    fn a_restored_interrupt_marker_shows_as_the_interrupt() {
        let mut app = intro();
        let message = |text: &str| serde_json::json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]});
        app.restore(&[
            message("hi"),
            message(crate::agent::TURN_ABORTED),
            message("again"),
        ]);
        let shown: Vec<_> = app.list[1..].iter().map(|e| (e.kind(), e.text())).collect();
        assert_eq!(
            shown,
            [("user", "hi"), ("info", "interrupted"), ("user", "again")]
        );
    }

    #[test]
    fn a_restored_environment_context_is_not_shown() {
        let mut app = intro();
        let message = |text: &str| serde_json::json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]});
        let context =
            crate::environment::update(&[], &crate::environment::Environment::default()).unwrap();
        app.restore(&[context, message("hi")]);
        let shown: Vec<_> = app.list[1..].iter().map(|e| (e.kind(), e.text())).collect();
        assert_eq!(shown, [("user", "hi")]);
        assert_eq!(app.attribution.items.get(&1), Some(&1));
    }
}
