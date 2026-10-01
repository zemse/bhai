//! `apply_patch`, a Responses `custom` tool whose input is a patch in the format the GPT
//! models are trained on, held to its grammar by the backend. The whole patch is planned
//! in memory before anything is written; each file is then replaced through a temp file
//! and a rename, and a failure part way puts back the files already written. Ollama has
//! no custom tools, so it leaves this one out and keeps `edit`.

use std::fs::Permissions;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use serde_json::{Value, json};

use super::{BoxFuture, Tool, string_arg};

pub const NAME: &str = "apply_patch";

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE: &str = "*** Move to: ";
const EOF: &str = "*** End of File";

/// Codex's grammar, which the models were trained to write.
const GRAMMAR: &str = r#"start: begin_patch hunk+ end_patch
begin_patch: "*** Begin Patch" LF
end_patch: "*** End Patch" LF?

hunk: add_hunk | delete_hunk | update_hunk
add_hunk: "*** Add File: " filename LF add_line+
delete_hunk: "*** Delete File: " filename LF
update_hunk: "*** Update File: " filename LF change_move? change?

filename: /(.+)/
add_line: "+" /(.*)/ LF -> line

change_move: "*** Move to: " filename LF
change: (change_context | change_line)+ eof_line?
change_context: ("@@" | "@@ " /(.+)/) LF
change_line: ("+" | "-" | " ") /(.*)/ LF
eof_line: "*** End of File" LF

%import common.LF
"#;

pub struct ApplyPatch;

#[derive(Debug, PartialEq)]
enum Hunk {
    Add {
        path: String,
        lines: Vec<String>,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        to: Option<String>,
        chunks: Vec<Chunk>,
    },
}

#[derive(Debug, Default, PartialEq)]
struct Chunk {
    /// The `@@` lines, each found in turn before the change is.
    anchors: Vec<String>,
    /// Context and `-` lines, as the file has them now.
    old: Vec<String>,
    /// Context and `+` lines, as the file will have them.
    new: Vec<String>,
    added: usize,
    removed: usize,
    /// The change ends at the end of the file.
    eof: bool,
}

impl Chunk {
    fn is_empty(&self) -> bool {
        self.old.is_empty() && self.new.is_empty()
    }
}

/// One file's change, `None` where the file is absent.
struct Change {
    path: PathBuf,
    before: Option<String>,
    after: Option<String>,
    mode: Option<Permissions>,
    /// The file a move takes this one's content from.
    from: Option<PathBuf>,
}

impl Tool for ApplyPatch {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "custom",
            "name": NAME,
            "description": "Edit files with a patch. This is a FREEFORM tool: send the patch \
        itself, not JSON.\n*** Begin Patch\n*** Update File: src/app.py\n@@ def greet():\n-    \
        print(\"hi\")\n+    print(\"hello\")\n*** Add File: notes.txt\n+first line\n*** Delete \
        File: old.txt\n*** End Patch\nPaths are relative to the working directory, or absolute. \
        `*** Move to: <path>` right after an Update File line renames the file. Give three lines \
        of context around each change, and an `@@` line naming the enclosing function or class \
        when that is what tells two places apart: every change must match exactly one place. Add \
        File refuses a file that exists. A patch applies in full or not at all, and the user \
        approves every patch.",
            "format": {
                "type": "grammar",
                "syntax": "lark",
                "definition": GRAMMAR,
            },
        })
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        let hunks = parse(input(args)?)?;
        let cwd = std::env::current_dir().unwrap_or_default();
        Ok(summary(&hunks, |path| {
            super::preview_text(&resolve(&cwd, path))
                .ok()
                .map(|text| text.lines().count())
        }))
    }

    fn preview(&self, args: &Value) -> Option<String> {
        let hunks = parse(input(args).ok()?).ok()?;
        let cwd = std::env::current_dir().ok()?;
        let changes = plan(&hunks, &cwd, &|path| super::preview_text(path)).ok()?;
        Some(diff(&changes, &cwd))
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let cwd = match std::env::current_dir() {
                Ok(cwd) => cwd,
                Err(e) => return (format!("Could not read the working directory: {e}"), false),
            };
            match input(args).and_then(|input| apply(input, &cwd)) {
                Ok(output) => (output, true),
                Err(e) => (format!("The patch was not applied: {e}"), false),
            }
        })
    }
}

