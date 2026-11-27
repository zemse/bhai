//! What the main agent holds on to for the rest of a session: the notes it keeps with
//! `update_notes`, one short document rewritten as a whole, and the memory notes it saved
//! with `remember` after the system prompt was built. Each reaches the model as a
//! developer item appended to the history, restated after every compaction, and is handed
//! to each child started after it changed.

use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{Value, json};

/// The most the notes may be, in bytes.
pub const BUDGET: usize = 4 * 1024;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct State {
    /// The notes as last written; `None` before any are, or once cleared.
    pub text: Option<String>,
    /// The lines `remember` saved this session, which the system prompt does not carry.
    pub remembered: Vec<String>,
}

pub type Shared = Arc<Mutex<State>>;

pub fn lock(shared: &Shared) -> MutexGuard<'_, State> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

const NOTES: &str = "<session_notes>";
const NOTES_END: &str = "</session_notes>";
const NOTES_HEAD: &str = "Your session notes, as you last wrote them with `update_notes`:";
const NO_NOTES: &str = "No session notes.";
const REMEMBERED: &str = "<remembered>";
const REMEMBERED_HEAD: &str = "Saved to the project's memory file during this session, after \
the system prompt was written, and in force from now on:";

fn tagged(item: &Value, tag: &str) -> bool {
    item["role"] == "developer"
        && item
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .is_some_and(|s| s.starts_with(tag))
}

pub fn is_context(item: &Value) -> bool {
    tagged(item, NOTES) || tagged(item, REMEMBERED)
}

fn developer(text: String) -> Value {
    json!({"type": "message", "role": "developer", "content": [{
        "type": "input_text", "text": text
    }]})
}

fn latest(history: &[Value], tag: &str) -> Option<Value> {
    history.iter().rev().find(|item| tagged(item, tag)).cloned()
}

/// The latest notes and the latest remembered lines in `history`, for a compacted history.
pub fn restated(history: &[Value]) -> Vec<Value> {
    [latest(history, NOTES), latest(history, REMEMBERED)]
        .into_iter()
        .flatten()
        .collect()
}

/// The items the next call needs so the history says what `state` holds: none when it
/// already does.
pub fn context(history: &[Value], state: &State) -> Vec<Value> {
    let mut items = Vec::new();
    let was = latest(history, NOTES);
    if state.text.is_some() || was.is_some() {
        let text = match &state.text {
            Some(text) => format!("{NOTES}\n{NOTES_HEAD}\n{text}\n{NOTES_END}"),
            None => format!("{NOTES}\n{NO_NOTES}\n{NOTES_END}"),
        };
        let item = developer(text);
        if was.as_ref() != Some(&item) {
            items.push(item);
        }
    }
    // Lines saved are never taken back, so there is nothing to say when there are none.
    if !state.remembered.is_empty() {
        let item = developer(format!(
            "{REMEMBERED}\n{}\n{}\n</remembered>",
            REMEMBERED_HEAD,
            state.remembered.join("\n")
        ));
        if latest(history, REMEMBERED).as_ref() != Some(&item) {
            items.push(item);
        }
    }
    items
}

/// The notes the latest item in `history` carries, for a session saved without a record
/// of them.
pub fn from_history(history: &[Value]) -> Option<String> {
    let item = latest(history, NOTES)?;
    let text = item.pointer("/content/0/text")?.as_str()?;
    let body = text
        .strip_prefix(NOTES)?
        .strip_suffix(NOTES_END)?
        .trim_matches('\n');
    body.strip_prefix(NOTES_HEAD)
        .map(|text| text.trim_start_matches('\n').to_string())
}

