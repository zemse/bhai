//! Run a shell command, after the user approves it. One still running when the call
//! yields stays alive as a session, which `write_stdin` polls or types into.

use std::collections::BTreeMap;
use std::io;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::Notify;

use super::{BoxFuture, Live, Tool, string_arg, truncate};
use crate::sandbox::Sandbox;

pub const NAME: &str = "bash";

/// How long a command typed after `!` may run, since nobody can poll it.
const TIMEOUT: Duration = Duration::from_secs(120);
/// How long a call waits before handing back a session, unless it says otherwise.
const YIELD: Duration = Duration::from_secs(10);
/// The bounds on the `yield_time_ms` a call may ask for.
const YIELD_MIN: Duration = Duration::from_millis(250);
const YIELD_MAX: Duration = Duration::from_secs(30);
/// Sessions kept running at once; past this the one used least recently is killed.
const MAX_SESSIONS: usize = 16;
/// The terminal a `tty` session gets.
const TTY_ROWS: u16 = 24;
const TTY_COLS: u16 = 80;
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
const RUNNING: &str = "Process running with session ID ";
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
    /// Still running, as this session.
    Running(u32),
}

impl Outcome {
    pub fn ok(self) -> bool {
        matches!(self, Outcome::Succeeded | Outcome::Running(_))
    }

