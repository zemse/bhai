//! Private PTY supervisor and detachable native terminal frontend.
#![allow(unsafe_code)]

use anyhow::{Context, Result, bail, ensure};
use ratatui::crossterm;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, unix::AsyncFd},
    net::{UnixListener, UnixStream},
    process::Command,
    sync::mpsc,
};

pub const HOST_FLAG: &str = "__terminal-host";
pub const CHILD_FLAG: &str = "__terminal-child";
pub const SOCKET_ENV: &str = crate::childenv::TERMINAL_SOCKET_ENV;
const ATTACH: u8 = 1;
const INPUT: u8 = 2;
const RESIZE: u8 = 3;
const MINIMISE: u8 = 4;
const STATUS: u8 = 5;
const OUTPUT: u8 = 6;
const OK: u8 = 7;
const ERROR: u8 = 8;
const EXIT: u8 = 9;
const MAX_FRAME: usize = 65536;
type Frame = (u8, Vec<u8>);

#[derive(Debug, Serialize, Deserialize)]
pub struct Status {
    pub attached: bool,
}

fn private_dir(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    // SAFETY: geteuid has no preconditions.
    ensure!(
        meta.is_dir()
            && !meta.file_type().is_symlink()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0,
        "terminal directory must be owned and private"
    );
    Ok(())
}

fn check_path(path: &Path, socket: bool) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    ensure!(
        path.is_absolute() && path.as_os_str().as_bytes().len() < 104,
        "terminal socket path must be absolute and shorter than 104 bytes"
    );
    private_dir(path.parent().context("socket has no parent")?)?;
    // Reject symlinks in every ancestor, including otherwise trusted registry paths.
    for ancestor in path.parent().unwrap().ancestors() {
        let meta = fs::symlink_metadata(ancestor)?;
        // SAFETY: geteuid has no preconditions.
        ensure!(
            meta.is_dir()
                && !meta.file_type().is_symlink()
                && (meta.uid() == 0 || meta.uid() == unsafe { libc::geteuid() })
                && meta.mode() & 0o022 == 0,
            "untrusted terminal socket ancestor"
        );
    }
    if socket {
        let meta = fs::symlink_metadata(path)?;
        // SAFETY: geteuid has no preconditions.
        ensure!(
            meta.file_type().is_socket()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.mode() & 0o077 == 0,
            "terminal socket must be owned and private"
        );
    }
    Ok(())
}

fn terminal_directory() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is unset")?;
    Ok(PathBuf::from(home).join(".config/bhai/terminals"))
}

fn trusted_path(path: &Path) -> Result<()> {
    let root = terminal_directory()?;
    ensure!(
        path.starts_with(&root)
            && !path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir)),
        "socket is outside the terminal directory"
    );
    private_dir(&root)?;
    Ok(())
}

fn create_terminal_directory() -> Result<PathBuf> {
    let directory = terminal_directory()?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)?;
    private_dir(&directory)?;
    Ok(directory)
}

pub async fn launch(args: Vec<String>) -> Result<()> {
    let directory = create_terminal_directory()?;
    let path = directory.join(format!(
        "{}.sock",
        &uuid::Uuid::new_v4().simple().to_string()[..16]
    ));
    check_path(&path, false)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg(HOST_FLAG)
        .arg(&path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: the closure only invokes async-signal-safe process setup functions.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }
    let mut supervisor = command.spawn()?;
    for _ in 0..200 {
        if matches!(
            tokio::time::timeout(Duration::from_millis(250), status(&path)).await,
            Ok(Ok(_))
        ) {
            return attach(&path).await;
        }
        if let Some(status) = supervisor.try_wait()? {
            bail!("terminal host exited: {status}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    bail!("terminal host did not become ready")
}

pub async fn host(path: PathBuf, args: Vec<String>) -> Result<()> {
    trusted_path(&path)?;
    let mut command = Command::new(std::env::current_exe()?);
    command.arg(CHILD_FLAG).args(args);
    host_inner(path, command, true).await
}

struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(meta) = fs::symlink_metadata(&self.path)
            && meta.dev() == self.device
            && meta.ino() == self.inode
            && meta.file_type().is_socket()
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct WorkerGuard(Option<u32>);
impl Drop for WorkerGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // SAFETY: this is the unreaped child's dedicated process group.
            unsafe {
                libc::killpg(pid as i32, libc::SIGKILL);
            }
        }
    }
}

