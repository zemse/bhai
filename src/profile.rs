//! Token profiler: where the context goes, item by item, and a per-call usage log.
//!
//! Each row takes its tokens from the best method available: exact (from the usage of
//! consecutive model calls), tokenized (the model's tokenizer) or estimated (bytes/4,
//! scaled by how far that estimate was off for the last call).

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};

use crate::agent::ChildUsage;
use crate::cache::Hit;
use crate::client::Usage;
use crate::prompt::SystemPrompt;
use crate::tokens::{self, Tokenizer};

/// Items `/context` prints into the transcript.
const TOP_ITEMS: usize = 5;
/// Label of the row for what the server adds around the instructions and tools.
const FRAMING: &str = "server framing (exact minus tokenized)";

/// One finished model call: its usage, the history items it was sent, and how many
/// output items it appended right after them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Call {
    pub usage: Usage,
    pub sent: usize,
    pub outputs: usize,
}

/// History items `start..end` cost exactly `tokens` input tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub tokens: u64,
    /// For a call's own output items: how many of `tokens` were reasoning.
    pub reasoning: Option<u64>,
    /// The delta came out negative, so earlier reasoning was likely dropped server-side
    /// and `tokens` (clamped to zero) is not usable.
    pub reasoning_dropped: bool,
}

/// How a row's tokens were counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    Exact,
    Tokenized,
    Estimated,
    /// Encrypted reasoning with no usage to go by.
    Unknown,
}

