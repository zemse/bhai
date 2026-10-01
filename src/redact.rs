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

#[cfg(test)]
mod tests {
    use super::*;

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
