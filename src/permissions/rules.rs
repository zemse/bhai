//! Permission rules in Claude Code syntax (`Bash(git log:*)`, `Edit(src/**)`), and the
//! path checks they share with the protected-path list.

use std::path::{Component, Path, PathBuf};

use super::bash;

/// One `allow`, `deny` or `ask` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    /// As written in the config, for messages.
    pub text: String,
    /// The file the rule came from, for `/permissions`.
    pub source: String,
    /// The user wrote or remembered it, so as an allow rule it also applies in `ask` mode.
    pub user: bool,
    /// It came from a repo-supplied settings file, so as an allow rule it needs `/trust`.
    pub repo: bool,
    /// Lowercase tool name.
    tool: String,
    pattern: Pattern,
}

#[derive(Debug, Clone, PartialEq)]
enum Pattern {
    Any,
    /// Word list, exact or as a prefix.
    Command {
        words: Vec<String>,
        prefix: bool,
    },
    Path(String),
}

/// Where relative and `~/` patterns resolve.
#[derive(Debug, Clone, Copy)]
pub struct Base<'a> {
    pub home: Option<&'a Path>,
    pub cwd: &'a Path,
}

impl Rule {
    pub fn parse(text: &str) -> Result<Self, String> {
        let bad = |why: &str| format!("bad permission rule `{text}`: {why}");
        let trimmed = text.trim();
        let (name, content) = match trimmed.split_once('(') {
            Some((name, rest)) => {
                let content = rest.strip_suffix(')').ok_or_else(|| bad("missing `)`"))?;
                (name, Some(content.trim()))
            }
            None => (trimmed, None),
        };
        if !is_tool_name(name) {
            return Err(bad("expected a tool name"));
        }
        let tool = name.to_lowercase();
        let pattern = match content {
            None | Some("*") | Some("") => Pattern::Any,
            Some(content) if tool == "bash" => command_pattern(content).map_err(&bad)?,
            Some(content) if matches!(tool.as_str(), "read" | "edit" | "write") => {
                Pattern::Path(content.to_string())
            }
            Some(_) => return Err(bad("this tool takes no pattern")),
        };
        Ok(Self {
            text: trimmed.to_string(),
            source: String::new(),
            user: false,
            repo: false,
            tool,
            pattern,
        })
    }

    pub fn with_source(self, source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            ..self
        }
    }

    pub fn by_user(self) -> Self {
        Self { user: true, ..self }
    }

    pub fn repo_supplied(self) -> Self {
        Self { repo: true, ..self }
    }

    /// `Edit` rules cover every tool that changes files, as in Claude Code. `mcp__server`
    /// and `mcp__server__*` cover every tool of that server.
    pub fn applies_to(&self, tool: &str) -> bool {
        if self.tool == tool || (self.tool == "edit" && tool == "write") {
            return true;
        }
        let Some(rest) = self.tool.strip_prefix("mcp__") else {
            return false;
        };
        match self.tool.strip_suffix('*') {
            Some(prefix) => tool.starts_with(prefix),
            None => !rest.contains("__") && tool.starts_with(&format!("{}__", self.tool)),
        }
    }

    pub fn is_any(&self) -> bool {
        self.pattern == Pattern::Any
    }

    /// `fold` compares case-insensitively, for rules where a miss is the unsafe side.
    pub fn matches_words(&self, words: &[String], fold: bool) -> bool {
        match &self.pattern {
            Pattern::Any => true,
            Pattern::Command {
                words: want,
                prefix,
            } => {
                let long_enough = if *prefix {
                    words.len() >= want.len()
                } else {
                    words.len() == want.len()
                };
                long_enough
                    && want.iter().zip(words).all(|(want, word)| {
                        if fold {
                            want.eq_ignore_ascii_case(word)
                        } else {
                            want == word
                        }
                    })
            }
            Pattern::Path(_) => false,
        }
    }

    /// `path` must be absolute; it is normalized before matching.
    pub fn matches_path(&self, path: &Path, base: Base, fold: bool) -> bool {
        match &self.pattern {
            Pattern::Any => true,
            Pattern::Path(pattern) => {
                let Some(pattern) = resolve(pattern, base) else {
                    return false;
                };
                let fold_all = |parts: Vec<String>| -> Vec<String> {
                    if fold {
                        parts.iter().map(|p| p.to_lowercase()).collect()
                    } else {
                        parts
                    }
                };
                let pattern = fold_all(pattern);
                let path = fold_all(components(path));
                glob(&pattern, &path)
            }
            Pattern::Command { .. } => false,
        }
    }
}

