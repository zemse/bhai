//! Saved sessions: each session's history as append-only JSONL under
//! `.bhai/sessions/<id>.jsonl`, a header line first, so `--resume` can continue it.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Where sessions live, under the project root.
pub const DIR: &str = ".bhai/sessions";
/// Characters of the first user message `bhai sessions` shows.
const FIRST_CHARS: usize = 60;

/// The first line of a session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    pub session: String,
    pub identity: String,
    pub model: String,
    pub effort: String,
    pub cwd: PathBuf,
    pub created: DateTime<Local>,
    pub version: String,
    /// Fingerprint of the model, instructions and tools; see [`prefix`].
    #[serde(default)]
    pub prefix: String,
}

impl Header {
    pub fn new(session: &str, identity: &str, model: &str, effort: &str, cwd: &Path) -> Self {
        Self {
            session: session.to_string(),
            identity: identity.to_string(),
            model: model.to_string(),
            effort: effort.to_string(),
            cwd: cwd.to_path_buf(),
            created: Local::now(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            prefix: String::new(),
        }
    }
}

/// sha256 of what the cached prefix is made of, so a resume can tell it changed.
pub fn prefix(model: &str, instructions: &str, tools: &[Value]) -> String {
    let mut hasher = Sha256::new();
    for part in [
        model,
        instructions,
        &Value::from(tools.to_vec()).to_string(),
    ] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// The file for session `id` under `dir`.
pub fn path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.jsonl"))
}

/// Appends history items to one session file. The header is written with the first
/// item, so a session that never got a message leaves no file.
pub struct Writer {
    path: PathBuf,
    pub header: Header,
    started: bool,
    /// The id of the last record written.
    last: Option<String>,
    /// Where child transcripts of this session go.
    children: PathBuf,
}

impl Writer {
    pub fn create(dir: &Path, header: Header) -> Self {
        Self {
            path: path(dir, &header.session),
            children: dir.join(&header.session),
            header,
            started: false,
            last: None,
        }
    }

    /// Continue `loaded`, cutting off whatever `load` skipped so new records follow
    /// the last good one.
    pub fn resume(dir: &Path, loaded: &Loaded) -> Result<Self> {
        let path = path(dir, &loaded.header.session);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .with_context(|| format!("could not open {}", path.display()))?;
        if file.metadata()?.len() != loaded.len {
            file.set_len(loaded.len)?;
        }
        Ok(Self {
            path,
            children: dir.join(&loaded.header.session),
            header: loaded.header.clone(),
            started: true,
            last: loaded.last.clone(),
        })
    }

    /// Append one history item, flushed before returning.
    pub fn append(&mut self, item: &Value) -> Result<()> {
        let mut record = json!({ "type": "item", "item": item });
        if let Some(sidechain) = self.sidechain(item) {
            record["sidechain"] = json!(sidechain);
        }
        self.write(record)
    }

    /// Record a `/model` switch, so a resume comes back on the model the session was
    /// last on. Before the header has gone out there is nothing to follow it with, so
    /// the header itself is what changes.
    pub fn model(&mut self, model: &str, effort: &str, prefix: &str) -> Result<()> {
        self.header.model = model.to_string();
        self.header.effort = effort.to_string();
        self.header.prefix = prefix.to_string();
        if !self.started {
            return Ok(());
        }
        self.write(json!({
            "type": "model",
            "model": model,
            "effort": effort,
            "prefix": prefix,
        }))
    }

    /// Record a compaction: `items` replace the history so far on load.
    pub fn compact(&mut self, stage: &str, before: u64, after: u64, items: &[Value]) -> Result<()> {
        self.write(json!({
            "type": "compaction",
            "stage": stage,
            "before": before,
            "after": after,
            "items": items,
        }))
    }

    /// Append `record` with its id and parent, flushed before returning.
    fn write(&mut self, mut record: Value) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("could not open {}", self.path.display()))?;
        if !self.started {
            let mut header = serde_json::to_value(&self.header)?;
            header["type"] = json!("header");
            writeln!(file, "{header}")?;
            self.started = true;
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        record["id"] = json!(id);
        record["parent_id"] = json!(self.last);
        writeln!(file, "{record}")?;
        file.flush()?;
        self.last = Some(id);
        Ok(())
    }

