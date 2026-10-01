//! `remember`, for the main agent: append a note to `.bhai/MEMORY.md` for later sessions,
//! after the user approves it. A child is never given it.

use std::path::PathBuf;

use serde_json::{Value, json};

use super::{BoxFuture, Tool, string_arg};
use crate::memory;

pub const NAME: &str = "remember";

pub struct Remember {
    pub path: PathBuf,
}

impl Remember {
    fn line(&self, args: &Value) -> Result<String, String> {
        let note = string_arg(args, "note")
            .ok_or_else(|| "missing required string field `note`.".to_string())?;
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        memory::entry(note, &date)
    }
}

impl Tool for Remember {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": NAME,
            "description": "Save one short note to this project's memory file for later \
        sessions: a fact about the project or how the user wants work done that the next \
        session would otherwise have to rediscover, not a record of this task. The user \
        approves every note. Notes load when a session starts, so this one reaches the next \
        session, not this one.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "note": {
                        "type": "string",
                        "description": "The note, one line of at most 1024 bytes."
                    }
                },
                "required": ["note"],
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        let line = self.line(args)?;
        Ok(format!("remember {}", line.trim_start_matches("- ")))
    }

    fn preview(&self, args: &Value) -> Option<String> {
        let line = self.line(args).ok()?;
        let old = match super::preview_text(&self.path) {
            Ok(old) => old,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(_) => return None,
        };
        // As `append` writes it.
        let sep = if old.is_empty() || old.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        let new = format!("{old}{sep}{line}\n");
        crate::diff::unified(&old, &new)
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let line = match self.line(args) {
                Ok(line) => line,
                Err(e) => return (e, false),
            };
            match memory::append(&self.path, &line) {
                Ok(()) => (
                    format!(
                        "Saved to {}. It loads when the next session starts.",
                        self.path.display()
                    ),
                    true,
                ),
                Err(e) => (
                    format!("Could not write {}: {e}", self.path.display()),
                    false,
                ),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_note_is_appended_as_a_dated_line() {
        let dir = super::super::temp_dir();
        let tool = Remember {
            path: memory::path(&dir.join(".bhai")),
        };
        assert!(tool.needs_approval());
        let args = json!({"note": "tests run with\n `cargo nextest`"});
        let summary = tool.describe(&args).unwrap();
        assert!(
            summary.starts_with("remember 20")
                && summary.ends_with(": tests run with `cargo nextest`"),
            "{summary}"
        );
        let preview = tool.preview(&args).unwrap();
        assert!(preview.starts_with("@@ -0,0 +1 @@\n+- 20"), "{preview}");
        let (out, ok) = tool.execute(&args).await;
        assert!(ok, "{out}");
        assert!(out.contains("next session"), "{out}");
        let (_, ok) = tool.execute(&json!({"note": "second"})).await;
        assert!(ok);
        let text = std::fs::read_to_string(&tool.path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].ends_with(": tests run with `cargo nextest`"));
        assert!(lines[1].starts_with("- 20") && lines[1].ends_with(": second"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_bad_note_is_refused_before_it_is_asked_about() {
        let dir = super::super::temp_dir();
        let tool = Remember {
            path: memory::path(&dir),
        };
        assert!(tool.describe(&json!({})).is_err());
        assert!(tool.describe(&json!({"note": "  "})).is_err());
        let (out, ok) = tool.execute(&json!({"note": ""})).await;
        assert!(!ok && out.contains("empty"), "{out}");
        assert!(!tool.path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
