//! Output too long to hand the model whole: saved in full to a file in the session's
//! scratch directory, and summarised in the result as where it is, the lines that look
//! like failures, and its first and last lines.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::{self, BufRead, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Output since the last result past this many bytes is summarised instead of shown.
pub(super) const SPILL_AT: u64 = 6 * 1024;
/// Bytes one file keeps. What comes after is counted, and its last lines still shown.
const FILE_CAP: u64 = 8 << 20;
/// The directory under the session's scratch directory the files go in.
pub(super) const OUTPUT: &str = "output";
/// Output files kept in a session's directory; opening another removes the oldest.
const FILES: usize = 32;
/// Bytes of the latest output held in memory, which the last lines come from.
const RING: usize = 4096;
/// The first lines shown, by count and by bytes.
const HEAD_LINES: usize = 5;
const HEAD_BYTES: usize = 500;
/// The last lines shown.
const TAIL_LINES: usize = 20;
const TAIL_BYTES: usize = 1500;
/// The lines that look like failures shown, from the first.
const FAILURE_LINES: usize = 10;
const FAILURE_BYTES: usize = 1200;
/// Characters of one line shown.
const LINE_CHARS: usize = 200;

/// One command's output as it is saved: held in memory until it passes [`SPILL_AT`],
/// then written to a file, which every later byte goes to up to [`FILE_CAP`].
pub(super) struct Spill {
    dir: PathBuf,
    buf: Vec<u8>,
    file: Option<(PathBuf, std::fs::File)>,
    /// Bytes and newlines seen, those past the cap included.
    total: u64,
    newlines: u64,
    /// Bytes in the file, or in `buf` before it opens.
    kept: u64,
    ring: VecDeque<u8>,
    /// `total` and `newlines` when the last result was taken.
    mark: (u64, u64),
    /// Saving failed or was withdrawn; results stay inline.
    off: bool,
}

impl Spill {
    /// `dir` is created when the first file is, and only if its parent exists: a session
    /// directory private mode erased is not brought back.
    pub(super) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            buf: Vec::new(),
            file: None,
            total: 0,
            newlines: 0,
            kept: 0,
            ring: VecDeque::new(),
            mark: (0, 0),
            off: false,
        }
    }

    /// Add output, already redacted.
    pub(super) fn push(&mut self, bytes: &[u8]) {
        if self.off || bytes.is_empty() {
            return;
        }
        let len = bytes.len() as u64;
        self.total += len;
        self.newlines += bytes.iter().filter(|&&b| b == b'\n').count() as u64;
        self.ring.extend(bytes);
        if self.ring.len() > RING {
            let over = self.ring.len() - RING;
            self.ring.drain(..over);
        }
        let room = FILE_CAP.saturating_sub(self.kept).min(len) as usize;
        if room > 0 {
            let part = &bytes[..room];
            match &mut self.file {
                Some((_, file)) => {
                    if file.write_all(part).is_err() {
                        self.stop();
                        return;
                    }
                }
                None => self.buf.extend_from_slice(part),
            }
            self.kept += room as u64;
        }
        if self.file.is_none() && self.total > SPILL_AT {
            self.open();
        }
    }

    fn open(&mut self) {
        let opened = create(&self.dir).and_then(|(path, mut file)| {
            if let Err(e) = file.write_all(&self.buf) {
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
            Ok((path, file))
        });
        match opened {
            Ok(file) => {
                self.buf = Vec::new();
                self.file = Some(file);
            }
            Err(_) => self.stop(),
        }
    }

    fn stop(&mut self) {
        self.off = true;
        self.buf = Vec::new();
        self.file = None;
    }

    /// The output since the last take, when it is long enough to be summarised and is
    /// saved. `private` removes the file and keeps every later result inline.
    pub(super) fn take(&mut self, private: bool) -> Option<Portion> {
        let (start, before) = std::mem::replace(&mut self.mark, (self.total, self.newlines));
        if private {
            if let Some((path, _)) = self.file.take() {
                let _ = std::fs::remove_file(path);
            }
            self.stop();
            return None;
        }
        if self.off || self.total - start <= SPILL_AT {
            return None;
        }
        let (path, _) = self.file.as_ref()?;
        let (front, back) = self.ring.as_slices();
        Some(Portion {
            path: path.clone(),
            start,
            end: self.total,
            kept: self.kept,
            first: before + 1,
            newlines: self.newlines,
            ring: [front, back].concat(),
        })
    }
}

