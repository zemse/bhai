//! Secret values bhai knows, blanked out of tool output before it enters history. The
//! values are the withheld environment (see `childenv`), the Codex tokens and the MCP
//! header values. Substring matching only: base64 gets past it.

use std::borrow::Cow;
use std::sync::RwLock;

const MARK: &str = "[REDACTED]";
/// Shorter values would blank ordinary words.
const MIN_LEN: usize = 8;
/// The shortest piece of a value blanked where output was cut.
const MIN_PIECE: usize = 4;

/// Longest first, so a value that contains another is replaced whole.
static KNOWN: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Remember `value`; a short one is ignored.
pub fn register(value: &str) {
    if value.len() < MIN_LEN {
        return;
    }
    let mut known = KNOWN.write().unwrap_or_else(|e| e.into_inner());
    if known.iter().any(|k| k == value) {
        return;
    }
    known.push(value.to_string());
    known.sort_by_key(|k| std::cmp::Reverse(k.len()));
}

/// Remember an HTTP header value, and the credential after a scheme such as `Bearer `.
pub fn register_header(value: &str) {
    register(value);
    if let Some((_, credential)) = value.rsplit_once(' ') {
        register(credential);
    }
}

/// Remember the values of the variables a child does not inherit.
pub fn register_withheld_env() {
    for name in crate::childenv::withheld() {
        if let Some(value) = std::env::var_os(name) {
            register(&value.to_string_lossy());
        }
    }
}

/// `text` with every known value replaced. Run it before truncating, so a value is
/// still whole.
pub fn apply(text: &str) -> Cow<'_, str> {
    let known = KNOWN.read().unwrap_or_else(|e| e.into_inner());
    let mut out = Cow::Borrowed(text);
    for value in known.iter() {
        if out.contains(value.as_str()) {
            out = Cow::Owned(out.replace(value.as_str(), MARK));
        }
    }
    out
}

/// Both ends of output whose middle was dropped, each passed through [`apply`], with
/// the piece of a value the gap cut in two blanked as well: an end of `head` that
/// starts a value, a start of `tail` that ends one.
pub fn apply_around_gap(head: &str, tail: &str) -> (String, String) {
    let (head, tail) = (apply(head), apply(tail));
    let known = KNOWN.read().unwrap_or_else(|e| e.into_inner());
    let longest = |fits: &dyn Fn(&str, usize) -> bool| {
        known
            .iter()
            .flat_map(|value| (MIN_PIECE..value.len()).rev().map(move |k| (value, k)))
            .filter(|&(value, k)| fits(value, k))
            .map(|(_, k)| k)
            .max()
    };
    let starts = longest(&|value, k| value.is_char_boundary(k) && head.ends_with(&value[..k]));
    let ends = longest(&|value, k| {
        let at = value.len() - k;
        value.is_char_boundary(at) && tail.starts_with(&value[at..])
    });
    let head = match starts {
        Some(k) => format!("{}{MARK}", &head[..head.len() - k]),
        None => head.into_owned(),
    };
    let tail = match ends {
        Some(k) => format!("{MARK}{}", &tail[k..]),
        None => tail.into_owned(),
    };
    (head, tail)
}

/// A pipe's UTF-8 output, with known values withheld until they can be ruled out.
/// Each call uses the current registry; already emitted text cannot be recalled.
#[derive(Default)]
pub struct Stream {
    pending: String,
    utf8: Vec<u8>,
    masked: usize,
}

