//! Rendering. Text is hard-wrapped here so the scroll offset can be computed exactly.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::app::{App, Entry};
use crate::permissions::Mode;

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

pub fn render(frame: &mut Frame, app: &mut App) {
    let approval_height = app
        .pending
        .as_ref()
        .map(|pending| {
            let width = frame.area().width.saturating_sub(4).max(10) as usize;
            let lines = wrap(&pending.command, width).len() as u16;
            (lines + 4).min(frame.area().height / 2).max(5)
        })
        .unwrap_or(3);

    let [status_area, transcript_area, bottom_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(approval_height),
    ])
    .areas(frame.area());

    render_status(frame, status_area, app);
    render_transcript(frame, transcript_area, app);
    if app.pending.is_some() {
        render_approval(frame, bottom_area, app);
    } else {
        render_input(frame, bottom_area, app);
    }
}

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    let dim = Style::new().fg(Color::DarkGray);
    let mut spans = vec![
        Span::styled(" bhai ", Style::new().fg(Color::Black).bg(Color::Cyan)),
        Span::styled(format!(" {} ", app.model), dim),
        Span::styled(
            format!("{} ", app.mode),
            match app.mode {
                Mode::Ask => dim,
                Mode::Auto => Style::new().fg(Color::Yellow),
                Mode::Bypass => Style::new().fg(Color::Red),
            },
        ),
    ];
    if app.working {
        spans.push(Span::styled(
            format!("{} working ", SPINNER[app.spinner % SPINNER.len()]),
            Style::new().fg(Color::Yellow),
        ));
    }
    if app.tokens_in + app.tokens_out > 0 {
        spans.push(Span::styled(
            format!("↑{} ↓{} ", compact(app.tokens_in), compact(app.tokens_out)),
            dim,
        ));
    }
    if let Some(rate) = app.last_usage.and_then(|u| u.cache_rate()) {
        spans.push(Span::styled(format!("cache {rate:.0}% "), dim));
    }
    spans.push(Span::styled(
        if app.pending.is_some() {
            "  a accept · r reject"
        } else if app.working {
            "  ctrl+c interrupt"
        } else {
            "  enter send · shift+tab mode · wheel/pgup scroll · ctrl+c quit"
        },
        dim,
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn compact(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1_000.0)
    }
}

fn render_transcript(frame: &mut Frame, area: Rect, app: &mut App) {
    let width = area.width.saturating_sub(1).max(10) as usize;
    let mut lines: Vec<Line> = Vec::new();
    for entry in &app.entries {
        lines.extend(entry_lines(entry, width));
    }

    let height = area.height as usize;
    app.page = height.saturating_sub(1).max(1);
    let max_scroll = lines.len().saturating_sub(height);
    if app.follow {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
    }
    app.max_scroll = max_scroll;

    frame.render_widget(Paragraph::new(lines).scroll((app.scroll as u16, 0)), area);
}

fn entry_lines(entry: &Entry, width: usize) -> Vec<Line<'static>> {
    let (prefix, text, style) = match entry {
        Entry::User(t) => ("› ", t, Style::new().fg(Color::Cyan).bold()),
        Entry::Assistant(t) => ("", t, Style::new()),
        Entry::Reasoning(t) => ("", t, Style::new().fg(Color::DarkGray).italic()),
        Entry::Command(t) => ("$ ", t, Style::new().fg(Color::Yellow)),
        Entry::Output(t) => ("", t, Style::new().fg(Color::Gray)),
        Entry::Rejected(t) => ("✗ ", t, Style::new().fg(Color::Red)),
        Entry::Error(t) => ("! ", t, Style::new().fg(Color::Red).bold()),
        Entry::Info(t) => ("", t, Style::new().fg(Color::DarkGray)),
    };

    let indent = " ".repeat(prefix.chars().count());
    let mut lines: Vec<Line> = Vec::new();
    for (i, wrapped) in wrap(text, width.saturating_sub(prefix.len()).max(4))
        .into_iter()
        .enumerate()
    {
        let lead = if i == 0 {
            prefix.to_string()
        } else {
            indent.clone()
        };
        lines.push(Line::from(Span::styled(format!("{lead}{wrapped}"), style)));
    }
    lines.push(Line::from(""));
    lines
}

fn render_input(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().border_style(Style::new().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [marker_area, text_area] =
        Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Span::styled("› ", Style::new().fg(Color::Cyan))),
        marker_area,
    );

    // tui-input tracks the cursor; the widget only has to scroll the value to follow it.
    // One column is reserved so the cursor itself is never off-screen.
    let width = text_area.width.saturating_sub(1) as usize;
    let scroll = app.input.visual_scroll(width);
    frame.render_widget(
        Paragraph::new(app.input.value()).scroll((0, scroll as u16)),
        text_area,
    );
    let cursor_x = app.input.visual_cursor().max(scroll) - scroll;
    frame.set_cursor_position((
        (text_area.x + cursor_x as u16).min(text_area.right().saturating_sub(1)),
        text_area.y,
    ));
}

fn render_approval(frame: &mut Frame, area: Rect, app: &App) {
    let Some(pending) = &app.pending else {
        return;
    };
    let title = if pending.tool == "bash" {
        " run this command? ".to_string()
    } else {
        format!(" allow {}? ", pending.tool)
    };
    let block = Block::bordered()
        .title(title)
        .border_style(Style::new().fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = wrap(&pending.command, inner.width.max(4) as usize)
        .into_iter()
        .map(|l| Line::from(Span::styled(l, Style::new().fg(Color::Yellow))))
        .collect();
    lines.truncate(inner.height.saturating_sub(2) as usize);
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("[a]", Style::new().fg(Color::Green).bold()),
        Span::raw("ccept   "),
        Span::styled("[r]", Style::new().fg(Color::Red).bold()),
        Span::raw("eject"),
    ]));
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Greedy word wrap that keeps existing newlines and never loses characters.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split(' ') {
            let mut word = word;
            // A single word longer than the line gets hard-split.
            while word.chars().count() > width {
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                let split = char_index(word, width);
                out.push(word[..split].to_string());
                word = &word[split..];
            }
            let extra = if line.is_empty() { 0 } else { 1 };
            if line.chars().count() + extra + word.chars().count() > width {
                out.push(std::mem::take(&mut line));
            } else if extra == 1 {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push(line);
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
}