impl Method {
    fn name(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Tokenized => "tokenized",
            Self::Estimated => "estimated",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Profile {
    /// The session's identity.
    pub identity: String,
    pub total_bytes: usize,
    /// Bytes/4 over every item.
    pub estimated_tokens: u64,
    /// Best available tokens over every item.
    pub tokens: u64,
    pub calibration: Option<Calibration>,
    /// Call deltas that came out negative.
    pub reasoning_dropped: usize,
    /// Totals per category, largest first.
    pub categories: Vec<Category>,
    /// Every component of the context, largest first.
    pub items: Vec<Item>,
    /// Child agents run so far, with their own usage; not part of this context.
    pub children: Vec<ChildUsage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Calibration {
    /// Input tokens the backend reported for the last call.
    pub input_tokens: u64,
    /// What bytes/4 estimated for that same call.
    pub estimated_tokens: u64,
    /// Real divided by estimated; applied to estimated rows only.
    pub factor: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub label: String,
    pub category: &'static str,
    pub bytes: usize,
    pub method: Method,
    pub tokens: u64,
    /// Bytes/4.
    pub estimated: u64,
    /// Percent of all context bytes.
    pub share: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Category {
    pub name: &'static str,
    pub items: usize,
    pub bytes: usize,
    pub tokens: u64,
    pub estimated: u64,
    pub share: f64,
}

/// The exact spans of history that consecutive calls pin down. Call N's own output
/// costs its output tokens; the items appended after it cost
/// input(N+1) - input(N) - output(N).
pub fn attribute(calls: &[Call]) -> Vec<Span> {
    let mut spans = Vec::new();
    for (index, call) in calls.iter().enumerate() {
        if index > 0 {
            let prev = &calls[index - 1];
            let start = prev.sent + prev.outputs;
            if start < call.sent {
                let delta =
                    call.usage.input as i64 - prev.usage.input as i64 - prev.usage.output as i64;
                spans.push(Span {
                    start,
                    end: call.sent,
                    tokens: delta.max(0) as u64,
                    reasoning: None,
                    reasoning_dropped: delta < 0,
                });
            }
        }
        if call.outputs > 0 {
            spans.push(Span {
                start: call.sent,
                end: call.sent + call.outputs,
                tokens: call.usage.output,
                reasoning: Some(call.usage.reasoning),
                reasoning_dropped: false,
            });
        }
    }
    spans
}

/// Break the context of the next request down into its components. `calls` are the
/// model calls of this conversation so far, oldest first.
pub fn build(
    prompt: &SystemPrompt,
    tools: &[Value],
    history: &[Value],
    calls: &[Call],
    tokenizer: &dyn Tokenizer,
) -> Profile {
    let counted = if tokenizer.estimates() {
        Method::Estimated
    } else {
        Method::Tokenized
    };
    let row = |label: String, category, text: &str| Item {
        label,
        category,
        bytes: text.len(),
        method: counted,
        tokens: tokenizer.count(text) as u64,
        estimated: (text.len() as u64).div_ceil(4),
        share: 0.0,
    };

    let text = &prompt.text;
    let appended: usize = prompt.sources.iter().map(|s| s.bytes).sum::<usize>()
        + prompt.skills_bytes
        + prompt.agents_bytes
        + prompt.mcp_bytes;
    // The prompt is laid out as base, sources, skills, agents, mcp.
    let mut at = text.len() - appended;
    let base = text.get(..at).unwrap_or_default();
    let mut next = |bytes: usize| {
        let slice = text.get(at..at + bytes).unwrap_or_default();
        at += bytes;
        slice
    };
    let mut items = vec![row("system prompt".to_string(), "system prompt", base)];
    for source in &prompt.sources {
        let label = format!("instructions: {}", source.label);
        items.push(row(label, "instructions", next(source.bytes)));
    }
    let skills = next(prompt.skills_bytes);
    if !skills.is_empty() {
        let label = format!("skills listing ({})", prompt.skills.len());
        items.push(row(label, "skills", skills));
    }
    let agents = next(prompt.agents_bytes);
    if !agents.is_empty() {
        items.push(row(
            "agent identities listing".to_string(),
            "agents",
            agents,
        ));
    }
    let mcp = next(prompt.mcp_bytes);
    if !mcp.is_empty() {
        let servers = prompt.mcp.as_ref().map_or(0, |hub| {
            hub.servers.iter().filter(|s| !s.tools.is_empty()).count()
        });
        items.push(row(format!("mcp servers listing ({servers})"), "mcp", mcp));
    }
    for tool in tools {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("?");
        items.push(row(
            format!("tool: {name}"),
            "tool schema",
            &tool.to_string(),
        ));
    }
    let fixed = items.len();
    for (index, entry) in history.iter().enumerate() {
        let (category, detail) = classify(entry);
        let label = match detail {
            Some(detail) => format!("#{index} {category}: {detail}"),
            None => format!("#{index} {category}"),
        };
        let mut item = row(label, category, &json_string(entry));
        match tokens::item_text(entry) {
            Some(text) => item.tokens = tokenizer.count(&text) as u64,
            None => (item.method, item.tokens) = (Method::Unknown, 0),
        }
        items.push(item);
    }

    let spans = attribute(calls);
    for span in &spans {
        if !span.reasoning_dropped && span.end <= history.len() {
            apply(&mut items[fixed + span.start..fixed + span.end], span);
        }
    }

    let calibration = calls.last().map(|last| {
        let sent = fixed + last.sent.min(history.len());
        let estimated_tokens: u64 = items[..sent].iter().map(|i| i.estimated).sum();
        Calibration {
            input_tokens: last.usage.input,
            estimated_tokens,
            factor: last.usage.input as f64 / estimated_tokens.max(1) as f64,
        }
    });
    if let Some(c) = &calibration {
        for item in items.iter_mut().filter(|i| i.method == Method::Estimated) {
            item.tokens = scale(item.estimated, c.factor);
        }
    }

    if let Some(first) = calls.first()
        && counted == Method::Tokenized
        && first.sent <= history.len()
    {
        let sent: u64 = items[fixed..fixed + first.sent]
            .iter()
            .map(|i| i.tokens)
            .sum();
        let fixed_tokens: u64 = items[..fixed].iter().map(|i| i.tokens).sum();
        items.push(Item {
            label: FRAMING.to_string(),
            category: "framing",
            bytes: 0,
            method: Method::Exact,
            tokens: first.usage.input.saturating_sub(sent + fixed_tokens),
            estimated: 0,
            share: 0.0,
        });
    }

    let total_bytes: usize = items.iter().map(|i| i.bytes).sum();
    let estimated_tokens = items.iter().map(|i| i.estimated).sum();
    let total_tokens = items.iter().map(|i| i.tokens).sum();
    for item in &mut items {
        item.share = percent(item.bytes, total_bytes);
    }

    let mut categories: Vec<Category> = Vec::new();
    for item in &items {
        match categories.iter_mut().find(|c| c.name == item.category) {
            Some(c) => {
                c.items += 1;
                c.bytes += item.bytes;
                c.tokens += item.tokens;
                c.estimated += item.estimated;
            }
            None => categories.push(Category {
                name: item.category,
                items: 1,
                bytes: item.bytes,
                tokens: item.tokens,
                estimated: item.estimated,
                share: 0.0,
            }),
        }
    }
    for c in &mut categories {
        c.share = percent(c.bytes, total_bytes);
    }

    // Stable sorts, so equal sizes keep their context order.
    items.sort_by_key(|i| std::cmp::Reverse((i.tokens, i.bytes)));
    categories.sort_by_key(|c| std::cmp::Reverse((c.tokens, c.bytes)));
    Profile {
        identity: prompt.identity.name.clone(),
        total_bytes,
        estimated_tokens,
        tokens: total_tokens,
        calibration,
        reasoning_dropped: spans.iter().filter(|s| s.reasoning_dropped).count(),
        categories,
        items,
        children: Vec::new(),
    }
}

/// Share an exact span out over its rows. Reasoning rows split the call's reasoning
/// tokens by size; the rest split what remains by their tokenized counts.
fn apply(rows: &mut [Item], span: &Span) {
    let is_reasoning = |i: &Item| i.category == "reasoning";
    let (text, reasoning) = match span.reasoning {
        Some(r) if rows.iter().any(is_reasoning) => {
            let r = r.min(span.tokens);
            (span.tokens - r, Some(r))
        }
        _ => (span.tokens, None),
    };
    {
        let mut plain: Vec<&mut Item> = rows.iter_mut().filter(|i| !is_reasoning(i)).collect();
        share(&mut plain, text, |i| i.tokens);
    }
    if let Some(r) = reasoning {
        let mut encrypted: Vec<&mut Item> = rows.iter_mut().filter(|i| is_reasoning(i)).collect();
        share(&mut encrypted, r, |i| i.bytes as u64);
    }
}

/// Split `total` over `rows` by `weight`, evenly when every weight is zero; the last
/// row takes the rounding remainder.
fn share(rows: &mut [&mut Item], total: u64, weight: impl Fn(&Item) -> u64) {
    let weights: Vec<u64> = rows.iter().map(|i| weight(i)).collect();
    let sum: u64 = weights.iter().sum();
    let mut left = total;
    let count = rows.len();
    for (index, (item, w)) in rows.iter_mut().zip(weights).enumerate() {
        let part = if index + 1 == count {
            left
        } else if sum == 0 {
            total / count as u64
        } else {
            (total as u128 * w as u128 / sum as u128) as u64
        };
        left -= part;
        item.tokens = part;
        item.method = Method::Exact;
    }
}

impl Profile {
    /// The markdown report written next to the JSON export.
    pub fn markdown(&self) -> String {
        let mut out = String::from("# bhai context\n\n");
        let _ = writeln!(
            out,
            "Identity `{}`: {} items, {} bytes, {} tokens ({} by bytes/4).",
            self.identity,
            self.items.len(),
            self.total_bytes,
            self.tokens,
            self.estimated_tokens
        );
        match &self.calibration {
            Some(c) => {
                let _ = writeln!(
                    out,
                    "Last call: {} real input tokens vs {} estimated, factor {:.2} \
(applied to estimated rows only).",
                    c.input_tokens, c.estimated_tokens, c.factor
                );
            }
            None => out.push_str("No model call yet, so nothing is exact.\n"),
        }
        if self.reasoning_dropped > 0 {
            let _ = writeln!(
                out,
                "{} call deltas were negative (reasoning dropped server-side); \
those rows fall back to tokenized.",
                self.reasoning_dropped
            );
        }

        out.push_str("\n## By category\n\n");
        out.push_str("| category | items | bytes | tokens | est. tokens | share |\n");
        out.push_str("|---|---:|---:|---:|---:|---:|\n");
        for c in &self.categories {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {:.1}% |",
                c.name, c.items, c.bytes, c.tokens, c.estimated, c.share
            );
        }

        out.push_str("\n## Items\n\n");
        out.push_str("| item | category | method | bytes | tokens | est. tokens | share |\n");
        out.push_str("|---|---|---|---:|---:|---:|---:|\n");
        for i in &self.items {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} | {:.1}% |",
                i.label.replace('|', "\\|"),
                i.category,
                i.method.name(),
                i.bytes,
                i.tokens,
                i.estimated,
                i.share
            );
        }

        if !self.children.is_empty() {
            out.push_str("\n## Children\n\n");
            out.push_str("| child | identity | description | input | cached | output |\n");
            out.push_str("|---|---|---|---:|---:|---:|\n");
            for c in &self.children {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {} | {} |",
                    c.id,
                    c.identity,
                    c.description.replace('|', "\\|"),
                    c.input_tokens,
                    c.cached_tokens,
                    c.output_tokens
                );
            }
        }
        out
    }