    /// The child transcript an `agent` tool result points at, if it exists.
    fn sidechain(&self, item: &Value) -> Option<PathBuf> {
        if item.get("type").and_then(Value::as_str) != Some("function_call_output") {
            return None;
        }
        let output = item.get("output").and_then(Value::as_str)?;
        let id = output.strip_prefix("child ")?.split(' ').next()?;
        let path = self.children.join(format!("child-{id}.jsonl"));
        path.exists().then_some(path)
    }
}

/// A session read back from disk.
#[derive(Debug)]
pub struct Loaded {
    /// The header as it was written, which is the model the session opened on.
    pub header: Header,
    /// The model and effort the session was last on, which is what its cached prefix
    /// and its encrypted reasoning belong to. The header's until a `/model` switch was
    /// recorded after it.
    pub model: String,
    pub effort: String,
    pub items: Vec<Value>,
    /// The id of the last record kept.
    pub last: Option<String>,
    /// Bytes of the file that hold the header and the records kept.
    pub len: u64,
    pub warnings: Vec<String>,
}

/// Read a session file, applying compactions as they come. A truncated last line is
/// skipped, and so is a trailing function call whose output never landed, each with a
/// warning.
pub fn load(path: &Path) -> Result<Loaded> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let mut lines = text.split_inclusive('\n').peekable();
    let first = lines.next().unwrap_or_default();
    let header: Header =
        serde_json::from_str(first).with_context(|| format!("{}: bad header", path.display()))?;
    // Each record's id and where it ends; each item with the record it came from.
    let mut records: Vec<(String, u64)> = Vec::new();
    let mut items: Vec<(Value, usize)> = Vec::new();
    let mut len = first.len() as u64;
    let mut warnings = Vec::new();
    // The model the session ends on, which a `/model` record later in the file moves.
    let (mut model, mut effort) = (header.model.clone(), header.effort.clone());
    while let Some(line) = lines.next() {
        let record = serde_json::from_str::<Value>(line)
            .ok()
            .filter(|_| line.ends_with('\n'));
        let Some(record) = record else {
            if lines.peek().is_none() {
                warnings.push(format!("{}: skipped a truncated last line", path.display()));
                break;
            }
            bail!("{}: line {} is not JSON", path.display(), records.len() + 2);
        };
        let id = record.get("id").and_then(Value::as_str).unwrap_or_default();
        let parent = record.get("parent_id").and_then(Value::as_str);
        if parent != records.last().map(|(id, _)| id.as_str()) {
            bail!(
                "{}: record {id} does not follow the one before",
                path.display()
            );
        }
        let index = records.len();
        let kind = record.get("type").and_then(Value::as_str);
        if kind == Some("compaction") {
            let Some(compacted) = record.get("items").and_then(Value::as_array) else {
                bail!("{}: compaction {id} has no items", path.display());
            };
            items = compacted.iter().map(|item| (item.clone(), index)).collect();
        } else if kind == Some("model") {
            let text = |key| record.get(key).and_then(Value::as_str).map(str::to_string);
            let (Some(switched), Some(to)) = (text("model"), text("effort")) else {
                bail!("{}: model record {id} names no model", path.display());
            };
            (model, effort) = (switched, to);
            // As the live switch does: encrypted reasoning belongs to the model that
            // produced it and cannot be replayed to another one.
            items.retain(|(item, _)| item.get("type").and_then(Value::as_str) != Some("reasoning"));
        } else {
            let Some(item) = record.get("item") else {
                bail!("{}: record {id} has no item", path.display());
            };
            items.push((item.clone(), index));
        }
        len += line.len() as u64;
        records.push((id.to_string(), len));
    }
    let keep = answered(items.iter().map(|(item, _)| item));
    if keep < items.len() {
        warnings.push(format!(
            "{}: dropped {} trailing item(s) after a tool call with no result",
            path.display(),
            items.len() - keep
        ));
        records.truncate(items[keep].1);
        items.truncate(keep);
        len = records.last().map_or(first.len() as u64, |(_, end)| *end);
    }
    Ok(Loaded {
        header,
        model,
        effort,
        last: records.last().map(|(id, _)| id.clone()),
        items: items.into_iter().map(|(item, _)| item).collect(),
        len,
        warnings,
    })
}

