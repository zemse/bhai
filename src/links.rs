//! OSC 8 hyperlinks over the transcript, so the terminal makes a URL or a file path on
//! screen something to click. They go on after the frame is drawn, cell by cell, since
//! ratatui measures a cell's symbol to place the next and counts an escape as width.

use std::num::NonZeroU16;
use std::ops::Range;
use std::path::Path;

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::{Position, Rect};

use crate::wrap::Join;

/// A word longer than this is no link, so a megabyte of minified output on one line is
/// not scanned whole on every frame.
const MAX_WORD: usize = 4096;

/// A run of text the terminal should open, in chars of the text it was found in.
#[derive(Debug, PartialEq, Eq)]
pub struct Link {
    pub chars: Range<usize>,
    pub target: String,
}

/// A link as the frame drew it: its target and, row by row, the screen columns it
/// covers. A link wrapped over rows has a span on each.
#[derive(Debug, PartialEq, Eq)]
pub struct Shown {
    pub target: String,
    pub spans: Vec<(u16, Range<u16>)>,
}

impl Shown {
    pub fn contains(&self, at: Position) -> bool {
        self.spans
            .iter()
            .any(|(y, xs)| *y == at.y && xs.contains(&at.x))
    }
}

/// Whether a click may open `target`: only a web page, so text in the transcript can
/// never have a click run a `file:` URL or another scheme's handler.
pub fn openable(target: &str) -> bool {
    let lower = target.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://"))
        && !target.chars().any(char::is_control)
}

/// Open `target` with `launch`, refusing anything [`openable`] does not pass.
pub fn open(target: &str, launch: fn(&str) -> std::io::Result<()>) -> Result<(), String> {
    if !openable(target) {
        return Err(format!(
            "not opening {target}: only http and https links open"
        ));
    }
    launch(target).map_err(|err| format!("opening {target}: {err}"))
}

/// The default browser on `url`: `open` on macOS, `xdg-open` elsewhere, with the URL as
/// an argument and no shell. Nothing waits for it but a thread that reaps it.
pub fn launch(url: &str) -> std::io::Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut command = std::process::Command::new(program);
    command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Its own group, so a ctrl+c meant for bhai does not reach it.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Where to look for the files a path names.
pub struct Places<'a> {
    pub root: &'a Path,
    pub home: Option<&'a Path>,
    /// Off over ssh: a `file://` link opens on the machine the terminal runs on, which
    /// does not have the file.
    pub files: bool,
}

/// The URLs in `text`, and the words that name a file or directory that exists.
pub fn find(text: &str, places: &Places) -> Vec<Link> {
    let chars: Vec<char> = text.chars().collect();
    let mut links = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        if chars[at].is_whitespace() {
            at += 1;
            continue;
        }
        let start = at;
        while at < chars.len() && !chars[at].is_whitespace() {
            at += 1;
        }
        if at - start > MAX_WORD {
            continue;
        }
        if let Some(link) = word(&chars, start..at, places) {
            links.push(link);
        }
    }
    links
}

fn word(chars: &[char], range: Range<usize>, places: &Places) -> Option<Link> {
    let word: String = chars[range.clone()].iter().collect();
    // `url=https://…` and `(https://…` link from the scheme on.
    let scheme = ["https://", "http://"]
        .iter()
        .filter_map(|scheme| word.find(scheme))
        .min();
    let lead = match scheme {
        Some(byte) => word[..byte].chars().count(),
        None => word
            .chars()
            .take_while(|c| matches!(c, '(' | '[' | '{' | '<' | '"' | '\'' | '`' | '*'))
            .count(),
    };
    let chars = &chars[range.start + lead..range.end];
    let kept = trimmed(chars);
    let text: String = chars[..kept].iter().collect();
    let target = match scheme {
        Some(_) => url(&text)?,
        None => path(&text, places)?,
    };
    let start = range.start + lead;
    Some(Link {
        chars: start..start + kept,
        target,
    })
}

/// How many of `chars` are left once the punctuation a sentence puts after a word is
/// off, and a closing bracket the word did not open.
fn trimmed(chars: &[char]) -> usize {
    let mut end = chars.len();
    while end > 0 {
        let open = |c: char| chars[..end].iter().filter(|x| **x == c).count();
        let drop = match chars[end - 1] {
            '.' | ',' | ';' | ':' | '!' | '?' | '"' | '\'' | '`' | '*' | '>' => true,
            ')' => open('(') < open(')'),
            ']' => open('[') < open(']'),
            '}' => open('{') < open('}'),
            _ => false,
        };
        if !drop {
            break;
        }
        end -= 1;
    }
    end
}

fn url(text: &str) -> Option<String> {
    let (_, rest) = text.split_once("://")?;
    if rest.is_empty() {
        return None;
    }
    // A control character would end the escape early and print the rest of it.
    Some(text.chars().filter(|c| !c.is_control()).collect())
}

