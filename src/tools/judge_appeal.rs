//! Agent context for bounded reconsideration of a judge denial.

use std::sync::Arc;

use serde_json::{Value, json};

use super::{BoxFuture, Tool};
use crate::judge::Judge;

pub const NAME: &str = "judge_appeal";

pub struct JudgeAppeal {
    pub judge: Arc<Judge>,
}

impl Tool for JudgeAppeal {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": NAME,
            "description": "After a judge denial, supply the activity id from the rejection and explain why the exact call is benign and task-related. Then retry the original tool with exactly the same arguments. The judge reassesses the concern and may approve a reasonable benign explanation. At most three reconsiderations per activity. This only queues context, not approval or execution. It cannot appeal hard policy blocks, grant user permission, override restrictions or allow secret disclosure.",
            "strict": true,
            "parameters": {
                "type": "object",
                "properties": {
                    "activity": {"type": "string"},
                    "justification": {"type": "string"}
                },
                "required": ["activity", "justification"],
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        let id = args["activity"]
            .as_str()
            .ok_or("activity must be a string")?;
        let justification = args["justification"]
            .as_str()
            .ok_or("justification must be a string")?;
        if justification.trim().is_empty() || justification.chars().count() > 2000 {
            return Err("justification must contain 1 to 2000 characters".into());
        }
        Ok(format!("justify denied activity {id}"))
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            match self.judge.appeal(
                args["activity"].as_str().unwrap_or_default(),
                args["justification"].as_str().unwrap_or_default(),
            ) {
                Ok(output) => (output, true),
                Err(error) => (error.to_string(), false),
            }
        })
    }
}
