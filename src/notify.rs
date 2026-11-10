//! A desktop notification when the session wants the user and the terminal is not in
//! front: an approval waiting, or a turn that ended. OSC 9 where the terminal is known
//! to show one, a bell everywhere else.
//!
//! `[notify] command` in the global config is run as well, whatever has focus and with
//! no terminal at all, which is how a session on a remote box reaches the user. What it
//! is about goes in its environment, never into the command line, so nothing the model
//! wrote is ever parsed by the shell.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::TimeZone as _;
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::session;

/// Longest message sent, in characters; a notification banner shows less than this.
const CLIP: usize = 180;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Escape {
    Osc9,
    Bell,
}

pub struct Notifier {
    kind: Escape,
    tmux: bool,
    /// Off after a write fails, so a closed terminal is not written to on every turn.
    enabled: bool,
    /// Starts true: a terminal without focus reporting never says it lost focus, and one
    /// that is never known to be away is never notified.
    focused: bool,
}

impl Notifier {
    pub fn from_env() -> Self {
        let var = |name| std::env::var(name).unwrap_or_default().to_ascii_lowercase();
        Self::new(
            kind(&var("TERM_PROGRAM"), &var("TERM"), &var("LC_TERMINAL")),
            std::env::var_os("TMUX").is_some(),
        )
    }

    fn new(kind: Escape, tmux: bool) -> Self {
        Self {
            kind,
            tmux,
            enabled: true,
            focused: true,
        }
    }

    pub fn focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Notify, when the terminal is away. Only the thread that draws may call this, as
    /// with `title::set`.
    pub fn notify(&mut self, message: &str) {
        let _ = self.send(&mut std::io::stdout(), message);
    }

    /// Returns whether anything was written.
    fn send(&mut self, out: &mut impl Write, message: &str) -> bool {
        if self.focused || !self.enabled {
            return false;
        }
        let sent = out
            .write_all(&bytes(self.kind, self.tmux, message))
            .and_then(|()| out.flush());
        self.enabled = sent.is_ok();
        self.enabled
    }
}

/// OSC 9 on the terminals that show it as a notification. `LC_TERMINAL` is iTerm2's,
/// and unlike `TERM_PROGRAM` it survives tmux and ssh.
fn kind(term_program: &str, term: &str, lc_terminal: &str) -> Escape {
    let known = ["ghostty", "iterm", "kitty", "warp", "wezterm"];
    match [term_program, term, lc_terminal]
        .iter()
        .any(|var| known.iter().any(|name| var.contains(name)))
    {
        true => Escape::Osc9,
        false => Escape::Bell,
    }
}

fn bytes(kind: Escape, tmux: bool, message: &str) -> Vec<u8> {
    if kind == Escape::Bell {
        return vec![b'\x07'];
    }
    // A control character would end the sequence early and print the rest.
    let message: String = message
        .chars()
        .filter(|c| !c.is_control())
        .take(CLIP)
        .collect();
    let osc = format!("\x1b]9;{message}\x07");
    match tmux {
        // tmux forwards a DCS block to the outer terminal, with every escape doubled.
        true => format!("\x1bPtmux;{}\x1b\\", osc.replace('\x1b', "\x1b\x1b")).into_bytes(),
        false => osc.into_bytes(),
    }
}

/// A run of the command still going after this long is killed.
const TIMEOUT: Duration = Duration::from_secs(30);
/// One run per kind in this long: parallel steps park approvals in a burst, and queued
/// prompts end turns back to back.
const DEBOUNCE: Duration = Duration::from_secs(10);

/// `[notify]` as the config reads it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Run with `$SHELL -c`, output discarded.
    pub command: String,
    /// The kinds it runs for; every kind when absent.
    pub events: Option<Vec<Kind>>,
}

/// What the command is run about, as `BHAI_NOTIFY_KIND` says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A call waits on an approval.
    Approval,
    /// A turn ended.
    Done,
    /// The Codex credentials could not be refreshed, so nothing runs until the user acts.
    Auth,
    /// The weekly quota is nearly spent.
    Quota,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Approval => "approval",
            Kind::Done => "done",
            Kind::Auth => "auth",
            Kind::Quota => "quota",
        }
    }
}

