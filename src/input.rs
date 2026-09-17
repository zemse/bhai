//! The prompt editor: multi-line text on top of tui-input, and the prompt history.

use std::io::Write;
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
}

impl Editor {
    pub fn value(&self) -> &str {
        self.input.value()
    }

    /// The cursor as a char index into the value.
    pub fn cursor(&self) -> usize {
        self.input.cursor()
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
    }

    /// Take the text, leaving the editor empty.
    pub fn take(&mut self) -> String {
        self.top = 0;
        self.input.value_and_reset()
    }

    pub fn handle(&mut self, request: InputRequest) {
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

    /// Insert `text` at the cursor. Pasted line endings arrive as `\r` or `\r\n`.
    pub fn insert(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let cursor = self.cursor();
        self.replace(cursor..cursor, &text, cursor + text.chars().count());
    }

    pub fn newline(&mut self) {
        self.insert("\n");
    }

    /// Move the cursor `delta` lines, keeping its column where the line allows.
    /// False when there is no line that way.
    pub fn move_line(&mut self, delta: isize) -> bool {
        let (row, column) = self.cursor_position();
        let target = row as isize + delta;
        if target < 0 || target as usize >= self.value().split('\n').count() {
            return false;
        }
        self.place(target as usize, column);
        true
    }

    /// The cursor's line and display column.
    pub fn cursor_position(&self) -> (usize, usize) {
        let before: String = self.value().chars().take(self.cursor()).collect();
        let line = before.rsplit('\n').next().unwrap_or("");
        (before.matches('\n').count(), Line::raw(line).width())
    }

    /// Put the cursor on `row` at display column `column`, or the nearest place to it.
    pub fn place(&mut self, row: usize, column: usize) {
        let mut cursor = 0;
        for line in self.value().split('\n').take(row) {
            cursor += line.chars().count() + 1;
        }
        let line = self.value().split('\n').nth(row).unwrap_or("");
        let mut width = 0;
        for c in line.chars() {
            if width >= column {
                break;
            }
            width += Line::raw(c.to_string()).width();
            cursor += 1;
        }
        self.move_to(cursor.min(self.value().chars().count()));
    }

    /// The first of `height` visible lines, moved only as far as keeps the cursor in view.
    pub fn top(&mut self, height: usize) -> usize {
        let row = self.cursor_position().0;
        let lines = self.value().split('\n').count();
        let top = self.top.min(row).max((row + 1).saturating_sub(height));
        self.top = top.min(lines.saturating_sub(height));
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

    fn move_to(&mut self, cursor: usize) {
        self.input.handle(InputRequest::SetCursor(cursor));
    }

    /// Replace the chars in `range` with `text` and put the cursor at `cursor`.
    fn replace(&mut self, range: std::ops::Range<usize>, text: &str, cursor: usize) {
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

    fn editor(text: &str) -> Editor {
        let mut editor = Editor::default();
        editor.set(text.to_string());
        editor
    }

    #[test]
    fn newline_splits_the_line_at_the_cursor() {
        let mut editor = editor("ab");
        editor.handle(InputRequest::GoToPrevChar);
        editor.newline();
        assert_eq!(editor.value(), "a\nb");
        assert_eq!(editor.cursor_position(), (1, 0));
        editor.handle(InputRequest::InsertChar('x'));
        assert_eq!(editor.value(), "a\nxb");
    }

    #[test]
    fn paste_keeps_newlines_and_normalizes_carriage_returns() {
        let mut editor = editor("> ");
        editor.insert("one\r\ntwo\rthree\nfour");
        assert_eq!(editor.value(), "> one\ntwo\nthree\nfour");
        assert_eq!(editor.cursor_position(), (3, 4));
    }

    #[test]
    fn up_and_down_keep_the_column() {
        let mut editor = editor("héllo\nab\nworld");
        assert!(!editor.move_line(1));
        assert!(editor.move_line(-1));
        assert_eq!(editor.cursor_position(), (1, 2));
        assert!(editor.move_line(-1));
        assert_eq!(editor.cursor_position(), (0, 2));
        assert_eq!(editor.cursor(), 2);
        assert!(!editor.move_line(-1));
    }

    #[test]
    fn line_keys_act_on_the_cursor_line() {
        let mut editor = editor("one\ntwo");
        editor.handle(InputRequest::GoToStart);
        assert_eq!(editor.cursor(), 4);
        editor.handle(InputRequest::GoToPrevChar);
        editor.handle(InputRequest::DeleteTillEnd);
        assert_eq!(editor.value(), "onetwo");
        editor.set("one\ntwo".to_string());
        editor.handle(InputRequest::DeleteLine);
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
        assert_eq!(editor.top(8), 4);
        editor.move_line(-3);
        assert_eq!(editor.top(8), 4);
        editor.move_line(-5);
        assert_eq!(editor.top(8), 3);
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
