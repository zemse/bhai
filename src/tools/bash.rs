//! Run a shell command, after the user approves it.

use std::io;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

use super::{BoxFuture, Live, Tool, string_arg, truncate};
use crate::sandbox::Sandbox;

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
/// How each result starts, which is how [`outcome`] reads it back.
const EXIT: &str = "exit code: ";
const KILLED: &str = "killed by signal";
const TIMED_OUT: &str = "Command timed out";
const UNSTARTED: &str = "Could not start the command";
/// What the last line of a result starts with: the lines the command printed in all and
/// how long it ran, so a result cut in the middle still says how much it was.
const TRAILER: &str = "[output: ";

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    Failed(i32),
    Killed,
    TimedOut,
    Unstarted,
}

impl Outcome {
    pub fn ok(self) -> bool {
        self == Outcome::Succeeded
    }

    pub fn label(self) -> String {
        match self {
            Outcome::Succeeded => "succeeded".to_string(),
            Outcome::Failed(code) => format!("failed, exit {code}"),
            Outcome::Killed => "killed by a signal".to_string(),
            Outcome::TimedOut => format!("timed out after {}s", TIMEOUT.as_secs()),
            Outcome::Unstarted => "could not start".to_string(),
        }
    }
}

/// How the command behind a result the model was given ended, or `None` when the
/// result is not one this tool wrote.
pub fn outcome(output: &str) -> Option<Outcome> {
    let first = output.lines().next().unwrap_or_default();
    if let Some(code) = first.strip_prefix(EXIT) {
        return match code {
            "0" => Some(Outcome::Succeeded),
            KILLED => Some(Outcome::Killed),
            code => code.parse().ok().map(Outcome::Failed),
        };
    }
    if first.starts_with(TIMED_OUT) {
        return Some(Outcome::TimedOut);
    }
    first.starts_with(UNSTARTED).then_some(Outcome::Unstarted)
}

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
        let command = parse_command(args)?;
        Ok(match parse_workdir(args)? {
            Some(dir) => format!("{command}  (in {dir})"),
            None => command,
        })
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        static NEVER: AtomicBool = AtomicBool::new(false);
        let live = Live {
            progress: &|_| {},
            cancel: &NEVER,
            conversation: None,
        };
        self.execute_live(args, live)
    }

    fn execute_live<'a>(
        &'a self,
        args: &'a Value,
        live: Live<'a>,
    ) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            match parse_command(args).and_then(|c| Ok((c, parse_workdir(args)?))) {
                Ok((command, dir)) => {
                    let sandbox = crate::sandbox::active();
                    (run_in(&command, dir, sandbox, live).await, true)
                }
                Err(e) => (e, false),
            }
        })
    }
}

