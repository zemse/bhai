//! UI state and the event handling that mutates it.

use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use std::collections::HashSet;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tui_input::InputRequest;
use tui_input::backend::crossterm::to_input_request;

use crate::branch::Branch;
use crate::client::Usage;
use crate::clipboard;
use crate::commands::{self, Item};
use crate::compact::Limits;
use crate::debug;
use crate::diff::DiffView;
use crate::entries::Entries;
pub use crate::entries::Entry;
use crate::input::{Editor, History};
use crate::limits::RateLimits;
use crate::models::{Choice, Picker};
use crate::permissions::{Answer, Mode, Remember};
use crate::profile::{self, Transcript};
use crate::session::{Approval, ChildRow, Event, Prompt, Session};
use crate::skills::Skill;
use crate::speed::Speed;
use crate::workflow::{self, Found};
use crate::wrap::Join;

/// Lines a mouse wheel notch moves the transcript.
const WHEEL_LINES: usize = 3;

/// Most lines one tick of a selection drag held past the edge of the transcript moves it.
/// A row past the edge is a line, so a small overshoot creeps and a big one travels.
const EDGE_LINES: isize = 5;

/// Presses on one cell this close together count as a double or triple click.
const MULTI_CLICK: Duration = Duration::from_millis(400);

/// What bare `/allow` prints. The rule syntax is the config's, and the examples are the
/// three shapes it takes rather than a grammar.
const ALLOW_USAGE: &str = "/allow <rule> lets a kind of call run without asking, for \
this session and every later one.
  /allow Bash(brew install:*)     that command and anything after it
  /allow Bash(cargo fmt)          exactly that command
  /allow Write(~/.gnupg/**)       writing anywhere under a directory
  /allow mcp__chrome-devtools     every tool of one MCP server
A deny rule still refuses, and a protected path still asks. /permissions lists what is \
set.";

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

/// What a drag's copy left behind: when it happened, how much it took and the cell the
/// drag ended on, which the note is drawn beside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Copied {
    pub at: Instant,
    pub chars: usize,
    pub cell: Position,
}

/// The question asked on opening a project the trust store does not know. Until it is
/// answered the session is in `ask` mode, since `auto` and `bypass` run code this
/// project supplies.
#[derive(Debug, Clone, PartialEq)]
pub struct TrustGate {
    /// The project root, as the store keys it.
    pub root: String,
    /// Allow rules this project's own settings files ship.
    pub rules: usize,
    /// The mode answering yes puts the session in.
    pub mode: Mode,
}

/// The child agent whose pane is open: the transcript shows its work and the prompt
/// types into it rather than into the session.
pub struct Inside {
    pub id: String,
    pub identity: String,
    pub description: String,
    entries: Arc<Mutex<Entries>>,
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
    /// Entries with rows folded away right now, filled in by the renderer. A click on
    /// one of these opens or closes it; a click anywhere else pins the badge.
    pub folds: HashSet<usize>,
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
    /// Where a transcript selection drag was last seen, while one is under way. The
    /// terminal reports a drag only when the pointer moves, so a pointer held past the
    /// edge has to be carried on by the tick, and a wheel turned mid-drag has to take
    /// the selection with it.
    drag_at: Option<(u16, u16)>,
    /// Plain text of each wrapped transcript line, filled in by the renderer.
    pub lines: Vec<String>,
    /// How each of those lines joins the one above it, so a copy can undo the wrapping.
    pub joins: Vec<Join>,
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
    /// The last drag's copy, for the note drawn where the drag ended.
    pub copied: Option<Copied>,
    pub input: Editor,
    /// The `/` menu's highlighted row, while the menu is open.
    pub menu: Option<usize>,
    /// Submitted prompts, for `ctrl+p` and `ctrl+n`.
    pub history: History,
    pub working: bool,
    /// Prompts waiting behind the running turn, as they were typed. They are not in
    /// the transcript: a message joins the conversation when its turn starts, which is
    /// where the model sees it too.
    pub queued: Vec<String>,
    /// The call the judge is deciding, while it decides one.
    pub judging: Option<String>,
    /// The tool call waiting for approval.
    pub pending: Option<Approval>,
    /// The trust question, until it is answered.
    pub trust_gate: Option<TrustGate>,
    pub scroll: usize,
    pub max_scroll: usize,
    /// Transcript viewport height, filled in by the renderer so page keys match the view.
    pub page: usize,
    pub follow: bool,
    pub spinner: usize,
    pub model: String,
    pub effort: String,
    pub identity: String,
    pub mode: Mode,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// How fast the model is answering, for the working row.
    pub speed: Speed,
    pub last_usage: Option<Usage>,
    /// The field that broke the prompt cache, until the next clean call.
    pub cache_break: Option<String>,
    /// The hit percent of the last judged call, when it missed.
    pub cache_miss: Option<f64>,
    /// Judged calls have been missing in a row, until one hits again.
    pub cache_stalled: bool,
    pub rate_limits: Option<RateLimits>,
    /// The branch of the directory the session runs in, for the status bar.
    pub branch: Branch,
    /// When history is compacted, which is what the bar's context fill is read against.
    pub limits: Limits,
    /// The skills in the system prompt, for `/skills`.
    pub skills: Vec<Skill>,
    /// The session's MCP servers, for `/mcp`.
    pub mcp: Option<Arc<crate::mcp::Hub>>,
    /// The workflow definitions, for `/workflows` and `/workflow`.
    pub workflows: Found,
    /// The `/diff` pane, shown instead of the transcript while open.
    pub diff: Option<DiffView>,
    /// The child agent whose pane is open, while one is.
    pub inside: Option<Inside>,
    /// Each subagent row's area and id, filled in by the renderer so a click opens it.
    pub child_rows: Vec<(Rect, String)>,
    /// The `/model` picker, shown instead of the prompt while open.
    pub picker: Option<Picker>,
    /// Where the Ollama server is, for asking it what it has pulled.
    pub ollama_url: String,
    /// The session's id, which names its file on disk; empty until main fills it in.
    pub session_id: String,
    pub quit: bool,
    session: Arc<Session>,
}

impl App {
    pub fn new(session: Arc<Session>) -> Self {
        session.entries().push(Entry::Info(
            "bhai · bash, read, write and edit. The mode on the prompt's border says what runs without asking; shift+tab changes it, / lists the commands. Type a task and hit enter."
                .to_string(),
        ));
        Self {
            rows: Vec::new(),
            hover: None,
            mouse_row: None,
            pinned: HashSet::new(),
            expanded: HashSet::new(),
            folds: HashSet::new(),
            all_badges: false,
            buttons: Vec::new(),
            input_area: None,
            scrollbar: None,
            dragging: false,
            selecting: false,
            drag_at: None,
            lines: Vec::new(),
            joins: Vec::new(),
            transcript_area: None,
            selection: None,
            anchor: None,
            press: None,
            clicks: None,
            mouse: true,
            copied: None,
            input: Editor::default(),
            menu: None,
            history: History::default(),
            working: false,
            queued: Vec::new(),
            judging: None,
            pending: None,
            trust_gate: None,
            scroll: 0,
            max_scroll: 0,
            page: 10,
            follow: true,
            spinner: 0,
            model: session.state().model,
            effort: session.state().effort,
            identity: session.state().identity,
            mode: session.state().mode,
            tokens_in: 0,
            tokens_out: 0,
            speed: Speed::default(),
            last_usage: None,
            cache_break: None,
            cache_miss: None,
            cache_stalled: false,
            rate_limits: None,
            branch: Branch::at(std::env::current_dir().unwrap_or_default()),
            limits: Limits::default(),
            skills: Vec::new(),
            mcp: None,
            workflows: Found::default(),
            diff: None,
            inside: None,
            child_rows: Vec::new(),
            picker: None,
            ollama_url: crate::ollama::DEFAULT_URL.to_string(),
            session_id: String::new(),
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

        // The trust question is modal and comes first: the mode it decides governs every
        // approval after it.
        if self.trust_gate.is_some() {
            match key.code {
                KeyCode::Char('y') => self.answer_trust(true),
                KeyCode::Char('n') | KeyCode::Esc => self.answer_trust(false),
                KeyCode::Char('c') | KeyCode::Char('d') if ctrl => self.quit = true,
                _ => {}
            }
            return;
        }

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

        // The `/model` picker takes the prompt's keys until it is answered or closed.
        if let Some(picker) = &mut self.picker
            && !(ctrl && key.code == KeyCode::Char('c'))
        {
            match picker.on_key(key.code) {
                Choice::Waiting => {}
                Choice::Closed => self.picker = None,
                Choice::Picked {
                    model,
                    effort,
                    window,
                } => {
                    self.picker = None;
                    self.switch_model(model, effort, window);
                }
            }
            return;
        }

        // The `/` menu takes the keys that move and accept a row; everything else goes
        // on editing the prompt, which reopens the menu on the next keystroke.
        if let Some(selected) = self.menu {
            let rows = self.menu_items().len();
            match key.code {
                KeyCode::Up => {
                    self.menu = Some((selected + rows - 1) % rows);
                    return;
                }
                KeyCode::Down => {
                    self.menu = Some((selected + 1) % rows);
                    return;
                }
                KeyCode::Tab => return self.accept_menu(false),
                // Enter belongs to the menu only where the whole prompt is the command
                // it is offering. Mid-sentence it sends what was typed, as it would with
                // no menu open, and tab is what completes the name.
                KeyCode::Enter if key.modifiers.is_empty() && self.menu_alone() => {
                    return self.accept_menu(true);
                }
                KeyCode::Esc => {
                    self.menu = None;
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Char('c') if ctrl => {
                if self.busy() {
                    self.interrupt();
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Char('d') if ctrl && self.input.is_empty() => self.quit = true,
            // The panel's rows, in order, and then back out to the transcript.
            KeyCode::Char('o') if ctrl => self.cycle_child(),
            KeyCode::Esc if self.inside.is_some() => self.leave_child(),
            KeyCode::Esc if self.busy() => self.interrupt(),
            KeyCode::Esc if self.selection.is_some() => self.selection = None,
            KeyCode::Char('t') if ctrl => self.all_badges = !self.all_badges,
            KeyCode::Char('y') if ctrl => self.copy(),
            KeyCode::Char('v') if ctrl => self.paste(),
            KeyCode::Char('a') if ctrl && !self.input.is_empty() => self.input.select_all(),
            // With nothing drafted there is nothing in the prompt to take, so ctrl+a
            // reaches past it to the transcript, which is the thing worth taking whole.
            KeyCode::Char('a') if ctrl => self.select_transcript(),
            KeyCode::BackTab => {
                let mode = self.session.cycle_mode();
                // The only mode on offer in an untrusted project is `ask`, so say why.
                if mode == self.mode && mode == Mode::Ask {
                    self.follow = true;
                    self.note(Entry::Info(
                        "ask is the only mode here until you trust this project: /trust"
                            .to_string(),
                    ));
                }
                self.mode = mode;
            }
            // Most terminals cannot report shift+enter, so alt+enter and ctrl+j also work.
            KeyCode::Enter if !key.modifiers.is_empty() => self.input.newline(),
            KeyCode::Char('j') if ctrl => self.input.newline(),
            KeyCode::Enter => self.submit(),
            KeyCode::Char('p') if ctrl => return self.recall(-1),
            KeyCode::Char('n') if ctrl => return self.recall(1),
            KeyCode::PageUp => self.scroll_by(-(self.page as isize)),
            KeyCode::PageDown => self.scroll_by(self.page as isize),
            // The transcript scrolls a line at a time with ctrl held, since the arrows
            // themselves belong to the prompt.
            KeyCode::Up if ctrl => self.scroll_by(-1),
            KeyCode::Down if ctrl => self.scroll_by(1),
            // Inside a prompt of more than one row the arrows move between its rows; off
            // the top or bottom edge of it they walk the prompt history, as a shell does.
            KeyCode::Up | KeyCode::Down => {
                let delta = if key.code == KeyCode::Up { -1 } else { 1 };
                self.input.selecting(shift);
                if !self.input.move_line(delta, self.input_width()) && !shift {
                    return self.recall(delta);
                }
            }
            _ => {
                if let Some(request) = input_request(key) {
                    self.input.handle(request, shift);
                }
            }
        }
        self.refresh_menu();
    }

    /// Bracketed paste: the text goes into the input as typed, newlines and all.
    pub fn on_paste(&mut self, text: &str) {
        if self.pending.is_none() && self.diff.is_none() {
            self.input.insert(text);
            self.refresh_menu();
        }
    }

    /// Walk the prompt history: back one entry, or forward one and then to the draft the
    /// walk started from. The cursor lands at the end, ready to edit or send.
    fn recall(&mut self, delta: isize) {
        let found = match delta < 0 {
            true => self.history.prev(self.input.value()),
            false => self.history.next(),
        };
        if let Some(text) = found {
            self.input.set(text);
        }
        // A recalled `/command` must not take the arrows back for the menu, or the walk
        // would stop there. The next keystroke opens it again.
        self.menu = None;
    }

    /// The menu rows for what is typed; empty whenever the menu is shut.
    pub fn menu_items(&self) -> Vec<Item> {
        match self.menu {
            Some(_) => commands::matches(self.input.before(), &self.skills),
            None => Vec::new(),
        }
    }

    /// The tail of the highlighted row, drawn grey from the cursor on and filled in by
    /// `tab`. Only with the cursor at the end of what is typed, so grey text never sits
    /// in the middle of the prompt.
    pub fn suggestion(&self) -> String {
        if self.input.cursor() != self.input.value().chars().count() {
            return String::new();
        }
        let items = self.menu_items();
        self.menu
            .and_then(|row| items.get(row))
            .map(|item| item.completion(self.input.before()))
            .unwrap_or_default()
    }

    /// Open the menu on anything that matches, keeping the highlighted row in range.
    /// An edit that matches nothing shuts it, and the next one can open it again.
    fn refresh_menu(&mut self) {
        let rows = commands::matches(self.input.before(), &self.skills).len();
        self.menu = (rows > 0).then(|| self.menu.unwrap_or(0).min(rows - 1));
    }

    /// Whether the `/word` the menu is offering for is the whole prompt, which is what
    /// makes it a command about to run rather than a name being written into a sentence.
    fn menu_alone(&self) -> bool {
        let value = self.input.value();
        commands::typing(self.input.before())
            .is_some_and(|typed| typed.chars().count() + 1 == value.chars().count())
    }

    /// Enter or tab on a menu row: the name replaces the `/word` being typed, wherever
    /// in the prompt that is. A command typed on its own, and only then, runs straight
    /// away; one in the middle of a sentence is a name being completed, not a command.
    fn accept_menu(&mut self, run: bool) {
        let items = self.menu_items();
        let Some(item) = self.menu.and_then(|row| items.get(row)) else {
            return;
        };
        let Some(typed) = commands::typing(self.input.before()) else {
            return;
        };
        let more = item.takes_input();
        let alone = self.menu_alone();
        let end = self.input.cursor();
        let start = end - typed.chars().count() - 1;
        self.input
            .splice(start..end, &(item.label() + if more { " " } else { "" }));
        self.menu = None;
        if run && !more && alone {
            self.submit();
        }
    }

    /// Returns whether the screen needs a redraw.
    pub fn on_mouse(&mut self, mouse: MouseEvent) -> bool {
        // With an approval up, clicks still reach its buttons.
        if let Some(diff) = self.diff.as_mut().filter(|_| self.pending.is_none()) {
            return diff_mouse(diff, mouse);
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.wheel(-(WHEEL_LINES as isize)),
            MouseEventKind::ScrollDown => self.wheel(WHEEL_LINES as isize),
            MouseEventKind::Moved => {
                self.mouse_row = Some(mouse.row);
                return self.rehover();
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
                let selecting = self.selecting || self.anchor.is_some();
                self.dragging = false;
                self.selecting = false;
                self.anchor = None;
                self.drag_at = None;
                // A press and release on one cell is a click, not a drag.
                let clicked = self.press.take() == Some((mouse.column, mouse.row));
                if selecting && !clicked {
                    let at = Position::new(mouse.column, mouse.row);
                    return self.copy_selection(at) | self.rehover();
                }
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
        // A click on a subagent row goes inside it, and on the open one comes back out.
        if let Some(id) = self
            .child_rows
            .iter()
            .find(|(area, _)| area.contains(at))
            .map(|(_, id)| id.clone())
        {
            self.open_child(&id);
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

    /// The release of a click that never moved: the failure the last turn ended on runs
    /// that turn again, an entry with rows folded away expands or collapses and any
    /// other pins its badge. Returns whether the screen needs a redraw.
    fn click_entry(&mut self, y: u16) -> bool {
        let Some(entry) = self.entry_at(y) else {
            return false;
        };
        if self.retryable() == Some(entry) {
            self.retry();
            return true;
        }
        let set = if self.folds.contains(&entry) {
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

    /// The transcript cell a drag is over, with the pointer allowed to be outside the
    /// transcript: a drag is how a selection runs past what is on screen, so above the
    /// transcript is the top line in view and below it the bottom one. Past the bottom
    /// takes the whole line and past the top takes none of it, so the lines a drag has
    /// travelled over are the ones it holds.
    fn drag_cell(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let area = self.transcript_area?;
        let last = self.lines.len().checked_sub(1)?;
        let row = y.clamp(area.y, area.bottom().saturating_sub(1));
        let line = (self.scroll + (row - area.y) as usize).min(last);
        let column = match y {
            _ if y < area.y => 0,
            _ if y >= area.bottom() => self.lines[line].chars().count(),
            _ => x.clamp(area.x, area.right()).saturating_sub(area.x) as usize,
        };
        Some((line, column))
    }

    /// Lines to scroll for a selection drag at this row, away from the edge it is on.
    /// Reaching the first or last row in view is enough to start it, since that is where
    /// a drag runs out of transcript; a row is a line, so the further past the edge the
    /// pointer goes the faster the view moves under it.
    fn edge_scroll(&self, y: u16) -> isize {
        let Some(area) = self.transcript_area else {
            return 0;
        };
        let last = area.bottom().saturating_sub(1);
        match y {
            _ if y <= area.y => -((1 + (area.y - y) as isize).min(EDGE_LINES)),
            _ if y >= last => (1 + (y - last) as isize).min(EDGE_LINES),
            _ => 0,
        }
    }

    /// `ctrl+a` with nothing drafted: the whole transcript, from its first cell to the
    /// last cell of its last line. Only the part in view is highlighted; `ctrl+y` takes
    /// the lot.
    fn select_transcript(&mut self) {
        let Some(last) = self.lines.len().checked_sub(1) else {
            return;
        };
        self.selection = Some(Selection {
            anchor: (0, 0),
            head: (last, self.lines[last].chars().count()),
        });
    }

    /// Scroll the transcript, taking a selection drag under way with it: the pointer has
    /// not moved, so the cell under it is whatever the wheel has just brought there.
    fn wheel(&mut self, delta: isize) {
        self.scroll_by(delta);
        if let Some((x, y)) = self.drag_at {
            self.extend_selection(x, y, 0);
        }
    }

    /// Extend the transcript selection to the cell under the pointer, scrolling the view
    /// when the pointer has left it. Returns whether the screen needs a redraw.
    fn drag_selection(&mut self, x: u16, y: u16) -> bool {
        // A drag reported on the press cell has not moved yet, so it is still a click.
        if self.press == Some((x, y)) {
            return false;
        }
        self.press = None;
        self.drag_at = Some((x, y));
        let step = self.edge_scroll(y);
        self.extend_selection(x, y, step)
    }

    /// Scroll by `step` and take the selection to where that leaves the pointer.
    /// Returns whether the screen needs a redraw.
    fn extend_selection(&mut self, x: u16, y: u16, step: isize) -> bool {
        let before = self.scroll;
        if step != 0 {
            self.scroll_by(step);
        }
        let scrolled = self.scroll != before;
        let (Some(anchor), Some(head)) = (self.anchor, self.drag_cell(x, y)) else {
            return scrolled;
        };
        let selection = Some(Selection { anchor, head });
        std::mem::replace(&mut self.selection, selection) != selection || scrolled
    }

    /// The selected transcript text, trailing spaces trimmed. Rows the wrap broke go
    /// back on one line: only the newlines the text itself has survive the copy.
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        let (from, to) = selection.range();
        let rows: Vec<(usize, String)> = (from.0..=to.0.min(self.lines.len().checked_sub(1)?))
            .map(|line| {
                let chars = self.lines[line].chars();
                let range = selection.on_line(line, self.lines[line].chars().count());
                let range = range.unwrap_or(0..0);
                let part: String = chars.skip(range.start).take(range.len()).collect();
                (line, part.trim_end().to_string())
            })
            .collect();
        if rows.iter().all(|(_, part)| part.is_empty()) {
            return None;
        }
        let mut text = String::new();
        for (index, (line, part)) in rows.iter().enumerate() {
            match index {
                0 => text.push_str(part),
                _ => self.join_at(*line).append(&mut text, part),
            }
        }
        Some(text)
    }

    /// How transcript line `line` joins the one above it. A renderer that has not run
    /// says nothing, so the line stands on its own.
    fn join_at(&self, line: usize) -> Join {
        self.joins.get(line).copied().unwrap_or_default()
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

    /// What the end of a drag copies on its own: the selection it just made, and nothing
    /// when it made none, since a click must not put the whole input on the clipboard.
    /// It says so beside where the drag ended rather than in the transcript, which a
    /// drag has no business writing to. Returns whether the screen needs a redraw.
    fn copy_selection(&mut self, at: Position) -> bool {
        let Some(text) = self.selected_text().or_else(|| self.input.selected()) else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        let chars = text.chars().count();
        match clipboard::copy(&text) {
            Ok(()) => {
                self.copied = Some(Copied {
                    at: Instant::now(),
                    chars,
                    cell: at,
                })
            }
            Err(err) => self.note(Entry::Error(format!("copy failed: {err}"))),
        }
        true
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
    #[cfg(test)]
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
                // behind it is the front of that queue starting, and it has just
                // joined the transcript as the entry the model will see.
                if !self.queued.is_empty() {
                    self.queued.remove(0);
                }
            }
            // The position is the length of the queue the prompt joined. Scrolling to it
            // is what a submitted prompt does, queued or not.
            Event::Queued { position, text } => {
                self.follow = true;
                // The position is the length of the queue the prompt joined, so a
                // prompt from another consumer this never saw still leaves no gap.
                self.queued.truncate(position.saturating_sub(1));
                self.queued.push(text);
            }
            // The queue survives an interrupt, so the panel keeps showing it; the
            // front of it starts as soon as the cancelled turn ends.
            Event::Interrupted => self.judging = None,
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
            Event::Sending(tokens) => self.speed.sending(Instant::now(), tokens),
            Event::Streaming(on) => match on {
                true => self.speed.start(Instant::now()),
                false => self.speed.end(Instant::now()),
            },
            Event::Reasoning(ref text) | Event::Text(ref text) => {
                let tokens = crate::tokens::for_model(&self.model).count(text);
                self.speed.streamed(Instant::now(), tokens as u64);
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
            Event::Model { model, effort } => {
                self.model = model;
                self.effort = effort;
            }
            // A child's report opened a turn nobody typed. Deliberately not `follow`:
            // a landing report must not yank a scrolled-up reader to the bottom.
            Event::Resumed(_) => self.working = true,
            Event::Judging(what) => self.judging = what.clone(),
            Event::TurnEnd => {
                self.working = false;
                self.judging = None;
            }
            _ => {}
        }
    }

    pub fn tick(&mut self) {
        self.branch.refresh();
        if self.busy() {
            self.spinner = self.spinner.wrapping_add(1);
        }
        // A selection drag still held past the edge of the transcript, which the
        // terminal has nothing more to say about until the pointer moves again.
        if let Some((x, y)) = self.drag_at.filter(|&(_, y)| self.edge_scroll(y) != 0) {
            self.extend_selection(x, y, self.edge_scroll(y));
        }
        // The backends answer on a task of their own; this is where the picker hears.
        if let Some(picker) = &mut self.picker {
            picker.poll();
        }
    }

    fn submit(&mut self) {
        if self.input.value().trim().is_empty() {
            return;
        }
        let message = self.input.take().trim().to_string();
        self.selection = None;
        self.menu = None;
        if let Err(e) = self.history.push(&message) {
            self.note(Entry::Error(format!("could not save prompt history: {e}")));
        }
        // Inside a child's pane the prompt types into that child, not the session, so
        // none of the commands below apply.
        if let Some(id) = self.inside.as_ref().map(|inside| inside.id.clone()) {
            self.follow = true;
            if self.session.steer(&id, message).is_err() {
                self.entries().push(Entry::Error(
                    "that subagent has finished, so there is nothing to tell it".to_string(),
                ));
            }
            return;
        }
        if message == "/help" {
            self.follow = true;
            self.note(Entry::Info(commands::help()));
            return;
        }
        if message == "/quit" || message == "/exit" {
            self.quit = true;
            return;
        }
        if message == "/tokens" {
            self.all_badges = !self.all_badges;
            return;
        }
        if message == "/copy" {
            self.follow = true;
            self.copy();
            return;
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
        if message == "/clear" {
            self.clear();
            return;
        }
        if let Some(rest) = message.strip_prefix("/compact") {
            let asked = rest.trim();
            self.follow = true;
            match self
                .session
                .compact(Some(asked.to_string()).filter(|a| !a.is_empty()))
            {
                Ok(()) => self.working = true,
                Err(e) => self.note(Entry::Error(e.to_string())),
            }
            return;
        }
        if message.starts_with("/export-debug") {
            self.export_debug();
            return;
        }
        if message.starts_with("/permissions") {
            self.follow = true;
            self.note(Entry::Info(self.session.permissions()));
            return;
        }
        if let Some(rest) = message.strip_prefix("/allow")
            && (rest.is_empty() || rest.starts_with(' '))
        {
            self.follow = true;
            let rule = rest.trim();
            self.note(match rule.is_empty() {
                true => Entry::Info(ALLOW_USAGE.to_string()),
                false => match self.session.allow(rule) {
                    Ok(text) => Entry::Info(text),
                    Err(e) => Entry::Error(format!("{e:#}")),
                },
            });
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
        if let Some(rest) = message.strip_prefix("/model")
            && (rest.is_empty() || rest.starts_with(' '))
        {
            self.follow = true;
            self.model_command(rest.trim());
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
        // `/<skill>` is the one slash form that reaches the model: it asks for the skill
        // by name, and the agent loads it through the `skill` tool. The transcript keeps
        // showing what was typed, not the sentence it turns into.
        let prompt = match command(&message) {
            Some((name, input)) if commands::skill(name, &self.skills).is_some() => {
                Prompt::shown_as(commands::skill_prompt(name, input), message)
            }
            Some((name, _)) => {
                self.follow = true;
                self.note(Entry::Error(format!(
                    "no command or skill called /{name}. Type / to see what there is."
                )));
                return;
            }
            None => Prompt::from(message),
        };
        // The transcript entry arrives back as `Event::User` once the session accepts it.
        if let Err(e) = self.session.submit(prompt) {
            self.note(Entry::Error(e.to_string()));
        }
    }

    /// `/model` on its own opens the picker, which asks each backend what it will serve
    /// and then, for a model that takes one, which effort to run it at. `/model <name>
    /// [effort]` skips both questions, for a model already known by name.
    fn model_command(&mut self, rest: &str) {
        if rest.is_empty() {
            let picker = Picker::new(&self.model, &self.effort);
            picker.ask(&self.ollama_url);
            self.picker = Some(picker);
            return;
        }
        let (name, effort) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let effort = match effort.trim() {
            "" => self.effort.clone(),
            given => given.to_string(),
        };
        // The window is the picker's to pass on, since only the backend's list says what
        // it is; named by hand, the model keeps whatever the config asked for.
        self.switch_model(name.to_string(), effort, None);
    }

    /// Put the session on `model`, and say so.
    fn switch_model(&mut self, model: String, effort: String, window: Option<u64>) {
        let notice = model_notice(&model, &effort);
        match self.session.set_model(model, effort, window) {
            Ok(()) => self.note(Entry::Info(notice)),
            Err(e) => self.note(Entry::Error(format!("cannot switch models: {e}"))),
        }
    }

    /// `/clear`: the conversation goes, and with it everything the transcript's indexes
    /// stood for, so the view starts again at the bottom of an empty screen.
    fn clear(&mut self) {
        if let Err(e) = self.session.clear() {
            self.note(Entry::Error(e.to_string()));
            return;
        }
        self.expanded.clear();
        self.pinned.clear();
        self.folds.clear();
        self.selection = None;
        self.hover = None;
        self.scroll = 0;
        self.follow = true;
    }

    /// `/queue` lists the prompts waiting behind the turn; `/queue clear` drops them.
    fn queue(&mut self, rest: &str) {
        if rest == "clear" {
            if self.session.clear_queue() == 0 {
                self.note(Entry::Info("queue: nothing waiting".to_string()));
            }
            self.queued.clear();
            return;
        }
        let queued = self.session.queued();
        self.queued = queued.clone();
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

    /// Everything about this session in one file, for `/export-debug`. It goes in the
    /// working directory rather than under `.bhai/debug`, since the point of it is to be
    /// handed to someone: a path the user can see beats one they have to be told about.
    /// The profile is the one part that has to be asked for, so the rest is gathered
    /// here and the write waits on it.
    fn export_debug(&mut self) {
        self.follow = true;
        let transcript = self.transcript();
        let session = Arc::clone(&self.session);
        let mut bundle = self.debug_bundle();
        tokio::spawn(async move {
            bundle.profile = session.context().await.map(|mut profile| {
                profile.transcript = Some(transcript);
                profile
            });
            let dir = std::env::current_dir().unwrap_or_default();
            session.publish(match debug::export(&bundle, &dir) {
                Ok(path) => Event::Info(bundle.summary(&path)),
                Err(e) => Event::Error(format!("debug export failed: {e:#}")),
            });
        });
    }

    /// Everything the export carries except the profile, which has to be asked for.
    fn debug_bundle(&self) -> debug::Bundle {
        let cwd = std::env::current_dir().unwrap_or_default();
        debug::Bundle {
            version: env!("CARGO_PKG_VERSION"),
            session_id: self.session_id.clone(),
            session_file: (!self.session_id.is_empty())
                .then(|| crate::sessions::path(&cwd.join(crate::sessions::DIR), &self.session_id)),
            home: std::env::var_os("HOME").map(PathBuf::from),
            branch: self.branch.name().map(str::to_string),
            terminal: ratatui::crossterm::terminal::size().ok(),
            backend: match self.model.starts_with("ollama:") {
                true => format!("ollama at {}", self.ollama_url),
                false => "codex".to_string(),
            },
            state: self.session.state(),
            wanted_mode: self.session.wanted_mode(),
            trusted: self.session.trusted(),
            limits: self.limits,
            permissions: self.session.permissions(),
            skills: skills_report(&self.skills),
            mcp: crate::mcp::report(self.mcp.as_deref()),
            workflows: workflow::report(&self.workflows),
            judged: debug::judge_log(&profile::debug_dir().join("judge.jsonl")),
            children: self.session.child_logs(),
            sidechains: (!self.session_id.is_empty())
                .then(|| cwd.join(crate::sessions::DIR).join(&self.session_id)),
            // The session's own transcript, not the child pane that may be open over it.
            entries: self.session.entries().list.clone(),
            profile: None,
            cwd,
        }
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

    /// The transcript on screen: the open child's while inside one, the session's
    /// otherwise.
    pub fn entries(&self) -> MutexGuard<'_, Entries> {
        match &self.inside {
            Some(inside) => inside.entries.lock().unwrap_or_else(|e| e.into_inner()),
            None => self.session.entries(),
        }
    }

    /// The child agents of the running turn, for the panel.
    pub fn children(&self) -> Vec<ChildRow> {
        self.session.children()
    }

    #[cfg(test)]
    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }

    /// Open child `id`'s pane, or close the one already open on it. A child whose pane
    /// has gone (a new turn cleared it) leaves the transcript where it was.
    pub fn open_child(&mut self, id: &str) {
        if self.inside.as_ref().is_some_and(|open| open.id == id) {
            return self.leave_child();
        }
        let rows = self.children();
        let Some(row) = rows.iter().find(|row| row.id == id) else {
            return;
        };
        let Some(entries) = self.session.child_entries(id) else {
            return;
        };
        self.inside = Some(Inside {
            id: row.id.clone(),
            identity: row.identity.clone(),
            description: row.description.clone(),
            entries,
        });
        self.enter_pane();
    }

    /// Step to the next child's pane, and out again past the last one.
    fn cycle_child(&mut self) {
        let rows = self.children();
        if rows.is_empty() {
            return;
        }
        let next = match &self.inside {
            None => Some(0),
            Some(open) => rows
                .iter()
                .position(|row| row.id == open.id)
                .map_or(Some(0), |at| Some(at + 1).filter(|at| *at < rows.len())),
        };
        match next.and_then(|at| rows.get(at)) {
            Some(row) => {
                let id = row.id.clone();
                self.inside = None;
                self.open_child(&id);
            }
            None => self.leave_child(),
        }
    }

    /// Go back to the session's transcript.
    pub fn leave_child(&mut self) {
        self.inside = None;
        self.enter_pane();
    }

    /// A pane swap shows the foot of whatever is now on screen, with nothing selected
    /// from the pane that was.
    fn enter_pane(&mut self) {
        self.selection = None;
        self.expanded.clear();
        self.pinned.clear();
        self.follow = true;
        self.scroll = 0;
    }

    /// Show a notice that only this TUI produced.
    fn note(&self, entry: Entry) {
        self.session.entries().push(entry);
    }

    /// The entry a click would run the turn again on: the failure the last turn ended
    /// on, and only while nothing is running and nothing has been said since.
    fn retryable(&self) -> Option<usize> {
        if self.working || self.inside.is_some() {
            return None;
        }
        let entries = self.entries();
        let last = entries.list.len().checked_sub(1)?;
        matches!(entries.list.get(last), Some(Entry::Failed(_))).then_some(last)
    }

    /// Run the failed turn again, on the history the agent still holds.
    fn retry(&mut self) {
        self.follow = true;
        if let Err(e) = self.session.retry() {
            self.note(Entry::Error(e.to_string()));
        }
    }

    /// Answer the trust question. Yes records the project and lets it into the mode the
    /// config asked for; no leaves it in `ask`, and `/trust` can still change that later.
    fn answer_trust(&mut self, trust: bool) {
        let Some(gate) = self.trust_gate.take() else {
            return;
        };
        if !trust {
            self.note(Entry::Info(format!(
                "not trusted: {} stays in ask mode. /trust when you have looked at it.",
                gate.root
            )));
            return;
        }
        match self.session.trust() {
            // `Event::Mode` follows and sets the mode; this only says what happened.
            Ok(text) => self.note(Entry::Info(text)),
            Err(e) => self.note(Entry::Error(format!("{e:#}"))),
        }
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

    /// Whether there is anything to stop: a turn, or a child still running after the
    /// turn that started it has ended.
    fn busy(&self) -> bool {
        self.working || self.session.child_running()
    }

    fn scroll_by(&mut self, delta: isize) {
        let target = self.scroll as isize + delta;
        self.scroll = target.clamp(0, self.max_scroll as isize) as usize;
        self.follow = self.scroll >= self.max_scroll;
    }
}
/// A message the prompt box should read as `/<name> [input]` rather than send. A first
/// word holding anything but a name's characters is left alone, so a prompt that opens
/// with a path like `/usr/bin/env is missing` still reaches the model.
fn command(message: &str) -> Option<(&str, &str)> {
    let rest = message.strip_prefix('/')?;
    let (name, input) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || "-_".contains(c));
    plain.then(|| (name, input.trim()))
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

/// What a switch prints. A model that takes no effort is not said to run at one, since
/// nothing on that backend is sent it.
fn model_notice(model: &str, effort: &str) -> String {
    let provider = crate::client::Provider::of(model);
    match provider {
        crate::client::Provider::Codex => {
            format!(
                "model: {model} ({effort} effort) on codex. The switch starts the prompt cache again, and the thinking of the model before it is dropped: it cannot be replayed to another one."
            )
        }
        crate::client::Provider::Ollama => {
            format!(
                "model: {model} on ollama, served locally. It takes no reasoning effort, so none is sent."
            )
        }
    }
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
            "medium".to_string(),
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

    /// An app whose session still has an agent listening, so a switch reaches it. The
    /// receivers come back with it: dropped, the channels close and nothing is accepted.
    type Listening = (
        App,
        tokio::sync::mpsc::Receiver<String>,
        tokio::sync::mpsc::Receiver<crate::agent::Control>,
    );

    fn connected() -> Listening {
        let (tx_user, user) = tokio::sync::mpsc::channel(4);
        let (tx_control, control) = tokio::sync::mpsc::channel(4);
        let app = App::new(Session::new(
            "gpt-5.5".to_string(),
            "medium".to_string(),
            "general".to_string(),
            tx_user,
            tx_control,
            Arc::default(),
            Arc::default(),
            None,
        ));
        (app, user, control)
    }

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

    #[test]
    fn clicking_the_failure_the_turn_ended_on_runs_it_again() {
        let (mut app, _user, mut control) = connected();
        app.session()
            .publish(Event::TurnFailed("request failed".to_string()));
        app.on_event(Event::TurnEnd);
        let last = app.entries().list.len() - 1;
        assert!(matches!(
            app.entries().list.last(),
            Some(Entry::Failed(t)) if t == "request failed"
        ));
        app.rows = vec![(0..1, 0), (2..3, last)];

        let click = |app: &mut App, row: u16| {
            app.on_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                ..moved(row)
            }) | app.on_mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..moved(row)
            })
        };
        assert!(click(&mut app, 2));
        assert!(
            matches!(control.try_recv(), Ok(crate::agent::Control::Retry)),
            "the click did not ask for a retry"
        );
        // Not a pin: the click ran the turn rather than marking the entry.
        assert!(app.pinned.is_empty());

        // While that turn runs there is nothing to run again, so the click pins as usual.
        assert!(click(&mut app, 2));
        assert!(app.pinned.contains(&last));
        assert!(control.try_recv().is_err(), "a second retry went out");
    }

    #[test]
    fn only_the_last_entry_is_the_one_a_click_runs_again() {
        let mut app = App::detached();
        app.session()
            .publish(Event::TurnFailed("request failed".to_string()));
        app.on_event(Event::TurnEnd);
        let failed = app.entries().list.len() - 1;
        assert_eq!(app.retryable(), Some(failed));
        // Anything said since has moved the history on.
        app.entries().push(Entry::User("never mind".to_string()));
        assert_eq!(app.retryable(), None);
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn ctrl_o_walks_the_subagents_and_the_prompt_types_into_the_open_one() {
        let mut app = App::detached();
        let started = |id: &str| Event::ChildStarted {
            id: id.to_string(),
            identity: "worker".to_string(),
            description: "read the docs".to_string(),
            task: "go".to_string(),
        };
        app.session().publish(started("a1"));
        app.session().publish(started("b2"));
        let (_mailbox, mut posted) = crate::agent::Mailbox::open(&app.session().mailboxes(), "b2");

        let ctrl_o = || key(KeyCode::Char('o'), KeyModifiers::CONTROL);
        app.on_key(ctrl_o());
        assert_eq!(app.inside.as_ref().map(|i| i.id.as_str()), Some("a1"));
        app.on_key(ctrl_o());
        assert_eq!(app.inside.as_ref().map(|i| i.id.as_str()), Some("b2"));

        // A prompt typed inside goes to that child, not to the session.
        app.input.set("look at the tests too".to_string());
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(posted.try_recv().unwrap(), "look at the tests too");
        assert!(
            app.session().entries().list.len() == 1,
            "the session's transcript keeps only its opening notice"
        );
        assert!(matches!(
            app.entries().list.last(),
            Some(Entry::User(t)) if t == "look at the tests too"
        ));

        // Past the last one it comes back out, and so does esc.
        app.on_key(ctrl_o());
        assert!(app.inside.is_none());
        app.on_key(ctrl_o());
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.inside.is_none());
    }

    #[test]
    fn a_prompt_for_a_subagent_that_has_ended_says_so() {
        let mut app = App::detached();
        app.session().publish(Event::ChildStarted {
            id: "a1".to_string(),
            identity: "worker".to_string(),
            description: "read the docs".to_string(),
            task: "go".to_string(),
        });
        app.open_child("a1");
        app.input.set("anything".to_string());
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            app.entries().list.last(),
            Some(Entry::Error(t)) if t.contains("that subagent has finished")
        ));
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
    fn model_notice_names_the_backend() {
        let local = model_notice("ollama:gemma4:e2b", "medium");
        assert!(local.starts_with("model: ollama:gemma4:e2b on ollama, served locally"));
        assert!(
            !local.contains("medium"),
            "no effort reaches Ollama, so none is claimed: {local}"
        );
        assert!(
            model_notice("gpt-5.5", "high").starts_with("model: gpt-5.5 (high effort) on codex.")
        );
    }

    #[test]
    fn clear_drops_the_conversation_and_what_the_view_held_of_it() {
        let (mut app, _user, mut control) = connected();
        app.entries().push(Entry::Output("old output".to_string()));
        app.expanded.insert(1);
        app.pinned.insert(1);
        app.scroll = 7;
        app.follow = false;

        app.input.set("/clear".to_string());
        app.submit();
        assert!(matches!(
            control.try_recv(),
            Ok(crate::agent::Control::Clear)
        ));
        assert!(app.expanded.is_empty() && app.pinned.is_empty());
        assert_eq!(app.scroll, 0);
        assert!(app.follow);
        // The agent answers once the history is gone, and the transcript goes with it.
        app.session.publish(Event::Cleared);
        let entries = app.entries();
        assert!(
            matches!(entries.list.as_slice(), [Entry::Info(t)] if t == "conversation cleared"),
            "{:?}",
            entries.list
        );
    }

    /// `auto` mode never prompts, so a call it denies has no approval to answer. The
    /// rule is the way to permit one from the prompt, and the deny text names it.
    #[test]
    fn allow_adds_a_rule_from_the_prompt_and_bare_allow_says_how() {
        let (mut app, _user, _control) = connected();
        let last = |app: &mut App| match app.entries().list.last() {
            Some(Entry::Info(text) | Entry::Error(text)) => text.clone(),
            other => panic!("{other:?}"),
        };

        app.input.set("/allow".to_string());
        app.submit();
        assert!(last(&mut app).starts_with("/allow <rule>"));

        app.input.set("/allow Bash(brew install:*)".to_string());
        app.submit();
        assert!(last(&mut app).starts_with("allowed Bash(brew install:*)"));
        assert!(app.session.permissions().contains("Bash(brew install:*)"));

        app.input.set("/allow not a rule".to_string());
        app.submit();
        assert!(last(&mut app).contains("expected a tool name"));
    }

    /// The export is meant to be handed to someone instead of a screenshot, so what it
    /// says about the session has to be the session, not a default the assembly missed.
    #[test]
    fn the_debug_export_carries_the_session_it_was_taken_from() {
        let (mut app, _user, _control) = connected();
        app.session_id = "abc123".to_string();
        app.input.set("/allow Bash(brew install:*)".to_string());
        app.submit();
        app.entries().push(Entry::Command {
            tool: "bash".to_string(),
            summary: "brew install gnupg".to_string(),
        });

        let bundle = app.debug_bundle();
        assert_eq!(bundle.session_id, "abc123");
        assert!(
            bundle
                .session_file
                .as_ref()
                .is_some_and(|f| f.ends_with(".bhai/sessions/abc123.jsonl")),
            "{:?}",
            bundle.session_file
        );
        assert_eq!(bundle.state.model, "gpt-5.5");
        assert_eq!(bundle.backend, "codex");
        assert!(bundle.permissions.contains("Bash(brew install:*)"));

        let dir = std::env::temp_dir().join(format!("bhai-export-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = crate::debug::export(&bundle, &dir).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("brew install gnupg"), "the transcript");
        assert!(text.contains("- session: abc123"), "the header");
        assert!(text.contains("Bash(brew install:*)"), "the rules");
        assert!(bundle.summary(&path).contains("not profiled"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn model_on_its_own_opens_the_picker() {
        let (mut app, _user, _control) = connected();
        app.input.set("/model".to_string());
        app.submit();
        let picker = app.picker.as_ref().expect("the picker is up");
        assert!(
            picker.catalogue.is_none(),
            "the lists are still being asked for"
        );
        // Escape closes it without touching the session.
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.picker.is_none());
        assert_eq!(app.model, "gpt-5.5");
    }

    #[test]
    fn model_with_a_name_switches_without_asking() {
        let (mut app, _user, _control) = connected();
        app.input.set("/model ollama:gemma4:e2b".to_string());
        app.submit();
        assert!(app.picker.is_none(), "a named model asks nothing");
        let state = app.session.state();
        assert_eq!(state.model, "ollama:gemma4:e2b");
        assert_eq!(state.effort, "medium", "the effort is left as it was");

        app.input.set("/model gpt-5.5 xhigh".to_string());
        app.submit();
        let state = app.session.state();
        assert_eq!(
            (state.model.as_str(), state.effort.as_str()),
            ("gpt-5.5", "xhigh")
        );
    }

    #[test]
    fn the_status_bar_follows_the_switch() {
        let (mut app, _user, _control) = connected();
        app.input.set("/model gpt-5.5 low".to_string());
        app.submit();
        // The session publishes the switch; the bar reads what the event carries.
        app.on_event(Event::Model {
            model: "gpt-5.5".to_string(),
            effort: "low".to_string(),
        });
        assert_eq!(
            (app.model.as_str(), app.effort.as_str()),
            ("gpt-5.5", "low")
        );
    }

    #[test]
    fn a_switch_is_refused_while_a_turn_runs() {
        let (mut app, _user, _control) = connected();
        app.input.set("hello".to_string());
        app.submit();
        app.on_event(Event::User("hello".to_string()));
        app.input.set("/model gpt-5.5 low".to_string());
        app.submit();
        assert_eq!(app.session.state().model, "gpt-5.5");
        let entries = app.entries();
        let last = entries.list.last().unwrap();
        assert!(
            matches!(last, Entry::Error(text) if text.contains("a turn is already running")),
            "{last:?}"
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
        assert_eq!(app.queued, ["p1", "p2"]);
        // A queued prompt scrolls into view like a message that starts a turn.
        assert!(app.follow);
        // The front of the queue starting is a user message like any other, and it
        // leaves the queue as it joins the transcript.
        app.on_event(Event::User("p1".to_string()));
        assert_eq!(app.queued, ["p2"]);
        // An interrupt stops the turn and leaves the panel alone: what is waiting is
        // still waiting, and the front of it starts once the cancelled turn ends.
        app.on_event(Event::Interrupted);
        assert_eq!(app.queued, ["p2"]);
        app.on_event(Event::User("p2".to_string()));
        assert!(app.queued.is_empty());
        app.on_event(Event::User("fresh".to_string()));
        assert!(app.queued.is_empty());
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
        // The release copied it without being asked, and said so on the border.
        assert_eq!(
            clipboard::last_copied().as_deref(),
            Some("two\nthree\nfour")
        );
        assert_eq!(app.copied.map(|c| c.chars), Some(14));
        // A drag back up the way it came selects the same span.
        app.on_mouse(at(MouseEventKind::Down(MouseButton::Left), 3, 2));
        app.on_mouse(at(MouseEventKind::Drag(MouseButton::Left), 4, 0));
        assert_eq!(app.selected_text().as_deref(), Some("two\nthree\nfour"));

        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.selection, None, "esc clears it");
        assert_eq!(app.copy_text(), "", "back to the empty input");
    }

    /// A transcript taller than the view it is drawn in, with a row above it so both
    /// edges can be dragged past.
    fn scrolled(lines: usize, height: u16) -> App {
        let mut app = App::detached();
        app.lines = (0..lines).map(|i| format!("line {i}")).collect();
        app.transcript_area = Some(Rect::new(0, 1, 40, height));
        app.max_scroll = lines - height as usize;
        app.follow = false;
        app
    }

    /// The whole point of a drag: the text worth copying is usually longer than the
    /// window, and a selection that stopped at the bottom row could never reach it.
    #[test]
    fn a_drag_past_the_bottom_edge_scrolls_and_keeps_selecting() {
        let mut app = scrolled(20, 4);
        app.on_mouse(at(MouseEventKind::Down(MouseButton::Left), 0, 1));
        // One row below the transcript: two lines a step, and the line it lands on is
        // taken whole, since the drag has travelled over it.
        app.on_mouse(at(MouseEventKind::Drag(MouseButton::Left), 0, 5));
        assert_eq!(app.scroll, 2);
        assert_eq!(
            app.selected_text().as_deref(),
            Some("line 0\nline 1\nline 2\nline 3\nline 4\nline 5")
        );

        // Held there, the pointer says nothing more, so the tick carries it on.
        app.tick();
        assert_eq!(app.scroll, 4);
        assert!(app.selected_text().unwrap().ends_with("line 7"));
        for _ in 0..40 {
            app.tick();
        }
        assert_eq!(app.scroll, app.max_scroll, "it stops at the end");
        assert!(app.selected_text().unwrap().ends_with("line 19"));

        // The release ends it: the tick has nothing left to carry.
        app.on_mouse(at(MouseEventKind::Up(MouseButton::Left), 0, 5));
        let (scroll, selected) = (app.scroll, app.selected_text());
        app.tick();
        assert_eq!((app.scroll, app.selected_text()), (scroll, selected));
    }

    #[test]
    fn a_drag_past_the_top_edge_scrolls_back_up() {
        let mut app = scrolled(20, 4);
        app.scroll = 10;
        app.on_mouse(at(MouseEventKind::Down(MouseButton::Left), 6, 2));
        // Above the transcript: the top line in view, and none of it, so the drag holds
        // what it has passed over rather than the line it is heading into.
        app.on_mouse(at(MouseEventKind::Drag(MouseButton::Left), 0, 0));
        assert_eq!(app.scroll, 8);
        assert_eq!(
            app.selected_text().as_deref(),
            Some("line 8\nline 9\nline 10\nline 11")
        );
        app.tick();
        assert_eq!(app.scroll, 6);
        assert!(app.selected_text().unwrap().starts_with("line 6\n"));
    }

    /// A drag runs out of transcript at the last row in view, not at the edge of the
    /// window, so that is where it starts pulling more of it into view.
    #[test]
    fn the_last_row_in_view_scrolls_without_leaving_the_transcript() {
        let mut app = scrolled(20, 4);
        app.on_mouse(at(MouseEventKind::Down(MouseButton::Left), 0, 1));
        // Row 4 is the last of the four in view, and the pointer is still inside it.
        app.on_mouse(at(MouseEventKind::Drag(MouseButton::Left), 6, 4));
        assert_eq!(app.scroll, 1, "one line, the plain rate");
        app.tick();
        app.tick();
        assert_eq!(app.scroll, 3);
        assert!(app.selected_text().unwrap().ends_with("line 6"));

        // The first row does the same the other way, and a row in between does nothing.
        app.on_mouse(at(MouseEventKind::Drag(MouseButton::Left), 6, 1));
        assert_eq!(app.scroll, 2);
        app.on_mouse(at(MouseEventKind::Drag(MouseButton::Left), 6, 2));
        let scroll = app.scroll;
        app.tick();
        assert_eq!(app.scroll, scroll, "the middle of the view holds still");
    }

    #[test]
    fn ctrl_a_takes_the_whole_transcript_when_nothing_is_drafted() {
        let mut app = scrolled(4, 2);
        app.on_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(
            app.copy_text(),
            "line 0\nline 1\nline 2\nline 3",
            "every line, not the two in view"
        );
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.selection, None);

        // A draft in the prompt is what ctrl+a is for while there is one.
        app.input.set("half a thought".to_string());
        app.on_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(app.selection, None, "the transcript is left alone");
        assert_eq!(app.copy_text(), "half a thought");
    }

    /// The other way to reach text that is off screen, for anyone who would rather
    /// scroll than hold the pointer past the edge.
    #[test]
    fn the_wheel_takes_a_running_drag_with_it() {
        let mut app = scrolled(20, 4);
        app.on_mouse(at(MouseEventKind::Down(MouseButton::Left), 0, 1));
        app.on_mouse(at(MouseEventKind::Drag(MouseButton::Left), 6, 2));
        assert_eq!(app.selected_text().as_deref(), Some("line 0\nline 1"));
        app.on_mouse(at(MouseEventKind::ScrollDown, 6, 2));
        assert_eq!(app.scroll, 3);
        // The pointer has not moved, so the head is whatever the wheel brought under it.
        assert_eq!(
            app.selected_text().as_deref(),
            Some("line 0\nline 1\nline 2\nline 3\nline 4")
        );
        // With no drag under way the wheel only scrolls.
        app.on_mouse(at(MouseEventKind::Up(MouseButton::Left), 6, 2));
        let selected = app.selected_text();
        app.on_mouse(at(MouseEventKind::ScrollDown, 6, 2));
        assert_eq!(app.scroll, 6);
        assert_eq!(app.selected_text(), selected);
    }

    #[test]
    fn a_click_that_selects_nothing_copies_nothing() {
        let mut app = transcript(&["one two"]);
        app.input.set("a draft".to_string());
        // A plain click in the transcript, then one in the input: neither selects, so
        // neither may put the draft or the line on the clipboard.
        app.on_mouse(at(MouseEventKind::Down(MouseButton::Left), 2, 0));
        app.on_mouse(at(MouseEventKind::Up(MouseButton::Left), 2, 0));
        assert_eq!(app.copied, None);
        let row = app.transcript_area.unwrap().bottom() + 1;
        app.input_area = Some((Rect::new(0, row, 40, 1), 0, 40));
        app.on_mouse(at(MouseEventKind::Down(MouseButton::Left), 1, row));
        app.on_mouse(at(MouseEventKind::Up(MouseButton::Left), 1, row));
        assert_eq!(app.copied, None);
        assert_eq!(app.input.value(), "a draft", "and the draft is untouched");
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

    /// A wake is a turn nobody typed. Without this the spinner and the child panel stay
    /// off for its whole duration and a trailing failure still offers `[click to retry]`.
    #[test]
    fn a_landing_report_puts_the_tui_back_to_work_without_moving_the_view() {
        let mut app = App::detached();
        app.scroll = 7;
        app.follow = false;
        app.on_event(Event::Resumed("clean up".to_string()));
        assert!(app.working);
        assert!(
            !app.follow,
            "a report must not yank a scrolled-up reader down"
        );
        assert_eq!(app.scroll, 7);

        app.on_event(Event::TurnEnd);
        assert!(!app.working);
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
    fn the_arrows_walk_the_history_and_move_inside_a_multi_line_prompt() {
        let mut app = App::detached();
        for text in ["/permissions", "/skills"] {
            type_text(&mut app, text);
            app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        }
        let up = |app: &mut App| app.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        let down = |app: &mut App| app.on_key(key(KeyCode::Down, KeyModifiers::NONE));

        // From an empty prompt, up walks back and down walks forward again.
        up(&mut app);
        assert_eq!(app.input.value(), "/skills");
        assert_eq!(app.menu, None, "a recalled command keeps the arrows");
        up(&mut app);
        assert_eq!(app.input.value(), "/permissions");
        up(&mut app);
        assert_eq!(app.input.value(), "/permissions", "the oldest one stays");
        down(&mut app);
        assert_eq!(app.input.value(), "/skills");
        down(&mut app);
        assert_eq!(app.input.value(), "", "back to the draft");
        down(&mut app);
        assert_eq!(app.input.value(), "", "and no further");

        // Inside a prompt of several rows the arrows move between them instead, and
        // only walk the history off the top or the bottom of it.
        let mut app = App::detached();
        app.history.push("earlier").unwrap();
        type_text(&mut app, "one");
        app.on_key(key(KeyCode::Enter, KeyModifiers::ALT));
        type_text(&mut app, "two");
        assert_eq!(app.input_rows(), 2);
        up(&mut app);
        assert_eq!(
            app.input.value(),
            "one\ntwo",
            "it moved a row, not a prompt"
        );
        assert_eq!(app.input.cursor(), 3);
        up(&mut app);
        assert_eq!(app.input.value(), "earlier");
        down(&mut app);
        assert_eq!(app.input.value(), "one\ntwo", "the draft comes back");
    }

    #[test]
    fn ctrl_up_and_down_scroll_the_transcript() {
        let mut app = transcript(&["one", "two", "three"]);
        app.max_scroll = 2;
        app.on_key(key(KeyCode::Down, KeyModifiers::CONTROL));
        assert_eq!(app.scroll, 1);
        app.on_key(key(KeyCode::Up, KeyModifiers::CONTROL));
        assert_eq!(app.scroll, 0);
        assert!(app.input.is_empty(), "the prompt is untouched");
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

    fn with_skill(name: &str) -> App {
        let mut app = App::detached();
        app.skills = vec![Skill {
            name: name.to_string(),
            description: "Does a thing.".to_string(),
            dir: std::path::PathBuf::from("/s"),
            source: "~/.claude/skills".to_string(),
        }];
        app
    }

    #[test]
    fn the_trust_question_is_modal_and_answered_with_one_key() {
        let gate = || {
            Some(TrustGate {
                root: "/repo".to_string(),
                rules: 0,
                mode: Mode::Auto,
            })
        };
        let mut app = App::detached();
        app.trust_gate = gate();
        // Nothing else reaches the app while it is up.
        type_text(&mut app, "hello");
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.input.is_empty());
        assert!(app.trust_gate.is_some());

        app.on_key(key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.trust_gate, None);
        assert!(
            matches!(app.entries().list.last(), Some(Entry::Info(t)) if t.starts_with("not trusted: /repo")),
            "{:?}",
            app.entries().list.last()
        );
        // Answered: the prompt takes keys again.
        type_text(&mut app, "hi");
        assert_eq!(app.input.value(), "hi");

        // Esc is a no, and ctrl+c still leaves.
        let mut app = App::detached();
        app.trust_gate = gate();
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.trust_gate, None);
        let mut app = App::detached();
        app.trust_gate = gate();
        app.on_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);
    }

    #[test]
    fn the_slash_menu_opens_on_what_is_typed_and_closes_on_esc() {
        let mut app = App::detached();
        type_text(&mut app, "hi");
        assert_eq!(app.menu, None);
        // A name is completed wherever it is typed, but only a path-free `/word`.
        type_text(&mut app, " /dif");
        assert_eq!(app.menu, Some(0));
        assert_eq!(
            app.menu_items().first().map(|i| i.name.clone()),
            Some("diff".to_string())
        );
        // Tab takes the name and leaves the rest of the prompt alone; enter does not
        // run a command written in the middle of a sentence.
        app.on_key(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input.value(), "hi /diff");
        assert_eq!(app.menu, None);
        type_text(&mut app, " and src/ma");
        assert_eq!(app.menu, None, "a path is not a command");

        // Enter mid-sentence sends the prompt rather than taking the highlighted row.
        let mut app = App::detached();
        type_text(&mut app, "look at /dif");
        assert_eq!(app.menu, Some(0));
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.input.value(), "", "it was sent");
        assert_eq!(app.menu, None);

        let mut app = App::detached();
        type_text(&mut app, "/qu");
        assert_eq!(app.menu, Some(0));
        let names: Vec<_> = app.menu_items().iter().map(|i| i.name.clone()).collect();
        assert_eq!(names, ["queue", "quit"]);
        app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.menu, Some(1));
        app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.menu, Some(0), "it wraps round");
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.menu, None);
        assert_eq!(app.input.value(), "/qu", "esc keeps what was typed");
        type_text(&mut app, "e");
        assert_eq!(app.menu, Some(0), "the next keystroke opens it again");
    }

    #[test]
    fn enter_runs_the_highlighted_command_and_tab_only_completes_it() {
        let mut app = App::detached();
        type_text(&mut app, "/tok");
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.all_badges, "/tokens ran");
        assert!(app.input.is_empty());
        assert_eq!(app.menu, None);

        type_text(&mut app, "/wor");
        app.on_key(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input.value(), "/workflows");
        assert_eq!(app.menu, None, "a completed name does not reopen the menu");

        let mut app = App::detached();
        type_text(&mut app, "/queu");
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.input.value(), "/queue ", "one that takes input waits");
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(app.entries().list.last(), Some(Entry::Info(t)) if t.starts_with("queue:"))
        );
    }

    #[test]
    fn the_speed_counts_only_what_the_model_streams() {
        let mut app = App::detached();
        app.on_event(Event::Text("not while idle".to_string()));
        assert_eq!(app.speed.counted(), 0);

        app.on_event(Event::Streaming(true));
        app.on_event(Event::Reasoning("thinking".to_string()));
        app.on_event(Event::Text("hello world".to_string()));
        assert_eq!(
            app.speed.counted(),
            5,
            "bytes/4 for a model with no tokenizer"
        );

        // A model with a tokenizer of its own is counted with it, not by its bytes.
        let mut gpt = App::detached();
        gpt.model = "gpt-5.6-sol".to_string();
        gpt.on_event(Event::Streaming(true));
        gpt.on_event(Event::Text("hello world".to_string()));
        assert_eq!(
            gpt.speed.counted(),
            2,
            "o200k_base, as the codex backend uses"
        );

        // The reasoning a call reports but never streamed stays out of it: the rate
        // describes the text on screen, and none of that was.
        let usage = Usage {
            input: 10,
            cached: 0,
            output: 500,
            reasoning: 480,
        };
        app.on_event(Event::Usage(usage));
        assert_eq!(app.speed.counted(), 5);
        assert_eq!(app.tokens_out, 500, "the totals count all of it");

        // A child's call is the parent's idle time, and so is everything after the end.
        app.on_event(Event::Streaming(false));
        app.on_event(Event::ChildUsage(usage));
        app.on_event(Event::Text("nor after".to_string()));
        assert_eq!(app.speed.counted(), 5);
    }

    #[test]
    fn the_grey_suggestion_is_what_tab_would_fill_in() {
        let mut app = with_skill("commit-helper");
        assert_eq!(app.suggestion(), "", "nothing typed, nothing offered");
        type_text(&mut app, "/co");
        assert_eq!(app.suggestion(), "mpact");
        // It follows the highlighted row, and tab fills in exactly what was grey.
        app.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.suggestion(), "mmit-helper");
        app.on_key(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input.value(), "/commit-helper ");
        assert_eq!(app.suggestion(), "", "the menu is shut");

        // Nothing grey mid-prompt, where it would read as text.
        let mut app = App::detached();
        type_text(&mut app, "/qu");
        assert_eq!(app.suggestion(), "eue");
        app.on_key(key(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.suggestion(), "");
        app.on_key(key(KeyCode::Right, KeyModifiers::NONE));
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.suggestion(), "", "esc shuts it");
    }

    #[test]
    fn a_skill_is_offered_after_the_commands_and_sent_as_a_prompt() {
        let mut app = with_skill("commit-helper");
        type_text(&mut app, "/co");
        let names: Vec<_> = app.menu_items().iter().map(|i| i.name.clone()).collect();
        assert_eq!(names, ["compact", "context", "copy", "commit-helper"]);

        app.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.input.value(), "/commit-helper ");
        type_text(&mut app, "only the staged files");
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        // The detached session has no agent, so the prompt gets as far as the channel.
        assert!(
            !matches!(app.entries().list.last(), Some(Entry::Error(t)) if t.starts_with("no command")),
            "the skill was recognised"
        );
    }

    #[test]
    fn an_unknown_slash_word_is_refused_but_a_path_is_not() {
        let mut app = App::detached();
        type_text(&mut app, "/nope");
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(app.entries().list.last(), Some(Entry::Error(t)) if t.starts_with("no command or skill called /nope")),
            "{:?}",
            app.entries().list.last()
        );

        type_text(&mut app, "/usr/bin/env is missing");
        assert_eq!(app.menu, None);
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            !matches!(app.entries().list.last(), Some(Entry::Error(t)) if t.starts_with("no command")),
            "a path is a prompt, not a command"
        );
    }
}
