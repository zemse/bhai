//! The `/diff` pane: the working tree's changes against HEAD, a file list and the
//! selected file's hunks. Git runs directly since these are the user's read-only views.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

/// Git's empty tree, the base in a repository with no commits yet.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// Untracked files larger than this are not shown.
const MAX_UNTRACKED_BYTES: u64 = 1 << 20;
/// Width of the file list column.
const LIST_WIDTH: u16 = 36;

/// One changed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    /// The path before a rename or copy.
    pub from: Option<String>,
    /// The porcelain `XY` status.
    pub status: String,
    pub added: Option<u64>,
    pub removed: Option<u64>,
}

impl FileChange {
    pub fn untracked(&self) -> bool {
        self.status == "??"
    }

    /// A one-letter status, untracked files counting as new.
    fn letter(&self) -> char {
        if self.untracked() {
            return 'N';
        }
        self.status.chars().find(|c| *c != ' ').unwrap_or(' ')
    }
}

/// How a diff line is colored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Header,
    Hunk,
    Added,
    Removed,
    Context,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Files,
    Diff,
}

#[derive(Debug)]
pub struct DiffView {
    root: PathBuf,
    /// Why there is nothing to show, e.g. outside a git repository.
    pub message: Option<String>,
    pub files: Vec<FileChange>,
    pub selected: usize,
    pub lines: Vec<(LineKind, String)>,
    pub scroll: usize,
    pub focus: Focus,
    /// The list and diff areas, filled in by the renderer.
    list_area: Rect,
    diff_area: Rect,
}

impl DiffView {
    /// Opens the view on the repository containing `dir`.
    pub fn open(dir: &Path) -> Self {
        let mut view = Self {
            root: dir.to_path_buf(),
            message: None,
            files: Vec::new(),
            selected: 0,
            lines: Vec::new(),
            scroll: 0,
            focus: Focus::Files,
            list_area: Rect::default(),
            diff_area: Rect::default(),
        };
        view.refresh();
        view
    }

    /// Reloads the file list, keeping the selection on the same path when it remains.
    pub fn refresh(&mut self) {
        let current = self.files.get(self.selected).map(|f| f.path.clone());
        match load_files(&self.root) {
            Ok((root, files)) => {
                self.root = root;
                self.files = files;
                self.message = self.files.is_empty().then(|| "no changes".to_string());
            }
            Err(e) => {
                self.files.clear();
                self.message = Some(format!("{e:#}"));
            }
        }
        self.selected = current
            .and_then(|path| self.files.iter().position(|f| f.path == path))
            .unwrap_or(0);
        self.load_diff();
    }

    fn load_diff(&mut self) {
        self.scroll = 0;
        self.lines = match self.files.get(self.selected) {
            Some(file) => file_diff(&self.root, file)
                .unwrap_or_else(|e| vec![(LineKind::Header, format!("{e:#}"))]),
            None => Vec::new(),
        };
    }

    fn select(&mut self, index: usize) {
        let index = index.min(self.files.len().saturating_sub(1));
        if index != self.selected {
            self.selected = index;
            self.load_diff();
        }
    }

    /// Returns false when the key closes the view.
    pub fn on_key(&mut self, code: KeyCode) -> bool {
        let page = self.diff_area.height.saturating_sub(2).max(1) as isize;
        match (code, self.focus) {
            (KeyCode::Esc | KeyCode::Char('q'), _) => return false,
            (KeyCode::Tab, Focus::Files) => self.focus = Focus::Diff,
            (KeyCode::Tab, Focus::Diff) => self.focus = Focus::Files,
            (KeyCode::Char('r'), _) => self.refresh(),
            (KeyCode::PageUp, _) => self.scroll_by(-page),
            (KeyCode::PageDown, _) => self.scroll_by(page),
            (KeyCode::Up | KeyCode::Char('k'), Focus::Files) => {
                self.select(self.selected.saturating_sub(1))
            }
            (KeyCode::Down | KeyCode::Char('j'), Focus::Files) => self.select(self.selected + 1),
            (KeyCode::Up | KeyCode::Char('k'), Focus::Diff) => self.scroll_by(-1),
            (KeyCode::Down | KeyCode::Char('j'), Focus::Diff) => self.scroll_by(1),
            _ => {}
        }
        true
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self
            .lines
            .len()
            .saturating_sub(self.diff_area.height.saturating_sub(2) as usize);
        self.scroll = self.scroll.saturating_add_signed(delta).min(max);
    }