fn pty() -> Result<(AsyncFd<OwnedFd>, OwnedFd)> {
    let (mut master, mut slave) = (-1, -1);
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty initializes both descriptors and reads a valid winsize.
    ensure!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        } == 0,
        "openpty: {}",
        io::Error::last_os_error()
    );
    // SAFETY: openpty returned newly owned descriptors.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [&master, &slave] {
        // SAFETY: both descriptors are owned here.
        ensure!(
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } >= 0,
            "fcntl: {}",
            io::Error::last_os_error()
        );
    }
    // SAFETY: only the master is changed to nonblocking mode.
    ensure!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } >= 0,
        "fcntl: {}",
        io::Error::last_os_error()
    );
    // SAFETY: OwnedFd keeps the descriptor open and stable until deregistration.
    Ok((unsafe { AsyncFd::register(master) }?, slave))
}

fn resize(fd: &impl AsRawFd, bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() == 4, "invalid terminal size");
    let cols = u16::from_be_bytes([bytes[0], bytes[1]]);
    let rows = u16::from_be_bytes([bytes[2], bytes[3]]);
    ensure!(cols > 0 && rows > 0, "empty terminal size");
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: ioctl reads a valid winsize for this owned PTY descriptor.
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &size) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    // Editors need a repaint even when a new terminal has the same dimensions.
    let mut foreground: libc::pid_t = 0;
    // SAFETY: the group comes from this owned PTY's foreground process group.
    unsafe {
        if libc::ioctl(fd.as_raw_fd(), libc::TIOCGPGRP, &mut foreground) == 0 && foreground > 0 {
            libc::killpg(foreground, libc::SIGWINCH);
        }
    }
    Ok(())
}

async fn fd_read(fd: &AsyncFd<OwnedFd>, bytes: &mut [u8]) -> io::Result<usize> {
    loop {
        let mut ready = fd.readable().await?;
        let result = ready.try_io(|fd| {
            // SAFETY: bytes is writable for its entire length and fd is owned.
            let n = unsafe { libc::read(fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        });
        if let Ok(result) = result {
            return result;
        }
    }
}

async fn fd_write(fd: &AsyncFd<OwnedFd>, bytes: &[u8]) -> io::Result<usize> {
    loop {
        let mut ready = fd.writable().await?;
        if let Ok(result) = ready.try_io(|fd| {
            // SAFETY: bytes is readable for its entire length and fd is owned.
            let n = unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }) {
            let n = result?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            return Ok(n);
        }
    }
}

async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Frame> {
    let kind = reader.read_u8().await?;
    let len = reader.read_u32().await? as usize;
    if len > MAX_FRAME {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes).await?;
    Ok((kind, bytes))
}

async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), frame: &Frame) -> io::Result<()> {
    writer.write_u8(frame.0).await?;
    writer.write_u32(frame.1.len() as u32).await?;
    writer.write_all(&frame.1).await
}

struct Request {
    id: u64,
    frame: Frame,
    reply: mpsc::Sender<Frame>,
}

async fn connection(stream: UnixStream, id: u64, requests: mpsc::Sender<Request>) {
    let (mut reader, mut writer) = stream.into_split();
    let (reply, mut replies) = mpsc::channel(32);
    let closed = requests.clone();
    let close_reply = reply.downgrade();
    let input = tokio::spawn(async move {
        let first = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut reader)).await;
        let Ok(Ok(frame)) = first else {
            return;
        };
        if requests
            .send(Request {
                id,
                frame,
                reply: reply.clone(),
            })
            .await
            .is_err()
        {
            return;
        }
        let weak = reply.downgrade();
        drop(reply);
        while let Ok(frame) = read_frame(&mut reader).await {
            let Some(reply) = weak.upgrade() else {
                return;
            };
            if requests.send(Request { id, frame, reply }).await.is_err() {
                return;
            }
        }
        if let Some(reply) = weak.upgrade() {
            let _ = requests
                .send(Request {
                    id,
                    frame: (0, vec![]),
                    reply,
                })
                .await;
        }
    });
    while let Some(frame) = replies.recv().await {
        if !matches!(
            tokio::time::timeout(Duration::from_secs(2), write_frame(&mut writer, &frame)).await,
            Ok(Ok(()))
        ) {
            break;
        }
        if matches!(frame.0, ERROR | STATUS | MINIMISE | EXIT) {
            break;
        }
    }
    input.abort();
    if let Some(reply) = close_reply.upgrade() {
        let _ = closed
            .send(Request {
                id,
                frame: (0, vec![]),
                reply,
            })
            .await;
    }
}

