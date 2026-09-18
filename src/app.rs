//! UI state and the event handling that mutates it.

use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use std::collections::HashSet;
use std::ops::Range;
use std::sync::{Arc, MutexGuard};
use std::time::{Duration, Instant};

use tui_input::InputRequest;
use tui_input::backend::crossterm::to_input_request;

use crate::client::Usage;
use crate::clipboard;
use crate::diff::DiffView;
use crate::entries::Entries;
pub use crate::entries::Entry;
use crate::input::{Editor, History};
use crate::limits::RateLimits;
use crate::permissions::{Answer, Mode, Remember};
use crate::profile::{self, Transcript};
use crate::session::{Approval, Event, Session};
use crate::skills::Skill;
use crate::workflow::{self, Found};

/// Lines a mouse wheel notch moves the transcript.
const WHEEL_LINES: usize = 3;

/// Presses on one cell this close together count as a double or triple click.
const MULTI_CLICK: Duration = Duration::from_millis(400);

/// A selection over the transcript's wrapped lines, as (line, column) cells. Anchoring
/// to the wrapped buffer rather than the screen keeps it put while the view scrolls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Selection {
    anchor: (usize, usize),
    head: (usize, usize),
}

impl Selection {
    /// The word around `cell`: the run of chars sharing its class.
    fn word(text: &str, cell: (usize, usize)) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let Some(&at) = chars.get(cell.1) else {
            return Self::line(text, cell);
        };
        let mut start = cell.1;
        let mut end = cell.1;
        while start > 0 && class(chars[start - 1]) == class(at) {
            start -= 1;
        }
        while end + 1 < chars.len() && class(chars[end + 1]) == class(at) {
            end += 1;
        }
        Self {
            anchor: (cell.0, start),
            head: (cell.0, end),
        }
    }

    /// The whole line the cell sits on.
    fn line(text: &str, cell: (usize, usize)) -> Self {
        Self {
            anchor: (cell.0, 0),
            head: (cell.0, text.chars().count().saturating_sub(1)),
        }
    }

    /// The ends in document order, the far one past the cell under the pointer so both
    /// ends of a drag are included.
    fn range(&self) -> ((usize, usize), (usize, usize)) {
        let (from, to) = match self.anchor <= self.head {
            true => (self.anchor, self.head),
            false => (self.head, self.anchor),
        };
        (from, (to.0, to.1 + 1))
    }

    /// The selected columns of the wrapped line `line`, which holds `len` chars.
    pub fn on_line(&self, line: usize, len: usize) -> Option<Range<usize>> {
        let (from, to) = self.range();
        if line < from.0 || line > to.0 {
            return None;
        }
        let start = if line == from.0 { from.1.min(len) } else { 0 };
        let end = if line == to.0 { to.1.min(len) } else { len };
        (start < end).then_some(start..end)
    }
}

/// Word, whitespace or punctuation: a double click takes the run of one class.
fn class(c: char) -> u8 {
    match c {
        c if c.is_alphanumeric() || c == '_' => 0,
        c if c.is_whitespace() => 1,
        _ => 2,
    }
}
pub struct App {
    /// Screen rows of each visible entry, filled in by the renderer.
    pub rows: Vec<(Range<u16>, usize)>,
    /// The entry under the mouse.
    pub hover: Option<usize>,
    mouse_row: Option<u16>,
    /// Entries whose badge stays up.
    pub pinned: HashSet<usize>,
    /// Tool output entries shown in full rather than collapsed.
    pub expanded: HashSet<usize>,
    pub all_badges: bool,
    /// Clickable approval choices and the key each stands for, filled in by the renderer.
    pub buttons: Vec<(Rect, KeyCode)>,
    /// The input text's area, first visible row and wrap width, filled in by the renderer.
    pub input_area: Option<(Rect, usize, usize)>,
    /// The transcript scrollbar, filled in by the renderer when the transcript overflows.
    pub scrollbar: Option<Rect>,
    /// A left drag that started on the scrollbar is in progress.
    dragging: bool,
    /// A left drag that started in the input is selecting text.
    selecting: bool,
    /// Plain text of each wrapped transcript line, filled in by the renderer.
    pub lines: Vec<String>,
    /// The transcript's text area, filled in by the renderer.
    pub transcript_area: Option<Rect>,
    /// The selected span of the transcript, drawn reversed and copied by `ctrl+y`.
    pub selection: Option<Selection>,
    /// The wrapped cell a transcript drag anchors at.
    anchor: Option<(usize, usize)>,
    /// Where a left press landed, until the pointer moves off that cell.
    press: Option<(u16, u16)>,
    /// The last press, for spotting a double or triple click.
    clicks: Option<(Instant, u16, u16, u8)>,
    /// Mouse capture is on; `/mouse` turns it off for the terminal's own selection.
    pub mouse: bool,
    pub input: Editor,
    /// Submitted prompts, for `ctrl+p` and `ctrl+n`.
    pub history: History,
    pub working: bool,
    /// Prompts waiting behind the running turn, for the status bar.
    pub queued: usize,
    /// The tool call waiting for approval.
    pub pending: Option<Approval>,
    pub scroll: usize,
    pub max_scroll: usize,
    /// Transcript viewport height, filled in by the renderer so page keys match the view.
    pub page: usize,
    pub follow: bool,
    pub spinner: usize,
    pub model: String,
    pub identity: String,
    pub mode: Mode,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub last_usage: Option<Usage>,
    /// The field that broke the prompt cache, until the next clean call.
    pub cache_break: Option<String>,
    /// The hit percent of the last judged call, when it missed.
    pub cache_miss: Option<f64>,
    /// Judged calls have been missing in a row, until one hits again.
    pub cache_stalled: bool,
    pub rate_limits: Option<RateLimits>,
    /// The rate-limit segment of the status bar, filled in by the renderer.
    pub limits_area: Option<Rect>,
    /// The mouse is over the rate-limit segment, so it shows reset times.
    pub limits_hover: bool,
    /// The skills in the system prompt, for `/skills`.
    pub skills: Vec<Skill>,
    /// The session's MCP servers, for `/mcp`.
    pub mcp: Option<Arc<crate::mcp::Hub>>,
    /// The workflow definitions, for `/workflows` and `/workflow`.
    pub workflows: Found,
    /// The `/diff` pane, shown instead of the transcript while open.
    pub diff: Option<DiffView>,
    pub quit: bool,
    session: Arc<Session>,
}

