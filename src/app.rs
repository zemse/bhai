//! UI state and the event handling that mutates it.

use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use std::sync::Arc;

use tui_input::Input;
use tui_input::InputRequest;
use tui_input::backend::crossterm::to_input_request;

use crate::client::Usage;
use crate::permissions::{Answer, Mode, Remember};
use crate::profile;
use crate::session::{Approval, Event, Session};
use crate::skills::Skill;

/// Lines a mouse wheel notch moves the transcript.
const WHEEL_LINES: usize = 3;

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

pub struct App {
    pub entries: Vec<Entry>,
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

    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_by(-(WHEEL_LINES as isize)),
            MouseEventKind::ScrollDown => self.scroll_by(WHEEL_LINES as isize),
            _ => {}
        }
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
            }
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
        if message.starts_with("/permissions") {
            self.follow = true;
            self.entries.push(Entry::Info(self.session.permissions()));
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
        let session = Arc::clone(&self.session);
        tokio::spawn(async move {
            let event = match session.context().await {
                Some(profile) => match profile::export(&profile, &profile::debug_dir()) {
                    Ok(path) => Event::Info(profile.summary(&path)),
                    Err(e) => Event::Error(format!("context export failed: {e:#}")),
                },
                None => Event::Error("the agent is not running".to_string()),
            };
            session.publish(event);
        });
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

    /// Append a streaming delta to the last entry of the same kind, or start a new one.
    fn append(&mut self, delta: String, kind: Stream) {
        let appended = match (self.entries.last_mut(), kind) {
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
mod tests {
    use super::*;

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
}
