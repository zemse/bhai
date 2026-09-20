//! The prompt editor: multi-line text on top of tui-input, and the prompt history.

use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::Result;
use ratatui::text::Line;
use serde::{Deserialize, Serialize};
use tui_input::{Input, InputRequest};

/// Prompts kept from the history file.
const MAX_HISTORY: usize = 1000;

/// Multi-line prompt text. tui-input edits the whole value as one string with `\n`
/// in it; the requests that mean "this line" are narrowed to the cursor's line here.
#[derive(Debug, Default)]
pub struct Editor {
    input: Input,
    /// The first line shown, kept between frames so the view only moves when it must.
    top: usize,
    /// Where a selection started, as a char index; the cursor is its other end.
    anchor: Option<usize>,
}

/// One row of the wrapped text: the char index it starts at, and what it shows.
#[derive(Debug, PartialEq)]
pub struct Row {
    pub start: usize,
    pub text: String,
}

impl Row {
    fn new(chars: &[char], start: usize, end: usize) -> Self {
        let text = chars[start..end].iter().collect();
        Row { start, text }
    }
}

/// A char's width in terminal cells.
fn char_width(c: char) -> usize {
    Line::raw(c.to_string()).width()
}

impl Editor {
    pub fn value(&self) -> &str {
        self.input.value()
    }

    /// The cursor as a char index into the value.
    pub fn cursor(&self) -> usize {
        self.input.cursor()
    }

    /// The text before the cursor, which is what a completion is offered on.
    pub fn before(&self) -> &str {
        let value = self.value();
        let end = value
            .char_indices()
            .nth(self.cursor())
            .map_or(value.len(), |(at, _)| at);
        &value[..end]
    }

    /// Replace the chars in `range` with `text`, the cursor landing after it.
    pub fn splice(&mut self, range: Range<usize>, text: &str) {
        let cursor = range.start + text.chars().count();
        self.replace(range, text, cursor);
    }

    pub fn is_empty(&self) -> bool {
        self.value().is_empty()
    }

    pub fn is_multiline(&self) -> bool {
        self.value().contains('\n')
    }

    /// Replace the text, with the cursor at its end.
    pub fn set(&mut self, text: String) {
        self.input = Input::new(text);
        self.top = 0;
        self.anchor = None;
    }

    /// Take the text, leaving the editor empty.
    pub fn take(&mut self) -> String {
        self.top = 0;
        self.anchor = None;
        self.input.value_and_reset()
    }

    /// The selected chars, when the anchor is set somewhere other than the cursor.
    pub fn selection(&self) -> Option<Range<usize>> {
        let anchor = self.anchor?;
        let cursor = self.cursor();
        (anchor != cursor).then(|| anchor.min(cursor)..anchor.max(cursor))
    }

    /// The selected text.
    pub fn selected(&self) -> Option<String> {
        let range = self.selection()?;
        Some(
            self.value()
                .chars()
                .skip(range.start)
                .take(range.len())
                .collect(),
        )
    }

    /// Anchor a selection at the cursor, or drop it, before a cursor move.
    pub fn selecting(&mut self, on: bool) {
        match on {
            true => {
                self.anchor.get_or_insert(self.cursor());
            }
            false => self.anchor = None,
        }
    }

