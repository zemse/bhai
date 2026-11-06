//! The ssh tunnel `bhai attach --ssh` runs to reach a server on another machine: one
//! `ssh -N -L` from a free local port to the server's, started again whenever it exits,
//! for as long as attach runs.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::sync::watch;

/// Where `bhai serve --token-file` keeps the token on the server, by the README's setup.
pub const REMOTE_TOKEN: &str = "~/.config/bhai/server-token";
/// How long after ssh exits it is started again.
const RESTART: Duration = Duration::from_secs(1);
/// How often a starting tunnel is tried until its local port answers.
const PROBE: Duration = Duration::from_millis(100);
/// How long the first tunnel may take to come up before attach gives up on it.
const FIRST: Duration = Duration::from_secs(20);

/// Every ssh run: never prompt, since nothing could answer under the tui.
const COMMON: [&str; 4] = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"];

/// How to reach the server's machine.
#[derive(Clone, Debug)]
pub struct Ssh {
    /// `ssh`, or a stand-in in tests.
    pub program: String,
    /// `user@host`, or a host from the ssh config.
    pub dest: String,
    /// Given to ssh before `dest`, as `--ssh-arg` gave them.
    pub args: Vec<String>,
}

impl Ssh {
    pub fn new(dest: String, args: Vec<String>) -> Self {
        Self {
            program: "ssh".to_string(),
            dest,
            args,
        }
    }

    /// The arguments for a tunnel from `local` here to `remote` on the server's loopback.
    /// The keep-alives make ssh itself exit within about 10s of the link dying, so it is
    /// started again rather than left holding a dead connection.
    pub fn forward(&self, local: u16, remote: u16) -> Vec<String> {
        let mut args: Vec<String> = [
            "-N",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=2",
        ]
        .into_iter()
        .chain(COMMON)
        .map(str::to_string)
        .collect();
        args.extend(self.args.iter().cloned());
        args.push("-L".to_string());
        args.push(format!("{local}:127.0.0.1:{remote}"));
        args.push(self.dest.clone());
        args
    }

    /// The arguments that print the file at `path` on the server.
    fn cat(&self, path: &str) -> Vec<String> {
        let mut args: Vec<String> = COMMON.into_iter().map(str::to_string).collect();
        args.extend(self.args.iter().cloned());
        args.push(self.dest.clone());
        args.push(format!("cat -- {}", remote_path(path)));
        args
    }

    /// Read the server's token from `path` on its machine. Only ssh's error is ever
    /// shown, never what the file holds.
    pub async fn token(&self, path: &str) -> Result<String> {
        let output = tokio::process::Command::new(&self.program)
            .args(self.cat(path))
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("cannot run {}", self.program))?;
        if !output.status.success() {
            bail!(
                "cannot read the token from {}:{path} over ssh: {}",
                self.dest,
                last_line(&String::from_utf8_lossy(&output.stderr))
                    .unwrap_or_else(|| output.status.to_string())
            );
        }
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if token.is_empty() {
            bail!("{}:{path} is empty", self.dest);
        }
        Ok(token)
    }
}

/// `path` for the remote shell: a leading `~/` left for it to expand, the rest quoted.
fn remote_path(path: &str) -> String {
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    match path.strip_prefix("~/") {
        Some(rest) => format!("~/{}", quote(rest)),
        None => quote(path),
    }
}

/// The last line ssh wrote that says anything.
fn last_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(str::to_string)
}

/// A port on 127.0.0.1 nothing listens on now, for the tunnel's local end.
pub fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .context("cannot find a free local port for the tunnel")?;
    Ok(listener.local_addr()?.port())
}

/// Where the tunnel is.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Starting,
    /// Its local port answers.
    Up,
    /// ssh exited, with the last thing it said; it is started again shortly.
    Down(String),
}

/// A running tunnel, kept up until dropped, which kills ssh.
pub struct Tunnel {
    pub local: u16,
    state: watch::Receiver<State>,
    task: tokio::task::JoinHandle<()>,
}

impl Tunnel {
    /// Start ssh forwarding `local` to `remote` on the server, and keep it running.
    pub fn open(ssh: Ssh, local: u16, remote: u16) -> Self {
        let (tx, state) = watch::channel(State::Starting);
        let task = tokio::spawn(keep(ssh, local, remote, tx));
        Self { local, state, task }
    }

    pub fn state(&self) -> watch::Receiver<State> {
        self.state.clone()
    }

    /// Wait for the first ssh to come up, or say why it did not.
    pub async fn first(&self) -> Result<()> {
        let mut state = self.state();
        let settled = tokio::time::timeout(FIRST, state.wait_for(|s| *s != State::Starting))
            .await
            .map_err(|_| anyhow::anyhow!("the ssh tunnel did not come up in {FIRST:?}"))?
            .map(|s| s.clone());
        match settled {
            Ok(State::Down(why)) => bail!("the ssh tunnel failed: {why}"),
            _ => Ok(()),
        }
    }