impl App {
    pub fn new(session: Arc<Session>) -> Self {
        session.entries().push(Entry::Info(
            "bhai · bash, read, write and edit; every change needs your approval. Type a task and hit enter."
                .to_string(),
        ));
        Self {
            rows: Vec::new(),
            hover: None,
            mouse_row: None,
            pinned: HashSet::new(),
            expanded: HashSet::new(),
            all_badges: false,
            buttons: Vec::new(),
            input_area: None,
            scrollbar: None,
            dragging: false,
            selecting: false,
            lines: Vec::new(),
            transcript_area: None,
            selection: None,
            anchor: None,
            press: None,
            clicks: None,
            mouse: true,
            input: Editor::default(),
            history: History::default(),
            working: false,
            queued: 0,
            pending: None,
            scroll: 0,
            max_scroll: 0,
            page: 10,
            follow: true,
            spinner: 0,
            model: session.state().model,
            identity: session.state().identity,
            mode: session.state().mode,
            tokens_in: 0,
            tokens_out: 0,
            last_usage: None,
            cache_break: None,
            cache_miss: None,
            cache_stalled: false,
            rate_limits: None,
            limits_area: None,
            limits_hover: false,
            skills: Vec::new(),
            mcp: None,
            workflows: Found::default(),
            diff: None,
            quit: false,
            session,
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        // An approval is modal: nothing else happens until it is answered.
        if self.pending.is_some() {
            match key.code {
                KeyCode::Char('y') => self.answer(Answer::Accept(None)),
                KeyCode::Char('a') => self.answer(Answer::Accept(Some(Remember::Exact))),
                KeyCode::Char('p') => self.answer(Answer::Accept(Some(Remember::Prefix))),
                KeyCode::Char('r') | KeyCode::Char('n') | KeyCode::Esc => {
                    self.answer(Answer::Reject)
                }
                KeyCode::Char('c') if ctrl => self.interrupt(),
                _ => {}
            }
            return;
        }

        if let Some(diff) = &mut self.diff
            && !(ctrl && key.code == KeyCode::Char('c'))
        {
            if !diff.on_key(key.code) {
                self.diff = None;
            }
            return;
        }

        match key.code {
            KeyCode::Char('c') if ctrl => {
                if self.working {
                    self.interrupt();
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Char('d') if ctrl && self.input.is_empty() => self.quit = true,
            KeyCode::Esc if self.working => self.interrupt(),
            KeyCode::Esc if self.selection.is_some() => self.selection = None,
            KeyCode::Char('t') if ctrl => self.all_badges = !self.all_badges,
            KeyCode::Char('y') if ctrl => self.copy(),
            KeyCode::Char('v') if ctrl => self.paste(),
            KeyCode::Char('a') if ctrl && !self.input.is_empty() => self.input.select_all(),
            KeyCode::BackTab => self.mode = self.session.cycle_mode(),
            // Most terminals cannot report shift+enter, so alt+enter and ctrl+j also work.
            KeyCode::Enter if !key.modifiers.is_empty() => self.input.newline(),
            KeyCode::Char('j') if ctrl => self.input.newline(),
            KeyCode::Enter => self.submit(),
            KeyCode::Char('p') if ctrl => {
                if let Some(text) = self.history.prev(self.input.value()) {
                    self.input.set(text);
                }
            }
            KeyCode::Char('n') if ctrl => {
                if let Some(text) = self.history.next() {
                    self.input.set(text);
                }
            }
            KeyCode::PageUp => self.scroll_by(-(self.page as isize)),
            KeyCode::PageDown => self.scroll_by(self.page as isize),
            // Left/Right belong to the cursor, so the transcript scrolls with up/down
            // unless the input has lines to move between.
            // Up and down move within the input whenever it draws on more than one row.
            KeyCode::Up if self.input_rows() > 1 => {
                self.input.selecting(shift);
                self.input.move_line(-1, self.input_width());
            }
            KeyCode::Down if self.input_rows() > 1 => {
                self.input.selecting(shift);
                self.input.move_line(1, self.input_width());
            }
            KeyCode::Up => self.scroll_by(-1),
            KeyCode::Down => self.scroll_by(1),
            _ => {
                if let Some(request) = input_request(key) {
                    self.input.handle(request, shift);
                }
            }
        }
    }

    /// Bracketed paste: the text goes into the input as typed, newlines and all.
    pub fn on_paste(&mut self, text: &str) {
        if self.pending.is_none() && self.diff.is_none() {
            self.input.insert(text);
        }
    }

    /// Returns whether the screen needs a redraw.
    pub fn on_mouse(&mut self, mouse: MouseEvent) -> bool {
        // With an approval up, clicks still reach its buttons.
        if let Some(diff) = self.diff.as_mut().filter(|_| self.pending.is_none()) {
            return diff_mouse(diff, mouse);
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_by(-(WHEEL_LINES as isize)),
            MouseEventKind::ScrollDown => self.scroll_by(WHEEL_LINES as isize),
            MouseEventKind::Moved => {
                self.mouse_row = Some(mouse.row);
                let at = Position::new(mouse.column, mouse.row);
                let over = self.limits_area.is_some_and(|area| area.contains(at));
                let changed = std::mem::replace(&mut self.limits_hover, over) != over;
                return self.rehover() | changed;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.mouse_row = Some(mouse.row);
                return self.click(mouse.column, mouse.row);
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => self.drag_to(mouse.row),
            MouseEventKind::Drag(MouseButton::Left) if self.selecting => {
                return self.select_to(mouse.column, mouse.row);
            }
            MouseEventKind::Drag(MouseButton::Left) if self.anchor.is_some() => {
                return self.drag_selection(mouse.column, mouse.row);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.dragging = false;
                self.selecting = false;
                self.anchor = None;
                // A press and release on one cell is a click, not a drag.
                let clicked = self.press.take() == Some((mouse.column, mouse.row));
                return clicked && self.click_entry(mouse.row);
            }
            _ => return false,
        }
        true
    }

    /// A left click: an approval choice acts as its key, the scrollbar starts a drag,
    /// the input moves its cursor, tool output expands or collapses and any other
    /// entry pins its badge. Returns whether the screen needs a redraw.
    fn click(&mut self, x: u16, y: u16) -> bool {
        let at = Position::new(x, y);
        let button = self.buttons.iter().find(|(area, _)| area.contains(at));
        if let Some(&(_, code)) = button.filter(|_| self.pending.is_some()) {
            self.on_key(KeyEvent::new(code, KeyModifiers::NONE));
            return true;
        }
        if self.scrollbar.is_some_and(|bar| bar.contains(at)) {
            self.dragging = true;
            self.drag_to(y);
            return true;
        }
        if let Some((area, top, width)) = self.input_area.filter(|(area, ..)| area.contains(at)) {
            if self.input.is_empty() {
                return false;
            }
            self.input.selecting(false);
            let row = top + (y - area.y) as usize;
            self.input.place(row, (x - area.x) as usize, width);
            self.selecting = true;
            return true;
        }
        // Whether it turns out to be a click is known only when the button comes up.
        self.press = Some((x, y));
        let Some(cell) = self.cell_at(x, y) else {
            return false;
        };
        self.anchor = Some(cell);
        let count = self.click_count(x, y);
        // A single press drops the old selection; a drag builds the new one.
        self.selection = match count {
            2 => Some(Selection::word(&self.lines[cell.0], cell)),
            3 => Some(Selection::line(&self.lines[cell.0], cell)),
            _ => None,
        };
        if count > 1 {
            self.press = None;
        }
        true
    }

    /// The release of a click that never moved: tool output expands or collapses and
    /// any other entry pins its badge. Returns whether the screen needs a redraw.
    fn click_entry(&mut self, y: u16) -> bool {
        let Some(entry) = self.entry_at(y) else {
            return false;
        };
        let set = if matches!(
            self.entries().list.get(entry),
            Some(Entry::Output(_) | Entry::Running { .. })
        ) {
            &mut self.expanded
        } else {
            &mut self.pinned
        };
        if !set.remove(&entry) {
            set.insert(entry);
        }
        true
    }

    /// The wrapped transcript line and column under a screen cell, clamped to the last
    /// line so a drag past the end still selects.
    fn cell_at(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let area = self
            .transcript_area
            .filter(|area| area.contains(Position::new(x, y)))?;
        let last = self.lines.len().checked_sub(1)?;
        let line = (self.scroll + (y - area.y) as usize).min(last);
        Some((line, (x - area.x) as usize))
    }

    /// Presses in a row on the same cell: 1, 2 or 3, then round again.
    fn click_count(&mut self, x: u16, y: u16) -> u8 {
        let now = Instant::now();
        let count = match self.clicks {
            Some((at, cx, cy, n)) if (cx, cy) == (x, y) && now - at < MULTI_CLICK => n % 3 + 1,
            _ => 1,
        };
        self.clicks = Some((now, x, y, count));
        count
    }

    /// Extend the transcript selection to the cell under the pointer.
    /// Returns whether the screen needs a redraw.
    fn drag_selection(&mut self, x: u16, y: u16) -> bool {
        // A drag reported on the press cell has not moved yet, so it is still a click.
        if self.press == Some((x, y)) {
            return false;
        }
        self.press = None;
        let (Some(anchor), Some(head)) = (self.anchor, self.cell_at(x, y)) else {
            return false;
        };
        let selection = Some(Selection { anchor, head });
        std::mem::replace(&mut self.selection, selection) != selection
    }

    /// The selected transcript text, lines joined and trailing spaces trimmed.
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        let (from, to) = selection.range();
        let text: Vec<String> = (from.0..=to.0.min(self.lines.len().checked_sub(1)?))
            .map(|line| {
                let chars = self.lines[line].chars();
                let range = selection.on_line(line, self.lines[line].chars().count());
                let range = range.unwrap_or(0..0);
                let part: String = chars.skip(range.start).take(range.len()).collect();
                part.trim_end().to_string()
            })
            .collect();
        text.iter()
            .any(|line| !line.is_empty())
            .then(|| text.join("\n"))
    }

    /// Extend the input's selection to the char under the pointer. The anchor lands on
    /// the char the drag started at, since the cursor is still there.
    /// Returns whether the screen needs a redraw.
    fn select_to(&mut self, x: u16, y: u16) -> bool {
        let Some((area, top, width)) = self.input_area else {
            return false;
        };
        self.input.selecting(true);
        let row = top + y.saturating_sub(area.y) as usize;
        self.input
            .place(row, x.saturating_sub(area.x) as usize, width);
        true
    }

    /// What `ctrl+y` copies: the transcript selection, else the input's own selection,
    /// else the whole input.
    fn copy_text(&self) -> String {
        self.selected_text()
            .or_else(|| self.input.selected())
            .unwrap_or_else(|| self.input.value().to_string())
    }

    /// `ctrl+y`: copy the selection, or the whole input when nothing is selected.
    fn copy(&mut self) {
        let text = self.copy_text();
        if text.is_empty() {
            return;
        }
        let chars = text.chars().count();
        match clipboard::copy(&text) {
            Ok(()) => self.note(Entry::Info(format!("copied {chars} chars"))),
            Err(err) => self.note(Entry::Error(format!("copy failed: {err}"))),
        }
    }

    /// `ctrl+v`: insert what a clipboard command reads back. The terminal's own paste
    /// arrives as a bracketed paste instead, which needs no clipboard command.
    fn paste(&mut self) {
        match clipboard::paste() {
            Some(text) => self.input.insert(&text),
            None => self.note(Entry::Info(
                "no clipboard reader here; use the terminal's own paste".to_string(),
            )),
        }
    }

    /// Scroll in proportion to where row `y` sits on the scrollbar.
    fn drag_to(&mut self, y: u16) {
        let Some(bar) = self.scrollbar else {
            return;
        };
        let track = bar.height.saturating_sub(1).max(1) as usize;
        let offset = y.saturating_sub(bar.y).min(bar.height.saturating_sub(1)) as usize;
        let target = (offset * self.max_scroll + track / 2) / track;
        self.scroll_by(target as isize - self.scroll as isize);
    }

    /// The entry drawn on screen row `y`.
    /// The width the input wraps at, from the last frame; a wide default before one.
    fn input_width(&self) -> usize {
        self.input_area.map_or(80, |(.., width)| width)
    }

    /// Rows the input draws on at that width.
    fn input_rows(&self) -> usize {
        self.input.rows(self.input_width()).len()
    }

    pub fn entry_at(&self, y: u16) -> Option<usize> {
        self.rows
            .iter()
            .find(|(rows, _)| rows.contains(&y))
            .map(|(_, entry)| *entry)
    }

    /// Point the hover at the entry under the mouse; true when that changed.
    pub fn rehover(&mut self) -> bool {
        let hover = self.mouse_row.and_then(|y| self.entry_at(y));
        std::mem::replace(&mut self.hover, hover) != hover
    }

    /// UI state for a published event; the session has already updated the entries.
    pub fn on_event(&mut self, event: Event) {
        match event {
            // Messages from any consumer (this TUI or the debug server) land here.
            Event::User(_) => {
                self.follow = true;
                self.working = true;
                // Nothing waits unless a turn runs, so a user message with a queue
                // behind it is the front of that queue starting.
                self.queued = self.queued.saturating_sub(1);
            }
            // The position is the length of the queue the prompt joined. Scrolling to it
            // is what a submitted prompt does, queued or not.
            Event::Queued { position, .. } => {
                self.follow = true;
                self.queued = position;
            }
            // An interrupt drops whatever was waiting.
            Event::Interrupted => self.queued = 0,
            Event::Approval {
                id,
                tool,
                command,
                offers,
            } => {
                self.pending = Some(Approval {
                    id,
                    tool,
                    command,
                    offers,
                });
            }
            Event::Resolved { id, .. } if self.pending.as_ref().is_some_and(|p| p.id == id) => {
                self.pending = None;
            }
            Event::Usage(usage) => {
                self.tokens_in += usage.input;
                self.tokens_out += usage.output;
                self.last_usage = Some(usage);
            }
            // Children count towards the status bar total, not the cache rate.
            Event::ChildUsage(usage) => {
                self.tokens_in += usage.input;
                self.tokens_out += usage.output;
            }
            Event::Cache(found) => self.cache_break = found.map(|f| f.field),
            Event::CacheHit(hit) => {
                self.cache_miss = hit
                    .hit_ratio
                    .filter(|_| hit.miss())
                    .map(|ratio| ratio * 100.0);
                self.cache_stalled &= hit.miss();
            }
            Event::CacheStalled(_) => self.cache_stalled = true,
            Event::RateLimits(limits) => self.rate_limits = Some(limits),
            Event::Mode(mode) => self.mode = mode,
            Event::TurnEnd => self.working = false,
            _ => {}
        }
    }

    pub fn tick(&mut self) {
        if self.working {
            self.spinner = self.spinner.wrapping_add(1);
        }
    }

    fn submit(&mut self) {
        if self.input.value().trim().is_empty() {
            return;
        }
        let message = self.input.take().trim().to_string();
        self.selection = None;
        if let Err(e) = self.history.push(&message) {
            self.note(Entry::Error(format!("could not save prompt history: {e}")));
        }
        if message == "/diff" {
            let dir = std::env::current_dir().unwrap_or_default();
            self.diff = Some(DiffView::open(&dir));
            return;
        }
        if message.starts_with("/context") {
            self.export_context();
            return;
        }
        if message == "/compact" {
            self.follow = true;
            match self.session.compact() {
                Ok(()) => self.working = true,
                Err(e) => self.note(Entry::Error(e.to_string())),
            }
            return;
        }
        if message.starts_with("/permissions") {
            self.follow = true;
            self.note(Entry::Info(self.session.permissions()));
            return;
        }
        if message == "/trust" || message == "/untrust" {
            self.follow = true;
            let result = match message.as_str() {
                "/trust" => self.session.trust(),
                _ => self.session.untrust(),
            };
            self.note(match result {
                Ok(text) => Entry::Info(text),
                Err(e) => Entry::Error(format!("{e:#}")),
            });
            return;
        }
        if let Some(rest) = message.strip_prefix("/as")
            && (rest.is_empty() || rest.starts_with(' '))
        {
            self.follow = true;
            self.note(Entry::Info(switch_notice(&self.identity, rest.trim())));
            return;
        }
        if message.starts_with("/mcp") {
            self.follow = true;
            self.note(Entry::Info(crate::mcp::report(self.mcp.as_deref())));
            return;
        }
        if message.starts_with("/workflows") {
            self.follow = true;
            self.note(Entry::Info(workflow::report(&self.workflows)));
            return;
        }
        if let Some(rest) = message.strip_prefix("/workflow")
            && (rest.is_empty() || rest.starts_with(' '))
        {
            self.follow = true;
            self.start_workflow(rest.trim());
            return;
        }
        if message.starts_with("/mouse") {
            self.follow = true;
            self.mouse = !self.mouse;
            self.selection = None;
            self.note(Entry::Info(mouse_notice(self.mouse)));
            return;
        }
        if message.starts_with("/skills") {
            self.follow = true;
            self.note(Entry::Info(skills_report(&self.skills)));
            return;
        }
        if let Some(rest) = message.strip_prefix("/queue")
            && (rest.is_empty() || rest.starts_with(' '))
        {
            self.follow = true;
            self.queue(rest.trim());
            return;
        }
        // The transcript entry arrives back as `Event::User` once the session accepts it.
        if let Err(e) = self.session.submit(message) {
            self.note(Entry::Error(e.to_string()));
        }
    }

    /// `/queue` lists the prompts waiting behind the turn; `/queue clear` drops them.
    fn queue(&mut self, rest: &str) {
        if rest == "clear" {
            if self.session.clear_queue() == 0 {
                self.note(Entry::Info("queue: nothing waiting".to_string()));
            }
            self.queued = 0;
            return;
        }
        let queued = self.session.queued();
        self.queued = queued.len();
        self.note(Entry::Info(queue_report(&queued)));
    }

    /// `/workflow <name> [input]`: the agent asks for confirmation before it launches
    /// anything, so this only hands the definition over.
    fn start_workflow(&mut self, rest: &str) {
        let (name, input) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        if name.is_empty() {
            self.note(Entry::Info(workflow::report(&self.workflows)));
            return;
        }
        let started = workflow::find(&self.workflows.workflows, name)
            .map_err(|e| format!("{e:#}"))
            .and_then(|found| {
                self.session
                    .workflow(found, input.trim().to_string())
                    .map_err(|e| e.to_string())
            });
        match started {
            Ok(()) => self.working = true,
            Err(e) => self.note(Entry::Error(e)),
        }
    }

    /// Handle `/context` locally: export the breakdown and report where it went.
    fn export_context(&mut self) {
        self.follow = true;
        let transcript = self.transcript();
        let session = Arc::clone(&self.session);
        tokio::spawn(async move {
            let event = match session.context().await {
                Some(mut profile) => {
                    profile.transcript = Some(transcript);
                    match profile::export(&profile, &profile::debug_dir()) {
                        Ok(path) => Event::Info(profile.summary(&path)),
                        Err(e) => Event::Error(format!("context export failed: {e:#}")),
                    }
                }
                None => Event::Error("the agent is not running".to_string()),
            };
            session.publish(event);
        });
    }

    /// The badge numbers of every attributed entry, with the session totals.
    fn transcript(&self) -> Transcript {
        let state = self.session.state();
        Transcript {
            entries: state.entries,
            totals: Usage {
                input: state.input_tokens,
                cached: state.cached_tokens,
                output: state.output_tokens,
                reasoning: state.reasoning_tokens,
            },
            calls: state.calls,
        }
    }

    /// The transcript, shared with the session.
    pub fn entries(&self) -> MutexGuard<'_, Entries> {
        self.session.entries()
    }

    /// Show a notice that only this TUI produced.
    fn note(&self, entry: Entry) {
        self.session.entries().push(entry);
    }

    /// A remember key does nothing unless the prompt offers that rule.
    fn answer(&mut self, answer: Answer) {
        let Some(pending) = &self.pending else {
            return;
        };
        if answer
            .remember()
            .is_some_and(|r| pending.offers.get(r).is_none())
        {
            return;
        }
        let id = pending.id;
        self.pending = None;
        self.session.answer(answer, Some(id));
    }

    fn interrupt(&mut self) {
        self.pending = None;
        self.session.interrupt();
    }

    fn scroll_by(&mut self, delta: isize) {
        let target = self.scroll as isize + delta;
        self.scroll = target.clamp(0, self.max_scroll as isize) as usize;
        self.follow = self.scroll >= self.max_scroll;
    }
}
/// What `/as` prints. The identity is fixed for the session, so switching needs a new one.
fn switch_notice(current: &str, name: &str) -> String {
    if name.is_empty() {
        return format!("identity: {current}. Usage: /as <name>");
    }
    format!(
        "identity: {current}. The identity is fixed for the session; to switch, quit and run: bhai --as {name}"
    )
}

/// What `/mouse` prints. Capture is what turns the wheel into scroll events and drags
/// into transcript selection, so turning it off hands both back to the terminal.
fn mouse_notice(on: bool) -> String {
    match on {
        true => "mouse: on. Drag selects, ctrl+y copies, the wheel scrolls.".to_string(),
        false => {
            "mouse: off. Select and copy with the terminal's own mouse; /mouse turns it back on."
                .to_string()
        }
    }
}

/// What `/queue` prints: each waiting prompt in the order it will run.
fn queue_report(queued: &[String]) -> String {
    if queued.is_empty() {
        return "queue: nothing waiting. /queue clear drops what is.".to_string();
    }
    let mut out = format!("queue: {} waiting", queued.len());
    for (i, text) in queued.iter().enumerate() {
        let line = text.trim().lines().next().unwrap_or_default();
        out.push_str(&format!("\n{:>3}. {}", i + 1, line));
    }
    out
}

/// What `/skills` prints: each skill, where it came from and its listing cost.
fn skills_report(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return "skills: none loaded".to_string();
    }
    let total: usize = skills.iter().map(|s| s.entry().len() + 1).sum();
    let mut out = format!(
        "skills: {} listed, about {} tokens",
        skills.len(),
        total.div_ceil(4)
    );
    for skill in skills {
        let tokens = (skill.entry().len() + 1).div_ceil(4);
        out.push_str(&format!(
            "\n{:>5} tok  {}  ({})",
            tokens, skill.name, skill.source
        ));
    }
    out
}

/// Mouse input while the diff pane is open. Returns whether the screen needs a redraw.
fn diff_mouse(diff: &mut DiffView, mouse: MouseEvent) -> bool {
    match mouse.kind {
        MouseEventKind::ScrollUp => diff.scroll_by(-(WHEEL_LINES as isize)),
        MouseEventKind::ScrollDown => diff.scroll_by(WHEEL_LINES as isize),
        MouseEventKind::Down(MouseButton::Left) => diff.click(mouse.column, mouse.row),
        _ => return false,
    }
    true
}

/// tui-input's crossterm mapping covers readline keys and `ctrl+arrow`, but it matches
/// the modifiers exactly, so it has nothing for the escape sequences macOS terminals
/// send for `option+arrow` and `cmd+arrow`, nor for a cursor key held with shift. Those
/// are mapped here first; everything else falls through to tui-input.
fn input_request(key: KeyEvent) -> Option<InputRequest> {
    let alt =
        key.modifiers.contains(KeyModifiers::ALT) || key.modifiers.contains(KeyModifiers::META);
    let cmd = key.modifiers.contains(KeyModifiers::SUPER);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    let mapped = match key.code {
        // option+arrow: `\e[1;3D` in most terminals, `\eb` when option is sent as meta.
        KeyCode::Left if alt => Some(InputRequest::GoToPrevWord),
        KeyCode::Right if alt => Some(InputRequest::GoToNextWord),
        KeyCode::Char('b') if alt => Some(InputRequest::GoToPrevWord),
        KeyCode::Char('f') if alt => Some(InputRequest::GoToNextWord),
        // cmd+arrow jumps to the ends of the line, cmd+backspace clears it.
        KeyCode::Left if cmd => Some(InputRequest::GoToStart),
        KeyCode::Right if cmd => Some(InputRequest::GoToEnd),
        KeyCode::Backspace if cmd => Some(InputRequest::DeleteLine),
        // shift+cursor key: the same move, with the caller extending the selection.
        KeyCode::Left if shift => Some(InputRequest::GoToPrevChar),
        KeyCode::Right if shift => Some(InputRequest::GoToNextChar),
        KeyCode::Home if shift => Some(InputRequest::GoToStart),
        KeyCode::End if shift => Some(InputRequest::GoToEnd),
        _ => None,
    };
    mapped.or_else(|| to_input_request(&TermEvent::Key(key)))
}

#[cfg(test)]
impl App {
    /// An app on a session with no agent behind it.
    pub fn detached() -> Self {
        let (tx_user, _) = tokio::sync::mpsc::channel(1);
        let (tx_control, _) = tokio::sync::mpsc::channel(1);
        App::new(Session::new(
            "m".to_string(),
            "general".to_string(),
            tx_user,
            tx_control,
            Arc::default(),
            Arc::default(),
            None,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moved(row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Moved,
            column: 0,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn hover_redraws_only_when_the_entry_changes() {
        let mut app = App::detached();
        app.rows = vec![(1..3, 0), (4..5, 1)];
        assert!(app.on_mouse(moved(1)));
        assert_eq!(app.hover, Some(0));
        assert!(!app.on_mouse(moved(2)), "same entry");
        assert!(app.on_mouse(moved(3)), "the blank line between entries");
        assert_eq!(app.hover, None);
        assert!(!app.on_mouse(moved(0)));
        assert!(app.on_mouse(moved(4)));
        assert_eq!(app.hover, Some(1));

        // The entry acts on the release, so a drag can select instead of clicking.
        let click = |app: &mut App| {
            app.on_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                ..moved(4)
            });
            app.on_mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..moved(4)
            })
        };
        assert!(click(&mut app));
        assert!(app.pinned.contains(&1));
        assert!(click(&mut app));
        assert!(app.pinned.is_empty());
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            ..moved(4)
        };
        assert!(app.on_mouse(wheel), "the wheel still scrolls");
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn option_arrows_jump_by_word() {
        for (code, want) in [
            (KeyCode::Left, InputRequest::GoToPrevWord),
            (KeyCode::Right, InputRequest::GoToNextWord),
        ] {
            assert_eq!(input_request(key(code, KeyModifiers::ALT)), Some(want));
            assert_eq!(input_request(key(code, KeyModifiers::META)), Some(want));
        }
        // The `option as meta` form terminals send instead of an arrow sequence.
        assert_eq!(
            input_request(key(KeyCode::Char('b'), KeyModifiers::ALT)),
            Some(InputRequest::GoToPrevWord)
        );
    }