impl Stream {
    /// Decode bytes lossily, retaining incomplete characters and possible secrets.
    pub fn push(&mut self, bytes: &[u8]) -> String {
        let known = KNOWN.read().unwrap_or_else(|e| e.into_inner());
        let mut out = String::new();
        for &byte in bytes {
            self.utf8.push(byte);
            loop {
                match std::str::from_utf8(&self.utf8) {
                    Ok(text) => {
                        self.pending.push_str(text);
                        self.utf8.clear();
                        self.flush(&known, false, &mut out);
                        break;
                    }
                    Err(error) => {
                        let valid = error.valid_up_to();
                        if valid != 0 {
                            self.pending
                                .push_str(&String::from_utf8_lossy(&self.utf8[..valid]));
                            self.utf8.drain(..valid);
                            self.flush(&known, false, &mut out);
                        }
                        let Some(invalid) = error.error_len() else {
                            break;
                        };
                        self.pending.push('\u{fffd}');
                        self.utf8.drain(..invalid);
                        self.flush(&known, false, &mut out);
                    }
                }
            }
        }
        // Registration may have changed even when no bytes arrived.
        self.flush(&known, false, &mut out);
        out
    }

    /// Flush the end of a pipe, including an incomplete UTF-8 character.
    pub fn finish(&mut self) -> String {
        let known = KNOWN.read().unwrap_or_else(|e| e.into_inner());
        self.pending.push_str(&String::from_utf8_lossy(&self.utf8));
        self.utf8.clear();
        let mut out = String::new();
        self.flush(&known, true, &mut out);
        out
    }