fn input(args: &Value) -> Result<&str, String> {
    string_arg(args, "input").ok_or_else(|| "missing the patch text.".to_string())
}

/// Every path a patch writes or removes, resolved against `cwd`; `None` when it does not
/// parse.
pub fn paths(args: &Value, cwd: &Path) -> Option<Vec<PathBuf>> {
    let hunks = parse(input(args).ok()?).ok()?;
    Some(
        targets(&hunks)
            .into_iter()
            .map(|path| resolve(cwd, path))
            .collect(),
    )
}

/// The approval line for a patch, read from its text alone, for a transcript read back.
pub fn summarize(input: &str) -> Option<String> {
    parse(input).ok().map(|hunks| summary(&hunks, |_| None))
}

/// `apply_patch` and each file with its counts. `lines` counts a deleted file's lines.
fn summary(hunks: &[Hunk], lines: impl Fn(&str) -> Option<usize>) -> String {
    let each: Vec<String> = hunks
        .iter()
        .map(|hunk| match hunk {
            Hunk::Add { path, lines } => format!("add {path} (+{})", lines.len()),
            Hunk::Delete { path } => match lines(path) {
                Some(n) => format!("delete {path} (-{n})"),
                None => format!("delete {path}"),
            },
            Hunk::Update { path, to, chunks } => {
                let added: usize = chunks.iter().map(|c| c.added).sum();
                let removed: usize = chunks.iter().map(|c| c.removed).sum();
                match to {
                    Some(to) => format!("move {path} -> {to} (+{added} -{removed})"),
                    None => format!("update {path} (+{added} -{removed})"),
                }
            }
        })
        .collect();
    format!("{NAME} {}", each.join(", "))
}

fn parse(input: &str) -> Result<Vec<Hunk>, String> {
    let lines: Vec<&str> = input.trim().lines().collect();
    if lines.first().map(|l| l.trim()) != Some(BEGIN) {
        return Err(format!("the first line must be `{BEGIN}`."));
    }
    if lines.len() < 2 || lines.last().map(|l| l.trim()) != Some(END) {
        return Err(format!("the last line must be `{END}`."));
    }
    let body = &lines[1..lines.len() - 1];
    let name = |raw: &str, at: usize| {
        let path = raw.trim();
        match path.is_empty() {
            true => Err(format!("line {at} names no file.")),
            false => Ok(path.to_string()),
        }
    };
    let mut hunks = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let line = body[i];
        // The line's number in the patch, counting `*** Begin Patch` as the first.
        let at = i + 2;
        i += 1;
        if let Some(path) = line.strip_prefix(ADD) {
            let mut added = Vec::new();
            while let Some(text) = body.get(i).and_then(|l| l.strip_prefix('+')) {
                added.push(text.to_string());
                i += 1;
            }
            hunks.push(Hunk::Add {
                path: name(path, at)?,
                lines: added,
            });
        } else if let Some(path) = line.strip_prefix(DELETE) {
            hunks.push(Hunk::Delete {
                path: name(path, at)?,
            });
        } else if let Some(path) = line.strip_prefix(UPDATE) {
            let path = name(path, at)?;
            let to = match body.get(i).and_then(|l| l.strip_prefix(MOVE)) {
                Some(to) => {
                    i += 1;
                    Some(name(to, at + 1)?)
                }
                None => None,
            };
            let mut chunks: Vec<Chunk> = Vec::new();
            while let Some(&line) = body.get(i) {
                let at = i + 2;
                if line.trim_end() == EOF {
                    match chunks.last_mut() {
                        Some(chunk) if !chunk.is_empty() => chunk.eof = true,
                        _ => return Err(format!("line {at}: `{EOF}` follows no change.")),
                    }
                    i += 1;
                    continue;
                }
                if line.starts_with("*** ") {
                    break;
                }
                i += 1;
                if let Some(anchor) = line.strip_prefix("@@") {
                    // A run of `@@` lines narrows in turn, so they share one chunk.
                    if chunks.last().is_none_or(|c| !c.is_empty()) {
                        chunks.push(Chunk::default());
                    }
                    let anchor = anchor.trim();
                    if !anchor.is_empty() {
                        chunks
                            .last_mut()
                            .expect("pushed")
                            .anchors
                            .push(anchor.to_string());
                    }
                    continue;
                }
                if chunks.last().is_none_or(|c| c.eof) {
                    chunks.push(Chunk::default());
                }
                let chunk = chunks.last_mut().expect("pushed");
                // An empty line is a blank context line whose leading space was dropped.
                let text = line.get(1..).unwrap_or_default().to_string();
                match line.chars().next() {
                    None => {
                        chunk.old.push(String::new());
                        chunk.new.push(String::new());
                    }
                    Some(' ') => {
                        chunk.old.push(text.clone());
                        chunk.new.push(text);
                    }
                    Some('-') => {
                        chunk.old.push(text);
                        chunk.removed += 1;
                    }
                    Some('+') => {
                        chunk.new.push(text);
                        chunk.added += 1;
                    }
                    _ => {
                        return Err(format!(
                            "line {at}: `{line}` is not part of a change. Each line of one \
starts with a space, `-` or `+`."
                        ));
                    }
                }
            }
            if chunks.iter().any(Chunk::is_empty) {
                return Err(format!("an `@@` line in `{path}` has no change under it."));
            }
            if chunks.is_empty() && to.is_none() {
                return Err(format!("`{UPDATE}{path}` changes nothing."));
            }
            hunks.push(Hunk::Update { path, to, chunks });
        } else if !line.trim().is_empty() {
            return Err(format!(
                "line {at}: expected `{ADD}`, `{DELETE}` or `{UPDATE}`, got `{line}`."
            ));
        }
    }
    if hunks.is_empty() {
        return Err("the patch changes no file.".to_string());
    }
    Ok(hunks)
}