    #[test]
    fn plain_arrows_move_one_character() {
        assert_eq!(
            input_request(key(KeyCode::Left, KeyModifiers::NONE)),
            Some(InputRequest::GoToPrevChar)
        );
    }

    #[test]
    fn shift_maps_the_cursor_keys_tui_input_skips() {
        for (code, want) in [
            (KeyCode::Left, InputRequest::GoToPrevChar),
            (KeyCode::Right, InputRequest::GoToNextChar),
            (KeyCode::Home, InputRequest::GoToStart),
            (KeyCode::End, InputRequest::GoToEnd),
        ] {
            assert_eq!(input_request(key(code, KeyModifiers::SHIFT)), Some(want));
        }
    }

    #[test]
    fn readline_keys_still_work() {
        assert_eq!(
            input_request(key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Some(InputRequest::GoToStart)
        );
        assert_eq!(
            input_request(key(KeyCode::Char('w'), KeyModifiers::CONTROL)),
            Some(InputRequest::DeletePrevWord)
        );
        assert_eq!(
            input_request(key(KeyCode::Left, KeyModifiers::CONTROL)),
            Some(InputRequest::GoToPrevWord)
        );
    }

    #[test]
    fn switch_notice_shows_the_command() {
        assert_eq!(
            switch_notice("general", ""),
            "identity: general. Usage: /as <name>"
        );
        assert!(switch_notice("general", "router").ends_with("run: bhai --as router"));
    }

    #[test]
    fn the_queue_count_follows_the_events() {
        let mut app = App::detached();
        let queued = |position| Event::Queued {
            position,
            text: format!("p{position}"),
        };
        app.follow = false;
        app.on_event(queued(1));
        app.on_event(queued(2));
        assert_eq!(app.queued, 2);
        // A queued prompt scrolls into view like a message that starts a turn.
        assert!(app.follow);
        // The front of the queue starting is a user message like any other.
        app.on_event(Event::User("p1".to_string()));
        assert_eq!(app.queued, 1);
        app.on_event(Event::Interrupted);
        assert_eq!(app.queued, 0);
        app.on_event(Event::User("fresh".to_string()));
        assert_eq!(app.queued, 0);
    }

    #[test]
    fn queue_report_lists_the_waiting_prompts() {
        assert_eq!(
            queue_report(&[]),
            "queue: nothing waiting. /queue clear drops what is."
        );
        let queued = ["fix the test".to_string(), "then\nship it".to_string()];
        assert_eq!(
            queue_report(&queued),
            "queue: 2 waiting\n  1. fix the test\n  2. then"
        );
    }

    #[test]
    fn skills_report_lists_source_and_cost() {
        assert_eq!(skills_report(&[]), "skills: none loaded");
        let skill = Skill {
            name: "pdf".to_string(),
            description: "Read PDFs.".to_string(),
            dir: "/s/pdf".into(),
            source: "~/.claude/skills".to_string(),
        };
        // "- pdf: Read PDFs." plus its newline is 18 bytes.
        assert_eq!(
            skills_report(&[skill]),
            "skills: 1 listed, about 5 tokens\n    5 tok  pdf  (~/.claude/skills)"
        );
    }

    #[test]
    fn typing_inserts_characters() {
        assert_eq!(
            input_request(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(InputRequest::InsertChar('x'))
        );
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    #[test]
    fn modified_enter_and_ctrl_j_insert_newlines() {
        let mut app = App::detached();
        type_text(&mut app, "a");
        app.on_key(key(KeyCode::Enter, KeyModifiers::SHIFT));
        type_text(&mut app, "b");
        app.on_key(key(KeyCode::Enter, KeyModifiers::ALT));
        app.on_key(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(app.input.value(), "a\nb\n\n");
        app.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        app.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input.cursor_position(80), (1, 0));
    }

    #[test]
    fn shift_arrows_select_and_ctrl_y_picks_what_to_copy() {
        let mut app = App::detached();
        app.on_key(key(KeyCode::Char('h'), KeyModifiers::NONE));
        app.on_key(key(KeyCode::Char('i'), KeyModifiers::NONE));
        assert_eq!(app.copy_text(), "hi");
        app.on_key(key(KeyCode::Left, KeyModifiers::SHIFT));
        assert_eq!(app.input.selected().as_deref(), Some("i"));
        assert_eq!(app.copy_text(), "i");
        // A move without shift drops the selection, so ctrl+y copies it all again.
        app.on_key(key(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.input.selection(), None);
        assert_eq!(app.copy_text(), "hi");
        // ctrl+a takes the lot, and the next char typed replaces it.
        app.on_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(app.copy_text(), "hi");
        app.on_key(key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.input.value(), "x");
        assert_eq!(app.input.selection(), None);
    }

    fn transcript(lines: &[&str]) -> App {
        let mut app = App::detached();
        app.lines = lines.iter().map(|l| l.to_string()).collect();
        app.transcript_area = Some(Rect::new(0, 0, 40, lines.len() as u16));
        app
    }

    fn at(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn a_double_click_takes_the_word_and_a_triple_the_line() {
        let mut app = transcript(&["hello wide world"]);
        let down = at(MouseEventKind::Down(MouseButton::Left), 8, 0);
        app.on_mouse(down);
        app.on_mouse(down);
        assert_eq!(app.selected_text().as_deref(), Some("wide"));
        app.on_mouse(down);
        assert_eq!(app.selected_text().as_deref(), Some("hello wide world"));
        // The count rounds, so a fourth press starts over and selects nothing.
        app.on_mouse(down);
        assert_eq!(app.selection, None);
        // A double click on the space between words takes the space.
        let space = at(MouseEventKind::Down(MouseButton::Left), 5, 0);
        app.on_mouse(space);
        app.on_mouse(space);
        assert_eq!(app.selected_text(), None, "a space alone trims to nothing");
    }

    #[test]
    fn a_drag_that_never_left_the_press_cell_stays_a_click() {
        let mut app = transcript(&["one two"]);
        app.on_mouse(at(MouseEventKind::Down(MouseButton::Left), 4, 0));
        assert!(!app.on_mouse(at(MouseEventKind::Drag(MouseButton::Left), 4, 0)));
        app.on_mouse(at(MouseEventKind::Up(MouseButton::Left), 4, 0));
        assert_eq!(app.selection, None);
    }

    #[test]
    fn a_drag_selects_across_lines_and_ctrl_y_copies_it() {
        let mut app = transcript(&["one two", "three   ", "four"]);
        app.on_mouse(at(MouseEventKind::Down(MouseButton::Left), 4, 0));
        assert_eq!(app.selection, None, "the press alone selects nothing");
        app.on_mouse(at(MouseEventKind::Drag(MouseButton::Left), 3, 2));
        // Both end cells are included, and each line loses its trailing spaces.
        assert_eq!(app.selected_text().as_deref(), Some("two\nthree\nfour"));
        assert_eq!(app.copy_text(), "two\nthree\nfour");
        app.on_mouse(at(MouseEventKind::Up(MouseButton::Left), 3, 2));
        assert_eq!(app.copy_text(), "two\nthree\nfour", "the release keeps it");
        // A drag back up the way it came selects the same span.
        app.on_mouse(at(MouseEventKind::Down(MouseButton::Left), 3, 2));
        app.on_mouse(at(MouseEventKind::Drag(MouseButton::Left), 4, 0));
        assert_eq!(app.selected_text().as_deref(), Some("two\nthree\nfour"));

        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.selection, None, "esc clears it");
        assert_eq!(app.copy_text(), "", "back to the empty input");
    }

    #[test]
    fn mouse_toggles_capture_and_says_which_way() {
        let mut app = transcript(&["one"]);
        app.on_mouse(at(MouseEventKind::Down(MouseButton::Left), 0, 0));
        app.on_mouse(at(MouseEventKind::Drag(MouseButton::Left), 2, 0));
        assert!(app.selection.is_some());
        type_text(&mut app, "/mouse");
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.mouse);
        assert_eq!(app.selection, None, "the selection goes with the capture");
        assert!(
            matches!(app.entries().list.last(), Some(Entry::Info(t)) if t.starts_with("mouse: off"))
        );
        type_text(&mut app, "/mouse");
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.mouse);
        assert!(
            matches!(app.entries().list.last(), Some(Entry::Info(t)) if t.starts_with("mouse: on"))
        );
    }

    #[test]
    fn paste_inserts_newlines_without_submitting() {
        let mut app = App::detached();
        app.on_paste("one\ntwo\n");
        assert_eq!(app.input.value(), "one\ntwo\n");
        assert!(!app.working);
        assert!(app.history.prev("").is_none(), "nothing was submitted");
    }

    #[test]
    fn ctrl_p_and_ctrl_n_walk_history_and_restore_the_draft() {
        let mut app = App::detached();
        type_text(&mut app, "/permissions");
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.input.is_empty());
        type_text(&mut app, "draft");
        app.on_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.input.value(), "/permissions");
        app.on_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.input.value(), "/permissions");
        app.on_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(app.input.value(), "draft");
    }
}