    /// A left click: a file row selects it, and either column takes the focus.
    pub fn click(&mut self, x: u16, y: u16) {
        let at = Position::new(x, y);
        if self.list_area.contains(at) {
            self.focus = Focus::Files;
            let row = y.saturating_sub(self.list_area.y + 1) as usize + self.list_top();
            if y > self.list_area.y && row < self.files.len() {
                self.select(row);
            }
        } else if self.diff_area.contains(at) {
            self.focus = Focus::Diff;
        }
    }

    /// The first file shown, so the selection stays in view.
    fn list_top(&self) -> usize {
        let height = self.list_area.height.saturating_sub(2).max(1) as usize;
        (self.selected + 1).saturating_sub(height)
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let [hint_area, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
        let dim = Style::new().fg(Color::DarkGray);
        frame.render_widget(
            Paragraph::new(Span::styled(
                " diff vs HEAD · ↑↓/jk select · tab focus · pgup/pgdn scroll · r refresh · esc/q close",
                dim,
            )),
            hint_area,
        );
        if let Some(message) = self.message.as_deref().filter(|_| self.files.is_empty()) {
            self.list_area = Rect::default();
            self.diff_area = Rect::default();
            frame.render_widget(Paragraph::new(Span::styled(message, dim)), body);
            return;
        }
        let list_width = LIST_WIDTH.min(body.width / 2);
        let [list_area, diff_area] =
            Layout::horizontal([Constraint::Length(list_width), Constraint::Min(1)]).areas(body);
        self.list_area = list_area;
        self.diff_area = diff_area;
        let border = |focus| {
            Style::new().fg(if self.focus == focus {
                Color::Cyan
            } else {
                Color::DarkGray
            })
        };

        let top = self.list_top();
        let rows: Vec<Line> = self.files[top..]
            .iter()
            .enumerate()
            .map(|(i, file)| file_line(file, top + i == self.selected))
            .collect();
        let list = Block::bordered()
            .title(format!(" files ({}) ", self.files.len()))
            .border_style(border(Focus::Files));
        frame.render_widget(Paragraph::new(rows).block(list), list_area);

        let path = self.files.get(self.selected).map(|f| f.path.as_str());
        let lines: Vec<Line> = self
            .lines
            .iter()
            .skip(self.scroll)
            .take(diff_area.height as usize)
            .map(|(kind, text)| Line::styled(text.clone(), style(*kind)))
            .collect();
        let block = Block::bordered()
            .title(format!(" {} ", path.unwrap_or("")))
            .border_style(border(Focus::Diff));
        frame.render_widget(Paragraph::new(lines).block(block), diff_area);
    }
}

fn file_line(file: &FileChange, selected: bool) -> Line<'static> {
    let letter_style = match file.letter() {
        'N' | 'A' => Style::new().fg(Color::Green),
        'D' => Style::new().fg(Color::Red),
        _ => Style::new().fg(Color::Yellow),
    };
    let mut spans = vec![
        Span::styled(format!("{} ", file.letter()), letter_style),
        Span::raw(file.path.clone()),
    ];
    if let Some(added) = file.added {
        spans.push(Span::styled(
            format!(" +{added}"),
            Style::new().fg(Color::Green),
        ));
    }
    if let Some(removed) = file.removed.filter(|r| *r > 0) {
        spans.push(Span::styled(
            format!(" -{removed}"),
            Style::new().fg(Color::Red),
        ));
    }
    let line = Line::from(spans);
    if selected {
        line.style(Style::new().bg(Color::DarkGray))
    } else {
        line
    }
}

