//! The transcript entries and their per-entry token attribution, kept by the session so
//! the TUI badges, `/context` and `GET /state` all read the same numbers.

use std::collections::{BTreeMap, HashMap, VecDeque};

use serde_json::Value;

use crate::client::Usage;
use crate::profile::{self, CallTokens, EntryTokens, Tokens};
use crate::session::Event;

/// Characters of an entry the `/context` transcript table shows.
const LABEL_CHARS: usize = 40;
/// Bytes of a running command's output kept for the transcript; older output is dropped.
const LIVE_BYTES: usize = 16_000;

#[derive(Debug)]
pub enum Entry {
    User(String),
    Assistant(String),
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
    Rejected(String),
    Error(String),
    /// A turn that failed rather than answering. Unlike an `Error`, the history still
    /// stands behind it, so the transcript offers to run the turn again.
    Failed(String),
    Info(String),
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
            Event::User(message) => self.push(Entry::User(message.clone())),
            Event::Queued { .. } => {}
            Event::Text(delta) => self.append(delta, Stream::Assistant),
            Event::Reasoning(delta) => self.append(delta, Stream::Reasoning),
            Event::ToolStart { tool, summary } => self.push(Entry::Command {
                tool: tool.clone(),
                summary: summary.clone(),
            }),
            Event::ToolProgress(chunk) => self.progress(chunk),
            Event::ToolOutput(output) => {
                let entry = Entry::Output(output.trim_end().to_string());
                // The result takes the running entry's place.
                match self.running() {
                    Some(index) => self.list[index] = entry,
                    None => self.push(entry),
                }
            }
            Event::ToolRejected(command) => {
                self.push(Entry::Rejected(format!("rejected: {command}")))
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
            Event::Info(message) => self.push(Entry::Info(message.clone())),
            Event::Compacted(message) => {
                self.attribution.items.clear();
                self.push(Entry::Info(message.clone()));
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
                Some("message") if item["role"] == "user" => Entry::User(text("content")),
                Some("message") => Entry::Assistant(text("content")),
                Some("reasoning") if !text("summary").is_empty() => {
                    Entry::Reasoning(text("summary"))
                }
                Some("function_call") => Entry::Command {
                    tool: item["name"].as_str().unwrap_or_default().to_string(),
                    summary: format!(
                        "{} {}",
                        item["name"].as_str().unwrap_or_default(),
                        item["arguments"].as_str().unwrap_or_default()
                    ),
                },
                Some("function_call_output") => Entry::Output(
                    item["output"]
                        .as_str()
                        .unwrap_or_default()
                        .trim_end()
                        .to_string(),
                ),
                _ => continue,
            };
            if matches!(entry, Entry::User(_) | Entry::Output(_)) {
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
        let text = of(|e| matches!(e, Entry::Assistant(_)));
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
                let result = last(|e| matches!(e, Entry::Output(_) | Entry::Rejected(_)));
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
            Stream::Reasoning => Entry::Reasoning(text),
        });
    }
}

impl Entry {
    pub fn text(&self) -> &str {
        match self {
            Entry::User(t)
            | Entry::Assistant(t)
            | Entry::Reasoning(t)
            | Entry::Command { summary: t, .. }
            | Entry::Running { tail: t, .. }
            | Entry::Output(t)
            | Entry::Rejected(t)
            | Entry::Error(t)
            | Entry::Failed(t)
            | Entry::Info(t) => t,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Entry::User(_) => "user",
            Entry::Assistant(_) => "assistant",
            Entry::Reasoning(_) => "thinking",
            Entry::Command { .. } => "command",
            Entry::Running { .. } => "running",
            Entry::Output(_) => "output",
            Entry::Rejected(_) => "rejected",
            Entry::Error(_) => "error",
            Entry::Failed(_) => "failed",
            Entry::Info(_) => "info",
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

#[derive(Clone, Copy)]
enum Stream {
    Assistant,
    Reasoning,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Method;

    fn usage(input: u64, cached: u64, output: u64, reasoning: u64) -> Usage {
        Usage {
            input,
            cached,
            output,
            reasoning,
        }
    }

    fn start(tool: &str, summary: &str) -> Event {
        Event::ToolStart {
            tool: tool.to_string(),
            summary: summary.to_string(),
        }
    }

    /// Entries that start with the TUI's intro, as the app shows them.
    fn intro() -> Entries {
        let mut entries = Entries::default();
        entries.push(Entry::Info("intro".to_string()));
        entries
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
        app.apply(&Event::ToolRejected("ls".to_string()));
        app.apply(&Event::Item(2));
        let rejected = app.tokens[&2];
        assert_eq!(rejected.output, Some(3));
        assert_eq!(rejected.call, Some(usage(10, 0, 3, 0)));
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
}