    /// What `/context` prints: where the export went and the largest items.
    pub fn summary(&self, path: &Path) -> String {
        let mut out = format!(
            "context ({}): {} tokens, written to {} and .md",
            self.identity,
            self.tokens,
            path.display()
        );
        for i in self.items.iter().take(TOP_ITEMS) {
            let _ = write!(
                out,
                "\n{:>5.1}% {:>8} tok  {} ({})",
                i.share,
                i.tokens,
                i.label,
                i.method.name()
            );
        }
        if !self.children.is_empty() {
            let input: u64 = self.children.iter().map(|c| c.input_tokens).sum();
            let output: u64 = self.children.iter().map(|c| c.output_tokens).sum();
            let _ = write!(
                out,
                "\nchildren: {} run, {input} input / {output} output tokens",
                self.children.len()
            );
        }
        out
    }
}

/// Where debug output goes: `.bhai/debug` under the working directory.
pub fn debug_dir() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_default()
        .join(".bhai")
        .join("debug")
}

/// Write `context-<timestamp>.json` and `.md` into `dir`; returns the JSON path.
pub fn export(profile: &Profile, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f");
    let path = dir.join(format!("context-{stamp}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(profile)?)
        .with_context(|| format!("could not write {}", path.display()))?;
    let md = path.with_extension("md");
    std::fs::write(&md, profile.markdown())
        .with_context(|| format!("could not write {}", md.display()))?;
    Ok(path)
}

/// Append one JSONL line for a finished model call.
pub fn log_usage(path: &Path, usage: &Usage, hit: &Hit, items: usize) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let line = json!({
        "timestamp": chrono::Local::now().to_rfc3339(),
        "input": usage.input,
        "cached": usage.cached,
        "output": usage.output,
        "reasoning": usage.reasoning,
        "history_items": items,
        "expected_cached": hit.expected_cached,
        "hit_ratio": hit.hit_ratio,
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// A history item's category and, where it helps, a short detail for its label.
fn classify(entry: &Value) -> (&'static str, Option<String>) {
    let field = |key: &str| entry.get(key).and_then(Value::as_str);
    match field("type") {
        Some("message") if field("role") == Some("user") => ("user message", None),
        Some("message") => ("assistant message", None),
        Some("reasoning") => ("reasoning", None),
        Some("function_call") => ("function_call", field("name").map(str::to_string)),
        Some("function_call_output") => ("function_call_output", None),
        Some(other) => ("other", Some(other.to_string())),
        None => ("other", None),
    }
}

fn json_string(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn percent(part: usize, total: usize) -> f64 {
    part as f64 * 100.0 / total.max(1) as f64
}

fn scale(tokens: u64, factor: f64) -> u64 {
    (tokens as f64 * factor).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::{ByteEstimate, O200k};

    fn usage(input: u64, output: u64, reasoning: u64) -> Usage {
        Usage {
            input,
            cached: 0,
            output,
            reasoning,
        }
    }

    fn plain(text: &str) -> SystemPrompt {
        SystemPrompt {
            text: text.to_string(),
            ..SystemPrompt::default()
        }
    }

    fn history() -> Vec<Value> {
        vec![
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}),
            json!({"type": "reasoning", "summary": [], "encrypted_content": "x".repeat(400)}),
            json!({"type": "function_call", "name": "bash", "call_id": "c1", "arguments": "{\"command\":\"ls\"}"}),
            json!({"type": "function_call_output", "call_id": "c1", "output": "y".repeat(2000)}),
            json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]}),
        ]
    }

    #[test]
    fn breakdown_is_sorted_categorised_and_sums_up() {
        let tools = [json!({"type": "function", "name": "bash"})];
        let profile = build(&plain("be brief"), &tools, &history(), &[], &ByteEstimate);

        assert_eq!(profile.items.len(), 7);
        assert!(profile.items.windows(2).all(|w| w[0].tokens >= w[1].tokens));
        assert_eq!(profile.items[0].label, "#3 function_call_output");
        let reasoning = profile.items.last().unwrap();
        assert_eq!(reasoning.category, "reasoning");
        assert_eq!((reasoning.method, reasoning.tokens), (Method::Unknown, 0));
        assert!(reasoning.bytes > 400, "encrypted content counts as bytes");
        assert!(
            profile
                .items
                .iter()
                .any(|i| i.label == "#2 function_call: bash")
        );
        assert!(profile.items.iter().any(|i| i.label == "tool: bash"));
        let system = profile
            .items
            .iter()
            .find(|i| i.category == "system prompt")
            .unwrap();
        assert_eq!((system.bytes, system.tokens), (8, 2));
        assert_eq!(system.method, Method::Estimated);

        let bytes: usize = profile.items.iter().map(|i| i.bytes).sum();
        let tokens: u64 = profile.items.iter().map(|i| i.tokens).sum();
        let estimated: u64 = profile.items.iter().map(|i| i.estimated).sum();
        assert_eq!(profile.total_bytes, bytes);
        assert_eq!(profile.tokens, tokens);
        assert_eq!(profile.estimated_tokens, estimated);
        let share: f64 = profile.items.iter().map(|i| i.share).sum();
        assert!((share - 100.0).abs() < 1e-6);

        let names: Vec<_> = profile.categories.iter().map(|c| c.name).collect();
        assert_eq!(names[0], "function_call_output");
        assert_eq!(names.len(), 7);
        assert_eq!(
            profile.categories.iter().map(|c| c.bytes).sum::<usize>(),
            bytes
        );
        assert!(profile.calibration.is_none());
        assert!(profile.items.iter().all(|i| i.label != FRAMING));
    }

    #[test]
    fn instruction_files_are_items_of_their_own() {
        let files = [crate::instructions::File {
            path: "CLAUDE.md".into(),
            label: "./CLAUDE.md".to_string(),
            content: "z".repeat(600),
        }];
        let skill = crate::skills::Skill {
            name: "s".to_string(),
            description: "d".repeat(200),
            dir: "/s".into(),
            source: "~/.claude/skills".to_string(),
        };
        let prompt = crate::prompt::system_prompt(&files, vec![skill]);
        let profile = build(&prompt, &[], &[], &[], &ByteEstimate);
        assert_eq!(profile.items.len(), 3);
        assert_eq!(profile.items[0].label, "system prompt");
        assert_eq!(profile.items[1].label, "instructions: ./CLAUDE.md");
        assert_eq!(profile.items[1].category, "instructions");
        assert_eq!(profile.items[1].bytes, prompt.sources[0].bytes);
        assert_eq!(profile.items[2].label, "skills listing (1)");
        assert_eq!(profile.items[2].category, "skills");
        assert_eq!(profile.items[2].bytes, prompt.skills_bytes);
        assert_eq!(profile.total_bytes, prompt.text.len());
    }

    #[test]
    fn calibration_uses_only_what_the_last_call_sent() {
        let history = history();
        let call = Call {
            usage: usage(1000, 0, 0),
            sent: 4,
            outputs: 0,
        };
        let profile = build(&plain("be brief"), &[], &history, &[call], &ByteEstimate);
        let c = profile.calibration.as_ref().unwrap();
        let unsent = profile
            .items
            .iter()
            .find(|i| i.label == "#4 assistant message")
            .unwrap();
        assert_eq!(
            c.estimated_tokens,
            profile.estimated_tokens - unsent.estimated
        );
        assert!((c.factor - 1000.0 / c.estimated_tokens as f64).abs() < 1e-9);
        let top = &profile.items[0];
        assert_eq!(top.method, Method::Estimated);
        assert_eq!(top.tokens, scale(top.estimated, c.factor));
        // Estimated counts leave no room for a framing row.
        assert!(profile.items.iter().all(|i| i.label != FRAMING));
    }

    #[test]
    fn markdown_has_the_expected_rows() {
        let call = Call {
            usage: usage(900, 0, 0),
            sent: 5,
            outputs: 0,
        };
        let mut profile = build(&plain("be brief"), &[], &history(), &[call], &ByteEstimate);
        assert!(!profile.markdown().contains("## Children"));
        profile.children.push(ChildUsage {
            id: "ab12cd".to_string(),
            identity: "researcher".to_string(),
            description: "find the docs".to_string(),
            input_tokens: 120,
            cached_tokens: 40,
            output_tokens: 9,
        });
        let md = profile.markdown();
        assert!(md.ends_with(
            "## Children\n\n| child | identity | description | input | cached | output |\n\
|---|---|---|---:|---:|---:|\n| ab12cd | researcher | find the docs | 120 | 40 | 9 |\n"
        ));
        let summary = profile.summary(Path::new("/x.json"));
        assert!(summary.ends_with("children: 1 run, 120 input / 9 output tokens"));
        assert!(
            md.starts_with("# bhai context\n\nIdentity `general`: "),
            "{md}"
        );
        assert!(md.contains("Last call: 900 real input tokens"));
        assert!(md.contains("## By category"));
        assert!(md.contains("| function_call_output | 1 | "));
        assert!(md.contains("| #3 function_call_output | function_call_output | estimated | "));
        assert!(md.contains("| #1 reasoning | reasoning | unknown | "));
        assert!(md.contains("| system prompt | system prompt | estimated | 8 | "));
        // Largest item comes before the smallest in the items table.
        let items = &md[md.find("## Items").unwrap()..];
        assert!(items.find("#3 function_call_output") < items.find("system prompt"));
    }

    #[test]
    fn export_writes_json_and_markdown() {
        let dir = std::env::temp_dir().join(format!("bhai-profile-{}", uuid::Uuid::new_v4()));
        let profile = build(&plain("x"), &[], &history(), &[], &ByteEstimate);
        let path = export(&profile, &dir).unwrap();
        let json: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["items"].as_array().unwrap().len(), 6);
        assert_eq!(json["items"][0]["method"], "estimated");
        assert!(path.with_extension("md").exists());
        assert!(profile.summary(&path).lines().count() == 1 + TOP_ITEMS);

        let log = dir.join("usage.jsonl");
        let usage = Usage {
            input: 10,
            cached: 8,
            output: 3,
            reasoning: 1,
        };
        log_usage(&log, &usage, &Hit::default(), 4).unwrap();
        let hit = Hit {
            expected_cached: Some(8),
            hit_ratio: Some(1.0),
        };
        log_usage(&log, &usage, &hit, 6).unwrap();
        let lines: Vec<Value> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["cached"], 8);
        assert_eq!(lines[1]["history_items"], 6);
        assert!(lines[0]["expected_cached"].is_null());
        assert_eq!(lines[1]["expected_cached"], 8);
        assert_eq!(lines[1]["hit_ratio"], 1.0);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Two turns: a tool round, then a new user message after reasoning was dropped.
    fn calls() -> Vec<Call> {
        vec![
            // Sent #0; appended #1 reasoning, #2 function_call.
            Call {
                usage: usage(100, 30, 20),
                sent: 1,
                outputs: 2,
            },
            // Sent #0..#3; #3 is the tool output, delta 180 - 100 - 30 = 50.
            Call {
                usage: usage(180, 5, 0),
                sent: 4,
                outputs: 1,
            },
            // Sent #0..#5; #5 is a user message, delta 150 - 180 - 5 < 0.
            Call {
                usage: usage(150, 3, 0),
                sent: 6,
                outputs: 1,
            },
        ]
    }

    #[test]
    fn deltas_pin_down_appended_items() {
        let spans = attribute(&calls());
        let span = |start, end, tokens, reasoning, reasoning_dropped| Span {
            start,
            end,
            tokens,
            reasoning,
            reasoning_dropped,
        };
        assert_eq!(
            spans,
            [
                span(1, 3, 30, Some(20), false),
                span(3, 4, 50, None, false),
                span(4, 5, 5, Some(0), false),
                span(5, 6, 0, None, true),
                span(6, 7, 3, Some(0), false),
            ]
        );
        assert!(attribute(&[]).is_empty());
    }

    #[test]
    fn rows_take_the_best_method() {
        let mut history = history();
        history.push(json!({"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "hello world"}]}));
        history.push(say_done());
        let tools = [json!({"type": "function", "name": "bash"})];
        let prompt = plain("be brief");
        let profile = build(&prompt, &tools, &history, &calls(), &O200k);
        let row = |label: &str| {
            let item = profile.items.iter().find(|i| i.label == label);
            let item = item.unwrap_or_else(|| panic!("no row {label}"));
            (item.method, item.tokens)
        };

        assert_eq!(row("#0 user message"), (Method::Tokenized, 1));
        assert_eq!(row("#1 reasoning"), (Method::Exact, 20));
        assert_eq!(row("#2 function_call: bash"), (Method::Exact, 10));
        assert_eq!(row("#3 function_call_output"), (Method::Exact, 50));
        assert_eq!(row("#4 assistant message"), (Method::Exact, 5));
        // The negative delta falls back to the tokenizer.
        assert_eq!(row("#5 user message"), (Method::Tokenized, 2));
        assert_eq!(row("#6 assistant message"), (Method::Exact, 3));
        assert_eq!(profile.reasoning_dropped, 1);

        let fixed = O200k.count("be brief") + O200k.count(&tools[0].to_string());
        let framing = 100 - 1 - fixed as u64;
        assert_eq!(row(FRAMING), (Method::Exact, framing));
        assert_eq!(row("system prompt").0, Method::Tokenized);
        assert!(
            profile
                .markdown()
                .contains("| server framing (exact minus tokenized) | framing | exact | 0 | ")
        );
        assert!(profile.markdown().contains("1 call deltas were negative"));
    }

    #[test]
    fn a_span_is_shared_by_tokenized_weight() {
        let mut rows: Vec<Item> = [3, 1, 0]
            .into_iter()
            .map(|tokens| Item {
                label: String::new(),
                category: "function_call",
                bytes: 1,
                method: Method::Tokenized,
                tokens,
                estimated: 1,
                share: 0.0,
            })
            .collect();
        let span = Span {
            start: 0,
            end: 3,
            tokens: 10,
            reasoning: Some(0),
            reasoning_dropped: false,
        };
        apply(&mut rows, &span);
        let tokens: Vec<u64> = rows.iter().map(|i| i.tokens).collect();
        assert_eq!(tokens, [7, 2, 1]);
        assert!(rows.iter().all(|i| i.method == Method::Exact));
    }

    fn say_done() -> Value {
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]})
    }
}
