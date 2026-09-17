//! UI state and the event handling that mutates it.

use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::sync::Arc;

use serde_json::Value;
use tui_input::Input;
use tui_input::InputRequest;
use tui_input::backend::crossterm::to_input_request;

use crate::client::Usage;
use crate::permissions::{Answer, Mode, Remember};
use crate::profile::{self, CallTokens, EntryTokens, Tokens, Transcript};
use crate::session::{Approval, Event, Session};
use crate::skills::Skill;

/// Lines a mouse wheel notch moves the transcript.
const WHEEL_LINES: usize = 3;
/// Characters of an entry the `/context` transcript table shows.
const LABEL_CHARS: usize = 40;

#[derive(Debug)]
pub enum Entry {
    User(String),
    Assistant(String),
    Reasoning(String),
    Command(String),
    Output(String),
    Rejected(String),
    Error(String),
    Info(String),
}

/// Ties history items and calls to the entries that show them.
#[derive(Default)]
struct Attribution {
    /// History index to entry, for user messages and tool results.
    items: BTreeMap<usize, usize>,
    /// Entries from here on are not yet tied to a call or item.
    mark: usize,
    /// Output tokens of each function call still waiting for its result.
    calls: VecDeque<u64>,
    /// Totals of a call that wrote no text, for its first function call entry.
    totals: Option<Usage>,
    /// Child agent usage since the last tool result.
    child: Option<Usage>,
}

pub struct App {
    pub entries: Vec<Entry>,
    /// Token attribution by entry index.
    pub tokens: HashMap<usize, Tokens>,
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
    /// The input text's area and horizontal scroll, filled in by the renderer.
    pub input_area: Option<(Rect, usize)>,
    /// The transcript scrollbar, filled in by the renderer when the transcript overflows.
    pub scrollbar: Option<Rect>,
    /// A left drag that started on the scrollbar is in progress.
    dragging: bool,
    attribution: Attribution,
    pub input: Input,
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
    /// The skills in the system prompt, for `/skills`.
    pub skills: Vec<Skill>,
    /// The session's MCP servers, for `/mcp`.
    pub mcp: Option<Arc<crate::mcp::Hub>>,
    pub quit: bool,
    session: Arc<Session>,
}

