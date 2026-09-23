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
/// How long the pipes are drained after the command itself has exited. A backgrounded
/// grandchild inherits them and holds them open, so waiting for the end of file waits
/// for that job instead of for the command.
const DRAIN: Duration = Duration::from_millis(100);
/// Bytes kept from each end of each stream. Two streams at twice this is what
/// `format_output` may pass on, which is [`super::MAX_OUTPUT`].
const KEEP: usize = super::MAX_OUTPUT / 4;

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
    after 120 seconds, so avoid anything interactive or long-running. A job sent to the \
    background must redirect its output to a file, since it inherits this command's own.",
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
    let (stdout, stderr, status) = match collect(&mut child, live).await {
        Err(e) => return format!("Could not start the command: {e}"),
        Ok(collected) => collected,
    };
    match status {
        Some(status) => format_output(&stdout, &stderr, status),
        // What it printed before the clock ran out is still what it was doing.
        None => format!(
            "Command timed out after {}s and was killed. Run something shorter, or send it to \
the background with its output redirected to a file and poll that.\n{}",
            TIMEOUT.as_secs(),
            truncate(&body(&stdout, &stderr))
        ),
    }
}

/// Output kept from one stream: the first [`KEEP`] bytes and the last [`KEEP`], with
/// what fell between them counted. Unbounded, a runaway command fills memory until the
/// process dies and nobody reads a word of it.
#[derive(Default)]
struct Kept {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    dropped: usize,
}

impl Kept {
    fn push(&mut self, bytes: &[u8]) {
        let room = KEEP.saturating_sub(self.head.len()).min(bytes.len());
        let (head, rest) = bytes.split_at(room);
        self.head.extend_from_slice(head);
        self.tail.extend(rest.iter().copied());
        if self.tail.len() > KEEP {
            let over = self.tail.len() - KEEP;
            self.tail.drain(..over);
            self.dropped += over;
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let mut out = self.head.clone();
        if self.dropped > 0 {
            let marker = format!("\n\n[... {} bytes trimmed ...]\n\n", self.dropped);
            out.extend_from_slice(marker.as_bytes());
        }
        out.extend(self.tail.iter().copied());
        out
    }
}

/// Read both pipes, passing output on at most every `TICK`, until the command exits or
/// the clock runs out, killing the process group in either of the latter cases. The
/// status is `None` when the command timed out. Reading is bounded twice over: the pipes
/// are only drained for `DRAIN` once the command itself is gone, since a backgrounded
/// grandchild inherits them and never closes them, and what is kept is capped.
async fn collect(
    child: &mut Child,
    live: Live<'_>,
) -> io::Result<(Kept, Kept, Option<ExitStatus>)> {
    let group = child.id();
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let (mut stdout, mut stderr) = (Kept::default(), Kept::default());
    let mut pending = Vec::new();
    let mut tick = tokio::time::interval(TICK);
    let (mut out_buf, mut err_buf) = ([0u8; 8192], [0u8; 8192]);
    let over = tokio::time::Instant::now() + TIMEOUT;
    let mut status = None;
    let mut drained_by = None;
    let mut timed_out = false;
    while out_pipe.is_some() || err_pipe.is_some() {
        let now = tokio::time::Instant::now();
        if now >= over {
            kill_group(group);
            timed_out = true;
            break;
        }
        if drained_by.is_some_and(|at| now >= at) {
            break;
        }
        tokio::select! {
            read = read_some(&mut out_pipe, &mut out_buf) => {
                let n = read?;
                stdout.push(&out_buf[..n]);
                pending.extend_from_slice(&out_buf[..n]);
            }
            read = read_some(&mut err_pipe, &mut err_buf) => {
                let n = read?;
                stderr.push(&err_buf[..n]);
                pending.extend_from_slice(&err_buf[..n]);
            }
            exit = child.wait(), if status.is_none() => {
                status = Some(exit?);
                drained_by = Some(tokio::time::Instant::now() + DRAIN);
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
    let status = match (timed_out, status) {
        (true, _) => None,
        (false, Some(status)) => Some(status),
        (false, None) => Some(child.wait().await?),
    };
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

/// What the command printed: stdout, then stderr.
fn body(stdout: &Kept, stderr: &Kept) -> String {
    let mut body = String::from_utf8_lossy(&stdout.bytes()).into_owned();
    let stderr = String::from_utf8_lossy(&stderr.bytes()).into_owned();
    if !stderr.is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&stderr);
    }
    if body.trim().is_empty() {
        body.push_str("(no output)");
    }
    body
}

/// The result the model sees: exit status, then stdout, then stderr.
fn format_output(stdout: &Kept, stderr: &Kept, status: ExitStatus) -> String {
    let code = status
        .code()
        .map_or_else(|| "killed by signal".to_string(), |c| c.to_string());
    format!("exit code: {code}\n{}", truncate(&body(stdout, stderr)))
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
        // The two pipes can be read in either order once both are ready.
        let joined = chunks.iter().map(|(_, c)| c.as_str()).collect::<String>();
        assert!(joined == "a\nb\ne" || joined == "a\neb\n", "{joined:?}");

        let plain = std::process::Command::new("bash")
            .arg("-lc")
            .arg(command)
            .output()
            .unwrap();
        let kept = |bytes: &[u8]| {
            let mut kept = Kept::default();
            kept.push(bytes);
            kept
        };
        assert_eq!(
            out,
            format_output(&kept(&plain.stdout), &kept(&plain.stderr), plain.status)
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

    /// A backgrounded job inherits both pipes, so waiting for the end of file waits for
    /// that job. The command's own exit is what ends the read.
    #[tokio::test]
    async fn a_backgrounded_job_does_not_hold_the_command_open() {
        let start = Instant::now();
        let out = run("printf 'quick\\n'; (sleep 3; printf 'late\\n') &", quiet()).await;
        assert!(start.elapsed() < Duration::from_secs(2), "{out}");
        assert!(out.starts_with("exit code: 0\nquick\n"), "{out}");
    }

    #[test]
    fn output_past_the_cap_keeps_both_ends_and_counts_the_middle() {
        let mut kept = Kept::default();
        // In chunks, as the pipe delivers it.
        for _ in 0..(KEEP * 3 / 100) {
            kept.push(&b"x".repeat(100));
        }
        kept.push(b"tail");
        let out = kept.bytes();
        assert!(out.len() < KEEP * 3, "{}", out.len());
        assert!(out.ends_with(b"tail"), "the last bytes are kept");
        assert!(kept.dropped > 0);
        assert!(String::from_utf8_lossy(&out).contains(&format!("{} bytes trimmed", kept.dropped)));
        // Under the cap nothing is touched.
        let mut small = Kept::default();
        small.push(b"a\nb\n");
        assert_eq!(small.bytes(), b"a\nb\n");
    }
}