/// A plain tool name, or an MCP name like `mcp__chrome-devtools__*` whose only `*` ends it.
fn is_tool_name(name: &str) -> bool {
    let plain = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    match name.strip_prefix("mcp__") {
        Some(rest) => {
            let rest = rest.strip_suffix('*').unwrap_or(rest);
            rest.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                && (!rest.is_empty() || name.ends_with('*'))
        }
        None => plain(name),
    }
}

/// Tools whose second word picks the subcommand, so a prefix rule keeps both words.
const MULTI_VERB: &[&str] = &[
    "git", "cargo", "npm", "pnpm", "yarn", "docker", "kubectl", "gh", "go",
];

/// `Bash(<command>)` for exactly this simple command, if the rule reads back as such.
pub fn exact_command(command: &str) -> Option<Rule> {
    let words = single(command)?.words;
    let rule = Rule::parse(&format!("Bash({})", command.trim())).ok()?;
    let want = Pattern::Command {
        words,
        prefix: false,
    };
    (rule.pattern == want).then_some(rule)
}

/// `Bash(ls:*)`, or `Bash(git log:*)` for multi-verb tools. Never behind a wrapper,
/// where the prefix would be the wrapper itself.
pub fn prefix_command(command: &str) -> Option<Rule> {
    let command = single(command)?;
    if command.unwrapped().len() < command.words.len() {
        return None;
    }
    let program = command.words.first()?;
    let len = if MULTI_VERB.contains(&program.as_str()) {
        2
    } else {
        1
    };
    let words = command.words.get(..len)?.to_vec();
    let plain = |w: &String| {
        !w.starts_with('-')
            && w.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./+@".contains(c))
    };
    if !words.iter().all(plain) {
        return None;
    }
    let rule = Rule::parse(&format!("Bash({}:*)", words.join(" "))).ok()?;
    let want = Pattern::Command {
        words,
        prefix: true,
    };
    (rule.pattern == want).then_some(rule)
}

/// The one simple command in `command`, unless it may reach a protected path.
fn single(command: &str) -> Option<bash::Command> {
    let mut commands = bash::parse(command)?;
    if commands.len() != 1 {
        return None;
    }
    let command = commands.remove(0);
    (!command.words.is_empty() && !bash::mentions_protected(&command)).then_some(command)
}

/// `Edit(/src/main.rs)` for one file: `/` anchors at the working directory, `//` at root.
pub fn exact_path(tool: &str, path: &Path, base: Base) -> Option<Rule> {
    let name = if tool == "write" { "Write" } else { "Edit" };
    let pattern = path_pattern(&components(path), base)?;
    let rule = Rule::parse(&format!("{name}({pattern})")).ok()?;
    rule.matches_path(path, base, false).then_some(rule)
}

/// `Edit(/src/**)`: every change under the file's directory.
pub fn dir_path(path: &Path, base: Base) -> Option<Rule> {
    let parts = components(path);
    let dir = parts.get(..parts.len().checked_sub(1)?)?;
    if dir.is_empty() {
        return None;
    }
    let pattern = path_pattern(dir, base)?;
    let pattern = match pattern.ends_with('/') {
        true => format!("{pattern}**"),
        false => format!("{pattern}/**"),
    };
    let rule = Rule::parse(&format!("Edit({pattern})")).ok()?;
    rule.matches_path(path, base, false).then_some(rule)
}

fn path_pattern(parts: &[String], base: Base) -> Option<String> {
    if parts.iter().any(|p| p.contains(['*', '?', '[', '(', ')'])) {
        return None;
    }
    let cwd = components(base.cwd);
    Some(match parts.strip_prefix(cwd.as_slice()) {
        Some(rest) => format!("/{}", rest.join("/")),
        None => format!("//{}", parts.join("/")),
    })
}

