//! `update_title`, for the main agent: rename the terminal tab without approval.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{BoxFuture, Tool};
use crate::agent::AgentEvent;

pub const NAME: &str = "update_title";

pub struct UpdateTitle {
    /// A tool update wins over the initial naming call, even if that call finishes later.
    pub updated: Arc<Mutex<bool>>,
    pub tx: mpsc::UnboundedSender<AgentEvent>,
}

fn parse(args: &Value) -> Result<String, String> {
    let title = args["title"].as_str().ok_or("title must be a string")?;
    if title.chars().any(char::is_control) {
        return Err("title must be one line without control characters".to_string());
    }
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 60 {
        return Err("title must be 1 to 60 characters".to_string());
    }
    Ok(title.to_string())
}

impl Tool for UpdateTitle {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": NAME,
            "description": "Update the terminal title only when it no longer represents the current conversation. Use a few plain words describing the current topic, without a directory, app name or state suffix. Do not update it for every message or small step.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "The current topic in a few words, 1 to 60 characters." }
                },
                "required": ["title"],
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        parse(args)
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let title = match parse(args) {
                Ok(title) => title,
                Err(e) => return (e, false),
            };
            let mut updated = self.updated.lock().unwrap_or_else(|e| e.into_inner());
            *updated = true;
            let _ = self.tx.send(AgentEvent::Titled(title.clone()));
            (format!("Title updated: {title}"), true)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_title_update_emits_an_event_without_approval() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let updated = Arc::new(Mutex::new(false));
        let tool = UpdateTitle {
            updated: Arc::clone(&updated),
            tx,
        };
        assert!(!tool.needs_approval());
        let args = json!({"title": " fix terminal titles "});
        assert_eq!(tool.describe(&args).unwrap(), "fix terminal titles");
        let (out, ok) = tool.execute(&args).await;
        assert!(ok, "{out}");
        assert!(*updated.lock().unwrap());
        assert!(matches!(rx.try_recv(), Ok(AgentEvent::Titled(t)) if t == "fix terminal titles"));
    }

    #[tokio::test]
    async fn an_invalid_title_changes_nothing() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tool = UpdateTitle {
            updated: Arc::default(),
            tx,
        };
        for args in [
            json!({}),
            json!({"title": " "}),
            json!({"title": "a\nb"}),
            json!({"title": "\u{1b}]0;bad"}),
            json!({"title": "a".repeat(61)}),
        ] {
            assert!(tool.describe(&args).is_err());
            assert!(!tool.execute(&args).await.1);
        }
        assert!(!*tool.updated.lock().unwrap());
        assert!(rx.try_recv().is_err());
    }
}
