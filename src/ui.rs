//! Rendering. Text is hard-wrapped here so the scroll offset can be computed exactly.

use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use std::ops::Range;

use crate::app::{App, Entry};
use crate::client::Usage;
use crate::permissions::Mode;
use crate::profile::{Method, Tokens};

/// Rows a tool output shows until it is clicked open.
const COLLAPSED_LINES: usize = 3;

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

pub fn render(frame: &mut Frame, app: &mut App) {
    let approval_height = app
        .pending
        .as_ref()
        .map(|pending| {
            let width = frame.area().width.saturating_sub(4).max(10) as usize;
            let lines = wrap(&pending.command, width).len() as u16;
            let offers = [&pending.offers.exact, &pending.offers.prefix]
                .iter()
                .filter(|o| o.is_some())
                .count() as u16;
            (lines + offers + 4).min(frame.area().height / 2).max(5)
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
        app.input_area = None;
        render_approval(frame, bottom_area, app);
    } else {
        app.buttons.clear();
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
    if let Some(field) = &app.cache_break {
        spans.push(Span::styled(
            format!("cache break: {field} "),
            Style::new().fg(Color::Red).bold(),
        ));
    }
    if let Some(percent) = app.cache_miss {
        spans.push(Span::styled(
            format!("cache miss {percent:.0}% "),
            Style::new().fg(Color::Yellow).bold(),
        ));
    }
    spans.push(Span::styled(
        if app.pending.is_some() {
            "  y yes · n no · or click a choice"
        } else if app.working {
            "  ctrl+c interrupt"
        } else {
            "  enter send · shift+tab mode · wheel/pgup scroll · click expands output, pins other badges · ctrl+t tokens · ctrl+c quit"
        },
        dim,
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn compact(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

fn render_transcript(frame: &mut Frame, area: Rect, app: &mut App) {
    let [text_area, bar_area] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).areas(area);
    let width = text_area.width.max(10) as usize;
    let mut lines: Vec<Line> = Vec::new();
    let mut spans = Vec::with_capacity(app.entries.len());
    for (index, entry) in app.entries.iter().enumerate() {
        let start = lines.len();
        lines.extend(entry_lines(entry, width, app.expanded.contains(&index)));
        // The blank separator line belongs to no entry.
        spans.push((start..lines.len() - 1, index));
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
    app.rows = row_map(&spans, app.scroll, area);
    app.rehover();

    frame.render_widget(
        Paragraph::new(lines).scroll((app.scroll as u16, 0)),
        text_area,
    );
    render_badges(frame, text_area, app);

    app.scrollbar = (max_scroll > 0).then_some(bar_area);
    if max_scroll > 0 {
        // Positions run 0..=max_scroll, so the thumb reaches the bottom when following.
        let mut state = ScrollbarState::new(max_scroll + 1)
            .position(app.scroll)
            .viewport_content_length(height);
        let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        frame.render_stateful_widget(bar, bar_area, &mut state);
    }
}

/// Screen rows of each entry that is at least partly visible.
fn row_map(spans: &[(Range<usize>, usize)], scroll: usize, area: Rect) -> Vec<(Range<u16>, usize)> {
    let bottom = scroll + area.height as usize;
    spans
        .iter()
        .filter_map(|(lines, entry)| {
            let start = lines.start.max(scroll);
            let end = lines.end.min(bottom);
            let row = |line: usize| area.y + (line - scroll) as u16;
            (start < end).then(|| (row(start)..row(end), *entry))
        })
        .collect()
}

/// Badges of hovered, pinned or (with ctrl+t) all entries, right-aligned on each
/// entry's last visible row.
fn render_badges(frame: &mut Frame, area: Rect, app: &App) {
    for (rows, entry) in &app.rows {
        let shown = app.all_badges || app.hover == Some(*entry) || app.pinned.contains(entry);
        let Some(text) = app.tokens.get(entry).filter(|_| shown).and_then(badge) else {
            continue;
        };
        let line = Line::from(Span::styled(
            format!(" {text}"),
            Style::new().fg(Color::DarkGray),
        ));
        let width = (line.width() as u16).min(area.width);
        let spot = Rect::new(area.right() - width, rows.end - 1, width, 1);
        frame.render_widget(Clear, spot);
        frame.render_widget(Paragraph::new(line), spot);
    }
}

/// An entry's token badge, with only the parts that are known.
fn badge(tokens: &Tokens) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(input) = tokens.input {
        let how = match tokens.method {
            Method::Exact => String::new(),
            method => format!(" ({})", method.name()),
        };
        parts.push(format!("in {}{how}", compact(input)));
    }
    if tokens.resends > 0 {
        parts.push(format!(
            "resent {}x, {} cached",
            tokens.resends,
            compact(tokens.cached)
        ));
    }
    if let Some(output) = tokens.output {
        parts.push(format!("out {}", compact(output)));
    }
    if let Some(reasoning) = tokens.reasoning {
        parts.push(format!("{} thinking", compact(reasoning)));
    }
    if let Some(call) = tokens.call {
        parts.push(format!("call: {}", usage_badge(call)));
    }
    if let Some(child) = tokens.child {
        parts.push(format!("child: {}", usage_badge(child)));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// `in 1.2k new · 8.3k cached · out 340 (210 thinking)`, without the zero parts.
fn usage_badge(usage: Usage) -> String {
    let fresh = usage.input - usage.cached.min(usage.input);
    let mut parts = vec![format!("in {} new", compact(fresh))];
    if usage.cached > 0 {
        parts.push(format!("{} cached", compact(usage.cached)));
    }
    let mut out = format!("out {}", compact(usage.output));
    if usage.reasoning > 0 {
        out.push_str(&format!(" ({} thinking)", compact(usage.reasoning)));
    }
    parts.push(out);
    parts.join(" · ")
}

/// An entry's rows plus a blank separator; long tool output shows only its head
/// unless `expanded`.
fn entry_lines(entry: &Entry, width: usize, expanded: bool) -> Vec<Line<'static>> {
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
    let mut wrapped_lines = wrap(text, width.saturating_sub(prefix.len()).max(4));
    let hidden = match entry {
        Entry::Output(_) if !expanded => wrapped_lines.len().saturating_sub(COLLAPSED_LINES),
        _ => 0,
    };
    wrapped_lines.truncate(wrapped_lines.len() - hidden);
    for (i, wrapped) in wrapped_lines.into_iter().enumerate() {
        let lead = if i == 0 {
            prefix.to_string()
        } else {
            indent.clone()
        };
        lines.push(Line::from(Span::styled(format!("{lead}{wrapped}"), style)));
    }
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!("[+{hidden} lines]"),
            Style::new().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    lines
}

fn render_input(frame: &mut Frame, area: Rect, app: &mut App) {
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
    app.input_area = Some((text_area, scroll));
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

/// The approval prompt. Each `[k]` choice is recorded in `app.buttons` so a click on
/// it acts exactly as pressing `k`.
fn render_approval(frame: &mut Frame, area: Rect, app: &mut App) {
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

    let key = |k: &'static str, color| Span::styled(k, Style::new().fg(color).bold());
    let mut options = Vec::new();
    let mut option_keys = Vec::new();
    let bash = pending.tool == "bash";
    if let Some(rule) = &pending.offers.exact {
        let what = if bash { "command" } else { "file" };
        option_keys.push('a');
        options.push(Line::from(vec![
            key("[a]", Color::Cyan),
            Span::raw(format!(" always allow this exact {what}: {rule}")),
        ]));
    }
    if let Some(rule) = &pending.offers.prefix {
        let what = if bash {
            "always allow this prefix"
        } else {
            "always allow edits under this directory"
        };
        option_keys.push('p');
        options.push(Line::from(vec![
            key("[p]", Color::Cyan),
            Span::raw(format!(" {what}: {rule}")),
        ]));
    }

    let mut lines: Vec<Line> = wrap(&pending.command, inner.width.max(4) as usize)
        .into_iter()
        .map(|l| Line::from(Span::styled(l, Style::new().fg(Color::Yellow))))
        .collect();
    lines.truncate(inner.height.saturating_sub(2 + options.len() as u16) as usize);
    lines.push(Line::from(""));
    let row = inner.y + lines.len() as u16;
    let mut buttons = vec![
        (Rect::new(inner.x, row, 5, 1), 'y'),
        (Rect::new(inner.x + 8, row, 4, 1), 'n'),
    ];
    for (i, k) in option_keys.into_iter().enumerate() {
        buttons.push((Rect::new(inner.x, row + 1 + i as u16, inner.width, 1), k));
    }
    lines.push(Line::from(vec![
        key("[y]", Color::Green),
        Span::raw("es   "),
        key("[n]", Color::Red),
        Span::raw("o"),
    ]));
    lines.extend(options);
    frame.render_widget(Paragraph::new(lines), inner);
    app.buttons = buttons
        .into_iter()
        .map(|(spot, k)| (spot.intersection(inner), KeyCode::Char(k)))
        .filter(|(spot, _)| !spot.is_empty())
        .collect();
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
    use crate::permissions::Offers;
    use crate::session::Approval;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use tui_input::Input;

    fn usage(input: u64, cached: u64, output: u64, reasoning: u64) -> Usage {
        Usage {
            input,
            cached,
            output,
            reasoning,
        }
    }

    #[test]
    fn badges_show_only_the_parts_that_apply() {
        assert_eq!(badge(&Tokens::default()), None);
        let call = Tokens {
            call: Some(usage(9_500, 8_300, 340, 210)),
            ..Tokens::default()
        };
        assert_eq!(
            badge(&call).unwrap(),
            "call: in 1.2k new · 8.3k cached · out 340 (210 thinking)"
        );
        let fresh = Tokens {
            call: Some(usage(1_500_000, 0, 12, 0)),
            ..Tokens::default()
        };
        assert_eq!(badge(&fresh).unwrap(), "call: in 1.5M new · out 12");
        let resent = Tokens {
            input: Some(999),
            method: Method::Exact,
            resends: 3,
            cached: 2_000,
            ..Tokens::default()
        };
        assert_eq!(badge(&resent).unwrap(), "in 999 · resent 3x, 2.0k cached");
        let guessed = Tokens {
            input: Some(5),
            method: Method::Tokenized,
            ..Tokens::default()
        };
        assert_eq!(badge(&guessed).unwrap(), "in 5 (tokenized)");
        let thinking = Tokens {
            reasoning: Some(40),
            ..Tokens::default()
        };
        assert_eq!(badge(&thinking).unwrap(), "40 thinking");
    }

    #[test]
    fn row_map_follows_the_scroll() {
        let spans = [(0..2, 0), (3..8, 1), (9..10, 2)];
        let area = Rect::new(0, 1, 20, 5);
        assert_eq!(row_map(&spans, 4, area), vec![(1..5, 1)]);
        assert_eq!(row_map(&spans, 0, area), vec![(1..3, 0), (4..6, 1)]);
    }

    #[test]
    fn hovering_draws_the_badge_on_the_entry_last_row() {
        let mut app = App::detached();
        app.entries
            .push(Entry::User("hello there, a message that wraps".to_string()));
        app.tokens.insert(
            1,
            Tokens {
                input: Some(5),
                method: Method::Tokenized,
                ..Tokens::default()
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(30, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        // The intro wraps onto several rows; the message follows its blank line.
        let (rows, _) = app.rows.iter().find(|(_, e)| *e == 1).cloned().unwrap();
        assert!(app.rows[0].0.len() > 1, "{:?}", app.rows);
        assert_eq!(rows.len(), 2);
        assert_eq!(app.entry_at(rows.start), Some(1));
        assert_eq!(app.entry_at(rows.start - 1), None);

        let row_text = |terminal: &Terminal<TestBackend>, y: u16| -> String {
            let buffer = terminal.backend().buffer();
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        };
        assert!(!row_text(&terminal, rows.end - 1).contains("tokenized"));
        let moved = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 3,
            row: rows.start,
            modifiers: KeyModifiers::NONE,
        };
        assert!(app.on_mouse(moved));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(
            row_text(&terminal, rows.end - 1)
                .trim_end()
                .ends_with(" in 5 (tokenized)"),
            "{:?}",
            row_text(&terminal, rows.end - 1)
        );
        assert!(row_text(&terminal, rows.start).starts_with("› hello"));
    }

    fn left(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn down(column: u16, row: u16) -> MouseEvent {
        left(MouseEventKind::Down(MouseButton::Left), column, row)
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                let row: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                row.trim_end().to_string() + "\n"
            })
            .collect()
    }

    fn approval(exact: Option<&str>) -> Approval {
        Approval {
            id: 7,
            tool: "bash".to_string(),
            command: "ls".to_string(),
            offers: Offers {
                exact: exact.map(str::to_string),
                prefix: None,
            },
        }
    }

    #[test]
    fn clicking_an_approval_choice_answers_it() {
        let mut app = App::detached();
        app.pending = Some(approval(None));
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let &(yes, _) = app
            .buttons
            .iter()
            .find(|(_, k)| *k == KeyCode::Char('y'))
            .unwrap();
        assert!(
            screen(&terminal)
                .lines()
                .nth(yes.y as usize)
                .unwrap()
                .contains("[y]es")
        );
        // Nothing offers an exact rule, so `a` is not a button and a click there is inert.
        assert!(app.buttons.iter().all(|(_, k)| *k != KeyCode::Char('a')));
        assert!(!app.on_mouse(down(yes.right() + 1, yes.y)));
        assert!(app.pending.is_some());
        assert!(app.on_mouse(down(yes.x + 1, yes.y)));
        assert!(app.pending.is_none());

        app.pending = Some(approval(Some("ls")));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let &(always, _) = app
            .buttons
            .iter()
            .find(|(_, k)| *k == KeyCode::Char('a'))
            .unwrap();
        assert!(
            screen(&terminal)
                .lines()
                .nth(always.y as usize)
                .unwrap()
                .contains("[a]")
        );
        assert!(app.on_mouse(down(always.x + 10, always.y)));
        assert!(app.pending.is_none());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.buttons.is_empty());
        app.on_mouse(down(always.x, always.y));
        assert_eq!(app.input.value(), "", "a stale button types nothing");
    }

    #[test]
    fn long_tool_output_collapses_until_clicked() {
        let mut app = App::detached();
        app.entries
            .push(Entry::Output("one\ntwo\nthree\nfour\nfive".to_string()));
        app.entries.push(Entry::Output("short".to_string()));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("one\ntwo\nthree\n[+2 lines]\n"), "{text}");
        assert!(!text.contains("four"));
        assert!(text.contains("short\n"));

        let (rows, _) = app.rows.iter().find(|(_, e)| *e == 1).cloned().unwrap();
        assert_eq!(rows.len(), 4);
        assert!(app.on_mouse(down(2, rows.start)));
        assert!(app.pinned.is_empty(), "a click on output does not pin");
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("three\nfour\nfive\n"), "{text}");
        assert!(!text.contains("[+2 lines]"));

        assert!(app.on_mouse(down(2, rows.start)));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("[+2 lines]"));
    }

    #[test]
    fn dragging_the_scrollbar_scrolls_in_proportion() {
        let mut app = App::detached();
        for i in 0..40 {
            app.entries.push(Entry::User(format!("message {i}")));
        }
        let mut terminal = Terminal::new(TestBackend::new(40, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let bar = app.scrollbar.unwrap();
        assert_eq!(bar.x, 39);
        assert!(app.max_scroll > 0);
        assert_eq!(app.scroll, app.max_scroll);

        assert!(app.on_mouse(down(bar.x, bar.y)));
        assert_eq!(app.scroll, 0);
        assert!(!app.follow);
        let middle = bar.y + (bar.height - 1) / 2;
        assert!(app.on_mouse(left(MouseEventKind::Drag(MouseButton::Left), 5, middle)));
        let half = app.max_scroll / 2;
        assert!(
            app.scroll.abs_diff(half) <= app.max_scroll / 10,
            "{}",
            app.scroll
        );
        let dragged = app.scroll;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.scroll, dragged, "a redraw keeps the dragged offset");

        assert!(app.on_mouse(left(
            MouseEventKind::Drag(MouseButton::Left),
            5,
            bar.bottom() + 5
        )));
        assert_eq!(app.scroll, app.max_scroll);
        assert!(app.follow);

        assert!(!app.on_mouse(left(MouseEventKind::Up(MouseButton::Left), 5, 0)));
        let before = app.scroll;
        assert!(!app.on_mouse(left(MouseEventKind::Drag(MouseButton::Left), 5, bar.y)));
        assert_eq!(app.scroll, before, "a drag after release does nothing");
    }

    #[test]
    fn clicking_the_input_moves_the_cursor() {
        let mut app = App::detached();
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let (area, _) = app.input_area.unwrap();
        assert!(
            !app.on_mouse(down(area.x + 2, area.y)),
            "nothing to move through"
        );

        app.input = Input::new("héllo world".to_string());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.on_mouse(down(area.x + 3, area.y)));
        assert_eq!(app.input.cursor(), 3);
        assert!(app.on_mouse(down(area.right() - 1, area.y)));
        assert_eq!(app.input.cursor(), 11);
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
}
