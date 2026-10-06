//! The background list over the transcript: one row for each thing
//! [`crate::background`] finds running, and one of them open to inspect.

use std::time::{Duration, SystemTime};

use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::background::{Kind, Row};
use crate::ui::clip;

/// What a key or click on the list asks the app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    None,
    Close,
    /// Open this child's pane.
    Child(String),
    /// Stop this bash session.
    Kill(u32),
    /// Cancel this schedule.
    Cancel(String),
    /// Stop this observer, not its observed task.
    Monitor(String),
}

#[derive(Debug, Default)]
pub struct BgView {
    pub rows: Vec<Row>,
    pub selected: usize,
    /// The row being inspected, by kind and id, while one is.
    pub open: Option<(Kind, String)>,
    /// The kill or cancel was asked for once on the open row; the second ask does it.
    pub armed: bool,
    /// Offset in the monitor inspector's details.
    detail_scroll: u16,
    /// Each list row's area and index, filled in by the renderer.
    list: Vec<(Rect, usize)>,
    /// The way back to the list and the open row's action, filled in by the renderer.
    back: Option<Rect>,
    action: Option<Rect>,
    /// The whole overlay, filled in by the renderer.
    pub area: Option<Rect>,
}

impl BgView {
    pub fn new(rows: Vec<Row>) -> Self {
        Self {
            rows,
            ..Self::default()
        }
    }

    /// Take a newer snapshot, keeping the selection on the same row. A row that stopped
    /// running goes, and the inspector open on it closes with it.
    pub fn refresh(&mut self, rows: Vec<Row>) {
        let selected = self.rows.get(self.selected).map(|r| (r.kind, r.id.clone()));
        self.rows = rows;
        self.selected = selected
            .and_then(|(kind, id)| self.index(kind, &id))
            .unwrap_or(self.selected)
            .min(self.rows.len().saturating_sub(1));
        if let Some((kind, id)) = &self.open
            && self.index(*kind, id).is_none()
        {
            self.open = None;
            self.armed = false;
        }
    }

    fn index(&self, kind: Kind, id: &str) -> Option<usize> {
        self.rows.iter().position(|r| r.kind == kind && r.id == id)
    }

    /// The row being inspected.
    pub fn opened(&self) -> Option<&Row> {
        let (kind, id) = self.open.as_ref()?;
        self.index(*kind, id).map(|at| &self.rows[at])
    }