#[cfg(test)]
async fn host_command(path: PathBuf, command: Command) -> Result<()> {
    host_inner(path, command, false).await
}

async fn host_inner(path: PathBuf, mut command: Command, wait_for_terminal: bool) -> Result<()> {
    check_path(&path, false)?;
    let listener = UnixListener::bind(&path)?;
    let meta = fs::symlink_metadata(&path)?;
    let _socket = SocketGuard {
        path: path.clone(),
        device: meta.dev(),
        inode: meta.ino(),
    };
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let (master, slave) = pty()?;
    command
        .env(SOCKET_ENV, &path)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave))
        .kill_on_drop(true);
    // SAFETY: only async-signal-safe calls run between spawn and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            libc::signal(libc::SIGHUP, libc::SIG_DFL);
            libc::signal(libc::SIGUSR1, libc::SIG_IGN);
            Ok(())
        });
    }
    let (requests, mut incoming) = mpsc::channel::<Request>(32);
    let mut attached: Option<(u64, mpsc::Sender<Frame>)> = None;
    let mut next_id = 0;
    let mut connections = tokio::task::JoinSet::new();
    // Startup errors must reach the first terminal, not disappear before it connects.
    let startup = tokio::time::sleep(Duration::from_secs(60));
    tokio::pin!(startup);
    while wait_for_terminal && attached.is_none() {
        tokio::select! {
            _ = &mut startup => bail!("no terminal attached to start the session"),
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                next_id += 1;
                connections.spawn(connection(stream, next_id, requests.clone()));
            }
            Some(request) = incoming.recv() => {
                match request.frame.0 {
                    STATUS => { let _ = request.reply.try_send((STATUS, serde_json::to_vec(&Status { attached: false })?)); }
                    ATTACH if resize(master.get_ref(), &request.frame.1).is_ok() => {
                        let _ = request.reply.try_send((OK, vec![]));
                        attached = Some((request.id, request.reply));
                    }
                    _ => { let _ = request.reply.try_send((ERROR, b"attach a terminal to start the session".to_vec())); }
                }
            }
        }
        while connections.try_join_next().is_some() {}
    }
    let mut worker = command.spawn()?;
    let mut guard = WorkerGuard(worker.id());
    drop(command);
    let mut pending_input = Vec::new();
    // Output the frontend has no room for yet. The pty is not read while it waits, so a
    // slow terminal slows the worker; a stuck one is dropped by the write timeout.
    let mut pending_output: Option<Vec<u8>> = None;
    let mut bytes = [0; 8192];
    let mut pty_open = true;
    loop {
        let sender = attached.as_ref().map(|(_, tx)| tx.clone());
        tokio::select! {
            result = worker.wait() => {
                let status = result?;
                guard.0 = None;
                if let Some((_, tx)) = &attached {
                    let _ = tokio::time::timeout(Duration::from_millis(500), async {
                        if let Some(pending) = pending_output.take()
                            && tx.send((OUTPUT, pending)).await.is_err() { return; }
                        loop {
                            match fd_read(&master, &mut bytes).await {
                                Ok(n) if n > 0 => { if tx.send((OUTPUT, bytes[..n].to_vec())).await.is_err() { break; } }
                                _ => break,
                            }
                        }
                    }).await;
                    let _ = tokio::time::timeout(Duration::from_secs(2), tx.send((EXIT, status.code().unwrap_or(1).to_be_bytes().to_vec()))).await;
                }
                break;
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                next_id += 1;
                connections.spawn(connection(stream, next_id, requests.clone()));
            }
            output = fd_read(&master, &mut bytes), if pty_open && pending_output.is_none() => {
                match output {
                    Ok(0) => pty_open = false,
                    Ok(n) => {
                        if let Some((_, tx)) = &attached {
                            match tx.try_send((OUTPUT, bytes[..n].to_vec())) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full((_, data))) => pending_output = Some(data),
                                Err(mpsc::error::TrySendError::Closed(_)) => attached = None,
                            }
                        }
                    }
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => pty_open = false,
                    Err(error) => return Err(error.into()),
                }
            }
            permit = reserve(sender.as_ref()), if pending_output.is_some() => {
                match permit {
                    Ok(permit) => permit.send((OUTPUT, pending_output.take().unwrap_or_default())),
                    Err(_) => attached = None,
                }
            }
            written = fd_write(&master, &pending_input), if !pending_input.is_empty() => {
                match written {
                    Ok(n) => { pending_input.drain(..n); }
                    Err(_) => { attached = None; }
                }
            }
            Some(request) = incoming.recv() => {
                let Request { id, frame: (kind, data), reply } = request;
                let owns = attached.as_ref().is_some_and(|(current, _)| *current == id);
                match kind {
                    ATTACH if attached.is_none() => {
                        if resize(master.get_ref(), &data).is_err() { let _ = reply.try_send((ERROR, b"invalid terminal size".to_vec())); continue; }
                        let _ = reply.try_send((OK, vec![]));
                        attached = Some((id, reply));
                        if let Some(pid) = worker.id() {
                            // SAFETY: worker is an unreaped child, not a registry PID.
                            unsafe { libc::kill(pid as i32, libc::SIGUSR1); }
                        }
                    }
                    ATTACH => { let _ = reply.try_send((ERROR, b"terminal is already attached".to_vec())); }
                    STATUS => { let _ = reply.try_send((STATUS, serde_json::to_vec(&Status { attached: attached.is_some() })?)); }
                    MINIMISE => {
                        if let Some((_, tx)) = attached.take() { let _ = tx.try_send((MINIMISE, vec![])); }
                        let _ = reply.try_send((MINIMISE, vec![]));
                    }
                    INPUT if owns => {
                        if pending_input.len() + data.len() <= MAX_FRAME {
                            pending_input.extend_from_slice(&data);
                        } else { attached = None; }
                    }
                    RESIZE if owns => { if resize(master.get_ref(), &data).is_err() { attached = None; } }
                    0 if owns => { attached = None; }
                    0 => {}
                    _ => { let _ = reply.try_send((ERROR, b"invalid terminal request".to_vec())); }
                }
            }
        }
        if attached.is_none() {
            pending_input.clear();
            pending_output = None;
        }
        while connections.try_join_next().is_some() {}
    }
    drop(attached);
    drop(incoming);
    drop(requests);
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    Ok(())
}