    /// Select the whole text, with the cursor at its end.
    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.move_to(self.value().chars().count());
    }

    /// Apply `request`; while `selecting`, a cursor move extends the selection rather
    /// than dropping it. Typing or deleting over a selection replaces it.
    pub fn handle(&mut self, request: InputRequest, selecting: bool) {
        if moves(request) {
            self.selecting(selecting);
        } else if let Some(range) = self.take_selection() {
            self.replace(range.start..range.end, "", range.start);
            if !matches!(request, InputRequest::InsertChar(_)) {
                return;
            }
        }
        let (start, end) = self.line_bounds();
        match request {
            InputRequest::GoToStart => self.move_to(start),
            InputRequest::GoToEnd => self.move_to(end),
            InputRequest::DeleteLine if self.is_multiline() => self.replace(start..end, "", start),
            InputRequest::DeleteTillEnd if self.cursor() == end && self.is_multiline() => {
                let cursor = self.cursor();
                self.replace(cursor..cursor + 1, "", cursor)
            }
            InputRequest::DeleteTillEnd => {
                let cursor = self.cursor();
                self.replace(cursor..end, "", cursor)
            }
            request => {
                self.input.handle(request);
            }
        }
    }

    /// Insert `text` at the cursor, over the selection when there is one. Pasted line
    /// endings arrive as `\r` or `\r\n`.
    pub fn insert(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let range = self.take_selection().unwrap_or_else(|| {
            let cursor = self.cursor();
            cursor..cursor
        });
        self.replace(range.clone(), &text, range.start + text.chars().count());
    }

    pub fn newline(&mut self) {
        self.insert("\n");
    }

    /// Move the cursor `delta` rows, keeping its column where the row allows.
    /// False when there is no row that way.
    pub fn move_line(&mut self, delta: isize, width: usize) -> bool {
        let (row, column) = self.cursor_position(width);
        let target = row as isize + delta;
        if target < 0 || target as usize >= self.rows(width).len() {
            return false;
        }
        self.place(target as usize, column, width);
        true
    }

    /// The text as it is drawn: each logical line wrapped to `width`, with the char
    /// index it starts at. A row break at a space leaves that space on the row before.
    pub fn rows(&self, width: usize) -> Vec<Row> {
        let width = width.max(1);
        let chars: Vec<char> = self.value().chars().collect();
        let mut rows = Vec::new();
        let mut start = 0;
        let mut column = 0;
        let mut space = None;
        for (i, &c) in chars.iter().enumerate() {
            if c == '\n' {
                rows.push(Row::new(&chars, start, i));
                (start, column, space) = (i + 1, 0, None);
                continue;
            }
            let cell = char_width(c);
            if column + cell > width && i > start {
                // Break after the last space on the row, or mid-word if it has none.
                let at = space.filter(|&at| at > start).unwrap_or(i);
                rows.push(Row::new(&chars, start, at));
                start = if chars[at] == ' ' { at + 1 } else { at };
                column = chars[start..i].iter().copied().map(char_width).sum();
                space = None;
            }
            if c == ' ' {
                space = Some(i);
            }
            column += cell;
        }
        rows.push(Row::new(&chars, start, chars.len()));
        rows
    }

    /// The cursor's row and display column once the text is wrapped to `width`.
    pub fn cursor_position(&self, width: usize) -> (usize, usize) {
        let cursor = self.cursor();
        let rows = self.rows(width);
        let row = rows.iter().rposition(|r| r.start <= cursor).unwrap_or(0);
        let column = self
            .value()
            .chars()
            .skip(rows[row].start)
            .take(cursor - rows[row].start);
        (row, column.map(char_width).sum())
    }

    /// Put the cursor on `row` at display column `column`, or the nearest place to it.
    pub fn place(&mut self, row: usize, column: usize, width: usize) {
        let rows = self.rows(width);
        let Some(row) = rows.get(row.min(rows.len().saturating_sub(1))) else {
            return;
        };
        let mut cursor = row.start;
        let mut at = 0;
        for c in row.text.chars() {
            if at >= column {
                break;
            }
            at += char_width(c);
            cursor += 1;
        }
        self.move_to(cursor.min(self.value().chars().count()));
    }

    /// The first of `height` visible rows, moved only as far as keeps the cursor in view.
    pub fn top(&mut self, height: usize, width: usize) -> usize {
        let row = self.cursor_position(width).0;
        let rows = self.rows(width).len();
        let top = self.top.min(row).max((row + 1).saturating_sub(height));
        self.top = top.min(rows.saturating_sub(height));
        self.top
    }

    /// Char indexes of the start and end of the cursor's line.
    fn line_bounds(&self) -> (usize, usize) {
        let cursor = self.cursor();
        let chars: Vec<char> = self.value().chars().collect();
        let start = chars[..cursor]
            .iter()
            .rposition(|&c| c == '\n')
            .map_or(0, |i| i + 1);
        let end = chars[cursor..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(chars.len(), |i| cursor + i);
        (start, end)
    }

    /// The selection, if any, dropping the anchor either way.
    fn take_selection(&mut self) -> Option<Range<usize>> {
        let range = self.selection();
        self.anchor = None;
        range
    }

    fn move_to(&mut self, cursor: usize) {
        self.input.handle(InputRequest::SetCursor(cursor));
    }

    /// Replace the chars in `range` with `text` and put the cursor at `cursor`.
    fn replace(&mut self, range: Range<usize>, text: &str, cursor: usize) {
        let value = self.value();
        let byte = |chars: usize| {
            value
                .char_indices()
                .nth(chars)
                .map_or(value.len(), |(i, _)| i)
        };
        let (start, end) = (byte(range.start), byte(range.end));
        let edited = format!("{}{text}{}", &value[..start], &value[end..]);
        self.input = Input::new(edited).with_cursor(cursor);
    }
}

