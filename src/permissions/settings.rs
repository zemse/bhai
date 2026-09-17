//! Permission rules in JSON settings files: approvals bhai remembers in
//! `.bhai/settings.local.json`, and the `permissions` lists of Claude Code's settings.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{Rule, Rules};

/// Remembered approvals, under the project directory.
pub const LOCAL: &str = ".bhai/settings.local.json";

/// Claude Code's uncommitted project settings, which a repo may still ship.
pub const CLAUDE_LOCAL: &str = ".claude/settings.local.json";

/// The allow rules bhai saved in `path`, which count as the user's, and a notice for each one that does not parse.
pub fn load_local(path: &Path) -> (Vec<Rule>, Vec<String>) {
    local_rules(path, read(path))
}

/// The allow rules in `settings`, read from `path`, as [`load_local`] loads them.
pub(super) fn local_rules(path: &Path, settings: Result<Value>) -> (Vec<Rule>, Vec<String>) {
    let mut notices = Vec::new();
    let settings = match settings {
        Ok(settings) => settings,
        Err(e) => return (Vec::new(), vec![format!("{e:#}")]),
    };
    let rules = strings(&settings, "allow")
        .filter_map(|text| match Rule::parse(text) {
            Ok(rule) => Some(
                rule.with_source(path.display().to_string())
                    .by_user()
                    .repo_supplied(),
            ),
            Err(e) => {
                notices.push(format!("skipped in {}: {e}", path.display()));
                None
            }
        })
        .collect();
    (rules, notices)
}

/// Add `rule` to the allow list in `path`, keeping every other key.
pub fn remember(path: &Path, rule: &str) -> Result<()> {
    let mut settings = read(path)?;
    if !settings.is_object() {
        anyhow::bail!("{} is not a JSON object", path.display());
    }
    let permissions = settings
        .as_object_mut()
        .expect("checked above")
        .entry("permissions")
        .or_insert_with(|| json!({}));
    let allow = permissions
        .as_object_mut()
        .with_context(|| format!("`permissions` in {} is not an object", path.display()))?
        .entry("allow")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .with_context(|| format!("`permissions.allow` in {} is not a list", path.display()))?;
    if !allow.iter().any(|r| r.as_str() == Some(rule)) {
        allow.push(json!(rule));
    }
    let mut text = serde_json::to_string_pretty(&settings)?;
    text.push('\n');
    write_atomic(path, text.as_bytes())
}

/// The allow rules in `settings` as written, parsed or not.
pub(super) fn allow_texts(settings: &Value) -> Vec<String> {
    strings(settings, "allow").map(str::to_string).collect()
}

/// The allow rules in `settings`, read from `path`, as [`claude`] loads a repo's local file.
pub(super) fn claude_repo_allow(path: &Path, settings: &Value) -> Vec<Rule> {
    strings(settings, "allow")
        .filter_map(|text| claude_rule(text)?.ok())
        .map(|rule| rule.with_source(path.display().to_string()).repo_supplied())
        .collect()
}

/// Write to a temporary file next to `path`, then rename it over `path`.
pub(super) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("settings path has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let result = std::fs::write(&temp, bytes).and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.with_context(|| format!("writing {}", path.display()))
}

/// Claude Code's rules for the tools bhai has, from the global and project settings.
/// The shared project file is checked into the repo, so like `.bhai/config.toml` it may
/// only add deny and ask rules. Only the global file's rules count as the user's.
pub fn claude(home: Option<&Path>, cwd: &Path) -> (Rules, Vec<String>) {
    let global = home.map(|h| h.join(".claude/settings.json"));
    let shared = cwd.join(".claude/settings.json");
    let files = [
        global.clone().map(|p| (p, true, true)),
        (global.as_ref() != Some(&shared)).then_some((shared, false, false)),
        Some((cwd.join(CLAUDE_LOCAL), true, false)),
    ];
    let mut rules = Rules::default();
    let mut notices = Vec::new();
    for (path, trusted, user) in files.into_iter().flatten() {
        let repo = path == cwd.join(CLAUDE_LOCAL);
        let settings = match read(&path) {
            Ok(settings) => settings,
            Err(e) => {
                notices.push(format!("{e:#}"));
                continue;
            }
        };
        let mut load = |key: &str, into: &mut Vec<Rule>| {
            for text in strings(&settings, key) {
                match claude_rule(text) {
                    Some(Ok(rule)) => {
                        let rule = rule.with_source(path.display().to_string());
                        into.push(match (user, repo) {
                            (true, _) => rule.by_user(),
                            (false, true) => rule.repo_supplied(),
                            (false, false) => rule,
                        });
                    }
                    Some(Err(e)) => notices.push(format!("skipped in {}: {e}", path.display())),
                    None => {}
                }
            }
        };
        load("deny", &mut rules.deny);
        load("ask", &mut rules.ask);
        if trusted {
            load("allow", &mut rules.allow);
        } else if strings(&settings, "allow").next().is_some() {
            notices.push(format!(
                "ignored the allow rules in {}: a repo file cannot approve its own commands",
                path.display()
            ));
        }
    }
    (rules, notices)
}