    pub fn label(self) -> String {
        match self {
            Outcome::Succeeded => "succeeded".to_string(),
            Outcome::Failed(code) => format!("failed, exit {code}"),
            Outcome::Killed => "killed by a signal".to_string(),
            Outcome::TimedOut => format!("timed out after {}s", TIMEOUT.as_secs()),
            Outcome::Unstarted => "could not start".to_string(),
            Outcome::Running(id) => format!("still running as session {id}"),
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
    if let Some(id) = first.strip_prefix(RUNNING) {
        return id.parse().ok().map(Outcome::Running);
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
        parse_yield(args)?;
        let mut summary = match parse_workdir(args)? {
            Some(dir) => format!("{command}  (in {dir})"),
            None => command,
        };
        if parse_tty(args)? {
            summary.push_str("  (tty)");
        }
        Ok(summary)
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
            let parsed = parse_command(args).and_then(|c| {
                Ok((
                    c,
                    parse_workdir(args)?,
                    parse_yield(args)?,
                    parse_tty(args)?,
                ))
            });
            match parsed {
                Ok((command, dir, wait, tty)) => {
                    (start(&command, dir, wait, tty, live).await, true)
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
    before it runs; a rejected command does not execute. Use absolute paths. A command still \
    running after `yield_time_ms` keeps running as a session: the result starts `Process running \
    with session ID N` and holds the output so far, and `write_stdin` polls it for more or, \
    for a `tty` session, types into it. Use that for dev servers, watchers, long builds and \
    REPLs. A job sent to the background with `&` must redirect its output to a file, since \
    it inherits this command's own.{}{}",
        crate::sandbox::active().map_or("", Sandbox::describe),
        match crate::askpass::active() {
            true =>
                " For root, use `sudo -A`: it asks the user for their password, which you never see.",
            false => "",
        }
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
                },
                "yield_time_ms": {
                    "type": "integer",
                    "description": "How long to wait for the command to finish before \
    returning a session ID, 250 to 30000. Defaults to 10000."
                },
                "tty": {
                    "type": "boolean",
                    "description": "Run in a terminal, so `write_stdin` can type into it: for \
    REPLs, prompts and debuggers. Without it stdin is empty."
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

/// How long the call waits before yielding, clamped to the bounds.
fn parse_yield(args: &Value) -> Result<Duration, String> {
    parse_millis(args, YIELD, YIELD_MIN, YIELD_MAX)
}

/// `yield_time_ms` as a duration in `min..=max`, or `default` when absent.
pub(crate) fn parse_millis(
    args: &Value,
    default: Duration,
    min: Duration,
    max: Duration,
) -> Result<Duration, String> {
    match args.get("yield_time_ms") {
        None | Some(Value::Null) => Ok(default),
        Some(ms) => ms
            .as_u64()
            .map(|ms| Duration::from_millis(ms).clamp(min, max))
            .ok_or_else(|| "`yield_time_ms` must be a non-negative integer.".to_string()),
    }
}

fn parse_tty(args: &Value) -> Result<bool, String> {
    match args.get("tty") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(tty)) => Ok(*tty),
        Some(_) => Err("`tty` must be a boolean.".to_string()),
    }
}

/// Run a command the user typed after `!`: to the end, or until [`TIMEOUT`] kills it,
/// since there is nobody to poll a session.
pub async fn typed(command: &str) -> String {
    static NEVER: AtomicBool = AtomicBool::new(false);
    let live = Live {
        progress: &|_| {},
        cancel: &NEVER,
        conversation: None,
    };
    run_in(command, None, crate::sandbox::active(), live).await
}

#[cfg(test)]
async fn run(command: &str, workdir: Option<&str>, live: Live<'_>) -> String {
    run_in(command, workdir, None, live).await
}

/// Run `command` until it exits or the clock runs out, and kill it in the latter case.
async fn run_in(
    command: &str,
    workdir: Option<&str>,
    sandbox: Option<&Sandbox>,
    live: Live<'_>,
) -> String {
    let started = Instant::now();
    let mut proc = match Proc::spawn(command, workdir, sandbox, false) {
        Err(e) => return e,
        Ok(proc) => proc,
    };
    let until = tokio::time::Instant::now() + TIMEOUT;
    match proc.wait(until, live).await {
        Err(e) => format!("{UNSTARTED}: {e}"),
        Ok(Some(status)) => {
            let (stdout, stderr) = proc.take();
            format_output(&stdout, &stderr, status, started.elapsed())
        }
        Ok(None) => {
            kill_group(proc.group);
            proc.status = proc.child.wait().await.ok();
            let (stdout, stderr) = proc.take();
            // What it printed before the clock ran out is still what it was doing.
            format!(
                "{TIMED_OUT} after {}s and was killed. Run something shorter, or send it to \
the background with its output redirected to a file and poll that.\n{}{}",
                TIMEOUT.as_secs(),
                truncate(&body(&stdout, &stderr)),
                trailer(&stdout, &stderr, started.elapsed())
            )
        }
    }
}

/// Run `command` for up to `wait`, and keep it as a session if it is still running then.
async fn start(
    command: &str,
    workdir: Option<&str>,
    wait: Duration,
    tty: bool,
    live: Live<'_>,
) -> String {
    let started = Instant::now();
    let mut proc = match Proc::spawn(command, workdir, crate::sandbox::active(), tty) {
        Err(e) => return e,
        Ok(proc) => proc,
    };
    match proc.wait(tokio::time::Instant::now() + wait, live).await {
        Err(e) => format!("{UNSTARTED}: {e}"),
        Ok(Some(status)) => {
            let (stdout, stderr) = proc.take();
            format_output(&stdout, &stderr, status, started.elapsed())
        }
        Ok(None) => {
            let (stdout, stderr) = proc.take();
            let id = keep(command, proc);
            running(id, &stdout, &stderr, started.elapsed())
        }
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
    /// Read from a terminal, so escape sequences and carriage returns come out.
    tty: bool,
}

impl Kept {
    fn new(tty: bool) -> Self {
        Self {
            tty,
            ..Self::default()
        }
    }

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
        let decode = |bytes: &[u8]| {
            let text = String::from_utf8_lossy(bytes);
            match self.tty {
                true => plain(&text),
                false => text.into_owned(),
            }
        };
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        if self.dropped == 0 {
            let all = [self.head.as_slice(), &tail].concat();
            return crate::redact::apply(&decode(&all)).into_owned();
        }
        let (head, tail) = crate::redact::apply_around_gap(&decode(&self.head), &decode(&tail));
        format!(
            "{head}\n\n[... {} bytes trimmed ...]\n\n{tail}",
            self.dropped
        )
    }
}

/// Terminal output as the text it shows: escape sequences dropped, and a line a carriage
/// return went back over kept as it was last drawn.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: parameters and intermediates up to a final byte.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC and the other strings run to BEL or ST.
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                // A character set designation takes one more character.
                Some('(' | ')' | '*' | '+') => {
                    chars.next();
                }
                _ => {}
            },
            '\u{7}' => {}
            _ => out.push(c),
        }
    }
    let lines: Vec<&str> = out
        .split('\n')
        .map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            line.rsplit('\r')
                .find(|s| !s.is_empty())
                .unwrap_or_default()
        })
        .collect();
    lines.join("\n")
}