/// How many leading items to keep so every function call has its output.
fn answered<'a>(items: impl Iterator<Item = &'a Value>) -> usize {
    let mut open: Vec<&str> = Vec::new();
    let mut keep = 0;
    for (index, item) in items.enumerate() {
        let call_id = || item.get("call_id").and_then(Value::as_str).unwrap_or("");
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => open.push(call_id()),
            Some("function_call_output") => open.retain(|id| *id != call_id()),
            _ => {}
        }
        if open.is_empty() {
            keep = index + 1;
        }
    }
    keep
}

/// One line of `bhai sessions`.
#[derive(Debug)]
pub struct Summary {
    pub path: PathBuf,
    /// The id from the file name, which is all a session that will not load has.
    pub id: String,
    pub modified: std::time::SystemTime,
    /// What the file holds, or why [`load`] refused it.
    pub details: Result<Details, String>,
}

/// The part of a session `bhai sessions` prints.
#[derive(Debug)]
pub struct Details {
    pub header: Header,
    pub first: Option<String>,
    pub items: usize,
}

/// Every session under `dir`, most recently written first. One that will not load
/// keeps its place, carrying the reason instead of its contents, so a file that is
/// there is never reported as missing.
pub fn list(dir: &Path) -> Vec<Summary> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut sessions: Vec<Summary> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "jsonl"))
        .filter_map(|path| {
            let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
            let id = path.file_stem()?.to_string_lossy().into_owned();
            let details = load(&path)
                .map(|loaded| Details {
                    first: loaded.items.iter().find_map(user_text).map(truncate),
                    items: loaded.items.len(),
                    header: loaded.header,
                })
                .map_err(|e| format!("{e:#}"));
            Some(Summary {
                id,
                details,
                path,
                modified,
            })
        })
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
    sessions
}

/// The session to resume: `id` (or a unique prefix of it), else the latest. A file
/// that will not load is matched like any other, so the reason reaches the user.
pub fn find(dir: &Path, id: Option<&str>) -> Result<Loaded> {
    let sessions = list(dir);
    let found: Vec<&Summary> = match id {
        Some(id) => sessions.iter().filter(|s| s.id.starts_with(id)).collect(),
        None => sessions
            .iter()
            .filter(|s| s.details.is_ok())
            .take(1)
            .collect(),
    };
    match (found.as_slice(), id) {
        ([one], _) => load(&one.path),
        ([], Some(id)) => bail!("no session `{id}` in {}", dir.display()),
        ([], None) => bail!("no sessions in {}", dir.display()),
        (_, Some(id)) => bail!("`{id}` matches more than one session"),
        _ => unreachable!("the latest is one session"),
    }
}

/// `bhai sessions`: one line per session, each starting with the flag that resumes it.
pub fn report(sessions: &[Summary]) -> String {
    if sessions.is_empty() {
        return "no saved sessions\n".to_string();
    }
    sessions
        .iter()
        .map(|s| match &s.details {
            Ok(d) => format!(
                "--resume {}  {}  {:<10} {:>4} items  {}\n",
                d.header.session,
                d.header.created.format("%Y-%m-%d %H:%M"),
                d.header.identity,
                d.items,
                d.first.as_deref().unwrap_or("")
            ),
            Err(e) => format!("--resume {}  (unreadable: {e})\n", s.id),
        })
        .collect()
}

/// What a finished session prints so it can be picked up again. `newest` drops the id,
/// since a bare `--resume` continues the latest session of this project.
pub fn hint(id: &str, items: usize, newest: bool) -> String {
    let command = match newest {
        true => "bhai --resume".to_string(),
        false => format!("bhai --resume {id}"),
    };
    format!("bhai: session {id} saved ({items} items)\nbhai: resume it with: {command}\n")
}

/// [`hint`] for session `id` under `dir`, or `None` when it saved nothing to resume.
pub fn exit_hint(dir: &Path, id: &str) -> Option<String> {
    let items = load(&path(dir, id)).ok()?.items.len();
    if items == 0 {
        return None;
    }
    // The newest that loads, since that is the one a bare `--resume` picks.
    let newest = list(dir)
        .iter()
        .find(|s| s.details.is_ok())
        .is_some_and(|s| s.id == id);
    Some(hint(id, items, newest))
}

/// The text of a user message item.
pub fn user_text(item: &Value) -> Option<String> {
    if item.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    item.get("content")?
        .as_array()?
        .iter()
        .find_map(|part| part.get("text").and_then(Value::as_str))
        .map(str::to_string)
}