async fn reserve(
    sender: Option<&mpsc::Sender<Frame>>,
) -> Result<mpsc::Permit<'_, Frame>, mpsc::error::SendError<()>> {
    match sender {
        Some(sender) => sender.reserve().await,
        None => Err(mpsc::error::SendError(())),
    }
}

async fn connect(path: &Path) -> Result<UnixStream> {
    trusted_path(path)?;
    check_path(path, true)?;
    Ok(UnixStream::connect(path).await?)
}

pub async fn status(path: &Path) -> Result<Status> {
    let mut stream = connect(path).await?;
    write_frame(&mut stream, &(STATUS, vec![])).await?;
    let (kind, bytes) = read_frame(&mut stream).await?;
    ensure!(kind == STATUS, "terminal host refused status");
    Ok(serde_json::from_slice(&bytes)?)
}

pub async fn minimise() -> Result<()> {
    let path = std::env::var_os(SOCKET_ENV).context("this session has no terminal host")?;
    let mut stream = connect(Path::new(&path)).await?;
    write_frame(&mut stream, &(MINIMISE, vec![])).await?;
    ensure!(
        read_frame(&mut stream).await?.0 == MINIMISE,
        "terminal host refused minimise"
    );
    Ok(())
}

fn size_bytes() -> Result<Vec<u8>> {
    let (cols, rows) = crossterm::terminal::size()?;
    Ok([cols.to_be_bytes(), rows.to_be_bytes()].concat())
}