/// The command and what every run of it is told about the session.
struct Hook {
    settings: Settings,
    session: String,
    project: PathBuf,
    last: HashMap<Kind, Instant>,
}

impl Hook {
    /// The run for `kind` now, unless the config leaves it out or one ran too recently.
    fn prepare(&mut self, kind: Kind, body: &str, now: Instant) -> Option<Command> {
        if let Some(events) = &self.settings.events
            && !events.contains(&kind)
        {
            return None;
        }
        if self
            .last
            .get(&kind)
            .is_some_and(|at| now.duration_since(*at) < DEBOUNCE)
        {
            return None;
        }
        self.last.insert(kind, now);
        let shell = std::env::var_os("SHELL")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let title = format!("bhai · {}", crate::title::compose(&self.project, None));
        let mut command = Command::new(shell);
        command
            .arg("-c")
            .arg(&self.settings.command)
            .env("BHAI_NOTIFY_KIND", kind.as_str())
            .env("BHAI_NOTIFY_TITLE", title)
            .env("BHAI_NOTIFY_BODY", line(body))
            .env("BHAI_SESSION_ID", &self.session)
            .env("BHAI_PROJECT", &self.project)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Some(command)
    }
}

static HOOK: Mutex<Option<Hook>> = Mutex::new(None);

/// Run `settings` for session `session` in `project` from now on; `None` runs nothing.
pub fn set(settings: Option<Settings>, session: &str, project: &Path) {
    *HOOK.lock().unwrap_or_else(|e| e.into_inner()) = settings.map(|settings| Hook {
        settings,
        session: session.to_string(),
        project: project.to_path_buf(),
        last: HashMap::new(),
    });
}

/// Run the `[notify]` command about `kind`, when there is one and it wants that kind.
/// Returns at once: the command runs on a thread of its own.
pub fn fire(kind: Kind, body: &str, privacy: std::sync::Arc<crate::privacy::Privacy>) {
    let epoch = privacy.epoch();
    let command = HOOK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .and_then(|hook| hook.prepare(kind, body, Instant::now()));
    if let Some(command) = command {
        std::thread::spawn(move || {
            privacy.public_at(epoch, || {
                run(command, TIMEOUT)?;
                Ok(())
            })
        });
    }
}

