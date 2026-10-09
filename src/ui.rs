//! Rendering. Text is hard-wrapped here so the scroll offset can be computed exactly.

use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use std::collections::HashMap;
use std::ops::Range;
use std::time::{Duration, Instant};

use crate::app::{App, Entry, Source, TrustGate};
use crate::client::Usage;
use crate::commands::{self, Item};
use crate::input::Row;
use crate::limits::{self, RateLimits};
use crate::markdown::{self, Origin};
use crate::models::Picker;
use crate::permissions::Mode;
use crate::profile::{Method, Tokens};
use crate::search::Search;
use crate::session::{ChildRow, ChildState};
use crate::wrap::{Join, joined, wrap};

/// Rows a tool output shows until it is clicked open.
const COLLAPSED_LINES: usize = 3;

/// Lines the input grows to before it scrolls.
const MAX_INPUT_LINES: usize = 8;

/// Characters of the judged call the working row shows.
const JUDGING_CLIP: usize = 48;

/// Subagents the panel lists before it shows only the most recent ones.
const MAX_CHILD_ROWS: usize = 4;
/// Rows the queue panel shows before it says how many more are behind them.
const MAX_QUEUED_ROWS: usize = 3;
/// Steps the plan panel shows before it scrolls to the one in progress.
const MAX_PLAN_ROWS: usize = 6;

/// How long the note about a drag's copy stays on screen.
const COPIED_FOR: Duration = Duration::from_secs(3);

/// The ground the user's own words sit on: a shade up from the terminal's own, so a
/// prompt reads as a prompt rather than as something said in a colour of its own.
const USER_BG: Color = Color::Indexed(236);

/// The scrollbar track: there to say how far the transcript runs, faint enough that
/// the eye does not keep landing on it.
const TRACK: Color = Color::Indexed(238);

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// The mark a message of the model's own carries, in the same left margin a tool call's
/// sigil sits in. Unstyled, because the message is: the mark says where one starts, and
/// the margin it opens keeps the whole of it set in from everything around it.
const MESSAGE_MARK: &str = "⏺ ";

pub fn render(frame: &mut Frame, app: &mut App) {
    app.plan_area = None;
    app.monitor_area = None;
    draw(frame, app);
    // Last, so it sits over whatever the drag was made on.
    render_copied(frame, app);
    // After everything, so a link never runs under an overlay drawn over the transcript.
    render_links(frame, app);
}

/// The URLs and file paths in the transcript rows in view, as OSC 8 links, kept for
/// the mouse, and the web link under the pointer underlined in every row it covers.
fn render_links(frame: &mut Frame, app: &mut App) {
    let Some(area) = app.transcript_area else {
        app.links.clear();
        return;
    };
    let view = app.scroll..(app.scroll + area.height as usize).min(app.lines.len());
    let root = std::env::current_dir().unwrap_or_default();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let places = crate::links::Places {
        root: &root,
        home: home.as_deref(),
        files: !crate::clipboard::over_ssh(|name| std::env::var_os(name)),
    };
    app.links = crate::links::stamp(
        frame.buffer_mut(),
        area,
        view,
        &app.lines,
        &app.joins,
        &app.margins,
        &places,
    );
    if let Some(link) = app.hovered_link() {
        let buf = frame.buffer_mut();
        for (y, xs) in &link.spans {
            for x in xs.clone() {
                if let Some(cell) = buf.cell_mut((x, *y)) {
                    cell.modifier.insert(Modifier::UNDERLINED);
                }
            }
        }
    }
}

/// What a drag's copy leaves behind, beside where the drag ended so the eye is already
/// there, on a background so it reads as a note over the transcript and not a row of it.
fn render_copied(frame: &mut Frame, app: &App) {
    let Some(copied) = app.copied.filter(|c| c.at.elapsed() < COPIED_FOR) else {
        return;
    };
    let text = format!(" {} ", crate::clipboard::said(copied.chars, copied.of));
    let screen = frame.area();
    let width = (text.chars().count() as u16).min(screen.width);
    // Under the pointer, or over it at the last row, and never off the right edge.
    let y = match copied.cell.y + 1 < screen.bottom() {
        true => copied.cell.y + 1,
        false => copied.cell.y.saturating_sub(1),
    };
    let area = Rect {
        x: copied.cell.x.min(screen.right().saturating_sub(width)),
        y,
        width,
        height: 1,
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Span::styled(
            text,
            Style::new().fg(Color::Black).bg(Color::Cyan).bold(),
        )),
        area,
    );
}

