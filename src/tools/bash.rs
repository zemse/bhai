//! Run a shell command, after the user approves it.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;

use super::{BoxFuture, Tool, string_arg, truncate};

pub const NAME: &str = "bash";

const TIMEOUT: Duration = Duration::from_secs(120);

pub struct Bash;

impl Tool for Bash {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        tool_schema()
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        parse_command(args)
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            match parse_command(args) {
                Ok(command) => (run(&command).await, true),
                Err(e) => (e, false),
            }
        })
    }
}

fn tool_schema() -> Value {
    json!({
        "type": "function",
        "name": NAME,
        "description": "Run a shell command with `bash -lc` in the current working directory and \
    return its combined stdout and stderr plus the exit code. The user approves every command \
    before it runs; a rejected command does not execute. Use absolute paths. Commands time out \
    after 120 seconds, so avoid anything interactive or long-running.",
        "strict": false,
        "parameters": {
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to run."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        }
    })
}

/// Pull the command out of a call's arguments.
fn parse_command(args: &Value) -> Result<String, String> {
    string_arg(args, "command")
        .filter(|c| !c.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| "missing required string field `command`.".to_string())
}

async fn run(command: &str) -> String {
    let child = Command::new("bash")
        .arg("-lc")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    let output = match tokio::time::timeout(TIMEOUT, child).await {
        Err(_) => {
            return format!(
                "Command timed out after {}s and was killed. Run something shorter, or send it \
to the background and poll for the result.",
                TIMEOUT.as_secs()
            );
        }
        Ok(Err(e)) => return format!("Could not start the command: {e}"),
        Ok(Ok(output)) => output,
    };

    let mut body = String::new();
    body.push_str(&String::from_utf8_lossy(&output.stdout));
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&stderr);
    }
    if body.trim().is_empty() {
        body.push_str("(no output)");
    }

    let code = output
        .status
        .code()
        .map_or_else(|| "killed by signal".to_string(), |c| c.to_string());
    format!("exit code: {code}\n{}", truncate(&body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_call() {
        let args = json!({"command": "ls -la"});
        assert_eq!(parse_command(&args).unwrap(), "ls -la");
    }

    #[test]
    fn rejects_a_missing_or_empty_command() {
        assert!(parse_command(&json!({})).is_err());
        assert!(parse_command(&json!({"command": "  "})).is_err());
        assert!(parse_command(&json!({"command": 1})).is_err());
    }

    #[tokio::test]
    async fn runs_a_command_and_reports_the_exit_code() {
        let out = run("echo hi; exit 3").await;
        assert!(out.starts_with("exit code: 3"), "{out}");
        assert!(out.contains("hi"), "{out}");
    }

    #[tokio::test]
    async fn merges_stderr_into_the_output() {
        let out = run("echo oops >&2").await;
        assert!(out.contains("oops"), "{out}");
    }
}
