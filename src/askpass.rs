//! `[bash] sudo = true`: `sudo -A` in a bash call asks for the password in the terminal.
//! `SUDO_ASKPASS` is a script in a private directory that runs `bhai --askpass`, which
//! asks this process over a unix socket beside it and prints the answer to sudo alone,
//! so the password never passes through the command's output, the history or the model.
//!
//! Only a bash call still running can ask, and the question shows that call's exact
//! command. Any process the approved command starts can reach the socket, so what guards
//! the password is the user reading that command, not the socket.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

/// The argument `bhai` runs as the helper with, before the socket and sudo's prompt.
pub const FLAG: &str = "--askpass";
/// Which bash call a helper belongs to, set in that call's environment.
const CALL_VAR: &str = "BHAI_ASKPASS_CALL";
const SOCKET: &str = "sock";
const HELPER: &str = "askpass";
/// A request is a call id and sudo's prompt; anything longer is not one.
const MAX_REQUEST: u64 = 4096;
/// Chars of sudo's prompt shown, which the command chose with `-p`.
const MAX_PROMPT: usize = 200;
/// Bytes a password may have. Reserved up front, so typing never reallocates and leaves
/// a copy behind in freed memory.
const CAPACITY: usize = 1024;

/// A password being typed or on its way to sudo: no `Debug`, no `Clone`, and zeroed,
/// spare capacity included, when dropped.
pub struct Secret(String);

impl Secret {
    pub fn new() -> Self {
        Self(String::with_capacity(CAPACITY))
    }

    /// Past [`CAPACITY`] the rest is dropped rather than reallocated.
    pub fn push(&mut self, c: char) {
        if self.0.len() + c.len_utf8() <= CAPACITY {
            self.0.push(c);
        }
    }

    pub fn pop(&mut self) {
        self.0.pop();
    }

