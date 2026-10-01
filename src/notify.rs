//! A desktop notification when the session wants the user and the terminal is not in
//! front: an approval waiting, or a turn that ended. OSC 9 where the terminal is known
//! to show one, a bell everywhere else.

use std::io::Write;

/// Longest message sent, in characters; a notification banner shows less than this.
const CLIP: usize = 180;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Osc9,
    Bell,
}

pub struct Notifier {
    kind: Kind,
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

    fn new(kind: Kind, tmux: bool) -> Self {
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
fn kind(term_program: &str, term: &str, lc_terminal: &str) -> Kind {
    let known = ["ghostty", "iterm", "kitty", "warp", "wezterm"];
    match [term_program, term, lc_terminal]
        .iter()
        .any(|var| known.iter().any(|name| var.contains(name)))
    {
        true => Kind::Osc9,
        false => Kind::Bell,
    }
}

fn bytes(kind: Kind, tmux: bool, message: &str) -> Vec<u8> {
    if kind == Kind::Bell {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(notifier: &mut Notifier, message: &str) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        notifier.send(&mut out, message).then_some(out)
    }

    #[test]
    fn only_a_terminal_that_lost_focus_is_notified() {
        let mut notifier = Notifier::new(Kind::Osc9, false);
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
            bytes(Kind::Osc9, false, "done\n\x1bboom"),
            b"\x1b]9;doneboom\x07"
        );
        let long = "a".repeat(CLIP + 10);
        assert_eq!(bytes(Kind::Osc9, false, &long).len(), CLIP + 5);
    }

    #[test]
    fn tmux_wraps_the_escape_in_a_passthrough_block() {
        assert_eq!(
            bytes(Kind::Osc9, true, "done"),
            b"\x1bPtmux;\x1b\x1b]9;done\x07\x1b\\"
        );
    }

    #[test]
    fn an_unknown_terminal_gets_a_bell() {
        assert_eq!(kind("apple_terminal", "xterm-256color", ""), Kind::Bell);
        assert_eq!(bytes(Kind::Bell, true, "done"), b"\x07");
        assert_eq!(kind("ghostty", "xterm-ghostty", ""), Kind::Osc9);
        assert_eq!(kind("", "xterm-kitty", ""), Kind::Osc9);
        // iTerm2 under tmux, where TERM_PROGRAM says tmux.
        assert_eq!(kind("tmux", "tmux-256color", "iterm2"), Kind::Osc9);
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
        let mut notifier = Notifier::new(Kind::Bell, false);
        notifier.focus(false);
        assert!(!notifier.send(&mut Closed, "done"));
        assert_eq!(sent(&mut notifier, "done"), None);
    }
}