    /// Stop it and wait until ssh is killed.
    pub async fn close(mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Run ssh, mark the tunnel up once its local port answers, and when it exits say why
/// and run it again after [`RESTART`].
async fn keep(ssh: Ssh, local: u16, remote: u16, state: watch::Sender<State>) {
    loop {
        state.send_replace(State::Starting);
        let spawned = tokio::process::Command::new(&ssh.program)
            .args(ssh.forward(local, remote))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // Its own group, so a signal from the terminal reaches attach and not ssh.
            .process_group(0)
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => {
                state.send_replace(State::Down(format!("cannot run {}: {e}", ssh.program)));
                tokio::time::sleep(RESTART).await;
                continue;
            }
        };
        // Read as it comes, so a chatty ssh never blocks on a full pipe.
        let stderr = child.stderr.take();
        let said = tokio::spawn(async move {
            let mut said = None;
            if let Some(stderr) = stderr {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    said = last_line(&line).or(said);
                }
            }
            said
        });
        let probe = async {
            while tokio::net::TcpStream::connect(("127.0.0.1", local))
                .await
                .is_err()
            {
                tokio::time::sleep(PROBE).await;
            }
            state.send_replace(State::Up);
            std::future::pending::<()>().await
        };
        let status = tokio::select! {
            status = child.wait() => status,
            () = probe => unreachable!(),
        };
        // Bounded, since a process ssh started could hold the pipe open past it.
        let said = tokio::time::timeout(RESTART, said).await;
        let why = said
            .ok()
            .and_then(Result::ok)
            .flatten()
            .unwrap_or_else(|| match status {
                Ok(status) => format!("ssh exited ({status})"),
                Err(e) => format!("ssh: {e}"),
            });
        state.send_replace(State::Down(why));
        tokio::time::sleep(RESTART).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_is_told_to_forward_never_prompt_and_give_up_on_a_dead_link() {
        let ssh = Ssh::new(
            "ubuntu@1.2.3.4".into(),
            vec!["-i".into(), "~/key.pem".into()],
        );
        assert_eq!(
            ssh.forward(40001, 7878).join(" "),
            "-N -o ExitOnForwardFailure=yes -o ServerAliveInterval=5 -o ServerAliveCountMax=2 \
-o BatchMode=yes -o ConnectTimeout=10 -i ~/key.pem -L 40001:127.0.0.1:7878 ubuntu@1.2.3.4"
        );
        assert_eq!(
            ssh.cat(REMOTE_TOKEN),
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-i",
                "~/key.pem",
                "ubuntu@1.2.3.4",
                "cat -- ~/'.config/bhai/server-token'"
            ]
        );
        assert_eq!(remote_path("/a b/it's"), r"'/a b/it'\''s'");
    }

    #[test]
    fn a_free_port_can_be_bound() {
        let port = free_port().unwrap();
        assert_ne!(port, 0);
        std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    }

    /// A stand-in for ssh: a script that runs `body`.
    fn fake(dir: &std::path::Path, body: &str) -> Ssh {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("ssh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Ssh {
            program: path.display().to_string(),
            ..Ssh::new("host".into(), Vec::new())
        }
    }

    /// An ssh that exits is run again about a second later, and what it said last is
    /// why the tunnel is down.
    #[tokio::test]
    async fn an_exiting_ssh_is_restarted_and_its_error_kept() {
        let dir = crate::tools::temp_dir();
        let runs = dir.join("runs");
        let ssh = fake(
            &dir,
            &format!(
                "echo run >> '{}'\necho 'ssh: connect to host h port 22: Connection refused' >&2\nexit 255",
                runs.display()
            ),
        );
        let tunnel = Tunnel::open(ssh, free_port().unwrap(), 7878);
        let err = tunnel.first().await.unwrap_err().to_string();
        assert!(err.ends_with("port 22: Connection refused"), "{err}");
        tokio::time::sleep(RESTART * 2 + Duration::from_millis(500)).await;
        let count = std::fs::read_to_string(&runs).unwrap().lines().count();
        assert!((3..=4).contains(&count), "{count} runs");
        drop(tunnel);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The tunnel is up once its local port answers, and dropping it kills ssh.
    #[tokio::test]
    async fn a_tunnel_is_up_when_its_port_answers_and_killed_when_dropped() {
        let dir = crate::tools::temp_dir();
        let pid = dir.join("pid");
        let ssh = fake(
            &dir,
            &format!("echo $$ > '{}'\nexec sleep 30", pid.display()),
        );
        // Listening in ssh's place, as its -L would.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let local = listener.local_addr().unwrap().port();
        let tunnel = Tunnel::open(ssh, local, 7878);
        tunnel.first().await.unwrap();
        assert_eq!(*tunnel.state().borrow(), State::Up);
        // Up can come before the script has run, since the port was already listening.
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match std::fs::read_to_string(&pid) {
                    Ok(pid) if pid.ends_with('\n') => return pid.trim().to_string(),
                    _ => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .unwrap();
        let alive = || {
            std::process::Command::new("kill")
                .args(["-0", &pid])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        assert!(alive());
        drop(tunnel);
        tokio::time::timeout(Duration::from_secs(5), async {
            while alive() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("ssh outlives its tunnel");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn the_token_is_read_over_ssh_and_its_failure_said() {
        let dir = crate::tools::temp_dir();
        let ssh = fake(&dir, "echo \" secret-token-value \"");
        assert_eq!(ssh.token(REMOTE_TOKEN).await.unwrap(), "secret-token-value");
        let ssh = fake(&dir, "echo 'cat: x: No such file or directory' >&2\nexit 1");
        let err = ssh.token("x").await.unwrap_err().to_string();
        assert_eq!(
            err,
            "cannot read the token from host:x over ssh: cat: x: No such file or directory"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
