//! `update_notes`, for the main agent: this session's notes, rewritten whole or a section
//! at a time within a byte budget. They live in the session and its file, so it needs no
//! approval.

use serde_json::{Value, json};

use super::{BoxFuture, Tool, string_arg};
use crate::notes::{self, BUDGET, Shared};

pub const NAME: &str = "update_notes";

pub struct UpdateNotes {
    pub notes: Shared,
}

/// What a call asks for.
enum Change<'a> {
    Read,
    Whole(&'a str),
    Section(&'a str, &'a str),
}

fn change(args: &Value) -> Result<Change<'_>, String> {
    match (
        string_arg(args, "notes"),
        string_arg(args, "section"),
        string_arg(args, "text"),
    ) {
        (None, None, None) => Ok(Change::Read),
        (Some(notes), None, None) => Ok(Change::Whole(notes)),
        (None, Some(section), Some(text)) if !section.trim().is_empty() => {
            Ok(Change::Section(section, text))
        }
        (None, Some(_), Some(_)) => Err("`section` is empty.".to_string()),
        (None, _, _) => Err("`section` and `text` go together.".to_string()),
        (Some(_), _, _) => Err("give `notes`, or `section` and `text`, not both.".to_string()),
    }
}

impl Tool for UpdateNotes {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": NAME,
            "description": format!("Keep this session's notes: one short markdown document \
        of what the rest of the session needs to hold on to, such as decisions and why, \
        findings with `file:line`, approaches tried and rejected and why, and open questions. \
        They are restated after every compaction and given to each child agent started after \
        they change. They end with the session; `remember` is for what later sessions need. \
        `notes` replaces the whole document; `section` with `text` replaces one `## ` section, \
        adding it when missing and removing it when `text` is empty. At most {BUDGET} bytes: \
        when they are full, rewrite them rather than add. With no arguments, returns them."),
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "notes": {
                        "type": "string",
                        "description": "The whole document; empty clears it."
                    },
                    "section": {
                        "type": "string",
                        "description": "The heading of the section to replace, without `## `."
                    },
                    "text": {
                        "type": "string",
                        "description": "The section's new body."
                    }
                },
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        Ok(match change(args)? {
            Change::Read => "read notes".to_string(),
            Change::Whole(notes) if notes.trim().is_empty() => "notes cleared".to_string(),
            Change::Whole(notes) => format!("notes rewritten ({} bytes)", notes.trim().len()),
            Change::Section(section, _) => format!("notes: {}", section.trim()),
        })
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let change = match change(args) {
                Ok(change) => change,
                Err(e) => return (e, false),
            };
            let mut state = notes::lock(&self.notes);
            let next = match change {
                Change::Read => {
                    return match &state.text {
                        Some(text) => (text.clone(), true),
                        None => ("There are no session notes yet.".to_string(), true),
                    };
                }
                Change::Whole(notes) => notes.trim().to_string(),
                Change::Section(section, text) => {
                    notes::edit_section(state.text.as_deref().unwrap_or_default(), section, text)
                }
            };
            if next.len() > BUDGET {
                return (
                    format!(
                        "Not saved: the notes would be {} bytes, over the {BUDGET} they may be. \
Rewrite them shorter as a whole with `notes`.",
                        next.len()
                    ),
                    false,
                );
            }
            if next.is_empty() {
                state.text = None;
                return ("Session notes cleared.".to_string(), true);
            }
            let len = next.len();
            state.text = Some(next);
            (
                format!("Session notes saved: {len} of {BUDGET} bytes."),
                true,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn notes_are_rewritten_edited_by_section_and_kept_in_budget() {
        let tool = UpdateNotes {
            notes: Shared::default(),
        };
        assert!(!tool.needs_approval());
        let (out, ok) = tool.execute(&json!({})).await;
        assert!(ok && out.contains("no session notes"), "{out}");

        let args = json!({"notes": "## Decisions\nsqlite, for one file\n"});
        assert_eq!(tool.describe(&args).unwrap(), "notes rewritten (33 bytes)");
        let (out, ok) = tool.execute(&args).await;
        assert!(ok, "{out}");
        assert_eq!(out, format!("Session notes saved: 33 of {BUDGET} bytes."));

        let args = json!({"section": "Rejected", "text": "redis: no persistence"});
        assert_eq!(tool.describe(&args).unwrap(), "notes: Rejected");
        assert!(tool.execute(&args).await.1);
        assert_eq!(
            notes::lock(&tool.notes).text.as_deref(),
            Some("## Decisions\nsqlite, for one file\n\n## Rejected\nredis: no persistence")
        );
        let (out, _) = tool.execute(&json!({})).await;
        assert!(out.ends_with("redis: no persistence"), "{out}");

        // Over the budget nothing changes, and the model is told to rewrite.
        let before = notes::lock(&tool.notes).clone();
        let big = json!({"section": "Findings", "text": "x".repeat(BUDGET)});
        let (out, ok) = tool.execute(&big).await;
        assert!(!ok && out.contains("Rewrite them shorter"), "{out}");
        assert_eq!(*notes::lock(&tool.notes), before);
        assert!(tool.execute(&json!({"notes": "y".repeat(BUDGET)})).await.1);

        let (out, ok) = tool.execute(&json!({"notes": " "})).await;
        assert!(ok && out == "Session notes cleared.", "{out}");
        assert_eq!(notes::lock(&tool.notes).text, None);
    }

    #[test]
    fn mixed_or_partial_arguments_are_refused() {
        let tool = UpdateNotes {
            notes: Shared::default(),
        };
        assert!(
            tool.describe(&json!({"notes": "a", "section": "b"}))
                .is_err()
        );
        assert!(tool.describe(&json!({"section": "b"})).is_err());
        assert!(tool.describe(&json!({"text": "b"})).is_err());
        assert!(
            tool.describe(&json!({"section": " ", "text": "b"}))
                .is_err()
        );
    }
}