/// Every path a patch names, a move's destination included.
fn targets(hunks: &[Hunk]) -> Vec<&str> {
    let mut paths = Vec::new();
    for hunk in hunks {
        match hunk {
            Hunk::Add { path, .. } | Hunk::Delete { path } => paths.push(path.as_str()),
            Hunk::Update { path, to, .. } => {
                paths.push(path.as_str());
                paths.extend(to.as_deref());
            }
        }
    }
    paths
}

/// `raw` against `cwd`, with `.` and `..` worked out on the text.
fn resolve(cwd: &Path, raw: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for part in cwd.join(raw).components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The file a path names once its links are followed, so two names for one file are seen
/// as one.
fn real(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Every file's text before and after, read through `read`, or why the patch cannot apply.
fn plan(
    hunks: &[Hunk],
    cwd: &Path,
    read: &dyn Fn(&Path) -> std::io::Result<String>,
) -> Result<Vec<Change>, String> {
    let mut seen: Vec<PathBuf> = Vec::new();
    for raw in targets(hunks) {
        let path = real(&resolve(cwd, raw));
        if seen.contains(&path) {
            return Err(format!(
                "`{raw}` is named twice. Put every change to one file in one hunk."
            ));
        }
        seen.push(path);
    }
    let load =
        |raw: &str, path: &Path| read(path).map_err(|e| format!("could not read `{raw}`: {e}."));
    let absent = |raw: &str, path: &Path, what: &str| match path.symlink_metadata() {
        Ok(_) => Err(format!("`{raw}` already exists; {what}.")),
        Err(_) => Ok(()),
    };
    let mode = |path: &Path| std::fs::metadata(path).ok().map(|m| m.permissions());
    let mut changes = Vec::new();
    for hunk in hunks {
        match hunk {
            Hunk::Add { path: raw, lines } => {
                let path = resolve(cwd, raw);
                absent(raw, &path, "use `*** Update File:` to change it")?;
                let mut text = lines.join("\n");
                if !lines.is_empty() {
                    text.push('\n');
                }
                changes.push(Change {
                    path,
                    before: None,
                    after: Some(text),
                    mode: None,
                    from: None,
                });
            }
            Hunk::Delete { path: raw } => {
                let path = resolve(cwd, raw);
                let before = load(raw, &path)?;
                changes.push(Change {
                    mode: mode(&path),
                    path,
                    before: Some(before),
                    after: None,
                    from: None,
                });
            }
            Hunk::Update {
                path: raw,
                to,
                chunks,
            } => {
                let path = resolve(cwd, raw);
                let before = load(raw, &path)?;
                let after = update(&before, chunks).map_err(|e| format!("in `{raw}`, {e}"))?;
                match to {
                    Some(to) => {
                        let dest = resolve(cwd, to);
                        absent(to, &dest, "a move does not replace a file")?;
                        changes.push(Change {
                            path: dest,
                            before: None,
                            after: Some(after),
                            mode: mode(&path),
                            from: Some(path.clone()),
                        });
                        // After the copy, so a failed write leaves the original in place.
                        changes.push(Change {
                            mode: mode(&path),
                            path,
                            before: Some(before),
                            after: None,
                            from: None,
                        });
                    }
                    None => changes.push(Change {
                        mode: mode(&path),
                        path,
                        before: Some(before),
                        after: Some(after),
                        from: None,
                    }),
                }
            }
        }
    }
    Ok(changes)
}

/// `text` with every chunk applied in order. Each chunk is looked for after the last.
fn update(text: &str, chunks: &[Chunk]) -> Result<String, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let (mut copied, mut cursor) = (0, 0);
    for (n, chunk) in chunks.iter().enumerate() {
        let which = match chunks.len() {
            1 => "the change".to_string(),
            _ => format!("change {}", n + 1),
        };
        for anchor in &chunk.anchors {
            let at = seek(&lines, std::slice::from_ref(anchor), cursor, false)
                .map_err(|e| format!("the `@@ {anchor}` line of {which} {e}"))?;
            cursor = at + 1;
        }
        // A change that only adds goes under its `@@` line, or at the end without one.
        let start = match chunk.old.is_empty() {
            true if chunk.anchors.is_empty() || chunk.eof => lines.len(),
            true => cursor,
            false => seek(&lines, &chunk.old, cursor, chunk.eof)
                .map_err(|e| format!("the context and `-` lines of {which} {e}"))?,
        };
        out.extend(&lines[copied..start]);
        out.extend(chunk.new.iter().map(String::as_str));
        copied = start + chunk.old.len();
        cursor = copied;
    }
    out.extend(&lines[copied..]);
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut joined = out.join(newline);
    if !out.is_empty() && (text.is_empty() || text.ends_with('\n')) {
        joined.push_str(newline);
    }
    Ok(joined)
}

fn same(s: &str) -> &str {
    s
}

/// Where `want` starts at or after `from`: exactly, then ignoring trailing whitespace,
/// then ignoring indentation too. The first tier with a match decides, and it must have
/// only one.
fn seek(lines: &[&str], want: &[String], from: usize, eof: bool) -> Result<usize, String> {
    const MISSING: &str = "were not found. Read the file again and copy the lines exactly.";
    let Some(last) = lines
        .len()
        .checked_sub(want.len())
        .filter(|&last| last >= from)
    else {
        return Err(MISSING.to_string());
    };
    let starts = match eof {
        true => last..=last,
        false => from..=last,
    };
    let tiers: [fn(&str) -> &str; 3] = [same, str::trim_end, str::trim];
    for norm in tiers {
        let found: Vec<usize> = starts
            .clone()
            .filter(|&s| {
                lines[s..s + want.len()]
                    .iter()
                    .zip(want)
                    .all(|(have, want)| norm(have) == norm(want))
            })
            .collect();
        match found.as_slice() {
            [] => continue,
            [one] => return Ok(*one),
            many => {
                return Err(format!(
                    "match {} places. Add context lines, or an `@@` line naming the enclosing \
function or class, until they match one.",
                    many.len()
                ));
            }
        }
    }
    Err(MISSING.to_string())
}

/// Plan the patch, write it, and say what changed.
fn apply(input: &str, cwd: &Path) -> Result<String, String> {
    let hunks = parse(input)?;
    let changes = plan(&hunks, cwd, &|path| {
        if !std::fs::metadata(path)?.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        std::fs::read_to_string(path)
    })?;
    commit(&changes)?;
    let summary = summary(&hunks, |raw| {
        let path = resolve(cwd, raw);
        changes
            .iter()
            .find(|c| c.path == path)
            .and_then(|c| c.before.as_deref())
            .map(|text| text.lines().count())
    });
    Ok(format!(
        "Applied: {}.",
        summary
            .strip_prefix(&format!("{NAME} "))
            .unwrap_or(&summary)
    ))
}

/// Write every change, or none: a failure puts back the files written before it.
fn commit(changes: &[Change]) -> Result<(), String> {
    let mut made = Vec::new();
    for (done, change) in changes.iter().enumerate() {
        let Err(e) = put(change, change.after.as_deref(), &mut made) else {
            continue;
        };
        let mut lost = Vec::new();
        for change in changes[..done].iter().rev() {
            if let Err(e) = put(change, change.before.as_deref(), &mut made) {
                lost.push(e);
            }
        }
        // Deepest first, and only once empty.
        made.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
        for dir in &made {
            let _ = std::fs::remove_dir(dir);
        }
        return Err(match lost.is_empty() {
            true => format!("{e}; the files written before it were put back."),
            false => format!(
                "{e}; putting back the files written before it failed too: {}.",
                lost.join("; ")
            ),
        });
    }
    Ok(())
}

/// Give `change.path` the content `text`, or remove it for `None`. A write goes through a
/// temp file beside the target, so the file is never seen half written; `made` gets the
/// directories it had to create.
fn put(change: &Change, text: Option<&str>, made: &mut Vec<PathBuf>) -> Result<(), String> {
    let path = &change.path;
    let Some(text) = text else {
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("could not delete {}: {e}", path.display()))
            }
            _ => Ok(()),
        };
    };
    // Through a link to the file it points at, as a plain write would.
    let target = real(path);
    let failed = |e: std::io::Error| format!("could not write {}: {e}", path.display());
    let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
        return Err(failed(std::io::Error::other("no file name")));
    };
    let mut missing = dir;
    while !missing.as_os_str().is_empty() && !missing.exists() {
        made.push(missing.to_path_buf());
        missing = missing.parent().unwrap_or(Path::new(""));
    }
    std::fs::create_dir_all(dir).map_err(failed)?;
    let temp = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        uuid::Uuid::new_v4().simple()
    ));
    if let Err(e) = swap(&temp, text, change.mode.as_ref(), &target) {
        let _ = std::fs::remove_file(&temp);
        return Err(failed(e));
    }
    Ok(())
}

