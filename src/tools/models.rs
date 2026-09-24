//! List the models this session's backends will serve, so a child can be put on one the
//! caller has a reason to pick. The same list `/model` offers the user.

use std::sync::Arc;

use serde_json::{Value, json};

use super::{BoxFuture, Tool};
use crate::agent::Model;

pub const NAME: &str = "models";

pub struct Models {
    /// The model this session is on, which the list marks and always holds.
    pub current: Arc<dyn Model>,
}

impl Tool for Models {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": NAME,
            "description": "List the models available to this session, with the reasoning \
        efforts each takes and its context window. Use it before putting a child agent on a model \
        other than this one: an id that is not on the list is not a model either backend will serve.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn describe(&self, _args: &Value) -> Result<String, String> {
        Ok("models".to_string())
    }

    fn execute<'a>(&'a self, _args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let found = crate::models::load(self.current.ollama_url(), self.current.name()).await;
            (listing(&found, self.current.name()), true)
        })
    }
}

/// One line per model: the id to pass, then whatever the backend said about it.
pub fn listing(found: &crate::models::Catalogue, current: &str) -> String {
    let mut out = String::new();
    for model in &found.models {
        out.push_str(&format!("- {}", model.id));
        if model.id == current {
            out.push_str(" (this session's)");
        }
        if !model.efforts.is_empty() {
            let efforts: Vec<&str> = model.efforts.iter().map(|e| e.name.as_str()).collect();
            out.push_str(&format!("; efforts: {}", efforts.join(", ")));
        }
        if let Some(window) = model.window {
            out.push_str(&format!("; window: {window}"));
        }
        if !model.detail.is_empty() {
            out.push_str(&format!("; {}", model.detail));
        }
        out.push('\n');
    }
    for note in &found.notes {
        out.push_str(&format!("- {note}\n"));
    }
    match out.is_empty() {
        true => "No backend answered with a model list.".to_string(),
        false => out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Catalogue, Effort, Model as Listed};

    fn listed(id: &str, efforts: &[&str], window: Option<u64>, detail: &str) -> Listed {
        Listed {
            id: id.to_string(),
            label: id.to_string(),
            detail: detail.to_string(),
            efforts: efforts
                .iter()
                .map(|e| Effort {
                    name: e.to_string(),
                    detail: String::new(),
                })
                .collect(),
            default_effort: None,
            window,
        }
    }

    /// The id is what a call passes, so it leads every row; the rest is what the backend
    /// said, and a backend that could not be asked says so rather than going missing.
    #[test]
    fn the_listing_leads_with_the_id_a_call_would_pass() {
        let found = Catalogue {
            models: vec![
                listed(
                    "gpt-5.6-sol",
                    &["low", "high"],
                    Some(272_000),
                    "the fast one",
                ),
                listed("ollama:gemma4:e4b", &[], None, ""),
            ],
            notes: vec!["ollama: connection refused".to_string()],
        };
        assert_eq!(
            listing(&found, "gpt-5.6-sol"),
            "- gpt-5.6-sol (this session's); efforts: low, high; window: 272000; the fast one\n\
             - ollama:gemma4:e4b\n\
             - ollama: connection refused\n"
        );
        assert_eq!(
            listing(&Catalogue::default(), "x"),
            "No backend answered with a model list."
        );
    }
}
