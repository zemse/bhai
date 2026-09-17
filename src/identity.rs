//! Agent identities: markdown files with frontmatter, the same shape as Claude Code agent
//! files, that narrow the skills, tools and instructions a session carries. The identity
//! is fixed for the session so the prompt cache holds.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::config::{Config, Source};
use crate::frontmatter;
use crate::instructions::{self, File, Roots};
use crate::prompt::{self, SystemPrompt};
use crate::skills;
use crate::tools::{self, Registry};

/// The identity used without `--as`.
pub const DEFAULT: &str = "general";

/// Agent file roots under the home directory, lowest precedence first.
const HOME_DIRS: [&str; 2] = [".claude/agents", ".config/bhai/agents"];
/// Agent file roots under the project root, lowest precedence first.
const PROJECT_DIRS: [&str; 2] = [".claude/agents", ".bhai/agents"];

const ROUTER_PROMPT: &str = "You are the router. You carry no skills and can only read \
files, so do not do the work yourself: pick the specialised identity below that fits the \
task and delegate to it. Until delegation is available, tell the user which identity to \
start with `bhai --as <name>`.";

#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub name: String,
    pub description: String,
    /// Where it was defined, as shown to the user.
    pub source: String,
    pub path: Option<PathBuf>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// bhai tool names; `None` allows every tool.
    pub tools: Option<Vec<String>>,
    /// Skill name globs, `!pat` to exclude; empty allows every skill.
    pub skills: Vec<String>,
    /// MCP server or tool globs, kept for when MCP lands.
    pub mcp: Vec<String>,
    /// Instruction sources to load; `None` keeps the config's.
    pub instructions: Option<Vec<Source>>,
    /// Extra system prompt, after the instruction files and before the skills.
    pub prompt: String,
}

impl Default for Identity {
    fn default() -> Self {
        general()
    }
}

impl Identity {
    fn builtin(name: &str, description: &str) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            source: "built-in".to_string(),
            path: None,
            model: None,
            effort: None,
            tools: None,
            skills: Vec::new(),
            mcp: Vec::new(),
            instructions: None,
            prompt: String::new(),
        }
    }

    pub fn allows_tool(&self, name: &str) -> bool {
        self.tools
            .as_ref()
            .is_none_or(|tools| tools.iter().any(|t| t == name))
    }

    /// Included by some pattern (or there are none) and excluded by none.
    pub fn allows_skill(&self, name: &str) -> bool {
        let (excludes, includes): (Vec<&str>, Vec<&str>) = self
            .skills
            .iter()
            .map(String::as_str)
            .partition(|p| p.starts_with('!'));
        (includes.is_empty() || includes.iter().any(|p| glob(p, name)))
            && !excludes.iter().any(|p| glob(&p[1..], name))
    }
}

fn general() -> Identity {
    Identity::builtin(
        DEFAULT,
        "Everything: all skills, tools and instructions, no extra prompt.",
    )
}

fn router() -> Identity {
    Identity {
        tools: Some(vec![tools::read::NAME.to_string()]),
        skills: vec!["!*".to_string()],
        prompt: ROUTER_PROMPT.to_string(),
        ..Identity::builtin(
            "router",
            "No skills, read only; delegates to specialised identities.",
        )
    }
}

/// An agent file. `None` without a frontmatter `name`.
pub fn parse(text: &str, path: &Path, source: &str) -> Option<Identity> {
    let (front, body) = frontmatter::split(text)?;
    let value = |key| frontmatter::value(&front, key).filter(|v| !v.is_empty());
    let list = |key| frontmatter::list(&front, key).unwrap_or_default();
    let tools = frontmatter::list(&front, "tools")
        .filter(|t| !t.is_empty())
        .map(|t| t.iter().filter_map(|t| tool_name(t)).collect());
    let instructions = frontmatter::list(&front, "instructions")
        .map(|list| list.iter().filter_map(|s| Source::parse(s)).collect());
    Some(Identity {
        name: value("name")?,
        description: value("description").unwrap_or_default(),
        source: source.to_string(),
        path: Some(path.to_path_buf()),
        model: value("model"),
        effort: value("effort"),
        tools,
        skills: list("skills"),
        mcp: list("mcp"),
        instructions,
        prompt: body.trim_end().to_string(),
    })
}