fn draw(frame: &mut Frame, app: &mut App) {
    // The trust question takes the bottom area before anything else can, sized to what
    // it has to say.
    if let Some(gate) = app.trust_gate.clone() {
        let width = frame.area().width.saturating_sub(4).max(10) as usize;
        let body = wrap(&trust_text(&gate), width).len() as u16;
        let height = (body + 5).min(frame.area().height.saturating_sub(2)).max(4);
        let [transcript_area, bottom_area, status_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(height),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        render_status(frame, status_area, app);
        render_transcript(frame, transcript_area, app);
        app.input_area = None;
        app.buttons.clear();
        app.child_rows.clear();
        render_trust(frame, bottom_area, &gate);
        return;
    }
    if app.clear_pending {
        let text = "Also kill kept processes and dismiss all monitors?\n[y] clean slate   [n] conversation only   [Esc] cancel";
        let width = frame.area().width.saturating_sub(2).max(1) as usize;
        let height =
            (wrap(text, width).len() as u16 + 2).min(frame.area().height.saturating_sub(2));
        let [transcript_area, bottom_area, status_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(height),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        render_status(frame, status_area, app);
        render_transcript(frame, transcript_area, app);
        app.input_area = None;
        app.buttons.clear();
        app.child_rows.clear();
        frame.render_widget(Clear, bottom_area);
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: false }).block(
                Block::bordered()
                    .title(" clear conversation ")
                    .border_style(Style::new().fg(Color::Yellow)),
            ),
            bottom_area,
        );
        return;
    }
    // sudo's question takes the bottom area next, over an approval or the prompt.
    if let Some(request) = app.passwords.front() {
        let width = frame.area().width.saturating_sub(4).max(10) as usize;
        let body = wrap(&request.command, width).len() as u16;
        let height = (body + 5).min(frame.area().height.saturating_sub(2)).max(5);
        let [transcript_area, bottom_area, status_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(height),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        let lines = password_lines(
            app,
            bottom_area.width.saturating_sub(2).max(4) as usize,
            bottom_area.height.saturating_sub(2) as usize,
        );
        render_status(frame, status_area, app);
        render_transcript(frame, transcript_area, app);
        app.input_area = None;
        app.buttons.clear();
        app.child_rows.clear();
        let block = Block::bordered()
            .title(" sudo wants your password ")
            .border_style(Style::new().fg(Color::Yellow));
        let inner = block.inner(bottom_area);
        frame.render_widget(Clear, bottom_area);
        frame.render_widget(block, bottom_area);
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }
    // The `/model` picker and the history search take the prompt's place, sized to the
    // list they are showing. An approval that comes in meanwhile is drawn over them.
    let search = app.search.as_ref().filter(|_| app.pending.is_none());
    let overlay = app
        .picker
        .as_ref()
        .map(Picker::height)
        .or(search.map(Search::height));
    if let Some(height) = overlay {
        let height = height.min(frame.area().height.saturating_sub(2));
        let [transcript_area, bottom_area, status_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(height),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        render_status(frame, status_area, app);
        render_transcript(frame, transcript_area, app);
        app.input_area = None;
        app.buttons.clear();
        app.child_rows.clear();
        if let Some(picker) = &mut app.picker {
            picker.render(frame, bottom_area);
        } else if let Some(search) = &app.search {
            search.render(frame, bottom_area);
        }
        return;
    }
    let approval_height = app
        .pending
        .as_ref()
        .map(|pending| {
            let width = frame.area().width.saturating_sub(4).max(10) as usize;
            let lines = (wrap(&pending.command, width).len()
                + pending
                    .preview
                    .as_deref()
                    .map_or(0, |diff| diff_rows(diff, width).len())) as u16;
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

    // The turn's subagents sit above all of that, so the panel does not move as the
    // menu opens or the spinner comes and goes. Once the turn is over and every child
    // has ended there is nothing to watch, so the rows go rather than sitting between
    // the transcript and the prompt. Children run detached, so one can outlive its turn;
    // a pane that is open keeps them too, since the panel is its title and its way out.
    let children = app.children();
    let watching = app.working
        || app.inside.is_some()
        || children.iter().any(|c| c.state == ChildState::Running);
    let children_height = match children.len() {
        0 => 0,
        _ if !watching => 0,
        rows => rows.min(MAX_CHILD_ROWS) as u16 + 2,
    };

    // Prompts waiting on the turn sit with the prompt box, not in the transcript: they
    // are not part of the conversation until their turn starts, and the transcript is
    // what the model is being shown.
    let queued_height = match app.queued.len() {
        0 => 0,
        rows => rows.min(MAX_QUEUED_ROWS) as u16 + 2,
    };

    // Goal-owned progress stays visible after completion; standalone lists hide at idle.
    let plan = app.plan().filter(|plan| {
        app.goal().is_some_and(|g| g.plan.as_ref() == Some(plan)) || app.working || !plan.done()
    });
    let plan_height = plan.as_ref().map_or(0, |plan| {
        if app.plan_collapsed {
            1
        } else {
            plan.steps.len().min(MAX_PLAN_ROWS) as u16 + 2
        }
    });

    let monitors = app.monitors();
    let monitor_height = if app.monitors_collapsed && !monitors.is_empty() {
        1
    } else {
        crate::monitorview::height(&monitors, frame.area().height)
    };

    let [
        transcript_area,
        plan_area,
        monitor_area,
        children_area,
        queued_area,
        menu_area,
        working_area,
        bottom_area,
        status_area,
    ] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(plan_height),
        Constraint::Length(monitor_height),
        Constraint::Length(children_height),
        Constraint::Length(queued_height),
        Constraint::Length(menu_height),
        Constraint::Length(working_height),
        Constraint::Length(approval_height),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    app.plan_area = (plan_area.height > 0).then_some(plan_area);
    app.monitor_area = (monitor_area.height > 0).then_some(monitor_area);
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
        // The pane covered those rows, so they go back on top.
        render_plan(frame, plan_area, plan.as_ref(), app.plan_collapsed);
        crate::monitorview::render(frame, monitor_area, &monitors, app.monitors_collapsed);
        render_children(frame, children_area, app, &children);
        render_queued(frame, queued_area, app);
        render_working(frame, working_area, app);
        if app.pending.is_some() {
            render_approval(frame, bottom_area, app);
        }
        return;
    }
    match &mut app.bg {
        // The background list covers the transcript, so nothing of it is there to click.
        Some(bg) => {
            bg.render(frame, transcript_area);
            app.rows.clear();
            app.lines.clear();
            app.transcript_area = None;
            app.scrollbar = None;
        }
        None => render_transcript(frame, transcript_area, app),
    }
    render_plan(frame, plan_area, plan.as_ref(), app.plan_collapsed);
    crate::monitorview::render(frame, monitor_area, &monitors, app.monitors_collapsed);
    render_children(frame, children_area, app, &children);
    render_queued(frame, queued_area, app);
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

/// The checklist the model keeps with `update_plan`, with the step in progress kept in
/// view when there are more steps than rows.
fn render_plan(frame: &mut Frame, area: Rect, plan: Option<&crate::plan::Plan>, collapsed: bool) {
    use crate::plan::Status;
    let Some(plan) = plan.filter(|_| area.height > 0) else {
        return;
    };
    let dim = Style::new().fg(Color::DarkGray);
    let mut block = Block::bordered().border_style(dim).title(Line::styled(
        format!(
            " plan {}/{}{} ",
            plan.completed() + plan.skipped(),
            plan.steps.len(),
            if plan.skipped() > 0 {
                format!(", {} skipped", plan.skipped())
            } else {
                String::new()
            }
        ),
        dim,
    ));
    block =
        block.title_top(Line::styled(if collapsed { " ▸ " } else { " ▾ " }, dim).right_aligned());
    if collapsed {
        frame.render_widget(block, area);
        return;
    }
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let rows = inner.height as usize;
    let width = inner.width as usize;
    // The step to keep in view: the one in progress, else the first still to do.
    let focus = plan
        .current()
        .or_else(|| plan.steps.iter().position(|s| !s.status.resolved()))
        .unwrap_or(plan.steps.len().saturating_sub(1));
    let top = focus
        .saturating_sub(1)
        .min(plan.steps.len().saturating_sub(rows));
    let lines: Vec<Line> = plan
        .steps
        .iter()
        .skip(top)
        .take(rows)
        .map(|step| {
            let (mark, mark_style, text_style) = match step.status {
                Status::Completed => ("✓", Style::new().fg(Color::Green), dim),
                Status::InProgress => ("▸", Style::new().fg(Color::Yellow), Style::new().bold()),
                Status::Pending => ("○", dim, Style::new()),
                Status::Skipped => ("⊘", dim, dim),
            };
            let head = format!(" {mark} ");
            let room = width.saturating_sub(head.chars().count()).max(1);
            Line::from(vec![
                Span::styled(head, mark_style),
                Span::styled(
                    clip(
                        &if step.reason.is_empty() {
                            step.step.clone()
                        } else {
                            format!("{}: {}", step.step, step.reason)
                        },
                        room,
                    ),
                    text_style,
                ),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The prompts waiting on the running turn. They are shown here rather than in the
/// transcript because that is what they are: typed, not yet said. Each joins the
/// transcript when its own turn starts, in the place the model's history puts it.
fn render_queued(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let dim = Style::new().fg(Color::DarkGray);
    let block = Block::bordered()
        .border_style(dim)
        .title(Line::styled(" queued ", dim))
        .title_bottom(
            Line::styled(" ↑ edits them · /queue clear drops them ", dim).right_aligned(),
        );
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let rows = inner.height as usize;
    let width = inner.width as usize;
    // A bordered block has no inside at all on a terminal two rows tall.
    if rows == 0 {
        return;
    }
    // The last row is spent saying how many did not fit, rather than dropping them
    // silently: what is waiting is the whole point of the panel.
    let shown = match app.queued.len() > rows {
        true => rows - 1,
        false => rows,
    };
    let mut lines: Vec<Line> = app
        .queued
        .iter()
        .take(shown)
        .enumerate()
        .map(|(at, text)| {
            let head = format!(" {}. ", at + 1);
            let room = width.saturating_sub(head.chars().count()).max(1);
            // A prompt of several lines is one row here; it is sent whole when it runs.
            Line::from(vec![
                Span::styled(head, dim),
                Span::styled(clip(text, room), dim),
            ])
        })
        .collect();
    if let Some(rest) = app.queued.len().checked_sub(shown).filter(|n| *n > 0) {
        lines.push(Line::styled(format!("    {rest} more"), dim));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// What a segment of the bar gives up its room for. Everything fits on a wide
/// terminal; on a narrow one the hint is what goes.
const HINT: u8 = 2;
const ALWAYS: u8 = 0;

/// The bottom bar: where the session is running, how full its context is, and what is
/// left of the rate-limit windows. It sits under the prompt so the transcript has the
/// whole screen above it to scroll through.
fn render_status(frame: &mut Frame, area: Rect, app: &mut App) {
    let area = render_chip(frame, area, app);
    let app: &App = app;
    // Where a click would go, in place of the bar while the pointer is on a link.
    if let Some(link) = app.hovered_link() {
        let line = Span::styled(format!(" ↗ {}", link.target), Style::new().fg(Color::Cyan));
        frame.render_widget(Paragraph::new(Line::from(line)), area);
        return;
    }
    // The user's own template, when they wrote one, clipped at the edge like any row.
    if let Some(template) = &app.statusline {
        let spans = template.render(&crate::statusline::values(app), area.width as usize);
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    let dim = Style::new().fg(Color::DarkGray);
    let (model, window, used) = app.status_context();
    // The effort rides along with the model, since `/model` can change either.
    let mut bar = vec![(
        ALWAYS,
        Span::styled(
            match crate::client::Provider::of(&model) {
                crate::client::Provider::Codex if app.inside.is_none() => {
                    let fast = if app.fast { " fast" } else { "" };
                    format!(" {model} {}{fast} ({} context)", app.effort, size(window))
                }
                _ => format!(" {model} ({} context)", size(window)),
            },
            dim,
        ),
    )];
    // Which branch the work is landing on, after the directory as `dir:branch`. A
    // checkout in another terminal moves it, so it is re-read on the tick rather than
    // read once at startup.
    let dir = std::env::current_dir()
        .ok()
        .and_then(|cwd| cwd.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    match app.branch.name() {
        Some(branch) => segment(&mut bar, Span::styled(format!("{dir}:{branch}"), dim)),
        None if !dir.is_empty() => segment(&mut bar, Span::styled(dir, dim)),
        None => {}
    }
    // How full the window is, from what the last call actually read: the number
    // compaction watches, and the only one here that says how much room is left.
    // While the prompt starts with `/compact-then`, the compacted copy it would run on.
    let fork = app.forked().filter(|_| app.inside.is_none());
    if let Some(used) = used {
        let percent = 100.0 * used as f64 / window as f64;
        let label = if fork.is_some() { "fork ctx" } else { "ctx" };
        segment(
            &mut bar,
            Span::styled(format!("{label}:{percent:.0}%"), headroom(percent)),
        );
    }
    if let Some(goal) = app.goal() {
        segment(
            &mut bar,
            Span::styled(
                format!("goal {}", goal.state.label()),
                match goal.active() {
                    true => Style::new().fg(Color::Cyan),
                    false => dim,
                },
            ),
        );
    }
    match app.scheduled() {
        0 => {}
        n => segment(&mut bar, Span::styled(format!("{n} scheduled"), dim)),
    }
    if let Some(field) = &app.cache_break {
        segment(
            &mut bar,
            Span::styled(
                format!("cache break: {field}"),
                Style::new().fg(Color::Red).bold(),
            ),
        );
    }
    if app.cache_stalled {
        segment(
            &mut bar,
            Span::styled("cache stalled", Style::new().fg(Color::Yellow).bold()),
        );
    }
    if let Some(percent) = app.cache_miss {
        segment(
            &mut bar,
            Span::styled(
                format!("cache miss {percent:.0}%"),
                Style::new().fg(Color::Yellow).bold(),
            ),
        );
    }
    // The copy has never been sent, so nothing of it past the first message is cached.
    if let Some(tokens) = fork {
        segment(
            &mut bar,
            Span::styled(
                format!("fork uncached: ~{} tokens", compact(tokens)),
                Style::new().fg(Color::Yellow),
            ),
        );
    }
    if let Some(left) = app.cache_left().filter(|_| fork.is_none()) {
        segment(
            &mut bar,
            Span::styled(
                format!("cache expires in {}", clock(left)),
                Style::new().fg(Color::Yellow),
            ),
        );
    }
    if let Some(tokens) = app.cold_tokens().filter(|_| fork.is_none()) {
        segment(
            &mut bar,
            Span::styled(
                format!("cache expired: /clear to save ~{} tokens", compact(tokens)),
                Style::new().fg(Color::Yellow),
            ),
        );
    }
    if let Some(found) = app.rate_limits {
        for span in limit_spans(&found) {
            segment(&mut bar, span);
        }
    }
    bar.push((
        HINT,
        Span::styled(
            match app.pending.is_some() {
                true => "   y yes · n no · or click a choice",
                // The rest of the keys live in /help rather than along the bar.
                false => "   / for commands",
            },
            dim,
        ),
    ));
    // A terminal too narrow for all of it keeps the branch, the fill and the windows:
    // the bar is read for where the session stands, and the keys are also in /help.
    let width = |bar: &[(u8, Span)]| bar.iter().map(|(_, span)| span.width()).sum::<usize>();
    if width(&bar) > area.width as usize {
        bar.retain(|(drop, _)| *drop < HINT);
    }
    let spans: Vec<Span> = bar.into_iter().map(|(_, span)| span).collect();
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// `3 processes` at the right end of the bar while anything runs in the background, kept in
/// `app.chip` so a click on it opens the list. Returns what is left of the bar.
fn render_chip(frame: &mut Frame, area: Rect, app: &mut App) -> Rect {
    if app.background == 0 {
        app.chip = None;
        return area;
    }
    let noun = if app.background == 1 {
        "process"
    } else {
        "processes"
    };
    let text = format!(" {} {noun} ", app.background);
    let width = (text.chars().count() as u16).min(area.width);
    let [rest, chip] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(width)]).areas(area);
    app.chip = Some(chip);
    let style = Style::new()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let style = match app.pointing_at(chip) || app.bg.is_some() {
        true => style.add_modifier(Modifier::UNDERLINED),
        false => style,
    };
    frame.render_widget(Paragraph::new(Span::styled(text, style)), chip);
    rest
}

/// The spinner row above the prompt, drawn only while a turn is actually running.
fn render_working(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let mut spans = vec![Span::styled(
        format!(
            " {} {}...",
            SPINNER[app.spinner % SPINNER.len()],
            app.verb()
        ),
        Style::new().fg(Color::Yellow),
    )];
    let dim = Style::new().fg(Color::DarkGray);
    if let Some(call) = &app.judging {
        spans.push(Span::styled(
            format!(" · auto mode is checking {}", clip(call, JUDGING_CLIP)),
            Style::new().fg(Color::Cyan),
        ));
    }
    let now = Instant::now();
    // Before the first token there is the prompt being read. No backend says how far
    // through one it is, so what is shown is the size of it and how long it has been:
    // enough to tell a big prompt from a stalled call, which is what the wait is for.
    if let Some((prompt, waited)) = app.speed.reading_prompt(now) {
        spans.push(Span::styled(
            format!(
                " · reading {} in · {:.0}s",
                compact(prompt),
                waited.as_secs_f64()
            ),
            dim,
        ));
    } else {
        // Once the first token is back that wait has a rate of its own, and it stays up
        // beside the answer's: the two halves of a call are read together.
        if let Some(prompt) = app.speed.prompt_rate() {
            spans.push(Span::styled(
                format!(" · {} in/s", compact(prompt as u64)),
                dim,
            ));
        }
        if let Some(rate) = app.speed.rate(now) {
            spans.push(Span::styled(format!(" · {rate:.0} tok/s"), dim));
        }
    }
    if !app.queued.is_empty() {
        spans.push(Span::styled(format!(" · {} queued", app.queued.len()), dim));
    }
    spans.push(Span::styled(" · ctrl+c interrupt", dim));
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A span on the bar, after a ` | ` when it is not the first.
fn segment(bar: &mut Vec<(u8, Span<'static>)>, span: Span<'static>) {
    if !bar.is_empty() {
        bar.push((
            ALWAYS,
            Span::styled(" | ", Style::new().fg(Color::DarkGray)),
        ));
    }
    bar.push((ALWAYS, span));
}

/// A context window's size as the bar names it: `272k`, `1M`, `262.1k`.
fn size(tokens: u64) -> String {
    compact(tokens).replace(".0", "")
}

/// `5h:42% resets@03:10`, `7d:17% resets@Fri 09:00`, `credits:8.3k/10.0k`: each
/// window's use and the local time it comes back, coloured by how close it is to its
/// limit, then the credits left where there are some to count. `5h:none` stands in for
/// a short window the plan does not have.
fn limit_spans(found: &RateLimits) -> Vec<Span<'static>> {
    let now = chrono::Local::now();
    let mut spans = Vec::new();
    // A plan with no 5h cap says so, rather than leaving the reader to wonder.
    if found.short().is_none() && found.long().is_some() {
        spans.push(Span::styled("5h:none", Style::new().fg(Color::DarkGray)));
    }
    let mut windows: Vec<_> = found.windows().collect();
    windows.sort_by_key(|w| w.window_minutes.unwrap_or(u64::MAX));
    for window in windows {
        let mut text = format!("{}:{:.0}%", window.label(), window.used_percent);
        if let Some(at) = window.resets_at_clock(now) {
            text.push_str(&format!(" resets@{at}"));
        }
        spans.push(Span::styled(text, headroom(window.used_percent)));
    }
    if let Some(credits) = found.credits.filter(|c| !c.unlimited) {
        spans.push(Span::styled(
            format!("credits:{}", credits.amount()),
            headroom(credits.used_percent().unwrap_or(0.0)),
        ));
    }
    spans
}

/// How a percentage used is drawn, for the context fill and the limit windows alike.
fn headroom(used_percent: f64) -> Style {
    if used_percent >= limits::ALERT {
        Style::new().fg(Color::Red).bold()
    } else if used_percent >= limits::WARN {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new().fg(Color::DarkGray)
    }
}

/// `text` cut to `max` characters, with an ellipsis when it had more. Callers work out
/// their room by subtraction, so `max` can reach zero on a narrow terminal.
pub(crate) fn clip(text: &str, max: usize) -> String {
    let flat = text.replace('\n', " ");
    match flat.chars().count() > max {
        true => flat.chars().take(max.saturating_sub(1)).collect::<String>() + "…",
        false => flat,
    }
}

/// A countdown as `4:07`, rounded up so it never shows `0:00` while time is left.
pub fn clock(left: Duration) -> String {
    let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
    format!("{}:{:02}", secs / 60, secs % 60)
}

pub fn compact(n: u64) -> String {
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
    let mut cache = std::mem::take(&mut app.drawn);
    let entries = app.entries();
    cache.fit(entries.list.len());
    let mut spans = Vec::with_capacity(entries.list.len());
    let mut plains: Vec<String> = Vec::new();
    let mut joins: Vec<Join> = Vec::new();
    let mut margins: Vec<usize> = Vec::new();
    let mut sources: Vec<Option<Source>> = Vec::new();
    let mut copies = Vec::new();
    let mut folds = HashMap::new();
    // Each drawn entry and how many of its cached rows it shows.
    let mut parts: Vec<(usize, usize)> = Vec::new();
    for (index, entry) in entries.list.iter().enumerate() {
        let start = plains.len();
        // Only the last entry can be run again: anything after it has moved the history
        // on, and a turn of its own is already running.
        let retryable = index + 1 == entries.list.len() && !app.working;
        // A shell command's output opens and closes with the command above it.
        let owner = match index
            .checked_sub(1)
            .map(|above| (above, &entries.list[above]))
        {
            Some((above, command)) if shell_result(command, entry).is_some() => above,
            _ => index,
        };
        let expanded = app.expanded.contains(&owner);
        if owner != index && !expanded {
            folds.insert(index, owner);
            continue;
        }
        let result = entries
            .list
            .get(index + 1)
            .and_then(|next| shell_result(entry, next));
        let rows = cache.rows(index, entry, width, expanded, retryable, result);
        // A diff hangs from the command that made it, with no separator between them.
        let hangs = matches!(entries.list.get(index + 1), Some(Entry::Diff(_)));
        let shown = rows.lines.len() - usize::from(hangs);
        plains.extend(rows.plains[..shown].iter().cloned());
        joins.extend_from_slice(&rows.joins[..shown]);
        margins.extend_from_slice(&rows.margins[..shown]);
        sources.extend(rows.origins[..shown].iter().map(|row| {
            row.clone().map(|origins| Source {
                entry: index,
                origins,
            })
        }));
        copies.extend(
            rows.copies
                .iter()
                .map(|(row, chars, code)| (start + row, chars.clone(), code.clone())),
        );
        if rows.folded || owner != index {
            folds.insert(index, owner);
        }
        parts.push((index, shown));
        // The blank separator line belongs to no entry.
        spans.push((start..plains.len() - usize::from(!hangs), index));
    }
    drop(entries);
    app.folds = folds;

    let total = plains.len();
    let height = area.height as usize;
    app.page = height.saturating_sub(1).max(1);
    let max_scroll = total.saturating_sub(height);
    if app.follow {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
    }
    app.max_scroll = max_scroll;
    app.rows = row_map(&spans, app.scroll, area);
    app.transcript_area = Some(text_area);
    app.lines = plains;
    app.joins = joins;
    app.margins = margins;
    app.sources = sources;
    app.copies = copies;
    app.rehover();

    // Only the rows in view are cloned out of the cache. Scrolling by leaving out the
    // rows above the view, rather than by the widget's own offset, which is a u16, also
    // keeps a transcript longer than 65535 rows from wrapping back to the top of itself.
    let view = app.scroll..(app.scroll + height).min(total);
    let mut lines: Vec<Line> = Vec::with_capacity(view.len());
    let mut at = 0;
    for (index, shown) in parts {
        let rows = &cache.slots[index].as_ref().expect("drawn above").1;
        let from = view.start.max(at) - at;
        let to = view.end.min(at + shown).saturating_sub(at);
        if from < to {
            lines.extend(rows.lines[from..to].iter().cloned());
        }
        at += shown;
        if at >= view.end {
            break;
        }
    }
    app.drawn = cache;

    if let Some(selection) = app.selection {
        for (offset, line) in lines.iter_mut().enumerate() {
            let index = view.start + offset;
            let Some(range) = selection.on_line(index, app.lines[index].chars().count()) else {
                continue;
            };
            *line = highlight(std::mem::take(line), range);
        }
    }

    frame.render_widget(Paragraph::new(lines), text_area);
    render_badges(frame, text_area, app);

    app.scrollbar = (max_scroll > 0).then_some(bar_area);
    if max_scroll > 0 {
        // Positions run 0..=max_scroll, so the thumb reaches the bottom when following.
        let mut state = ScrollbarState::new(max_scroll + 1)
            .position(app.scroll)
            .viewport_content_length(height);
        // A thin thumb against the screen edge, in the same greys as the rest of the
        // furniture: the bar says where in the transcript you are, and a full block in
        // the terminal's brightest white said it louder than the text beside it.
        let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .track_style(Style::new().fg(TRACK))
            .thumb_symbol("▐")
            .thumb_style(Style::new().fg(Color::DarkGray));
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

/// Badges of hovered, pinned or (with ctrl+t) all entries, right-aligned on the blank
/// row under each entry, or on its last visible row when that blank is not in view.
fn render_badges(frame: &mut Frame, area: Rect, app: &App) {
    let entries = app.entries();
    for (rows, entry) in &app.rows {
        let shown = app.all_badges || app.hover == Some(*entry) || app.pinned.contains(entry);
        let Some(text) = entries.tokens.get(entry).filter(|_| shown).and_then(badge) else {
            continue;
        };
        // The separator under the entry belongs to no entry, so a badge there covers no
        // text being read. An entry clipped by the bottom edge has none in view, and the
        // badge goes back on its last row rather than the transcript moving to make room.
        let under =
            (rows.end < area.bottom() && blank_row(app, area, rows.end)).then_some(rows.end);
        // A badge inside an entry with a ground of its own keeps that ground, so it does
        // not punch a hole in the block. The separator below is outside the block.
        let style = match (under, entries.list.get(*entry)) {
            (None, Some(Entry::User(_))) => Style::new().fg(Color::DarkGray).bg(USER_BG),
            _ => Style::new().fg(Color::DarkGray),
        };
        let line = Line::from(Span::styled(format!(" {text}"), style));
        let width = (line.width() as u16).min(area.width);
        let spot = Rect::new(
            area.right() - width,
            under.unwrap_or(rows.end - 1),
            width,
            1,
        );
        frame.render_widget(Clear, spot);
        frame.render_widget(Paragraph::new(line), spot);
    }
}

/// Is the screen row empty text? The separator between two entries is; a row the bottom
/// edge cut an entry off at is not.
fn blank_row(app: &App, area: Rect, row: u16) -> bool {
    let line = app.scroll + (row - area.y) as usize;
    app.lines
        .get(line)
        .is_some_and(|text| text.trim().is_empty())
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

/// The sigil a tool call is drawn with, and its colour. Only `bash` runs a shell
/// command, so only `bash` gets a shell prompt; yellow is for the tools that change the
/// machine, cyan for the ones that only bring something in.
fn tool_mark(tool: &str) -> (&'static str, Color) {
    use crate::tools::{agent, bash, edit, image_gen, patch, read, skill, view_image, write};
    match tool {
        bash::NAME => ("$ ", Color::Yellow),
        write::NAME | edit::NAME | patch::NAME | image_gen::NAME => ("✎ ", Color::Yellow),
        read::NAME | view_image::NAME => ("▸ ", Color::Cyan),
        skill::NAME => ("✦ ", Color::Magenta),
        agent::NAME => ("⇢ ", Color::Magenta),
        // The MCP tools, and whatever else an identity was given.
        _ => ("⚙ ", Color::Cyan),
    }
}

/// Rows an entry shows while it is closed, or `None` when it is never folded. Thinking
/// comes down to the one line that says it is thinking; output, a shell command and a
/// compaction summary to their first rows.
fn collapsed_rows(entry: &Entry) -> Option<usize> {
    match entry {
        Entry::Reasoning(_) => Some(1),
        Entry::Output(_) | Entry::Running { .. } | Entry::Summary(_) => Some(COLLAPSED_LINES),
        Entry::Command { tool, .. } if tool == crate::tools::bash::NAME => Some(COLLAPSED_LINES),
        _ => None,
    }
}

/// `entry` when it is how the shell command `command` ended: what it printed, or why
/// it never ran.
fn shell_result<'a>(command: &Entry, entry: &'a Entry) -> Option<&'a Entry> {
    match (command, entry) {
        (Entry::Command { tool, .. }, Entry::Output(_) | Entry::Rejected { .. })
            if tool == crate::tools::bash::NAME =>
        {
            Some(entry)
        }
        _ => None,
    }
}

/// How a shell command ended, as the row drawn under it, or `None` for a result the
/// shell did not write, such as one read back from an older session.
fn shell_status(result: &Entry) -> Option<(String, Color)> {
    match result {
        Entry::Output(output) => {
            crate::tools::bash::outcome(output).map(|outcome| match outcome.ok() {
                true => (format!("✓ {}", outcome.label()), Color::Green),
                false => (format!("✗ {}", outcome.label()), Color::Red),
            })
        }
        Entry::Rejected { by, .. } => Some((format!("✗ {}", by.label()), Color::Red)),
        _ => None,
    }
}

/// Each entry's rows as last drawn, so a frame lays out only the entries that changed
/// since the one before: markdown, syntax highlighting and wrapping cost the same however
/// little of the transcript moved, and the transcript only grows.
#[derive(Default)]
pub struct Drawn {
    slots: Vec<Option<(Key, Rows)>>,
    /// Entries laid out afresh rather than taken from the cache.
    #[cfg(test)]
    built: usize,
}

/// What an entry's rows were drawn from. The entry is fingerprinted rather than given a
/// revision because `Entries::list` is changed in place all over the session.
#[derive(PartialEq)]
struct Key {
    fingerprint: u64,
    width: usize,
    expanded: bool,
    retryable: bool,
}

impl Drawn {
    /// One slot per entry, dropping those past the end of a transcript that shrank.
    fn fit(&mut self, len: usize) {
        self.slots.truncate(len);
        self.slots.resize_with(len, || None);
    }

    fn rows(
        &mut self,
        index: usize,
        entry: &Entry,
        width: usize,
        expanded: bool,
        retryable: bool,
        result: Option<&Entry>,
    ) -> &Rows {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::hash::DefaultHasher::new();
        (entry, result).hash(&mut hasher);
        let key = Key {
            fingerprint: hasher.finish(),
            width,
            expanded,
            // Only a failure draws it, so the entry before a new one stays cached.
            retryable: retryable && matches!(entry, Entry::Failed(_)),
        };
        let slot = &mut self.slots[index];
        if slot.as_ref().is_none_or(|(drawn, _)| *drawn != key) {
            #[cfg(test)]
            {
                self.built += 1;
            }
            *slot = Some((key, entry_lines(entry, width, expanded, retryable, result)));
        }
        &slot.as_ref().expect("filled above").1
    }
}

/// What an entry draws: its rows and their text, how each one joins the row above it,
/// how many chars in front of each are drawn rather than text, where each char of a
/// markdown row came from, the code blocks a click copies and whether it has rows a click
/// folds away.
struct Rows {
    lines: Vec<Line<'static>>,
    plains: Vec<String>,
    joins: Vec<Join>,
    margins: Vec<usize>,
    origins: Vec<Option<Vec<Option<Origin>>>>,
    copies: Vec<(usize, Range<usize>, String)>,
    folded: bool,
}

/// An entry's rows plus a blank separator, and whether it has rows a click folds away.
/// Thinking and long tool output show only a little of themselves unless `expanded`.
/// `result` is how a shell command ended, which stays out of sight until then.
fn entry_lines(
    entry: &Entry,
    width: usize,
    expanded: bool,
    retryable: bool,
    result: Option<&Entry>,
) -> Rows {
    if let Entry::Diff(text) = entry {
        return diff_entry(text, width, expanded);
    }
    if let Entry::Assistant(text) | Entry::Commentary(text) = entry {
        // Commentary is narration on the way to the answer, so it sits back from it.
        let tone = match entry {
            Entry::Commentary(_) => Style::new().fg(Color::DarkGray),
            _ => Style::new(),
        };
        let lead = MESSAGE_MARK.chars().count();
        let indent = " ".repeat(lead);
        let rendered = markdown::render(text, width.saturating_sub(lead).max(4));
        let mut joins = rendered.joins;
        let mut margins: Vec<usize> = rendered.margins.iter().map(|m| m + lead).collect();
        let mut origins: Vec<Option<Vec<Option<Origin>>>> = rendered
            .origins
            .into_iter()
            .map(|row| Some([vec![None; lead], row].concat()))
            .collect();
        let copies = rendered
            .copies
            .into_iter()
            .map(|(row, chars, code)| (row, chars.start + lead..chars.end + lead, code))
            .collect();
        let mut lines: Vec<Line> = rendered
            .lines
            .into_iter()
            .enumerate()
            .map(|(i, mut line)| {
                line.spans.insert(
                    0,
                    match i {
                        0 => Span::raw(MESSAGE_MARK),
                        _ => Span::raw(indent.clone()),
                    },
                );
                line.patch_style(tone)
            })
            .collect();
        // A message that has only started still gets its mark, so the eye has somewhere
        // to land while the first words are on their way.
        if lines.is_empty() {
            lines.push(Line::from(MESSAGE_MARK));
            joins.push(Join::Newline);
            margins.push(lead);
            origins.push(None);
        }
        lines.push(Line::from(""));
        joins.push(Join::Newline);
        margins.push(0);
        origins.push(None);
        return Rows {
            plains: lines.iter().map(plain).collect(),
            lines,
            joins,
            margins,
            origins,
            copies,
            folded: false,
        };
    }
    let rejected: String;
    let (prefix, text, style): (&str, &str, Style) = match entry {
        Entry::User(t) => ("› ", t, Style::new().bg(USER_BG)),
        Entry::Assistant(t) | Entry::Commentary(t) => ("", t, Style::new()),
        Entry::Reasoning(t) => ("✻ ", t, Style::new().fg(Color::DarkGray).italic()),
        Entry::Command { tool, summary } => {
            let (prefix, colour) = tool_mark(tool);
            (prefix, summary.as_str(), Style::new().fg(colour))
        }
        Entry::Output(t) | Entry::Diff(t) => ("", t, Style::new().fg(Color::Gray)),
        Entry::Running { tail, .. } => ("", tail.trim_end(), Style::new().fg(Color::Gray)),
        Entry::Rejected { by, reason } => {
            rejected = match reason.is_empty() {
                true => by.label().to_string(),
                false => format!("{}: {reason}", by.label()),
            };
            ("✗ ", rejected.as_str(), Style::new().fg(Color::Red))
        }
        Entry::Error(t) | Entry::Failed(t) => ("! ", t, Style::new().fg(Color::Red).bold()),
        Entry::Info(t) => ("", t, Style::new().fg(Color::DarkGray)),
        Entry::Done(t) => ("\u{273b} ", t, Style::new().fg(Color::DarkGray)),
        Entry::Summary(t) => ("≡ ", t, Style::new().fg(Color::DarkGray)),
    };

    let lead = prefix.chars().count();
    let indent = " ".repeat(lead);
    let mut lines: Vec<Line> = Vec::new();
    let mut joins: Vec<Join> = Vec::new();
    let text = &crate::wrap::readable(text);
    let mut wrapped_lines = joined(text, width.saturating_sub(lead).max(4));
    let hidden = collapsed_rows(entry).map_or(0, |rows| wrapped_lines.len().saturating_sub(rows));
    let printed = result.map_or(0, |result| result.text().lines().count());
    if hidden > 0 && !expanded {
        match entry {
            // A running command shows its latest lines; everything else its first.
            Entry::Running { .. } => {
                wrapped_lines.drain(..hidden);
            }
            _ => wrapped_lines.truncate(wrapped_lines.len() - hidden),
        }
    }
    // Thinking stays on its one line, so what it hides is said at the end of it rather
    // than on a row of its own.
    let inline = hidden > 0 && !expanded && matches!(entry, Entry::Reasoning(_));
    if inline && let Some((first, _)) = wrapped_lines.first_mut() {
        let note = format!(" [+{hidden} lines]");
        let room = width.saturating_sub(lead + note.chars().count()).max(1);
        *first = format!("{}{note}", clip(first, room));
    }
    // A ground of its own is only a block if it runs the width of the transcript, so the
    // rows under one are filled out rather than ending where the text does.
    let ground = matches!(entry, Entry::User(_));
    let tucked = result.is_some() && !expanded;
    for (i, (wrapped, join)) in wrapped_lines.into_iter().enumerate() {
        let lead = if i == 0 {
            prefix.to_string()
        } else {
            indent.clone()
        };
        let mut row = format!("{lead}{wrapped}");
        if ground {
            let pad = width.saturating_sub(crate::wrap::width(&row));
            row.push_str(&" ".repeat(pad));
        }
        lines.push(Line::from(Span::styled(row, style)));
        // The entry starts under the one before it, whatever the wrap made of the rest.
        joins.push(match i {
            0 => Join::Newline,
            _ => join,
        });
    }
    // Every row so far is the entry's text behind its mark or indent; the notes below
    // are the harness's, so a copy takes them whole.
    let mut margins = vec![lead; lines.len()];
    // A finished shell command says how it ended on the row below, and what that row
    // hides, counting the command's own folded rows in with what it printed.
    if let Some(result) = result {
        let hides = hidden + printed;
        let note = (tucked && hides > 0).then(|| format!("[+{hides} lines]"));
        let mut spans = Vec::new();
        if let Some((label, colour)) = shell_status(result) {
            spans.push(Span::styled(label, Style::new().fg(colour)));
        }
        if let Some(note) = note {
            let gap = if spans.is_empty() { "" } else { " " };
            spans.push(Span::styled(
                format!("{gap}{note}"),
                Style::new().fg(Color::DarkGray),
            ));
        }
        if !spans.is_empty() {
            spans.insert(0, Span::raw(indent.clone()));
            lines.push(Line::from(spans));
            joins.push(Join::Newline);
        }
    }
    // A running command says how far it has got instead, on the row below.
    if hidden > 0 && !inline && !tucked && !matches!(entry, Entry::Running { .. }) {
        lines.push(Line::from(Span::styled(
            match expanded {
                true => format!("{indent}[collapse]"),
                false => format!("{indent}[+{hidden} lines]"),
            },
            Style::new().fg(Color::DarkGray),
        )));
        joins.push(Join::Newline);
    }
    // Said on a row of its own, the way a fold says what it hides, rather than inside the
    // failure, which is the backend's words and is what a copy of the entry should carry.
    if matches!(entry, Entry::Failed(_)) && retryable {
        lines.push(Line::from(Span::styled(
            format!("{indent}[click to retry]"),
            Style::new().fg(Color::DarkGray),
        )));
        joins.push(Join::Newline);
    }
    if let Entry::Running { tail, lines: done } = entry {
        let total = done + usize::from(!tail.is_empty() && !tail.ends_with('\n'));
        lines.push(Line::from(Span::styled(
            format!("[running, {total} lines]"),
            Style::new().fg(Color::DarkGray),
        )));
        joins.push(Join::Newline);
    }
    lines.push(Line::from(""));
    joins.push(Join::Newline);
    margins.resize(lines.len(), 0);
    Rows {
        origins: vec![None; lines.len()],
        plains: lines.iter().map(plain).collect(),
        lines,
        joins,
        margins,
        copies: Vec::new(),
        folded: hidden > 0 || printed > 0,
    }
}

/// The rows of a unified diff cut to `width`, each coloured as the line it came from.
/// Cut rather than word wrapped, which would drop the indentation code is read by.
fn diff_rows(text: &str, width: usize) -> Vec<(Line<'static>, Join)> {
    let text = crate::wrap::readable(text);
    let mut rows = Vec::new();
    for (kind, line) in crate::diff::classify(&text) {
        let style = crate::diff::style(kind);
        let line = line.replace('\t', "    ");
        let mut rest = line.as_str();
        let mut join = Join::Newline;
        loop {
            let mut cut = crate::wrap::split_at_width(rest, width.max(1));
            // A character wider than the row still has to go somewhere.
            if cut == 0 {
                cut = rest.chars().next().map_or(0, char::len_utf8);
            }
            rows.push((
                Line::from(Span::styled(rest[..cut].to_string(), style)),
                join,
            ));
            join = Join::Split;
            rest = &rest[cut..];
            if rest.is_empty() {
                break;
            }
        }
    }
    rows
}

/// The diff an edit or write made, under its command: one row saying how much it changed
/// until a click opens it.
fn diff_entry(text: &str, width: usize, expanded: bool) -> Rows {
    let indent = "  ";
    let mut lines = Vec::new();
    let mut joins = Vec::new();
    if expanded {
        for (mut line, join) in diff_rows(text, width.saturating_sub(indent.len()).max(4)) {
            line.spans.insert(0, Span::raw(indent));
            lines.push(line);
            joins.push(join);
        }
    }
    let mut margins = vec![indent.len(); lines.len()];
    let count = |kind| {
        crate::diff::classify(text)
            .iter()
            .filter(|(k, _)| *k == kind)
            .count()
    };
    let note = match expanded {
        true => "[collapse]".to_string(),
        false => format!(
            "[diff +{} -{}]",
            count(crate::diff::LineKind::Added),
            count(crate::diff::LineKind::Removed)
        ),
    };
    lines.push(Line::from(Span::styled(
        format!("{indent}{note}"),
        Style::new().fg(Color::DarkGray),
    )));
    lines.push(Line::from(""));
    joins.extend([Join::Newline, Join::Newline]);
    margins.resize(lines.len(), 0);
    Rows {
        origins: vec![None; lines.len()],
        plains: lines.iter().map(plain).collect(),
        lines,
        joins,
        margins,
        copies: Vec::new(),
        folded: true,
    }
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
/// The turn's subagents, one row each, above the prompt. Each row's area is recorded in
/// `app.child_rows` so a click on it opens that child's pane.
fn render_children(frame: &mut Frame, area: Rect, app: &mut App, children: &[ChildRow]) {
    app.child_rows.clear();
    if area.height == 0 {
        return;
    }
    let dim = Style::new().fg(Color::DarkGray);
    let open = app.inside.as_ref().map(|inside| inside.id.as_str());
    let hint = match open {
        Some(_) => " ctrl+o next · ctrl+x stop · esc close ",
        None => " ctrl+o opens one · click to read ",
    };
    let block = Block::bordered()
        .border_style(dim)
        .title(Line::styled(" subagents ", dim))
        .title_bottom(Line::styled(hint, dim).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let rows = inner.height as usize;
    // The running ones are the newest, so it is the tail that is worth the room.
    let top = children.len().saturating_sub(rows);
    let width = inner.width as usize;
    let lines: Vec<Line> = children
        .iter()
        .enumerate()
        .skip(top)
        .map(|(at, child)| {
            let row = Rect {
                y: inner.y + (at - top) as u16,
                height: 1,
                ..inner
            };
            app.child_rows.push((row, child.id.clone()));
            child_row(child, width, app.spinner, open == Some(&child.id))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// What the open pane's row offers at its right end, and what a click on it does.
const CLOSE: &str = " ✕ close ";

/// One subagent row: how it is doing, its id, who it runs as and what it was sent for.
/// A running child says something every few seconds, so silence past this is worth
/// showing: it is the only thing that separates a long build from a wedged retry loop.
const QUIET: u64 = 20;

/// How long a running child has been quiet, once that is long enough to mean anything.
fn quiet(child: &ChildRow) -> String {
    if child.state != ChildState::Running {
        return String::new();
    }
    let secs = child.idle().as_secs();
    if secs < QUIET {
        return String::new();
    }
    match (secs / 3600, (secs % 3600) / 60, secs % 60) {
        (0, 0, s) => format!(" · quiet {s}s"),
        (0, m, _) => format!(" · quiet {m}m"),
        (h, m, _) => format!(" · quiet {h}h{m}m"),
    }
}

fn child_row(child: &ChildRow, width: usize, spinner: usize, open: bool) -> Line<'static> {
    let (mark, colour) = match child.state {
        ChildState::Running => (SPINNER[spinner % SPINNER.len()], Color::Yellow),
        ChildState::Done => ("✓", Color::Green),
        ChildState::Failed => ("✗", Color::Red),
    };
    let head = format!(" {mark} {} ", child.id);
    let quiet = quiet(child);
    let mut tail = format!(
        "{} · {} · {}",
        child.identity, child.model, child.description
    );
    // The open row is the way back out, which nothing said until it said so: the close
    // reads as a button and the whole row is what a click lands on.
    let close = if open { CLOSE } else { "" };
    let room =
        width.saturating_sub(head.chars().count() + close.chars().count() + quiet.chars().count());
    tail = clip(&tail, room) + &quiet;
    let room = room + quiet.chars().count();
    let pad = room.saturating_sub(tail.chars().count());
    if open {
        // The open pane's row is the transcript's title, so it reads as selected.
        return Line::styled(
            format!("{head}{tail}{}{close}", " ".repeat(pad)),
            Style::new().fg(Color::Black).bg(Color::Cyan),
        );
    }
    Line::from(vec![
        Span::styled(head, Style::new().fg(colour)),
        Span::styled(tail, Style::new().fg(Color::DarkGray)),
    ])
}

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
    // Inside a pane the prompt goes to that child, so it says so where the eye lands.
    if let Some(inside) = &app.inside {
        block = block.title(Line::styled(
            format!(" to {} · {} ", inside.identity, inside.description),
            Style::new().fg(Color::Cyan),
        ));
    }
    #[cfg(feature = "dictation")]
    if let Some(dictation) = &app.dictation {
        use crate::dictation::Status;
        match dictation.status() {
            Status::Idle => {}
            Status::Recording => {
                block = block.title(Line::styled(
                    " ● recording · ctrl+space stops · esc drops ",
                    Style::new().fg(Color::Red),
                ))
            }
            Status::Transcribing => {
                block = block.title(Line::styled(
                    " transcribing ",
                    Style::new().fg(Color::DarkGray),
                ))
            }
        }
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

/// What a remember choice says once its key has been pressed.
fn confirm_hint(armed: bool) -> Span<'static> {
    match armed {
        true => Span::styled("  again to save", Style::new().fg(Color::Yellow).bold()),
        false => Span::raw(""),
    }
}

/// The approval prompt. Each `[k]` choice is recorded in `app.buttons` so a click on
/// it acts exactly as pressing `k`.
fn render_approval(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(pending) = &app.pending else {
        return;
    };
    if app.approval_shown.is_none_or(|(id, _)| id != pending.id) {
        app.approval_shown = Some((pending.id, None));
        app.armed = None;
    }
    let scroll = app.approval_scroll;
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
            confirm_hint(app.armed.is_some_and(|(k, _)| k == 'a')),
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
            confirm_hint(app.armed.is_some_and(|(k, _)| k == 'p')),
        ]));
    }

    let width = inner.width.max(4) as usize;
    let mut all: Vec<Line> = wrap(&pending.command, width)
        .into_iter()
        .map(|l| Line::from(Span::styled(l, Style::new().fg(Color::Yellow))))
        .collect();
    // Part of what is approved, so it scrolls with the command and `y` waits for its end.
    if let Some(diff) = &pending.preview {
        all.extend(diff_rows(diff, width).into_iter().map(|(line, _)| line));
    }
    let room = inner.height.saturating_sub(2 + options.len() as u16) as usize;
    let (body, start) = match all.len() > room {
        // The marker takes a row of the room, so the body gives one up.
        true => (room.saturating_sub(1).max(1), scroll),
        false => (all.len(), 0),
    };
    let start = start.min(all.len() - body);
    let end = start + body;
    let seen = end >= all.len();
    let mut lines: Vec<Line> = all[start..end].to_vec();
    if body < all.len() {
        let mut note = Vec::new();
        if start > 0 {
            note.push(format!("{start} above"));
        }
        if !seen {
            note.push(format!("+{} lines hidden", all.len() - end));
        }
        let hint = match seen {
            true => "up scrolls back",
            false => "down scrolls, y waits for the end",
        };
        lines.push(Line::styled(
            format!("[{}] {hint}", note.join(", ")),
            Style::new().fg(Color::Cyan),
        ));
    }
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
        key("[y]", if seen { Color::Green } else { Color::DarkGray }),
        Span::raw("es   "),
        key("[n]", Color::Red),
        Span::raw("o"),
    ]));
    lines.extend(options);
    frame.render_widget(Paragraph::new(lines), inner);
    app.approval_scroll = start;
    app.approval_seen = seen;
    app.approval_page = body;
    app.buttons = buttons
        .into_iter()
        .map(|(spot, k)| (spot.intersection(inner), KeyCode::Char(k)))
        .filter(|(spot, _)| !spot.is_empty())
        .collect();
}

/// The front password question in `rows` rows: the command it is for, which the end of
/// is kept when it does not fit, sudo's prompt, a mask of fixed length so the password's
/// length does not show, and the keys.
fn password_lines(app: &App, width: usize, rows: usize) -> Vec<Line<'static>> {
    let Some(request) = app.passwords.front() else {
        return Vec::new();
    };
    let key = |k: &'static str, color| Span::styled(k, Style::new().fg(color).bold());
    let prompt = match request.prompt.trim() {
        "" => "Password:".to_string(),
        prompt => prompt.to_string(),
    };
    let mask = match app.typed.is_empty() {
        true => "",
        false => "********",
    };
    let mut tail = vec![
        Line::from(vec![
            Span::styled(prompt, Style::new().fg(Color::DarkGray)),
            Span::raw(" "),
            Span::raw(mask),
            Span::styled("_", Style::new().fg(Color::Cyan)),
        ]),
        Line::from(""),
    ];
    let mut keys = vec![
        key("[enter]", Color::Green),
        Span::raw(" send it to sudo   "),
        key("[esc]", Color::Red),
        Span::raw(" refuse"),
    ];
    if app.passwords.len() > 1 {
        keys.push(Span::raw(format!(
            "   {} more waiting",
            app.passwords.len() - 1
        )));
    }
    tail.push(Line::from(keys));
    let command: Vec<String> = wrap(&request.command, width);
    let room = rows.saturating_sub(tail.len()).max(1);
    let skip = command.len().saturating_sub(room);
    let mut lines: Vec<Line> = command
        .into_iter()
        .skip(skip)
        .map(|l| Line::from(Span::styled(l, Style::new().fg(Color::Yellow))))
        .collect();
    if skip > 0 {
        lines[0] = Line::styled(
            format!("[{} lines above]", skip + 1),
            Style::new().fg(Color::Cyan),
        );
    }
    lines.extend(tail);
    lines
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Rejecter;
    use crate::cache::CacheBreak;
    use crate::permissions::Offers;
    use crate::session::{Approval, Event};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{
        KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::style::Modifier;
    use std::time::{Duration, Instant};

    fn usage(input: u64, cached: u64, output: u64, reasoning: u64) -> Usage {
        Usage {
            input,
            cached,
            cache_write: 0,
            output,
            reasoning,
        }
    }

    #[test]
    fn a_countdown_rounds_up_to_the_second() {
        assert_eq!(clock(Duration::from_secs(247)), "4:07");
        assert_eq!(clock(Duration::from_millis(300)), "0:01");
        assert_eq!(clock(Duration::from_secs(300)), "5:00");
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
    fn hovering_draws_the_badge_under_the_entry() {
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
        assert!(!row_text(&terminal, rows.end).contains("tokenized"));
        let last = row_text(&terminal, rows.end - 1);
        let moved = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 3,
            row: rows.start,
            modifiers: KeyModifiers::NONE,
        };
        assert!(app.on_mouse(moved));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        // The badge lands on the blank row under the entry, so the message reads the
        // same hovered as not, and nothing above or below it has moved.
        assert!(
            row_text(&terminal, rows.end)
                .trim_end()
                .ends_with(" in 5 (tokenized)"),
            "{:?}",
            row_text(&terminal, rows.end)
        );
        assert_eq!(row_text(&terminal, rows.end - 1), last);
        assert!(row_text(&terminal, rows.start).starts_with("› hello"));
        // That row is outside the user block, so it does not carry its ground.
        let buffer = terminal.backend().buffer();
        assert_ne!(buffer[(buffer.area.width - 1, rows.end)].bg, USER_BG);
    }

    /// A bordered block has no inside on a terminal this short, so the panel has no row
    /// to spend on the "N more" line and must not go looking for one.
    #[test]
    fn the_queued_panel_survives_a_terminal_with_no_room_for_it() {
        for rows in 1..=10 {
            let mut app = App::detached();
            app.working = true;
            app.queued = (1..=4).map(|i| format!("prompt {i}")).collect();
            let mut terminal = Terminal::new(TestBackend::new(40, rows)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
        }
    }

    #[test]
    fn a_failed_turn_offers_to_run_again_while_it_is_the_last_thing_said() {
        let mut app = App::detached();
        app.session()
            .publish(Event::TurnFailed("request failed: no route".to_string()));
        let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(shown.contains("! request failed: no route"), "{shown}");
        assert!(shown.contains("[click to retry]"), "{shown}");

        // Not while that turn runs: there is nothing to run again until it is over.
        app.working = true;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!screen(&terminal).contains("[click to retry]"));

        // Nor once anything has been said after it, which moves the history on.
        app.working = false;
        app.entries().push(Entry::User("never mind".to_string()));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(shown.contains("! request failed: no route"), "{shown}");
        assert!(!shown.contains("[click to retry]"), "{shown}");
    }

    #[test]
    fn a_message_is_marked_and_every_row_of_it_sits_in_from_the_mark() {
        let mut app = App::detached();
        app.entries().push(Entry::Assistant(
            "a message long enough that it wraps around".to_string(),
        ));
        // Tall enough that no scrollbar takes a column off the rows.
        let mut terminal = Terminal::new(TestBackend::new(24, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        let rows: Vec<&str> = shown
            .lines()
            .map(str::trim_end)
            .filter(|row| !row.is_empty())
            .collect();
        let start = rows
            .iter()
            .position(|row| row.starts_with("\u{23fa} "))
            .unwrap_or_else(|| panic!("{rows:?}"));
        assert_eq!(rows[start], "\u{23fa} a message long enough");
        // What the wrap made of the rest lines up under the message, not under the mark.
        assert_eq!(rows[start + 1], "  that it wraps around");
    }

    /// The widget's own scroll offset is a `u16`, so a transcript past 65535 rows used to
    /// wrap back to the top of itself. A long session reaches that.
    #[test]
    fn a_transcript_longer_than_a_u16_still_shows_its_end() {
        let mut app = App::detached();
        {
            let mut entries = app.entries();
            for i in 0..u16::MAX as usize + 40 {
                entries.push(Entry::Info(format!("line {i}")));
            }
        }
        let mut terminal = Terminal::new(TestBackend::new(20, 6)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.scroll > u16::MAX as usize, "{}", app.scroll);
        let shown = screen(&terminal);
        assert!(
            shown.contains(&format!("line {}", u16::MAX as usize + 39)),
            "{shown:?}"
        );
    }

    /// A tool's output and a model's answer are text nobody here wrote. What the terminal
    /// would act on never reaches a row, and what it would show as nothing is shown.
    #[test]
    fn a_hidden_payload_in_output_is_shown_and_an_escape_is_not_drawn() {
        let mut app = App::detached();
        app.entries().push(Entry::Output(
            "\u{1b}[31mred\u{1b}[0m \u{e0041}\u{e0042}".to_string(),
        ));
        app.entries()
            .push(Entry::Assistant("done \u{e0041} \u{1b}[2J".to_string()));
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(!shown.contains('\u{1b}'), "{shown:?}");
        assert!(!shown.contains('\u{e0041}'), "{shown:?}");
        assert!(shown.contains("··"), "{shown:?}");
        // The rows the selection and a copy read are the same string.
        assert!(
            app.lines.iter().all(|l| !l.contains('\u{1b}')),
            "{:?}",
            app.lines
        );
    }

    #[test]
    fn an_entry_cut_off_at_the_bottom_keeps_its_badge_on_its_last_row() {
        let mut app = App::detached();
        app.entries().push(Entry::Assistant("a line\n".repeat(40)));
        app.entries().tokens.insert(
            1,
            Tokens {
                input: Some(5),
                method: Method::Tokenized,
                ..Tokens::default()
            },
        );
        app.all_badges = true;
        app.follow = false;
        let mut terminal = Terminal::new(TestBackend::new(30, 10)).unwrap();
        // Scrolled past the intro, so the entry fills the view and runs past the bottom.
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        app.scroll = 8;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let (rows, _) = app.rows.iter().find(|(_, e)| *e == 1).cloned().unwrap();
        // The entry runs past the bottom, so there is no blank row under it in view.
        let area = app.transcript_area.unwrap();
        assert_eq!(rows.end, area.bottom(), "{rows:?} in {area:?}");
        let shown = screen(&terminal);
        let lines: Vec<&str> = shown.lines().collect();
        assert!(
            lines[rows.end as usize - 1].contains("in 5 (tokenized)"),
            "{:?}",
            lines[rows.end as usize - 1]
        );
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
        assert_eq!(entry[0], "\u{23fa} Plan");
        assert!(entry.contains(&"    wraps around"), "{entry:?}");
        assert!(entry[entry.len() - 1].starts_with("    ls"), "{entry:?}");
        assert!(!entry[entry.len() - 1].contains("tokenized"), "{entry:?}");
        // The badge is on the blank row the entry does not own.
        assert!(
            lines[rows.end as usize].trim_end().ends_with("(tokenized)"),
            "{:?}",
            lines[rows.end as usize]
        );
        assert!(matches!(&app.entries().list[1], Entry::Assistant(t) if t == raw));
    }

    #[test]
    fn ctrl_l_expands_every_fold_then_collapses_them_all() {
        let mut app = App::detached();
        let first = app.entries().list.len();
        app.entries()
            .push(Entry::Output("a\nb\nc\nd\ne".to_string()));
        app.entries().push(Entry::User("hi".to_string()));
        app.entries()
            .push(Entry::Output("1\n2\n3\n4\n5".to_string()));
        let mut terminal = Terminal::new(TestBackend::new(40, 40)).unwrap();
        let ctrl_l = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!screen(&terminal).contains("[collapse]"));

        app.on_key(ctrl_l);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(
            app.expanded,
            std::collections::HashSet::from([first, first + 2])
        );
        assert_eq!(screen(&terminal).matches("[collapse]").count(), 2);

        // With one closed again by hand, the key opens the rest rather than closing.
        app.expanded.remove(&first);
        app.on_key(ctrl_l);
        assert_eq!(
            app.expanded,
            std::collections::HashSet::from([first, first + 2])
        );

        app.on_key(ctrl_l);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.expanded.is_empty());
        assert!(!screen(&terminal).contains("[collapse]"));
    }

    /// A long session of markdown with code in it, the kind that is slow to lay out.
    fn long_session(messages: usize) -> App {
        let app = App::detached();
        let mut entries = app.entries();
        for i in 0..messages {
            entries.push(Entry::User(format!("question {i}")));
            entries.push(Entry::Assistant(format!(
                "## Step {i}\n\nrun **this** with `care`:\n\n```rust\nfn step_{i}() -> u32 {{\n    \
                 let x = {i};\n    x * 2\n}}\n```\n\n- one\n- two"
            )));
        }
        drop(entries);
        app
    }

    #[test]
    fn a_frame_lays_out_only_the_entries_that_changed() {
        let mut app = long_session(50);
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let all = app.entries().list.len();
        assert_eq!(app.drawn.built, all);
        let first = screen(&terminal);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(
            app.drawn.built, all,
            "nothing changed, so nothing is laid out"
        );
        assert_eq!(screen(&terminal), first);

        // A streaming answer grows its own entry and no other.
        app.entries()
            .apply(&crate::session::Event::Text("\n\nand more".to_string()));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.drawn.built, all + 1);
        assert!(screen(&terminal).contains("and more"));

        // A new entry is laid out by itself, and opening one lays out that one.
        app.entries()
            .push(Entry::Output("a\nb\nc\nd\ne".to_string()));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.drawn.built, all + 2);
        app.expanded.insert(all);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.drawn.built, all + 3);
        assert!(screen(&terminal).contains("[collapse]"));

        // A new width wraps everything again, and a cleared transcript drops the rest.
        terminal.backend_mut().resize(50, 20);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.drawn.built, 2 * all + 4);
        app.entries().list.truncate(1);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.drawn.slots.len(), 1);
    }

    /// The point of the cache: a frame over a long transcript that has not changed costs
    /// a small part of laying it out. The margin is wide so a loaded machine still passes.
    #[test]
    fn a_long_transcript_redraws_from_its_cache_quickly() {
        let mut app = long_session(200);
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        let began = Instant::now();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let laid_out = began.elapsed();
        let cached = (0..3)
            .map(|_| {
                let began = Instant::now();
                terminal.draw(|frame| render(frame, &mut app)).unwrap();
                began.elapsed()
            })
            .min()
            .unwrap();
        assert!(
            cached * 10 < laid_out,
            "cached {cached:?}, laid out {laid_out:?}"
        );
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

    #[test]
    fn clear_confirmation_shows_both_choices_and_cancel() {
        let mut app = App::detached();
        app.clear_pending = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(shown.contains("kill kept processes"), "{shown}");
        assert!(shown.contains("[y] clean slate"), "{shown}");
        assert!(shown.contains("[n] conversation only"), "{shown}");
        assert!(shown.contains("[Esc] cancel"), "{shown}");
        assert!(app.input_area.is_none());
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
    fn the_queue_panel_names_what_it_could_not_fit() {
        let mut app = App::detached();
        app.working = true;
        for (n, text) in ["one", "two", "three", "four"].iter().enumerate() {
            app.on_event(Event::Queued {
                position: n + 1,
                text: text.to_string(),
            });
        }
        let mut terminal = Terminal::new(TestBackend::new(40, 14)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(shown.contains(" 1. one"), "{shown}");
        assert!(shown.contains(" 2. two"), "{shown}");
        // The last row is spent on the count rather than on one more prompt.
        assert!(!shown.contains("three"), "{shown}");
        assert!(shown.contains("2 more"), "{shown}");

        // The panel is above the prompt, and the transcript holds none of it: nothing
        // waiting is part of the conversation yet.
        let rows: Vec<&str> = shown.lines().collect();
        let panel = rows.iter().position(|r| r.contains("queued ")).unwrap();
        let prompt = rows.iter().rposition(|r| r.contains("\u{203a}")).unwrap();
        assert!(panel < prompt, "{shown}");
        let entries = app.entries();
        assert!(
            !entries.list.iter().any(|e| e.text().contains("one")),
            "nothing waiting is in the transcript"
        );
    }

    #[test]
    fn the_spinner_and_the_queue_sit_above_the_prompt() {
        let mut app = App::detached();
        app.working = true;
        app.on_event(Event::Queued {
            position: 1,
            text: "later".to_string(),
        });
        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        // Waiting, so it is shown with the prompt box and not in the transcript.
        assert!(shown.contains("1. later"), "{shown}");
        assert!(shown.contains("queued"), "{shown}");

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
        // A caller works its room out by subtraction, so it can reach zero.
        assert_eq!(clip("abc", 0), "\u{2026}");
        assert_eq!(clip("", 0), "");
    }

    #[test]
    fn a_modal_view_takes_the_subagent_rows_with_it() {
        let mut app = App::detached();
        app.working = true;
        app.session().publish(Event::ChildStarted {
            id: "a1".to_string(),
            identity: "worker".to_string(),
            model: "gpt-5.5".to_string(),
            description: "read the docs".to_string(),
            task: "go".to_string(),
        });
        let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.child_rows.len(), 1);

        // The picker covers the panel, so nothing is left there to click on.
        app.picker = Some(Picker::new("m", "medium"));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.child_rows.is_empty(), "{:?}", app.child_rows);
    }

    #[test]
    fn the_panel_lists_the_turns_subagents_and_the_prompt_says_which_one_is_open() {
        let mut app = App::detached();
        let started = |id: &str, description: &str| Event::ChildStarted {
            id: id.to_string(),
            identity: "worker".to_string(),
            model: "gpt-5.5".to_string(),
            description: description.to_string(),
            task: "go".to_string(),
        };
        app.working = true;
        app.session().publish(started("a1", "read the docs"));
        app.session().publish(started("b2", "count the files"));
        app.session().publish(Event::ChildEnded {
            id: "a1".to_string(),
            ok: true,
        });

        let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(shown.contains("subagents"), "{shown}");
        assert!(
            shown.contains("✓ a1 worker · gpt-5.5 · read the docs"),
            "{shown}"
        );
        assert!(
            shown.contains("b2 worker · gpt-5.5 · count the files"),
            "{shown}"
        );

        // Going inside one says so on the prompt, since that is where it now types, and
        // the row it is the pane of offers the way back out.
        app.open_child("b2");
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(shown.contains("to worker · count the files"), "{shown}");
        assert!(shown.contains("b2 worker · gpt-5.5"), "{shown}");
        assert!(shown.contains("✕ close"), "{shown}");
        assert!(shown.contains("esc close"), "{shown}");

        app.leave_child();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(!shown.contains("to worker"), "{shown}");
        assert!(!shown.contains("✕ close"), "{shown}");
    }

    #[test]
    fn the_close_on_the_open_row_ends_the_pane() {
        let mut app = App::detached();
        app.working = true;
        app.session().publish(Event::ChildStarted {
            id: "b2c9".to_string(),
            identity: "worker".to_string(),
            model: "gpt-5.5".to_string(),
            description: "count the files".to_string(),
            task: "go".to_string(),
        });
        let mut terminal = Terminal::new(TestBackend::new(64, 16)).unwrap();
        app.open_child("b2c9");
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("✕ close"));

        let (area, _) = app.child_rows.iter().find(|(_, id)| id == "b2c9").unwrap();
        let (x, y) = (area.right() - 4, area.y);
        assert!(click(&mut app, x, y));
        assert!(app.inside.is_none(), "the close did not close the pane");
    }

    #[test]
    fn the_subagent_panel_goes_once_the_turn_is_over() {
        let mut app = App::detached();
        app.working = true;
        app.session().publish(Event::ChildStarted {
            id: "a1".to_string(),
            identity: "worker".to_string(),
            model: "gpt-5.5".to_string(),
            description: "read the docs".to_string(),
            task: "go".to_string(),
        });
        let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("subagents"));
        assert_eq!(app.child_rows.len(), 1);

        // Nothing left to watch, so the rows stop sitting above the prompt.
        app.session().publish(Event::ChildEnded {
            id: "a1".to_string(),
            ok: true,
        });
        app.on_event(Event::TurnEnd);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!screen(&terminal).contains("subagents"));
        assert!(app.child_rows.is_empty());

        // Reading one back brings them with it: the panel is the pane's way out.
        app.open_child("a1");
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(shown.contains("subagents"), "{shown}");
        assert!(shown.contains("✕ close"), "{shown}");
        assert_eq!(app.child_rows.len(), 1);
    }

    #[test]
    fn a_child_still_running_keeps_the_panel_after_its_turn() {
        let mut app = App::detached();
        app.working = true;
        app.session().publish(Event::ChildStarted {
            id: "a1".to_string(),
            identity: "worker".to_string(),
            model: "gpt-5.5".to_string(),
            description: "read the docs".to_string(),
            task: "go".to_string(),
        });
        let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();

        // The child is detached, so the turn ending does not end it.
        app.on_event(Event::TurnEnd);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(
            shown.contains("a1 worker · gpt-5.5 · read the docs"),
            "{shown}"
        );
        assert_eq!(app.child_rows.len(), 1);

        app.session().publish(Event::ChildEnded {
            id: "a1".to_string(),
            ok: true,
        });
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!screen(&terminal).contains("subagents"));
    }

    #[test]
    fn the_working_row_shows_how_fast_the_model_is_answering() {
        let mut app = App::detached();
        app.working = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!screen(&terminal).contains("tok/s"), "nothing streamed yet");

        // A reading of writing, twenty-five tokens in it, and one that closes it.
        let began = Instant::now() - crate::speed::PERIOD;
        app.speed.start(began);
        app.speed.streamed(began, 25);
        app.speed.streamed(Instant::now(), 1);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        let rate = 25.0 / crate::speed::PERIOD.as_secs_f64();
        assert!(
            shown.contains(&format!("working... · {rate:.0} tok/s")),
            "{shown}"
        );

        // It is a working-row reading, so it goes with the row.
        app.on_event(Event::TurnEnd);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!screen(&terminal).contains("tok/s"));
    }

    #[test]
    fn the_working_row_shows_the_prompt_being_read() {
        let mut app = App::detached();
        app.working = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();

        // A call goes out and says nothing for four seconds. There is no progress to be
        // had from a backend, so what the row has to show is what is being waited on.
        let sent = Instant::now() - Duration::from_secs(4);
        app.on_event(Event::Sending(8725));
        app.speed.sending(sent, 8725);
        app.speed.start(sent);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(shown.contains("reading 8.7k in · 4s"), "{shown}");

        // The first token ends the wait, and the wait becomes a rate of its own.
        app.speed.streamed(Instant::now(), 10);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(!shown.contains("reading"), "{shown}");
        assert!(shown.contains("2.2k in/s"), "{shown}");
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
            preview: None,
            offers: Offers {
                exact: exact.map(str::to_string),
                prefix: None,
            },
        }
    }

    /// Draw `app` and age the approval in it past the settle window, as one the user
    /// has had time to read.
    fn settle(app: &mut App) {
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        age(app);
    }

    /// Put the approval last drawn on screen, long enough ago to have been read.
    fn age(app: &mut App) {
        app.drawn();
        if let Some((_, Some(at))) = &mut app.approval_shown {
            *at -= Duration::from_secs(1);
        }
    }

    #[test]
    fn a_key_typed_as_the_approval_appears_does_not_allow_it() {
        let y = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        let mut app = App::detached();
        let shown = approval(Some("ls"));
        app.on_event(Event::Approval {
            id: shown.id,
            tool: shown.tool,
            command: shown.command,
            preview: shown.preview,
            offers: shown.offers,
        });
        app.on_key(y);
        assert!(
            app.pending.is_some(),
            "y before the box is drawn is dropped"
        );

        // The window opens when the frame reaches the screen, not when it is built.
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        app.on_key(y);
        assert!(
            app.pending.is_some(),
            "y before the frame is flushed is dropped"
        );
        app.drawn();
        app.on_key(y);
        assert!(
            app.pending.is_some(),
            "y inside the settle window is dropped"
        );

        app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(app.pending.is_none(), "rejecting never waits");

        app.pending = Some(approval(None));
        settle(&mut app);
        app.on_key(y);
        assert!(app.pending.is_none());
    }

    #[test]
    fn a_remember_key_needs_a_second_press() {
        let mut app = App::detached();
        app.pending = Some(approval(Some("ls")));
        settle(&mut app);
        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        app.on_key(a);
        assert!(app.pending.is_some());
        assert_eq!(app.armed, Some(('a', None)));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(shown.contains("again to save"), "{shown}");
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.armed, None, "another key disarms");
        app.on_key(a);
        settle_armed(&mut app);
        app.on_key(a);
        assert!(app.pending.is_none());
    }

    /// Draw the armed hint and let it stand long enough to have been read.
    fn settle_armed(app: &mut App) {
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        app.drawn();
        if let Some((_, Some(at))) = &mut app.armed {
            *at -= Duration::from_secs(1);
        }
    }

    #[test]
    fn a_typed_word_does_not_arm_and_confirm_a_remember_key() {
        let mut app = App::detached();
        app.pending = Some(Approval {
            offers: Offers {
                exact: None,
                prefix: Some("Bash(ls:*)".to_string()),
            },
            ..approval(None)
        });
        settle(&mut app);
        // "support" while a prefix rule is offered: p arms, the next p must not confirm.
        for c in "sup".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(app.pending.is_some(), "no frame showed the hint");
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        app.drawn();
        app.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(
            app.pending.is_some(),
            "the hint has not been up long enough"
        );
        settle_armed(&mut app);
        app.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(app.pending.is_none());
    }

    #[test]
    fn a_new_approval_settles_again_and_drops_an_armed_key() {
        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        let mut app = App::detached();
        app.pending = Some(approval(Some("ls")));
        settle(&mut app);
        app.on_key(a);
        assert_eq!(app.armed, Some(('a', None)));
        // Answered elsewhere, and the next call takes its place under the same keys.
        app.pending = Some(Approval {
            id: 8,
            ..approval(Some("ls"))
        });
        app.on_key(a);
        assert!(app.pending.is_some(), "the new box has not been drawn");
        settle(&mut app);
        assert_eq!(app.armed, None);
        app.on_key(a);
        assert!(app.pending.is_some(), "a still needs its second press");
    }

    #[test]
    fn a_double_click_does_not_answer_the_approval_that_comes_up_under_it() {
        let mut app = App::detached();
        app.pending = Some(approval(Some("ls")));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        age(&mut app);
        let &(always, _) = app
            .buttons
            .iter()
            .find(|(_, k)| *k == KeyCode::Char('a'))
            .unwrap();
        app.on_mouse(down(always.x + 1, always.y));
        assert!(app.pending.is_none());

        // A parallel step's approval was parked behind, and comes up in the same place.
        let next = approval(Some("ls"));
        app.on_event(Event::Approval {
            id: 9,
            tool: next.tool,
            command: next.command,
            preview: next.preview,
            offers: next.offers,
        });
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        app.drawn();
        assert!(app.buttons.iter().any(|&(area, k)| {
            k == KeyCode::Char('a')
                && area.contains(ratatui::layout::Position::new(always.x + 1, always.y))
        }));
        app.on_mouse(down(always.x + 1, always.y));
        assert!(
            app.pending.is_some(),
            "the second click of the double click"
        );
        age(&mut app);
        app.on_mouse(down(always.x + 1, always.y));
        assert!(app.pending.is_none());
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
        app.on_mouse(down(yes.x + 1, yes.y));
        assert!(
            app.pending.is_some(),
            "a click waits out the settle window too"
        );
        age(&mut app);
        assert!(app.on_mouse(down(yes.x + 1, yes.y)));
        assert!(app.pending.is_none());

        app.pending = Some(approval(Some("ls")));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        age(&mut app);
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
    fn a_long_command_cannot_be_approved_until_its_end_has_been_shown() {
        let mut app = App::detached();
        let mut long = approval(Some("seq"));
        long.command = (1..=40).map(|n| format!("line{n}\n")).collect();
        app.pending = Some(long);
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        age(&mut app);
        let shown = screen(&terminal);
        assert!(shown.contains("lines hidden"));
        assert!(!shown.contains("line40"));
        for code in ['y', 'a', 'p'] {
            app.on_key(KeyEvent::new(KeyCode::Char(code), KeyModifiers::NONE));
        }
        let &(yes, _) = app
            .buttons
            .iter()
            .find(|(_, k)| *k == KeyCode::Char('y'))
            .unwrap();
        app.on_mouse(down(yes.x + 1, yes.y));
        assert!(app.pending.is_some(), "nothing approves what is unread");

        app.on_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.pending.is_some());
        app.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("line40"));
        app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(app.pending.is_none());
    }

    #[test]
    fn rejecting_needs_no_reading() {
        let mut app = App::detached();
        let mut long = approval(None);
        long.command = (1..=40).map(|n| format!("line{n}\n")).collect();
        app.pending = Some(long);
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(app.pending.is_none());
    }

    #[test]
    fn a_running_command_shows_its_latest_lines() {
        let mut app = App::detached();
        app.entries().apply(&Event::ToolStart {
            tool: "bash".to_string(),
            summary: "seq 5".to_string(),
            preview: None,
        });
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
        assert!(
            text.contains("$ seq 5\n  ✓ succeeded [+2 lines]\n\n"),
            "{text}"
        );
        assert!(!text.contains("exit code"), "{text}");
        assert!(!text.contains("running"), "{text}");
    }

    #[test]
    fn a_shell_command_opens_and_closes_its_output() {
        let mut app = App::detached();
        app.entries().push(Entry::Command {
            tool: "bash".to_string(),
            summary: "seq 5".to_string(),
        });
        app.entries()
            .push(Entry::Output("exit code: 0\n1\n2\n3\n4\n5".to_string()));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(
            text.contains("$ seq 5\n  ✓ succeeded [+6 lines]\n"),
            "{text}"
        );
        assert!(!text.contains("exit code"), "{text}");

        let (rows, _) = app.rows.iter().find(|(_, e)| *e == 1).cloned().unwrap();
        assert!(click(&mut app, 2, rows.start));
        assert!(app.pinned.is_empty(), "a click on the command does not pin");
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(
            text.contains("$ seq 5\n  ✓ succeeded\n\nexit code: 0\n1\n2\n3\n4\n5\n[collapse]\n"),
            "{text}"
        );

        // A click on the output closes it again, under its command.
        let (rows, _) = app.rows.iter().find(|(_, e)| *e == 2).cloned().unwrap();
        assert!(click(&mut app, 2, rows.start));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(
            text.contains("$ seq 5\n  ✓ succeeded [+6 lines]\n"),
            "{text}"
        );
        assert!(
            app.rows.iter().all(|(_, e)| *e != 2),
            "the output has no rows"
        );
    }

    #[test]
    fn a_shell_command_says_how_it_ended() {
        let mut app = App::detached();
        let run = |app: &mut App, summary: &str, output: &str| {
            app.entries().apply(&Event::ToolStart {
                tool: "bash".to_string(),
                summary: summary.to_string(),
                preview: None,
            });
            app.entries().apply(&Event::ToolOutput(output.to_string()));
        };
        run(&mut app, "false", "exit code: 1\n");
        run(
            &mut app,
            "sleep 999",
            "Command timed out after 120s and was killed.\n",
        );
        app.entries().apply(&Event::ToolRejected {
            tool: "bash".to_string(),
            summary: "rm -rf /".to_string(),
            by: Rejecter::Judge,
            reason: "unrelated to the stated task".to_string(),
        });
        app.entries().apply(&Event::ToolRejected {
            tool: "bash".to_string(),
            summary: "ls".to_string(),
            by: Rejecter::User,
            reason: String::new(),
        });
        let mut terminal = Terminal::new(TestBackend::new(60, 30)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(
            text.contains("$ false\n  ✗ failed, exit 1 [+1 lines]\n"),
            "{text}"
        );
        assert!(
            text.contains("$ sleep 999\n  ✗ timed out after 120s [+1 lines]\n"),
            "{text}"
        );
        assert!(
            text.contains("$ rm -rf /\n  ✗ rejected by judge [+1 lines]\n"),
            "{text}"
        );
        assert!(!text.contains("unrelated"), "{text}");
        // Nothing was said of it, so there is nothing to open.
        assert!(text.contains("$ ls\n  ✗ rejected by you\n"), "{text}");

        // A click on the rejected command opens the judge's reason under it.
        let command = app
            .entries()
            .list
            .iter()
            .position(|e| e.text() == "rm -rf /")
            .unwrap();
        let (rows, _) = app
            .rows
            .iter()
            .find(|(_, e)| *e == command)
            .cloned()
            .unwrap();
        assert!(click(&mut app, 2, rows.start));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(
            text.contains(
                "  ✗ rejected by judge\n\n✗ rejected by judge: unrelated to the stated task\n"
            ),
            "{text}"
        );
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
        assert!(text.contains("three\nfour\nfive\n[collapse]\n"), "{text}");
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
    fn commentary_is_drawn_dimmer_than_the_answer() {
        let fg = |entry: Entry| {
            let rows = entry_lines(&entry, 40, false, false, None);
            let line = &rows.lines[0];
            let span = line.spans.last().unwrap();
            line.style.patch(span.style).fg
        };
        assert_eq!(
            fg(Entry::Commentary("checking first".to_string())),
            Some(Color::DarkGray)
        );
        assert_eq!(fg(Entry::Assistant("found it".to_string())), None);
    }

    #[test]
    fn thinking_comes_down_to_one_line_until_it_is_clicked() {
        let mut app = App::detached();
        app.entries().push(Entry::Reasoning(
            "Choosing the layout\n\nThe panel goes above the prompt.\nIt stays there.".to_string(),
        ));
        app.entries().push(Entry::Reasoning("brief".to_string()));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(
            text.contains("✻ Choosing the layout [+3 lines]\n"),
            "{text}"
        );
        assert!(!text.contains("stays there"), "{text}");
        // A thought that already fits says nothing about lines it is not hiding.
        assert!(text.contains("✻ brief\n"), "{text}");

        let (rows, _) = app.rows.iter().find(|(_, e)| *e == 1).cloned().unwrap();
        assert_eq!(
            rows.len(),
            1,
            "one line, and no row of its own to expand by"
        );
        assert!(click(&mut app, 2, rows.start));
        assert!(app.pinned.is_empty(), "a click on a thought does not pin");
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("  It stays there.\n  [collapse]\n"), "{text}");
        assert!(!text.contains("[+3 lines]"), "{text}");

        // The short one has nothing to fold, so clicking it pins its badge instead.
        let (rows, _) = app.rows.iter().find(|(_, e)| *e == 2).cloned().unwrap();
        assert!(click(&mut app, 2, rows.start));
        assert_eq!(app.pinned.iter().copied().collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn a_compaction_summary_shows_its_first_rows_until_it_is_clicked() {
        let mut app = App::detached();
        app.entries().push(Entry::Info(
            "compacted history (summarised earlier turns): ~900 -> ~400 tokens".to_string(),
        ));
        app.entries().push(Entry::Summary(
            "The goal is the picker.\nThe list is cached.\nThe keys are bound.\nThe badge is left."
                .to_string(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(
            text.contains("\u{2261} The goal is the picker.\n"),
            "{text}"
        );
        assert!(text.contains("[+1 lines]"), "{text}");
        assert!(!text.contains("badge is left"), "{text}");

        let (rows, _) = app.rows.iter().find(|(_, e)| *e == 2).cloned().unwrap();
        assert!(click(&mut app, 2, rows.start));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(
            text.contains("  The badge is left.\n  [collapse]\n"),
            "{text}"
        );
    }

    #[test]
    fn a_tool_call_is_drawn_as_the_tool_it_is() {
        let mut app = App::detached();
        let call = |tool: &str, summary: &str| Entry::Command {
            tool: tool.to_string(),
            summary: summary.to_string(),
        };
        app.entries().push(call("bash", "git status"));
        app.entries()
            .push(call("register_skills", "register_skills chrome"));
        app.entries().push(call("read", "read /tmp/x.rs"));
        app.entries().push(call("agent", "agent worker: check it"));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("$ git status\n"), "{text}");
        // Only the shell gets the shell's prompt.
        assert!(!text.contains("$ register_skills"), "{text}");
        assert!(text.contains("✦ register_skills chrome\n"), "{text}");
        assert!(text.contains("▸ read /tmp/x.rs\n"), "{text}");
        assert!(text.contains("⇢ agent worker: check it\n"), "{text}");
    }

    #[test]
    fn a_long_shell_command_folds_like_its_output() {
        let mut app = App::detached();
        app.entries().push(Entry::Command {
            tool: "bash".to_string(),
            summary: "one \\\ntwo \\\nthree \\\nfour".to_string(),
        });
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(
            text.contains("$ one \\\n  two \\\n  three \\\n  [+1 lines]\n"),
            "{text}"
        );

        let (rows, _) = app.rows.iter().find(|(_, e)| *e == 1).cloned().unwrap();
        assert!(click(&mut app, 2, rows.start));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("  four\n  [collapse]\n"));
    }

    #[test]
    fn an_edit_diff_is_folded_under_its_command_until_clicked() {
        let mut app = App::detached();
        app.entries().push(Entry::Command {
            tool: "edit".to_string(),
            summary: "edit /f.rs".to_string(),
        });
        app.entries()
            .push(Entry::Diff("@@ -1,2 +1,2 @@\n a\n-b\n+c".to_string()));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("✎ edit /f.rs\n  [diff +1 -1]\n"), "{text}");
        assert!(!text.contains("+c"), "{text}");

        let (rows, _) = app.rows.iter().find(|(_, e)| *e == 2).cloned().unwrap();
        assert!(click(&mut app, 2, rows.start));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(
            text.contains("  @@ -1,2 +1,2 @@\n   a\n  -b\n  +c\n  [collapse]\n"),
            "{text}"
        );
        let buffer = terminal.backend().buffer();
        let (_, y) = (0..buffer.area.height)
            .map(|y| (0, y))
            .find(|&(_, y)| buffer[(2, y)].symbol() == "+" && buffer[(3, y)].symbol() == "c")
            .unwrap();
        assert_eq!(buffer[(2, y)].fg, Color::Green);
        assert_eq!(buffer[(2, y - 1)].fg, Color::Red);
    }

    #[test]
    fn an_approval_shows_the_diff_and_waits_for_its_end() {
        let mut app = App::detached();
        let mut edit = approval(None);
        edit.tool = "edit".to_string();
        edit.command = "edit /f.rs (1 lines -> 1 lines)".to_string();
        edit.preview = Some("@@ -1 +1 @@\n-old\n+new".to_string());
        app.pending = Some(edit.clone());
        let mut terminal = Terminal::new(TestBackend::new(60, 30)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        age(&mut app);
        let text = screen(&terminal);
        assert!(text.contains("edit /f.rs (1 lines -> 1 lines)"), "{text}");
        assert!(text.contains("-old"), "{text}");
        assert!(text.contains("+new"), "{text}");
        assert!(app.approval_seen);

        // A long diff is part of what is approved, so `y` waits for its last line too.
        edit.preview = Some(
            std::iter::once("@@ -0,0 +1,40 @@".to_string())
                .chain((1..=40).map(|n| format!("+line{n}")))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        app.pending = Some(edit);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("lines hidden"));
        app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(app.pending.is_some());
        app.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("+line40"));
        app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(app.pending.is_none());
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

    /// The transcript line and char where `needle` is drawn.
    fn find(app: &App, needle: &str) -> (usize, usize) {
        app.lines
            .iter()
            .enumerate()
            .find_map(|(line, text)| {
                let at = text.find(needle)?;
                Some((line, text[..at].chars().count()))
            })
            .unwrap_or_else(|| panic!("{needle:?} is not drawn: {:?}", app.lines))
    }

    /// A drag from one transcript cell to another, both in view, ending on `to`.
    fn drag(app: &mut App, from: (usize, usize), to: (usize, usize)) {
        let area = app.transcript_area.unwrap();
        let at = |(line, column): (usize, usize)| {
            (area.x + column as u16, area.y + (line - app.scroll) as u16)
        };
        let (from, to) = (at(from), at(to));
        app.on_mouse(down(from.0, from.1));
        app.on_mouse(left(MouseEventKind::Drag(MouseButton::Left), to.0, to.1));
    }

    fn drawn(source: &str, width: u16) -> (App, Terminal<TestBackend>) {
        let mut app = App::detached();
        app.entries().push(Entry::Assistant(source.to_string()));
        let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        (app, terminal)
    }

    #[test]
    fn a_whole_message_copies_as_the_markdown_that_drew_it() {
        let key = "ab".repeat(60);
        let source = format!(
            "## Plan\n\nrun **this** now:\n\n```\nfn main() {{\n    go();\n}}\n```\n\n\
             | a | b |\n|---|---|\n| 1 | 2 |\n\n{key}"
        );
        let (mut app, _) = drawn(&source, 40);
        app.on_key(ratatui::crossterm::event::KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        ));
        let text = app.selected_text().unwrap();
        // The greeting above it is a note, copied as it reads; the message is its source.
        assert!(
            text.trim_end().ends_with(&format!("\n{source}")),
            "{text:?}"
        );
    }

    #[test]
    fn a_selection_copies_the_markdown_under_it() {
        let source = "## Plan\n\nrun **this one** now\n\n```rust\nfn main() {\n    go();\n}\n```\n\n\
                      | a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n\n- first\n- second";
        let (mut app, _) = drawn(source, 40);
        // From the first char of `from` to the last of `to`.
        let copy = |app: &mut App, from: &str, to: &str| {
            let (start, end) = (find(app, from), find(app, to));
            drag(app, start, (end.0, end.1 + to.chars().count() - 1));
            app.selected_text().unwrap()
        };

        // A heading's text copies with its `#`s.
        assert_eq!(copy(&mut app, "Plan", "Plan"), "## Plan");
        // Part of a bold run takes the whole of it, so its stars stay paired.
        assert_eq!(copy(&mut app, "run this", "this"), "run **this one**");
        // Inside a code block, the code and nothing the view drew around it.
        assert_eq!(copy(&mut app, "fn main", "go()"), "fn main() {\n    go()");
        // Two rows of a table are the whole table, or it is no table at all.
        assert_eq!(
            copy(&mut app, "1  2", "3  4"),
            "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |"
        );
        // A list item keeps its bullet.
        assert_eq!(copy(&mut app, "first", "second"), "- first\n- second");
        // Out of the paragraph and into the code block takes its fences too.
        assert_eq!(
            copy(&mut app, "now", "go();"),
            "now\n\n```rust\nfn main() {\n    go();\n}\n```"
        );
    }

    #[test]
    fn a_click_on_the_copy_label_copies_the_code() {
        let (mut app, _) = drawn("look:\n\n```sh\ncargo test\nls -la\n```", 40);
        let (line, column) = find(&app, markdown::COPY_LABEL);
        let area = app.transcript_area.unwrap();
        let (x, y) = (
            area.x + column as u16 + 2,
            area.y + (line - app.scroll) as u16,
        );
        assert!(click(&mut app, x, y));
        assert_eq!(
            crate::clipboard::last_copied().as_deref(),
            Some("cargo test\nls -la")
        );
        assert!(app.copied.is_some(), "the copy is not said");
    }

    #[test]
    fn a_click_on_inline_code_copies_without_backticks() {
        let (mut app, _) = drawn("run `cargo test` now", 40);
        let (line, column) = find(&app, "cargo test");
        let area = app.transcript_area.unwrap();
        let y = area.y + (line - app.scroll) as u16;
        assert!(click(&mut app, area.x + column as u16, y));
        assert_eq!(
            crate::clipboard::last_copied().as_deref(),
            Some("cargo test")
        );
        drag(&mut app, (line, column), (line, column + 4));
        assert_eq!(app.selected_text().as_deref(), Some("`cargo test`"));
    }

    #[test]
    fn code_clicks_use_char_positions_and_take_priority_over_links() {
        let (mut app, _) = drawn("界 [`cargo test`](https://example.com)", 40);
        let (line, column) = find(&app, "cargo test");
        let area = app.transcript_area.unwrap();
        let x = area.x
            + crate::wrap::width(&app.lines[line].chars().take(column).collect::<String>()) as u16;
        let y = area.y + (line - app.scroll) as u16;
        assert!(click(&mut app, x, y));
        assert_eq!(
            crate::clipboard::last_copied().as_deref(),
            Some("cargo test")
        );
    }

    #[test]
    fn a_click_inside_a_code_block_copies_the_whole_block() {
        let (mut app, _) = drawn("```sh\ncargo test\n  ls -la\n```", 40);
        for needle in ["cargo test", "ls -la"] {
            let (line, column) = find(&app, needle);
            let area = app.transcript_area.unwrap();
            let y = area.y + (line - app.scroll) as u16;
            assert!(click(&mut app, area.x + column as u16, y));
            assert_eq!(
                crate::clipboard::last_copied().as_deref(),
                Some("cargo test\n  ls -la")
            );
        }
    }

    #[test]
    fn a_table_with_wide_chars_keeps_its_columns() {
        let (app, _) = drawn("| 名前 | x |\n|---|---|\n| ab | y |", 40);
        let (head, _) = find(&app, "名前");
        let column = |line: usize, needle: &str| {
            let text = &app.lines[line];
            crate::wrap::width(&text[..text.find(needle).unwrap()])
        };
        assert_eq!(column(head, "x"), column(head + 2, "y"), "{:?}", app.lines);
    }

    #[test]
    fn the_user_s_own_words_sit_on_a_ground_of_their_own() {
        let mut app = App::detached();
        app.entries()
            .push(Entry::User("a prompt that wraps around".to_string()));
        let mut terminal = Terminal::new(TestBackend::new(20, 12)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let (rows, _) = app.rows.iter().find(|(_, e)| *e == 1).cloned().unwrap();
        assert!(rows.len() > 1, "the prompt wraps");
        let area = app.transcript_area.unwrap();
        let buffer = terminal.backend().buffer();
        // Every row of it is filled out to the width, so the ground reads as a block.
        for y in rows.clone() {
            for x in area.x..area.right() {
                assert_eq!(buffer[(x, y)].bg, USER_BG, "{x},{y}");
            }
        }
        // The blank line below it is not part of the prompt.
        assert_eq!(buffer[(0, rows.end)].bg, Color::Reset);
    }

    #[test]
    fn a_copy_puts_back_the_rows_the_wrap_broke() {
        let mut app = App::detached();
        app.entries().push(Entry::Output(
            "one two three four five\nsecond line".to_string(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(20, 12)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let (rows, _) = app.rows.iter().find(|(_, e)| *e == 1).cloned().unwrap();
        assert_eq!(rows.len(), 3, "the first line wraps onto two rows");
        let area = app.transcript_area.unwrap();

        assert!(app.on_mouse(down(area.x, rows.start)));
        assert!(app.on_mouse(left(
            MouseEventKind::Drag(MouseButton::Left),
            area.right() - 1,
            rows.end - 1
        )));
        // The break the wrap made is a space again; the one the text has stays a newline.
        assert_eq!(
            app.selected_text().as_deref(),
            Some("one two three four five\nsecond line")
        );
    }

    #[test]
    fn the_copy_note_lands_beside_the_drag_that_made_it() {
        let mut app = App::detached();
        app.entries().push(Entry::Output("one two".to_string()));
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let (rows, _) = app.rows.iter().find(|(_, e)| *e == 1).cloned().unwrap();
        let area = app.transcript_area.unwrap();
        app.on_mouse(down(area.x, rows.start));
        app.on_mouse(left(
            MouseEventKind::Drag(MouseButton::Left),
            area.x + 6,
            rows.start,
        ));
        app.on_mouse(left(
            MouseEventKind::Up(MouseButton::Left),
            area.x + 6,
            rows.start,
        ));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        // On the row under the pointer, not down on the prompt's border.
        let buffer = terminal.backend().buffer();
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, rows.start + 1)].symbol())
            .collect();
        assert!(row.trim_end().ends_with(" copied 7 chars"), "{row:?}");
        assert!(
            row.starts_with("      "),
            "it starts under the pointer: {row:?}"
        );
        assert_eq!(buffer[(area.x + 6, rows.start + 1)].bg, Color::Cyan);
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
        // A thin thumb on a fainter track, both grey: the bar used to be a solid block
        // on a double rule, in the terminal's brightest white.
        let buffer = terminal.backend().buffer();
        let column: Vec<&str> = (bar.top()..bar.bottom())
            .map(|y| buffer[(bar.x, y)].symbol())
            .collect();
        let thumb = column
            .iter()
            .position(|s| *s == "\u{2590}")
            .expect("a thumb");
        assert!(column.contains(&"\u{2502}"), "{column:?}");
        assert_eq!(
            buffer[(bar.x, bar.top() + thumb as u16)].fg,
            Color::DarkGray
        );
        assert_eq!(buffer[(bar.x, bar.top())].fg, TRACK);
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

    /// A window resetting in two hours, far enough off that the count of whole minutes
    /// the bar prints cannot slip while the test runs.
    fn window(used_percent: f64, window_minutes: u64) -> limits::Window {
        limits::Window {
            used_percent,
            window_minutes: Some(window_minutes),
            resets_at: Some(chrono::Local::now().timestamp() + 2 * 3600 + 30),
        }
    }

    /// The local time the windows of [`window`] reset, as the bar prints it.
    fn reset_clock(app: &App) -> String {
        let found = app.rate_limits.unwrap();
        let at = found.windows().next().unwrap().resets_at.unwrap();
        let at = chrono::TimeZone::timestamp_opt(&chrono::Local, at, 0).unwrap();
        at.format("%H:%M").to_string()
    }

    /// The bottom row, which is the status bar.
    fn status(terminal: &Terminal<TestBackend>) -> String {
        screen(terminal).lines().next_back().unwrap().to_string()
    }

    /// The status bar cell under the first character of `text`.
    fn status_cell<'a>(
        terminal: &'a Terminal<TestBackend>,
        text: &str,
    ) -> &'a ratatui::buffer::Cell {
        let row = status(terminal);
        let x = row[..row.find(text).unwrap()].chars().count() as u16;
        let y = terminal.backend().buffer().area.height - 1;
        &terminal.backend().buffer()[(x, y)]
    }

    #[test]
    fn status_bar_shows_rate_limits_coloured_by_headroom() {
        let mut app = App::detached();
        let mut terminal = Terminal::new(TestBackend::new(200, 10)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!status(&terminal).contains('%'));

        let cases = [
            (42.0, Color::DarkGray),
            (75.0, Color::Yellow),
            (90.0, Color::Red),
        ];
        for (used, colour) in cases {
            app.on_event(Event::RateLimits(RateLimits {
                primary: Some(window(used, 300)),
                secondary: Some(window(17.0, 10080)),
                credits: None,
            }));
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let bar = status(&terminal);
            // Each window says how much of it is used and the time it comes back.
            let at = reset_clock(&app);
            let expected = format!("5h:{used:.0}% resets@{at} | 7d:17% resets@{at}");
            assert!(bar.contains(&expected), "{bar}");
            assert_eq!(status_cell(&terminal, "5h:").fg, colour);
            assert_eq!(status_cell(&terminal, "7d:").fg, Color::DarkGray);
        }
    }

    #[test]
    fn a_narrow_bar_drops_its_hint_before_where_the_session_stands() {
        let mut app = App::detached();
        app.branch = crate::branch::Branch::named("side");
        app.on_event(Event::RateLimits(RateLimits {
            primary: Some(window(8.0, 300)),
            secondary: Some(window(20.0, 10080)),
            credits: None,
        }));

        let mut wide = Terminal::new(TestBackend::new(200, 10)).unwrap();
        wide.draw(|frame| render(frame, &mut app)).unwrap();
        let bar = status(&wide);
        assert!(bar.contains("/ for commands"), "{bar}");

        let mut narrow = Terminal::new(TestBackend::new(85, 10)).unwrap();
        narrow.draw(|frame| render(frame, &mut app)).unwrap();
        let bar = status(&narrow);
        assert!(
            bar.contains(":side | ") && bar.contains(" | 5h:8% resets@"),
            "{bar}"
        );
        assert!(!bar.contains("for commands"), "{bar}");
    }

    #[test]
    fn a_reset_more_than_a_day_off_is_a_day_and_a_time() {
        let mut app = App::detached();
        let mut terminal = Terminal::new(TestBackend::new(200, 10)).unwrap();
        let at = chrono::Local::now() + chrono::TimeDelta::days(3);
        app.on_event(Event::RateLimits(RateLimits {
            primary: None,
            secondary: Some(limits::Window {
                used_percent: 17.0,
                window_minutes: Some(10080),
                resets_at: Some(at.timestamp()),
            }),
            credits: None,
        }));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let bar = status(&terminal);
        let expected = format!("5h:none | 7d:17% resets@{}", at.format("%a %H:%M"));
        assert!(bar.contains(&expected), "{bar}");
    }

    #[test]
    fn the_status_bar_says_which_branch_and_how_full_the_context_is() {
        let mut app = App::detached();
        app.branch = crate::branch::Branch::named("side");
        let mut terminal = Terminal::new(TestBackend::new(200, 10)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let bar = status(&terminal);
        assert!(bar.contains(":side"), "{bar}");
        assert!(!bar.contains("ctx:"), "nothing read yet");

        app.limits = crate::compact::Limits {
            window: Some(100_000),
            ..crate::compact::Limits::default()
        };
        app.on_event(Event::Usage(usage(82_000, 0, 10, 0)));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let bar = status(&terminal);
        assert!(
            bar.contains("(100k context) | ") && bar.contains(" | ctx:82%"),
            "{bar}"
        );
        // Past the point compaction waits for, so it is not drawn as an idle number.
        assert_eq!(status_cell(&terminal, "ctx:").fg, Color::Yellow);
    }

    #[test]
    fn status_bar_follows_the_open_child() {
        for template in [None, Some("$model · ctx:$ctx")] {
            let mut app = App::detached();
            app.statusline = template.map(|text| crate::statusline::Template::parse(text).unwrap());
            app.limits.window = Some(100_000);
            app.last_usage = Some(usage(82_000, 0, 10, 0));
            app.session().publish(Event::ChildStarted {
                id: "a1".to_string(),
                identity: "worker".to_string(),
                model: "gpt-5.5".to_string(),
                description: "read the docs".to_string(),
                task: "go".to_string(),
            });
            let mut terminal = Terminal::new(TestBackend::new(200, 12)).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let main = status(&terminal);
            assert!(main.contains("ctx:82%"), "{main}");

            app.open_child("a1");
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let bar = status(&terminal);
            assert!(bar.contains("gpt-5.5"), "{bar}");
            assert!(!bar.contains("82%"), "no child usage yet: {bar}");
            let window = crate::compact::Limits::default().window("gpt-5.5");
            for percent in [50, 90] {
                app.session().publish(Event::Child {
                    id: "a1".to_string(),
                    event: Box::new(Event::Usage(usage(window * percent / 100, 0, 10, 0))),
                });
                terminal.draw(|frame| render(frame, &mut app)).unwrap();
                let bar = status(&terminal);
                assert!(bar.contains(&format!("ctx:{percent}%")), "{bar}");
            }
            app.session().publish(Event::ChildEnded {
                id: "a1".to_string(),
                ok: true,
            });
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            assert!(status(&terminal).contains("ctx:90%"));

            app.open_child("a1");
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            assert_eq!(status(&terminal), main);
        }
    }

    #[test]
    fn a_statusline_template_replaces_the_built_in_bar() {
        let mut app = App::detached();
        app.branch = crate::branch::Branch::named("main");
        app.statusline = Some(
            crate::statusline::Template::parse(
                "[ $model ](fg:black bg:green)( on $branch)( · ctx $ctx)",
            )
            .unwrap(),
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let bar = status(&terminal);
        // No call has read the context yet, so its group is not drawn at all.
        assert_eq!(bar.trim_end(), " m  on main", "{bar}");
        assert_eq!(status_cell(&terminal, " m ").bg, Color::Green);
        assert_eq!(status_cell(&terminal, "on main").fg, Color::DarkGray);

        app.last_usage = Some(Usage {
            input: 250_000,
            ..Usage::default()
        });
        app.limits.window = Some(262_144);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let bar = status(&terminal);
        assert!(bar.contains("on main · ctx 95%"), "{bar}");
        assert_eq!(status_cell(&terminal, "95%").fg, Color::Red);
    }

    #[test]
    fn status_bar_describes_the_compacted_copy_while_the_prompt_asks_for_it() {
        let (tx_user, _) = tokio::sync::mpsc::channel(1);
        let (tx_control, _) = tokio::sync::mpsc::channel(1);
        let session = crate::session::Session::new(
            "m".to_string(),
            "medium".to_string(),
            "general".to_string(),
            tx_user,
            tx_control,
            std::sync::Arc::default(),
            std::sync::Arc::default(),
            None,
        );
        let mut app = App::new(std::sync::Arc::clone(&session));
        app.last_usage = Some(Usage {
            input: 136_000,
            ..Usage::default()
        });
        app.limits.window = Some(272_000);
        let mut terminal = Terminal::new(TestBackend::new(200, 10)).unwrap();
        app.input.set("/compact-then go on".to_string());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        // No copy yet, so the bar is the full history's.
        assert!(
            status(&terminal).contains(" ctx:50%"),
            "{}",
            status(&terminal)
        );

        session.on_agent(crate::agent::AgentEvent::Fork(Some(13_600)));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let bar = status(&terminal);
        assert!(bar.contains("fork ctx:5%"), "{bar}");
        assert!(bar.contains("fork uncached: ~13.6k tokens"), "{bar}");

        app.input.set("go on".to_string());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let bar = status(&terminal);
        assert!(bar.contains(" ctx:50%") && !bar.contains("fork"), "{bar}");
    }

    #[tokio::test]
    async fn status_bar_counts_the_schedules_still_to_fire() {
        let (tx_user, _) = tokio::sync::mpsc::channel(1);
        let (tx_control, _) = tokio::sync::mpsc::channel(1);
        let session = crate::session::Session::new(
            "m".to_string(),
            "medium".to_string(),
            "general".to_string(),
            tx_user,
            tx_control,
            std::sync::Arc::default(),
            std::sync::Arc::default(),
            None,
        );
        let mut app = App::new(std::sync::Arc::clone(&session));
        let mut terminal = Terminal::new(TestBackend::new(200, 10)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(
            !status(&terminal).contains("scheduled"),
            "{}",
            status(&terminal)
        );
        let dir = crate::tools::temp_dir();
        session.run_schedules(crate::schedules::Schedules::new(&dir, &dir, "test-session"));
        let store = session.schedules().unwrap();
        store.remind("in 20m a").unwrap();
        let paused = store.remind("every 1h b").unwrap();
        store.pause(&paused.id, true).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(
            status(&terminal).contains(" 1 scheduled "),
            "{}",
            status(&terminal)
        );
        store.pause(&paused.id, false).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(
            status(&terminal).contains(" 2 scheduled "),
            "{}",
            status(&terminal)
        );
    }

    #[test]
    fn status_bar_shows_the_goal_state_without_accounting() {
        let (tx_user, _) = tokio::sync::mpsc::channel(1);
        let (tx_control, _) = tokio::sync::mpsc::channel(1);
        let session = crate::session::Session::new(
            "m".to_string(),
            "medium".to_string(),
            "general".to_string(),
            tx_user,
            tx_control,
            std::sync::Arc::default(),
            std::sync::Arc::default(),
            None,
        );
        let mut app = App::new(std::sync::Arc::clone(&session));
        let mut terminal = Terminal::new(TestBackend::new(200, 10)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!status(&terminal).contains("goal"), "{}", status(&terminal));

        session.on_agent(crate::agent::AgentEvent::Goal(Some(
            crate::goal::Goal::new("ship it"),
        )));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(
            status(&terminal).contains("goal active "),
            "{}",
            status(&terminal)
        );

        assert!(!status(&terminal).contains('∞'), "{}", status(&terminal));
        let mut goal = crate::goal::Goal::new("ship it");
        goal.pause("interrupted");
        session.on_agent(crate::agent::AgentEvent::Goal(Some(goal)));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(
            status(&terminal).contains("goal paused "),
            "{}",
            status(&terminal)
        );
        session.on_agent(crate::agent::AgentEvent::Goal(None));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!status(&terminal).contains("goal"), "{}", status(&terminal));
    }

    #[tokio::test]
    async fn monitor_cards_update_while_the_session_is_idle() {
        let mut app = App::detached();
        let session = std::sync::Arc::clone(app.session());
        let monitors = session.monitors();
        let dir = crate::tools::temp_dir();
        let path = dir.join("progress.json");
        let snapshot = |newupdate, baseline| {
            serde_json::json!({
            "summary":"Benchmarking matrix multiply",
            "tracks":[{"id":"newupdate","current":newupdate,"total":65},{"id":"baseline","current":baseline,"total":65}],
            "metrics":[{"label":"RAM","value":2.1,"unit":"GB"}],
            "details":["sample 3 of 10"]
        }).to_string()
        };
        std::fs::write(&path, snapshot(31, 63)).unwrap();
        let id = monitors
            .add(crate::monitor::Spec {
                name: "Benchmark".into(),
                command: format!("cat '{}'", path.display()),
                workdir: dir.clone(),
                interval_secs: 1,
                timeout_secs: 2,
                hooks: vec![],
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while monitors.views()[0].snapshot.is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("31/65") && text.contains("63/65"), "{text}");
        assert!(
            text.contains("RAM: 2.1 GB") && text.contains("sample 3 of 10"),
            "{text}"
        );
        assert!(!app.working);
        let area = app.monitor_area.unwrap();
        panel_click(&mut app, area);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.monitor_area.unwrap().height, 1);
        assert!(!App::new(std::sync::Arc::clone(&session)).monitors_collapsed);
        assert!(!app.plan_collapsed);
        assert!(screen(&terminal).contains("monitors · /monitor"));
        assert!(!screen(&terminal).contains("31/65"));
        std::fs::write(&path, snapshot(32, 64)).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while monitors.views()[0].snapshot.as_ref().unwrap().tracks[0].current != 32.0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!screen(&terminal).contains("32/65"));
        let area = app.monitor_area.unwrap();
        panel_click(&mut app, area);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.monitor_area.unwrap().height > 1);
        let text = screen(&terminal);
        assert!(text.contains("32/65") && text.contains("64/65"), "{text}");
        monitors.control(&id, "dismiss").unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!screen(&terminal).contains("monitors · /monitor"));
        assert!(app.monitor_area.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn panel_click(app: &mut App, area: Rect) {
        let event = |kind| MouseEvent {
            kind,
            column: area.x + 2,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(app.on_mouse(event(MouseEventKind::Down(MouseButton::Left))));
        app.on_mouse(event(MouseEventKind::Up(MouseButton::Left)));
    }

    #[test]
    fn the_plan_panel_keeps_the_step_in_progress_in_view_and_goes_once_done() {
        let (tx_user, _) = tokio::sync::mpsc::channel(1);
        let (tx_control, _) = tokio::sync::mpsc::channel(1);
        let session = crate::session::Session::new(
            "m".to_string(),
            "medium".to_string(),
            "general".to_string(),
            tx_user,
            tx_control,
            std::sync::Arc::default(),
            std::sync::Arc::default(),
            None,
        );
        let mut app = App::new(std::sync::Arc::clone(&session));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let plan = |at: usize, of: usize| {
            let steps: Vec<serde_json::Value> = (0..of)
                .map(|i| {
                    let status = match i.cmp(&at) {
                        std::cmp::Ordering::Less => "completed",
                        std::cmp::Ordering::Equal => "in_progress",
                        std::cmp::Ordering::Greater => "pending",
                    };
                    serde_json::json!({"step": format!("step {i}"), "status": status})
                })
                .collect();
            crate::plan::Plan::parse(&serde_json::json!({"plan": steps, "explanation": "why"}))
                .unwrap()
        };
        session.on_agent(crate::agent::AgentEvent::Plan(plan(8, 10)));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains(" plan 8/10 "), "{text}");
        assert!(text.contains("▸ step 8"), "{text}");
        assert!(text.contains("○ step 9"), "{text}");
        assert!(text.contains("✓ step 7"), "{text}");
        assert!(!text.contains("step 0"), "{text}");
        assert!(!text.contains(" why "), "{text}");
        assert_eq!(session.state().plan, plan(8, 10));
        let area = app.plan_area.unwrap();
        panel_click(&mut app, area);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.plan_area.unwrap().height, 1);
        assert!(!App::new(std::sync::Arc::clone(&session)).plan_collapsed);
        assert!(screen(&terminal).contains(" plan 8/10 "));
        assert!(!screen(&terminal).contains("step 8"));
        assert_eq!(session.state().plan, plan(8, 10));
        assert!(!app.monitors_collapsed);
        let area = app.plan_area.unwrap();
        panel_click(&mut app, area);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.plan_area.unwrap().height, 8);
        assert!(screen(&terminal).contains("▸ step 8"));

        // Every step done and the session idle: nothing left to watch.
        session.on_agent(crate::agent::AgentEvent::Plan(plan(10, 10)));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(
            !screen(&terminal).contains(" plan "),
            "{}",
            screen(&terminal)
        );
        let mut goal = crate::goal::Goal::new("ship");
        goal.plan = plan(10, 10);
        goal.state = crate::goal::State::Complete;
        session.on_agent(crate::agent::AgentEvent::Goal(Some(goal)));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(
            screen(&terminal).contains(" plan 10/10 "),
            "{}",
            screen(&terminal)
        );
        assert!(
            screen(&terminal).contains("✓ step 9"),
            "{}",
            screen(&terminal)
        );
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
        let bar = status(&terminal);
        assert!(bar.contains("cache break: input[3] "), "{bar}");
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
    fn the_mode_chip_sits_on_the_prompt_border_not_the_status_bar() {
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
                // The prompt's bottom border, the row above the status bar.
                let border = screen.lines().rev().nth(1).unwrap();
                assert!(border.contains(&chip), "{screen}");
                assert!(!status(&terminal).contains(name), "{screen}");

                let y = terminal.backend().buffer().area.height - 2;
                let x = border[..border.find(name).unwrap()].chars().count() as u16;
                assert_eq!(terminal.backend().buffer()[(x, y)].fg, colour);
            }
        }
        assert!(
            !status(&terminal).contains("shift+tab"),
            "the status bar keeps its other hints but not the mode"
        );
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
    fn the_history_search_takes_the_prompt_and_lists_the_matches() {
        let mut app = App::detached();
        for text in ["fix the parser", "run the tests", "fix the\nlexer"] {
            app.history.push(text).unwrap();
        }
        let mut search = crate::search::Search::new(app.history.entries());
        search.insert("fix");
        app.search = Some(search);

        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        for part in [
            "history · 2 of 3",
            "search: fix",
            "fix the ↵ lexer",
            "fix the parser",
            "esc close",
        ] {
            assert!(shown.contains(part), "{part} missing from {shown}");
        }
        assert!(!shown.contains("run the tests"), "{shown}");
        assert!(app.input_area.is_none());
    }

    #[test]
    fn the_model_picker_takes_the_prompt_and_shows_both_lists() {
        use crate::models::{Catalogue, Effort, Picker};

        let model = |id: &str, label: &str, efforts: &[&str]| crate::models::Model {
            id: id.to_string(),
            label: label.to_string(),
            detail: "does things".to_string(),
            efforts: efforts
                .iter()
                .map(|name| Effort {
                    name: name.to_string(),
                    detail: String::new(),
                })
                .collect(),
            default_effort: Some("medium".to_string()),
            window: None,
        };
        let mut app = App::detached();
        app.model = "gpt-5.5".to_string();
        app.effort = "high".to_string();
        let mut picker = Picker::new("gpt-5.5", "high");
        picker.fill(Catalogue {
            models: vec![
                model("gpt-5.5", "GPT-5.5", &["low", "medium", "high"]),
                model("ollama:gemma4:e2b", "gemma4:e2b", &[]),
            ],
            notes: vec!["ollama: no server at http://localhost:11434".to_string()],
        });
        app.picker = Some(picker);

        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        for part in [
            "pick a model",
            "GPT-5.5",
            "gemma4:e2b",
            "no server at",
            "enter choose",
        ] {
            assert!(shown.contains(part), "{part} missing from {shown}");
        }
        // No prompt to type into while the picker is up.
        assert!(app.input_area.is_none());

        // Enter on a model that takes an effort asks which one before switching.
        app.on_key(ratatui::crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(shown.contains("GPT-5.5 · pick an effort"), "{shown}");
        assert!(shown.contains("medium"), "{shown}");
        assert!(shown.contains("enter switch"), "{shown}");
        assert!(app.picker.is_some(), "still asking, nothing switched yet");
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

    #[test]
    fn a_url_in_a_message_is_drawn_as_an_osc_8_link() {
        let mut app = App::detached();
        app.entries().push(Entry::Assistant(
            "read [the docs](https://example.com/d) first".to_string(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let linked: String = buffer
            .content()
            .iter()
            .filter(|cell| cell.symbol().contains("\x1b]8;id="))
            .map(|cell| {
                assert!(cell.symbol().contains(";https://example.com/d\x1b\\"));
                assert!(cell.symbol().ends_with("\x1b]8;;\x1b\\"));
                cell.symbol()
                    .split('\\')
                    .nth(1)
                    .unwrap()
                    .chars()
                    .next()
                    .unwrap()
            })
            .collect();
        assert_eq!(linked, "https://example.com/d");
    }

    #[test]
    fn hovering_a_wrapped_link_underlines_every_row_of_it_and_names_it_below() {
        let mut app = App::detached();
        let url = "https://example.com/a/very/long/path/to/wrap";
        app.entries().push(Entry::Assistant(format!("see {url}")));
        let mut terminal = Terminal::new(TestBackend::new(30, 16)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.links.len(), 1);
        let spans = app.links[0].spans.clone();
        assert!(spans.len() > 1, "{spans:?}");
        let underlined = |terminal: &Terminal<TestBackend>| {
            spans.iter().all(|(y, xs)| {
                xs.clone().all(|x| {
                    terminal.backend().buffer()[(x, *y)]
                        .modifier
                        .contains(Modifier::UNDERLINED)
                })
            })
        };
        assert!(!underlined(&terminal));

        let (y, xs) = spans.last().unwrap();
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: xs.start,
            row: *y,
            modifiers: KeyModifiers::NONE,
        });
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(underlined(&terminal));
        let shown = screen(&terminal);
        assert!(shown.contains("↗ https://example.com/"), "{shown}");

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(!underlined(&terminal));
        assert!(!screen(&terminal).contains('↗'));
    }

    #[test]
    fn the_password_box_shows_the_command_and_never_the_password() {
        let mut app = App::detached();
        let (reply, _answer) = tokio::sync::oneshot::channel();
        app.ask_password(crate::askpass::Request {
            command: "sudo -A launchctl kickstart system/x".to_string(),
            prompt: "[sudo] password for u:".to_string(),
            reply,
        });
        "s3cret".chars().for_each(|c| app.typed.push(c));
        let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        assert!(
            shown.contains("sudo -A launchctl kickstart system/x"),
            "{shown}"
        );
        assert!(shown.contains("[sudo] password for u: ********"), "{shown}");
        assert!(!shown.contains("s3cret"), "{shown}");
    }

    fn key(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn moved(column: u16, row: u16) -> MouseEvent {
        left(MouseEventKind::Moved, column, row)
    }

    #[test]
    fn the_bg_chip_shows_while_something_runs_and_lights_under_the_pointer() {
        let mut app = App::detached();
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(
            !status(&terminal).contains("process"),
            "{}",
            status(&terminal)
        );
        assert!(app.chip.is_none());

        app.on_event(Event::Background(3));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(
            status(&terminal).ends_with(" 3 processes"),
            "{}",
            status(&terminal)
        );
        let chip = app.chip.unwrap();
        assert_eq!((chip.right(), chip.y), (80, 9));
        let cell = status_cell(&terminal, "3 processes");
        assert_eq!(cell.fg, Color::Black);
        assert_eq!(cell.bg, Color::Cyan);
        assert!(cell.modifier.contains(Modifier::BOLD));
        assert!(!cell.modifier.contains(Modifier::UNDERLINED));

        assert!(
            app.on_mouse(moved(chip.x + 1, chip.y)),
            "onto the chip redraws"
        );
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let cell = status_cell(&terminal, "3 processes");
        assert_eq!(cell.fg, Color::Black);
        assert_eq!(cell.bg, Color::Cyan);
        assert!(cell.modifier.contains(Modifier::UNDERLINED));
        assert!(
            !app.on_mouse(moved(chip.x + 2, chip.y)),
            "along it is no change"
        );
        assert!(app.on_mouse(moved(2, 2)), "off it redraws");

        app.on_event(Event::Background(1));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(status(&terminal).ends_with(" 1 process"));

        app.on_event(Event::Background(0));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(
            !status(&terminal).contains("process"),
            "{}",
            status(&terminal)
        );
        assert!(app.chip.is_none());
    }

    /// An app whose background is a bash session, a child, a schedule and a server.
    fn with_background() -> App {
        use crate::background::{Kind, tests::Fake, tests::row};
        let app = App::detached();
        let now = std::time::SystemTime::now();
        let ago = |secs| Some(now - Duration::from_secs(secs));
        let mut bash = row(Kind::Bash, "900001", "running");
        bash.label = "npm run dev".to_string();
        bash.started = ago(125);
        bash.pid = Some(4242);
        bash.detail = "ready on http://localhost:5173\nGET / 200\n".to_string();
        let mut schedule = row(Kind::Schedule, "s9", "scheduled");
        schedule.label = "check CI".to_string();
        schedule.started = ago(600);
        schedule.detail = "s9 `in 20m` next 17:30".to_string();
        let mut mcp = row(Kind::Mcp, "fs", "connected");
        mcp.label = "fs".to_string();
        mcp.detail = "project, 4 tools".to_string();
        let fake = Fake::default();
        fake.0.lock().unwrap().extend([bash, schedule, mcp]);
        app.session().watch_background(vec![Box::new(fake)]);
        app.session().publish(Event::ChildStarted {
            id: "c1".to_string(),
            identity: "worker".to_string(),
            model: "gpt-5.5".to_string(),
            description: "read the tests".to_string(),
            task: "go".to_string(),
        });
        app
    }

    /// The rows the overlay drew, which is the transcript's area.
    fn overlay(terminal: &Terminal<TestBackend>, app: &App) -> Vec<String> {
        let area = app.bg.as_ref().and_then(|bg| bg.area).unwrap();
        screen(terminal)
            .lines()
            .skip(area.y as usize)
            .take(area.height as usize)
            .map(str::to_string)
            .collect()
    }

    /// The screen row the list drew row `index` on.
    fn list_row(app: &App, index: usize) -> u16 {
        app.bg.as_ref().unwrap().area.unwrap().y + 1 + index as u16
    }

    /// The list, opened with a click on the chip.
    fn open_list(app: &mut App, terminal: &mut Terminal<TestBackend>) {
        app.on_event(Event::Background(4));
        terminal.draw(|frame| render(frame, app)).unwrap();
        let chip = app.chip.unwrap();
        assert!(click(app, chip.x + 1, chip.y));
        terminal.draw(|frame| render(frame, app)).unwrap();
    }

    #[tokio::test]
    async fn the_background_list_at_80_columns() {
        let mut app = with_background();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        open_list(&mut app, &mut terminal);
        let shown = overlay(&terminal, &app);
        assert_eq!(
            shown[..6],
            [
                "┌ background (4) ──────────────────────────────────────────────────────────────┐",
                "│ bash     npm run dev                                           2m  running   │",
                "│ child    read the tests                                        0s  running   │",
                "│ schedule check CI                                             10m  scheduled │",
                "│ mcp      fs                                                     -  connected │",
                "│                                                                              │",
            ],
            "{}",
            shown.join("\n")
        );
        assert!(
            shown
                .last()
                .unwrap()
                .ends_with("↑↓ select · enter open · esc close ┘"),
            "{}",
            shown.join("\n")
        );

        // A click on the bash row inspects it.
        let y = list_row(&app, 0);
        assert!(click(&mut app, 4, y));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = overlay(&terminal, &app);
        assert_eq!(
            shown[..9],
            [
                "┌ bash 900001 ─────────────────────────────────────────────────────────────────┐",
                "│ ‹ back                                                              [x kill] │",
                "│ command  npm run dev                                                         │",
                "│ pid      4242                                                                │",
                "│ age      2m                                                                  │",
                "│ state    running                                                             │",
                "│ output                                                                       │",
                "│  ready on http://localhost:5173                                              │",
                "│  GET / 200                                                                   │",
            ],
            "{}",
            shown.join("\n")
        );
    }

    #[tokio::test]
    async fn the_background_list_at_40_columns() {
        let mut app = with_background();
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        open_list(&mut app, &mut terminal);
        let shown = overlay(&terminal, &app);
        assert_eq!(
            shown[..5],
            [
                "┌ background (4) ──────────────────────┐",
                "│ bash     npm run dev   2m  running   │",
                "│ child    read the t…   0s  running   │",
                "│ schedule check CI     10m  scheduled │",
                "│ mcp      fs             -  connected │",
            ],
            "{}",
            shown.join("\n")
        );
        assert!(
            status(&terminal).ends_with(" 4 processes"),
            "{}",
            status(&terminal)
        );

        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = overlay(&terminal, &app);
        assert_eq!(
            shown[..6],
            [
                "┌ schedule s9 ─────────────────────────┐",
                "│ ‹ back                    [x cancel] │",
                "│ prompt   check CI                    │",
                "│ age      10m                         │",
                "│ state    scheduled                   │",
                "│ when     s9 `in 20m` next 17:30      │",
            ],
            "{}",
            shown.join("\n")
        );
    }

    #[tokio::test]
    async fn a_kill_takes_a_second_click_and_a_stray_click_disarms_it() {
        let mut app = with_background();
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        open_list(&mut app, &mut terminal);
        let y = list_row(&app, 0);
        assert!(click(&mut app, 4, y));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let shown = screen(&terminal);
        let (x, y) = shown
            .lines()
            .enumerate()
            .find_map(|(y, row)| {
                row.find("[x kill]")
                    .map(|at| (row[..at].chars().count() as u16, y as u16))
            })
            .unwrap();
        assert!(click(&mut app, x + 1, y));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("[x again to kill]"));
        // A click anywhere else in the inspector puts it back as it was.
        assert!(click(&mut app, 4, y + 3));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("[x kill]"));
        let ended = |app: &App| {
            app.session().entries().list.iter().any(|entry| {
                matches!(entry, Entry::Error(text) if text == "bash session 900001 has already ended")
            })
        };
        assert!(click(&mut app, x + 1, y));
        assert!(!ended(&app));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        // The second click lands on the button as the armed frame drew it.
        let x = screen(&terminal)
            .lines()
            .nth(y as usize)
            .and_then(|row| row.find("[x again").map(|at| row[..at].chars().count()))
            .unwrap() as u16;
        assert!(click(&mut app, x + 1, y));
        assert!(ended(&app), "{:?}", app.session().entries().list);

        // The way back, and esc out of the list.
        assert!(click(&mut app, 3, y));
        assert!(app.bg.as_ref().unwrap().open.is_none());
        key(&mut app, KeyCode::Esc);
        assert!(app.bg.is_none());
    }

    #[tokio::test]
    async fn a_child_row_opens_its_pane_and_ctrl_s_toggles_the_list() {
        let mut app = with_background();
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.bg.is_some());
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(app.bg.is_none());
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let y = list_row(&app, 1);
        assert!(click(&mut app, 4, y));
        assert!(app.bg.is_none());
        assert_eq!(app.inside.as_ref().map(|i| i.id.as_str()), Some("c1"));
    }

    #[tokio::test]
    async fn the_chip_does_nothing_while_the_picker_or_search_holds_the_prompt() {
        let mut app = with_background();
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        app.on_event(Event::Background(4));
        app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(app.search.is_some());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let chip = app.chip.unwrap();
        click(&mut app, chip.x + 1, chip.y);
        assert!(app.bg.is_none(), "no list the screen would not show");
        // The keys still reach the search on screen.
        key(&mut app, KeyCode::Esc);
        assert!(app.search.is_none());

        app.picker = Some(Picker::new("m", "medium"));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let chip = app.chip.unwrap();
        click(&mut app, chip.x + 1, chip.y);
        assert!(app.bg.is_none());
        key(&mut app, KeyCode::Esc);
        assert!(app.picker.is_none());

        // With the prompt back, the chip opens the list again.
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let chip = app.chip.unwrap();
        assert!(click(&mut app, chip.x + 1, chip.y));
        assert!(app.bg.is_some());
    }

    #[tokio::test]
    async fn cancelling_a_schedule_from_the_list_removes_it() {
        let mut app = App::detached();
        let dir = crate::tools::temp_dir();
        app.session()
            .run_schedules(crate::schedules::Schedules::new(&dir, &dir, "test-session"));
        let store = app.session().schedules().unwrap();
        let row = store.remind("in 20m check CI").unwrap();
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.background, 1);
        assert!(screen(&terminal).contains("schedule check CI"));
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Char('x'));
        assert_eq!(store.list().unwrap().0.len(), 1, "one x only arms it");
        key(&mut app, KeyCode::Char('x'));
        assert!(store.list().unwrap().0.is_empty());
        let bg = app.bg.as_ref().unwrap();
        assert!(bg.open.is_none() && bg.rows.is_empty());
        assert_eq!(app.background, 0);
        let said = format!("cancelled {}", row.id);
        assert!(
            app.session()
                .entries()
                .list
                .iter()
                .any(|entry| matches!(entry, Entry::Info(text) if text.starts_with(&said))),
            "{:?}",
            app.session().entries().list
        );
    }
}