/// What the readers have taken from the command's output so far.
struct Shared {
    stdout: Kept,
    stderr: Kept,
    /// Read but not yet shown live, its last [`KEEP`] bytes at most: a session nobody
    /// polls is not flushed, so uncapped it grows for as long as the command prints.
    pending: Vec<u8>,
    /// Streams not yet at end of file.
    open: usize,
}

/// A running command and the tasks reading its output. Reading goes on between calls,
/// so a command that prints while nobody polls it never blocks on a full pipe.
struct Proc {
    child: Child,
    group: Option<u32>,
    tty: bool,
    /// The terminal's master side, which typing goes to; `None` without a terminal.
    input: Option<tokio::fs::File>,
    shared: Arc<Mutex<Shared>>,
    /// Woken when a stream reaches its end.
    closed: Arc<Notify>,
    readers: Vec<tokio::task::JoinHandle<()>>,
    status: Option<ExitStatus>,
    drained_by: Option<tokio::time::Instant>,
    /// Held while the command lives: its sudo can ask only while it is registered.
    _asking: Option<crate::askpass::Call>,
}

impl Drop for Proc {
    fn drop(&mut self) {
        if self.status.is_none() {
            kill_group(self.group);
        }
        for reader in &self.readers {
            reader.abort();
        }
    }
}

type Pipe = Box<dyn AsyncRead + Unpin + Send>;

impl Proc {
    /// Start `bash -lc command`, in its own process group, with stdin empty or, for a
    /// `tty`, on a terminal of its own.
    /// The error is the whole result the call returns.
    fn spawn(
        command: &str,
        workdir: Option<&str>,
        sandbox: Option<&Sandbox>,
        tty: bool,
    ) -> Result<Self, String> {
        // On Linux it holds the Landlock ruleset the child applies, until the spawn.
        let shell = sandbox
            .map(Sandbox::bash)
            .transpose()
            .map_err(|e| format!("{UNSTARTED} in the sandbox: {e}"))?;
        let asking = crate::askpass::register(command);
        Self::spawn_in(command, workdir, shell, asking, tty)
            .map_err(|e| format!("{UNSTARTED}: {e}"))
    }

