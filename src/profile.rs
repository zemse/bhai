//! Token profiler: where the context goes, item by item, and a per-call usage log.
//!
//! Tokens are estimated as bytes/4 and, once a real input count is known, scaled by how
//! far that estimate was off for the last call.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};

use crate::client::Usage;
use crate::prompt::SystemPrompt;

/// Items `/context` prints into the transcript.
const TOP_ITEMS: usize = 5;

/// The real input count of a model call and how many history items it was sent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measured {
    pub input_tokens: u64,
    pub items: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Profile {
    /// The session's identity.
    pub identity: String,
    pub total_bytes: usize,
    pub estimated_tokens: u64,
    pub calibration: Option<Calibration>,
    /// Totals per category, largest first.
    pub categories: Vec<Category>,
    /// Every component of the context, largest first.
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Calibration {
    /// Input tokens the backend reported for the last call.
    pub input_tokens: u64,
    /// What bytes/4 estimated for that same call.
    pub estimated_tokens: u64,
    /// Real divided by estimated.
    pub factor: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub label: String,
    pub category: &'static str,
    pub bytes: usize,
    pub tokens: u64,
    pub scaled_tokens: Option<u64>,
    /// Percent of all context bytes.
    pub share: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Category {
    pub name: &'static str,
    pub items: usize,
    pub bytes: usize,
    pub tokens: u64,
    pub scaled_tokens: Option<u64>,
    pub share: f64,
}

/// Break the context of the next request down into its components.
pub fn build(
    prompt: &SystemPrompt,
    tools: &[Value],
    history: &[Value],
    measured: Option<Measured>,
) -> Profile {
    let appended: usize =
        prompt.sources.iter().map(|s| s.bytes).sum::<usize>() + prompt.skills_bytes;
    let mut items = vec![item(
        "system prompt".to_string(),
        "system prompt",
        prompt.text.len() - appended,
    )];
    for source in &prompt.sources {
        items.push(item(
            format!("instructions: {}", source.label),
            "instructions",
            source.bytes,
        ));
    }
    if prompt.skills_bytes > 0 {
        items.push(item(
            format!("skills listing ({})", prompt.skills.len()),
            "skills",
            prompt.skills_bytes,
        ));
    }
    for tool in tools {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("?");
        items.push(item(format!("tool: {name}"), "tool schema", json_len(tool)));
    }
    let fixed = items.len();
    for (index, entry) in history.iter().enumerate() {
        let (category, detail) = classify(entry);
        let label = match detail {
            Some(detail) => format!("#{index} {category}: {detail}"),
            None => format!("#{index} {category}"),
        };
        items.push(item(label, category, json_len(entry)));
    }

    let calibration = measured.map(|m| {
        let sent = fixed + m.items.min(history.len());
        let estimated_tokens: u64 = items[..sent].iter().map(|i| i.tokens).sum();
        Calibration {
            input_tokens: m.input_tokens,
            estimated_tokens,
            factor: m.input_tokens as f64 / estimated_tokens.max(1) as f64,
        }
    });
    let factor = calibration.as_ref().map(|c| c.factor);

    let total_bytes: usize = items.iter().map(|i| i.bytes).sum();
    let estimated_tokens = items.iter().map(|i| i.tokens).sum();
    for item in &mut items {
        item.share = percent(item.bytes, total_bytes);
        item.scaled_tokens = factor.map(|f| scale(item.tokens, f));
    }

    let mut categories: Vec<Category> = Vec::new();
    for item in &items {
        match categories.iter_mut().find(|c| c.name == item.category) {
            Some(c) => {
                c.items += 1;
                c.bytes += item.bytes;
                c.tokens += item.tokens;
            }
            None => categories.push(Category {
                name: item.category,
                items: 1,
                bytes: item.bytes,
                tokens: item.tokens,
                scaled_tokens: None,
                share: 0.0,
            }),
        }
    }
    for c in &mut categories {
        c.share = percent(c.bytes, total_bytes);
        c.scaled_tokens = factor.map(|f| scale(c.tokens, f));
    }

    // Stable sorts, so equal sizes keep their context order.
    items.sort_by_key(|i| std::cmp::Reverse(i.bytes));
    categories.sort_by_key(|c| std::cmp::Reverse(c.bytes));
    Profile {
        identity: prompt.identity.name.clone(),
        total_bytes,
        estimated_tokens,
        calibration,
        categories,
        items,
    }
}

impl Profile {
    /// The markdown report written next to the JSON export.
    pub fn markdown(&self) -> String {
        let mut out = String::from("# bhai context\n\n");
        let _ = writeln!(
            out,
            "Identity `{}`: {} items, {} bytes, about {} tokens (bytes/4).",
            self.identity,
            self.items.len(),
            self.total_bytes,
            self.estimated_tokens
        );
        match &self.calibration {
            Some(c) => {
                let _ = writeln!(
                    out,
                    "Last call: {} real input tokens vs {} estimated, factor {:.2}.",
                    c.input_tokens, c.estimated_tokens, c.factor
                );
            }
            None => out.push_str("No model call yet, so the estimates are uncalibrated.\n"),
        }

        out.push_str("\n## By category\n\n");
        out.push_str("| category | items | bytes | est. tokens | scaled | share |\n");
        out.push_str("|---|---:|---:|---:|---:|---:|\n");
        for c in &self.categories {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {:.1}% |",
                c.name,
                c.items,
                c.bytes,
                c.tokens,
                optional(c.scaled_tokens),
                c.share
            );
        }

        out.push_str("\n## Items\n\n");
        out.push_str("| item | category | bytes | est. tokens | scaled | share |\n");
        out.push_str("|---|---|---:|---:|---:|---:|\n");
        for i in &self.items {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {:.1}% |",
                i.label.replace('|', "\\|"),
                i.category,
                i.bytes,
                i.tokens,
                optional(i.scaled_tokens),
                i.share
            );
        }
        out
    }

    /// What `/context` prints: where the export went and the largest items.
    pub fn summary(&self, path: &Path) -> String {
        let mut out = format!(
            "context ({}): about {} tokens, written to {} and .md",
            self.identity,
            self.estimated_tokens,
            path.display()
        );
        for i in self.items.iter().take(TOP_ITEMS) {
            let tokens = i.scaled_tokens.unwrap_or(i.tokens);
            let _ = write!(out, "\n{:>5.1}% {:>8} tok  {}", i.share, tokens, i.label);
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
pub fn log_usage(path: &Path, usage: &Usage, items: usize) -> Result<()> {
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

fn item(label: String, category: &'static str, bytes: usize) -> Item {
    Item {
        label,
        category,
        bytes,
        tokens: (bytes as u64).div_ceil(4),
        scaled_tokens: None,
        share: 0.0,
    }
}

fn json_len(value: &Value) -> usize {
    serde_json::to_string(value).map_or(0, |s| s.len())
}

fn percent(part: usize, total: usize) -> f64 {
    part as f64 * 100.0 / total.max(1) as f64
}

fn scale(tokens: u64, factor: f64) -> u64 {
    (tokens as f64 * factor).round() as u64
}

fn optional(n: Option<u64>) -> String {
    n.map_or_else(|| "-".to_string(), |n| n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let profile = build(&plain("be brief"), &tools, &history(), None);

        assert_eq!(profile.items.len(), 7);
        assert!(profile.items.windows(2).all(|w| w[0].bytes >= w[1].bytes));
        assert_eq!(profile.items[0].label, "#3 function_call_output");
        assert_eq!(profile.items[1].category, "reasoning");
        assert!(profile.items[1].bytes > 400, "encrypted content counts");
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

        let bytes: usize = profile.items.iter().map(|i| i.bytes).sum();
        let tokens: u64 = profile.items.iter().map(|i| i.tokens).sum();
        assert_eq!(profile.total_bytes, bytes);
        assert_eq!(profile.estimated_tokens, tokens);
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
        assert!(profile.items.iter().all(|i| i.scaled_tokens.is_none()));
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
        let profile = build(&prompt, &[], &[], None);
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
        let measured = Measured {
            input_tokens: 1000,
            items: 4,
        };
        let profile = build(&plain("be brief"), &[], &history, Some(measured));
        let c = profile.calibration.as_ref().unwrap();
        let unsent = profile
            .items
            .iter()
            .find(|i| i.label == "#4 assistant message")
            .unwrap();
        assert_eq!(c.estimated_tokens, profile.estimated_tokens - unsent.tokens);
        assert!((c.factor - 1000.0 / c.estimated_tokens as f64).abs() < 1e-9);
        let top = &profile.items[0];
        assert_eq!(top.scaled_tokens, Some(scale(top.tokens, c.factor)));
    }

    #[test]
    fn markdown_has_the_expected_rows() {
        let measured = Measured {
            input_tokens: 900,
            items: 5,
        };
        let md = build(&plain("be brief"), &[], &history(), Some(measured)).markdown();
        assert!(
            md.starts_with("# bhai context\n\nIdentity `general`: "),
            "{md}"
        );
        assert!(md.contains("Last call: 900 real input tokens"));
        assert!(md.contains("## By category"));
        assert!(md.contains("| function_call_output | 1 | "));
        assert!(md.contains("| #3 function_call_output | function_call_output | "));
        assert!(md.contains("| system prompt | system prompt | 8 | 2 | "));
        // Largest item comes before the smallest in the items table.
        let items = &md[md.find("## Items").unwrap()..];
        assert!(items.find("#3 function_call_output") < items.find("system prompt"));
    }

    #[test]
    fn export_writes_json_and_markdown() {
        let dir = std::env::temp_dir().join(format!("bhai-profile-{}", uuid::Uuid::new_v4()));
        let profile = build(&plain("x"), &[], &history(), None);
        let path = export(&profile, &dir).unwrap();
        let json: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["items"].as_array().unwrap().len(), 6);
        assert!(path.with_extension("md").exists());
        assert!(profile.summary(&path).lines().count() == 1 + TOP_ITEMS);

        let log = dir.join("usage.jsonl");
        let usage = Usage {
            input: 10,
            cached: 8,
            output: 3,
            reasoning: 1,
        };
        log_usage(&log, &usage, 4).unwrap();
        log_usage(&log, &usage, 6).unwrap();
        let lines: Vec<Value> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["cached"], 8);
        assert_eq!(lines[1]["history_items"], 6);
        let _ = std::fs::remove_dir_all(dir);
    }
}
