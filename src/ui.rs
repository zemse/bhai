//! Rendering. Text is hard-wrapped here so the scroll offset can be computed exactly.

use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use std::ops::Range;
use std::time::Duration;

use crate::app::{App, Entry, TrustGate};
use crate::client::Usage;
use crate::commands::{self, Item};
use crate::input::Row;
use crate::limits::{self, RateLimits};
use crate::markdown;
use crate::permissions::Mode;
use crate::profile::{Method, Tokens};

/// Rows a tool output shows until it is clicked open.
const COLLAPSED_LINES: usize = 3;

/// Lines the input grows to before it scrolls.
const MAX_INPUT_LINES: usize = 8;

/// Characters of the judged call the working row shows.
const JUDGING_CLIP: usize = 48;

/// How long the note about a drag's copy stays on the input's border.
const COPIED_FOR: Duration = Duration::from_secs(3);

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

pub fn render(frame: &mut Frame, app: &mut App) {
    // The trust question takes the bottom area before anything else can, sized to what
    // it has to say.
    if let Some(gate) = app.trust_gate.clone() {
        let width = frame.area().width.saturating_sub(4).max(10) as usize;
        let body = wrap(&trust_text(&gate), width).len() as u16;
        let height = (body + 5).min(frame.area().height.saturating_sub(2)).max(4);
        let [status_area, transcript_area, bottom_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(height),
        ])
        .areas(frame.area());
        render_status(frame, status_area, app);
        render_transcript(frame, transcript_area, app);
        app.input_area = None;
        app.buttons.clear();
        render_trust(frame, bottom_area, &gate);
        return;
    }
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
        .unwrap_or_else(|| {
            // The input's own rows, wrapped to the width it will be drawn at.
            let width = input_width(frame.area().width.saturating_sub(4));
            app.input.rows(width).len().min(MAX_INPUT_LINES) as u16 + 2
        });

    // The `/` menu stands between the transcript and the prompt, and only while the
    // prompt is what the user is looking at.
    let items = match app.pending.is_some() || app.diff.is_some() {
        true => Vec::new(),
        false => app.menu_items(),
    };
    let menu_height = match items.len() {
        0 => 0,
        rows => rows.min(commands::MAX_ROWS) as u16 + 2,
    };

    // The spinner sits just above the prompt, where the eye already is. An approval is
    // the agent waiting on the user, so nothing spins there.
    let working_height = u16::from(app.working && app.pending.is_none());

    let [
        status_area,
        transcript_area,
        menu_area,
        working_area,
        bottom_area,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(menu_height),
        Constraint::Length(working_height),
        Constraint::Length(approval_height),
    ])
    .areas(frame.area());

    render_status(frame, status_area, app);
    if let Some(diff) = &mut app.diff {
        // The pane takes the transcript and input rows; an approval still shows below.
        let area = if app.pending.is_some() {
            transcript_area
        } else {
            transcript_area.union(bottom_area)
        };
        diff.render(frame, area);
        app.rows.clear();
        app.lines.clear();
        app.transcript_area = None;
        app.scrollbar = None;
        app.input_area = None;
        // The pane covered the spinner's row, so it goes back on top.
        render_working(frame, working_area, app);
        if app.pending.is_some() {
            render_approval(frame, bottom_area, app);
        }
        return;
    }
    render_transcript(frame, transcript_area, app);
    if menu_height > 0 {
        render_menu(frame, menu_area, &items, app.menu.unwrap_or(0));
    }
    render_working(frame, working_area, app);
    if app.pending.is_some() {
        app.input_area = None;
        render_approval(frame, bottom_area, app);
    } else {
        app.buttons.clear();
        render_input(frame, bottom_area, app);
    }
}