/// Requests that only move the cursor, so they can extend a selection.
fn moves(request: InputRequest) -> bool {
    matches!(
        request,
        InputRequest::SetCursor(_)
            | InputRequest::GoToPrevChar
            | InputRequest::GoToNextChar
            | InputRequest::GoToPrevWord
            | InputRequest::GoToNextWord
            | InputRequest::GoToStart
            | InputRequest::GoToEnd
    )
}

#[derive(Serialize, Deserialize)]
struct Record {
    text: String,
}

/// Submitted prompts, oldest first, walked with `ctrl+p` and `ctrl+n`.
#[derive(Debug, Default)]
pub struct History {
    entries: Vec<String>,
    /// The `history.jsonl` new prompts are appended to.
    path: Option<PathBuf>,
    /// The entry being shown, while walking.
    index: Option<usize>,
    /// What was typed before the walk started.
    draft: String,
}

impl History {
    /// Load the last prompts from `path`, trimming the file when it holds too many.
    pub fn load(path: Option<PathBuf>) -> Self {
        let text = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_default();
        let mut entries: Vec<String> = text
            .lines()
            .filter_map(|line| serde_json::from_str::<Record>(line).ok())
            .map(|record| record.text)
            .collect();
        if entries.len() > MAX_HISTORY {
            entries.drain(..entries.len() - MAX_HISTORY);
            if let Some(path) = &path {
                let _ = rewrite(path, &entries);
            }
        }
        Self {
            entries,
            path,
            ..Self::default()
        }
    }

    /// Record a submitted prompt and end any walk.
    pub fn push(&mut self, text: &str) -> Result<()> {
        self.index = None;
        self.draft.clear();
        if self.entries.last().is_some_and(|last| last == text) {
            return Ok(());
        }
        self.entries.push(text.to_string());
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{}", line(text)?)?;
        Ok(())
    }

    /// The entry before the one shown; `current` is kept as the draft when a walk starts.
    pub fn prev(&mut self, current: &str) -> Option<String> {
        let index = match self.index {
            None if self.entries.is_empty() => return None,
            None => {
                self.draft = current.to_string();
                self.entries.len() - 1
            }
            Some(0) => return None,
            Some(index) => index - 1,
        };
        self.index = Some(index);
        Some(self.entries[index].clone())
    }

    /// The entry after the one shown, or the draft once past the newest.
    pub fn next(&mut self) -> Option<String> {
        let index = self.index?;
        if index + 1 < self.entries.len() {
            self.index = Some(index + 1);
            return Some(self.entries[index + 1].clone());
        }
        self.index = None;
        Some(std::mem::take(&mut self.draft))
    }
}

