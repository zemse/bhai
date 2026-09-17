//! UI state and the event handling that mutates it.

use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use std::collections::HashSet;
use std::ops::Range;
use std::sync::{Arc, MutexGuard};

use tui_input::InputRequest;
use tui_input::backend::crossterm::to_input_request;

use crate::client::Usage;
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
    /// The input text's area and its line and column scroll, filled in by the renderer.
    pub input_area: Option<(Rect, usize, usize)>,
    /// The transcript scrollbar, filled in by the renderer when the transcript overflows.
    pub scrollbar: Option<Rect>,
    /// A left drag that started on the scrollbar is in progress.
    dragging: bool,
    pub input: Editor,
    /// Submitted prompts, for `ctrl+p` and `ctrl+n`.
    pub history: History,
    pub working: bool,
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
            input: Editor::default(),
            history: History::default(),
            working: false,
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
            KeyCode::Char('t') if ctrl => self.all_badges = !self.all_badges,
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
            KeyCode::Up if self.input.is_multiline() => {
                self.input.move_line(-1);
            }
            KeyCode::Down if self.input.is_multiline() => {
                self.input.move_line(1);
            }
            KeyCode::Up => self.scroll_by(-1),
            KeyCode::Down => self.scroll_by(1),
            _ => {
                if let Some(request) = input_request(key) {
                    self.input.handle(request);
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
            MouseEventKind::Up(MouseButton::Left) => {
                self.dragging = false;
                return false;
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
        if let Some((area, top, scroll)) = self.input_area.filter(|(area, ..)| area.contains(at)) {
            if self.input.is_empty() {
                return false;
            }
            let row = top + (y - area.y) as usize;
            self.input.place(row, scroll + (x - area.x) as usize);
            return true;
        }
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
            }
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
            }
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
        if message.starts_with("/skills") {
            self.follow = true;
            self.note(Entry::Info(skills_report(&self.skills)));
            return;
        }
        // The transcript entry arrives back as `Event::User` once the session accepts it.
        if let Err(e) = self.session.submit(message) {
            self.note(Entry::Error(e.to_string()));
        }
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

/// tui-input's crossterm mapping covers readline keys and `ctrl+arrow`, but not the
/// escape sequences macOS terminals send for `option+arrow` and `cmd+arrow`. Those are
/// mapped here first; everything else falls through to tui-input.
fn input_request(key: KeyEvent) -> Option<InputRequest> {
    let alt =
        key.modifiers.contains(KeyModifiers::ALT) || key.modifiers.contains(KeyModifiers::META);
    let cmd = key.modifiers.contains(KeyModifiers::SUPER);

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

        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            ..moved(4)
        };
        assert!(app.on_mouse(click));
        assert!(app.pinned.contains(&1));
        assert!(app.on_mouse(click));
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
        assert_eq!(app.input.cursor_position(), (1, 0));
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