/// What a child started now is told after its task: what its system prompt, built from
/// the memory file as the session started, does not carry.
pub fn for_child(shared: &Shared) -> Option<String> {
    let state = lock(shared);
    let mut parts = Vec::new();
    if !state.remembered.is_empty() {
        parts.push(format!(
            "Notes saved to the project's memory file since your system prompt was written:\n{}",
            state.remembered.join("\n")
        ));
    }
    if let Some(text) = &state.text {
        parts.push(format!(
            "The parent session's notes, for context:\n<session_notes>\n{text}\n</session_notes>"
        ));
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// A child's `task`, with what `for_child` tells it after.
pub fn task(task: String, shared: &Shared) -> String {
    match for_child(shared) {
        Some(told) => format!("{task}\n\n{told}"),
        None => task,
    }
}

/// `text` with the `## name` section's body replaced by `body`: added at the end when there
/// is no such section, and removed, heading too, when `body` is empty.
pub fn edit_section(text: &str, name: &str, body: &str) -> String {
    let name = name.trim().trim_start_matches('#').trim();
    let heading = |line: &str| line.starts_with("## ") && line[3..].trim() == name;
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.iter().position(|line| heading(line));
    let body = body.trim_matches('\n');
    let section = match body.trim().is_empty() {
        true => Vec::new(),
        false => vec![format!("## {name}"), body.to_string()],
    };
    let mut out: Vec<String> = Vec::new();
    match start {
        Some(start) => {
            let end = lines[start + 1..]
                .iter()
                .position(|line| line.starts_with("# ") || line.starts_with("## "))
                .map_or(lines.len(), |at| start + 1 + at);
            out.extend(lines[..start].iter().map(|s| s.to_string()));
            if !section.is_empty() {
                out.extend(section);
                if end < lines.len() {
                    out.push(String::new());
                }
            }
            out.extend(lines[end..].iter().map(|s| s.to_string()));
        }
        None => {
            out.extend(lines.iter().map(|s| s.to_string()));
            if !section.is_empty() {
                if out.last().is_some_and(|line| !line.trim().is_empty()) {
                    out.push(String::new());
                }
                out.extend(section);
            }
        }
    }
    out.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(text: Option<&str>, remembered: &[&str]) -> State {
        State {
            text: text.map(str::to_string),
            remembered: remembered.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn context_says_each_change_once() {
        let mut history = Vec::new();
        assert!(context(&history, &State::default()).is_empty());

        let first = state(Some("## Decisions\nuse sqlite"), &[]);
        let items = context(&history, &first);
        assert_eq!(items.len(), 1);
        assert!(is_context(&items[0]));
        history.extend(items);
        assert!(context(&history, &first).is_empty());
        assert_eq!(
            from_history(&history).as_deref(),
            Some("## Decisions\nuse sqlite")
        );

        let saved = state(
            Some("## Decisions\nuse sqlite"),
            &["- 2026-11-27: uses pnpm"],
        );
        let items = context(&history, &saved);
        assert_eq!(items.len(), 1);
        let text = items[0]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("<remembered>\n") && text.contains("uses pnpm"));
        history.extend(items);

        // Cleared notes are said to be gone; remembered lines stay.
        let cleared = state(None, &["- 2026-11-27: uses pnpm"]);
        let items = context(&history, &cleared);
        assert_eq!(items.len(), 1);
        assert!(items[0].to_string().contains(NO_NOTES));
        history.extend(items);
        assert_eq!(from_history(&history), None);
        assert!(context(&history, &cleared).is_empty());
        assert_eq!(restated(&history).len(), 2);
    }

    #[test]
    fn a_child_is_told_what_its_prompt_does_not_carry() {
        let shared = Shared::default();
        assert_eq!(for_child(&shared), None);
        *lock(&shared) = state(Some("rejected: a cache file"), &["- d: uses pnpm"]);
        let told = for_child(&shared).unwrap();
        assert!(told.contains("since your system prompt was written:\n- d: uses pnpm"));
        assert!(told.contains("<session_notes>\nrejected: a cache file\n</session_notes>"));
    }

    #[test]
    fn a_section_is_replaced_added_or_removed() {
        let text = "## Decisions\nsqlite\n\n## Open\nwhy slow?";
        assert_eq!(
            edit_section(text, "Decisions", "postgres\nbecause of locks"),
            "## Decisions\npostgres\nbecause of locks\n\n## Open\nwhy slow?"
        );
        assert_eq!(
            edit_section(text, "## Open", "none"),
            "## Decisions\nsqlite\n\n## Open\nnone"
        );
        assert_eq!(
            edit_section(text, "Rejected", "redis: no persistence"),
            format!("{text}\n\n## Rejected\nredis: no persistence")
        );
        assert_eq!(edit_section(text, "Decisions", ""), "## Open\nwhy slow?");
        assert_eq!(edit_section("", "Open", "x"), "## Open\nx");
    }
}