    pub fn clear(&mut self) {
        wipe(&mut self.0);
        self.0 = String::with_capacity(CAPACITY);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Default for Secret {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

fn wipe(text: &mut String) {
    zero(std::mem::take(text).into_bytes());
}

/// `black_box` keeps the writes from being dropped as dead stores.
fn zero(mut bytes: Vec<u8>) {
    bytes.fill(0);
    bytes.resize(bytes.capacity(), 0);
    std::hint::black_box(&bytes);
}

/// sudo asking for a password, for the TUI to put to the user. Dropping `reply` refuses.
pub struct Request {
    /// The bash call's command, exactly as it was approved.
    pub command: String,
    /// What sudo says, control characters removed.
    pub prompt: String,
    pub reply: oneshot::Sender<Option<Secret>>,
}

/// The bash calls running now, by the id their environment carries.
type Calls = Arc<Mutex<HashMap<String, String>>>;

struct Shared {
    helper: PathBuf,
    calls: Calls,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

/// Whether `sudo -A` asks in the terminal, which the bash tool tells the model.
pub fn active() -> bool {
    SHARED.get().is_some()
}

/// Removes the private directory, socket and helper with it, when dropped.
pub struct Guard {
    dir: PathBuf,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Listen for helpers from here on, once for the process. Requests arrive on the
/// receiver, in the order sudo asked.
pub fn start() -> io::Result<(Guard, mpsc::UnboundedReceiver<Request>)> {
    if active() {
        return Err(io::Error::other("already listening"));
    }
    let exe = std::env::current_exe()?;
    let server = Server::bind(&private_dir()?, &exe)?;
    let guard = Guard {
        dir: server.dir.clone(),
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let _ = SHARED.set(Shared {
        helper: server.dir.join(HELPER),
        calls: Arc::clone(&server.calls),
    });
    tokio::spawn(server.serve(tx));
    Ok((guard, rx))
}

/// A bash call that may ask, until it is dropped.
pub struct Call {
    id: String,
    calls: Calls,
    helper: PathBuf,
}

impl Call {
    /// Point `command`'s sudo at the helper, as this call.
    pub fn env(&self, command: &mut tokio::process::Command) {
        command
            .env("SUDO_ASKPASS", &self.helper)
            .env(CALL_VAR, &self.id);
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        lock(&self.calls).remove(&self.id);
    }
}

/// Record `command` as running, when the helper is listening.
pub fn register(command: &str) -> Option<Call> {
    let shared = SHARED.get()?;
    Some(register_in(&shared.calls, &shared.helper, command))
}

fn register_in(calls: &Calls, helper: &Path, command: &str) -> Call {
    let id = uuid::Uuid::new_v4().simple().to_string();
    lock(calls).insert(id.clone(), command.to_string());
    Call {
        id,
        calls: Arc::clone(calls),
        helper: helper.to_path_buf(),
    }
}

fn lock(calls: &Calls) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
    calls.lock().unwrap_or_else(|e| e.into_inner())
}

/// A fresh directory only the user can enter. A unix socket path is capped at 104 bytes
/// on macOS, where the temp dir is already long, so the name is short and a temp dir too
/// long for it gives way to `/tmp`.
fn private_dir() -> io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt as _;
    let id = uuid::Uuid::new_v4().simple().to_string();
    let name = format!("bhai-{}", &id[..12]);
    let mut dir = std::env::temp_dir().join(&name);
    if dir.join(SOCKET).as_os_str().len() > 100 {
        dir = Path::new("/tmp").join(&name);
    }
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    Ok(dir)
}

/// `text` as one single-quoted shell word.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The script sudo runs: it takes no arguments of its own, so the socket is written in.
fn script(exe: &Path, socket: &Path) -> String {
    format!(
        "#!/bin/sh\nexec {} {FLAG} {} \"$@\"\n",
        quote(&exe.to_string_lossy()),
        quote(&socket.to_string_lossy())
    )
}

struct Server {
    dir: PathBuf,
    listener: UnixListener,
    calls: Calls,
}

impl Server {
    /// The socket and the helper script in `dir`, which the caller made private.
    fn bind(dir: &Path, exe: &Path) -> io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let socket = dir.join(SOCKET);
        let listener = UnixListener::bind(&socket)?;
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o700)
            .open(dir.join(HELPER))?
            .write_all(script(exe, &socket).as_bytes())?;
        Ok(Self {
            dir: dir.to_path_buf(),
            listener,
            calls: Calls::default(),
        })
    }

    async fn serve(self, tx: mpsc::UnboundedSender<Request>) {
        while let Ok((stream, _)) = self.listener.accept().await {
            tokio::spawn(answer(stream, Arc::clone(&self.calls), tx.clone()));
        }
    }
}

/// One helper's question. Nothing is written back on a refusal, an unknown call, or a
/// TUI that has gone.
async fn answer(stream: UnixStream, calls: Calls, tx: mpsc::UnboundedSender<Request>) {
    let (read, mut write) = stream.into_split();
    let mut read = tokio::io::BufReader::new(read.take(MAX_REQUEST));
    let mut line = String::new();
    if read.read_line(&mut line).await.is_err() {
        return;
    }
    let Ok(asked) = serde_json::from_str::<Value>(&line) else {
        return;
    };
    let call = asked["call"].as_str().unwrap_or_default();
    let Some(command) = lock(&calls).get(call).cloned() else {
        return;
    };
    let prompt: String = asked["prompt"]
        .as_str()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_PROMPT)
        .collect();
    let (reply, answered) = oneshot::channel();
    if tx
        .send(Request {
            command,
            prompt,
            reply,
        })
        .is_err()
    {
        return;
    }
    // The helper going away (sudo killed, the call interrupted) withdraws the question:
    // the receiver drops with this task, which the TUI sees as a closed reply.
    let secret = tokio::select! {
        secret = answered => secret.ok().flatten(),
        _ = read.read_u8() => None,
    };
    if let Some(secret) = secret {
        let _ = write.write_all(secret.expose().as_bytes()).await;
        let _ = write.write_all(b"\n").await;
    }
}

/// `bhai --askpass <socket> [prompt]`, as sudo runs it: the password on stdout and exit
/// 0, or exit 1 when there is none.
pub fn helper(args: &[String]) -> i32 {
    let (Some(socket), Some(call)) = (args.first(), std::env::var(CALL_VAR).ok()) else {
        eprintln!("bhai: --askpass is run by sudo from a bash call, not by hand");
        return 1;
    };
    let prompt = args.get(1).map_or("", String::as_str);
    match ask(Path::new(socket), &call, prompt) {
        Ok(Some(answer)) => {
            let written = io::stdout().lock().write_all(&answer);
            zero(answer);
            i32::from(written.is_err())
        }
        Ok(None) => {
            eprintln!("bhai: no password was given");
            1
        }
        Err(e) => {
            eprintln!("bhai: could not ask for the password: {e}");
            1
        }
    }
}

/// Ask the session at `socket` for `call`'s password: what to print, newline included,
/// or `None` when it was refused.
fn ask(socket: &Path, call: &str, prompt: &str) -> io::Result<Option<Vec<u8>>> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    let request = json!({ "call": call, "prompt": prompt });
    stream.write_all(format!("{request}\n").as_bytes())?;
    let mut answer = Vec::with_capacity(CAPACITY + 1);
    stream.read_to_end(&mut answer)?;
    Ok(match answer.is_empty() {
        true => None,
        false => Some(answer),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        private_dir().unwrap()
    }

    fn secret(text: &str) -> Secret {
        let mut secret = Secret::new();
        text.chars().for_each(|c| secret.push(c));
        secret
    }

    async fn ask_async(socket: PathBuf, call: String) -> Option<Vec<u8>> {
        tokio::task::spawn_blocking(move || ask(&socket, &call, "[sudo] password for u:\x07"))
            .await
            .unwrap()
            .unwrap()
    }

    #[test]
    fn a_secret_stops_at_its_capacity() {
        let mut typed = secret(&"x".repeat(CAPACITY + 5));
        assert_eq!(typed.expose().len(), CAPACITY);
        typed.pop();
        typed.push('é');
        assert_eq!(typed.expose().len(), CAPACITY - 1);
        typed.clear();
        assert!(typed.is_empty());
    }

    #[test]
    fn the_script_quotes_what_it_runs() {
        let text = script(Path::new("/opt/it's/bhai"), Path::new("/tmp/b x/sock"));
        assert_eq!(
            text,
            "#!/bin/sh\nexec '/opt/it'\\''s/bhai' --askpass '/tmp/b x/sock' \"$@\"\n"
        );
    }

    #[test]
    fn the_helper_and_its_directory_are_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _enter = rt.enter();
        let dir = temp_dir();
        Server::bind(&dir, Path::new("/bin/bhai")).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(HELPER)), 0o700);
        drop(Guard { dir: dir.clone() });
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn a_running_call_gets_the_password_typed_for_it() {
        let dir = temp_dir();
        let server = Server::bind(&dir, Path::new("/bin/bhai")).unwrap();
        let call = register_in(&server.calls, &dir.join(HELPER), "sudo -A ls /root");
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(server.serve(tx));

        let asked = tokio::spawn(ask_async(dir.join(SOCKET), call.id.clone()));
        let request = rx.recv().await.unwrap();
        assert_eq!(request.command, "sudo -A ls /root");
        assert_eq!(request.prompt, "[sudo] password for u:");
        let _ = request.reply.send(Some(secret("hunter2")));
        assert_eq!(asked.await.unwrap().as_deref(), Some(&b"hunter2\n"[..]));

        let asked = tokio::spawn(ask_async(dir.join(SOCKET), call.id.clone()));
        drop(rx.recv().await.unwrap());
        assert_eq!(asked.await.unwrap(), None, "a refusal prints nothing");
        drop(Guard { dir });
    }

    #[tokio::test]
    async fn a_call_that_is_not_running_is_never_asked_about() {
        let dir = temp_dir();
        let server = Server::bind(&dir, Path::new("/bin/bhai")).unwrap();
        let ended = register_in(&server.calls, &dir.join(HELPER), "sudo -A true")
            .id
            .clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(server.serve(tx));

        for call in [ended, "made-up".to_string()] {
            assert_eq!(ask_async(dir.join(SOCKET), call).await, None);
        }
        assert!(rx.try_recv().is_err());
        drop(Guard { dir });
    }

    #[tokio::test]
    async fn a_helper_that_goes_away_withdraws_its_question() {
        let dir = temp_dir();
        let server = Server::bind(&dir, Path::new("/bin/bhai")).unwrap();
        let call = register_in(&server.calls, &dir.join(HELPER), "sudo -A true");
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(server.serve(tx));

        let mut stream = UnixStream::connect(dir.join(SOCKET)).await.unwrap();
        let line = format!("{}\n", json!({ "call": call.id, "prompt": "Password:" }));
        stream.write_all(line.as_bytes()).await.unwrap();
        let mut request = rx.recv().await.unwrap();
        assert!(!request.reply.is_closed());
        drop(stream);
        tokio::time::timeout(std::time::Duration::from_secs(5), request.reply.closed())
            .await
            .unwrap();
        drop(Guard { dir });
    }
}