/// A new output file in `dir`, readable only by the user, the oldest ones past
/// [`FILES`] removed.
fn create(dir: &Path) -> io::Result<(PathBuf, std::fs::File)> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    prune(dir, FILES - 1);
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let path = dir.join(format!(
        "bash-{}-{}-{}.log",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed) + 1
    ));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    Ok((path, file))
}

/// Remove the oldest output files in `dir` until `keep` are left.
fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("bash-") && name.ends_with(".log")
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort();
    for (_, path) in &files[..files.len() - keep] {
        let _ = std::fs::remove_file(path);
    }
}

/// Lines with their numbers in the file.
type Numbered = Vec<(u64, String)>;

/// Output too long to show, as saved: bytes `start..end` of everything the command
/// printed, of which the file holds the first `kept`.
pub(super) struct Portion {
    path: PathBuf,
    start: u64,
    end: u64,
    kept: u64,
    /// The file's line the portion starts on, from 1.
    first: u64,
    /// Newlines up to `end`.
    newlines: u64,
    /// The last [`RING`] bytes up to `end`.
    ring: Vec<u8>,
}

impl Portion {
    /// The result's body: where the output is, the lines that look like failures, then
    /// the first and last lines, each numbered as a line of the file.
    pub(super) fn render(&self) -> String {
        let last = self.newlines + u64::from(self.ring.last().is_some_and(|&b| b != b'\n'));
        let tail = self.tail(last);
        let tail_from = tail.first().map_or(last + 1, |(n, _)| *n);
        let (head, failures, failed) = self.scan(tail_from);
        let name = self.path.file_name().unwrap_or_default().to_string_lossy();
        let mut out = format!(
            "The output was {} bytes over {} lines, too long to show here. All of it is \
saved in {}",
            self.end - self.start,
            last + 1 - self.first.min(last + 1),
            self.path.display(),
        );
        match self.first {
            1 => out.push('.'),
            first => {
                let _ = write!(
                    out,
                    " as lines {first} to {last}; the lines before them are what the command \
printed earlier."
                );
            }
        }
        if self.end > self.kept {
            let _ = write!(
                out,
                " The file stops at {} bytes, so the last {} bytes are not in it; the last \
lines below are.",
                self.kept,
                self.end - self.kept.max(self.start)
            );
        }
        // The variable is set for the session that saved it, and is shorter than the path.
        let _ = writeln!(
            out,
            " In bash it is $BHAI_SCRATCH_DIR/{OUTPUT}/{name}: `sed -n '{a},{b}p'` prints a \
range of its lines and `rg -n PATTERN` searches it. The read tool reads it by line range.",
            a = self.first,
            b = (self.first + 39).min(last),
        );
        if failed > 0 {
            let _ = write!(
                out,
                "\nLines that look like failures, outside those shown below, {} of {failed}:\n",
                failures.len()
            );
            push_lines(&mut out, &failures);
        }
        if !head.is_empty() {
            out.push_str("\nFirst lines:\n");
            push_lines(&mut out, &head);
        }
        if !tail.is_empty() {
            out.push_str("\nLast lines:\n");
            push_lines(&mut out, &tail);
        }
        out.pop();
        out
    }

    /// The last lines, from memory, numbered back from `last`.
    fn tail(&self, last: u64) -> Numbered {
        let text = String::from_utf8_lossy(&self.ring);
        let text = text.strip_suffix('\n').unwrap_or(&text);
        let mut lines: Vec<&str> = text.split('\n').collect();
        // The ring starts partway through a line, unless it holds the whole portion.
        if self.end - self.start > self.ring.len() as u64 && lines.len() > 1 {
            lines.remove(0);
        }
        let mut picked = Vec::new();
        let mut bytes = 0;
        for (n, line) in (0..=last).rev().zip(lines.into_iter().rev()) {
            let line = clip(line);
            bytes += line.len();
            if !picked.is_empty() && (picked.len() == TAIL_LINES || bytes > TAIL_BYTES) {
                break;
            }
            picked.push((n, line));
        }
        picked.reverse();
        picked
    }