/// Run `command` to its end or for `timeout`, whichever is first; `None` when killed.
fn run(mut command: Command, timeout: Duration) -> std::io::Result<Option<ExitStatus>> {
    let mut child = command.spawn()?;
    let began = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if began.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `body` on one line and clipped. An environment variable cannot hold a NUL.
fn line(body: &str) -> String {
    body.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(CLIP)
        .collect()
}

/// Run the command on what the session waits on the user for, from `events`.
pub fn watch(
    mut events: broadcast::Receiver<session::Event>,
    privacy: std::sync::Arc<crate::privacy::Privacy>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut seen = Seen::default();
        loop {
            match events.recv().await {
                Ok(event) => {
                    if privacy.active() {
                        continue;
                    }
                    let now = chrono::Utc::now().timestamp();
                    if let Some((kind, body)) = about(&event, &mut seen, now) {
                        fire(kind, &body, std::sync::Arc::clone(&privacy));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

/// What earlier events decide about later ones.
#[derive(Default)]
struct Seen {
    /// A login failed, so the end of that turn is not reported as done as well.
    failed: bool,
    /// The weekly window is past the checkpoint line. Usage only climbs within a window,
    /// so it is under the line again only once the window has reset.
    low: bool,
}

/// The kind and body of a run for `event`, if it is one the user is wanted for.
fn about(event: &session::Event, seen: &mut Seen, now: i64) -> Option<(Kind, String)> {
    match event {
        session::Event::Approval { tool, command, .. } => {
            Some((Kind::Approval, format!("{tool} needs approval: {command}")))
        }
        session::Event::TurnFailed(message) if message.starts_with(crate::client::AUTH_FAILED) => {
            seen.failed = true;
            Some((Kind::Auth, message.clone()))
        }
        session::Event::TurnEnd if std::mem::take(&mut seen.failed) => None,
        session::Event::TurnEnd => Some((Kind::Done, "done".to_string())),
        // A report without the weekly window says nothing about it.
        session::Event::RateLimits(limits)
            if limits
                .windows()
                .any(|w| w.window_minutes == Some(7 * 24 * 60)) =>
        {
            let window = crate::tools::status::checkpoint(limits, now);
            if std::mem::replace(&mut seen.low, window.is_some()) {
                return None;
            }
            let window = window?;
            let resets = chrono::Local
                .timestamp_opt(now, 0)
                .single()
                .and_then(|now| window.resets_at_clock(now))
                .map(|at| format!(", resets {at}"))
                .unwrap_or_default();
            let used = window.used_percent;
            Some((Kind::Quota, format!("weekly quota {used:.0}% used{resets}")))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(command: &str, events: Option<Vec<Kind>>) -> Hook {
        Hook {
            settings: Settings {
                command: command.to_string(),
                events,
            },
            session: "s1".to_string(),
            project: PathBuf::from("/work/proj"),
            last: HashMap::new(),
        }
    }

    #[test]
    fn the_command_reads_the_event_from_its_environment() {
        let out = std::env::temp_dir().join(format!("bhai-notify-{}", uuid::Uuid::new_v4()));
        let mut hook = hook(
            &format!(
                "printf '%s|%s|%s|%s|%s' \"$BHAI_NOTIFY_KIND\" \"$BHAI_NOTIFY_TITLE\" \
                 \"$BHAI_NOTIFY_BODY\" \"$BHAI_SESSION_ID\" \"$BHAI_PROJECT\" > '{}'",
                out.display()
            ),
            None,
        );
        // The body is the model's to write, so the shell must never read it as code.
        let body = "bash needs approval: $(echo injected) `echo too`\n; exit 3";
        let command = hook.prepare(Kind::Approval, body, Instant::now()).unwrap();
        let status = run(command, Duration::from_secs(10)).unwrap().unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "approval|bhai · proj|bash needs approval: $(echo injected) `echo too` ; exit 3|s1|/work/proj"
        );
        std::fs::remove_file(out).unwrap();
    }

    #[test]
    fn a_kind_runs_once_in_the_debounce_and_only_when_asked_for() {
        let mut hook = hook("true", Some(vec![Kind::Auth, Kind::Done]));
        let now = Instant::now();
        assert!(hook.prepare(Kind::Approval, "x", now).is_none());
        assert!(hook.prepare(Kind::Auth, "x", now).is_some());
        assert!(hook.prepare(Kind::Auth, "x", now + DEBOUNCE / 2).is_none());
        assert!(hook.prepare(Kind::Done, "x", now).is_some(), "per kind");
        assert!(hook.prepare(Kind::Auth, "x", now + DEBOUNCE).is_some());
    }

    #[test]
    fn a_command_that_hangs_is_killed() {
        let mut hook = hook("sleep 30", None);
        let command = hook.prepare(Kind::Done, "done", Instant::now()).unwrap();
        let began = Instant::now();
        assert_eq!(run(command, Duration::from_millis(200)).unwrap(), None);
        assert!(began.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn approvals_and_turn_ends_are_what_the_user_is_wanted_for() {
        let approval = session::Event::Approval {
            id: 1,
            tool: "bash".to_string(),
            command: "ls".to_string(),
            preview: None,
            offers: Default::default(),
        };
        let mut seen = Seen::default();
        assert_eq!(
            about(&approval, &mut seen, 0),
            Some((Kind::Approval, "bash needs approval: ls".to_string()))
        );
        assert_eq!(
            about(&session::Event::TurnEnd, &mut seen, 0),
            Some((Kind::Done, "done".to_string()))
        );
        assert_eq!(
            about(&session::Event::Info("x".to_string()), &mut seen, 0),
            None
        );
    }

    #[test]
    fn a_failed_login_is_reported_as_auth_and_not_as_done() {
        let mut seen = Seen::default();
        let message = format!("{}: unauthorized (401)", crate::client::AUTH_FAILED);
        assert_eq!(
            about(&session::Event::TurnFailed(message.clone()), &mut seen, 0),
            Some((Kind::Auth, message))
        );
        assert_eq!(about(&session::Event::TurnEnd, &mut seen, 0), None);
        assert_eq!(
            about(&session::Event::TurnFailed("500".to_string()), &mut seen, 0),
            None
        );
        assert_eq!(
            about(&session::Event::TurnEnd, &mut seen, 0),
            Some((Kind::Done, "done".to_string()))
        );
    }

    /// Once as the weekly window crosses the line, not on every report after, and again
    /// only after a reset has brought it back under.
    #[test]
    fn a_nearly_spent_week_is_reported_once_per_window() {
        use crate::limits::{RateLimits, Window};

        let week = |used: f64, resets_at: i64| {
            session::Event::RateLimits(RateLimits {
                secondary: Some(Window {
                    used_percent: used,
                    window_minutes: Some(7 * 24 * 60),
                    resets_at: Some(resets_at),
                }),
                ..RateLimits::default()
            })
        };
        let mut seen = Seen::default();
        let now = 1_000;
        assert_eq!(about(&week(97.0, 5_000), &mut seen, now), None);
        let (kind, body) = about(&week(98.0, 5_000), &mut seen, now).unwrap();
        assert_eq!(kind, Kind::Quota);
        assert!(body.starts_with("weekly quota 98% used, resets "), "{body}");
        assert_eq!(about(&week(99.0, 5_000), &mut seen, now), None);
        assert_eq!(about(&week(100.0, 5_001), &mut seen, now), None);
        // The next window starts low and crosses again.
        assert_eq!(about(&week(3.0, 90_000), &mut seen, 6_000), None);
        assert!(about(&week(98.5, 90_000), &mut seen, 6_000).is_some());
        // A short window is not the week.
        let mut seen = Seen::default();
        let five_hours = session::Event::RateLimits(RateLimits {
            primary: Some(Window {
                used_percent: 99.0,
                window_minutes: Some(300),
                resets_at: None,
            }),
            ..RateLimits::default()
        });
        assert_eq!(about(&five_hours, &mut seen, now), None);
        assert!(about(&week(99.0, 5_000), &mut seen, now).is_some());
        assert_eq!(about(&five_hours, &mut seen, now), None);
        assert_eq!(about(&week(99.0, 5_000), &mut seen, now), None);
    }

    fn sent(notifier: &mut Notifier, message: &str) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        notifier.send(&mut out, message).then_some(out)
    }

    #[test]
    fn only_a_terminal_that_lost_focus_is_notified() {
        let mut notifier = Notifier::new(Escape::Osc9, false);
        assert_eq!(sent(&mut notifier, "done"), None);
        notifier.focus(false);
        assert_eq!(
            sent(&mut notifier, "done"),
            Some(b"\x1b]9;done\x07".to_vec())
        );
        notifier.focus(true);
        assert_eq!(sent(&mut notifier, "done"), None);
    }

    #[test]
    fn the_message_is_one_escape_sequence() {
        assert_eq!(
            bytes(Escape::Osc9, false, "done\n\x1bboom"),
            b"\x1b]9;doneboom\x07"
        );
        let long = "a".repeat(CLIP + 10);
        assert_eq!(bytes(Escape::Osc9, false, &long).len(), CLIP + 5);
    }

    #[test]
    fn tmux_wraps_the_escape_in_a_passthrough_block() {
        assert_eq!(
            bytes(Escape::Osc9, true, "done"),
            b"\x1bPtmux;\x1b\x1b]9;done\x07\x1b\\"
        );
    }

    #[test]
    fn an_unknown_terminal_gets_a_bell() {
        assert_eq!(kind("apple_terminal", "xterm-256color", ""), Escape::Bell);
        assert_eq!(bytes(Escape::Bell, true, "done"), b"\x07");
        assert_eq!(kind("ghostty", "xterm-ghostty", ""), Escape::Osc9);
        assert_eq!(kind("", "xterm-kitty", ""), Escape::Osc9);
        // iTerm2 under tmux, where TERM_PROGRAM says tmux.
        assert_eq!(kind("tmux", "tmux-256color", "iterm2"), Escape::Osc9);
    }

    #[test]
    fn a_failed_write_turns_notifications_off() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut notifier = Notifier::new(Escape::Bell, false);
        notifier.focus(false);
        assert!(!notifier.send(&mut Closed, "done"));
        assert_eq!(sent(&mut notifier, "done"), None);
    }
}