impl App {
    pub fn new(session: Arc<Session>) -> Self {
        Self {
            entries: vec![Entry::Info(
                "bhai · bash, read, write and edit; every change needs your approval. Type a task and hit enter."
                    .to_string(),
            )],
            tokens: HashMap::new(),
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
            attribution: Attribution::default(),
            input: Input::default(),
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
            skills: Vec::new(),
            mcp: None,
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

        match key.code {
            KeyCode::Char('c') if ctrl => {
                if self.working {
                    self.interrupt();
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Char('d') if ctrl && self.input.value().is_empty() => self.quit = true,
            KeyCode::Esc if self.working => self.interrupt(),
            KeyCode::Char('t') if ctrl => self.all_badges = !self.all_badges,
            KeyCode::BackTab => self.mode = self.session.cycle_mode(),
            KeyCode::Enter => self.submit(),
            KeyCode::PageUp => self.scroll_by(-(self.page as isize)),
            KeyCode::PageDown => self.scroll_by(self.page as isize),
            // Left/Right belong to the cursor, so the transcript scrolls with up/down.
            KeyCode::Up => self.scroll_by(-1),
            KeyCode::Down => self.scroll_by(1),
            _ => {
                if let Some(request) = input_request(key) {
                    self.input.handle(request);
                }
            }
        }
    }

    /// Returns whether the screen needs a redraw.
    pub fn on_mouse(&mut self, mouse: MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_by(-(WHEEL_LINES as isize)),
            MouseEventKind::ScrollDown => self.scroll_by(WHEEL_LINES as isize),
            MouseEventKind::Moved => {
                self.mouse_row = Some(mouse.row);
                return self.rehover();
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
        if let Some((area, scroll)) = self.input_area.filter(|(area, _)| area.contains(at)) {
            return self.place_cursor(scroll + (x - area.x) as usize);
        }
        let Some(entry) = self.entry_at(y) else {
            return false;
        };
        let set = if matches!(self.entries.get(entry), Some(Entry::Output(_))) {
            &mut self.expanded
        } else {
            &mut self.pinned
        };
        if !set.remove(&entry) {
            set.insert(entry);
        }
        true
    }

    /// Put the input cursor at display column `column`; false when there is no text.
    fn place_cursor(&mut self, column: usize) -> bool {
        if self.input.value().is_empty() {
            return false;
        }
        let chars = self.input.value().chars().count();
        let mut input = std::mem::take(&mut self.input);
        for cursor in 0..=chars {
            input = input.with_cursor(cursor);
            if input.visual_cursor() >= column {
                break;
            }
        }
        self.input = input;
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

    /// Show a history resumed from disk. Encrypted reasoning without a summary is skipped.
    pub fn restore(&mut self, history: &[Value]) {
        for (index, item) in history.iter().enumerate() {
            let text = |key: &str| {
                item[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let entry = match item["type"].as_str() {
                Some("message") if item["role"] == "user" => Entry::User(text("content")),
                Some("message") => Entry::Assistant(text("content")),
                Some("reasoning") if !text("summary").is_empty() => {
                    Entry::Reasoning(text("summary"))
                }
                Some("function_call") => Entry::Command(format!(
                    "{} {}",
                    item["name"].as_str().unwrap_or_default(),
                    item["arguments"].as_str().unwrap_or_default()
                )),
                Some("function_call_output") => Entry::Output(
                    item["output"]
                        .as_str()
                        .unwrap_or_default()
                        .trim_end()
                        .to_string(),
                ),
                _ => continue,
            };
            if matches!(entry, Entry::User(_) | Entry::Output(_)) {
                self.attribution.items.insert(index, self.entries.len());
            }
            self.entries.push(entry);
        }
        self.attribution.mark = self.entries.len();
    }

    pub fn on_event(&mut self, event: Event) {
        match event {
            // Messages from any consumer (this TUI or the debug server) land here.
            Event::User(message) => {
                self.entries.push(Entry::User(message));
                self.follow = true;
                self.working = true;
            }
            Event::Text(delta) => self.append(delta, Stream::Assistant),
            Event::Reasoning(delta) => self.append(delta, Stream::Reasoning),
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
            Event::Resolved { id, .. } => {
                if self.pending.as_ref().is_some_and(|p| p.id == id) {
                    self.pending = None;
                }
            }
            Event::ToolStart(command) => self.entries.push(Entry::Command(command)),
            Event::ToolOutput(output) => {
                self.entries
                    .push(Entry::Output(output.trim_end().to_string()));
            }
            Event::ToolRejected(command) => {
                self.entries
                    .push(Entry::Rejected(format!("rejected: {command}")));
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
                let child = self.attribution.child.get_or_insert_default();
                child.input += usage.input;
                child.cached += usage.cached;
                child.output += usage.output;
                child.reasoning += usage.reasoning;
            }
            Event::Call(call) => self.on_call(call),
            Event::Item(index) => self.on_item(index),
            Event::Cache(found) => {
                if let Some(found) = &found {
                    self.entries.push(Entry::Error(format!(
                        "cache break: {}: {}",
                        found.field, found.detail
                    )));
                }
                self.cache_break = found.map(|f| f.field);
            }
            Event::CacheHit(hit) => {
                self.cache_miss = hit
                    .hit_ratio
                    .filter(|_| hit.miss())
                    .map(|ratio| ratio * 100.0);
            }
            Event::Info(message) => self.entries.push(Entry::Info(message)),
            Event::Compacted(message) => {
                self.attribution.items.clear();
                self.entries.push(Entry::Info(message));
            }
            Event::Mode(mode) => self.mode = mode,
            Event::Error(message) => self.entries.push(Entry::Error(message)),
            Event::Interrupted => self.entries.push(Entry::Info("interrupted".to_string())),
            Event::TurnEnd => self.working = false,
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
        let message = self.input.value_and_reset().trim().to_string();
        if message.starts_with("/context") {
            self.export_context();
            return;
        }
        if message == "/compact" {
            self.follow = true;
            match self.session.compact() {
                Ok(()) => self.working = true,
                Err(e) => self.entries.push(Entry::Error(e.to_string())),
            }
            return;
        }
        if message.starts_with("/permissions") {
            self.follow = true;
            self.entries.push(Entry::Info(self.session.permissions()));
            return;
        }
        if message == "/trust" || message == "/untrust" {
            self.follow = true;
            let result = match message.as_str() {
                "/trust" => self.session.trust(),
                _ => self.session.untrust(),
            };
            self.entries.push(match result {
                Ok(text) => Entry::Info(text),
                Err(e) => Entry::Error(format!("{e:#}")),
            });
            return;
        }
        if let Some(rest) = message.strip_prefix("/as")
            && (rest.is_empty() || rest.starts_with(' '))
        {
            self.follow = true;
            self.entries
                .push(Entry::Info(switch_notice(&self.identity, rest.trim())));
            return;
        }
        if message.starts_with("/mcp") {
            self.follow = true;
            self.entries
                .push(Entry::Info(crate::mcp::report(self.mcp.as_deref())));
            return;
        }
        if message.starts_with("/skills") {
            self.follow = true;
            self.entries.push(Entry::Info(skills_report(&self.skills)));
            return;
        }
        // The transcript entry arrives back as `Event::User` once the session accepts it.
        if let Err(e) = self.session.submit(message) {
            self.entries.push(Entry::Error(e.to_string()));
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
        let mut entries: Vec<EntryTokens> = self
            .tokens
            .iter()
            .map(|(&index, &tokens)| {
                let entry = &self.entries[index];
                EntryTokens {
                    index,
                    kind: entry.kind(),
                    label: entry.label(),
                    tokens,
                }
            })
            .collect();
        entries.sort_by_key(|e| e.index);
        let state = self.session.state();
        Transcript {
            entries,
            totals: Usage {
                input: state.input_tokens,
                cached: state.cached_tokens,
                output: state.output_tokens,
                reasoning: state.reasoning_tokens,
            },
            calls: state.calls,
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

    fn scroll_by(&mut self, delta: isize) {
        let target = self.scroll as isize + delta;
        self.scroll = target.clamp(0, self.max_scroll as isize) as usize;
        self.follow = self.scroll >= self.max_scroll;
    }

    /// Tie a finished call to the entries it read and wrote.
    fn on_call(&mut self, call: CallTokens) {
        let first = call.sent - call.inputs.len();
        for (offset, &input) in call.inputs.iter().enumerate() {
            if let Some(&entry) = self.attribution.items.get(&(first + offset)) {
                let tokens = self.tokens.entry(entry).or_default();
                tokens.input = Some(input);
                tokens.method = call.method;
            }
        }
        // The usage only says how much of the whole call was cached, so each resent
        // entry takes the call's cached ratio: the per-entry split is proportional, not
        // exact.
        let ratio = match call.usage.input {
            0 => 0.0,
            input => call.usage.cached as f64 / input as f64,
        };
        for entry in self.attribution.items.range(..first).map(|(_, e)| *e) {
            let tokens = self.tokens.entry(entry).or_default();
            if let Some(input) = tokens.input {
                tokens.resends += 1;
                tokens.cached += (input as f64 * ratio).round() as u64;
            }
        }

        let fresh = self.attribution.mark..self.entries.len();
        let of = |want: fn(&Entry) -> bool| -> Vec<usize> {
            fresh.clone().filter(|&i| want(&self.entries[i])).collect()
        };
        let text = of(|e| matches!(e, Entry::Assistant(_)));
        let thinking = of(|e| matches!(e, Entry::Reasoning(_)));
        self.share(&text, call.text, |tokens, part| tokens.output = Some(part));
        self.share(&thinking, call.usage.reasoning, |tokens, part| {
            tokens.reasoning = Some(part)
        });
        match (text.last(), thinking.last()) {
            (Some(&entry), _) => self.tokens.entry(entry).or_default().call = Some(call.usage),
            _ if !call.calls.is_empty() => self.attribution.totals = Some(call.usage),
            (None, Some(&entry)) => self.tokens.entry(entry).or_default().call = Some(call.usage),
            (None, None) => {}
        }
        self.attribution.calls.extend(call.calls);
        self.attribution.mark = self.entries.len();
    }

    /// Split `total` over `entries` by their text length.
    fn share(&mut self, entries: &[usize], total: u64, set: impl Fn(&mut Tokens, u64)) {
        let weights: Vec<u64> = entries
            .iter()
            .map(|&i| self.entries[i].text().len() as u64)
            .collect();
        for (&entry, part) in entries.iter().zip(profile::split(total, &weights)) {
            set(self.tokens.entry(entry).or_default(), part);
        }
    }

    /// Tie history item `index` to the entry just shown: a tool result when a function
    /// call is waiting for one, else the user message.
    fn on_item(&mut self, index: usize) {
        let fresh = self.attribution.mark..self.entries.len();
        let last = |want: fn(&Entry) -> bool| fresh.clone().rev().find(|&i| want(&self.entries[i]));
        match self.attribution.calls.pop_front() {
            Some(output) => {
                // A child's commands come after the parent's own, and its results before.
                let command = fresh
                    .clone()
                    .find(|&i| matches!(self.entries[i], Entry::Command(_)));
                let result = last(|e| matches!(e, Entry::Output(_) | Entry::Rejected(_)));
                if let Some(entry) = command.or(result) {
                    let tokens = self.tokens.entry(entry).or_default();
                    tokens.output = Some(output);
                    tokens.call = self.attribution.totals.take().or(tokens.call);
                }
                let child = self.attribution.child.take();
                if let Some(entry) = result {
                    self.attribution.items.insert(index, entry);
                    if child.is_some() {
                        self.tokens.entry(entry).or_default().child = child;
                    }
                }
            }
            None => {
                if let Some(entry) = last(|e| matches!(e, Entry::User(_))) {
                    self.attribution.items.insert(index, entry);
                }
            }
        }
        self.attribution.mark = self.entries.len();
    }

    /// Append a streaming delta to the last entry of the same kind, or start a new one.
    /// Entries of a call already accounted for are never extended.
    fn append(&mut self, delta: String, kind: Stream) {
        let open = self.entries.len() > self.attribution.mark;
        let appended = match (self.entries.last_mut().filter(|_| open), kind) {
            (Some(Entry::Assistant(text)), Stream::Assistant) => {
                text.push_str(&delta);
                true
            }
            (Some(Entry::Reasoning(text)), Stream::Reasoning) => {
                text.push_str(&delta);
                true
            }
            _ => false,
        };
        if appended {
            return;
        }
        // Drop the leading whitespace a new block often starts with.
        let text = delta.trim_start().to_string();
        if text.is_empty() {
            return;
        }
        self.entries.push(match kind {
            Stream::Assistant => Entry::Assistant(text),
            Stream::Reasoning => Entry::Reasoning(text),
        });
    }
}

impl Entry {
    pub fn text(&self) -> &str {
        match self {
            Entry::User(t)
            | Entry::Assistant(t)
            | Entry::Reasoning(t)
            | Entry::Command(t)
            | Entry::Output(t)
            | Entry::Rejected(t)
            | Entry::Error(t)
            | Entry::Info(t) => t,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Entry::User(_) => "user",
            Entry::Assistant(_) => "assistant",
            Entry::Reasoning(_) => "thinking",
            Entry::Command(_) => "command",
            Entry::Output(_) => "output",
            Entry::Rejected(_) => "rejected",
            Entry::Error(_) => "error",
            Entry::Info(_) => "info",
        }
    }

    /// The first line, cut to `LABEL_CHARS`.
    pub fn label(&self) -> String {
        let line = self.text().trim().lines().next().unwrap_or_default();
        match line.char_indices().nth(LABEL_CHARS) {
            Some((end, _)) => format!("{}...", &line[..end]),
            None => line.to_string(),
        }
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

#[derive(Clone, Copy)]
enum Stream {
    Assistant,
    Reasoning,
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
    use crate::profile::Method;

    fn usage(input: u64, cached: u64, output: u64, reasoning: u64) -> Usage {
        Usage {
            input,
            cached,
            output,
            reasoning,
        }
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
    fn calls_are_attributed_to_their_entries() {
        let mut app = App::detached();
        let text = |s: &str| s.to_string();
        app.on_event(Event::User(text("hi")));
        app.on_event(Event::Item(0));
        app.on_event(Event::Reasoning(text("think")));
        app.on_event(Event::Text(text("hello")));
        app.on_event(Event::Call(CallTokens {
            usage: usage(100, 0, 30, 10),
            sent: 1,
            outputs: 3,
            inputs: vec![5],
            method: Method::Tokenized,
            text: 12,
            calls: vec![8],
        }));
        app.on_event(Event::ToolStart(text("agent worker: go")));
        app.on_event(Event::ChildUsage(usage(7, 2, 1, 0)));
        app.on_event(Event::ToolStart(text("[child a worker] ls")));
        app.on_event(Event::ToolOutput(text("[child a worker] x")));
        app.on_event(Event::ToolOutput(text("child done")));
        app.on_event(Event::Item(4));
        // The next call's text starts an entry of its own.
        app.on_event(Event::Reasoning(text("more")));
        app.on_event(Event::Text(text("done")));
        app.on_event(Event::Call(CallTokens {
            usage: usage(150, 100, 5, 1),
            sent: 5,
            outputs: 2,
            inputs: vec![20],
            method: Method::Exact,
            text: 4,
            calls: vec![],
        }));
        assert_eq!(app.entries.len(), 10);

        let get = |i: usize| app.tokens.get(&i).copied().unwrap_or_default();
        // The user message was tokenized, then resent once at the call's cached ratio.
        assert_eq!(
            get(1),
            Tokens {
                input: Some(5),
                method: Method::Tokenized,
                resends: 1,
                cached: 3,
                ..Tokens::default()
            }
        );
        assert_eq!(get(2).reasoning, Some(10));
        assert_eq!(get(3).output, Some(12));
        assert_eq!(get(3).call, Some(usage(100, 0, 30, 10)));
        assert_eq!(get(4).output, Some(8));
        assert_eq!(
            get(5),
            Tokens::default(),
            "child entries are not the parent's"
        );
        let result = get(7);
        assert_eq!((result.input, result.method), (Some(20), Method::Exact));
        assert_eq!(result.resends, 0);
        assert_eq!(result.child, Some(usage(7, 2, 1, 0)));
        assert_eq!(get(8).reasoning, Some(1));
        assert_eq!(get(9).output, Some(4));
        assert_eq!(get(9).call, Some(usage(150, 100, 5, 1)));
    }

    #[test]
    fn a_call_without_text_puts_its_totals_on_the_command() {
        let mut app = App::detached();
        app.on_event(Event::User("hi".to_string()));
        app.on_event(Event::Item(0));
        app.on_event(Event::Call(CallTokens {
            usage: usage(10, 0, 3, 0),
            sent: 1,
            outputs: 1,
            inputs: vec![1],
            calls: vec![3],
            ..CallTokens::default()
        }));
        app.on_event(Event::ToolRejected("ls".to_string()));
        app.on_event(Event::Item(2));
        let rejected = app.tokens[&2];
        assert_eq!(rejected.output, Some(3));
        assert_eq!(rejected.call, Some(usage(10, 0, 3, 0)));
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
    fn a_restored_history_shows_as_entries() {
        let mut app = App::detached();
        let history = [
            serde_json::json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}),
            serde_json::json!({"type": "reasoning", "encrypted_content": "x", "summary": []}),
            serde_json::json!({"type": "function_call", "call_id": "c", "name": "bash", "arguments": "{}"}),
            serde_json::json!({"type": "function_call_output", "call_id": "c", "output": "ok\n"}),
            serde_json::json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]}),
        ];
        app.restore(&history);
        let kinds: Vec<_> = app.entries[1..].iter().map(Entry::kind).collect();
        assert_eq!(kinds, ["user", "command", "output", "assistant"]);
        assert_eq!(app.entries[3].text(), "ok");
        assert_eq!(app.attribution.items.get(&3), Some(&3));
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
}
