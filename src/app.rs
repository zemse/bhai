//! UI state and the event handling that mutates it.

use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use tokio::sync::{mpsc, oneshot};
use tui_input::Input;
use tui_input::InputRequest;
use tui_input::backend::crossterm::to_input_request;

use crate::agent::AgentEvent;

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
    pub pending: Option<(String, oneshot::Sender<bool>)>,
    pub scroll: usize,
    pub max_scroll: usize,
    /// Transcript viewport height, filled in by the renderer so page keys match the view.
    pub page: usize,
    pub follow: bool,
    pub spinner: usize,
    pub model: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub quit: bool,
    tx_user: mpsc::Sender<String>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl App {
    pub fn new(
        model: String,
        tx_user: mpsc::Sender<String>,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            entries: vec![Entry::Info(
                "bhai · one tool (bash), every command needs your approval. Type a task and hit enter."
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
            model,
            tokens_in: 0,
            tokens_out: 0,
            quit: false,
            tx_user,
            cancel,
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
                KeyCode::Char('a') | KeyCode::Char('y') => self.answer(true),
                KeyCode::Char('r') | KeyCode::Char('n') | KeyCode::Esc => self.answer(false),
                KeyCode::Char('c') if ctrl => {
                    self.answer(false);
                    self.interrupt();
                }
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

    pub fn on_agent(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Text(delta) => self.append(delta, Stream::Assistant),
            AgentEvent::Reasoning(delta) => self.append(delta, Stream::Reasoning),
            AgentEvent::Approval { command, reply } => self.pending = Some((command, reply)),
            AgentEvent::ToolStart(command) => self.entries.push(Entry::Command(command)),
            AgentEvent::ToolOutput(output) => {
                self.entries
                    .push(Entry::Output(output.trim_end().to_string()));
            }
            AgentEvent::ToolRejected(command) => {
                self.entries
                    .push(Entry::Rejected(format!("rejected: {command}")));
            }
            AgentEvent::Usage { input, output } => {
                self.tokens_in += input;
                self.tokens_out += output;
            }
            AgentEvent::Error(message) => self.entries.push(Entry::Error(message)),
            AgentEvent::TurnEnd => self.working = false,
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
        self.entries.push(Entry::User(message.clone()));
        self.follow = true;
        match self.tx_user.try_send(message) {
            Ok(()) => self.working = true,
            Err(_) => self.entries.push(Entry::Error(
                "the agent is not accepting messages".to_string(),
            )),
        }
    }

    fn answer(&mut self, accept: bool) {
        if let Some((_, reply)) = self.pending.take() {
            let _ = reply.send(accept);
        }
    }

    fn interrupt(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.entries.push(Entry::Info("interrupted".to_string()));
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
    fn typing_inserts_characters() {
        assert_eq!(
            input_request(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(InputRequest::InsertChar('x'))
        );
    }
}
