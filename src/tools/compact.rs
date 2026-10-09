//! Model-requested compaction, applied after every call in the step has its result.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::{BoxFuture, Tool};

pub const NAME: &str = "compact";
pub type Requested = Arc<Mutex<Option<Option<String>>>>;

pub struct Compact {
    pub requested: Requested,
}

fn parse(args: &Value) -> Result<Option<String>, String> {
    match args.get("prompt") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(prompt)) => {
            Ok(Some(prompt.trim().to_string()).filter(|p| !p.is_empty()))
        }
        _ => Err("prompt must be a string".to_string()),
    }
}

impl Tool for Compact {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": NAME,
            "description": "Compact the current conversation and continue working from its summary. An optional prompt steers what the summary preserves. Runs after all tool results in this step are recorded, before the next model call. Use status to inspect context usage first; avoid repeated compaction when context is small.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "What the summary should preserve."}
                },
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        Ok(parse(args)?.map_or_else(
            || "compact history".to_string(),
            |p| format!("compact history, keeping {p}"),
        ))
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let prompt = match parse(args) {
                Ok(prompt) => prompt,
                Err(e) => return (e, false),
            };
            *self.requested.lock().unwrap_or_else(|e| e.into_inner()) = Some(prompt);
            (
                "Compaction requested; it runs before the next model call, then work continues."
                    .to_string(),
                true,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn requests_preserve_optional_prompts_without_approval() {
        let tool = Compact {
            requested: Arc::default(),
        };
        assert!(!tool.needs_approval());
        assert!(!tool.parallel());
        for (args, expected) in [
            (json!({}), None),
            (
                json!({"prompt": " decisions "}),
                Some("decisions".to_string()),
            ),
        ] {
            assert!(tool.execute(&args).await.1);
            assert_eq!(tool.requested.lock().unwrap().take(), Some(expected));
        }
        assert!(!tool.execute(&json!({"prompt": 42})).await.1);
        assert!(tool.requested.lock().unwrap().is_none());
    }
}
