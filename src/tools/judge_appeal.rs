//! Agent context for bounded reconsideration of a judge denial.

use std::sync::Arc;

use serde_json::{Value, json};

use super::{BoxFuture, Tool};
use crate::judge::Judge;

pub const NAME: &str = "judge_appeal";

pub struct JudgeAppeal {
    pub judge: Arc<Judge>,
}

fn replacement(args: &Value) -> Result<Option<&str>, String> {
    match args.get("replacement_command") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(command))
            if !command.trim().is_empty() && command.chars().count() <= 2000 =>
        {
            Ok(Some(command))
        }
        Some(Value::String(_)) => {
            Err("replacement command must contain 1 to 2000 characters".into())
        }
        Some(_) => Err("replacement_command must be a string or null".into()),
    }
}

impl Tool for JudgeAppeal {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": NAME,
            "description": "After a judge denial, supply its activity id and justify why the call is benign and task-related. For bash, the coding model may propose an optional replacement_command; use null to retry the last denied call unchanged. Retry only the exact arguments returned by this tool. The judge sees the original denial and proposed replacement, then reassesses the current call. All revisions share three reconsiderations per activity. This only queues context, not approval or execution. Proposed calls still undergo fresh permission and secret checks. Justification cannot grant permission or override restrictions; hard-blocked calls have no appeal id of their own.",
            "strict": true,
            "parameters": {
                "type": "object",
                "properties": {
                    "activity": {"type": "string"},
                    "justification": {"type": "string"},
                    "replacement_command": {"type": ["string", "null"]}
                },
                "required": ["activity", "justification", "replacement_command"],
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
        replacement(args)?;
        Ok(format!("justify denied activity {id}"))
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let replacement = match replacement(args) {
                Ok(replacement) => replacement,
                Err(error) => return (error, false),
            };
            match self.judge.appeal_command(
                args["activity"].as_str().unwrap_or_default(),
                args["justification"].as_str().unwrap_or_default(),
                replacement,
            ) {
                Ok(output) => (output, true),
                Err(error) => (error.to_string(), false),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_is_nullable_bounded_and_backward_compatible() {
        assert_eq!(replacement(&json!({})).unwrap(), None);
        assert_eq!(
            replacement(&json!({"replacement_command": null})).unwrap(),
            None
        );
        assert_eq!(
            replacement(&json!({"replacement_command": "echo safe"})).unwrap(),
            Some("echo safe")
        );
        for value in [
            json!(""),
            json!(" "),
            json!("x".repeat(2001)),
            json!(42),
            json!(["echo safe"]),
        ] {
            assert!(replacement(&json!({"replacement_command": value})).is_err());
        }
    }
}