fn render_status(frame: &mut Frame, area: Rect, app: &mut App) {
    let dim = Style::new().fg(Color::DarkGray);
    let mut spans = vec![
        Span::styled(" bhai ", Style::new().fg(Color::Black).bg(Color::Cyan)),
        Span::styled(format!(" {} ", app.model), dim),
    ];
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
    if app.cache_stalled {
        spans.push(Span::styled(
            "cache stalled ",
            Style::new().fg(Color::Yellow).bold(),
        ));
    }
    if let Some(percent) = app.cache_miss {
        spans.push(Span::styled(
            format!("cache miss {percent:.0}% "),
            Style::new().fg(Color::Yellow).bold(),
        ));
    }
    app.limits_area = app.rate_limits.map(|found| {
        let x = area.x + spans.iter().map(Span::width).sum::<usize>() as u16;
        let segment = limit_spans(&found, app.limits_hover);
        let width = segment.iter().map(Span::width).sum::<usize>() as u16;
        spans.extend(segment);
        Rect::new(x, area.y, width, 1).intersection(area)
    });
    spans.push(Span::styled(
        match app.pending.is_some() {
            true => "  y yes · n no · or click a choice",
            // The rest of the keys live in /help rather than across the top bar.
            false => "  / for commands",
        },
        dim,
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The spinner row above the prompt, drawn only while a turn is actually running.
fn render_working(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let mut spans = vec![Span::styled(
        format!(" {} working", SPINNER[app.spinner % SPINNER.len()]),
        Style::new().fg(Color::Yellow),
    )];
    let dim = Style::new().fg(Color::DarkGray);
    if let Some(call) = &app.judging {
        spans.push(Span::styled(
            format!(" · auto mode is checking {}", clip(call, JUDGING_CLIP)),
            Style::new().fg(Color::Cyan),
        ));
    }
    if app.queued > 0 {
        spans.push(Span::styled(format!(" · {} queued", app.queued), dim));
    }
    spans.push(Span::styled(" · ctrl+c interrupt", dim));
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// `5h 42% · wk 17% `, each window coloured by how close it is to its limit; with
/// reset times when `hover`.
fn limit_spans(found: &RateLimits, hover: bool) -> Vec<Span<'static>> {
    let dim = Style::new().fg(Color::DarkGray);
    let now = chrono::Local::now();
    let mut spans = Vec::new();
    for (i, window) in found.windows().enumerate() {
        if i > 0 {
            spans.push(Span::styled("· ", dim));
        }
        let style = if window.used_percent >= limits::ALERT {
            Style::new().fg(Color::Red).bold()
        } else if window.used_percent >= limits::WARN {
            Style::new().fg(Color::Yellow)
        } else {
            dim
        };
        let mut text = format!("{} {:.0}% ", window.label(), window.used_percent);
        if let Some(at) = window.reset_label(now).filter(|_| hover) {
            text.push_str(&format!("resets {at} "));
        }
        spans.push(Span::styled(text, style));
    }
    spans
}

/// `text` cut to `max` characters, with an ellipsis when it had more.
fn clip(text: &str, max: usize) -> String {
    let flat = text.replace('\n', " ");
    match flat.chars().count() > max {
        true => flat.chars().take(max - 1).collect::<String>() + "…",
        false => flat,
    }
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
    let entries = app.entries();
    let mut spans = Vec::with_capacity(entries.list.len());
    for (index, entry) in entries.list.iter().enumerate() {
        let start = lines.len();
        lines.extend(entry_lines(entry, width, app.expanded.contains(&index)));
        // The blank separator line belongs to no entry.
        spans.push((start..lines.len() - 1, index));
    }
    drop(entries);

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
    app.transcript_area = Some(text_area);
    app.lines = lines.iter().map(plain).collect();
    app.rehover();

    if let Some(selection) = app.selection {
        for (index, line) in lines.iter_mut().enumerate() {
            let Some(range) = selection.on_line(index, app.lines[index].chars().count()) else {
                continue;
            };
            *line = highlight(std::mem::take(line), range);
        }
    }

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

/// One rendered line's text, which is what a selection copies.
fn plain(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Draw the chars in `range` of one rendered line reversed, keeping each span's style.
fn highlight(mut line: Line<'static>, range: Range<usize>) -> Line<'static> {
    let mut out = Vec::with_capacity(line.spans.len() + 2);
    let mut at = 0;
    for span in std::mem::take(&mut line.spans) {
        let len = span.content.chars().count();
        let start = range.start.saturating_sub(at).min(len);
        let end = range.end.saturating_sub(at).min(len);
        at += len;
        if start >= end {
            out.push(span);
            continue;
        }
        let part = |from: usize, to: usize| -> String {
            span.content.chars().skip(from).take(to - from).collect()
        };
        if start > 0 {
            out.push(Span::styled(part(0, start), span.style));
        }
        out.push(Span::styled(part(start, end), span.style.reversed()));
        if end < len {
            out.push(Span::styled(part(end, len), span.style));
        }
    }
    line.spans = out;
    line
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
    let entries = app.entries();
    for (rows, entry) in &app.rows {
        let shown = app.all_badges || app.hover == Some(*entry) || app.pinned.contains(entry);
        let Some(text) = entries.tokens.get(entry).filter(|_| shown).and_then(badge) else {
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
    if let Entry::Assistant(text) = entry {
        let mut lines = markdown::render(text, width);
        if lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(""));
        return lines;
    }
    let (prefix, text, style): (&str, &str, Style) = match entry {
        Entry::User(t) => ("› ", t, Style::new().fg(Color::Cyan).bold()),
        Entry::Queued(t) => ("queued › ", t, Style::new().fg(Color::DarkGray)),
        Entry::Assistant(t) => ("", t, Style::new()),
        Entry::Reasoning(t) => ("", t, Style::new().fg(Color::DarkGray).italic()),
        Entry::Command(t) => ("$ ", t, Style::new().fg(Color::Yellow)),
        Entry::Output(t) => ("", t, Style::new().fg(Color::Gray)),
        Entry::Running { tail, .. } => ("", tail.trim_end(), Style::new().fg(Color::Gray)),
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
    // A running command shows its latest lines instead.
    if let Entry::Running { .. } = entry
        && !expanded
    {
        let skip = wrapped_lines.len().saturating_sub(COLLAPSED_LINES);
        wrapped_lines.drain(..skip);
    }
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
    if let Entry::Running { tail, lines: done } = entry {
        let total = done + usize::from(!tail.is_empty() && !tail.ends_with('\n'));
        lines.push(Line::from(Span::styled(
            format!("[running, {total} lines]"),
            Style::new().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    lines
}

/// The width the input text wraps at: the text area less the cursor's own column.
fn input_width(area_width: u16) -> usize {
    area_width.saturating_sub(3).max(1) as usize
}

/// The permission mode, drawn on the bottom border of whatever the user is looking at
/// so it stays in sight while typing and while an approval is up.
fn mode_chip(mode: Mode) -> Line<'static> {
    let style = match mode {
        Mode::Ask => Style::new().fg(Color::DarkGray),
        Mode::Auto => Style::new().fg(Color::Yellow),
        Mode::Bypass => Style::new().fg(Color::Red),
    };
    Line::styled(format!(" {mode} · shift+tab "), style).right_aligned()
}

/// The `/` menu: one row per command or skill, the highlighted one reversed, scrolled
/// so the highlighted row stays in view.
fn render_menu(frame: &mut Frame, area: Rect, items: &[Item], selected: usize) {
    let block = Block::bordered()
        .border_style(Style::new().fg(Color::DarkGray))
        .title_bottom(
            Line::styled(
                " ↑↓ pick · tab complete · esc close ",
                Style::new().fg(Color::DarkGray),
            )
            .right_aligned(),
        );
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let rows = inner.height as usize;
    let top = selected
        .saturating_sub(rows.saturating_sub(1))
        .min(items.len().saturating_sub(rows));
    let width = inner.width as usize;
    let lines: Vec<Line> = items
        .iter()
        .enumerate()
        .skip(top)
        .take(rows)
        .map(|(i, item)| menu_row(item, width, i == selected))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// One menu row: `/name args` padded out, then the help text.
fn menu_row(item: &Item, width: usize, selected: bool) -> Line<'static> {
    let name = format!("{}{}", item.label(), item.args);
    let mut text = format!(" {name:<22} {}", item.help);
    text.truncate(
        text.char_indices()
            .nth(width)
            .map_or(text.len(), |(at, _)| at),
    );
    let style = match selected {
        true => Style::new().fg(Color::Black).bg(Color::Cyan),
        false => Style::new().fg(Color::DarkGray),
    };
    // The name keeps its colour on an unselected row; the help stays dim.
    if selected {
        return Line::styled(format!("{text:<width$}"), style);
    }
    let cut = name.chars().count() + 1;
    let head: String = text.chars().take(cut).collect();
    let tail: String = text.chars().skip(cut).collect();
    Line::from(vec![
        Span::styled(head, Style::new().fg(Color::Cyan)),
        Span::styled(tail, style),
    ])
}

fn render_input(frame: &mut Frame, area: Rect, app: &mut App) {
    let mut block = Block::bordered()
        .border_style(Style::new().fg(Color::DarkGray))
        .title_bottom(mode_chip(app.mode));
    // A drag copies as it ends, and says so here rather than in the transcript.
    if let Some(chars) = app
        .copied
        .filter(|(at, _)| at.elapsed() < COPIED_FOR)
        .map(|(_, n)| n)
    {
        block = block.title_bottom(Line::styled(
            format!(" copied {chars} chars "),
            Style::new().fg(Color::Cyan),
        ));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [marker_area, text_area] =
        Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Span::styled("› ", Style::new().fg(Color::Cyan))),
        marker_area,
    );

    // The text wraps to the width, less one column so the cursor at the end of a full
    // row is never off-screen. The widget scrolls only when it has more rows than fit.
    let width = input_width(text_area.width);
    let top = app.input.top(text_area.height.max(1) as usize, width);
    let (row, column) = app.input.cursor_position(width);
    app.input_area = Some((text_area, top, width));
    let selection = app.input.selection();
    let mut rows: Vec<Line> = app
        .input
        .rows(width)
        .into_iter()
        .map(|r| input_row(r, selection.as_ref()))
        .collect();
    // What tab would complete, grey from the cursor on. The cursor is at the end of the
    // text for it to show at all, so it belongs on the last row.
    let suggestion = app.suggestion();
    if !suggestion.is_empty()
        && let Some(last) = rows.last_mut()
    {
        last.push_span(Span::styled(suggestion, Style::new().fg(Color::DarkGray)));
    }
    frame.render_widget(Paragraph::new(rows).scroll((top as u16, 0)), text_area);
    frame.set_cursor_position((
        (text_area.x + column as u16).min(text_area.right().saturating_sub(1)),
        text_area.y + (row - top) as u16,
    ));
}

/// One row of the input, with the part inside `selection` drawn reversed.
fn input_row(row: Row, selection: Option<&Range<usize>>) -> Line<'static> {
    let len = row.text.chars().count();
    let Some(range) = selection else {
        return Line::raw(row.text);
    };
    let start = range.start.saturating_sub(row.start).min(len);
    let end = range.end.saturating_sub(row.start).min(len);
    if start >= end {
        return Line::raw(row.text);
    }
    let part = |from: usize, to: usize| -> String {
        row.text.chars().skip(from).take(to - from).collect()
    };
    Line::from(vec![
        Span::raw(part(0, start)),
        Span::styled(part(start, end), Style::new().reversed()),
        Span::raw(part(end, len)),
    ])
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
        .title_bottom(mode_chip(app.mode))
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

/// What trusting this project would let it do.
fn trust_text(gate: &TrustGate) -> String {
    let mut out = format!(
        "{} mode writes inside this project and runs its build and test commands \
without asking, and the judge decides the rest. That runs code this project supplies.",
        gate.mode
    );
    if gate.rules > 0 {
        out.push_str(&format!(
            " It also honours the {} allow rules the project's own settings files ship.",
            gate.rules
        ));
    }
    out
}

/// The question asked on opening a project the trust store does not know.
fn render_trust(frame: &mut Frame, area: Rect, gate: &TrustGate) {
    let block = Block::bordered()
        .title(" do you trust this folder? ")
        .border_style(Style::new().fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let what = trust_text(gate);
    let key = |k: &'static str, color| Span::styled(k, Style::new().fg(color).bold());
    let mut lines = vec![Line::from(Span::styled(
        gate.root.clone(),
        Style::new().fg(Color::Yellow),
    ))];
    lines.extend(
        wrap(&what, inner.width.max(4) as usize)
            .into_iter()
            .map(|l| Line::from(Span::styled(l, Style::new().fg(Color::DarkGray)))),
    );
    lines.truncate(inner.height.saturating_sub(2) as usize);
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        key("[y]", Color::Green),
        Span::raw(format!(" trust it, use {} mode   ", gate.mode)),
        key("[n]", Color::Red),
        Span::raw("o, stay in ask mode"),
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
    use crate::cache::CacheBreak;
    use crate::permissions::Offers;
    use crate::session::{Approval, Event};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::style::Modifier;

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
        app.entries()
            .push(Entry::User("hello there, a message that wraps".to_string()));
        app.entries().tokens.insert(
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

    #[test]
    fn assistant_markdown_rows_line_up_with_the_row_map() {
        let mut app = App::detached();
        let raw = "# Plan\n\n- first step that wraps around\n- second\n\n```sh\nls";
        app.entries().push(Entry::Assistant(raw.to_string()));
        app.entries().tokens.insert(
            1,
            Tokens {
                input: Some(5),
                method: Method::Tokenized,
                ..Tokens::default()
            },
        );
        app.all_badges = true;
        let mut terminal = Terminal::new(TestBackend::new(24, 30)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let (rows, _) = app.rows.iter().find(|(_, e)| *e == 1).cloned().unwrap();
        let screen = screen(&terminal);
        let lines: Vec<&str> = screen.lines().collect();
        let entry: Vec<&str> = lines[rows.start as usize..rows.end as usize]
            .iter()
            .map(|l| l.trim_end())
            .collect();
        assert_eq!(entry[0], "Plan");
        assert!(entry.contains(&"  around"), "{entry:?}");
        assert!(entry[entry.len() - 1].starts_with("  ls"), "{entry:?}");
        assert!(entry[entry.len() - 1].ends_with("(tokenized)"), "{entry:?}");
        assert!(matches!(&app.entries().list[1], Entry::Assistant(t) if t == raw));
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

    fn up(column: u16, row: u16) -> MouseEvent {
        left(MouseEventKind::Up(MouseButton::Left), column, row)
    }

    /// A press and release on one cell, which is what counts as a click.
    fn click(app: &mut App, column: u16, row: u16) -> bool {
        app.on_mouse(down(column, row)) | app.on_mouse(up(column, row))
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

    #[test]
    fn the_spinner_and_the_queue_count_sit_above_the_prompt() {
        let mut app = App::detached();
        app.working = true;
        app.on_event(Event::Queued {
            position: 1,
            text: "later".to_string(),
        });
        app.entries().push(Entry::Queued("later".to_string()));
        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(shown.contains("queued › later"), "{shown}");

        let rows: Vec<&str> = shown.lines().collect();
        let working = rows.iter().position(|r| r.contains("working")).unwrap();
        assert!(working > 0, "not the top bar: {shown}");
        assert!(rows[working].contains("1 queued"), "{shown}");
        assert!(rows[working].contains("ctrl+c interrupt"), "{shown}");
        let prompt = rows.iter().rposition(|r| r.contains("›")).unwrap();
        assert!(working < prompt, "{shown}");

        // An approval is the agent waiting on the user, so the spinner goes away.
        app.pending = Some(approval(None));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!screen(&terminal).contains("working"), "{shown}");
    }

    #[test]
    fn the_working_row_says_when_the_judge_is_deciding() {
        let mut app = App::detached();
        app.working = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!screen(&terminal).contains("checking"));

        app.on_event(Event::Judging(Some("bash: cargo fmt".to_string())));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(
            shown.contains("auto mode is checking bash: cargo fmt"),
            "{shown}"
        );
        app.on_event(Event::Judging(None));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!screen(&terminal).contains("checking"));

        // A long call is cut to fit beside the spinner, and never wraps the row.
        let long = clip(&"x".repeat(200), JUDGING_CLIP);
        assert_eq!(long, "x".repeat(47) + "\u{2026}");
        assert_eq!(clip("one\ntwo", JUDGING_CLIP), "one two");
    }

    #[test]
    fn the_selected_part_of_the_input_is_reversed() {
        let mut app = App::detached();
        app.input.set("hello".to_string());
        app.input
            .handle(tui_input::InputRequest::SetCursor(1), false);
        app.input
            .handle(tui_input::InputRequest::GoToNextChar, true);
        app.input
            .handle(tui_input::InputRequest::GoToNextChar, true);
        let mut terminal = Terminal::new(TestBackend::new(20, 8)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let selected: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.modifier.contains(Modifier::REVERSED))
            .map(|cell| cell.symbol())
            .collect();
        assert_eq!(selected, "el");
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
    fn a_running_command_shows_its_latest_lines() {
        let mut app = App::detached();
        app.entries().apply(&Event::ToolStart("seq 5".to_string()));
        app.entries()
            .apply(&Event::ToolProgress("one\ntwo\nthree\n".to_string()));
        app.entries()
            .apply(&Event::ToolProgress("four\nfi".to_string()));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(
            text.contains("three\nfour\nfi\n[running, 5 lines]\n"),
            "{text}"
        );
        assert!(!text.contains("two"), "{text}");

        app.entries()
            .apply(&Event::ToolOutput("exit code: 0\n1".to_string()));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("$ seq 5\n\nexit code: 0\n1\n"), "{text}");
        assert!(!text.contains("running"), "{text}");
    }

    #[test]
    fn long_tool_output_collapses_until_clicked() {
        let mut app = App::detached();
        app.entries()
            .push(Entry::Output("one\ntwo\nthree\nfour\nfive".to_string()));
        app.entries().push(Entry::Output("short".to_string()));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("one\ntwo\nthree\n[+2 lines]\n"), "{text}");
        assert!(!text.contains("four"));
        assert!(text.contains("short\n"));

        let (rows, _) = app.rows.iter().find(|(_, e)| *e == 1).cloned().unwrap();
        assert_eq!(rows.len(), 4);
        assert!(click(&mut app, 2, rows.start));
        assert!(app.pinned.is_empty(), "a click on output does not pin");
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("three\nfour\nfive\n"), "{text}");
        assert!(!text.contains("[+2 lines]"));

        // A second click on the same cell in a row would be a double click, which
        // selects a word instead, so this one lands on the row below.
        assert!(click(&mut app, 2, rows.start + 1));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("[+2 lines]"));

        // A press that moves is a drag, so it selects instead of collapsing again.
        assert!(app.on_mouse(down(2, rows.start)));
        assert!(app.on_mouse(left(MouseEventKind::Drag(MouseButton::Left), 4, rows.start)));
        // The release copies what was dragged over, which is a redraw of its own.
        assert!(app.on_mouse(up(4, rows.start)));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("[+2 lines]"), "still collapsed");
    }

    #[test]
    fn dragging_the_transcript_selects_the_text_it_covers() {
        let mut app = App::detached();
        app.entries().push(Entry::User("hello there".to_string()));
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let (rows, _) = app.rows.iter().find(|(_, e)| *e == 1).cloned().unwrap();
        let area = app.transcript_area.unwrap();

        // The entry draws as `› hello there`, so the drag starts on the `h`.
        assert!(app.on_mouse(down(area.x + 2, rows.start)));
        assert!(app.on_mouse(left(
            MouseEventKind::Drag(MouseButton::Left),
            area.x + 6,
            rows.start
        )));
        assert_eq!(app.selected_text().as_deref(), Some("hello"));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let reversed: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.modifier.contains(Modifier::REVERSED))
            .map(|cell| cell.symbol())
            .collect();
        assert_eq!(reversed, "hello");

        // Scrolling away leaves the selection on the line it was made on.
        for _ in 0..30 {
            app.entries().push(Entry::User("filler".to_string()));
        }
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.max_scroll > 0);
        assert_eq!(app.selected_text().as_deref(), Some("hello"));
    }

    #[test]
    fn dragging_the_scrollbar_scrolls_in_proportion() {
        let mut app = App::detached();
        for i in 0..40 {
            app.entries().push(Entry::User(format!("message {i}")));
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
        let (area, ..) = app.input_area.unwrap();
        assert!(
            !app.on_mouse(down(area.x + 2, area.y)),
            "nothing to move through"
        );

        app.input.set("héllo world".to_string());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.on_mouse(down(area.x + 3, area.y)));
        assert_eq!(app.input.cursor(), 3);
        assert!(app.on_mouse(down(area.right() - 1, area.y)));
        assert_eq!(app.input.cursor(), 11);
    }

    #[test]
    fn the_input_grows_with_its_lines_then_scrolls() {
        let mut app = App::detached();
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        app.input.set("one\ntwo".to_string());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let (area, top, _) = app.input_area.unwrap();
        assert_eq!((area.height, top), (2, 0));
        assert!(screen(&terminal).contains("› one"));
        assert!(app.on_mouse(down(area.x + 1, area.y + 1)));
        assert_eq!(app.input.cursor(), 5);

        let lines: Vec<String> = (0..12).map(|i| format!("line {i}")).collect();
        app.input.set(lines.join("\n"));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let (area, top, _) = app.input_area.unwrap();
        assert_eq!((area.height, top), (MAX_INPUT_LINES as u16, 4));
        let text = screen(&terminal);
        assert!(text.contains("line 11") && !text.contains("line 3"));
        assert_eq!(terminal.get_cursor_position().unwrap().y, area.bottom() - 1);
    }

    #[test]
    fn a_long_prompt_wraps_instead_of_running_off_the_side() {
        let mut app = App::detached();
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        app.input
            .set("the quick brown fox jumps over the lazy dog".to_string());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let (area, top, _) = app.input_area.unwrap();
        assert_eq!((area.height, top), (2, 0));
        let text = screen(&terminal);
        assert!(text.contains("› the quick brown fox jumps over"));
        assert!(text.contains("the lazy dog"));
        // The cursor sits at the end of the second row, not off the right edge.
        let at = terminal.get_cursor_position().unwrap();
        assert_eq!((at.x, at.y), (area.x + 12, area.y + 1));
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

    fn window(used_percent: f64, window_minutes: u64) -> limits::Window {
        limits::Window {
            used_percent,
            window_minutes: Some(window_minutes),
            resets_at: Some(chrono::Local::now().timestamp() + 60),
        }
    }

    /// The status bar cell under the first character of `text`.
    fn status_cell<'a>(
        terminal: &'a Terminal<TestBackend>,
        text: &str,
    ) -> &'a ratatui::buffer::Cell {
        let row = screen(terminal).lines().next().unwrap().to_string();
        let x = row[..row.find(text).unwrap()].chars().count() as u16;
        &terminal.backend().buffer()[(x, 0)]
    }

    #[test]
    fn status_bar_shows_rate_limits_coloured_by_headroom() {
        let mut app = App::detached();
        let mut terminal = Terminal::new(TestBackend::new(200, 10)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.limits_area.is_none());

        let cases = [
            (42.0, Color::DarkGray),
            (75.0, Color::Yellow),
            (90.0, Color::Red),
        ];
        for (used, colour) in cases {
            app.on_event(Event::RateLimits(RateLimits {
                primary: Some(window(used, 300)),
                secondary: Some(window(17.0, 10080)),
            }));
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let top = screen(&terminal);
            let expected = format!("5h {used:.0}% · wk 17% ");
            assert!(top.lines().next().unwrap().contains(&expected), "{top}");
            assert_eq!(status_cell(&terminal, "5h ").fg, colour);
            assert_eq!(status_cell(&terminal, "wk ").fg, Color::DarkGray);
        }
    }

    #[test]
    fn status_bar_shows_a_cache_break_until_the_next_clean_call() {
        let mut app = App::detached();
        let mut terminal = Terminal::new(TestBackend::new(200, 10)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!screen(&terminal).contains("cache break"));

        app.on_event(Event::Cache(Some(CacheBreak {
            field: "input[3]".to_string(),
            detail: "shrank from 5 to 3 items".to_string(),
        })));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let top = screen(&terminal);
        assert!(
            top.lines()
                .next()
                .unwrap()
                .contains("cache break: input[3] "),
            "{top}"
        );
        assert_eq!(status_cell(&terminal, "cache break").fg, Color::Red);

        app.on_event(Event::Cache(None));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!screen(&terminal).contains("cache break"));
    }

    #[test]
    fn status_bar_shows_a_stalled_cache_until_the_next_hit() {
        use crate::cache::Hit;

        let mut app = App::detached();
        let mut terminal = Terminal::new(TestBackend::new(200, 10)).unwrap();
        app.on_event(Event::CacheStalled(8));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("cache stalled "));
        assert_eq!(status_cell(&terminal, "cache stalled").fg, Color::Yellow);

        app.on_event(Event::CacheHit(Hit {
            expected_cached: Some(7936),
            hit_ratio: Some(1.0),
        }));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!screen(&terminal).contains("cache stalled"));
    }

    #[test]
    fn the_mode_chip_sits_on_the_bottom_bar_not_the_top() {
        let mut app = App::detached();
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let cases = [
            (Mode::Ask, "ask", Color::DarkGray),
            (Mode::Auto, "auto", Color::Yellow),
            (Mode::Bypass, "bypass", Color::Red),
        ];
        for pending in [None, Some(approval(None))] {
            app.pending = pending;
            for (mode, name, colour) in cases {
                app.mode = mode;
                terminal.draw(|frame| render(frame, &mut app)).unwrap();
                let screen = screen(&terminal);
                let chip = format!("{name} · shift+tab ");
                let bottom = screen.lines().next_back().unwrap();
                assert!(bottom.contains(&chip), "{screen}");
                let top = screen.lines().next().unwrap();
                assert!(!top.contains(name), "{screen}");

                let y = terminal.backend().buffer().area.height - 1;
                let x = bottom[..bottom.find(name).unwrap()].chars().count() as u16;
                assert_eq!(terminal.backend().buffer()[(x, y)].fg, colour);
            }
        }
        assert!(
            !screen(&terminal)
                .lines()
                .next()
                .unwrap()
                .contains("shift+tab"),
            "the top bar keeps its other hints but not the mode"
        );
    }

    #[test]
    fn hovering_the_rate_limits_shows_reset_times() {
        let mut app = App::detached();
        app.on_event(Event::RateLimits(RateLimits {
            primary: Some(window(10.0, 300)),
            secondary: None,
        }));
        let mut terminal = Terminal::new(TestBackend::new(200, 10)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!screen(&terminal).contains("resets"));

        let area = app.limits_area.unwrap();
        assert!(app.on_mouse(left(MouseEventKind::Moved, area.x + 1, 0)));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let top = screen(&terminal);
        assert!(
            top.lines().next().unwrap().contains("5h 10% resets "),
            "{top}"
        );

        assert!(app.on_mouse(left(MouseEventKind::Moved, 0, 0)));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!screen(&terminal).contains("resets"));
    }

    #[test]
    fn the_slash_menu_sits_between_the_transcript_and_the_prompt() {
        let mut app = App::detached();
        let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!screen(&terminal).contains("/compact"));

        for c in "/co".chars() {
            app.on_key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            ));
        }
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        for name in ["/compact", "/context", "/copy"] {
            assert!(shown.contains(name), "{shown}");
        }
        assert!(shown.contains("tab complete"), "{shown}");
        // The prompt keeps the bottom rows, with what was typed still in it.
        let rows: Vec<&str> = shown.lines().collect();
        let menu = rows.iter().position(|r| r.contains("/compact")).unwrap();
        let prompt = rows
            .iter()
            .position(|r| r.contains("\u{203a} /co"))
            .unwrap();
        assert!(menu < prompt, "{shown}");

        // The highlighted row is the first, and it moves with the arrow keys.
        let buffer = terminal.backend().buffer();
        let y = menu as u16;
        assert_eq!(buffer[(2, y)].bg, Color::Cyan);
        app.on_key(ratatui::crossterm::event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        ));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(2, y)].bg, Color::Reset);
        assert_eq!(buffer[(2, y + 1)].bg, Color::Cyan);
    }

    #[test]
    fn the_completion_is_drawn_grey_past_the_cursor() {
        let mut app = App::detached();
        let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
        for c in "/com".chars() {
            app.on_key(ratatui::crossterm::event::KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            ));
        }
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        let rows: Vec<&str> = shown.lines().collect();
        let y = rows
            .iter()
            .position(|r| r.contains("\u{203a} /compact"))
            .unwrap() as u16;

        // What was typed is drawn plain, the rest of the name grey after it.
        let x = app.input_area.unwrap().0.x;
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(x + 3, y)].symbol(), "m");
        assert_eq!(buffer[(x + 3, y)].fg, Color::Reset);
        assert_eq!(buffer[(x + 4, y)].symbol(), "p");
        assert_eq!(buffer[(x + 4, y)].fg, Color::DarkGray);
        assert_eq!(buffer[(x + 7, y)].fg, Color::DarkGray);
    }

    #[test]
    fn the_trust_question_takes_the_prompt_until_it_is_answered() {
        let mut app = App::detached();
        app.trust_gate = Some(TrustGate {
            root: "/Users/z/code/ripgrep".to_string(),
            rules: 2,
            mode: Mode::Auto,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        for part in [
            "do you trust this folder?",
            "/Users/z/code/ripgrep",
            "writes inside this project",
            "2 allow rules",
            "[y] trust it, use auto mode",
            "[n]o, stay in ask mode",
        ] {
            assert!(shown.contains(part), "{part} missing from {shown}");
        }
        // No prompt to type into while the question is up.
        assert!(app.input_area.is_none());
        assert!(!shown.contains("\u{203a} "), "{shown}");

        // The whole sentence fits, however narrow the terminal.
        let mut narrow = Terminal::new(TestBackend::new(34, 24)).unwrap();
        narrow.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&narrow);
        assert!(shown.contains("ship."), "{shown}");
        assert!(shown.contains("[y] trust"), "{shown}");
    }
}
