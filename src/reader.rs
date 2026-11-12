//! A local, read-only hackmd view over the running session.

use std::path::Path;

use hackmd::tui::{app, theme, ui};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::style::{Color, Style};
use ratatui::widgets::Paragraph;

pub struct Reader {
    app: app::App,
}

impl Reader {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let mut app = app::App::new(
            app::Source::File(path.to_path_buf()),
            app::Options {
                width: 0,
                line_numbers: false,
                theme: theme::Theme::dark(),
            },
        )?;
        if let app::View::Reader(reader) = &mut app.view {
            reader.exit_present();
        }
        Ok(Self { app })
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        ui::draw(frame, &mut self.app);
        let mut footer = frame.area();
        footer.y = footer.bottom().saturating_sub(1);
        footer.height = footer.height.min(1);
        frame.render_widget(
            Paragraph::new(" Esc back to bhai · ↑/↓ j/k scroll · PgUp/PgDn page · Home/End ")
                .style(Style::new().fg(Color::DarkGray)),
            footer,
        );
    }

    pub fn key(&mut self, key: KeyEvent) {
        let page = i32::from(self.app.viewport.height.saturating_sub(1).max(1));
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let delta = match key.code {
            KeyCode::Down | KeyCode::Char('j') => 1,
            KeyCode::Up | KeyCode::Char('k') => -1,
            KeyCode::PageDown | KeyCode::Char(' ') => page,
            KeyCode::PageUp => -page,
            KeyCode::Char('d') if ctrl => (page / 2).max(1),
            KeyCode::Char('u') if ctrl => -(page / 2).max(1),
            KeyCode::Home | KeyCode::Char('g') => -i32::MAX,
            KeyCode::End | KeyCode::Char('G') => i32::MAX,
            _ => return,
        };
        self.scroll(delta);
    }

    pub fn mouse(&mut self, mouse: MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::ScrollDown => self.scroll(3),
            MouseEventKind::ScrollUp => self.scroll(-3),
            _ => return false,
        }
        true
    }

    fn scroll(&mut self, delta: i32) {
        let app::View::Reader(reader) = &mut self.app.view else {
            return;
        };
        let rows = reader
            .rendered
            .as_ref()
            .map_or(0, |rendered| rendered.lines.len());
        let max = rows.saturating_sub(usize::from(self.app.viewport.height));
        reader.scroll = i64::from(reader.scroll)
            .saturating_add(i64::from(delta))
            .clamp(0, max.min(u16::MAX as usize) as i64) as u16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn scroll(reader: &Reader) -> u16 {
        match &reader.app.view {
            app::View::Reader(view) => view.scroll,
            _ => panic!("not a reader"),
        }
    }

    #[test]
    fn hackmd_renders_and_keyboard_and_mouse_scroll_with_bounds() {
        let raw = (0..100).map(|i| format!("paragraph {i}\n\n")).collect();
        let mut reader = Reader {
            app: app::App::new(
                app::Source::Stdin(raw),
                app::Options {
                    width: 0,
                    line_numbers: false,
                    theme: theme::Theme::dark(),
                },
            )
            .unwrap(),
        };
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal.draw(|frame| reader.draw(frame)).unwrap();
        assert_eq!(scroll(&reader), 0);
        reader.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        let page = scroll(&reader);
        assert!(page > 1);
        reader.mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 3,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(scroll(&reader), page + 3);
        reader.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(scroll(&reader), 0);
        reader.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(scroll(&reader), 0);
        reader.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        let end = scroll(&reader);
        assert!(end > page);
        reader.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(scroll(&reader), end);
        terminal.draw(|frame| reader.draw(frame)).unwrap();
        reader.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert!(scroll(&reader) < end);
    }

    #[test]
    fn marp_files_open_as_a_scrolling_document_not_one_slide() {
        let dir = crate::tools::temp_dir();
        let path = dir.join("slides.md");
        std::fs::write(
            &path,
            "---\nmarp: true\n---\n# first slide\n\n---\n\n# second slide\n",
        )
        .unwrap();
        let mut reader = Reader::open(&path).unwrap();
        let app::View::Reader(view) = &reader.app.view else {
            panic!("not a reader")
        };
        assert!(!view.marp_present());
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| reader.draw(frame)).unwrap();
        let app::View::Reader(view) = &reader.app.view else {
            panic!("not a reader")
        };
        let text: String = view
            .rendered
            .as_ref()
            .unwrap()
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("first slide") && text.contains("second slide"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_file_is_an_error() {
        assert!(Reader::open(Path::new("/nonexistent-bhai-reader/file.md")).is_err());
    }
}
