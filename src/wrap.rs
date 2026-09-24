//! Word wrap for display, and how to undo it. A selection is measured in the rows the
//! wrap made, so every row carries the break it came after and a copy puts back the rows
//! the wrap broke rather than the lines the text really has.

/// What a character that renders as nothing is shown as.
const MARK: char = '·';

/// Text as it can be drawn: a terminal acts on a control character rather than showing
/// it, and shows nothing at all for a tag or bidi character. Neither is written by anyone
/// here, since a transcript carries what a model said and what a command printed, and the
/// copy, the rows on screen and the selection all read this one string.
pub fn readable(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.chars().any(|c| dropped(c) || marked(c)) {
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(
        text.chars()
            .filter(|c| !dropped(*c))
            .map(|c| if marked(c) { MARK } else { c })
            .collect(),
    )
}

/// A control character the terminal would act on. `\n` is the line structure and `\t` is
/// width, so both stay.
fn dropped(c: char) -> bool {
    c.is_control() && c != '\n' && c != '\t'
}

/// A character that carries text while rendering as nothing: the tag block, which is where
/// text is hidden in a page or a tool result today, and the bidi overrides, which reorder
/// what is read without changing what is there. Emoji joiners and variation selectors are
/// left alone: they render as the character they compose.
fn marked(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{e0000}'..='\u{e007f}')
}

/// How a row joins the one above it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Join {
    /// A newline the text itself has.
    #[default]
    Newline,
    /// The space the wrap ate at the break.
    Space,
    /// Nothing: a word too long for the line was split mid-word.
    Split,
}

impl Join {
    /// `part` appended to what is copied so far, with the break undone: a row the wrap
    /// made loses the indent the renderer put in front of it.
    pub fn append(self, text: &mut String, part: &str) {
        match self {
            Join::Newline => {
                text.push('\n');
                text.push_str(part);
            }
            Join::Space => {
                text.push(' ');
                text.push_str(part.trim_start());
            }
            Join::Split => text.push_str(part.trim_start()),
        }
    }
}

/// Greedy word wrap that keeps existing newlines and never loses characters.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    joined(text, width)
        .into_iter()
        .map(|(row, _)| row)
        .collect()
}

/// The same rows, each with how it joins the one above.
pub fn joined(text: &str, width: usize) -> Vec<(String, Join)> {
    let width = width.max(1);
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        // The row being built starts the paragraph, so the text's own newline is its break.
        let mut join = Join::Newline;
        let mut line = String::new();
        for word in paragraph.split(' ') {
            let mut word = word;
            // Counted once: re-measuring the rest of the word each pass is quadratic.
            let mut left = word.chars().count();
            // A single word longer than the line gets hard-split.
            while left > width {
                if !line.is_empty() {
                    out.push((std::mem::take(&mut line), join));
                    join = Join::Space;
                }
                let split = char_index(word, width);
                out.push((word[..split].to_string(), join));
                join = Join::Split;
                word = &word[split..];
                left -= width;
            }
            let extra = if line.is_empty() { 0 } else { 1 };
            if line.chars().count() + extra + left > width {
                out.push((std::mem::take(&mut line), join));
                join = Join::Space;
            } else if extra == 1 {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push((line, join));
    }
    out
}

fn char_index(s: &str, chars: usize) -> usize {
    s.char_indices()
        .nth(chars)
        .map_or(s.len(), |(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joins(text: &str, width: usize) -> Vec<Join> {
        joined(text, width).into_iter().map(|(_, j)| j).collect()
    }

    #[test]
    fn wrap_keeps_every_character() {
        let text = "the quick brown fox jumps over the lazy dog";
        let wrapped = wrap(text, 10);
        assert!(wrapped.iter().all(|l| l.chars().count() <= 10));
        assert_eq!(wrapped.join(" "), text);
    }

    #[test]
    fn wrap_preserves_blank_lines() {
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
    }

    #[test]
    fn wrap_hard_splits_a_long_word() {
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn wrap_splits_a_long_word_into_whole_rows() {
        let word = "\u{e9}".repeat(1000);
        let wrapped = wrap(&word, 7);
        assert_eq!(wrapped.len(), 1000_usize.div_ceil(7));
        assert!(
            wrapped[..wrapped.len() - 1]
                .iter()
                .all(|row| row.chars().count() == 7)
        );
        assert_eq!(wrapped.concat(), word);
        // A word that divides evenly leaves no short row behind.
        assert_eq!(wrap(&"x".repeat(12), 4), vec!["xxxx", "xxxx", "xxxx"]);
    }

    #[test]
    fn wrap_handles_multibyte_text() {
        assert_eq!(wrap("héllo wörld", 5), vec!["héllo", "wörld"]);
    }

    #[test]
    fn what_a_terminal_would_act_on_or_hide_does_not_reach_a_row() {
        // Colour from a command's output: the terminal would act on it, the copy would
        // carry it, and the wrap would count it as width it does not have.
        // The escape byte and the bell go; what they bracketed is text like any other.
        assert_eq!(readable("\u{1b}[31mred\u{1b}[0m\u{7}"), "[31mred[0m");
        assert_eq!(readable("a\rb"), "ab");
        // Structure stays.
        assert_eq!(readable("one\ntwo\tthree"), "one\ntwo\tthree");
        // Text that renders as nothing is shown rather than dropped, so a payload hidden
        // in a tool result is visible in the transcript.
        assert_eq!(readable("hi\u{e0041}\u{e0042}"), "hi··");
        assert_eq!(readable("a\u{202e}b"), "a·b");
        // An emoji is composed of joiners and selectors that render as what they compose.
        let family = "👨\u{200d}👩\u{200d}👧";
        assert_eq!(readable(family), family);
        assert_eq!(readable("plain"), "plain");
    }

    #[test]
    fn a_row_says_what_break_it_came_after() {
        // The text's own newline, then the space the wrap ate.
        assert_eq!(
            joins("one\ntwo three", 5),
            [Join::Newline, Join::Newline, Join::Space]
        );
        // A word too long for the line is split mid-word, so nothing stands between.
        assert_eq!(
            joins("abcdefgh", 3),
            [Join::Newline, Join::Split, Join::Split]
        );
        // The break before an over-long word is still the space it stood on.
        assert_eq!(
            joins("hi abcdefgh", 3),
            [Join::Newline, Join::Space, Join::Split, Join::Split]
        );
    }
}