    /// The first lines, and the lines between them and `tail_from` that look like
    /// failures, read from the file, with how many such lines there were.
    fn scan(&self, tail_from: u64) -> (Numbered, Numbered, usize) {
        let (mut head, mut failures, mut failed) = (Vec::new(), Vec::new(), 0);
        if self.start >= self.kept {
            return (head, failures, failed);
        }
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return (head, failures, failed);
        };
        if file.seek(io::SeekFrom::Start(self.start)).is_err() {
            return (head, failures, failed);
        }
        let mut reader = io::BufReader::new(io::Read::take(file, self.kept - self.start));
        let (mut line, mut n) = (Vec::new(), self.first);
        let (mut head_bytes, mut failure_bytes) = (0, 0);
        let mut heading = true;
        while n < tail_from {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let text = String::from_utf8_lossy(&line);
            let text = text.trim_end_matches(['\n', '\r']);
            if heading {
                let clipped = clip(text);
                head_bytes += clipped.len();
                if head.is_empty() || (head.len() < HEAD_LINES && head_bytes <= HEAD_BYTES) {
                    head.push((n, clipped));
                    n += 1;
                    continue;
                }
                heading = false;
            }
            if looks_failed(text) {
                failed += 1;
                let clipped = clip(text);
                failure_bytes += clipped.len();
                if failures.len() < FAILURE_LINES && failure_bytes <= FAILURE_BYTES {
                    failures.push((n, clipped));
                }
            }
            n += 1;
        }
        (head, failures, failed)
    }
}

fn push_lines(out: &mut String, lines: &[(u64, String)]) {
    for (n, line) in lines {
        let _ = writeln!(out, "{n}: {line}");
    }
}

/// `line` cut to [`LINE_CHARS`] characters.
fn clip(line: &str) -> String {
    match line.char_indices().nth(LINE_CHARS) {
        None => line.to_string(),
        Some((at, _)) => format!(
            "{} [{} more characters]",
            &line[..at],
            line[at..].chars().count()
        ),
    }
}