/// `git log:*` and `npm run test *` are prefixes; anything else is an exact word list.
fn command_pattern(content: &str) -> Result<Pattern, &'static str> {
    let (rest, prefix) = match content.strip_suffix(":*") {
        Some(rest) => (rest, true),
        None => match content.strip_suffix(" *") {
            Some(rest) => (rest, true),
            None => (content, false),
        },
    };
    let mut commands = bash::parse(rest).ok_or("the command does not parse")?;
    let words = match commands.len() {
        0 if prefix => return Ok(Pattern::Any),
        1 => commands.remove(0).words,
        _ => return Err("expected one simple command"),
    };
    if words.iter().any(|w| w.contains(['*', '?', '['])) {
        return Err("only a trailing `:*` or ` *` wildcard is supported");
    }
    Ok(Pattern::Command { words, prefix })
}

/// A path pattern as absolute components: `//x` is absolute, `~/x` under home, and the
/// rest under the working directory, gitignore style (no slash matches at any depth).
fn resolve(pattern: &str, base: Base) -> Option<Vec<String>> {
    let (root, rest) = if let Some(rest) = pattern.strip_prefix("//") {
        (PathBuf::from("/"), rest.to_string())
    } else if let Some(rest) = pattern.strip_prefix("~/") {
        (base.home?.to_path_buf(), rest.to_string())
    } else if let Some(rest) = pattern.strip_prefix('/') {
        (base.cwd.to_path_buf(), rest.to_string())
    } else if pattern.trim_end_matches('/').contains('/') {
        (base.cwd.to_path_buf(), pattern.to_string())
    } else {
        (base.cwd.to_path_buf(), format!("**/{pattern}"))
    };
    let mut parts = components(&root);
    let rest = match rest.strip_suffix('/') {
        Some(dir) => format!("{dir}/**"),
        None => rest,
    };
    for part in rest.split('/').filter(|p| !p.is_empty() && *p != ".") {
        if part == ".." {
            parts.pop();
        } else {
            parts.push(part.to_string());
        }
    }
    Some(parts)
}

/// Lexically normalized components, so `/a/../b` is `["b"]`.
pub fn components(path: &Path) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    parts
}

/// `**` matches any number of components, `*` and `?` stay within one.
fn glob(pattern: &[String], path: &[String]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((first, rest)) if first == "**" => {
            (0..=path.len()).any(|skip| glob(rest, &path[skip..]))
        }
        Some((first, rest)) => path
            .split_first()
            .is_some_and(|(part, tail)| wildcard(first, part) && glob(rest, tail)),
    }
}