fn style(kind: LineKind) -> Style {
    match kind {
        LineKind::Header => Style::new().fg(Color::Yellow).bold(),
        LineKind::Hunk => Style::new().fg(Color::Cyan),
        LineKind::Added => Style::new().fg(Color::Green),
        LineKind::Removed => Style::new().fg(Color::Red),
        LineKind::Context => Style::new(),
    }
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").current_dir(root).args(args).output()?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The repository root and its changed files, with line counts.
fn load_files(dir: &Path) -> Result<(PathBuf, Vec<FileChange>)> {
    let Ok(top) = git(dir, &["rev-parse", "--show-toplevel"]) else {
        bail!("not a git repository");
    };
    let root = PathBuf::from(top.trim());
    let status = git(
        &root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    let numstat = git(&root, &["diff", "-M", "--numstat", "-z", base(&root)])?;
    let counts = parse_numstat(&numstat);
    let mut files = parse_status(&status);
    for file in &mut files {
        if file.untracked() {
            file.added = std::fs::read(root.join(&file.path))
                .ok()
                .filter(|bytes| !bytes.contains(&0))
                .map(|bytes| String::from_utf8_lossy(&bytes).lines().count() as u64);
        } else if let Some(&(added, removed)) = counts.get(&file.path) {
            file.added = added;
            file.removed = removed;
        }
    }
    Ok((root, files))
}

fn base(root: &Path) -> &'static str {
    match git(root, &["rev-parse", "--verify", "--quiet", "HEAD"]) {
        Ok(_) => "HEAD",
        Err(_) => EMPTY_TREE,
    }
}

/// The lines to show for `file`: its diff against HEAD, or an untracked file's content.
fn file_diff(root: &Path, file: &FileChange) -> Result<Vec<(LineKind, String)>> {
    if file.untracked() {
        let path = root.join(&file.path);
        if std::fs::metadata(&path)?.len() > MAX_UNTRACKED_BYTES {
            return Ok(vec![(
                LineKind::Header,
                "new file, too large to show".into(),
            )]);
        }
        return Ok(untracked_lines(&std::fs::read(path)?));
    }
    let mut args = vec!["diff", "-M", base(root), "--"];
    args.extend(file.from.as_deref());
    args.push(&file.path);
    Ok(classify(&git(root, &args)?))
}

/// Parses `git status --porcelain=v1 -z`. A rename or copy entry is followed by its
/// original path.
pub fn parse_status(text: &str) -> Vec<FileChange> {
    let mut fields = text.split('\0').filter(|f| !f.is_empty());
    let mut files = Vec::new();
    while let Some(field) = fields.next() {
        let (Some(status), Some(path)) = (field.get(..2), field.get(3..)) else {
            continue;
        };
        let from = if status.contains(['R', 'C']) {
            fields.next().map(str::to_string)
        } else {
            None
        };
        files.push(FileChange {
            path: path.to_string(),
            from,
            status: status.to_string(),
            added: None,
            removed: None,
        });
    }
    files
}

/// Parses `git diff --numstat -z` into counts by new path; binary files have none.
pub fn parse_numstat(text: &str) -> HashMap<String, (Option<u64>, Option<u64>)> {
    let mut fields = text.split('\0');
    let mut counts = HashMap::new();
    while let Some(field) = fields.next() {
        let mut parts = field.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        // A rename leaves the path empty and follows with the old and new paths.
        let path = if path.is_empty() {
            fields.next();
            fields.next().unwrap_or_default()
        } else {
            path
        };
        counts.insert(path.to_string(), (added.parse().ok(), removed.parse().ok()));
    }
    counts
}

/// Classifies each line of unified diff text. `+++`/`---` only count as headers
/// before a file's first hunk, so removed lines starting with `--` stay removed.
pub fn classify(text: &str) -> Vec<(LineKind, String)> {
    let mut in_header = false;
    text.lines()
        .map(|line| {
            if line.starts_with("diff ") {
                in_header = true;
            } else if line.starts_with("@@") {
                in_header = false;
                return (LineKind::Hunk, line.to_string());
            }
            let kind = if in_header {
                LineKind::Header
            } else if line.starts_with('+') {
                LineKind::Added
            } else if line.starts_with('-') {
                LineKind::Removed
            } else {
                LineKind::Context
            };
            (kind, line.to_string())
        })
        .collect()
}

/// An untracked file's content, every line added.
pub fn untracked_lines(bytes: &[u8]) -> Vec<(LineKind, String)> {
    if bytes.contains(&0) {
        return vec![(LineKind::Header, "new binary file".to_string())];
    }
    let mut lines = vec![(LineKind::Header, "new file".to_string())];
    lines.extend(
        String::from_utf8_lossy(bytes)
            .lines()
            .map(|line| (LineKind::Added, format!("+{line}"))),
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn change(status: &str, path: &str, from: Option<&str>) -> FileChange {
        FileChange {
            path: path.to_string(),
            from: from.map(str::to_string),
            status: status.to_string(),
            added: None,
            removed: None,
        }
    }

    #[test]
    fn status_lists_files_and_renames() {
        let text = " M src/a.rs\0R  new.rs\0old.rs\0?? notes/todo.txt\0A  b.rs\0";
        assert_eq!(
            parse_status(text),
            vec![
                change(" M", "src/a.rs", None),
                change("R ", "new.rs", Some("old.rs")),
                change("??", "notes/todo.txt", None),
                change("A ", "b.rs", None),
            ]
        );
        assert!(parse_status(text)[2].untracked());
        assert_eq!(parse_status(text)[2].letter(), 'N');
        assert_eq!(parse_status(text)[0].letter(), 'M');
    }

    #[test]
    fn numstat_reads_counts_renames_and_binaries() {
        let text = "3\t1\tsrc/a.rs\0-\t-\timg.png\x002\t0\t\0old.rs\0new.rs\0";
        let counts = parse_numstat(text);
        assert_eq!(counts["src/a.rs"], (Some(3), Some(1)));
        assert_eq!(counts["img.png"], (None, None));
        assert_eq!(counts["new.rs"], (Some(2), Some(0)));
        assert_eq!(counts.len(), 3);
    }

    #[test]
    fn diff_lines_are_classified() {
        let text = "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@ fn\n keep\n-old\n--- dashes\n+new\n+++ plus\n";
        let kinds: Vec<LineKind> = classify(text).into_iter().map(|(k, _)| k).collect();
        use LineKind::*;
        assert_eq!(
            kinds,
            [
                Header, Header, Header, Header, Hunk, Context, Removed, Removed, Added, Added
            ]
        );
    }

    #[test]
    fn untracked_content_is_all_added() {
        assert_eq!(
            untracked_lines(b"one\ntwo\n"),
            vec![
                (LineKind::Header, "new file".to_string()),
                (LineKind::Added, "+one".to_string()),
                (LineKind::Added, "+two".to_string()),
            ]
        );
        assert_eq!(untracked_lines(b"a\0b").len(), 1);
    }

    fn view() -> DiffView {
        let mut first = change(" M", "src/a.rs", None);
        first.added = Some(1);
        first.removed = Some(1);
        let mut second = change("??", "new.txt", None);
        second.added = Some(2);
        DiffView {
            root: PathBuf::new(),
            message: None,
            files: vec![first, second],
            selected: 0,
            lines: classify("@@ -1 +1 @@\n-old\n+new\n"),
            scroll: 0,
            focus: Focus::Files,
            list_area: Rect::default(),
            diff_area: Rect::default(),
        }
    }

    #[test]
    fn renders_the_file_list_and_colored_hunks() {
        let mut view = view();
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y: u16| -> String {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        };
        assert!(row(2).contains("M src/a.rs +1 -1"), "{:?}", row(2));
        assert!(row(3).contains("N new.txt +2"), "{:?}", row(3));
        assert!(row(1).contains("src/a.rs"), "{:?}", row(1));
        assert!(row(3).contains("-old"), "{:?}", row(3));
        // The diff column starts at 30, its text one cell in.
        assert_eq!(buffer[(31, 4)].symbol(), "+");
        assert_eq!(buffer[(31, 4)].fg, Color::Green);
        assert_eq!(buffer[(31, 3)].fg, Color::Red);
        assert_eq!(buffer[(31, 2)].fg, Color::Cyan);

        // Clicking the second file row selects it.
        view.click(2, 3);
        assert_eq!(view.selected, 1);
        assert_eq!(view.focus, Focus::Files);
        view.click(40, 4);
        assert_eq!(view.focus, Focus::Diff);
        assert!(!view.on_key(KeyCode::Char('q')));
    }

    #[test]
    fn outside_a_repository_shows_a_message() {
        let mut view = view();
        view.files.clear();
        view.message = Some("not a git repository".to_string());
        let mut terminal = Terminal::new(TestBackend::new(40, 4)).unwrap();
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row: String = (0..40).map(|x| buffer[(x, 1)].symbol()).collect();
        assert_eq!(row.trim_end(), "not a git repository");
    }
}
