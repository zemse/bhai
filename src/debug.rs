//! `/export-debug`: one markdown file holding what it takes to work out what a session
//! did, so a report is a file to hand over rather than a screenshot of the transcript.
//!
//! What goes in is what the loop's own questions turn on: which model and mode it ran
//! under, what the permission layer decided and why, what the judge was asked and
//! answered, where the context went, and the transcript itself in order. Nothing is
//! collected that the session did not already hold: no environment variables, no
//! credentials, no auth files. The home directory is written as `~` throughout, which is
//! the one identifying thing this would otherwise add on its own; what the user typed is
//! left as they typed it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::compact::Limits;
use crate::entries::Entry;
use crate::limits::RateLimits;
use crate::permissions::Mode;
use crate::profile::Profile;
use crate::session::{ChildLog, State};

/// Characters of one transcript entry the file carries before the rest is counted off.
/// Well past any command or answer; it is there so a session that catted a binary does
/// not produce a file nobody can open.
const ENTRY_CLIP: usize = 20_000;
/// What the context section says when the agent did not answer the profile request.
const PROFILE_MISSING: &str = "not available: the agent did not answer, which is what \
happens while a turn runs and when none is attached";
/// Judge decisions the file carries, newest last. A denial the user is asking about is
/// a recent one, and each line holds the whole summary the judge was sent.
const JUDGE_LINES: usize = 50;

/// What the TUI knows about the session, gathered for one export.
pub struct Bundle {
    pub version: &'static str,
    pub session_id: String,
    pub session_file: Option<PathBuf>,
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
    pub branch: Option<String>,
    pub terminal: Option<(u16, u16)>,
    pub backend: String,
    pub state: State,
    /// The mode the config asked for, which trust may have held back.
    pub wanted_mode: Mode,
    pub trusted: bool,
    pub limits: Limits,
    /// `/permissions`, which carries the rules, their sources and the judge's budget.
    pub permissions: String,
    pub skills: String,
    pub mcp: String,
    pub workflows: String,
    /// The judge's log, as its JSONL lines.
    pub judged: Vec<Value>,
    /// Every child agent of the session, oldest first, with what its pane showed.
    pub children: Vec<ChildLog>,
    /// Where the children's full histories are, one `child-<id>.jsonl` each.
    pub sidechains: Option<PathBuf>,
    /// The transcript in order.
    pub entries: Vec<Entry>,
    /// `/context`, when the profile could be built.
    pub profile: Option<Profile>,
}

/// Write the bundle as `bhai-debug-<timestamp>.md` in `dir`, and return its path.
pub fn export(bundle: &Bundle, dir: &Path) -> Result<PathBuf> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = dir.join(format!("bhai-debug-{stamp}.md"));
    let checkpoint = bundle
        .session_file
        .as_ref()
        .map(|file| file.with_extension("").join("authorization.json"));
    let diagnostics = authorization_diagnostics(
        checkpoint.as_deref(),
        &crate::profile::debug_dir().join("authorization.jsonl"),
    );
    crate::sessions::private_write(&path, &bundle.markdown(&diagnostics))
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

/// Read the last `JUDGE_LINES` decisions of the judge log, oldest first.
pub fn judge_log(path: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(JUDGE_LINES)..]
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

impl Bundle {
    /// The line the transcript shows once the file is written: the path, and enough of
    /// what went in for the user to see whether it is worth sending on.
    pub fn summary(&self, path: &Path) -> String {
        let rules = self
            .permissions
            .lines()
            .filter(|line| line.starts_with("  "))
            .count();
        format!(
            "session debug written to {}\n{} transcript entries, {} child agents, {} judge \
decisions, {rules} permission rules, context {}",
            path.display(),
            self.entries.len(),
            self.children.len(),
            self.judged.len(),
            match self.profile.is_some() {
                true => "profiled",
                false => "not profiled: the agent did not answer",
            }
        )
    }

    fn markdown(&self, diagnostics: &str) -> String {
        let mut out = String::new();
        self.session(&mut out);
        self.totals(&mut out);
        section(&mut out, "permissions", self.permissions.trim());
        self.judge(&mut out);
        section(&mut out, "authorization diagnostics", diagnostics);
        section(&mut out, "skills", self.skills.trim());
        section(&mut out, "mcp servers", self.mcp.trim());
        section(&mut out, "workflows", self.workflows.trim());
        match &self.profile {
            Some(profile) => section(&mut out, "context", profile.markdown().trim()),
            None => section(&mut out, "context", PROFILE_MISSING),
        }
        self.transcript(&mut out);
        self.children(&mut out);
        // One pass at the end, so it reaches the transcript and the judge summaries too.
        match &self.home {
            Some(home) => out.replace(&home.display().to_string(), "~"),
            None => out,
        }
    }

