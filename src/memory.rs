//! `.bhai/MEMORY.md`: notes the main agent saves with `remember`, one per line, read into
//! the system prompt once when a session starts. A note saved mid-session reaches the rest
//! of it through `notes`, so the cached prefix never moves.
//!
//! Each session also keeps the facts its summaries proposed for the file, in
//! `memory-candidates.md` in its state directory, for the idle review in `consolidate`
//! to promote.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// The file, in the project's `.bhai` directory.
pub const FILE: &str = "MEMORY.md";

/// The most of the file the prompt carries, the newest lines kept.
pub const MAX_LOAD: usize = 4 * 1024;

/// The most one note may be.
pub const MAX_NOTE: usize = 1024;

/// The candidates file, in a session's state directory.
pub const CANDIDATES: &str = "memory-candidates.md";

/// The most candidates a session keeps, the newest kept.
const MAX_CANDIDATES: usize = 40;

/// Where the memory file of the `.bhai` directory `bhai` is.
pub fn path(bhai: &Path) -> PathBuf {
    bhai.join(FILE)
}

/// What the prompt carries of the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    /// The path as shown to the user.
    pub label: String,
    /// The newest whole lines within `MAX_LOAD`.
    pub content: String,
    /// Bytes of older lines left out.
    pub cut: usize,
}

/// Whether `path`, or the directory it would be created in, really lives in the project:
/// the directory holding `.bhai`. A cloned repo can commit the file, or `.bhai`, as a
/// symlink to `~/.aws/credentials`; a dangling link counts as outside.
fn inside(path: &Path) -> bool {
    let Some(bhai) = path.parent() else {
        return false;
    };
    let Some(Ok(root)) = bhai.parent().map(Path::canonicalize) else {
        return false;
    };
    let real = match path.symlink_metadata() {
        Ok(_) => path.canonicalize(),
        Err(_) => bhai.canonicalize(),
    };
    real.is_ok_and(|real| real.starts_with(root))
}

/// The file's last `MAX_LOAD` bytes of whole lines; `None` when it is missing, empty or
/// not text, and the startup notice when it resolves outside the project.
pub fn load(path: &Path, label: String) -> Result<Option<Memory>, String> {
    if path.symlink_metadata().is_err() {
        return Ok(None);
    }
    if !inside(path) {
        return Err(format!("skipped {label} (outside project)"));
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let text = text.trim_end();
    if text.trim().is_empty() {
        return Ok(None);
    }
    let mut start = 0;
    if text.len() > MAX_LOAD {
        let mut from = text.len() - MAX_LOAD;
        while !text.is_char_boundary(from) {
            from += 1;
        }
        // From the first line that starts inside the budget; a last line longer than the
        // budget leaves nothing, which is said rather than cut mid-line.
        start = match text.as_bytes()[from - 1] {
            b'\n' => from,
            _ => text[from..]
                .find('\n')
                .map_or(text.len(), |at| from + at + 1),
        };
    }
    Ok(Some(Memory {
        label,
        content: text[start..].to_string(),
        cut: start,
    }))
}

/// A note as the line it is saved as: dated, and on one line however it was written.
pub fn entry(note: &str, date: &str) -> Result<String, String> {
    let note = note.split_whitespace().collect::<Vec<_>>().join(" ");
    if note.is_empty() {
        return Err("`note` is empty.".to_string());
    }
    if note.len() > MAX_NOTE {
        return Err(format!(
            "`note` is {} bytes; keep it under {MAX_NOTE}. Save the fact, not the story.",
            note.len()
        ));
    }
    Ok(format!("- {date}: {note}"))
}

/// Append `line` to the file, creating it and its directory, on a line of its own.
pub fn append(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if !inside(path) {
        return Err(std::io::Error::other("it resolves outside the project"));
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)?;
    let mut last = [0u8; 1];
    let len = file.metadata()?.len();
    let mut text = String::new();
    if len > 0 {
        file.seek(SeekFrom::End(-1))?;
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            text.push('\n');
        }
    }
    text.push_str(line);
    text.push('\n');
    file.write_all(text.as_bytes())
}

/// The whole file, empty when it is missing; an error when it resolves outside the project.
pub fn read(path: &Path) -> std::io::Result<String> {
    if path.symlink_metadata().is_err() {
        return Ok(String::new());
    }
    if !inside(path) {
        return Err(std::io::Error::other("it resolves outside the project"));
    }
    std::fs::read_to_string(path)
}

