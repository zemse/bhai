//! Run a shell command, after the user approves it.

use std::io;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

use super::{BoxFuture, Live, Tool, string_arg, truncate};

pub const NAME: &str = "bash";

const TIMEOUT: Duration = Duration::from_secs(120);
/// How often live output reaches the UI, and how soon an interrupt is noticed.
const TICK: Duration = Duration::from_millis(50);

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
        static NEVER: AtomicBool = AtomicBool::new(false);
        let live = Live {
            progress: &|_| {},
            cancel: &NEVER,
        };
        self.execute_live(args, live)
    }

    fn execute_live<'a>(
        &'a self,
        args: &'a Value,
        live: Live<'a>,
    ) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            match parse_command(args) {
                Ok(command) => (run(&command, live).await, true),
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

async fn run(command: &str, live: Live<'_>) -> String {
    let child = Command::new("bash")
        .arg("-lc")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Err(e) => return format!("Could not start the command: {e}"),
        Ok(child) => child,
    };
    let group = child.id();

    let (stdout, stderr, status) =
        match tokio::time::timeout(TIMEOUT, collect(&mut child, live)).await {
            Err(_) => {
                kill_group(group);
                return format!(
                    "Command timed out after {}s and was killed. Run something shorter, or send it \
to the background and poll for the result.",
                    TIMEOUT.as_secs()
                );
            }
            Ok(Err(e)) => return format!("Could not start the command: {e}"),
            Ok(Ok(collected)) => collected,
        };
    format_output(&stdout, &stderr, status)
}

/// Read both pipes to the end, passing output on at most every `TICK`, and kill the
/// process group once the turn is interrupted.
async fn collect(child: &mut Child, live: Live<'_>) -> io::Result<(Vec<u8>, Vec<u8>, ExitStatus)> {
    let group = child.id();
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let (mut stdout, mut stderr, mut pending) = (Vec::new(), Vec::new(), Vec::new());
    let mut tick = tokio::time::interval(TICK);
    let (mut out_buf, mut err_buf) = ([0u8; 8192], [0u8; 8192]);
    while out_pipe.is_some() || err_pipe.is_some() {
        tokio::select! {
            read = read_some(&mut out_pipe, &mut out_buf) => {
                let n = read?;
                stdout.extend_from_slice(&out_buf[..n]);
                pending.extend_from_slice(&out_buf[..n]);
            }
            read = read_some(&mut err_pipe, &mut err_buf) => {
                let n = read?;
                stderr.extend_from_slice(&err_buf[..n]);
                pending.extend_from_slice(&err_buf[..n]);
            }
            _ = tick.tick() => {
                if live.cancel.load(Ordering::Relaxed) {
                    kill_group(group);
                    break;
                }
                flush(&mut pending, live.progress);
            }
        }
    }
    flush(&mut pending, live.progress);
    let status = child.wait().await?;
    Ok((stdout, stderr, status))
}

/// Read from a pipe, dropping it at end of file; a closed pipe never resolves.
async fn read_some(pipe: &mut Option<impl AsyncRead + Unpin>, buf: &mut [u8]) -> io::Result<usize> {
    let Some(reader) = pipe else {
        return std::future::pending().await;
    };
    let n = reader.read(buf).await?;
    if n == 0 {
        *pipe = None;
    }
    Ok(n)
}

/// Send the complete UTF-8 prefix of `pending`, keeping a split character for later.
fn flush(pending: &mut Vec<u8>, progress: &(dyn Fn(String) + Send + Sync)) {
    let end = match std::str::from_utf8(pending) {
        Ok(_) => pending.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => pending.len(),
    };
    if end == 0 {
        return;
    }
    let rest = pending.split_off(end);
    progress(String::from_utf8_lossy(pending).into_owned());
    *pending = rest;
}

fn kill_group(group: Option<u32>) {
    if let Some(pid) = group.and_then(|pid| i32::try_from(pid).ok()) {
        // SAFETY: `killpg` only sends a signal; the group is the child's own.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
}

/// The result the model sees: exit status, then stdout, then stderr.
fn format_output(stdout: &[u8], stderr: &[u8], status: ExitStatus) -> String {
    let mut body = String::new();
    body.push_str(&String::from_utf8_lossy(stdout));
    let stderr = String::from_utf8_lossy(stderr);
    if !stderr.is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&stderr);
    }
    if body.trim().is_empty() {
        body.push_str("(no output)");
    }

    let code = status
        .code()
        .map_or_else(|| "killed by signal".to_string(), |c| c.to_string());
    format!("exit code: {code}\n{}", truncate(&body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;

    static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

    fn quiet() -> Live<'static> {
        Live {
            progress: &|_| {},
            cancel: &NOT_CANCELLED,
        }
    }

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
        let out = run("echo hi; exit 3", quiet()).await;
        assert!(out.starts_with("exit code: 3"), "{out}");
        assert!(out.contains("hi"), "{out}");
    }

    #[tokio::test]
    async fn merges_stderr_into_the_output() {
        let out = run("echo oops >&2", quiet()).await;
        assert!(out.contains("oops"), "{out}");
    }

    #[tokio::test]
    async fn output_streams_before_exit_and_the_result_is_unchanged() {
        let command = "printf 'a\\n'; sleep 0.3; printf 'b\\n'; printf 'e' >&2";
        let start = Instant::now();
        let chunks = Mutex::new(Vec::new());
        let progress = |chunk: String| chunks.lock().unwrap().push((start.elapsed(), chunk));
        let cancel = AtomicBool::new(false);
        let live = Live {
            progress: &progress,
            cancel: &cancel,
        };
        let out = run(command, live).await;
        let total = start.elapsed();

        let chunks = chunks.into_inner().unwrap();
        let (first_at, first) = &chunks[0];
        assert_eq!(first, "a\n");
        assert!(
            total - *first_at >= Duration::from_millis(200),
            "{chunks:?}"
        );
        assert_eq!(
            chunks.iter().map(|(_, c)| c.as_str()).collect::<String>(),
            "a\nb\ne"
        );

        let plain = std::process::Command::new("bash")
            .arg("-lc")
            .arg(command)
            .output()
            .unwrap();
        assert_eq!(
            out,
            format_output(&plain.stdout, &plain.stderr, plain.status)
        );
        assert_eq!(out, "exit code: 0\na\nb\ne");
    }

    #[tokio::test]
    async fn an_interrupt_kills_the_whole_group() {
        let dir = crate::tools::temp_dir();
        let marker = dir.join("late");
        let command = format!(
            "printf 'a\\n'; (sleep 1; touch {}) & sleep 5",
            marker.display()
        );
        let cancel = AtomicBool::new(false);
        let progress = |_: String| cancel.store(true, Ordering::Relaxed);
        let live = Live {
            progress: &progress,
            cancel: &cancel,
        };
        let start = Instant::now();
        let out = run(&command, live).await;
        assert!(start.elapsed() < Duration::from_secs(1), "{out}");
        assert_eq!(out, "exit code: killed by signal\na\n");
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(
            !marker.exists(),
            "the background job outlived the interrupt"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
