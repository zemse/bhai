//! The only tool: run a shell command, after the user approves it.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;

pub const NAME: &str = "bash";

const TIMEOUT: Duration = Duration::from_secs(120);
/// Tool output past this is trimmed in the middle; the tail usually carries the error.
const MAX_OUTPUT: usize = 20_000;

pub fn tool_schema() -> Value {
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

/// Pull the command out of a `function_call`'s JSON-string arguments.
pub fn parse_command(arguments: &str) -> Result<String, String> {
    let parsed: Value = serde_json::from_str(arguments)
        .map_err(|e| format!("arguments were not valid JSON: {e}. Send a JSON object."))?;
    parsed
        .get("command")
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| "missing required string field `command`.".to_string())
}

pub async fn run(command: &str) -> String {
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

fn truncate(s: &str) -> String {
    if s.len() <= MAX_OUTPUT {
        return s.to_string();
    }
    let head = floor_boundary(s, MAX_OUTPUT / 2);
    let tail = ceil_boundary(s, s.len() - MAX_OUTPUT / 2);
    format!(
        "{}\n\n[... {} bytes trimmed ...]\n\n{}",
        &s[..head],
        tail - head,
        &s[tail..]
    )
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_call() {
        assert_eq!(parse_command(r#"{"command":"ls -la"}"#).unwrap(), "ls -la");
    }

    #[test]
    fn rejects_a_missing_or_empty_command() {
        assert!(parse_command("{}").is_err());
        assert!(parse_command(r#"{"command":"  "}"#).is_err());
        assert!(parse_command("not json").is_err());
    }

    #[test]
    fn truncate_keeps_both_ends_and_stays_valid_utf8() {
        let long = "é".repeat(MAX_OUTPUT);
        let out = truncate(&long);
        assert!(out.len() < long.len());
        assert!(out.contains("bytes trimmed"));
        assert!(out.starts_with('é') && out.ends_with('é'));
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
