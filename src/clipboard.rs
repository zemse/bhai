//! The system clipboard: a platform command when one is installed, else OSC 52.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// Commands that take the text on stdin, best first.
#[cfg_attr(test, allow(dead_code))]
#[cfg(target_os = "macos")]
const WRITERS: &[&[&str]] = &[&["pbcopy"]];
#[cfg_attr(test, allow(dead_code))]
#[cfg(not(target_os = "macos"))]
const WRITERS: &[&[&str]] = &[
    &["wl-copy"],
    &["xclip", "-selection", "clipboard"],
    &["xsel", "--clipboard", "--input"],
];

/// Commands that print the clipboard, best first.
#[cfg(target_os = "macos")]
const READERS: &[&[&str]] = &[&["pbpaste"]];
#[cfg(not(target_os = "macos"))]
const READERS: &[&[&str]] = &[&["wl-paste"], &["xclip", "-o", "-selection", "clipboard"]];

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Put `text` on the clipboard, by a platform command or, when none runs, by the
/// OSC 52 escape, which is what reaches the clipboard over ssh.
#[cfg(not(test))]
pub fn copy(text: &str) -> Result<()> {
    if WRITERS.iter().any(|argv| write(argv, text).is_ok()) {
        return Ok(());
    }
    terminal(text)
}

/// Under test the machine's own clipboard is left alone: a drag copies as it ends, and
/// a test run must not walk over whatever the developer had on it.
#[cfg(test)]
pub fn copy(text: &str) -> Result<()> {
    LAST.with(|last| *last.borrow_mut() = Some(text.to_string()));
    Ok(())
}

#[cfg(test)]
thread_local! {
    static LAST: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// What the last `copy` on this thread took, for tests.
#[cfg(test)]
pub fn last_copied() -> Option<String> {
    LAST.with(|last| last.borrow().clone())
}

/// The clipboard's text, when a platform command can read it back.
pub fn paste() -> Option<String> {
    READERS.iter().find_map(|argv| {
        let out = Command::new(argv[0]).args(&argv[1..]).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    })
}

/// Feed `text` to one clipboard command's stdin.
#[cfg_attr(test, allow(dead_code))]
fn write(argv: &[&str], text: &str) -> Result<()> {
    let mut child = Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .context("no stdin")?
        .write_all(text.as_bytes())?;
    if !child.wait()?.success() {
        bail!("{} failed", argv[0]);
    }
    Ok(())
}

/// Write the escape to the terminal itself rather than through the TUI's buffer.
#[cfg_attr(test, allow(dead_code))]
fn terminal(text: &str) -> Result<()> {
    let sequence = sequence(text, std::env::var_os("TMUX").is_some());
    let mut tty = std::fs::OpenOptions::new().write(true).open("/dev/tty")?;
    tty.write_all(sequence.as_bytes())?;
    tty.flush()?;
    Ok(())
}

/// The OSC 52 copy escape, in tmux's passthrough form when asked for it.
fn sequence(text: &str, tmux: bool) -> String {
    let osc = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    match tmux {
        // tmux forwards a DCS block to the outer terminal, with every escape doubled.
        true => format!("\x1bPtmux;{}\x1b\\", osc.replace('\x1b', "\x1b\x1b")),
        false => osc,
    }
}

/// Standard base64 with padding, so OSC 52 needs no crate.
fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut block = [0u8; 3];
        block[..chunk.len()].copy_from_slice(chunk);
        let bits = (u32::from(block[0]) << 16) | (u32::from(block[1]) << 8) | u32::from(block[2]);
        for i in 0..4 {
            out.push(match i <= chunk.len() {
                true => ALPHABET[(bits >> (18 - 6 * i)) as usize & 63] as char,
                false => '=',
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_known_vectors() {
        // RFC 4648 section 10, plus the padding each remainder needs.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // The last two alphabet entries only show up on high bytes.
        assert_eq!(base64(&[0xff, 0xef, 0xbf]), "/++/");
    }

    #[test]
    fn osc52_carries_the_text_as_base64() {
        assert_eq!(sequence("foo", false), "\x1b]52;c;Zm9v\x07");
    }

    #[test]
    fn tmux_wraps_the_escape_in_a_passthrough_block() {
        assert_eq!(
            sequence("foo", true),
            "\x1bPtmux;\x1b\x1b]52;c;Zm9v\x07\x1b\\"
        );
    }
}