    fn session(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "# bhai session debug\n\nWritten {}. Paths under the home directory are \
written as `~`, which is the only thing rewritten: the transcript and the judge \
summaries below hold whatever this session saw, so read them before sending this on.\
\n\n## session\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z")
        );
        let s = &self.state;
        let mut row = |name: &str, value: String| {
            let _ = writeln!(out, "- {name}: {value}");
        };
        row("bhai", self.version.to_string());
        row("session", self.session_id.clone());
        if let Some(file) = &self.session_file {
            row("session file", file.display().to_string());
        }
        row("identity", s.identity.clone());
        row("model", format!("{} (effort {})", s.model, s.effort));
        row("backend", self.backend.clone());
        row(
            "mode",
            match (self.state.mode == self.wanted_mode, self.trusted) {
                (true, true) => format!("{} (project trusted)", s.mode),
                (true, false) => format!("{} (project not trusted)", s.mode),
                (false, _) => format!(
                    "{} ({} asked for, held back until the project is trusted)",
                    s.mode, self.wanted_mode
                ),
            },
        );
        row("cwd", self.cwd.display().to_string());
        row(
            "branch",
            self.branch.clone().unwrap_or_else(|| "not a repo".into()),
        );
        row(
            "platform",
            format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        );
        if let Some((w, h)) = self.terminal {
            row("terminal", format!("{w}x{h}"));
        }
        row("working", s.working.to_string());
        if !s.queued.is_empty() {
            row("queued prompts", format!("{:?}", s.queued));
        }
        if let Some(pending) = &s.pending {
            row(
                "waiting on approval",
                format!("{} {}", pending.tool, pending.command),
            );
        }
    }

    fn totals(&self, out: &mut String) {
        let s = &self.state;
        let _ = writeln!(out, "\n## totals\n");
        let _ = writeln!(
            out,
            "- calls: {}\n- input: {} ({} cached)\n- output: {} ({} reasoning)",
            s.calls, s.input_tokens, s.cached_tokens, s.output_tokens, s.reasoning_tokens
        );
        if let Some(last) = &s.last_usage {
            let _ = writeln!(
                out,
                "- last call: {} in ({} cached) / {} out ({} reasoning)",
                last.input, last.cached, last.output, last.reasoning
            );
        }
        let _ = writeln!(
            out,
            "- children: {} in / {} out\n- judge: {} in / {} out",
            s.children.input, s.children.output, s.judge.input, s.judge.output
        );
        let _ = writeln!(
            out,
            "- context window: {}, compacting at {:.0}%",
            self.limits
                .window
                .map_or("the model's own".to_string(), |w| w.to_string()),
            self.limits.compact_at * 100.0
        );
        if let Some(broke) = &s.last_cache_break {
            let _ = writeln!(out, "- prompt cache last broke on: {}", broke.field);
        }
        if let Some(rate) = &s.rate_limits {
            let _ = writeln!(out, "- rate limits: {}", windows(rate));
        }
        for child in &self.children {
            let row = &child.row;
            let _ = writeln!(
                out,
                "- child {} ({}, {:?}): {}",
                row.id, row.identity, row.state, row.description
            );
        }
    }

    fn judge(&self, out: &mut String) {
        let _ = writeln!(out, "\n## judge decisions\n");
        if self.judged.is_empty() {
            let _ = writeln!(
                out,
                "None logged. The judge only runs in `auto` mode in a trusted project.\n"
            );
            return;
        }
        let _ = writeln!(
            out,
            "The last {} of them, oldest first. Each summary is exactly what the judge \
was sent.\n",
            self.judged.len()
        );
        for line in &self.judged {
            let field = |key: &str| {
                line.get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            let number = |key: &str| line.get(key).and_then(Value::as_u64).unwrap_or(0);
            let verdict = match field("verdict").as_str() {
                "" => format!("no verdict ({})", field("error")),
                name => format!("{name}: {}", field("reason")),
            };
            let from = match field("agent").as_str() {
                "" => String::new(),
                id => format!(" (child {id})"),
            };
            let _ = writeln!(
                out,
                "### {}{from} — {verdict}\n\n{}ms, {} in / {} out\n\n```\n{}\n```\n",
                field("timestamp"),
                number("latency_ms"),
                number("input"),
                number("output"),
                field("summary").trim()
            );
        }
    }

    fn transcript(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "\n## transcript\n\n{} entries, in order. Anything over {ENTRY_CLIP} \
characters is cut, with what was left off counted at the end of it.\n",
            self.entries.len()
        );
        entries(out, &self.entries, "###");
    }

    /// Each child's own transcript, which the session's leaves out: it shows the
    /// `agent` call and the report, and nothing the child did in between.
    fn children(&self, out: &mut String) {
        let _ = writeln!(out, "\n## children\n");
        if self.children.is_empty() {
            let _ = writeln!(out, "None started in this session.\n");
            return;
        }
        let _ = writeln!(
            out,
            "{} child agents, oldest first, each with what its pane showed. The first \
entry is the brief it was given.\n",
            self.children.len()
        );
        for child in &self.children {
            let row = &child.row;
            let _ = writeln!(
                out,
                "### child {} ({}): {}\n\n- state: {:?}\n- events heard: {}",
                row.id, row.identity, row.description, row.state, row.steps
            );
            if let Some(dir) = &self.sidechains {
                let file = dir.join(format!("child-{}.jsonl", row.id));
                if file.exists() {
                    let _ = writeln!(out, "- full history: {}", file.display());
                }
            }
            let _ = writeln!(out);
            entries(out, &child.entries, "####");
        }
    }
}