/// A `file://` URL for `text` when it names something on disk. A `:line` or
/// `:line:column` after it is part of the link and not of the path.
fn path(text: &str, places: &Places) -> Option<String> {
    if !places.files || text.contains("://") {
        return None;
    }
    let mut name = text;
    for _ in 0..2 {
        match name.rsplit_once(':') {
            Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => {
                name = head
            }
            _ => break,
        }
    }
    if !shaped(name) {
        return None;
    }
    let path = match name.strip_prefix("~/") {
        Some(rest) => places.home?.join(rest),
        None => places.root.join(name),
    };
    let path = std::fs::canonicalize(path).ok()?;
    Some(file_url(&path))
}

/// Whether a word reads as a path at all, so prose is not looked up word by word: it
/// has a `/` in it, or ends in a short extension.
fn shaped(name: &str) -> bool {
    if !name.chars().any(char::is_alphanumeric) {
        return false;
    }
    if name.contains('/') {
        return true;
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && (1..=8).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// No host, which a terminal takes as its own machine.
fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'.' | b'_' | b'~' => {
                url.push(byte as char)
            }
            _ => url.push_str(&format!("%{byte:02X}")),
        }
    }
    url
}

/// The cell's symbol inside an OSC 8 link. The id makes the cells of one link, wrapped
/// over several rows, light up together.
fn wrapped(id: &str, target: &str, symbol: &str) -> String {
    format!("\x1b]8;id={id};{target}\x1b\\{symbol}\x1b]8;;\x1b\\")
}

