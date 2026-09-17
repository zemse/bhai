//! Keeping history inside the context window: old tool outputs are evicted first, and
//! only when that is not enough are the earlier turns replaced by a summary.

use serde_json::{Value, json};

use crate::tokens::{self, Tokenizer};

/// The context window of the gpt-5 family, in tokens.
pub const GPT5_WINDOW: u64 = 272_000;
/// The default fraction of the window a call may read before history is compacted.
pub const COMPACT_AT: f64 = 0.8;
/// Compaction aims for this fraction of the window.
const TARGET: f64 = 0.6;
/// The most recent tool results are never evicted.
const KEEP_RESULTS: usize = 6;
/// How the summary starts in the compacted history.
pub const SUMMARY_PREFIX: &str = "Summary of earlier conversation:";
/// What the model is asked for when history is summarised.
const REQUEST: &str = "Write a concise summary of the conversation so far, for yourself to \
continue from once the older messages are gone: the user's goals, decisions made, files \
touched, what is done and what is left. Do not call any tools; reply with the summary only.";

/// When to compact, from the config.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// The context window in tokens; unset means the model's own, if known.
    pub window: Option<u64>,
    pub compact_at: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            window: None,
            compact_at: COMPACT_AT,
        }
    }
}

impl Limits {
    /// The window for `model`: the configured one, else the gpt-5 family's.
    pub fn window(&self, model: &str) -> Option<u64> {
        self.window
            .or_else(|| model.starts_with("gpt-5").then_some(GPT5_WINDOW))
    }

    /// Whether a call that read `input` tokens filled the window past `compact_at`.
    pub fn over(&self, model: &str, input: u64) -> bool {
        self.window(model)
            .is_some_and(|window| input as f64 > self.compact_at * window as f64)
    }

    /// The size compaction aims for, in tokens, never above `compact_at`.
    pub fn target(&self, model: &str) -> Option<u64> {
        self.window(model)
            .map(|window| (TARGET.min(self.compact_at) * window as f64) as u64)
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

/// The request appended to history for the summary call.
pub fn request() -> Value {
    user_message(REQUEST)
}

/// Tokens the model reads in `items`, encrypted reasoning not counted.
pub fn estimate(items: &[Value], tokenizer: &dyn Tokenizer) -> u64 {
    items
        .iter()
        .filter_map(tokens::item_text)
        .map(|text| tokenizer.count(&text) as u64)
        .sum()
}

/// Replace the output of the oldest tool results, never the last few, until `excess`
/// tokens are freed or none are left. The call items stay, so every call keeps its
/// output. Returns the tokens freed.
pub fn evict(history: &mut [Value], excess: u64, tokenizer: &dyn Tokenizer) -> u64 {
    let results: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, item)| is(item, "function_call_output"))
        .map(|(index, _)| index)
        .collect();
    let old = results.len().saturating_sub(KEEP_RESULTS);
    let mut freed = 0;
    for &index in &results[..old] {
        if freed >= excess {
            break;
        }
        let item = &mut history[index];
        let text = tokens::item_text(item).unwrap_or_default();
        let placeholder = format!("[output removed to save context: {} bytes]", text.len());
        let (before, after) = (tokenizer.count(&text), tokenizer.count(&placeholder));
        if text.starts_with("[output removed") || after >= before {
            continue;
        }
        item["output"] = json!(placeholder);
        freed += (before - after) as u64;
    }
    freed
}

/// The history after a summary: the first user message, the summary, and the last
/// user turn onward. `None` when there is no earlier turn to fold.
pub fn fold(history: &[Value], summary: &str) -> Option<Vec<Value>> {
    let mut users = history
        .iter()
        .enumerate()
        .filter(|(_, item)| is(item, "message") && item["role"] == "user")
        .map(|(index, _)| index);
    let first = users.next()?;
    let last = users.next_back().filter(|&last| last > first)?;
    let mut folded = vec![
        history[first].clone(),
        user_message(&format!("{SUMMARY_PREFIX}\n{}", summary.trim())),
    ];
    folded.extend_from_slice(&history[last..]);
    Some(folded)
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
    fn limits_know_the_gpt5_window() {
        let limits = Limits::default();
        assert_eq!(limits.window("gpt-5.1-codex"), Some(GPT5_WINDOW));
        assert_eq!(limits.window("other"), None);
        assert!(limits.over("gpt-5", 220_000));
        assert!(!limits.over("gpt-5", 210_000));
        assert!(!limits.over("other", u64::MAX));
        let set = Limits {
            window: Some(1000),
            compact_at: 0.7,
        };
        assert!(set.over("other", 701));
        assert_eq!(set.target("other"), Some(600));
        let low = Limits {
            compact_at: 0.5,
            ..set
        };
        assert_eq!(low.target("other"), Some(500));
    }

    #[test]
    fn eviction_keeps_pairs_and_the_last_six_results() {
        let mut history = Vec::new();
        turn(&mut history, "one", 5);
        turn(&mut history, "two", 5);
        let before = history.clone();

        let freed = evict(&mut history, u64::MAX, &ByteEstimate);
        assert_eq!(history.len(), before.len());
        let outputs: Vec<&Value> = history
            .iter()
            .filter(|i| is(i, "function_call_output"))
            .collect();
        let evicted = outputs
            .iter()
            .filter(|o| o["output"] == "[output removed to save context: 400 bytes]")
            .count();
        assert_eq!(evicted, 4);
        assert!(
            outputs[4..]
                .iter()
                .all(|o| o["output"].as_str().unwrap().len() == 400)
        );
        assert_eq!(freed, 4 * (100 - 11));
        // Only outputs changed; calls and their ids are untouched.
        for (old, new) in before.iter().zip(&history) {
            assert_eq!(old["call_id"], new["call_id"]);
            if !is(old, "function_call_output") {
                assert_eq!(old, new);
            }
        }

        // It stops once enough is freed, and never evicts twice.
        let mut history = before.clone();
        assert_eq!(evict(&mut history, 1, &ByteEstimate), 89);
        assert_eq!(evict(&mut history, u64::MAX, &ByteEstimate), 3 * 89);
    }

    #[test]
    fn fold_keeps_the_first_message_the_summary_and_the_last_turn() {
        let mut history = Vec::new();
        turn(&mut history, "one", 1);
        turn(&mut history, "two", 2);
        turn(&mut history, "three", 1);
        let folded = fold(&history, " did things \n").unwrap();
        assert_eq!(folded[0], history[0]);
        assert_eq!(
            folded[1]["content"][0]["text"],
            "Summary of earlier conversation:\ndid things"
        );
        let last = history.len() - 5;
        assert_eq!(folded[2], user_message("three"));
        assert_eq!(&folded[2..], &history[last..]);

        // A single turn has nothing earlier to fold.
        assert_eq!(fold(&history[..5], "s"), None);
        assert_eq!(fold(&[], "s"), None);
    }
}