fn authorization_diagnostics(checkpoint: Option<&Path>, log: &Path) -> String {
    let mut out = String::from("Saved authorization state (including pending sources):\n");
    match checkpoint {
        Some(path) => {
            let _ = writeln!(out, "{}", path.display());
            match std::fs::read_to_string(path) {
                Ok(text) => match serde_json::from_str::<Value>(&text) {
                    Ok(value) => {
                        let _ = writeln!(
                            out,
                            "{}",
                            serde_json::to_string_pretty(&value)
                                .unwrap_or_else(|_| value.to_string())
                        );
                    }
                    Err(error) => {
                        let _ = writeln!(out, "Invalid checkpoint JSON: {error}\n{text}");
                    }
                },
                Err(error) => {
                    let _ = writeln!(out, "Not available: {error}");
                }
            }
        }
        None => out.push_str("Not available: no session file.\n"),
    }
    let _ = writeln!(
        out,
        "\nAuthorization stage log: {}\nShared debug log, not filtered to this session; last {JUDGE_LINES} nonempty records, oldest first. Requests and replies are included in full.",
        log.display()
    );
    match std::fs::read_to_string(log) {
        Ok(text) => {
            let lines: Vec<_> = text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect();
            if lines.is_empty() {
                out.push_str("None logged.\n");
            }
            for line in &lines[lines.len().saturating_sub(JUDGE_LINES)..] {
                match serde_json::from_str::<Value>(line) {
                    Ok(value) => {
                        let _ = writeln!(
                            out,
                            "{}\n",
                            serde_json::to_string_pretty(&value)
                                .unwrap_or_else(|_| value.to_string())
                        );
                    }
                    Err(error) => {
                        let _ = writeln!(out, "Invalid log JSON: {error}\n{line}\n");
                    }
                }
            }
        }
        Err(error) => {
            let _ = writeln!(out, "Not available: {error}");
        }
    }
    out
}

/// The used percent of each rate-limit window, as the status bar names them.
fn windows(rate: &RateLimits) -> String {
    rate.windows()
        .map(|w| format!("{} {:.0}%", w.label(), w.used_percent))
        .collect::<Vec<_>>()
        .join(", ")
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(ENTRY_CLIP) {
        Some((end, _)) => format!(
            "{}\n[{} more characters]",
            &text[..end],
            text.chars().count() - ENTRY_CLIP
        ),
        None => text.to_string(),
    }
}

/// A transcript's entries in order, each under a `level` heading.
fn entries(out: &mut String, entries: &[Entry], level: &str) {
    for (index, entry) in entries.iter().enumerate() {
        let kind = match entry {
            Entry::Command { tool, .. } => format!("command ({tool})"),
            other => other.kind().to_string(),
        };
        let _ = writeln!(
            out,
            "{level} {index} {kind}\n\n```\n{}\n```\n",
            clip(entry.text())
        );
    }
}