/// Write `text` to `temp`, synced and with `mode`, then rename it over `target`.
fn swap(temp: &Path, text: &str, mode: Option<&Permissions>, target: &Path) -> std::io::Result<()> {
    let mut file = std::fs::File::create(temp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    if let Some(mode) = mode {
        std::fs::set_permissions(temp, mode.clone())?;
    }
    std::fs::rename(temp, target)
}

/// Each file's unified diff under a `diff` header, cut at the preview's length.
fn diff(changes: &[Change], cwd: &Path) -> String {
    let shown = |path: &Path| path.strip_prefix(cwd).unwrap_or(path).display().to_string();
    let moved: Vec<&PathBuf> = changes.iter().filter_map(|c| c.from.as_ref()).collect();
    let mut out: Vec<String> = Vec::new();
    for change in changes {
        let (header, before) = match (&change.from, &change.before, &change.after) {
            (Some(from), ..) => {
                let before = changes
                    .iter()
                    .find(|c| &c.path == from)
                    .and_then(|c| c.before.as_deref());
                (
                    format!("diff move {} -> {}", shown(from), shown(&change.path)),
                    before,
                )
            }
            (None, _, None) if moved.contains(&&change.path) => continue,
            (None, None, _) => (format!("diff add {}", shown(&change.path)), None),
            (None, before, None) => (
                format!("diff delete {}", shown(&change.path)),
                before.as_deref(),
            ),
            (None, before, Some(_)) => (
                format!("diff update {}", shown(&change.path)),
                before.as_deref(),
            ),
        };
        out.push(header);
        let after = change.after.as_deref().unwrap_or_default();
        if let Some(hunks) = crate::diff::unified(before.unwrap_or_default(), after) {
            out.extend(hunks.lines().map(str::to_string));
        }
    }
    let max = crate::diff::MAX_PREVIEW_LINES;
    if out.len() > max {
        let more = out.len() - max;
        out.truncate(max);
        out.push(format!("[+{more} more lines]"));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(body: &str) -> String {
        format!("{BEGIN}\n{body}{END}\n")
    }

    fn run(dir: &Path, body: &str) -> Result<String, String> {
        apply(&patch(body), dir)
    }

    #[test]
    fn the_schema_is_a_custom_tool_with_the_lark_grammar() {
        let schema = ApplyPatch.schema();
        assert_eq!(schema["type"], "custom");
        assert_eq!(schema["name"], NAME);
        assert_eq!(schema["format"]["syntax"], "lark");
        assert!(
            schema["format"]["definition"]
                .as_str()
                .unwrap()
                .starts_with("start: begin_patch hunk+ end_patch")
        );
    }

    #[test]
    fn parses_every_kind_of_hunk() {
        let hunks = parse(&patch(
            "*** Add File: a.txt\n+one\n+two\n*** Delete File: b.txt\n*** Update File: c.rs\n\
*** Move to: d.rs\n@@ fn main\n@@ let x\n keep\n-old\n+new\n\n*** End of File\n",
        ))
        .unwrap();
        assert_eq!(
            hunks,
            vec![
                Hunk::Add {
                    path: "a.txt".into(),
                    lines: vec!["one".into(), "two".into()],
                },
                Hunk::Delete {
                    path: "b.txt".into()
                },
                Hunk::Update {
                    path: "c.rs".into(),
                    to: Some("d.rs".into()),
                    chunks: vec![Chunk {
                        anchors: vec!["fn main".into(), "let x".into()],
                        old: vec!["keep".into(), "old".into(), String::new()],
                        new: vec!["keep".into(), "new".into(), String::new()],
                        added: 1,
                        removed: 1,
                        eof: true,
                    }],
                },
            ]
        );
    }

    #[test]
    fn a_malformed_patch_says_where() {
        let err = parse("*** Update File: a\n").unwrap_err();
        assert!(err.contains("first line"), "{err}");
        let err = parse(&format!("{BEGIN}\n*** Add File: a\n+x\n")).unwrap_err();
        assert!(err.contains("last line"), "{err}");
        let err = parse(&patch("*** Update File: a\n@@\n x\nnope\n")).unwrap_err();
        assert!(err.contains("line 5") && err.contains("`nope`"), "{err}");
        let err = parse(&patch("*** Rename File: a\n")).unwrap_err();
        assert!(err.contains("line 2"), "{err}");
        assert!(parse(&patch("")).unwrap_err().contains("no file"));
        let err = parse(&patch("*** Update File: a\n")).unwrap_err();
        assert!(err.contains("changes nothing"), "{err}");
        let err = parse(&patch("*** Update File: a\n@@ f\n*** Delete File: b\n")).unwrap_err();
        assert!(err.contains("no change under it"), "{err}");
    }

    #[test]
    fn the_summary_lists_every_file_with_its_counts() {
        let input = patch(
            "*** Update File: src/a.rs\n@@\n x\n-y\n+z\n+w\n*** Add File: b.txt\n+1\n+2\n\
*** Delete File: c.txt\n*** Update File: d.rs\n*** Move to: e.rs\n",
        );
        assert_eq!(
            summarize(&input).unwrap(),
            "apply_patch update src/a.rs (+2 -1), add b.txt (+2), delete c.txt, \
move d.rs -> e.rs (+0 -0)"
        );
    }

    #[test]
    fn applies_adds_deletes_updates_and_moves_in_one_call() {
        let dir = super::super::temp_dir();
        std::fs::write(dir.join("a.rs"), "fn a() {\n    1\n}\n").unwrap();
        std::fs::write(dir.join("gone.txt"), "x\ny\n").unwrap();
        std::fs::write(dir.join("old.rs"), "keep\nchange\n").unwrap();
        let out = run(
            &dir,
            "*** Update File: a.rs\n@@ fn a() {\n-    1\n+    2\n*** Add File: new/b.txt\n+hi\n\
*** Delete File: gone.txt\n*** Update File: old.rs\n*** Move to: moved/new.rs\n keep\n-change\n+changed\n",
        )
        .unwrap();
        assert_eq!(
            out,
            "Applied: update a.rs (+1 -1), add new/b.txt (+1), delete gone.txt (-2), \
move old.rs -> moved/new.rs (+1 -1)."
        );
        let read = |p: &str| std::fs::read_to_string(dir.join(p)).unwrap();
        assert_eq!(read("a.rs"), "fn a() {\n    2\n}\n");
        assert_eq!(read("new/b.txt"), "hi\n");
        assert_eq!(read("moved/new.rs"), "keep\nchanged\n");
        assert!(!dir.join("gone.txt").exists());
        assert!(!dir.join("old.rs").exists());
    }

    #[test]
    fn an_ambiguous_match_is_refused_rather_than_guessed() {
        let dir = super::super::temp_dir();
        let text = "fn a() {\n    x\n}\nfn b() {\n    x\n}\n";
        std::fs::write(dir.join("f.rs"), text).unwrap();
        let err = run(&dir, "*** Update File: f.rs\n-    x\n+    y\n").unwrap_err();
        assert!(err.contains("match 2 places"), "{err}");
        assert_eq!(std::fs::read_to_string(dir.join("f.rs")).unwrap(), text);
        // An anchor narrows it to one.
        run(&dir, "*** Update File: f.rs\n@@ fn b() {\n-    x\n+    y\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("f.rs")).unwrap(),
            "fn a() {\n    x\n}\nfn b() {\n    y\n}\n"
        );
        // So does the end of the file.
        run(
            &dir,
            "*** Update File: f.rs\n-    y\n-}\n+}\n*** End of File\n",
        )
        .unwrap();
        assert!(
            std::fs::read_to_string(dir.join("f.rs"))
                .unwrap()
                .ends_with("fn b() {\n}\n")
        );
    }

    #[test]
    fn whitespace_tiers_match_only_when_exact_does_not() {
        let lines = ["  a  ", "a", "\tb"];
        let want = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(seek(&lines, &want(&["a"]), 0, false), Ok(1));
        assert_eq!(seek(&lines, &want(&["b"]), 0, false), Ok(2));
        assert!(
            seek(&lines, &want(&[" a"]), 0, false)
                .unwrap_err()
                .contains("2 places")
        );
        assert!(
            seek(&lines, &want(&["c"]), 0, false)
                .unwrap_err()
                .contains("not found")
        );
        assert!(
            seek(&lines, &want(&["a"]), 2, false).is_err(),
            "only after `from`"
        );
    }

    #[test]
    fn line_endings_and_a_missing_final_newline_are_kept() {
        let chunk = |old: &str, new: &str| Chunk {
            old: vec![old.into()],
            new: vec![new.into()],
            ..Chunk::default()
        };
        assert_eq!(
            update("a\r\nb\r\n", &[chunk("b", "c")]).unwrap(),
            "a\r\nc\r\n"
        );
        assert_eq!(update("a\nb", &[chunk("b", "c")]).unwrap(), "a\nc");
        let add = Chunk {
            new: vec!["z".into()],
            ..Chunk::default()
        };
        assert_eq!(update("a\n", &[add]).unwrap(), "a\nz\n");
    }

    #[test]
    fn nothing_is_written_when_any_file_fails_to_plan() {
        let dir = super::super::temp_dir();
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        let err = run(
            &dir,
            "*** Update File: a.txt\n-a\n+A\n*** Update File: b.txt\n-zzz\n+B\n",
        )
        .unwrap_err();
        assert!(
            err.contains("in `b.txt`") && err.contains("not found"),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a\n");

        let err = run(&dir, "*** Add File: a.txt\n+x\n").unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        let err = run(&dir, "*** Update File: a.txt\n*** Move to: b.txt\n").unwrap_err();
        assert!(err.contains("already exists"), "{err}");
    }

    #[test]
    fn a_file_named_twice_is_refused() {
        let dir = super::super::temp_dir();
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        let err = run(
            &dir,
            "*** Update File: a.txt\n-a\n+b\n*** Update File: ./sub/../a.txt\n-b\n+c\n",
        )
        .unwrap_err();
        assert!(err.contains("named twice"), "{err}");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("a.txt"), dir.join("link.txt")).unwrap();
            let err = run(
                &dir,
                "*** Update File: a.txt\n-a\n+b\n*** Delete File: link.txt\n",
            )
            .unwrap_err();
            assert!(err.contains("named twice"), "{err}");
        }
    }

    #[test]
    fn a_failed_write_puts_back_what_was_already_written() {
        let dir = super::super::temp_dir();
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        std::fs::write(dir.join("gone.txt"), "g\n").unwrap();
        // A regular file where a directory has to go fails only once writing starts.
        std::fs::write(dir.join("blocker"), "").unwrap();
        let err = run(
            &dir,
            "*** Update File: a.txt\n-a\n+A\n*** Delete File: gone.txt\n*** Add File: new/x.txt\n\
+x\n*** Add File: blocker/y.txt\n+y\n",
        )
        .unwrap_err();
        assert!(err.contains("put back"), "{err}");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a\n");
        assert_eq!(
            std::fs::read_to_string(dir.join("gone.txt")).unwrap(),
            "g\n"
        );
        assert!(
            !dir.join("new").exists(),
            "the directory it made is gone too"
        );
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(names.len(), 3, "no temp file is left behind");
    }

    #[cfg(unix)]
    #[test]
    fn a_write_keeps_the_mode_and_goes_through_a_link() {
        use std::os::unix::fs::PermissionsExt;
        let dir = super::super::temp_dir();
        let script = dir.join("run.sh");
        std::fs::write(&script, "echo a\n").unwrap();
        std::fs::set_permissions(&script, Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&script, dir.join("link.sh")).unwrap();
        run(&dir, "*** Update File: link.sh\n-echo a\n+echo b\n").unwrap();
        assert_eq!(std::fs::read_to_string(&script).unwrap(), "echo b\n");
        let mode = std::fs::metadata(&script).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert!(dir.join("link.sh").symlink_metadata().unwrap().is_symlink());
    }

    #[test]
    fn the_preview_diffs_every_file() {
        let dir = super::super::temp_dir();
        std::fs::write(dir.join("a.txt"), "a\nb\n").unwrap();
        std::fs::write(dir.join("m.txt"), "m\n").unwrap();
        let hunks = parse(&patch(
            "*** Update File: a.txt\n a\n-b\n+c\n*** Add File: n.txt\n+n\n\
*** Update File: m.txt\n*** Move to: z.txt\n",
        ))
        .unwrap();
        let changes = plan(&hunks, &dir, &|p| super::super::preview_text(p)).unwrap();
        assert_eq!(
            diff(&changes, &dir),
            "diff update a.txt\n@@ -1,2 +1,2 @@\n a\n-b\n+c\ndiff add n.txt\n@@ -0,0 +1 @@\n+n\n\
diff move m.txt -> z.txt"
        );
    }

    #[test]
    fn paths_resolves_every_target_against_the_root() {
        let args = json!({"input": patch("*** Add File: a/b.txt\n+x\n*** Update File: /abs/c\n\
*** Move to: ../d\n")});
        assert_eq!(
            paths(&args, Path::new("/p/root")).unwrap(),
            [
                PathBuf::from("/p/root/a/b.txt"),
                PathBuf::from("/abs/c"),
                PathBuf::from("/p/d")
            ]
        );
        assert!(paths(&json!({"input": "nope"}), Path::new("/p")).is_none());
    }
}
