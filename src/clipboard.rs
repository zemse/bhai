//! The system clipboard: a platform command when one is installed, else OSC 52. Over
//! ssh OSC 52 goes first, since a command there fills the remote machine's clipboard.

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

/// Commands that print the clipboard's image as PNG bytes, best first. macOS has none,
/// so AppleScript prints it as `«data PNGf<hex>»`.
#[cfg_attr(test, allow(dead_code))]
#[cfg(target_os = "macos")]
const IMAGE_READERS: &[&[&str]] = &[&["osascript", "-e", "the clipboard as «class PNGf»"]];
#[cfg_attr(test, allow(dead_code))]
#[cfg(not(target_os = "macos"))]
const IMAGE_READERS: &[&[&str]] = &[
    &["wl-paste", "--no-newline", "--type", "image/png"],
    &["xclip", "-o", "-selection", "clipboard", "-t", "image/png"],
];

/// The most text OSC 52 carries, in bytes before base64. Terminals drop an escape
/// longer than they allow whole, so the text is cut to fit instead.
const MAX_OSC52: usize = 100_000;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Put `text` on the clipboard, by a platform command or by the OSC 52 escape, which
/// is what reaches the local clipboard over ssh. Returns how many chars it took, fewer
/// than `text` has when OSC 52 had to cut it.
#[cfg(not(test))]
pub fn copy(text: &str) -> Result<usize> {
    let command = || WRITERS.iter().any(|argv| write(argv, text).is_ok());
    if over_ssh(|name| std::env::var_os(name)) {
        // With no tty to write the escape to, the remote clipboard is better than none.
        return terminal(text).or_else(|err| match command() {
            true => Ok(text.chars().count()),
            false => Err(err),
        });
    }
    match command() {
        true => Ok(text.chars().count()),
        false => terminal(text),
    }
}

/// Under test the machine's own clipboard is left alone: a drag copies as it ends, and
/// a test run must not walk over whatever the developer had on it. It cuts the text as
/// the OSC 52 path does, so a caller can be tested on what it says about a cut.
#[cfg(test)]
pub fn copy(text: &str) -> Result<usize> {
    let text = clip(text);
    LAST.with(|last| *last.borrow_mut() = Some(text.to_string()));
    Ok(text.chars().count())
}

/// What to tell the user once `copied` of the `of` chars asked for went on the clipboard.
pub fn said(copied: usize, of: usize) -> String {
    match copied < of {
        true => format!("copied {copied} of {of} chars (OSC 52 caps at 100 KB)"),
        false => format!("copied {copied} chars"),
    }
}

/// Whether bhai runs in an ssh session, by the variables sshd sets.
pub fn over_ssh(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    ["SSH_CONNECTION", "SSH_TTY"]
        .iter()
        .any(|name| var(name).is_some_and(|value| !value.is_empty()))
}

/// The longest prefix of `text` OSC 52 carries, cut on a char boundary.
fn clip(text: &str) -> &str {
    &text[..text.floor_char_boundary(MAX_OSC52)]
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

/// The clipboard's image as PNG bytes, when it holds one and a command can read it.
#[cfg(not(test))]
pub fn paste_image() -> Option<Vec<u8>> {
    IMAGE_READERS.iter().find_map(|argv| {
        let out = Command::new(argv[0])
            .args(&argv[1..])
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() || out.stdout.is_empty() {
            return None;
        }
        match argv[0] {
            "osascript" => applescript_data(&String::from_utf8_lossy(&out.stdout)),
            _ => Some(out.stdout),
        }
    })
}

/// Under test the clipboard holds what `set_image` put there, so nothing runs.
#[cfg(test)]
pub fn paste_image() -> Option<Vec<u8>> {
    IMAGE.with(|image| image.borrow().clone())
}

#[cfg(test)]
thread_local! {
    static IMAGE: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
}

/// Put an image on this thread's test clipboard.
#[cfg(test)]
pub fn set_image(bytes: Option<Vec<u8>>) {
    IMAGE.with(|image| *image.borrow_mut() = bytes);
}

/// The bytes in AppleScript's `«data PNGf89504E47...»`.
#[cfg_attr(test, allow(dead_code))]
fn applescript_data(out: &str) -> Option<Vec<u8>> {
    let hex = out
        .trim()
        .strip_prefix("«data ")?
        .strip_suffix('»')?
        .get(4..)?;
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
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
/// Returns how many chars of `text` it carried.
#[cfg_attr(test, allow(dead_code))]
fn terminal(text: &str) -> Result<usize> {
    let text = clip(text);
    let sequence = sequence(text, std::env::var_os("TMUX").is_some());
    let mut tty = std::fs::OpenOptions::new().write(true).open("/dev/tty")?;
    tty.write_all(sequence.as_bytes())?;
    tty.flush()?;
    Ok(text.chars().count())
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

/// Standard base64 with padding, so OSC 52 and images need no crate.
pub(crate) fn base64(bytes: &[u8]) -> String {
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
    fn applescript_prints_the_image_as_hex() {
        assert_eq!(
            applescript_data("«data PNGf89504E47»\n"),
            Some(vec![0x89, 0x50, 0x4e, 0x47])
        );
        assert_eq!(applescript_data("«data PNGf895»"), None);
        assert_eq!(applescript_data("«data PNGfzz»"), None);
        assert_eq!(applescript_data("some text"), None);
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

    #[test]
    fn ssh_is_told_by_either_variable_sshd_sets() {
        fn env(
            set: &'static [(&'static str, &'static str)],
        ) -> impl Fn(&str) -> Option<std::ffi::OsString> {
            move |name| {
                set.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.into())
            }
        }
        assert!(over_ssh(env(&[(
            "SSH_CONNECTION",
            "10.0.0.2 51234 10.0.0.1 22"
        )])));
        assert!(over_ssh(env(&[("SSH_TTY", "/dev/pts/3")])));
        assert!(!over_ssh(env(&[])));
        // Set but empty is not a session.
        assert!(!over_ssh(env(&[("SSH_CONNECTION", ""), ("SSH_TTY", "")])));
    }

    #[test]
    fn osc52_text_is_cut_to_the_cap_on_a_char_boundary() {
        assert_eq!(clip("short"), "short");
        let exact = "a".repeat(MAX_OSC52);
        assert_eq!(clip(&exact), exact);
        // A two-byte char straddling the cap is left out rather than split.
        let straddle = format!("{}é", "a".repeat(MAX_OSC52 - 1));
        assert_eq!(clip(&straddle), "a".repeat(MAX_OSC52 - 1));
    }

    #[test]
    fn a_cut_copy_says_how_much_of_the_text_it_took() {
        assert_eq!(said(14, 14), "copied 14 chars");
        assert_eq!(
            said(100_000, 100_001),
            "copied 100000 of 100001 chars (OSC 52 caps at 100 KB)"
        );
    }
}
