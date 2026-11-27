//! Model-requested compaction, applied after every call in the step has its result.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::{BoxFuture, Tool, status};
use crate::compact::Limits;

pub const NAME: &str = "compact";
pub type Requested = Arc<Mutex<Option<Option<String>>>>;

pub struct Compact {
    pub requested: Requested,
    pub context: status::Context,
}

/// Why compacting now would not help, when context is below the size compaction aims
/// for: the summary call rereads everything and could not leave less than there is.
fn small(context: &Value) -> Option<String> {
    let input = context["input_tokens"].as_u64()?;
    let window = context["window_tokens"].as_u64()?;
    let auto = context["auto_compact_tokens"].as_u64()?;
    let limits = Limits {
        window: Some(window),
        compact_at: auto as f64 / window.max(1) as f64,
        reported: None,
    };
    let target = limits.target("");
    (input < target).then(|| {
        format!(
            "Not compacted: context is {input} of {window} tokens, below the {target} that \
compaction aims to leave. History compacts on its own once a call reads more than {auto}. \
Pass force to compact anyway."
        )
    })
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
            "description": "Compact the current conversation and continue working from its summary. An optional prompt steers what the summary preserves. Runs after all tool results in this step are recorded, before the next model call. While context is below the size compaction aims for, the call answers with the current usage and does nothing unless forced.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "What the summary should preserve."},
                    "force": {"type": "boolean", "description": "Compact even while context is small, as when the user asks for it."}
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
            let forced = args.get("force").and_then(Value::as_bool) == Some(true);
            let context = self
                .context
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if !forced && let Some(declined) = context.as_ref().and_then(small) {
                return (declined, true);
            }
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
            context: Arc::default(),
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

    #[tokio::test]
    async fn small_context_declines_unless_forced_and_large_context_compacts() {
        let tool = Compact {
            requested: Arc::default(),
            context: Arc::default(),
        };
        let limits = Limits {
            window: Some(272_000),
            ..Limits::default()
        };
        let reading = |input| Some(status::context(input, limits, "gpt-5.5", false));
        *tool.context.lock().unwrap() = reading(140_000);
        let (text, ok) = tool.execute(&json!({})).await;
        assert!(ok);
        assert!(tool.requested.lock().unwrap().is_none());
        for part in ["140000 of 272000", "163200", "more than 217600", "force"] {
            assert!(text.contains(part), "{part}: {text}");
        }
        assert!(tool.execute(&json!({"force": true})).await.1);
        assert_eq!(tool.requested.lock().unwrap().take(), Some(None));
        *tool.context.lock().unwrap() = reading(170_000);
        let (text, ok) = tool.execute(&json!({"prompt": "decisions"})).await;
        assert!(ok);
        assert!(text.starts_with("Compaction requested"), "{text}");
        assert_eq!(
            tool.requested.lock().unwrap().take(),
            Some(Some("decisions".to_string()))
        );
    }
}