fn section(out: &mut String, name: &str, body: &str) {
    let _ = writeln!(out, "\n## {name}\n\n```\n{body}\n```\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Usage;

    fn bundle(entries: Vec<Entry>) -> Bundle {
        Bundle {
            version: "0.0.1",
            session_id: "abc123".to_string(),
            session_file: Some(PathBuf::from("/home/u/repo/.bhai/sessions/abc123.jsonl")),
            cwd: PathBuf::from("/home/u/repo"),
            home: Some(PathBuf::from("/home/u")),
            branch: Some("main".to_string()),
            terminal: Some((120, 40)),
            backend: "codex".to_string(),
            state: State {
                model: "gpt-5.5".to_string(),
                effort: "medium".to_string(),
                fast: false,
                identity: "general".to_string(),
                mode: Mode::Auto,
                working: false,
                queued: Vec::new(),
                input_tokens: 400,
                cached_tokens: 300,
                cache_write_tokens: 0,
                output_tokens: 20,
                reasoning_tokens: 5,
                calls: 2,
                last_usage: None,
                children: Usage::default(),
                judge: Usage::default(),
                last_cache_break: None,
                rate_limits: None,
                goal: None,
                plan: None,
                pending: None,
                entries: Vec::new(),
                seq: 0,
            },
            wanted_mode: Mode::Auto,
            trusted: true,
            limits: Limits::default(),
            permissions: "permission mode: auto".to_string(),
            skills: "no skills".to_string(),
            mcp: "no servers".to_string(),
            workflows: "no workflows".to_string(),
            judged: Vec::new(),
            children: Vec::new(),
            sidechains: None,
            entries,
            profile: None,
        }
    }

    /// The one identifying thing the export would add by itself is the path it writes
    /// down, so the home directory goes everywhere it appears, transcript included.
    #[test]
    fn the_home_directory_is_written_as_a_tilde() {
        let mut b = bundle(vec![Entry::Command {
            tool: "bash".to_string(),
            summary: "cat /home/u/.gnupg/gpg-agent.conf".to_string(),
        }]);
        b.judged = vec![serde_json::json!({
            "timestamp": "2026-09-23T12:00:00+05:30",
            "verdict": "deny",
            "reason": "outside the project root",
            "summary": "target: /home/u/.zshrc",
            "latency_ms": 900,
        })];
        let out = b.markdown("");
        assert!(!out.contains("/home/u"), "{out}");
        assert!(out.contains("~/repo/.bhai/sessions/abc123.jsonl"), "{out}");
        assert!(out.contains("cat ~/.gnupg/gpg-agent.conf"), "{out}");
        assert!(out.contains("deny: outside the project root"), "{out}");
    }

    /// A session that catted something enormous still produces a file that opens.
    #[test]
    fn a_huge_entry_is_cut_and_says_by_how_much() {
        let out = bundle(vec![Entry::Output("x".repeat(ENTRY_CLIP + 500))]).markdown("");
        assert!(out.contains("[500 more characters]"), "cut");
        assert!(out.len() < ENTRY_CLIP + 6_000, "{} bytes", out.len());
    }

    /// Every section is there whether or not the session filled it, so a reader can tell
    /// "nothing happened" from "this export does not carry it".
    #[test]
    fn an_empty_session_still_names_every_section() {
        let out = bundle(Vec::new()).markdown("");
        for heading in [
            "## session",
            "## totals",
            "## permissions",
            "## judge decisions",
            "## authorization diagnostics",
            "## skills",
            "## mcp servers",
            "## workflows",
            "## context",
            "## transcript",
            "## children",
        ] {
            assert!(out.contains(heading), "{heading} missing from {out}");
        }
        assert!(out.contains(PROFILE_MISSING));
        assert!(out.contains("None logged."));
        assert!(out.contains("None started in this session."));
    }

    #[test]
    fn authorization_stages_and_pending_sources_are_exported_and_redacted() {
        let dir = std::env::temp_dir().join(format!("bhai-debug-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let checkpoint = dir.as_path().join("authorization.json");
        let log = dir.as_path().join("authorization.jsonl");
        std::fs::write(
            &checkpoint,
            r#"{"revision":5,"entries":[],"pending":[{"id":2,"text":"hide monitors"},{"id":4,"text":"commit them"}]}"#,
        )
        .unwrap();
        let records = [
            serde_json::json!({"stage":"judge-authorization-extract", "request":{"message":{"text":"hide monitors", "reference":"/home/u/repo"}}, "reply":{"candidates":[]}}),
            serde_json::json!({"stage":"judge-authorization-merge", "request":{"candidates":[]}, "reply":{"changes":[], "error":"Candidates omit the request"}}),
        ];
        std::fs::write(
            &log,
            records
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let diagnostics = authorization_diagnostics(Some(&checkpoint), &log);
        let out = bundle(Vec::new()).markdown(&diagnostics);
        for text in [
            "judge-authorization-extract",
            "judge-authorization-merge",
            "\"request\"",
            "\"reply\"",
            "\"pending\"",
            "hide monitors",
            "commit them",
            "Candidates omit the request",
            "~/repo",
            "Shared debug log, not filtered to this session",
        ] {
            assert!(out.contains(text), "{text} missing from {out}");
        }
        assert!(!out.contains("/home/u"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn export_loads_the_sessions_authorization_checkpoint() {
        let dir = std::env::temp_dir().join(format!("bhai-debug-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let mut b = bundle(Vec::new());
        b.session_file = Some(dir.as_path().join("abc123.jsonl"));
        std::fs::create_dir(dir.as_path().join("abc123")).unwrap();
        std::fs::write(
            dir.as_path().join("abc123/authorization.json"),
            r#"{"pending":[{"text":"commit them"}],"revision":5}"#,
        )
        .unwrap();
        let file = export(&b, dir.as_path()).unwrap();
        let out = std::fs::read_to_string(file).unwrap();
        assert!(out.contains("\"pending\""));
        assert!(out.contains("commit them"));
        assert!(out.contains("Authorization stage log:"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn authorization_log_is_bounded_and_bad_records_remain_visible() {
        let dir = std::env::temp_dir().join(format!("bhai-debug-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let log = dir.as_path().join("authorization.jsonl");
        let mut records = String::from("{\"stage\":\"oldest omitted\"}\n");
        for index in 0..JUDGE_LINES - 1 {
            records.push_str(&format!("{{\"index\":{index}}}\n"));
        }
        records.push_str("invalid record\n\n");
        std::fs::write(&log, records).unwrap();
        let out = authorization_diagnostics(None, &log);
        assert!(!out.contains("oldest omitted"));
        assert!(out.contains("\"index\": 0"));
        assert!(out.contains("Invalid log JSON:"));
        assert!(out.contains("invalid record"));
        assert!(out.contains("Not available: no session file."));
        let out = authorization_diagnostics(Some(&log), &dir.as_path().join("missing"));
        assert!(out.contains("Invalid checkpoint JSON:"));
        assert!(out.contains("Not available:"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A child's work is not in the session's transcript, so the export carries each
    /// child's own, and says which judge decisions were its.
    #[test]
    fn every_child_is_written_with_its_own_transcript() {
        use crate::session::{ChildRow, ChildState};

        let mut b = bundle(Vec::new());
        b.children = vec![ChildLog {
            row: ChildRow {
                id: "74b01e".to_string(),
                identity: "general".to_string(),
                model: "gpt-5.5".to_string(),
                description: "build the circuits".to_string(),
                state: ChildState::Done,
                steps: 14,
                last: std::time::Instant::now(),
            },
            entries: vec![
                Entry::User("create the branch".to_string()),
                Entry::Command {
                    tool: "bash".to_string(),
                    summary: "git status".to_string(),
                },
            ],
        }];
        b.judged = vec![serde_json::json!({
            "timestamp": "2026-09-24T12:00:00+05:30",
            "agent": "74b01e",
            "verdict": "approve",
            "reason": "reads the repo",
            "summary": "target: git status",
        })];
        let out = b.markdown("");
        let children = &out[out.find("## children").unwrap()..];
        assert!(children.contains("### child 74b01e (general): build the circuits"));
        assert!(children.contains("create the branch"), "{children}");
        assert!(children.contains("#### 1 command (bash)"), "{children}");
        assert!(
            out.contains("(child 74b01e) — approve: reads the repo"),
            "{out}"
        );
    }
}
