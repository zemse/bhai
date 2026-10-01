//! The `ctrl+r` search: type to filter the prompt history, pick one to edit and send.
//! The same list, over saved sessions, is `/sessions` (a mention of one) and `--pick`
//! (one to resume).

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

/// What the rows are and what a pick is for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// The prompt history; a pick replaces the draft.
    History,
    /// Saved sessions; a pick is the session file's path, put into the draft.
    Mention,
    /// Saved sessions; a pick is the id to resume.
    Resume,
}

#[derive(Debug)]
pub struct Search {
    pub kind: Kind,
    /// The rows, newest first, each prompt once.
    all: Vec<String>,
    /// What picking each row gives, where it is not the row itself.
    values: Vec<String>,
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
        Self::open(Kind::History, all, Vec::new())
    }

    /// Open over the sessions [`crate::sessions::list`] found, leaving out `current` and
    /// any that will not load. Each row is its age, its model and its first message.
    pub fn sessions(
        kind: Kind,
        list: &[crate::sessions::Summary],
        current: &str,
        now: std::time::SystemTime,
    ) -> Self {
        let (all, values) = list
            .iter()
            .filter(|s| s.id != current)
            .filter_map(|s| {
                let details = s.details.as_ref().ok()?;
                let age = now.duration_since(s.modified).unwrap_or_default();
                let row = format!(
                    "{:>3}  {}  {}",
                    ago(age),
                    details.model,
                    details.first.as_deref().unwrap_or("(no message)")
                );
                let value = match kind {
                    Kind::Mention => s.path.display().to_string(),
                    _ => s.id.clone(),
                };
                Some((row, value))
            })
            .unzip();
        Self::open(kind, all, values)
    }

    fn open(kind: Kind, all: Vec<String>, values: Vec<String>) -> Self {
        let mut search = Self {
            kind,
            all,
            values,
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

    pub fn is_empty(&self) -> bool {
        self.all.is_empty()
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
                    Some(&i) => Pick::Picked(self.values.get(i).unwrap_or(&self.all[i]).clone()),
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
        let (what, keys, none) = match self.kind {
            Kind::History => (
                "history",
                " ↑↓ or ctrl+r pick · enter take · esc close ",
                " no prompts sent yet",
            ),
            Kind::Mention => (
                "sessions",
                " ↑↓ pick · enter mention its file · esc close ",
                " no other saved sessions",
            ),
            Kind::Resume => (
                "sessions",
                " ↑↓ pick · enter resume · esc quit ",
                " no saved sessions",
            ),
        };
        let title = format!(" {what} · {} of {} ", self.found.len(), self.all.len());
        let block = Block::bordered()
            .title(title)
            .title_bottom(Line::styled(keys, dim).right_aligned())
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
                true => none,
                false => " nothing matches",
            };
            lines.push(Line::styled(empty, dim));
        }
        frame.render_widget(Paragraph::new(lines), inner);
        let x = inner.x + 9 + self.query.chars().count() as u16;
        frame.set_cursor_position((x.min(inner.right().saturating_sub(1)), inner.y));
    }
}

/// How long ago, in the largest unit that is at least one.
fn ago(age: std::time::Duration) -> String {
    match age.as_secs() {
        s if s < 60 => "now".to_string(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

/// Run `search` on the terminal on its own, before the session starts: the pick, or
/// `None` when it was closed.
pub fn run(mut search: Search) -> std::io::Result<Option<String>> {
    use ratatui::crossterm::event::{self, Event, KeyEventKind};
    let mut terminal = ratatui::init();
    let picked = loop {
        if let Err(e) = terminal.draw(|frame| {
            let area = frame.area();
            let height = search.height().min(area.height);
            search.render(frame, Rect { height, ..area });
        }) {
            break Err(e);
        }
        let key = match event::read() {
            Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => key,
            Ok(Event::Paste(text)) => {
                search.insert(&text);
                continue;
            }
            Ok(_) => continue,
            Err(e) => break Err(e),
        };
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            break Ok(None);
        }
        match search.on_key(key) {
            Pick::Waiting => {}
            Pick::Closed => break Ok(None),
            Pick::Picked(value) => break Ok(Some(value)),
        }
    };
    ratatui::restore();
    picked
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
    use std::path::{Path, PathBuf};

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
    fn sessions_show_age_model_and_first_message_and_pick_their_id_or_path() {
        use crate::sessions::{Details, Header, Summary};
        use std::time::{Duration, SystemTime};

        let now = SystemTime::now();
        let summary = |id: &str, ago: u64, first: Option<&str>| Summary {
            path: PathBuf::from(format!("/repo/.bhai/sessions/{id}.jsonl")),
            id: id.to_string(),
            modified: now - Duration::from_secs(ago),
            details: Ok(Details {
                header: Header::new(id, "default", "gpt-5", "low", Path::new("/repo")),
                first: first.map(str::to_string),
                items: 2,
                model: "gpt-5.5".to_string(),
            }),
        };
        let list = [
            summary("current", 0, Some("this one")),
            summary("aaa", 30, Some("fix the parser")),
            summary("bbb", 7200, None),
            Summary {
                details: Err("bad header".to_string()),
                ..summary("broken", 9000, None)
            },
            summary("ccc", 3 * 86400, Some("write the tests")),
        ];

        let mut search = Search::sessions(Kind::Resume, &list, "current", now);
        assert_eq!(
            search.all,
            [
                "now  gpt-5.5  fix the parser",
                " 2h  gpt-5.5  (no message)",
                " 3d  gpt-5.5  write the tests",
            ]
        );
        search.insert("tests");
        assert_eq!(
            press(&mut search, KeyCode::Enter),
            Pick::Picked("ccc".to_string())
        );

        let mut search = Search::sessions(Kind::Mention, &list, "current", now);
        assert_eq!(
            press(&mut search, KeyCode::Enter),
            Pick::Picked("/repo/.bhai/sessions/aaa.jsonl".to_string())
        );
        assert!(Search::sessions(Kind::Mention, &list[..1], "current", now).is_empty());
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