    fn spawn_in(
        command: &str,
        workdir: Option<&str>,
        mut shell: Option<crate::sandbox::Shell>,
        asking: Option<crate::askpass::Call>,
        tty: bool,
    ) -> io::Result<Self> {
        let mut plain = Command::new("bash");
        let bash = match &mut shell {
            Some(shell) => &mut shell.command,
            None => &mut plain,
        };
        crate::childenv::scrub(bash);
        crate::childenv::non_interactive(bash);
        if let Some(call) = &asking {
            call.env(bash);
        }
        if let Some(dir) = workdir {
            bash.current_dir(dir);
        }
        bash.arg("-lc").arg(command).kill_on_drop(true);
        let (child, pipes, input): (Child, Vec<(Pipe, bool)>, _) = if tty {
            let (master, slave) = open_pty()?;
            bash.stdin(Stdio::from(slave.try_clone()?))
                .stdout(Stdio::from(slave.try_clone()?))
                .stderr(Stdio::from(slave));
            controlling_terminal(bash);
            let child = bash.spawn()?;
            // The command holds copies of the terminal's slave side; while they are open,
            // the master never reads the end of file.
            drop((shell, plain));
            let master = std::fs::File::from(master);
            let reader = tokio::fs::File::from_std(master.try_clone()?);
            let pipes = vec![(Box::new(reader) as Pipe, false)];
            (child, pipes, Some(tokio::fs::File::from_std(master)))
        } else {
            let mut child = bash
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0)
                .spawn()?;
            let mut pipes: Vec<(Pipe, bool)> = Vec::new();
            if let Some(out) = child.stdout.take() {
                pipes.push((Box::new(out), false));
            }
            if let Some(err) = child.stderr.take() {
                pipes.push((Box::new(err), true));
            }
            (child, pipes, None)
        };
        let shared = Arc::new(Mutex::new(Shared {
            stdout: Kept::new(tty),
            stderr: Kept::new(tty),
            pending: Vec::new(),
            open: pipes.len(),
        }));
        let closed = Arc::new(Notify::new());
        let readers = pipes
            .into_iter()
            .map(|(pipe, err)| read_into(pipe, err, Arc::clone(&shared), Arc::clone(&closed)))
            .collect();
        Ok(Self {
            group: child.id(),
            child,
            tty,
            input,
            shared,
            closed,
            readers,
            status: None,
            drained_by: None,
            _asking: asking,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wait for the command to exit, passing output on at most every `TICK`, until
    /// `until`, when `None` says it is still running. An interrupt kills the process
    /// group. Once the command itself has exited its output is only drained for `DRAIN`,
    /// since a backgrounded grandchild inherits the pipes and never closes them.
    async fn wait(
        &mut self,
        until: tokio::time::Instant,
        live: Live<'_>,
    ) -> io::Result<Option<ExitStatus>> {
        let mut tick = tokio::time::interval(TICK);
        loop {
            let now = tokio::time::Instant::now();
            if let Some(status) = self.status
                && (self.lock().open == 0 || self.drained_by.is_some_and(|at| now >= at))
            {
                self.flush(live.progress);
                return Ok(Some(status));
            }
            if self.status.is_none() && now >= until {
                self.flush(live.progress);
                return Ok(None);
            }
            tokio::select! {
                exit = self.child.wait(), if self.status.is_none() => {
                    self.status = Some(exit?);
                    self.drained_by = Some(tokio::time::Instant::now() + DRAIN);
                }
                _ = self.closed.notified() => {}
                _ = tokio::time::sleep_until(until), if self.status.is_none() => {}
                _ = tick.tick() => {
                    if live.cancel.load(Ordering::Relaxed) && self.status.is_none() {
                        kill_group(self.group);
                        self.status = Some(self.child.wait().await?);
                        self.flush(live.progress);
                        return Ok(self.status);
                    }
                    self.flush(live.progress);
                }
            }
        }
    }

    fn flush(&self, progress: &(dyn Fn(String) + Send + Sync)) {
        let tty = self.tty;
        flush(&mut self.lock().pending, progress, tty);
    }

    /// The output read since the last take.
    fn take(&mut self) -> (Kept, Kept) {
        let tty = self.tty;
        let mut shared = self.lock();
        (
            std::mem::replace(&mut shared.stdout, Kept::new(tty)),
            std::mem::replace(&mut shared.stderr, Kept::new(tty)),
        )
    }
}

/// Read `pipe` into `shared` until its end, which a read error also counts as: a
/// terminal's master side fails with `EIO` once the command is gone.
fn read_into(
    mut pipe: Pipe,
    stderr: bool,
    shared: Arc<Mutex<Shared>>,
    closed: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            let n = pipe.read(&mut buf).await.unwrap_or(0);
            let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
            if n == 0 {
                shared.open -= 1;
                drop(shared);
                closed.notify_one();
                return;
            }
            match stderr {
                true => shared.stderr.push(&buf[..n]),
                false => shared.stdout.push(&buf[..n]),
            }
            queue(&mut shared.pending, &buf[..n]);
        }
    })
}

/// Add `bytes` to `pending`, dropping what comes before its last [`KEEP`] bytes and any
/// piece of a character that the cut left at the front.
fn queue(pending: &mut Vec<u8>, bytes: &[u8]) {
    pending.extend_from_slice(bytes);
    if pending.len() > KEEP {
        let mut cut = pending.len() - KEEP;
        while pending.get(cut).is_some_and(|&b| b & 0xc0 == 0x80) {
            cut += 1;
        }
        pending.drain(..cut);
    }
}