fn tool_schema() -> Value {
    let description = format!(
        "Run a shell command with `bash -lc` in the current working directory, or in \
    `workdir` when given, and return its combined stdout and stderr plus the exit code. The user approves every command \
    before it runs; a rejected command does not execute. Use absolute paths. Commands time out \
    after 120 seconds, so avoid anything interactive or long-running. A job sent to the \
    background must redirect its output to a file, since it inherits this command's own.{}",
        crate::sandbox::active().map_or("", Sandbox::describe)
    );
    json!({
        "type": "function",
        "name": NAME,
        "description": description,
        "strict": false,
        "parameters": {
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to run."
                },
                "workdir": {
                    "type": "string",
                    "description": "Directory to run the command in, instead of `cd dir && ...`. \
    A relative path is taken from the current working directory."
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

/// The directory the call asks to run in: `None` when it names none, an error when it
/// names something that is not a directory.
fn parse_workdir(args: &Value) -> Result<Option<&str>, String> {
    let dir = match args.get("workdir") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(dir)) => dir.as_str(),
        Some(_) => return Err("`workdir` must be a string.".to_string()),
    };
    if dir.is_empty() {
        return Ok(None);
    }
    match std::path::Path::new(dir).is_dir() {
        true => Ok(Some(dir)),
        false => Err(format!("`workdir` {dir} is not a directory.")),
    }
}

#[cfg(test)]
async fn run(command: &str, workdir: Option<&str>, live: Live<'_>) -> String {
    run_in(command, workdir, None, live).await
}

async fn run_in(
    command: &str,
    workdir: Option<&str>,
    sandbox: Option<&Sandbox>,
    live: Live<'_>,
) -> String {
    let started = Instant::now();
    // Kept to the end: on Linux it holds the Landlock ruleset the child applies.
    let mut shell = match sandbox.map(Sandbox::bash).transpose() {
        Err(e) => return format!("{UNSTARTED} in the sandbox: {e}"),
        Ok(shell) => shell,
    };
    let mut plain = Command::new("bash");
    let bash = match &mut shell {
        Some(shell) => &mut shell.command,
        None => &mut plain,
    };
    crate::childenv::scrub(bash);
    crate::childenv::non_interactive(bash);
    if let Some(dir) = workdir {
        bash.current_dir(dir);
    }
    let child = bash
        .arg("-lc")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Err(e) => return format!("{UNSTARTED}: {e}"),
        Ok(child) => child,
    };
    let (stdout, stderr, status) = match collect(&mut child, live).await {
        Err(e) => return format!("{UNSTARTED}: {e}"),
        Ok(collected) => collected,
    };
    match status {
        Some(status) => format_output(&stdout, &stderr, status, started.elapsed()),
        // What it printed before the clock ran out is still what it was doing.
        None => format!(
            "{TIMED_OUT} after {}s and was killed. Run something shorter, or send it to \
the background with its output redirected to a file and poll that.\n{}{}",
            TIMEOUT.as_secs(),
            truncate(&body(&stdout, &stderr)),
            trailer(&stdout, &stderr, started.elapsed())
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
    /// Newlines seen, trimmed ones included.
    newlines: usize,
    last: Option<u8>,
}

impl Kept {
    fn push(&mut self, bytes: &[u8]) {
        let Some(&last) = bytes.last() else {
            return;
        };
        self.newlines += bytes.iter().filter(|&&b| b == b'\n').count();
        self.last = Some(last);
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

    /// Lines printed, a last one without its newline counted.
    fn lines(&self) -> usize {
        self.newlines + usize::from(self.last.is_some_and(|b| b != b'\n'))
    }

    /// The text kept, known secrets blanked, a value the gap cut in two included.
    fn text(&self) -> String {
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        if self.dropped == 0 {
            let all = [self.head.as_slice(), &tail].concat();
            return crate::redact::apply(&String::from_utf8_lossy(&all)).into_owned();
        }
        let (head, tail) = crate::redact::apply_around_gap(
            &String::from_utf8_lossy(&self.head),
            &String::from_utf8_lossy(&tail),
        );
        format!(
            "{head}\n\n[... {} bytes trimmed ...]\n\n{tail}",
            self.dropped
        )
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

#[allow(unsafe_code)]
pub(crate) fn kill_group(group: Option<u32>) {
    if let Some(pid) = group.and_then(|pid| i32::try_from(pid).ok()) {
        // SAFETY: `killpg` only sends a signal; the group is the child's own.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
}

/// What the command printed: stdout, then stderr.
fn body(stdout: &Kept, stderr: &Kept) -> String {
    let mut body = stdout.text();
    let stderr = stderr.text();
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

/// The result the model sees: exit status, then stdout, then stderr, then the trailer.
fn format_output(stdout: &Kept, stderr: &Kept, status: ExitStatus, took: Duration) -> String {
    let code = status
        .code()
        .map_or_else(|| KILLED.to_string(), |c| c.to_string());
    format!(
        "{EXIT}{code}\n{}{}",
        truncate(&body(stdout, stderr)),
        trailer(stdout, stderr, took)
    )
}

/// Last, so [`outcome`], which reads the first line, is unaffected.
fn trailer(stdout: &Kept, stderr: &Kept, took: Duration) -> String {
    let lines = stdout.lines() + stderr.lines();
    let s = if lines == 1 { "" } else { "s" };
    format!("\n{TRAILER}{lines} line{s}, {:.1}s]", took.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;

    static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

    #[test]
    fn the_result_says_how_the_command_ended() {
        assert_eq!(outcome("exit code: 0\nok"), Some(Outcome::Succeeded));
        assert_eq!(outcome("exit code: 128\n"), Some(Outcome::Failed(128)));
        assert_eq!(
            outcome("exit code: killed by signal\n"),
            Some(Outcome::Killed)
        );
        assert_eq!(
            outcome("Command timed out after 120s and was killed."),
            Some(Outcome::TimedOut)
        );
        assert_eq!(
            outcome("Could not start the command: no such file"),
            Some(Outcome::Unstarted)
        );
        assert_eq!(
            outcome("The user rejected this call; it did not run."),
            None
        );
    }

    fn quiet() -> Live<'static> {
        Live {
            progress: &|_| {},
            cancel: &NOT_CANCELLED,
            conversation: None,
        }
    }

    #[test]
    fn parses_a_well_formed_call() {
        let args = json!({"command": "ls -la"});
        assert_eq!(parse_command(&args).unwrap(), "ls -la");
    }

    #[tokio::test]
    async fn a_workdir_is_where_the_command_runs_and_shows_in_the_summary() {
        let dir = std::env::temp_dir().join(format!("bhai-workdir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.canonicalize().unwrap();
        let args = json!({"command": "pwd", "workdir": path.to_str().unwrap()});
        assert_eq!(
            Bash.describe(&args).unwrap(),
            format!("pwd  (in {})", path.display())
        );
        let (out, ran) = Bash.execute(&args).await;
        assert!(ran && out.contains(path.to_str().unwrap()), "{out}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_workdir_must_be_a_directory() {
        assert_eq!(parse_workdir(&json!({"command": "ls"})), Ok(None));
        assert_eq!(parse_workdir(&json!({"workdir": ""})), Ok(None));
        assert!(parse_workdir(&json!({"workdir": 1})).is_err());
        let args = json!({"command": "ls", "workdir": "/no/such/dir/bhai"});
        assert!(
            Bash.describe(&args)
                .unwrap_err()
                .contains("not a directory")
        );
    }

    #[test]
    fn rejects_a_missing_or_empty_command() {
        assert!(parse_command(&json!({})).is_err());
        assert!(parse_command(&json!({"command": "  "})).is_err());
        assert!(parse_command(&json!({"command": 1})).is_err());
    }

    #[tokio::test]
    async fn runs_a_command_and_reports_the_exit_code() {
        let out = run("echo hi; exit 3", None, quiet()).await;
        assert!(out.starts_with("exit code: 3"), "{out}");
        assert!(out.contains("hi"), "{out}");
    }

    /// Runs itself as a child with credential-named variables set, since a test cannot
    /// set the environment of its own process.
    #[tokio::test]
    async fn credential_variables_do_not_reach_the_command() {
        const NAME: &str = "tools::bash::tests::credential_variables_do_not_reach_the_command";
        if std::env::var_os("BHAI_TEST_CHILD").is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture"])
                .env("BHAI_TEST_CHILD", "1")
                .env("BHAI_TEST_API_TOKEN", "withheld-value")
                .env("BHAI_TEST_PLAIN", "kept-value")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success() && stdout.contains("1 passed"),
                "{stdout}{}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        crate::childenv::set_pass(&[]);
        let out = run("env", None, quiet()).await;
        assert!(out.contains("BHAI_TEST_PLAIN=kept-value"), "{out}");
        assert!(!out.contains("BHAI_TEST_API_TOKEN"), "{out}");
        assert!(!out.contains("withheld-value"), "{out}");
    }

    #[tokio::test]
    async fn the_command_runs_with_a_fixed_non_interactive_environment() {
        let out = run(
            "echo \"$TERM $NO_COLOR $PAGER $GIT_PAGER $GIT_TERMINAL_PROMPT\"",
            None,
            quiet(),
        )
        .await;
        assert!(out.contains("dumb 1 cat cat 0"), "{out}");
        let out = run("echo \"$LANG $LC_ALL\"", None, quiet()).await;
        assert!(out.contains("UTF-8"), "{out}");
    }

    #[tokio::test]
    async fn registered_secrets_are_redacted_from_stdout_and_stderr() {
        crate::redact::register("bash-test-secret-value");
        let out = run(
            "echo out bash-test-secret-value; echo err bash-test-secret-value >&2",
            None,
            quiet(),
        )
        .await;
        assert!(out.contains("out [REDACTED]"), "{out}");
        assert!(out.contains("err [REDACTED]"), "{out}");
        assert!(!out.contains("bash-test-secret-value"), "{out}");
    }

    /// `None` when this kernel has no Landlock to test with.
    async fn sandboxed(command: &str, sandbox: &Sandbox) -> Option<String> {
        let out = run_in(command, None, Some(sandbox), quiet()).await;
        if cfg!(target_os = "linux") && out.starts_with(UNSTARTED) && out.contains("Landlock") {
            return None;
        }
        Some(out)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn a_sandboxed_command_writes_only_under_its_writable_dirs() {
        let (inside, outside) = (crate::tools::temp_dir(), crate::tools::temp_dir());
        let sandbox = Sandbox::with(vec![inside.clone(), "/dev".into()], true);
        let command = format!(
            "echo a > {0}/a; mkdir {0}/d; echo b > {1}/b; mkdir {1}/d; \
             echo c > /dev/null && echo done",
            inside.display(),
            outside.display(),
        );
        let Some(out) = sandboxed(&command, &sandbox).await else {
            return;
        };
        assert!(out.contains("done"), "{out}");
        assert!(out.contains("Operation not permitted"), "{out}");
        assert!(
            inside.join("a").exists() && inside.join("d").is_dir(),
            "{out}"
        );
        assert!(!outside.join("b").exists(), "{out}");
        assert!(!outside.join("d").exists(), "{out}");
        // Reads are not confined.
        std::fs::write(outside.join("r"), "readable").unwrap();
        let read = format!("cat {}/r", outside.display());
        let out = sandboxed(&read, &sandbox).await.unwrap();
        assert!(out.starts_with("exit code: 0\nreadable"), "{out}");
        let _ = std::fs::remove_dir_all(inside);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn a_sandbox_without_network_refuses_a_connection() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let command = format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo connected");
        let open = Sandbox::with(vec!["/dev".into()], true);
        let Some(out) = sandboxed(&command, &open).await else {
            return;
        };
        assert!(out.contains("connected"), "{out}");
        let Some(out) = sandboxed(&command, &Sandbox::with(vec!["/dev".into()], false)).await
        else {
            return;
        };
        assert!(!out.contains("connected"), "{out}");
        assert!(!out.starts_with("exit code: 0"), "{out}");
    }

    #[tokio::test]
    async fn merges_stderr_into_the_output() {
        let out = run("echo oops >&2", None, quiet()).await;
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
            conversation: None,
        };
        let out = run(command, None, live).await;
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
        let (body, trailer) = out.rsplit_once('\n').unwrap();
        assert!(trailer.starts_with("[output: 3 lines, "), "{out}");
        let plain = format_output(
            &kept(&plain.stdout),
            &kept(&plain.stderr),
            plain.status,
            Duration::ZERO,
        );
        assert_eq!(body, plain.rsplit_once('\n').unwrap().0);
        assert_eq!(body, "exit code: 0\na\nb\ne");
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
            conversation: None,
        };
        let start = Instant::now();
        let out = run(&command, None, live).await;
        assert!(start.elapsed() < Duration::from_secs(1), "{out}");
        assert!(
            out.starts_with("exit code: killed by signal\na\n\n[output: 1 line, "),
            "{out}"
        );
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
        let out = run(
            "printf 'quick\\n'; (sleep 3; printf 'late\\n') &",
            None,
            quiet(),
        )
        .await;
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
        let out = kept.text();
        assert!(out.len() < KEEP * 3, "{}", out.len());
        assert!(out.ends_with("tail"), "the last bytes are kept");
        assert!(kept.dropped > 0);
        assert!(out.contains(&format!("{} bytes trimmed", kept.dropped)));
        // Under the cap nothing is touched.
        let mut small = Kept::default();
        small.push(b"a\nb\n");
        assert_eq!(small.text(), "a\nb\n");
    }

    #[test]
    fn a_secret_cut_by_the_gap_leaves_no_piece_behind() {
        crate::redact::register("bash-test-gap-secret-e41c");
        let mut kept = Kept::default();
        kept.push(&b"x".repeat(KEEP - 9));
        kept.push(b"bash-test-gap-secret-e41c");
        kept.push(&b"y".repeat(KEEP - 11));
        assert!(kept.dropped > 0);
        let out = kept.text();
        assert!(
            out.contains("x[REDACTED]\n\n[..."),
            "{}",
            &out[KEEP - 20..KEEP + 40]
        );
        assert!(
            out.contains("...]\n\n[REDACTED]y"),
            "{}",
            &out[out.len() - KEEP - 40..]
        );
        assert!(!out.contains("bash-test") && !out.contains("e41c"));
    }

    #[tokio::test]
    async fn the_result_ends_with_the_line_count_and_wall_time() {
        let out = run("seq 1 5", None, quiet()).await;
        assert!(out.starts_with("exit code: 0\n1\n2\n3\n4\n5\n"), "{out}");
        let last = out.lines().last().unwrap();
        assert!(
            last.starts_with("[output: 5 lines, ") && last.ends_with("s]"),
            "{last}"
        );
        assert_eq!(outcome(&out), Some(Outcome::Succeeded));

        let out = run("sleep 0.3", None, quiet()).await;
        assert!(
            out.contains("(no output)\n[output: 0 lines, 0.3s]"),
            "{out}"
        );

        // A last line without its newline counts, and so does stderr.
        let out = run("printf 'a\\nb'; echo e >&2; exit 2", None, quiet()).await;
        assert!(out.contains("[output: 3 lines, "), "{out}");
        assert_eq!(outcome(&out), Some(Outcome::Failed(2)));
    }

    #[tokio::test]
    async fn the_line_count_includes_trimmed_output() {
        let out = run("yes | head -n 200000", None, quiet()).await;
        assert!(out.contains("bytes trimmed"), "{out}");
        assert!(out.contains("\n[output: 200000 lines, "), "{out}");
    }
}