fn truncate(text: String) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(FIRST_CHARS) {
        Some((cut, _)) => format!("{}...", &line[..cut]),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::temp_dir;

    fn header(id: &str) -> Header {
        Header::new(id, "general", "gpt-5", "high", Path::new("/work"))
    }

    fn items() -> Vec<Value> {
        vec![
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi \u{1F600} \"there\""}]}),
            json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "gAAA+/==", "summary": []}),
            json!({"type": "function_call", "call_id": "c1", "name": "bash", "arguments": "{\"command\":\"ls\"}"}),
            json!({"type": "function_call_output", "call_id": "c1", "output": "a\nb\n"}),
            json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done", "annotations": []}]}),
        ]
    }

    fn write(dir: &Path, id: &str, items: &[Value]) -> PathBuf {
        let mut writer = Writer::create(dir, header(id));
        for item in items {
            writer.append(item).unwrap();
        }
        path(dir, id)
    }

    #[test]
    fn a_session_reloads_byte_for_byte() {
        let dir = temp_dir();
        let path = write(&dir, "s1", &items());
        let loaded = load(&path).unwrap();
        assert_eq!(
            (
                loaded.header.session.as_str(),
                loaded.header.effort.as_str()
            ),
            ("s1", "high")
        );
        let text = |items: &[Value]| items.iter().map(Value::to_string).collect::<Vec<_>>();
        assert_eq!(text(&loaded.items), text(&items()));
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.len, std::fs::metadata(&path).unwrap().len());

        // Each record points at the one before it, and a resume continues the chain.
        let mut writer = Writer::resume(&dir, &loaded).unwrap();
        writer.append(&items()[0]).unwrap();
        assert_eq!(load(&path).unwrap().items.len(), 6);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_truncated_last_line_is_skipped_and_cut_on_resume() {
        let dir = temp_dir();
        let path = write(&dir, "s1", &items()[..1]);
        let good = std::fs::metadata(&path).unwrap().len();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(file, "{{\"type\":\"item\",\"id\":\"x\",\"par").unwrap();

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.items.len(), 1);
        assert_eq!(loaded.len, good);
        assert!(loaded.warnings[0].contains("truncated"));
        Writer::resume(&dir, &loaded)
            .unwrap()
            .append(&items()[4])
            .unwrap();
        let reloaded = load(&path).unwrap();
        assert_eq!(reloaded.items.len(), 2);
        assert!(reloaded.warnings.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_switch_is_recorded_so_a_resume_comes_back_on_that_model() {
        let dir = temp_dir();
        let path = write(&dir, "s1", &items());
        let loaded = load(&path).unwrap();
        assert_eq!(
            (loaded.model.as_str(), loaded.effort.as_str()),
            ("gpt-5", "high")
        );

        let mut writer = Writer::resume(&dir, &loaded).unwrap();
        writer.model("ollama:gemma4:e4b", "low", "abc").unwrap();
        writer.append(&items()[0]).unwrap();
        let loaded = load(&path).unwrap();
        // The header still says what the session opened on; the session ends elsewhere.
        assert_eq!(loaded.header.model, "gpt-5");
        assert_eq!(
            (loaded.model.as_str(), loaded.effort.as_str()),
            ("ollama:gemma4:e4b", "low")
        );
        // A switch is not an item, and it drops the reasoning of the model before it.
        assert_eq!(loaded.items.len(), 5);
        assert!(loaded.warnings.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_switch_drops_the_reasoning_the_model_before_it_produced() {
        let dir = temp_dir();
        let path = write(&dir, "s1", &items());
        let loaded = load(&path).unwrap();
        assert!(loaded.items.iter().any(|item| item["type"] == "reasoning"));

        let mut writer = Writer::resume(&dir, &loaded).unwrap();
        writer.model("ollama:gemma4:e4b", "low", "abc").unwrap();
        writer.append(&items()[1]).unwrap();
        let loaded = load(&path).unwrap();
        let reasoning = loaded
            .items
            .iter()
            .filter(|item| item["type"] == "reasoning")
            .count();
        assert_eq!(reasoning, 1, "only what the model now in use produced");
        assert_eq!(loaded.items.len(), 5);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_switch_before_the_first_item_is_the_header_itself() {
        let dir = temp_dir();
        let mut writer = Writer::create(&dir, header("s1"));
        writer.model("gpt-5.5", "xhigh", "abc").unwrap();
        writer.append(&items()[0]).unwrap();
        let loaded = load(&path(&dir, "s1")).unwrap();
        assert_eq!(loaded.header.model, "gpt-5.5");
        assert_eq!(loaded.model, "gpt-5.5");
        assert_eq!(loaded.items.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_compaction_replaces_the_history_before_it() {
        let dir = temp_dir();
        let path = write(&dir, "s1", &items());
        let loaded = load(&path).unwrap();
        let mut writer = Writer::resume(&dir, &loaded).unwrap();
        let compacted = [items()[0].clone(), items()[4].clone()];
        writer.compact("summary", 50, 10, &compacted).unwrap();
        writer.append(&items()[0]).unwrap();
        writer.append(&items()[2]).unwrap();

        // The unanswered call after the compaction is dropped, and a resume cuts it off.
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.items, [&compacted[..], &items()[..1]].concat());
        assert!(loaded.warnings[0].contains("dropped 1"));
        Writer::resume(&dir, &loaded)
            .unwrap()
            .append(&items()[4])
            .unwrap();
        let reloaded = load(&path).unwrap();
        assert_eq!(reloaded.items.len(), 4);
        assert!(reloaded.warnings.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_call_without_its_output_is_dropped() {
        let dir = temp_dir();
        let path = write(&dir, "s1", &items()[..3]);
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.items.len(), 2);
        assert!(loaded.warnings[0].contains("dropped 1"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sessions_list_newest_first_and_resolve_by_prefix() {
        let dir = temp_dir();
        let old = write(&dir, "aaa111", &items());
        write(&dir, "bbb222", &items()[..1]);
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(past)
            .unwrap();
        std::fs::write(dir.join("broken.jsonl"), "not json\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(dir.join("broken.jsonl"))
            .unwrap()
            .set_modified(past - std::time::Duration::from_secs(60))
            .unwrap();

        let sessions = list(&dir);
        let ids: Vec<_> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["bbb222", "aaa111", "broken"]);
        fn details(s: &Summary) -> &Details {
            s.details.as_ref().unwrap()
        }
        assert_eq!(
            details(&sessions[0]).first.as_deref(),
            Some("hi \u{1F600} \"there\"")
        );
        assert_eq!(details(&sessions[1]).items, 5);
        assert!(report(&sessions).contains("--resume aaa111"));
        assert!(report(&sessions).contains("5 items"));

        // A file that will not load is still listed and still resumable by name, so
        // the reason reaches the user instead of the session looking lost.
        assert!(report(&sessions).contains("--resume broken  (unreadable: "));
        assert!(report(&sessions).contains("bad header"));
        let refused = find(&dir, Some("broken")).unwrap_err().to_string();
        assert!(refused.contains("bad header"), "{refused}");

        assert_eq!(find(&dir, None).unwrap().header.session, "bbb222");
        assert_eq!(find(&dir, Some("aaa")).unwrap().header.session, "aaa111");
        assert!(find(&dir, Some("zzz")).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_exit_hint_names_the_session_and_drops_the_id_when_it_is_newest() {
        assert_eq!(
            hint("abc", 7, false),
            "bhai: session abc saved (7 items)\nbhai: resume it with: bhai --resume abc\n"
        );
        assert_eq!(
            hint("abc", 1, true),
            "bhai: session abc saved (1 items)\nbhai: resume it with: bhai --resume\n"
        );

        // A session that never wrote an item leaves no file, so there is nothing to say.
        let dir = temp_dir();
        assert_eq!(exit_hint(&dir, "s1"), None);
        let old = write(&dir, "s1", &items());
        write(&dir, "s2", &items()[..1]);
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(past)
            .unwrap();
        assert_eq!(exit_hint(&dir, "s1").unwrap(), hint("s1", 5, false));
        assert_eq!(exit_hint(&dir, "s2").unwrap(), hint("s2", 1, true));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_agent_result_points_at_its_sidechain() {
        let dir = temp_dir();
        let child = dir.join("s1").join("child-abc123.jsonl");
        std::fs::create_dir_all(child.parent().unwrap()).unwrap();
        std::fs::write(&child, "").unwrap();
        let output = json!({"type": "function_call_output", "call_id": "c", "output": "child abc123 (general) finished"});
        let path = write(&dir, "s1", &[output]);
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("child-abc123.jsonl"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