    pub fn on_key(&mut self, code: KeyCode) -> Act {
        if self.open.is_some() {
            let again = self.armed && code == KeyCode::Char('x');
            self.armed = false;
            return match code {
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('q') => {
                    self.open = None;
                    Act::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                    Act::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                    Act::None
                }
                KeyCode::Char('x') if again => self.act(),
                KeyCode::Char('x') => {
                    self.armed = self.opened().is_some_and(|row| action(row).is_some());
                    Act::None
                }
                _ => Act::None,
            };
        }
        match code {
            KeyCode::Esc | KeyCode::Char('q') => Act::Close,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                Act::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1));
                Act::None
            }
            KeyCode::Enter | KeyCode::Right => self.inspect(self.selected),
            _ => Act::None,
        }
    }

    /// A left click: a list row opens, the way back goes back, and the action arms on
    /// the first click and acts on the second.
    pub fn click(&mut self, x: u16, y: u16) -> Act {
        let at = Position::new(x, y);
        if self.open.is_some() {
            if self.back.is_some_and(|back| back.contains(at)) {
                self.open = None;
                self.armed = false;
                return Act::None;
            }
            if self.action.is_some_and(|action| action.contains(at)) {
                if self.armed {
                    self.armed = false;
                    return self.act();
                }
                self.armed = true;
                return Act::None;
            }
            self.armed = false;
            return Act::None;
        }
        match self.list.iter().find(|(area, _)| area.contains(at)) {
            Some(&(_, index)) => self.inspect(index),
            None => Act::None,
        }
    }

    /// Open row `index`: a child's own pane, anything else here.
    fn inspect(&mut self, index: usize) -> Act {
        let Some(row) = self.rows.get(index) else {
            return Act::None;
        };
        self.selected = index;
        if row.kind == Kind::Child {
            return Act::Child(row.id.clone());
        }
        self.open = Some((row.kind, row.id.clone()));
        self.detail_scroll = 0;
        self.armed = false;
        Act::None
    }

    /// What the open row's action asks for.
    fn act(&self) -> Act {
        match self.opened() {
            Some(row) if row.kind == Kind::Bash => row.id.parse().map_or(Act::None, Act::Kill),
            Some(row) if row.kind == Kind::Schedule => Act::Cancel(row.id.clone()),
            Some(row) if row.kind == Kind::Monitor => Act::Monitor(row.id.clone()),
            _ => Act::None,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.area = Some(area);
        self.list.clear();
        self.back = None;
        self.action = None;
        frame.render_widget(Clear, area);
        let dim = Style::new().fg(Color::DarkGray);
        let now = SystemTime::now();
        let opened = self.opened().cloned();
        let hint = match &opened {
            None => " ↑↓ select · enter open · esc close ".to_string(),
            Some(row) if row.kind == Kind::Monitor => {
                if self.armed {
                    " ↑↓ scroll · x again to stop · esc back ".into()
                } else {
                    " ↑↓ scroll · x stop · esc back ".into()
                }
            }
            Some(row) => match action(row) {
                Some(verb) if self.armed => format!(" x again to {verb} · esc back "),
                Some(verb) => format!(" x {verb} · esc back "),
                None => " esc back ".to_string(),
            },
        };
        let title = match &opened {
            None => format!(" background ({}) ", self.rows.len()),
            Some(row) => format!(" {} {} ", row.kind.name(), row.id),
        };
        let block = Block::bordered()
            .border_style(dim)
            .title(Line::styled(title, dim))
            .title_bottom(Line::styled(hint, dim).right_aligned());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        match opened {
            Some(row) => self.render_open(frame, inner, &row, now),
            None => self.render_list(frame, inner, now),
        }
    }

    fn render_list(&mut self, frame: &mut Frame, inner: Rect, now: SystemTime) {
        let dim = Style::new().fg(Color::DarkGray);
        if self.rows.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(" nothing runs in the background", dim)),
                inner,
            );
            return;
        }
        let rows = inner.height as usize;
        let width = inner.width as usize;
        // The selected row stays in view.
        let top = (self.selected + 1).saturating_sub(rows);
        let mut lines = Vec::new();
        for (at, row) in self.rows.iter().enumerate().skip(top).take(rows) {
            let head = format!(" {:<8} ", row.kind.name());
            let tail = format!(" {:>4}  {:<9} ", age(row.started, now), row.state);
            let room = width
                .saturating_sub(head.chars().count() + tail.chars().count())
                .max(1);
            let label = format!("{:<room$}", clip(&row.label, room));
            let mut line = Line::from(vec![
                Span::styled(head, dim),
                Span::raw(label),
                Span::styled(tail, state_style(&row.state)),
            ]);
            if at == self.selected {
                line = line.style(Style::new().bg(Color::DarkGray));
            }
            let y = inner.y + (at - top) as u16;
            self.list.push((Rect::new(inner.x, y, inner.width, 1), at));
            lines.push(line);
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_open(&mut self, frame: &mut Frame, inner: Rect, row: &Row, now: SystemTime) {
        let dim = Style::new().fg(Color::DarkGray);
        let width = inner.width as usize;
        // The first row is the way back, with the action at its right end.
        let back = " ‹ back";
        self.back = Some(Rect::new(
            inner.x,
            inner.y,
            (back.chars().count() as u16).min(inner.width),
            1,
        ));
        let mut top = vec![Span::styled(back, dim)];
        if let Some(verb) = action(row) {
            let button = match self.armed {
                true => format!("[x again to {verb}] "),
                false => format!("[x {verb}] "),
            };
            let style = match self.armed {
                true => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                false => Style::new().fg(Color::Red),
            };
            let len = button.chars().count();
            let pad = width.saturating_sub(back.chars().count() + len);
            let x = inner.x + (back.chars().count() + pad).min(width) as u16;
            self.action = Some(Rect::new(
                x,
                inner.y,
                (len as u16).min(inner.right().saturating_sub(x)),
                1,
            ));
            top.push(Span::raw(" ".repeat(pad)));
            top.push(Span::styled(button, style));
        }
        let mut lines = vec![Line::from(top)];
        let field = |name: &str, value: String, style: Style| {
            let head = format!(" {name:<8} ");
            let room = width.saturating_sub(head.chars().count()).max(1);
            Line::from(vec![
                Span::styled(head, dim),
                Span::styled(clip(&value, room), style),
            ])
        };
        let what = match row.kind {
            Kind::Bash => "command",
            Kind::Schedule => "prompt",
            Kind::Mcp => "server",
            Kind::Chrome => "page",
            Kind::Monitor => "monitor",
            Kind::Child | Kind::Proxy => "what",
        };
        lines.push(field(what, row.label.clone(), Style::new()));
        if let Some(pid) = row.pid {
            lines.push(field("pid", pid.to_string(), Style::new()));
        }
        if row.started.is_some() {
            lines.push(field("age", age(row.started, now), Style::new()));
        }
        lines.push(field("state", row.state.clone(), state_style(&row.state)));
        if row.kind == Kind::Bash {
            // The latest output, as many of its last rows as fit.
            lines.push(Line::styled(" output", dim));
            let left = (inner.height as usize).saturating_sub(lines.len());
            let text = row.detail.trim_end_matches('\n');
            let wrapped = crate::wrap::wrap(text, width.saturating_sub(2).max(1));
            let shown = wrapped.len().saturating_sub(left);
            lines.extend(
                wrapped[shown..]
                    .iter()
                    .map(|text| Line::raw(format!("  {text}"))),
            );
        } else if row.kind == Kind::Monitor {
            let room = width.saturating_sub(2).max(1);
            for text in crate::wrap::wrap(&row.detail, room) {
                lines.push(Line::raw(format!("  {text}")));
            }
        } else if !row.detail.is_empty() {
            let name = match row.kind {
                Kind::Schedule => "when",
                _ => "detail",
            };
            for (at, text) in row.detail.lines().enumerate() {
                let name = if at == 0 { name } else { "" };
                lines.push(field(name, text.to_string(), Style::new()));
            }
        }
        if row.kind == Kind::Monitor && inner.height > 1 {
            let top = lines.remove(0);
            frame.render_widget(
                Paragraph::new(top),
                Rect::new(inner.x, inner.y, inner.width, 1),
            );
            let rest = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);
            self.detail_scroll = self
                .detail_scroll
                .min(lines.len().saturating_sub(rest.height as usize) as u16);
            frame.render_widget(Paragraph::new(lines).scroll((self.detail_scroll, 0)), rest);
        } else {
            frame.render_widget(Paragraph::new(lines), inner);
        }
    }
}