/// Replace the file with `text` in one rename, first keeping what it held at `backup`.
/// Both must resolve inside the project. A file linked elsewhere in the project is
/// replaced where it lives, so the link stays; a link at `backup` is removed, never
/// written through.
pub fn replace(path: &Path, text: &str, backup: &Path) -> std::io::Result<()> {
    if !inside(path) || !inside(backup) {
        return Err(std::io::Error::other("it resolves outside the project"));
    }
    let real = match path.symlink_metadata() {
        Ok(_) => path.canonicalize()?,
        Err(_) => path.to_path_buf(),
    };
    let fresh = |at: &Path| {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(at)
    };
    match std::fs::read_to_string(&real) {
        Ok(old) => {
            match std::fs::remove_file(backup) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
            fresh(backup)?.write_all(old.as_bytes())?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let dir = real.parent().unwrap_or(Path::new("."));
    let temporary = dir.join(format!(".{FILE}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let written = fresh(&temporary).and_then(|mut file| {
        file.write_all(text.as_bytes())?;
        file.sync_all()
    });
    match written.and_then(|()| std::fs::rename(&temporary, &real)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&temporary);
            Err(e)
        }
    }
}

/// Add `facts` to the candidates file at `path` as dated notes, each unless the file
/// already has it, keeping the newest `MAX_CANDIDATES`. Returns how many were added.
/// Private mode writes nothing.
pub fn propose(path: &Path, facts: &[String], date: &str) -> std::io::Result<usize> {
    if facts.is_empty() || crate::privacy::active() {
        return Ok(0);
    }
    let old = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut lines: Vec<String> = old.lines().map(str::to_string).collect();
    // Compared without the date, case or spacing, so the same fact proposed by a later
    // summary is not added again.
    let key = |line: &str| {
        let fact = line.trim_start_matches("- ");
        let fact = fact.split_once(": ").map_or(fact, |(_, fact)| fact);
        fact.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    let mut seen: std::collections::HashSet<String> = lines.iter().map(|l| key(l)).collect();
    let mut added = 0;
    for fact in facts {
        let Ok(line) = entry(fact, date) else {
            continue;
        };
        if seen.insert(key(&line)) {
            lines.push(line);
            added += 1;
        }
    }
    if added == 0 {
        return Ok(0);
    }
    let start = lines.len().saturating_sub(MAX_CANDIDATES);
    if let Some(dir) = path.parent() {
        crate::sessions::private_dir(dir)?;
    }
    crate::sessions::private_write(path, &(lines[start..].join("\n") + "\n"))?;
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_are_dated_deduplicated_and_capped() {
        let dir = crate::tools::temp_dir();
        let file = dir.join("session").join(CANDIDATES);
        let facts = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(propose(&file, &[], "d").unwrap(), 0);
        assert!(!file.exists());
        let first = facts(&[
            "tests need `--features x`: the default build skips them",
            " ",
        ]);
        assert_eq!(propose(&file, &first, "2026-11-26").unwrap(), 1);
        // The same fact, spaced and cased differently, from a later summary.
        let again = facts(&[
            "Tests  need `--features x`: the default build skips them",
            "uses pnpm",
        ]);
        assert_eq!(propose(&file, &again, "2026-11-27").unwrap(), 1);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "- 2026-11-26: tests need `--features x`: the default build skips them\n- 2026-11-27: uses pnpm\n"
        );
        let many: Vec<String> = (0..MAX_CANDIDATES).map(|i| format!("fact {i}")).collect();
        assert_eq!(propose(&file, &many, "d").unwrap(), MAX_CANDIDATES);
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text.lines().count(), MAX_CANDIDATES);
        assert!(text.starts_with("- d: fact 0\n"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn private_mode_proposes_nothing() {
        let dir = crate::tools::temp_dir();
        let file = dir.join(CANDIDATES);
        let privacy = std::sync::Arc::new(crate::privacy::Privacy::default());
        privacy.begin();
        let added = crate::privacy::CURRENT
            .scope(std::cell::RefCell::new(privacy), async {
                propose(&file, &["uses pnpm".to_string()], "d").unwrap()
            })
            .await;
        assert_eq!(added, 0);
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn replace_keeps_the_old_file_as_the_backup() {
        let dir = crate::tools::temp_dir();
        let bhai = dir.join(".bhai");
        std::fs::create_dir_all(&bhai).unwrap();
        let (file, backup) = (path(&bhai), bhai.join("MEMORY.md.bak"));
        assert_eq!(read(&file).unwrap(), "");
        replace(&file, "- first\n", &backup).unwrap();
        assert!(!backup.exists());
        replace(&file, "- second\n", &backup).unwrap();
        assert_eq!(read(&file).unwrap(), "- second\n");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "- first\n");
        let left: Vec<_> = std::fs::read_dir(&bhai).unwrap().collect();
        assert_eq!(left.len(), 2, "{left:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_or_blank_file_loads_nothing() {
        let dir = crate::tools::temp_dir();
        let file = path(&dir);
        assert_eq!(load(&file, "m".to_string()), Ok(None));
        std::fs::write(&file, "\n  \n").unwrap();
        assert_eq!(load(&file, "m".to_string()), Ok(None));
        std::fs::write(&file, b"\xff\xfe").unwrap();
        assert_eq!(load(&file, "m".to_string()), Ok(None));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_small_file_loads_whole() {
        let dir = crate::tools::temp_dir();
        let file = path(&dir);
        std::fs::write(&file, "- a\n- b\n\n").unwrap();
        let memory = load(&file, "./.bhai/MEMORY.md".to_string())
            .unwrap()
            .unwrap();
        assert_eq!(memory.content, "- a\n- b");
        assert_eq!(memory.cut, 0);
        assert_eq!(memory.label, "./.bhai/MEMORY.md");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The file only grows, so what is kept is the newest notes, in whole lines.
    #[test]
    fn a_large_file_keeps_its_newest_whole_lines() {
        let dir = crate::tools::temp_dir();
        let file = path(&dir);
        let lines: Vec<String> = (0..1000).map(|i| format!("- note {i:04}")).collect();
        std::fs::write(&file, lines.join("\n")).unwrap();
        let memory = load(&file, "m".to_string()).unwrap().unwrap();
        assert!(memory.content.len() <= MAX_LOAD);
        assert!(
            memory.content.starts_with("- note "),
            "{}",
            &memory.content[..20]
        );
        assert!(memory.content.ends_with("- note 0999"));
        let whole = lines.join("\n");
        assert_eq!(&whole[memory.cut..], memory.content);
        assert_eq!(whole.as_bytes()[memory.cut - 1], b'\n');

        // One line past the budget leaves nothing to keep, never half a line.
        std::fs::write(&file, "é".repeat(MAX_LOAD)).unwrap();
        let memory = load(&file, "m".to_string()).unwrap().unwrap();
        assert_eq!(memory.content, "");
        assert_eq!(memory.cut, MAX_LOAD * 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_note_is_one_dated_line_under_the_cap() {
        assert_eq!(
            entry("  uses   pnpm,\nnot npm ", "2026-10-02").unwrap(),
            "- 2026-10-02: uses pnpm, not npm"
        );
        assert!(entry(" \n ", "d").unwrap_err().contains("empty"));
        let err = entry(&"x".repeat(MAX_NOTE + 1), "d").unwrap_err();
        assert!(err.contains("1025 bytes"), "{err}");
        assert!(entry(&"x".repeat(MAX_NOTE), "d").is_ok());
    }

    #[test]
    fn append_creates_the_file_and_keeps_each_note_on_its_own_line() {
        let dir = crate::tools::temp_dir();
        let file = path(&dir.join(".bhai"));
        append(&file, "- one").unwrap();
        append(&file, "- two").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "- one\n- two\n");
        // A file the user edited without a final newline.
        std::fs::write(&file, "- mine").unwrap();
        append(&file, "- three").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "- mine\n- three\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_linked_out_of_the_project_is_neither_read_nor_written() {
        let dir = crate::tools::temp_dir();
        let secret = dir.join("credentials");
        std::fs::write(&secret, "aws_secret_access_key = x\n").unwrap();
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join(".bhai")).unwrap();
        let file = path(&repo.join(".bhai"));
        std::os::unix::fs::symlink(&secret, &file).unwrap();
        assert_eq!(
            load(&file, "./.bhai/MEMORY.md".to_string()),
            Err("skipped ./.bhai/MEMORY.md (outside project)".to_string())
        );
        assert!(append(&file, "- note").is_err());
        assert!(read(&file).is_err());
        assert!(replace(&file, "- note\n", &repo.join(".bhai/MEMORY.md.bak")).is_err());
        assert_eq!(
            std::fs::read_to_string(&secret).unwrap(),
            "aws_secret_access_key = x\n"
        );
        // A backup linked out is refused before anything is written.
        std::fs::remove_file(&file).unwrap();
        std::fs::write(&file, "- mine\n").unwrap();
        let backup = repo.join(".bhai/MEMORY.md.bak");
        std::os::unix::fs::symlink(&secret, &backup).unwrap();
        assert!(replace(&file, "- new\n", &backup).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "- mine\n");
        std::fs::remove_file(&backup).unwrap();
        std::fs::remove_file(&file).unwrap();

        // `.bhai` itself linked out, with no memory file yet.
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::remove_dir_all(repo.join(".bhai")).unwrap();
        std::os::unix::fs::symlink(&other, repo.join(".bhai")).unwrap();
        assert!(append(&file, "- note").is_err());
        assert!(!other.join(FILE).exists());
        std::fs::write(other.join(FILE), "- planted").unwrap();
        assert!(load(&file, "m".to_string()).is_err());

        // A link that stays in the project is followed.
        std::fs::remove_file(repo.join(".bhai")).unwrap();
        std::fs::create_dir_all(repo.join(".bhai")).unwrap();
        std::fs::write(repo.join("notes.md"), "- kept").unwrap();
        std::os::unix::fs::symlink(repo.join("notes.md"), &file).unwrap();
        assert_eq!(
            load(&file, "m".to_string()).unwrap().unwrap().content,
            "- kept"
        );
        append(&file, "- two").unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.join("notes.md")).unwrap(),
            "- kept\n- two\n"
        );
        replace(&file, "- three\n", &repo.join(".bhai/MEMORY.md.bak")).unwrap();
        assert!(file.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(
            std::fs::read_to_string(repo.join("notes.md")).unwrap(),
            "- three\n"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