/// Send the complete UTF-8 prefix of `pending`, keeping a split character for later.
fn flush(pending: &mut Vec<u8>, progress: &(dyn Fn(String) + Send + Sync), tty: bool) {
    let end = match std::str::from_utf8(pending) {
        Ok(_) => pending.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => pending.len(),
    };
    if end == 0 {
        return;
    }
    let rest = pending.split_off(end);
    let text = String::from_utf8_lossy(pending);
    progress(match tty {
        true => plain(&text),
        false => text.into_owned(),
    });
    *pending = rest;
}

/// A new terminal: its master side, then its slave side.
#[allow(unsafe_code)]
fn open_pty() -> io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::{FromRawFd, OwnedFd};
    let (mut master, mut slave) = (-1, -1);
    let mut size = libc::winsize {
        ws_row: TTY_ROWS,
        ws_col: TTY_COLS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `openpty` only writes the two descriptors it opens, which are owned here
    // from then on, and reads the size it is given.
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if opened != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both were just opened by `openpty` and nothing else owns them.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [&master, &slave] {
        use std::os::fd::AsRawFd;
        // SAFETY: setting close-on-exec on a descriptor owned here; the child gets the
        // slave side as its stdio, which is duplicated without the flag.
        unsafe {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    Ok((master, slave))
}

/// Make the child a session leader with its stdin as the controlling terminal, so a
/// ctrl-c typed into it reaches its foreground job. A session leader leads its own
/// process group too, so `kill_group` reaches it as it does a piped command.
#[allow(unsafe_code)]
fn controlling_terminal(command: &mut Command) {
    // SAFETY: `setsid` and `ioctl` are async-signal-safe, which is all a `pre_exec`
    // closure may call.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

pub(crate) fn kill_group(group: Option<u32>) {
    signal_group(group, libc::SIGKILL);
}

#[allow(unsafe_code)]
fn signal_group(group: Option<u32>, signal: libc::c_int) {
    if let Some(pid) = group.and_then(|pid| i32::try_from(pid).ok()) {
        // SAFETY: `killpg` only sends a signal; the group is the child's own.
        unsafe {
            libc::killpg(pid, signal);
        }
    }
}

/// A command kept running past its call.
struct Session {
    command: String,
    tty: bool,
    /// What has been typed since the shell last finished a command or took a ctrl-c: an
    /// unfinished line, or lines left inside a quote or heredoc, that the next write
    /// completes and the permission check reads along with it.
    line: String,
    group: Option<u32>,
    used: Instant,
    proc: Arc<tokio::sync::Mutex<Proc>>,
}

#[derive(Default)]
struct Sessions {
    next: u32,
    live: BTreeMap<u32, Session>,
}

/// Every session this process has running, whichever agent started it. They outlive the
/// turn, so a dev server keeps serving while the user types; [`kill_all`] ends them.
static SESSIONS: LazyLock<Mutex<Sessions>> = LazyLock::new(Mutex::default);

fn sessions() -> std::sync::MutexGuard<'static, Sessions> {
    SESSIONS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Keep `proc` as a session, making room by killing the one used least recently.
fn keep(command: &str, proc: Proc) -> u32 {
    let mut sessions = sessions();
    while sessions.live.len() >= MAX_SESSIONS {
        let Some(oldest) = sessions
            .live
            .iter()
            .min_by_key(|(_, s)| s.used)
            .map(|(id, _)| *id)
        else {
            break;
        };
        if let Some(session) = sessions.live.remove(&oldest) {
            kill_group(session.group);
        }
    }
    sessions.next += 1;
    let id = sessions.next;
    sessions.live.insert(
        id,
        Session {
            command: command.to_string(),
            tty: proc.tty,
            line: String::new(),
            group: proc.group,
            used: Instant::now(),
            proc: Arc::new(tokio::sync::Mutex::new(proc)),
        },
    );
    id
}

/// The command a session runs and whether it has a terminal, or `None` when there is no
/// such session.
pub(crate) fn session(id: u32) -> Option<(String, bool)> {
    sessions().live.get(&id).map(|s| (s.command.clone(), s.tty))
}

/// What a shell has been given and not yet run once `chars` follow `line`: a ctrl-c drops
/// what came before it, and a finished command leaves.
pub(crate) fn unrun(line: &str, chars: &str) -> String {
    let typed = format!("{line}{chars}");
    let typed = match typed.rfind('\u{3}') {
        Some(at) => &typed[at + 1..],
        None => &typed,
    };
    let typed = crate::permissions::bash::as_typed(typed);
    typed[crate::permissions::bash::finished(&typed)..].to_string()
}

/// The command session `id` runs, and what typing `chars` into it gives the shell since
/// it last finished a command: what is left of earlier writes, then `chars`.
pub(crate) fn input_line(id: u32, chars: &str) -> Option<(String, String)> {
    let sessions = sessions();
    let session = sessions.live.get(&id)?;
    Some((session.command.clone(), format!("{}{chars}", session.line)))
}

/// What a session is told to do: `\u{3}` interrupts a session without a terminal, and
/// nothing else can be typed into one.
pub(crate) fn check_input(id: u32, chars: &str) -> Result<(String, bool), String> {
    let Some((command, tty)) = session(id) else {
        return Err(format!(
            "no running session {id}. It has exited, or was never started; run the command \
again with `bash`."
        ));
    };
    if !tty && !chars.is_empty() && chars != "\u{3}" {
        return Err(format!(
            "session {id} has no terminal, so only \"\\u0003\" (ctrl-c) can be sent to it. \
Start the command with `tty: true` to type into it."
        ));
    }
    Ok((command, tty))
}

/// Type `chars` into session `id`, then wait up to `wait` for it to exit, with the
/// output it printed since it was last read. An interrupt kills it.
pub(crate) async fn write(id: u32, chars: &str, wait: Duration, live: Live<'_>) -> String {
    let started = Instant::now();
    let proc = {
        let mut sessions = sessions();
        let Some(session) = sessions.live.get_mut(&id) else {
            return format!("no running session {id}.");
        };
        session.used = Instant::now();
        Arc::clone(&session.proc)
    };
    let mut proc = proc.lock().await;
    if !chars.is_empty() {
        let sent = match proc.input.as_mut() {
            Some(input) => match input.write_all(chars.as_bytes()).await {
                Ok(()) => input.flush().await,
                Err(e) => Err(e),
            },
            None if chars == "\u{3}" => {
                signal_group(proc.group, libc::SIGINT);
                Ok(())
            }
            None => Err(io::Error::other("the session has no terminal")),
        };
        if let Err(e) = sent {
            return format!("Could not write to session {id}: {e}");
        }
        if let Some(session) = sessions().live.get_mut(&id) {
            // Only a shell's typing is read as commands.
            session.line = match crate::permissions::bash::is_shell(&session.command) {
                true => unrun(&session.line, chars),
                false => String::new(),
            };
        }
    }
    let waited = proc.wait(tokio::time::Instant::now() + wait, live).await;
    let (stdout, stderr) = proc.take();
    match waited {
        Ok(None) => running(id, &stdout, &stderr, started.elapsed()),
        Ok(Some(status)) => {
            sessions().live.remove(&id);
            format_output(&stdout, &stderr, status, started.elapsed())
        }
        Err(e) => {
            sessions().live.remove(&id);
            format!("Could not wait for session {id}: {e}")
        }
    }
}

/// Kill every session, when bhai exits: a session's process group is its own, so
/// nothing else would end a dev server left running.
pub fn kill_all() {
    for (_, session) in std::mem::take(&mut sessions().live) {
        kill_group(session.group);
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

/// The result for a command still running: its session, then what it printed since it
/// was last read.
fn running(id: u32, stdout: &Kept, stderr: &Kept, took: Duration) -> String {
    format!(
        "{RUNNING}{id}\n{}{}",
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

    fn session_id(out: &str) -> u32 {
        match outcome(out) {
            Some(Outcome::Running(id)) => id,
            other => panic!("{other:?}: {out}"),
        }
    }

    #[tokio::test]
    async fn a_command_still_running_yields_a_session_that_a_poll_finishes() {
        let wait = Duration::from_millis(300);
        let command = "printf 'a\\n'; sleep 1; printf 'b\\n'; exit 4";
        let out = start(command, None, wait, false, quiet()).await;
        let id = session_id(&out);
        assert!(out.starts_with(&format!("{RUNNING}{id}\na\n")), "{out}");
        assert!(out.contains("[output: 1 line, "), "{out}");
        assert_eq!(session(id), Some((command.to_string(), false)));

        let out = write(id, "", Duration::from_secs(5), quiet()).await;
        // Only what it printed since the last read.
        assert!(out.starts_with("exit code: 4\nb\n"), "{out}");
        assert_eq!(outcome(&out), Some(Outcome::Failed(4)));
        assert_eq!(session(id), None);
        assert!(check_input(id, "").is_err());
    }

    #[tokio::test]
    async fn a_quick_command_finishes_in_one_call() {
        let out = start("echo hi", None, YIELD, false, quiet()).await;
        assert!(out.starts_with("exit code: 0\nhi\n"), "{out}");
    }

    #[tokio::test]
    async fn a_tty_session_takes_typed_input_and_strips_the_terminal_codes() {
        let command = "printf '\\033[1;32mready\\033[0m\\n'; read line; echo \"got $line\"";
        let out = start(command, None, Duration::from_millis(500), true, quiet()).await;
        let id = session_id(&out);
        assert!(out.contains("\nready\n"), "{out}");
        assert!(!out.contains('\u{1b}') && !out.contains('\r'), "{out:?}");
        assert!(check_input(id, "hello\n").is_ok());

        let out = write(id, "hello\n", Duration::from_secs(5), quiet()).await;
        assert_eq!(outcome(&out), Some(Outcome::Succeeded), "{out}");
        assert!(out.contains("got hello"), "{out}");
        assert!(!out.contains('\r'), "{out:?}");
    }

    /// The policy reads the session's command and the line it has so far, so a line
    /// finished across two writes is ruled on whole.
    #[tokio::test]
    async fn typing_into_a_shell_session_is_ruled_on_as_the_line_it_finishes() {
        use crate::permissions::{Mode, Policy, Reserved, Rules, Trust};
        let dir = std::env::temp_dir().join(format!("bhai-stdin-{}", uuid::Uuid::new_v4()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let policy = Policy::new(Mode::Auto, Rules::default(), None, repo.clone())
            .with_trust(Trust::new(&dir.join("config"), &repo));
        policy.trust().unwrap();

        let shell = "bash --norc --noprofile";
        let out = start(shell, None, Duration::from_millis(300), true, quiet()).await;
        let id = session_id(&out);
        let typed = |chars: &str| json!({ "session_id": id, "chars": chars });
        assert!(policy.judgeable("write_stdin", &typed("cat .e")).is_ok());
        write(id, "cat .e", Duration::from_millis(250), quiet()).await;
        assert_eq!(
            policy.judgeable("write_stdin", &typed("nv\n")),
            Err(Reserved::Protected(".env".to_string()))
        );
        write(id, "\u{3}", Duration::from_millis(250), quiet()).await;
        assert!(policy.judgeable("write_stdin", &typed("nv\n")).is_ok());
        write(id, "exit\n", Duration::from_secs(5), quiet()).await;

        let out = start("cat", None, Duration::from_millis(300), true, quiet()).await;
        let id = session_id(&out);
        assert_eq!(
            policy.judgeable("write_stdin", &json!({ "session_id": id, "chars": "x\n" })),
            Err(Reserved::Typed("cat".to_string()))
        );
        write(id, "\u{4}", Duration::from_secs(5), quiet()).await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_session_without_a_terminal_only_takes_ctrl_c() {
        let start_at = Instant::now();
        let out = start("sleep 30", None, YIELD_MIN, false, quiet()).await;
        let id = session_id(&out);
        let err = check_input(id, "y\n").unwrap_err();
        assert!(err.contains("tty: true"), "{err}");
        let out = write(id, "\u{3}", Duration::from_secs(5), quiet()).await;
        assert!(
            !matches!(outcome(&out), Some(Outcome::Running(_)) | None),
            "{out}"
        );
        assert!(start_at.elapsed() < Duration::from_secs(5), "{out}");
        assert_eq!(session(id), None);
    }

    #[tokio::test]
    async fn an_interrupt_while_polling_kills_the_session() {
        let out = start("sleep 30", None, YIELD_MIN, false, quiet()).await;
        let id = session_id(&out);
        let cancel = AtomicBool::new(true);
        let live = Live {
            progress: &|_| {},
            cancel: &cancel,
            conversation: None,
        };
        let start_at = Instant::now();
        let out = write(id, "", Duration::from_secs(30), live).await;
        assert!(start_at.elapsed() < Duration::from_secs(2), "{out}");
        assert_eq!(outcome(&out), Some(Outcome::Killed), "{out}");
        assert_eq!(session(id), None);
    }

    /// Nobody reads between polls, so without the readers a chatty command would stop
    /// on a full pipe until the next one.
    #[tokio::test]
    async fn output_is_read_between_polls() {
        let command = "sleep 0.5; yes line | head -n 100000; echo finished";
        let out = start(command, None, YIELD_MIN, false, quiet()).await;
        let id = session_id(&out);
        tokio::time::sleep(Duration::from_secs(2)).await;
        let polled = Instant::now();
        let out = write(id, "", Duration::from_secs(10), quiet()).await;
        assert!(polled.elapsed() < Duration::from_secs(1), "{out}");
        assert!(out.starts_with("exit code: 0\n"), "{out}");
        assert!(
            out.contains("bytes trimmed") && out.contains("finished"),
            "{out}"
        );
        assert!(out.contains("[output: 100001 lines, "), "{out}");
    }

    #[tokio::test]
    async fn output_nobody_polls_for_is_not_queued_past_the_cap() {
        let command = "yes line | head -n 200000; echo finished";
        let proc = Proc::spawn(command, None, None, false).unwrap();
        while proc.lock().open > 0 {
            tokio::time::sleep(TICK).await;
        }
        let pending = std::mem::take(&mut proc.lock().pending);
        assert!(pending.len() <= KEEP, "{}", pending.len());
        assert!(pending.ends_with(b"line\nfinished\n"));
    }

    #[test]
    fn a_cut_queue_starts_on_a_whole_character() {
        let mut pending = Vec::new();
        queue(&mut pending, "é".repeat(KEEP).as_bytes());
        queue(&mut pending, b"x");
        assert!(pending.len() <= KEEP);
        assert!(std::str::from_utf8(&pending).unwrap().ends_with("éx"));
    }

    #[test]
    fn terminal_output_is_read_as_the_text_it_shows() {
        assert_eq!(plain("\u{1b}[1;31mred\u{1b}[0m\r\n"), "red\n");
        assert_eq!(plain("\u{1b}]0;title\u{7}a\u{1b}]8;;x\u{1b}\\b"), "ab");
        assert_eq!(plain("10%\r50%\r100%\r\ndone"), "100%\ndone");
        assert_eq!(plain("\u{1b}(Bok\u{1b}="), "ok");
    }

    #[test]
    fn a_call_asks_for_a_session_with_its_wait_clamped() {
        assert_eq!(parse_yield(&json!({})), Ok(YIELD));
        assert_eq!(parse_yield(&json!({"yield_time_ms": 1})), Ok(YIELD_MIN));
        assert_eq!(
            parse_yield(&json!({"yield_time_ms": 10_000_000})),
            Ok(YIELD_MAX)
        );
        assert!(parse_yield(&json!({"yield_time_ms": -1})).is_err());
        assert!(parse_tty(&json!({"tty": "yes"})).is_err());
        let args = json!({"command": "python3", "tty": true});
        assert_eq!(Bash.describe(&args).unwrap(), "python3  (tty)");
        assert_eq!(
            outcome("Process running with session ID 12\n"),
            Some(Outcome::Running(12))
        );
        assert!(Outcome::Running(12).ok());
    }

    #[tokio::test]
    async fn the_line_count_includes_trimmed_output() {
        let out = run("yes | head -n 200000", None, quiet()).await;
        assert!(out.contains("bytes trimmed"), "{out}");
        assert!(out.contains("\n[output: 200000 lines, "), "{out}");
    }
}