/// Turn the links in the transcript rows `view` into OSC 8 links in `buf`, where `area`
/// shows them from its top row. A word split over rows is one link, and a cell whose
/// symbol is not the char the row has there, an overlay drawn over it, is left alone.
/// Returns each link with the cells it took.
pub fn stamp(
    buf: &mut Buffer,
    area: Rect,
    view: Range<usize>,
    lines: &[String],
    joins: &[Join],
    margins: &[usize],
    places: &Places,
) -> Vec<Shown> {
    let mut shown = Vec::new();
    let split = |line: usize| joins.get(line) == Some(&Join::Split);
    // Rows past these on either side of the view can only hold part of a word too long
    // to be a link.
    let reach = MAX_WORD / area.width.max(1) as usize + 1;
    let first = view.start.saturating_sub(reach);
    let last = view.end + reach;
    let mut line = view.start;
    // The first row in view may carry on a word from the rows above it.
    while line > first && split(line) {
        line -= 1;
    }
    while line < view.end.min(lines.len()) {
        let mut end = line + 1;
        while end < lines.len().min(last) && split(end) {
            end += 1;
        }
        // The run's text less its margins, and the row and column of each char of it.
        let mut text = String::new();
        let mut cells: Vec<(usize, u16, char)> = Vec::new();
        for (row, line) in lines.iter().enumerate().take(end).skip(line) {
            let margin = margins.get(row).copied().unwrap_or(0);
            let mut column = 0u16;
            for (index, c) in line.chars().enumerate() {
                if index >= margin {
                    text.push(c);
                    cells.push((row, column, c));
                }
                let width = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                column = column.saturating_add(width as u16);
            }
        }
        for link in find(&text, places) {
            let id = format!("bhai-{line}-{}", link.chars.start);
            let mut spans: Vec<(u16, Range<u16>)> = Vec::new();
            for &(row, column, c) in &cells[link.chars] {
                if !view.contains(&row) || column >= area.width {
                    continue;
                }
                let y = area.y + (row - view.start) as u16;
                if y >= area.bottom() {
                    continue;
                }
                let Some(cell) = buf.cell_mut((area.x + column, y)) else {
                    continue;
                };
                let mut own = [0; 4];
                if cell.symbol() != c.encode_utf8(&mut own) {
                    continue;
                }
                let width = unicode_width::UnicodeWidthChar::width(c)
                    .unwrap_or(1)
                    .max(1);
                let symbol = wrapped(&id, &link.target, cell.symbol());
                cell.set_symbol(&symbol)
                    .set_diff_option(CellDiffOption::ForcedWidth(
                        NonZeroU16::new(width as u16).expect("at least one"),
                    ));
                let x = area.x + column;
                let cells = x..x.saturating_add(width as u16);
                match spans.last_mut() {
                    Some((at, xs)) if *at == y && xs.end == x => xs.end = cells.end,
                    _ => spans.push((y, cells)),
                }
            }
            if !spans.is_empty() {
                shown.push(Shown {
                    target: link.target,
                    spans,
                });
            }
        }
        line = end;
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> Places<'static> {
        Places {
            root: Path::new("/nonexistent-bhai-root"),
            home: None,
            files: false,
        }
    }

    fn targets(text: &str, places: &Places) -> Vec<(String, String)> {
        find(text, places)
            .into_iter()
            .map(|link| {
                let shown: String = text
                    .chars()
                    .skip(link.chars.start)
                    .take(link.chars.len())
                    .collect();
                (shown, link.target)
            })
            .collect()
    }

    #[test]
    fn a_url_loses_the_punctuation_around_it() {
        let text = "see (https://example.com/a_(b)), or https://x.io/q?a=1. And url=http://h/p";
        assert_eq!(
            targets(text, &none()),
            [
                ("https://example.com/a_(b)", "https://example.com/a_(b)"),
                ("https://x.io/q?a=1", "https://x.io/q?a=1"),
                ("http://h/p", "http://h/p"),
            ]
            .map(|(a, b)| (a.to_string(), b.to_string()))
        );
    }

    #[test]
    fn a_word_too_long_to_be_a_link_is_passed_over() {
        let long = format!("https://a.io/{}", ")".repeat(MAX_WORD));
        assert!(find(&format!("{long} https://b.io"), &none()).len() == 1);
    }

    #[test]
    fn a_scheme_with_nothing_after_it_is_no_link() {
        assert!(find("https:// and http://", &none()).is_empty());
    }

    #[test]
    fn a_path_links_only_when_it_exists() {
        let root = std::env::temp_dir().join(format!("bhai-links-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a b.rs"), "").unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let places = Places {
            root: &root,
            home: None,
            files: true,
        };
        let found = targets("`Cargo.toml`, src/missing.rs and src: done.", &places);
        let base = file_url(&root);
        assert_eq!(
            found,
            [("Cargo.toml".to_string(), format!("{base}/Cargo.toml"))]
        );
        // A line and column stay on the link and off the path.
        let found = targets("at src/lib.rs:12:4", &places);
        assert_eq!(
            found,
            [("src/lib.rs:12:4".to_string(), format!("{base}/src/lib.rs"))]
        );
        assert_eq!(
            file_url(&root.join("src/a b.rs")),
            format!("{base}/src/a%20b.rs")
        );
        // Over ssh nothing on disk links.
        let places = Places {
            files: false,
            ..places
        };
        assert!(find("Cargo.toml src", &places).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_control_character_never_reaches_the_escape() {
        let link = &find("https://a.io/\u{7}x", &none())[0];
        assert_eq!(link.target, "https://a.io/x");
    }

    #[test]
    fn a_url_split_over_rows_is_one_link_in_each_of_its_cells() {
        let lines = vec![
            "> go to https://exa".to_string(),
            "  mple.com now".to_string(),
        ];
        let joins = [Join::Newline, Join::Split];
        let margins = [2, 2];
        let area = Rect::new(0, 0, 20, 2);
        let mut buf = Buffer::empty(area);
        for (y, line) in lines.iter().enumerate() {
            buf.set_string(0, y as u16, line, ratatui::style::Style::new());
        }
        // An overlay over the last cell of the link.
        buf[(5, 1)].set_symbol("#");
        let shown = stamp(&mut buf, area, 0..2, &lines, &joins, &margins, &none());
        // One link, a span on each row, broken where the overlay sits.
        assert_eq!(
            shown,
            [Shown {
                target: "https://example.com".to_string(),
                spans: vec![(0, 8..19), (1, 2..5), (1, 6..10)],
            }]
        );
        assert!(shown[0].contains(Position::new(3, 1)));
        assert!(!shown[0].contains(Position::new(5, 1)));
        assert!(!shown[0].contains(Position::new(7, 0)));
        let open = "\x1b]8;id=bhai-0-6;https://example.com\x1b\\";
        assert_eq!(buf[(8, 0)].symbol(), format!("{open}h\x1b]8;;\x1b\\"));
        assert_eq!(buf[(4, 1)].symbol(), format!("{open}l\x1b]8;;\x1b\\"));
        assert_eq!(buf[(5, 1)].symbol(), "#");
        assert_eq!(buf[(7, 0)].symbol(), " ");
        assert_eq!(buf[(11, 1)].symbol(), "n");
        // A view that starts on the second row still finds where the word began.
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
        buf.set_string(0, 0, &lines[1], ratatui::style::Style::new());
        stamp(
            &mut buf,
            Rect::new(0, 0, 20, 1),
            1..2,
            &lines,
            &joins,
            &margins,
            &none(),
        );
        assert_eq!(buf[(2, 0)].symbol(), format!("{open}m\x1b]8;;\x1b\\"));
    }

    fn launched(_: &str) -> std::io::Result<()> {
        Ok(())
    }

    #[test]
    fn only_a_web_link_opens() {
        assert!(open("https://example.com/a", launched).is_ok());
        assert!(open("HTTP://example.com", launched).is_ok());
        for target in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "x-man-page://ls",
            "https://a.io/\u{7}",
        ] {
            assert!(open(target, launched).is_err(), "{target}");
        }
        let failed = open("https://a.io", |_| Err(std::io::Error::other("no browser")));
        assert_eq!(failed, Err("opening https://a.io: no browser".to_string()));
    }

    #[test]
    fn a_wide_char_keeps_its_width() {
        let lines = vec!["https://例.jp".to_string()];
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, &lines[0], ratatui::style::Style::new());
        stamp(
            &mut buf,
            area,
            0..1,
            &lines,
            &[Join::Newline],
            &[0],
            &none(),
        );
        assert!(buf[(8, 0)].symbol().contains('例'));
        assert_eq!(
            buf[(8, 0)].diff_option,
            CellDiffOption::ForcedWidth(NonZeroU16::new(2).unwrap())
        );
        assert!(buf[(10, 0)].symbol().contains('.'));
    }
}