fn line(text: &str) -> Result<String> {
    Ok(serde_json::to_string(&Record {
        text: text.to_string(),
    })?)
}

fn rewrite(path: &Path, entries: &[String]) -> Result<()> {
    let mut out = String::new();
    for entry in entries {
        out.push_str(&line(entry)?);
        out.push('\n');
    }
    std::fs::write(path, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A width no test line reaches, so wrapping stays out of the way.
    const WIDE: usize = 200;

    fn editor(text: &str) -> Editor {
        let mut editor = Editor::default();
        editor.set(text.to_string());
        editor
    }

    #[test]
    fn newline_splits_the_line_at_the_cursor() {
        let mut editor = editor("ab");
        editor.handle(InputRequest::GoToPrevChar, false);
        editor.newline();
        assert_eq!(editor.value(), "a\nb");
        assert_eq!(editor.cursor_position(WIDE), (1, 0));
        editor.handle(InputRequest::InsertChar('x'), false);
        assert_eq!(editor.value(), "a\nxb");
    }

    #[test]
    fn paste_keeps_newlines_and_normalizes_carriage_returns() {
        let mut editor = editor("> ");
        editor.insert("one\r\ntwo\rthree\nfour");
        assert_eq!(editor.value(), "> one\ntwo\nthree\nfour");
        assert_eq!(editor.cursor_position(WIDE), (3, 4));
    }

    #[test]
    fn up_and_down_keep_the_column() {
        let mut editor = editor("héllo\nab\nworld");
        assert!(!editor.move_line(1, WIDE));
        assert!(editor.move_line(-1, WIDE));
        assert_eq!(editor.cursor_position(WIDE), (1, 2));
        assert!(editor.move_line(-1, WIDE));
        assert_eq!(editor.cursor_position(WIDE), (0, 2));
        assert_eq!(editor.cursor(), 2);
        assert!(!editor.move_line(-1, WIDE));
    }

    #[test]
    fn long_text_wraps_at_the_width() {
        let rows = |text: &str, width: usize| {
            editor(text)
                .rows(width)
                .iter()
                .map(|r| r.text.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(rows("the quick brown fox", 10), ["the quick", "brown fox"]);
        assert_eq!(editor("the quick brown fox").rows(10)[1].start, 10);
        // A word longer than the row is split where it runs out.
        assert_eq!(rows("abcdefgh", 3), ["abc", "def", "gh"]);
        // An empty line still draws a row, and so does a trailing newline.
        assert_eq!(editor("a\n\nb").rows(10).len(), 3);
        assert_eq!(editor("a\n").rows(10).len(), 2);
    }

    #[test]
    fn the_cursor_follows_the_wrapped_rows() {
        let mut editor = editor("the quick brown fox");
        assert_eq!(editor.cursor_position(10), (1, 9));
        // The space a row broke at belongs to the row before it.
        editor.handle(InputRequest::SetCursor(9), false);
        assert_eq!(editor.cursor_position(10), (0, 9));
        // Up moves between wrapped rows, not just between typed lines.
        editor.handle(InputRequest::GoToEnd, false);
        assert!(editor.move_line(-1, 10));
        assert_eq!(editor.cursor(), 9);
        editor.place(1, 5, 10);
        assert_eq!(editor.cursor(), 15);
    }

    #[test]
    fn shift_extends_the_selection_and_a_plain_move_drops_it() {
        let mut editor = editor("hello");
        editor.handle(InputRequest::GoToStart, false);
        assert_eq!(editor.selection(), None);
        editor.handle(InputRequest::GoToNextChar, true);
        editor.handle(InputRequest::GoToNextChar, true);
        assert_eq!(editor.selection(), Some(0..2));
        assert_eq!(editor.selected().as_deref(), Some("he"));
        // Back onto the anchor leaves nothing selected, but keeps the anchor.
        editor.handle(InputRequest::GoToPrevChar, true);
        editor.handle(InputRequest::GoToPrevChar, true);
        assert_eq!(editor.selection(), None);
        editor.handle(InputRequest::GoToEnd, true);
        assert_eq!(editor.selected().as_deref(), Some("hello"));
        editor.handle(InputRequest::GoToStart, false);
        assert_eq!(editor.selection(), None);
    }

    #[test]
    fn typing_and_pasting_replace_the_selection() {
        let mut editor = editor("one two");
        editor.select_all();
        assert_eq!(editor.selected().as_deref(), Some("one two"));
        editor.handle(InputRequest::InsertChar('x'), false);
        assert_eq!((editor.value(), editor.cursor()), ("x", 1));
        assert_eq!(editor.selection(), None);

        // Backspace over a selection deletes just the selection.
        editor.set("one two".to_string());
        editor.handle(InputRequest::SetCursor(4), false);
        editor.handle(InputRequest::GoToEnd, true);
        editor.handle(InputRequest::DeletePrevChar, false);
        assert_eq!((editor.value(), editor.cursor()), ("one ", 4));

        editor.set("one two".to_string());
        editor.select_all();
        editor.insert("three");
        assert_eq!((editor.value(), editor.cursor()), ("three", 5));
    }

    #[test]
    fn line_keys_act_on_the_cursor_line() {
        let mut editor = editor("one\ntwo");
        editor.handle(InputRequest::GoToStart, false);
        assert_eq!(editor.cursor(), 4);
        editor.handle(InputRequest::GoToPrevChar, false);
        editor.handle(InputRequest::DeleteTillEnd, false);
        assert_eq!(editor.value(), "onetwo");
        editor.set("one\ntwo".to_string());
        editor.handle(InputRequest::DeleteLine, false);
        assert_eq!((editor.value(), editor.cursor()), ("one\n", 4));
    }

    #[test]
    fn the_view_scrolls_only_to_keep_the_cursor_visible() {
        let mut editor = editor(
            &(0..12)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        assert_eq!(editor.top(8, WIDE), 4);
        editor.move_line(-3, WIDE);
        assert_eq!(editor.top(8, WIDE), 4);
        editor.move_line(-5, WIDE);
        assert_eq!(editor.top(8, WIDE), 3);
    }

    fn temp_path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bhai-input-{}", uuid::Uuid::new_v4()));
        dir.join("history.jsonl")
    }

    #[test]
    fn walking_past_the_newest_restores_the_draft() {
        let mut history = History::default();
        assert_eq!(history.prev("draft"), None);
        history.push("first").unwrap();
        history.push("second").unwrap();
        history.push("second").unwrap();
        assert_eq!(history.next(), None);
        assert_eq!(history.prev("draft").as_deref(), Some("second"));
        assert_eq!(history.prev("second").as_deref(), Some("first"));
        assert_eq!(history.prev("first"), None);
        assert_eq!(history.next().as_deref(), Some("second"));
        assert_eq!(history.next().as_deref(), Some("draft"));
        assert_eq!(history.next(), None);
    }

    #[test]
    fn history_persists_and_is_capped_on_load() {
        let path = temp_path();
        let mut history = History::load(Some(path.clone()));
        history.push("multi\nline").unwrap();
        assert_eq!(
            History::load(Some(path.clone())).prev("").as_deref(),
            Some("multi\nline")
        );

        let lines: Vec<String> = (0..MAX_HISTORY + 5)
            .map(|i| line(&i.to_string()).unwrap())
            .collect();
        std::fs::write(&path, lines.join("\n") + "\nnot json\n").unwrap();
        let mut history = History::load(Some(path.clone()));
        assert_eq!(history.entries.len(), MAX_HISTORY);
        assert_eq!(history.entries[0], "5");
        assert_eq!(history.prev("").as_deref(), Some("1004"));
        let kept = std::fs::read_to_string(&path).unwrap();
        assert_eq!(kept.lines().count(), MAX_HISTORY);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
