//! The `ctrl+r` search: type to filter the prompt history, pick one to edit and send.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

/// Matches shown at once; the rest scroll into view under the selection.
const MAX_ROWS: usize = 10;

/// What a key did to the overlay.
#[derive(Debug, PartialEq)]
pub enum Pick {
    Waiting,
    Closed,
    Picked(String),
}

#[derive(Debug)]
pub struct Search {
    /// The history, newest first, each prompt once.
    all: Vec<String>,
    query: String,
    /// Indices into `all` of what the query matches, newest first.
    found: Vec<usize>,
    selected: usize,
}

impl Search {
    /// Open over `entries`, which are oldest first as the history keeps them.
    pub fn new(entries: &[String]) -> Self {
        let mut all: Vec<String> = Vec::new();
        for entry in entries.iter().rev() {
            if !all.contains(entry) {
                all.push(entry.clone());
            }
        }
        let mut search = Self {
            all,
            query: String::new(),
            found: Vec::new(),
            selected: 0,
        };
        search.filter();
        search
    }

    /// Every word of the query, ignoring case, somewhere in the prompt.
    fn filter(&mut self) {
        let query = self.query.to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        self.found = (0..self.all.len())
            .filter(|&i| {
                let entry = self.all[i].to_lowercase();
                words.iter().all(|word| entry.contains(word))
            })
            .collect();
        self.selected = 0;
    }

    pub fn insert(&mut self, text: &str) {
        self.query.extend(text.chars().filter(|c| !c.is_control()));
        self.filter();
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Pick {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let last = self.found.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => return Pick::Closed,
            KeyCode::Enter | KeyCode::Tab => {
                return match self.found.get(self.selected) {
                    Some(&i) => Pick::Picked(self.all[i].clone()),
                    None => Pick::Waiting,
                };
            }
            // Older, as another ctrl+r is in a shell.
            KeyCode::Up => self.selected = (self.selected + 1).min(last),
            KeyCode::Char('r' | 'p') if ctrl => self.selected = (self.selected + 1).min(last),
            KeyCode::Down => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('n') if ctrl => self.selected = self.selected.saturating_sub(1),
            KeyCode::PageUp => self.selected = (self.selected + MAX_ROWS).min(last),
            KeyCode::PageDown => self.selected = self.selected.saturating_sub(MAX_ROWS),
            KeyCode::Backspace => {
                self.query.pop();
                self.filter();
            }
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                self.filter();
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.query.push(c);
                self.filter();
            }
            _ => {}
        }
        Pick::Waiting
    }

    /// The border, the query line and the rows.
    pub fn height(&self) -> u16 {
        self.found.len().clamp(1, MAX_ROWS) as u16 + 3
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let dim = Style::new().fg(Color::DarkGray);
        let title = format!(" history · {} of {} ", self.found.len(), self.all.len());
        let block = Block::bordered()
            .title(title)
            .title_bottom(
                Line::styled(" ↑↓ or ctrl+r pick · enter take · esc close ", dim).right_aligned(),
            )
            .border_style(Style::new().fg(Color::Cyan));
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        if inner.height == 0 {
            return;
        }

        let width = inner.width as usize;
        let mut lines = vec![Line::from(vec![
            Span::styled(" search: ", dim),
            Span::raw(self.query.clone()),
        ])];
        let listed = (inner.height as usize).saturating_sub(1);
        let top = self
            .selected
            .saturating_sub(listed.saturating_sub(1))
            .min(self.found.len().saturating_sub(listed.max(1)));
        lines.extend(
            self.found
                .iter()
                .enumerate()
                .skip(top)
                .take(listed)
                .map(|(row, &i)| line(&self.all[i], row == self.selected, width)),
        );
        if self.found.is_empty() {
            let empty = match self.all.is_empty() {
                true => " no prompts sent yet",
                false => " nothing matches",
            };
            lines.push(Line::styled(empty, dim));
        }
        frame.render_widget(Paragraph::new(lines), inner);
        let x = inner.x + 9 + self.query.chars().count() as u16;
        frame.set_cursor_position((x.min(inner.right().saturating_sub(1)), inner.y));
    }
}

/// A prompt on one row, its line breaks shown as `↵`.
fn line(entry: &str, selected: bool, width: usize) -> Line<'static> {
    let flat = entry.trim().replace('\n', " ↵ ");
    let mut text = format!(" {flat}");
    text.truncate(
        text.char_indices()
            .nth(width)
            .map_or(text.len(), |(at, _)| at),
    );
    match selected {
        true => Line::styled(
            format!("{text:<width$}"),
            Style::new().fg(Color::Black).bg(Color::Cyan),
        ),
        false => Line::raw(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(search: &mut Search, code: KeyCode) -> Pick {
        search.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn entries(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn newest_first_each_once_and_every_word_must_match() {
        let mut search = Search::new(&entries(&[
            "fix the parser",
            "run the Tests",
            "tests for the lexer",
            "fix the parser",
        ]));
        assert_eq!(
            search.all,
            ["fix the parser", "tests for the lexer", "run the Tests"]
        );
        search.insert("test");
        assert_eq!(search.found, [1, 2]);
        search.insert(" RUN");
        assert_eq!(search.found, [2]);
        assert_eq!(
            press(&mut search, KeyCode::Enter),
            Pick::Picked("run the Tests".to_string())
        );
    }

    #[test]
    fn keys_move_edit_and_close() {
        let mut search = Search::new(&entries(&["a1", "a2", "b"]));
        press(&mut search, KeyCode::Char('a'));
        assert_eq!(search.found.len(), 2);
        search.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        press(&mut search, KeyCode::Up);
        assert_eq!(search.selected, 1, "stops at the oldest match");
        press(&mut search, KeyCode::Down);
        assert_eq!(search.selected, 0);
        press(&mut search, KeyCode::Char('z'));
        assert_eq!(press(&mut search, KeyCode::Enter), Pick::Waiting);
        press(&mut search, KeyCode::Backspace);
        press(&mut search, KeyCode::Backspace);
        assert_eq!(search.found.len(), 3);
        assert_eq!(press(&mut search, KeyCode::Esc), Pick::Closed);
    }
}