/// Whether `line` reads as reporting a failure: a word such as `error`, `FAILED`,
/// `panicked` or `Traceback`, or one ending `Error` or `Exception`, that a `0` or `no`
/// does not come right before. Words keep their `-` and `_`, so `error-chain` is a name.
pub(super) fn looks_failed(line: &str) -> bool {
    let mut previous = String::new();
    let words = line
        .split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
        .filter(|w| !w.is_empty());
    for word in words {
        let lower = word.to_lowercase();
        let hit = matches!(
            lower.as_str(),
            "error"
                | "errors"
                | "fail"
                | "failed"
                | "failure"
                | "failures"
                | "fatal"
                | "panic"
                | "panicked"
                | "traceback"
                | "exception"
        ) || (word.len() > 5
            && word.starts_with(char::is_uppercase)
            && (lower.ends_with("error") || lower.ends_with("exception")));
        if hit && !matches!(previous.as_str(), "0" | "no") {
            return true;
        }
        previous = lower;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        super::super::super::temp_dir().join("output")
    }

    fn log(lines: std::ops::RangeInclusive<u32>, failing: &[u32]) -> String {
        lines
            .map(|n| match failing.contains(&n) {
                true => format!("error[E0425]: cannot find value `x{n}` in this scope\n"),
                false => format!("   Compiling crate-{n} v0.1.0 (/some/fairly/long/path)\n"),
            })
            .collect()
    }

    #[test]
    fn short_output_is_not_saved() {
        let dir = dir();
        let mut spill = Spill::new(dir.clone());
        spill.push(b"a few lines\nof output\n");
        assert!(spill.take(false).is_none());
        assert!(!dir.exists());
    }

    #[test]
    fn long_output_is_saved_whole_and_summarised_with_its_failures() {
        let dir = dir();
        let mut spill = Spill::new(dir.clone());
        let text = log(1..=2000, &[700, 1312]);
        for chunk in text.as_bytes().chunks(8192) {
            spill.push(chunk);
        }
        let portion = spill.take(false).unwrap();
        assert_eq!(std::fs::read_to_string(&portion.path).unwrap(), text);
        let result = portion.render();
        let path = portion.path.display().to_string();
        let name = portion
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(
            result.starts_with(&format!(
                "The output was {} bytes over 2000 lines, too long to show here. All of it \
is saved in {path}.",
                text.len()
            )),
            "{result}"
        );
        assert!(
            result.contains(&format!(
                "In bash it is $BHAI_SCRATCH_DIR/output/{name}: `sed -n '1,40p'`"
            )),
            "{result}"
        );
        assert!(
            result.contains(
                "\nLines that look like failures, outside those shown below, 2 of 2:\n\
700: error[E0425]: cannot find value `x700` in this scope\n\
1312: error[E0425]: cannot find value `x1312` in this scope\n"
            ),
            "{result}"
        );
        assert!(
            result.contains("\nFirst lines:\n1:    Compiling crate-1 v0.1.0"),
            "{result}"
        );
        assert!(
            result.ends_with("2000:    Compiling crate-2000 v0.1.0 (/some/fairly/long/path)"),
            "{result}"
        );
        assert!(result.contains("\nLast lines:\n1981: "), "{result}");
        assert!(result.len() < 4096, "{}", result.len());
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_later_portion_is_numbered_as_lines_of_the_file() {
        let dir = dir();
        let mut spill = Spill::new(dir.clone());
        spill.push(log(1..=10, &[]).as_bytes());
        assert!(spill.take(false).is_none());
        spill.push(log(11..=400, &[399]).as_bytes());
        let result = spill.take(false).unwrap().render();
        assert!(result.contains(" as lines 11 to 400; "), "{result}");
        assert!(result.contains("`sed -n '11,50p'`"), "{result}");
        assert!(result.contains("\nFirst lines:\n11: "), "{result}");
        assert!(result.contains("399: error[E0425]"), "{result}");
        assert!(!result.contains("look like failures"), "{result}");
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn caps_hold() {
        let dir = dir();
        let mut spill = Spill::new(dir.clone());
        let line = format!("{}\n", "error ".repeat(1000));
        let chunk = line.repeat(64);
        let mut pushed = 0u64;
        while pushed <= FILE_CAP + 100_000 {
            spill.push(chunk.as_bytes());
            pushed += chunk.len() as u64;
        }
        let portion = spill.take(false).unwrap();
        assert_eq!(std::fs::metadata(&portion.path).unwrap().len(), FILE_CAP);
        let result = portion.render();
        assert!(result.contains(&format!("The file stops at {FILE_CAP} bytes")));
        assert!(result.len() < 4096, "{}", result.len());
        assert!(result.contains("[5800 more characters]"), "{result}");
        for _ in 0..FILES + 3 {
            Spill::new(dir.clone()).push(chunk.as_bytes());
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), FILES);
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn private_mode_removes_the_file_and_stays_inline() {
        let dir = dir();
        let mut spill = Spill::new(dir.clone());
        spill.push(log(1..=400, &[]).as_bytes());
        let path = spill.file.as_ref().unwrap().0.clone();
        assert!(spill.take(true).is_none());
        assert!(!path.exists());
        spill.push(log(401..=800, &[]).as_bytes());
        assert!(spill.take(false).is_none());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn an_erased_session_directory_is_not_recreated() {
        let parent = super::super::super::temp_dir();
        let dir = parent.join("output");
        std::fs::remove_dir_all(&parent).unwrap();
        let mut spill = Spill::new(dir);
        spill.push(log(1..=400, &[]).as_bytes());
        assert!(spill.take(false).is_none());
        assert!(!parent.exists());
    }

    #[test]
    fn failure_lines_are_told_apart_conservatively() {
        for line in [
            "error[E0425]: cannot find value `x` in this scope",
            "test result: FAILED. 3 passed; 1 failed; 0 ignored",
            "thread 'main' panicked at src/main.rs:2:5:",
            "Traceback (most recent call last):",
            "TypeError: undefined is not a function",
            "FAIL src/app.test.ts",
            "[ERROR] Failed to execute goal",
            "java.lang.NullPointerException",
        ] {
            assert!(looks_failed(line), "{line}");
        }
        for line in [
            "test result: ok. 12 passed; 0 failed; 0 ignored",
            "   Compiling error-chain v0.12.4",
            "Finished with no errors",
            "warning: unused variable: `x`",
        ] {
            assert!(!looks_failed(line), "{line}");
        }
    }
}
