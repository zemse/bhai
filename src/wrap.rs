//! Word wrap for display, and how to undo it. A selection is measured in the rows the
//! wrap made, so every row carries the break it came after and a copy puts back the rows
//! the wrap broke rather than the lines the text really has.

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
            // A single word longer than the line gets hard-split.
            while word.chars().count() > width {
                if !line.is_empty() {
                    out.push((std::mem::take(&mut line), join));
                    join = Join::Space;
                }
                let split = char_index(word, width);
                out.push((word[..split].to_string(), join));
                join = Join::Split;
                word = &word[split..];
            }
            let extra = if line.is_empty() { 0 } else { 1 };
            if line.chars().count() + extra + word.chars().count() > width {
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
    fn wrap_handles_multibyte_text() {
        assert_eq!(wrap("héllo wörld", 5), vec!["héllo", "wörld"]);
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