    fn flush(&mut self, known: &[String], finish: bool, out: &mut String) {
        while let Some(ch) = self.pending.chars().next() {
            if !finish
                && known.iter().any(|value| {
                    value.len() > self.pending.len() && value.starts_with(&self.pending)
                })
            {
                break;
            }
            if let Some(value) = known
                .iter()
                .find(|value| self.pending.starts_with(value.as_str()))
            {
                if self.masked == 0 {
                    out.push_str(MARK);
                }
                self.masked = self.masked.max(value.len());
            }
            if self.masked == 0 {
                out.push(ch);
            }
            // Keep scanning masked text so overlapping values are blanked as well.
            self.masked = self.masked.saturating_sub(ch.len_utf8());
            self.pending.drain(..ch.len_utf8());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_withholds_bytewise_secrets() {
        let secret = "stream-bytewise-credential";
        register(secret);
        let mut stream = Stream::default();
        for &byte in &secret.as_bytes()[..secret.len() - 1] {
            assert_eq!(stream.push(&[byte]), "");
        }
        assert_eq!(stream.push(&secret.as_bytes()[secret.len() - 1..]), MARK);
        assert_eq!(stream.finish(), "");
        assert_eq!(stream.finish(), "");
    }

    #[test]
    fn stream_blanks_longer_and_overlapping_values() {
        register("overlap-abcdefgh");
        register("overlap-abcdefgh-extended");
        register("abcdefgh-extended-tail");
        let mut stream = Stream::default();
        let mut out = stream.push(b"before overlap-abcdefgh");
        assert_eq!(out, "before ");
        out.push_str(&stream.push(b"-extended-tail after"));
        out.push_str(&stream.finish());
        assert_eq!(out, "before [REDACTED] after");

        let mut stream = Stream::default();
        assert_eq!(stream.push(b"overlap-abcdefgh"), "");
        assert_eq!(stream.finish(), MARK);
    }

    #[test]
    fn stream_handles_all_unicode_and_ascii_split_boundaries() {
        register("unicode-🔐-秘密-value");
        register("ascii-stream-credential");
        let bytes = "é before unicode-🔐-秘密-value / ascii-stream-credential after 🦀".as_bytes();
        for split in 0..=bytes.len() {
            let mut stream = Stream::default();
            let mut out = stream.push(&bytes[..split]);
            out.push_str(&stream.push(&bytes[split..]));
            out.push_str(&stream.finish());
            assert_eq!(out, "é before [REDACTED] / [REDACTED] after 🦀");
        }
        let mut stream = Stream::default();
        let mut out = String::new();
        for byte in bytes {
            out.push_str(&stream.push(&[*byte]));
        }
        out.push_str(&stream.finish());
        assert_eq!(out, "é before [REDACTED] / [REDACTED] after 🦀");
    }

    #[test]
    fn stream_reconstructs_nonsecrets_and_lossy_utf8() {
        let bytes = b"plain\n\r\0\xf0\x9f\xa6\x80\xe2\x82!\xff\xed\xa0\x80\xf0\x9f";
        for size in 1..=bytes.len() {
            let mut stream = Stream::default();
            let mut out = String::new();
            for chunk in bytes.chunks(size) {
                out.push_str(&stream.push(chunk));
            }
            out.push_str(&stream.finish());
            assert_eq!(out, String::from_utf8_lossy(bytes));
        }
    }

    #[test]
    fn stream_uses_current_registry_and_redacts_lossy_values() {
        register("registry-pending-seed");
        let mut stream = Stream::default();
        assert_eq!(stream.push(b"registry-pending-"), "");
        register("registry-pending-new");
        assert_eq!(stream.push(b"new"), MARK);
        assert_eq!(stream.finish(), "");

        register("lossy-value-�");
        let mut stream = Stream::default();
        assert_eq!(stream.push(b"lossy-value-\xe2"), "");
        assert_eq!(stream.finish(), MARK);
    }

    #[test]
    fn stream_releases_disproved_and_unfinished_prefixes() {
        register("prefix-release-credential");
        let mut stream = Stream::default();
        assert_eq!(stream.push(b"prefix-release-"), "");
        assert_eq!(stream.push(b"no match"), "prefix-release-no match");
        assert_eq!(stream.push(b"prefix-release-"), "");
        assert_eq!(stream.finish(), "prefix-release-");
    }

    #[test]
    fn stream_pending_is_bounded_and_pipes_are_independent() {
        register("bounded-stream-secret");
        let mut stdout = Stream::default();
        let mut stderr = Stream::default();
        assert_eq!(stdout.push(b"bounded-stream-"), "");
        assert_eq!(stderr.push(b"secret"), "secret");
        assert_eq!(stdout.push(b"secret"), MARK);
        assert_eq!(stdout.finish(), "");
        assert_eq!(stderr.finish(), "");

        let bytes = "~".repeat(1024 * 1024);
        assert_eq!(stdout.push(bytes.as_bytes()), bytes);
        assert!(stdout.pending.is_empty());
        assert!(stdout.utf8.is_empty());
        for _ in 0..1000 {
            let _ = stdout.push(b"bounded-stream-secre");
            assert!(stdout.pending.len() <= "bounded-stream-secret".len());
            assert!(stdout.utf8.len() <= 3);
        }
        assert!(stdout.pending.capacity() < 1024);
    }

    #[test]
    fn known_values_are_blanked_longest_first() {
        register("redact-test-secret");
        register("redact-test-secret-extended");
        let out = apply("a redact-test-secret-extended b redact-test-secret c");
        assert_eq!(out, "a [REDACTED] b [REDACTED] c");
    }

    #[test]
    fn short_values_and_other_text_are_left_alone() {
        register("short");
        assert!(matches!(apply("a short word"), Cow::Borrowed(_)));
    }

    #[test]
    fn a_header_registers_the_credential_after_its_scheme() {
        register_header("Bearer redact-test-bearer-1234");
        assert_eq!(apply("token redact-test-bearer-1234"), "token [REDACTED]");
        assert_eq!(apply("Bearer redact-test-bearer-1234"), "[REDACTED]");
    }

    #[test]
    fn a_value_cut_by_the_gap_is_blanked_on_both_sides() {
        register("redact-test-gap-7a3f91");
        let (head, tail) = apply_around_gap("out redact-test-g", "ap-7a3f91 more");
        assert_eq!(
            (head.as_str(), tail.as_str()),
            ("out [REDACTED]", "[REDACTED] more")
        );
        // A piece shorter than MIN_PIECE is left, and so is text no value starts.
        let (head, tail) = apply_around_gap("out red", "91 more");
        assert_eq!((head.as_str(), tail.as_str()), ("out red", "91 more"));
    }
}