/// What the action on a row does, for the rows that have one.
fn action(row: &Row) -> Option<&'static str> {
    match row.kind {
        Kind::Bash => Some("kill"),
        Kind::Schedule => Some("cancel"),
        Kind::Monitor => Some("stop"),
        _ => None,
    }
}

fn state_style(state: &str) -> Style {
    match state {
        "exited" => Style::new().fg(Color::DarkGray),
        "running" | "connected" | "rendering" => Style::new().fg(Color::Green),
        _ => Style::new().fg(Color::Yellow),
    }
}

/// How long ago `started` was, in its largest unit: `45s`, `12m`, `3h`, `2d`; `-` when
/// nothing recorded it.
pub fn age(started: Option<SystemTime>, now: SystemTime) -> String {
    let Some(started) = started else {
        return "-".to_string();
    };
    let secs = now
        .duration_since(started)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background::tests::row;

    fn rows() -> Vec<Row> {
        vec![
            row(Kind::Bash, "3", "running"),
            row(Kind::Child, "c1", "running"),
            row(Kind::Schedule, "s9", "scheduled"),
            row(Kind::Mcp, "fs", "connected"),
        ]
    }

    #[test]
    fn arrows_select_and_enter_opens_a_row_or_a_childs_pane() {
        let mut view = BgView::new(rows());
        assert_eq!(view.on_key(KeyCode::Up), Act::None);
        assert_eq!(view.selected, 0);
        view.on_key(KeyCode::Down);
        assert_eq!(view.on_key(KeyCode::Enter), Act::Child("c1".to_string()));
        assert!(
            view.open.is_none(),
            "a child opens its pane, not the inspector"
        );
        for _ in 0..9 {
            view.on_key(KeyCode::Down);
        }
        assert_eq!(view.selected, 3);
        view.on_key(KeyCode::Enter);
        assert_eq!(view.opened().map(|r| r.id.as_str()), Some("fs"));
        // Nothing to kill on a server, so x arms nothing.
        view.on_key(KeyCode::Char('x'));
        assert!(!view.armed);
        assert_eq!(view.on_key(KeyCode::Esc), Act::None);
        assert!(view.open.is_none());
        assert_eq!(view.on_key(KeyCode::Esc), Act::Close);
    }

    #[test]
    fn a_kill_or_cancel_takes_a_second_key_and_any_other_key_disarms_it() {
        let mut view = BgView::new(rows());
        view.on_key(KeyCode::Enter);
        assert_eq!(view.on_key(KeyCode::Char('x')), Act::None);
        assert!(view.armed);
        view.on_key(KeyCode::Char('j'));
        assert!(!view.armed);
        view.on_key(KeyCode::Char('x'));
        assert_eq!(view.on_key(KeyCode::Char('x')), Act::Kill(3));
        assert!(!view.armed);

        view.on_key(KeyCode::Esc);
        view.selected = 2;
        view.on_key(KeyCode::Enter);
        view.on_key(KeyCode::Char('x'));
        assert_eq!(
            view.on_key(KeyCode::Char('x')),
            Act::Cancel("s9".to_string())
        );
    }

    #[test]
    fn a_refresh_keeps_the_selection_and_closes_a_row_that_stopped() {
        let mut view = BgView::new(rows());
        view.selected = 2;
        view.on_key(KeyCode::Enter);
        view.on_key(KeyCode::Char('x'));
        let mut fewer = rows();
        fewer.remove(0);
        view.refresh(fewer.clone());
        assert_eq!(view.selected, 1);
        assert!(view.armed, "the open row still runs");
        fewer.remove(1);
        view.refresh(fewer);
        assert!(view.open.is_none());
        assert!(!view.armed);
        assert_eq!(view.selected, 1);
        view.refresh(Vec::new());
        assert_eq!(view.selected, 0);
        assert_eq!(view.on_key(KeyCode::Enter), Act::None);
    }

    #[test]
    fn ages_are_in_their_largest_unit() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let ago = |secs| age(Some(now - Duration::from_secs(secs)), now);
        assert_eq!(ago(45), "45s");
        assert_eq!(ago(125), "2m");
        assert_eq!(ago(3 * 3600 + 5), "3h");
        assert_eq!(ago(2 * 86400), "2d");
        assert_eq!(age(None, now), "-");
        assert_eq!(age(Some(now + Duration::from_secs(5)), now), "0s");
    }
}