/// A Claude Code rule for a tool bhai has, MCP tools included; `None` for any other tool.
fn claude_rule(text: &str) -> Option<Result<Rule, String>> {
    let name = text.split('(').next().unwrap_or_default().trim();
    match name {
        "Bash" | "Read" | "Edit" | "Write" => Some(Rule::parse(text)),
        "MultiEdit" => Some(Rule::parse(&text.trim().replacen("MultiEdit", "Edit", 1))),
        _ if name.starts_with("mcp__") => Some(Rule::parse(text)),
        _ => None,
    }
}

/// The settings object in `path`, or an empty one if there is no file.
fn read(path: &Path) -> Result<Value> {
    match std::fs::read(path) {
        Ok(bytes) => parse(path, &bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// The settings object in `bytes`, read from `path`.
pub(super) fn parse(path: &Path, bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes).with_context(|| format!("bad settings {}", path.display()))
}

/// The strings in `permissions.<key>`.
fn strings<'a>(settings: &'a Value, key: &str) -> impl Iterator<Item = &'a str> {
    settings
        .pointer(&format!("/permissions/{key}"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bhai-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, value: Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, value.to_string()).unwrap();
    }

    fn texts(rules: &[Rule]) -> Vec<&str> {
        rules.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn remembered_rules_round_trip_and_dedupe() {
        let dir = temp_dir();
        let path = dir.join(LOCAL);
        assert!(load_local(&path).0.is_empty());
        remember(&path, "Bash(git log:*)").unwrap();
        remember(&path, "Edit(/src/**)").unwrap();
        remember(&path, "Bash(git log:*)").unwrap();
        let (rules, notices) = load_local(&path);
        assert_eq!(texts(&rules), ["Bash(git log:*)", "Edit(/src/**)"]);
        assert!(notices.is_empty());
        assert_eq!(rules[0].source, path.display().to_string());
        assert!(rules.iter().all(|r| r.user));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remember_keeps_other_keys_and_leaves_no_temp_files() {
        let dir = temp_dir();
        let path = dir.join(LOCAL);
        write(
            &path,
            json!({"theme": "dark", "permissions": {"deny": ["Bash(rm:*)"], "allow": ["Bash(ls)", "Bogus("]}}),
        );
        remember(&path, "Bash(pwd)").unwrap();
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["theme"], "dark");
        assert_eq!(saved["permissions"]["deny"], json!(["Bash(rm:*)"]));
        assert_eq!(
            saved["permissions"]["allow"],
            json!(["Bash(ls)", "Bogus(", "Bash(pwd)"])
        );
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["settings.local.json"]);
        let (rules, notices) = load_local(&path);
        assert_eq!(texts(&rules), ["Bash(ls)", "Bash(pwd)"]);
        assert_eq!(notices.len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remember_refuses_to_clobber_a_bad_file() {
        let dir = temp_dir();
        let path = dir.join(LOCAL);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        assert!(remember(&path, "Bash(ls)").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
        write(&path, json!({"permissions": {"allow": "Bash"}}));
        assert!(remember(&path, "Bash(ls)").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn imports_claude_rules_for_known_tools() {
        let dir = temp_dir();
        let (home, cwd) = (dir.join("home"), dir.join("repo"));
        write(
            &home.join(".claude/settings.json"),
            json!({"permissions": {
                "allow": ["Bash(git log:*)", "WebFetch(domain:x.com)", "mcp__github__get", "MultiEdit(src/**)"],
                "deny": ["Read(*.pem)", "Bash(npm run test?)"],
            }}),
        );
        write(
            &cwd.join(".claude/settings.json"),
            json!({"permissions": {"allow": ["Bash"], "ask": ["Write(docs/**)"]}}),
        );
        write(
            &cwd.join(".claude/settings.local.json"),
            json!({"permissions": {"allow": ["Read", "Glob"]}}),
        );
        let (rules, notices) = claude(Some(&home), &cwd);
        assert_eq!(
            texts(&rules.allow),
            [
                "Bash(git log:*)",
                "mcp__github__get",
                "Edit(src/**)",
                "Read"
            ]
        );
        let users: Vec<bool> = rules.allow.iter().map(|r| r.user).collect();
        assert_eq!(users, [true, true, true, false]);
        let repo: Vec<bool> = rules.allow.iter().map(|r| r.repo).collect();
        assert_eq!(repo, [false, false, false, true]);
        assert_eq!(texts(&rules.deny), ["Read(*.pem)"]);
        assert_eq!(texts(&rules.ask), ["Write(docs/**)"]);
        assert_eq!(
            rules.ask[0].source,
            cwd.join(".claude/settings.json").display().to_string()
        );
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert!(notices[0].contains("npm run"));
        assert!(notices[1].contains("ignored the allow rules"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_home_working_directory_reads_the_global_file_once() {
        let dir = temp_dir();
        write(
            &dir.join(".claude/settings.json"),
            json!({"permissions": {"allow": ["Bash(ls)"]}}),
        );
        let (rules, notices) = claude(Some(&dir), &dir);
        assert_eq!(texts(&rules.allow), ["Bash(ls)"]);
        assert!(notices.is_empty());
        let (rules, _) = claude(None, &dir.join("missing"));
        assert_eq!(rules, Rules::default());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