/// The bhai tool a bhai or Claude Code tool name refers to, like `Read` or `Bash(git:*)`.
fn tool_name(name: &str) -> Option<String> {
    let name = name.split('(').next().unwrap_or(name).trim().to_lowercase();
    tools::NAMES.contains(&name.as_str()).then_some(name)
}

/// `*` matches any run of characters and `?` any one.
fn glob(pattern: &str, text: &str) -> bool {
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
    p[pi..].iter().all(|&c| c == '*')
}

/// The built-ins, then every agent file; a later definition replaces an earlier one
/// of the same name. The router's prompt lists the others.
pub fn discover(roots: &Roots) -> Vec<Identity> {
    let mut roots_list = Vec::new();
    if let Some(home) = &roots.home {
        roots_list.extend(HOME_DIRS.map(|d| home.join(d)));
    }
    let project = instructions::project_root(&roots.cwd);
    roots_list.extend(PROJECT_DIRS.map(|d| project.join(d)));

    let mut found = vec![general(), router()];
    for root in roots_list {
        for identity in scan(&root, &instructions::label(&root, roots)) {
            match found.iter_mut().find(|i| i.name == identity.name) {
                Some(slot) => *slot = identity,
                None => found.push(identity),
            }
        }
    }
    let listing = found
        .iter()
        .filter(|i| i.name != "router")
        .map(|i| format!("- {}: {}", i.name, i.description))
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(router) = found
        .iter_mut()
        .find(|i| i.name == "router" && i.path.is_none())
    {
        let _ = write!(router.prompt, "\n\nIdentities:\n{listing}");
    }
    found
}

/// The agent files directly under one root, in file name order.
fn scan(root: &Path, source: &str) -> Vec<Identity> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|path| parse(&std::fs::read_to_string(&path).ok()?, &path, source))
        .collect()
}

/// The identity called `name`, or an error listing the available ones.
pub fn find(identities: &[Identity], name: &str) -> Result<Identity> {
    if let Some(identity) = identities.iter().find(|i| i.name == name) {
        return Ok(identity.clone());
    }
    let names: Vec<_> = identities.iter().map(|i| i.name.as_str()).collect();
    bail!(
        "unknown identity `{name}`. Available identities: {}",
        names.join(", ")
    )
}

/// The system prompt for a session running as `identity`.
pub fn build(config: &Config, roots: &Roots, identity: &Identity) -> SystemPrompt {
    let mut config = config.clone();
    if let Some(sources) = &identity.instructions {
        config.load_global_claude &= sources.contains(&Source::GlobalClaude);
        config.load_global_agents &= sources.contains(&Source::GlobalAgents);
        config.load_project_instructions &= sources.contains(&Source::Project);
    }
    let skills = if config.skills && identity.allows_tool(tools::skill::NAME) {
        skills::discover(roots, &config.skill_sources)
            .into_iter()
            .filter(|s| identity.allows_skill(&s.name))
            .collect()
    } else {
        Vec::new()
    };
    let loaded = instructions::load(&config, roots);
    let mut files = loaded.files;
    if !identity.prompt.is_empty() {
        files.push(File {
            path: identity.path.clone().unwrap_or_default(),
            label: format!("identity {}", identity.name),
            content: identity.prompt.clone(),
        });
    }
    let mut prompt = prompt::system_prompt(&files, skills);
    prompt.skipped = loaded.skipped;
    prompt.identity = identity.clone();
    prompt
}

/// Bytes a session sends before any message: the system prompt and the tool schemas.
pub fn baseline(prompt: &SystemPrompt) -> usize {
    let tools: usize = Registry::for_prompt(prompt)
        .schemas()
        .iter()
        .map(|s| s.to_string().len())
        .sum();
    prompt.text.len() + tools
}