fn wildcard(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Files the agent may never change without asking, whatever the mode or rules say.
pub fn is_protected(path: &Path, home: Option<&Path>) -> bool {
    let parts: Vec<String> = components(path).iter().map(|p| p.to_lowercase()).collect();
    let name = parts.last().map(String::as_str).unwrap_or_default();
    let parent = parts.len().checked_sub(2).map(|i| parts[i].as_str());
    if parts.iter().any(|p| p == ".git")
        || name.starts_with(".env")
        || parts.ends_with(&[".bhai".to_string(), "config.toml".to_string()])
        || parts.ends_with(&[".bhai".to_string(), "settings.local.json".to_string()])
        || (parent == Some(".claude") && name.starts_with("settings") && name.ends_with(".json"))
    {
        return true;
    }
    let Some(home) = home else {
        return false;
    };
    let home: Vec<String> = components(home).iter().map(|p| p.to_lowercase()).collect();
    [
        &[".ssh"][..],
        &[".codex"],
        &[".claude"],
        &[".config", "bhai"],
    ]
    .iter()
    .any(|dir| {
        let mut prefix = home.clone();
        prefix.extend(dir.iter().map(|d| d.to_string()));
        parts.starts_with(&prefix)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/u";
    const CWD: &str = "/home/u/repo";

    fn base() -> Base<'static> {
        Base {
            home: Some(Path::new(HOME)),
            cwd: Path::new(CWD),
        }
    }

    fn words(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn parses_rules() {
        assert!(Rule::parse("Read").unwrap().is_any());
        assert!(Rule::parse("Bash(*)").unwrap().is_any());
        assert!(Rule::parse("Bash(:*)").unwrap().is_any());
        assert!(Rule::parse("Skill").unwrap().applies_to("skill"));
        assert!(Rule::parse("Edit(x)").unwrap().applies_to("write"));
        assert!(!Rule::parse("Write(x)").unwrap().applies_to("edit"));
        for bad in [
            "Bash(ls",
            "",
            "Bash(ls; rm x)",
            "Bash(ls $(x))",
            "Bash(git log*)",
            "Skill(x)",
            "Ba sh",
            "mcp__",
            "mcp__a*b",
            "mcp__x(y)",
        ] {
            assert!(Rule::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn mcp_rules_match_a_tool_or_a_whole_server() {
        let cases = [
            ("mcp__github__get_issue", "mcp__github__get_issue", true),
            ("mcp__github__get_issue", "mcp__github__get_issues", false),
            ("mcp__GitHub", "mcp__github__get_issue", true),
            ("mcp__github", "mcp__github2__get_issue", false),
            ("mcp__github", "mcp__github", true),
            ("mcp__github__*", "mcp__github__x", true),
            ("mcp__github__*", "mcp__githubx__x", false),
            (
                "mcp__chrome-devtools__*",
                "mcp__chrome-devtools__click",
                true,
            ),
            ("mcp__*", "mcp__any__thing", true),
            ("mcp__*", "bash", false),
            ("mcp__github__get", "mcp__github__get__x", false),
            ("Bash", "mcp__bash__x", false),
        ];
        for (rule, tool, want) in cases {
            let rule = Rule::parse(rule).unwrap();
            assert!(rule.is_any());
            assert_eq!(rule.applies_to(tool), want, "{} vs {tool}", rule.text);
        }
    }

    #[test]
    fn prefix_rules_compare_words() {
        let cases = [
            ("Bash(git log:*)", "git log --oneline", true),
            ("Bash(git log:*)", "git log", true),
            ("Bash(git log:*)", "git logx", false),
            ("Bash(git log:*)", "git -c x log", false),
            ("Bash(npm run test *)", "npm run test --watch", true),
            ("Bash(npm run test *)", "npm run testx", false),
            ("Bash(git status)", "git status", true),
            ("Bash(git status)", "git status -s", false),
            ("Bash('a b' c)", "a b c", false),
        ];
        for (rule, command, want) in cases {
            let rule = Rule::parse(rule).unwrap();
            assert_eq!(
                rule.matches_words(&words(command), false),
                want,
                "{command}"
            );
        }
        let quoted = Rule::parse("Bash('a b' c)").unwrap();
        assert!(quoted.matches_words(&["a b".to_string(), "c".to_string()], false));
        let rm = Rule::parse("Bash(rm:*)").unwrap();
        assert!(!rm.matches_words(&words("RM -rf x"), false));
        assert!(rm.matches_words(&words("RM -rf x"), true));
    }

    #[test]
    fn offered_command_rules() {
        let text = |rule: Option<Rule>| rule.map(|r| r.text);
        let cases = [
            ("ls -la", Some("Bash(ls -la)"), Some("Bash(ls:*)")),
            (
                "git log --oneline",
                Some("Bash(git log --oneline)"),
                Some("Bash(git log:*)"),
            ),
            (
                "cargo test -p x",
                Some("Bash(cargo test -p x)"),
                Some("Bash(cargo test:*)"),
            ),
            ("git -C x log", Some("Bash(git -C x log)"), None),
            ("cargo", Some("Bash(cargo)"), None),
            (
                "timeout 5 cargo test",
                Some("Bash(timeout 5 cargo test)"),
                None,
            ),
            ("echo 'a b'", Some("Bash(echo 'a b')"), Some("Bash(echo:*)")),
            ("'my tool' x", Some("Bash('my tool' x)"), None),
            ("ls && pwd", None, None),
            ("ls $(pwd)", None, None),
            ("ls *.rs", None, Some("Bash(ls:*)")),
            ("cat .env", None, None),
            ("echo a:*", None, Some("Bash(echo:*)")),
        ];
        for (command, exact, prefix) in cases {
            assert_eq!(text(exact_command(command)).as_deref(), exact, "{command}");
            assert_eq!(
                text(prefix_command(command)).as_deref(),
                prefix,
                "{command}"
            );
        }
    }

    #[test]
    fn offered_path_rules() {
        let offer = |tool, path: &str| {
            let path = Path::new(path);
            (
                exact_path(tool, path, base()).map(|r| r.text),
                dir_path(path, base()).map(|r| r.text),
            )
        };
        let some = |a: &str, b: &str| (Some(a.to_string()), Some(b.to_string()));
        assert_eq!(
            offer("edit", "/home/u/repo/src/main.rs"),
            some("Edit(/src/main.rs)", "Edit(/src/**)")
        );
        assert_eq!(
            offer("write", "/home/u/repo/README.md"),
            some("Write(/README.md)", "Edit(/**)")
        );
        assert_eq!(
            offer("write", "/tmp/x/y.txt"),
            some("Write(//tmp/x/y.txt)", "Edit(//tmp/x/**)")
        );
        assert_eq!(
            offer("write", "/y.txt"),
            (Some("Write(//y.txt)".into()), None)
        );
        assert_eq!(
            offer("edit", "/home/u/repo/a*b"),
            (None, Some("Edit(/**)".into()))
        );
    }

    #[test]
    fn path_globs() {
        let cases = [
            ("Edit(src/**)", "/home/u/repo/src/a/b.rs", true),
            ("Edit(src/**)", "/home/u/repo/src", true),
            ("Edit(src/*.rs)", "/home/u/repo/src/a/b.rs", false),
            ("Edit(src/*.rs)", "/home/u/repo/src/main.rs", true),
            ("Edit(/src/*.rs)", "/home/u/repo/src/main.rs", true),
            ("Edit(*.md)", "/home/u/repo/docs/deep/x.md", true),
            ("Edit(*.md)", "/elsewhere/x.md", false),
            ("Edit(docs/)", "/home/u/repo/docs/x/y", true),
            ("Read(~/notes/**)", "/home/u/notes/a.txt", true),
            ("Read(//etc/*)", "/etc/passwd", true),
            ("Read(//etc/*)", "/etc/ssh/sshd_config", false),
            ("Write(src/**)", "/home/u/repo/src/../../x", false),
            ("Write(a?c)", "/home/u/repo/abc", true),
            ("Write(a*c*e)", "/home/u/repo/abxcde", true),
            ("Write(a*c*e)", "/home/u/repo/abxcd", false),
            ("Write", "/anything", true),
        ];
        for (rule, path, want) in cases {
            let parsed = Rule::parse(rule).unwrap();
            assert_eq!(
                parsed.matches_path(Path::new(path), base(), false),
                want,
                "{rule} {path}"
            );
        }
        let env = Rule::parse("Edit(.env)").unwrap();
        assert!(!env.matches_path(Path::new("/home/u/repo/.ENV"), base(), false));
        assert!(env.matches_path(Path::new("/home/u/repo/.ENV"), base(), true));
        let home = Rule::parse("Read(~/x)").unwrap();
        let no_home = Base {
            home: None,
            cwd: Path::new(CWD),
        };
        assert!(!home.matches_path(Path::new("/home/u/x"), no_home, false));
    }

    #[test]
    fn protected_paths() {
        let home = Some(Path::new(HOME));
        for path in [
            "/home/u/repo/.git/config",
            "/home/u/repo/.GIT/hooks/pre-commit",
            "/home/u/repo/.env",
            "/home/u/repo/sub/.env.local",
            "/home/u/.ssh/authorized_keys",
            "/home/u/.codex/auth.json",
            "/home/u/.claude/CLAUDE.md",
            "/home/u/.config/bhai/config.toml",
            "/home/u/repo/.bhai/config.toml",
            "/home/u/repo/.bhai/settings.local.json",
            "/home/u/repo/.claude/settings.local.json",
            "/home/u/repo/src/../.git/HEAD",
        ] {
            assert!(is_protected(Path::new(path), home), "{path}");
        }
        for path in [
            "/home/u/repo/src/main.rs",
            "/home/u/repo/.gitignore",
            "/home/u/repo/.claude/agents/x.md",
            "/home/u/repo/.bhai/debug/x.json",
            "/home/u/.config/other",
        ] {
            assert!(!is_protected(Path::new(path), home), "{path}");
        }
        assert!(!is_protected(Path::new("/home/u/.ssh/id"), None));
    }
}