struct TerminalInput {
    incoming: mpsc::Receiver<io::Result<Vec<u8>>>,
    wake: std::os::unix::net::UnixStream,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TerminalInput {
    fn new() -> io::Result<Self> {
        let tty = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open("/dev/tty")?;
        let (wake, stop) = std::os::unix::net::UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        let (send, incoming) = mpsc::channel(32);
        // Darwin cannot register /dev/tty with kqueue. Poll has an explicit wake fd.
        let thread = std::thread::Builder::new()
            .name("terminal-input".into())
            .spawn(move || {
                let mut fds = [
                    libc::pollfd {
                        fd: tty.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: stop.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                let mut bytes = [0u8; 8192];
                loop {
                    // SAFETY: poll reads and writes exactly these two live descriptors.
                    let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
                    if ready < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        let _ = send.try_send(Err(error));
                        break;
                    }
                    if fds[1].revents != 0 {
                        break;
                    }
                    if fds[0].revents == 0 {
                        continue;
                    }
                    // SAFETY: tty is nonblocking and bytes is writable for its full length.
                    let n = unsafe {
                        libc::read(tty.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len())
                    };
                    if n < 0 {
                        let error = io::Error::last_os_error();
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) {
                            continue;
                        }
                        let _ = send.try_send(Err(error));
                        break;
                    }
                    if send
                        .blocking_send(Ok(bytes[..n as usize].to_vec()))
                        .is_err()
                        || n == 0
                    {
                        break;
                    }
                }
            })?;
        Ok(Self {
            incoming,
            wake,
            thread: Some(thread),
        })
    }
}

impl Drop for TerminalInput {
    fn drop(&mut self) {
        self.incoming.close();
        let _ = self.wake.write_all(&[1]);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Pop all keyboard enhancements, then restore the shell's terminal surface.
        let _ = io::stdout().write_all(b"\x1b[<999u\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1015l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[0m\x1b[?25h\x1b[?1049l");
        let _ = io::stdout().flush();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

pub async fn attach(path: &Path) -> Result<()> {
    let mut stream = connect(path).await?;
    write_frame(&mut stream, &(ATTACH, size_bytes()?)).await?;
    let (kind, message) = read_frame(&mut stream).await?;
    ensure!(kind == OK, "{}", String::from_utf8_lossy(&message));
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    crossterm::terminal::enable_raw_mode()?;
    let _guard = TerminalGuard;
    let mut input = TerminalInput::new()?;
    io::stdout().write_all(
        b"\x1b[?1049h\x1b[2J\x1b[H\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h\x1b[?2004h\x1b[?1004h\x1b[=1u",
    )?;
    io::stdout().flush()?;
    let mut resized =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let (mut reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::channel(32);
    let output = tokio::spawn(async move {
        while let Ok(frame) = read_frame(&mut reader).await {
            if tx.send(frame).await.is_err() {
                break;
            }
        }
    });
    // Aborting this task drops the socket read half even on errors below.
    struct Abort(tokio::task::JoinHandle<()>);
    impl Drop for Abort {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _output = Abort(output);
    loop {
        tokio::select! {
            frame = rx.recv() => match frame {
                Some((OUTPUT, bytes)) => { io::stdout().write_all(&bytes)?; io::stdout().flush()?; }
                Some((MINIMISE, _)) | None => break,
                Some((EXIT, bytes)) => {
                    let code = i32::from_be_bytes(bytes.try_into().map_err(|_| anyhow::anyhow!("invalid worker exit status"))?);
                    ensure!(code == 0, "session worker exited with status {code}");
                    break;
                }
                _ => bail!("invalid terminal host response"),
            },
            bytes = input.incoming.recv() => {
                let Some(bytes) = bytes else { break; };
                let bytes = bytes?;
                if bytes.is_empty() { break; }
                write_frame(&mut writer, &(INPUT, bytes)).await?;
            }
            _ = resized.recv() => { write_frame(&mut writer, &(RESIZE, size_bytes()?)).await?; }
            _ = hangup.recv() => break,
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let root = create_terminal_directory().unwrap();
            let path = root.join(format!(
                "bhai-term-{}",
                &uuid::Uuid::new_v4().simple().to_string()[..12]
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
        fn socket(&self) -> PathBuf {
            self.0.join("s")
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    async fn start(directory: &Directory, script: &str) -> tokio::task::JoinHandle<Result<()>> {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script).current_dir(&directory.0);
        let task = tokio::spawn(host_command(directory.socket(), command));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if status(&directory.socket()).await.is_ok() {
                    break;
                }
                assert!(!task.is_finished(), "host exited before readiness");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task
    }

    async fn frontend(path: &Path, cols: u16, rows: u16) -> UnixStream {
        let mut stream = connect(path).await.unwrap();
        write_frame(
            &mut stream,
            &(ATTACH, [cols.to_be_bytes(), rows.to_be_bytes()].concat()),
        )
        .await
        .unwrap();
        assert_eq!(read_frame(&mut stream).await.unwrap().0, OK);
        stream
    }

    async fn until(stream: &mut UnixStream, needle: &str) -> String {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut output = String::new();
            loop {
                let (kind, bytes) = read_frame(stream).await.unwrap();
                assert_eq!(kind, OUTPUT);
                output.push_str(&String::from_utf8_lossy(&bytes));
                if output.contains(needle) {
                    return output;
                }
            }
        })
        .await
        .unwrap()
    }

    async fn detached(path: &Path) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while status(path).await.unwrap().attached {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn detached_output_is_drained_without_replay() {
        let directory = Directory::new();
        let task = start(&directory, "dd if=/dev/zero bs=65536 count=64 2>/dev/null; touch drained; read line; printf 'FRESH'; read line").await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !directory.0.join("drained").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut stream = frontend(&directory.socket(), 80, 24).await;
        write_frame(&mut stream, &(INPUT, b"go\n".to_vec()))
            .await
            .unwrap();
        let output = until(&mut stream, "FRESH").await;
        assert!(!output.contains('\0'));
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn a_briefly_slow_frontend_stays_attached_through_a_burst() {
        let directory = Directory::new();
        let task = start(
            &directory,
            "read line; dd if=/dev/zero bs=4096 count=512 2>/dev/null | tr '\\0' x; printf 'DONE'; read line",
        )
        .await;
        let path = directory.socket();
        let mut stream = frontend(&path, 80, 24).await;
        write_frame(&mut stream, &(INPUT, b"go\n".to_vec()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        // Only the tail is searched: the burst is too long to rescan per frame.
        tokio::time::timeout(Duration::from_secs(20), async {
            let mut tail = Vec::new();
            loop {
                let (kind, bytes) = read_frame(&mut stream).await.unwrap();
                assert_eq!(kind, OUTPUT);
                tail.extend_from_slice(&bytes);
                if tail.windows(4).any(|w| w == b"DONE") {
                    break;
                }
                tail.drain(..tail.len().saturating_sub(3));
            }
        })
        .await
        .unwrap();
        assert!(status(&path).await.unwrap().attached);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn reattach_resize_controls_and_single_lease() {
        let directory = Directory::new();
        let task = start(&directory, "while read line; do stty size; done").await;
        let path = directory.socket();
        let mut first = frontend(&path, 91, 37).await;
        assert!(status(&path).await.unwrap().attached);
        let mut second = connect(&path).await.unwrap();
        write_frame(&mut second, &(ATTACH, vec![0, 80, 0, 24]))
            .await
            .unwrap();
        assert_eq!(read_frame(&mut second).await.unwrap().0, ERROR);
        write_frame(&mut first, &(INPUT, b"size\n".to_vec()))
            .await
            .unwrap();
        until(&mut first, "37 91").await;
        write_frame(&mut first, &(RESIZE, vec![0, 99, 0, 42]))
            .await
            .unwrap();
        write_frame(&mut first, &(INPUT, b"size\n".to_vec()))
            .await
            .unwrap();
        until(&mut first, "42 99").await;
        let mut control = connect(&path).await.unwrap();
        write_frame(&mut control, &(MINIMISE, vec![]))
            .await
            .unwrap();
        assert_eq!(read_frame(&mut control).await.unwrap().0, MINIMISE);
        assert_eq!(read_frame(&mut first).await.unwrap().0, MINIMISE);
        detached(&path).await;
        let mut again = frontend(&path, 75, 25).await;
        write_frame(&mut again, &(INPUT, b"size\n".to_vec()))
            .await
            .unwrap();
        until(&mut again, "25 75").await;
        drop(again);
        detached(&path).await;
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn startup_waits_for_a_terminal_and_delivers_failure_output_and_exit_status() {
        let directory = Directory::new();
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("echo started > started; printf 'startup-failure'; exit 7")
            .current_dir(&directory.0);
        let task = tokio::spawn(host_inner(directory.socket(), command, true));
        tokio::time::timeout(Duration::from_secs(5), async {
            while status(&directory.socket()).await.is_err() {
                assert!(!task.is_finished());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!directory.0.join("started").exists());
        let mut stream = frontend(&directory.socket(), 80, 24).await;
        assert!(
            until(&mut stream, "startup-failure")
                .await
                .contains("startup-failure")
        );
        let (kind, code) = read_frame(&mut stream).await.unwrap();
        assert_eq!(kind, EXIT);
        assert_eq!(i32::from_be_bytes(code.try_into().unwrap()), 7);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!directory.socket().exists());
    }

    #[tokio::test]
    async fn worker_exit_closes_frontend_and_cleans_socket() {
        let directory = Directory::new();
        let task = start(&directory, "read line; exit 0").await;
        let mut stream = frontend(&directory.socket(), 80, 24).await;
        write_frame(&mut stream, &(INPUT, b"quit\n".to_vec()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while read_frame(&mut stream).await.is_ok() {}
        })
        .await
        .unwrap();
        assert!(!directory.socket().exists());
    }

    #[tokio::test]
    async fn permissions_symlinks_and_path_length_are_checked() {
        use std::os::unix::fs::symlink;
        let directory = Directory::new();
        let path = directory.socket();
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_path(&path, true).is_ok());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(connect(&path).await.is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(connect(&path).await.is_err());
        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o700)).unwrap();
        let alias = directory.0.join("alias");
        symlink(&path, &alias).unwrap();
        assert!(connect(&alias).await.is_err());
        assert!(check_path(&directory.0.join("x".repeat(104)), false).is_err());
        drop(listener);
    }

    #[tokio::test]
    async fn supervisor_cancellation_stops_worker() {
        let directory = Directory::new();
        let task = start(
            &directory,
            "echo $$ > worker.pid; while read line; do :; done",
        )
        .await;
        let pid = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(text) = fs::read_to_string(directory.0.join("worker.pid"))
                    && let Ok(pid) = text.trim().parse::<i32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // SAFETY: signal zero checks existence without changing the process.
                if unsafe { libc::kill(pid, 0) } == -1 {
                    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!directory.socket().exists());
    }

    #[test]
    fn fake_redraw_worker() {
        use std::io::BufRead;
        if std::env::var_os("BHAI_TEST_TERMINAL_WORKER").is_none() {
            return;
        }
        extern "C" fn redraw(_: libc::c_int) {
            // SAFETY: write is async-signal-safe; this static byte slice remains valid.
            unsafe {
                libc::write(1, b"REDRAW".as_ptr().cast(), 6);
            }
        }
        // The first attach arrives while SIGUSR1 is still inherited as ignored.
        std::thread::sleep(Duration::from_millis(200));
        // SAFETY: the handler only writes a fixed marker to stdout.
        unsafe {
            libc::signal(libc::SIGUSR1, redraw as *const () as libc::sighandler_t);
        }
        fs::write("ready", "").unwrap();
        for line in io::stdin().lock().lines() {
            if line.unwrap() == "quit" {
                break;
            }
        }
    }

    #[tokio::test]
    async fn startup_signal_is_safe_and_reattach_requests_redraw() {
        let directory = Directory::new();
        let path = directory.socket();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "terminal_host::tests::fake_redraw_worker",
                "--nocapture",
            ])
            .env("BHAI_TEST_TERMINAL_WORKER", "1")
            .current_dir(&directory.0);
        let task = tokio::spawn(host_command(path.clone(), command));
        tokio::time::timeout(Duration::from_secs(5), async {
            while status(&path).await.is_err() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let first = frontend(&path, 80, 24).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !directory.0.join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        drop(first);
        detached(&path).await;
        let mut second = frontend(&path, 90, 30).await;
        until(&mut second, "REDRAW").await;
        write_frame(&mut second, &(INPUT, b"quit\n".to_vec()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    fn terminal_flags(fd: libc::c_int) -> libc::tcflag_t {
        let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr initializes termios on success for this live PTY.
        assert_eq!(unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) }, 0);
        // SAFETY: tcgetattr succeeded.
        unsafe { termios.assume_init().c_lflag }
    }

    #[test]
    fn fake_frontend() {
        let Some(path) = std::env::var_os("BHAI_TEST_TERMINAL_FRONTEND") else {
            return;
        };
        let before = terminal_flags(0);
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(attach(Path::new(&path)))
            .unwrap();
        // Restoring canonical mode can set the kernel's pending-input flag.
        assert_eq!(terminal_flags(0) & !libc::PENDIN, before & !libc::PENDIN);
    }

    #[tokio::test]
    async fn frontend_exits_with_idle_input_and_restores_terminal() {
        let directory = Directory::new();
        let path = directory.socket();
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let (master, slave) = pty().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "terminal_host::tests::fake_frontend",
                "--nocapture",
            ])
            .env("BHAI_TEST_TERMINAL_FRONTEND", &path)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()))
            .kill_on_drop(true);
        // SAFETY: only async-signal-safe process setup runs in pre_exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        drop(command);
        let (mut client, _) = tokio::select! {
            accepted = listener.accept() => accepted.unwrap(),
            exited = child.wait() => {
                let mut bytes = [0; 8192];
                let n = fd_read(&master, &mut bytes).await.unwrap();
                panic!("frontend exited before connect: {exited:?}: {}", String::from_utf8_lossy(&bytes[..n]));
            }
            _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("frontend did not connect"),
        };
        assert_eq!(
            read_frame(&mut client).await.unwrap(),
            (ATTACH, vec![0, 80, 0, 24])
        );
        write_frame(&mut client, &(OK, vec![])).await.unwrap();
        write_frame(&mut client, &(MINIMISE, vec![])).await.unwrap();
        let mut output = Vec::new();
        let mut bytes = [0; 8192];
        // Consume terminal output while waiting, just as the real terminal does.
        let exited = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    exited = child.wait() => break exited.unwrap(),
                    read = fd_read(&master, &mut bytes) => {
                        match read {
                            Ok(0) => break child.wait().await.unwrap(),
                            Ok(n) => output.extend_from_slice(&bytes[..n]),
                            Err(error) if error.raw_os_error() == Some(libc::EIO) => break child.wait().await.unwrap(),
                            Err(error) => panic!("terminal output: {error}"),
                        }
                    }
                }
            }
        })
        .await;
        assert!(
            exited.is_ok(),
            "frontend did not exit: {}",
            String::from_utf8_lossy(&output)
        );
        let exited = exited.unwrap();
        while let Ok(Ok(n)) =
            tokio::time::timeout(Duration::from_millis(50), fd_read(&master, &mut bytes)).await
        {
            if n == 0 {
                break;
            }
            output.extend_from_slice(&bytes[..n]);
        }
        let output = String::from_utf8_lossy(&output);
        assert!(exited.success(), "frontend exited: {exited}: {output}");
        for sequence in [
            "\x1b[?1049h",
            "\x1b[?1049l",
            "\x1b[<999u",
            "\x1b[?1006l",
            "\x1b[?2004l",
            "\x1b[?1004l",
        ] {
            assert!(output.contains(sequence));
        }
    }

    #[test]
    fn a_full_frontend_input_queue_is_cancellable() {
        let (send, incoming) = mpsc::channel(1);
        send.try_send(Ok(vec![1])).unwrap();
        let (wake, _stop) = std::os::unix::net::UnixStream::pair().unwrap();
        let thread = std::thread::spawn(move || {
            assert!(send.blocking_send(Ok(vec![2])).is_err());
        });
        drop(TerminalInput {
            incoming,
            wake,
            thread: Some(thread),
        });
    }

    #[tokio::test]
    async fn pending_terminal_input_is_cancellable() {
        let (master, _slave) = pty().unwrap();
        let task = tokio::spawn(async move {
            let mut bytes = [0; 16];
            fd_read(&master, &mut bytes).await
        });
        tokio::task::yield_now().await;
        task.abort();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap_err()
                .is_cancelled()
        );
    }

    #[tokio::test]
    async fn oversized_frames_are_rejected() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer.write_u8(INPUT).await.unwrap();
        writer.write_u32(MAX_FRAME as u32 + 1).await.unwrap();
        assert_eq!(
            read_frame(&mut reader).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