/// What `bhai identities` prints: each identity, its source and its baseline cost.
pub fn report(identities: &[Identity], config: &Config, roots: &Roots) -> String {
    let cost = |i: &Identity| baseline(&build(config, roots, i)).div_ceil(4) as i64;
    let general = identities
        .iter()
        .find(|i| i.name == DEFAULT)
        .map_or(0, cost);
    let mut out = String::new();
    for identity in identities {
        let tokens = cost(identity);
        let saving = match general - tokens {
            0 => String::new(),
            n if n > 0 => format!(", saves ~{n} tok vs {DEFAULT}"),
            n => format!(", ~{} tok more than {DEFAULT}", -n),
        };
        let _ = writeln!(
            out,
            "{}  ({})  ~{tokens} tok{saving}\n    {}",
            identity.name, identity.source, identity.description
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: PathBuf,
        roots: Roots,
    }

    impl Fixture {
        /// home/ and a repo at home/repo, cwd at the repo root.
        fn new() -> Self {
            let dir = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("bhai-identity-{}", uuid::Uuid::new_v4()));
            let home = dir.join("home");
            let cwd = home.join("repo");
            std::fs::create_dir_all(cwd.join(".git")).unwrap();
            let roots = Roots {
                home: Some(home),
                codex_home: None,
                cwd,
            };
            Self { dir, roots }
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn skill(&self, name: &str) {
            let text = format!("---\nname: {name}\ndescription: the {name} skill\n---\nbody\n");
            self.write(&format!("home/.claude/skills/{name}/SKILL.md"), &text);
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn identity(text: &str) -> Identity {
        parse(text, Path::new("/a.md"), "~/.claude/agents").unwrap()
    }

    #[test]
    fn parses_bhai_and_claude_style_files() {
        let bhai = identity(
            "---\nname: apple-dev\ndescription: iOS work\nmodel: gpt-5\neffort: high\n\
tools: [read, edit]\nskills:\n  - ios*\n  - '!ios-old'\nmcp: XcodeBuildMCP\n\
instructions: [project, nope]\n---\n\nBe Swift-y.\n",
        );
        assert_eq!(bhai.name, "apple-dev");
        assert_eq!(bhai.model.as_deref(), Some("gpt-5"));
        assert_eq!(bhai.effort.as_deref(), Some("high"));
        assert_eq!(bhai.tools, Some(vec!["read".into(), "edit".into()]));
        assert_eq!(bhai.skills, ["ios*", "!ios-old"]);
        assert_eq!(bhai.mcp, ["XcodeBuildMCP"]);
        assert_eq!(bhai.instructions, Some(vec![Source::Project]));
        assert_eq!(bhai.prompt, "Be Swift-y.");

        let claude = identity(
            "---\nname: reviewer\ndescription: Reviews code\ntools: Read, Grep, Bash(git:*), Glob\n---\nReview.\n",
        );
        assert_eq!(claude.tools, Some(vec!["read".into(), "bash".into()]));
        assert!(claude.skills.is_empty() && claude.mcp.is_empty());
        assert!(claude.allows_skill("anything"));
        assert_eq!(claude.instructions, None);
        assert!(!claude.allows_tool("write"));

        let open = identity("---\nname: open\ntools:\n---\n");
        assert_eq!(open.tools, None);
        assert!(open.allows_tool("write"));
        assert!(parse("---\ndescription: x\n---\n", Path::new("/b.md"), "").is_none());
    }

    #[test]
    fn skill_globs_include_and_exclude() {
        let with = |skills: &[&str]| Identity {
            skills: skills.iter().map(|s| s.to_string()).collect(),
            ..general()
        };
        assert!(with(&[]).allows_skill("pdf"));
        let apple = with(&["ios*", "swift*", "!swift-old"]);
        assert!(apple.allows_skill("ios-dev") && apple.allows_skill("swift"));
        assert!(!apple.allows_skill("swift-old") && !apple.allows_skill("pdf"));
        let none = with(&["!*"]);
        assert!(!none.allows_skill("pdf"));
        let but = with(&["!pdf"]);
        assert!(but.allows_skill("yt") && !but.allows_skill("pdf"));
        assert!(glob("a?c*", "abcdef") && !glob("a?c", "ac") && glob("*b*", "abc"));
    }

    #[test]
    fn later_roots_override_earlier_ones_by_name() {
        let f = Fixture::new();
        let file = |name: &str, description: &str| {
            format!("---\nname: {name}\ndescription: {description}\n---\n")
        };
        f.write("home/.claude/agents/a.md", &file("one", "claude global"));
        f.write("home/.claude/agents/b.md", &file("two", "claude global"));
        f.write("home/.claude/agents/notes.txt", &file("three", "ignored"));
        f.write("home/.config/bhai/agents/a.md", &file("one", "bhai global"));
        f.write(
            "home/repo/.claude/agents/x.md",
            &file("two", "claude project"),
        );
        f.write(
            "home/repo/.bhai/agents/y.md",
            &file("general", "project general"),
        );

        let found = discover(&f.roots);
        let rows: Vec<_> = found
            .iter()
            .map(|i| (i.name.as_str(), i.description.as_str(), i.source.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("general", "project general", "./.bhai/agents"),
                ("router", found[1].description.as_str(), "built-in"),
                ("one", "bhai global", "~/.config/bhai/agents"),
                ("two", "claude project", "./.claude/agents"),
            ]
        );
        assert!(found[1].prompt.contains("- one: bhai global"));
        assert!(!found[1].prompt.contains("- router"));
        assert_eq!(find(&found, "one").unwrap().name, "one");
        let err = find(&found, "nope").unwrap_err().to_string();
        assert!(err.contains("general, router, one, two"), "{err}");
    }

    #[test]
    fn filters_apply_to_the_prompt_the_registry_and_the_skill_tool() {
        let f = Fixture::new();
        f.skill("ios-dev");
        f.skill("pdf");
        let apple = Identity {
            tools: Some(vec!["read".into(), "skill".into()]),
            skills: vec!["ios*".into()],
            prompt: "Apple only.".into(),
            ..Identity::builtin("apple", "")
        };
        let prompt = build(&Config::default(), &f.roots, &apple);
        assert_eq!(prompt.skills.len(), 1);
        assert!(prompt.text.contains("- ios-dev:") && !prompt.text.contains("- pdf:"));
        assert!(prompt.text.contains("Apple only.\n\n# Skills"));

        let registry = Registry::for_prompt(&prompt);
        let names: Vec<_> = registry
            .schemas()
            .iter()
            .map(|s| s["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["read", "skill"]);
        assert!(registry.get("bash").is_none());
        let skill = registry.get("skill").unwrap();
        assert!(
            skill
                .describe(&serde_json::json!({"name": "ios-dev"}))
                .is_ok()
        );
        let err = skill
            .describe(&serde_json::json!({"name": "pdf"}))
            .unwrap_err();
        assert!(err.contains("No skill named `pdf`"), "{err}");

        let router = build(&Config::default(), &f.roots, &router());
        assert!(router.skills.is_empty());
        assert!(Registry::for_prompt(&router).get("skill").is_none());
        assert_eq!(router.identity.name, "router");
    }

    #[test]
    fn instruction_toggles_narrow_the_config() {
        let f = Fixture::new();
        f.write("home/.claude/CLAUDE.md", "global");
        f.write("home/repo/CLAUDE.md", "project");
        let only_project = Identity {
            instructions: Some(vec![Source::Project]),
            ..general()
        };
        let labels = |config: &Config| -> Vec<String> {
            build(config, &f.roots, &only_project)
                .sources
                .into_iter()
                .map(|s| s.label)
                .collect()
        };
        assert_eq!(labels(&Config::default()), ["./CLAUDE.md"]);
        let no_project = Config {
            load_project_instructions: false,
            ..Config::default()
        };
        assert!(labels(&no_project).is_empty());
    }

    #[test]
    fn a_narrow_identity_costs_less_than_general() {
        let f = Fixture::new();
        for name in ["ios-dev", "swift", "pdf", "research", "web-search"] {
            f.skill(name);
        }
        f.write(
            "home/.config/bhai/agents/rust.md",
            "---\nname: rust-engineer\ndescription: Rust work\nskills: \"!*\"\n---\nWrite idiomatic Rust.\n",
        );
        let identities = discover(&f.roots);
        let config = Config::default();
        let cost =
            |name: &str| baseline(&build(&config, &f.roots, &find(&identities, name).unwrap()));
        assert!(cost("rust-engineer") < cost(DEFAULT));
        assert!(cost("router") < cost(DEFAULT));
        let report = report(&identities, &config, &f.roots);
        assert!(
            report.contains("rust-engineer  (~/.config/bhai/agents)  ~"),
            "{report}"
        );
        assert!(report.contains("tok vs general\n    Rust work"), "{report}");
    }
}
