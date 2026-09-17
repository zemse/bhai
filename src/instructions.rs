//! Instruction files (CLAUDE.md, AGENTS.md and friends) gathered for the system prompt,
//! lowest precedence first.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::Config;

/// Project instruction files looked for in each directory, in this order.
const PROJECT_FILES: [&str; 5] = [
    "AGENTS.md",
    "AGENT.md",
    "CLAUDE.md",
    ".claude/CLAUDE.md",
    "CLAUDE.local.md",
];

/// Where to look. Injectable so tests never touch the real home directory.
#[derive(Debug, Clone)]
pub struct Roots {
    pub home: Option<PathBuf>,
    /// `$CODEX_HOME`, or `~/.codex`.
    pub codex_home: Option<PathBuf>,
    pub cwd: PathBuf,
}

impl Roots {
    pub fn from_env(cwd: PathBuf) -> Self {
        Self {
            home: std::env::var_os("HOME").map(PathBuf::from),
            codex_home: crate::auth::codex_home().ok(),
            cwd,
        }
    }
}

/// One loaded file.
#[derive(Debug, Clone, PartialEq)]
pub struct File {
    pub path: PathBuf,
    /// The path as shown to the user: `./`, `~/` or absolute.
    pub label: String,
    pub content: String,
}

/// What `load` found.
#[derive(Debug, Default)]
pub struct Loaded {
    pub files: Vec<File>,
    /// Imports refused for leaving their root, like `skipped import ~/.ssh/id_rsa (outside project)`.
    pub skipped: Vec<String>,
}

/// A candidate file and the directory its imports must stay under.
struct Candidate {
    path: PathBuf,
    root: PathBuf,
    /// Global files live in their own config dir; project files in the project.
    global: bool,
}

/// Every enabled instruction file that exists, with `@path` imports one level deep.
/// Imports that resolve outside the importing file's root are skipped.
pub fn load(config: &Config, roots: &Roots) -> Loaded {
    let mut candidates = Vec::new();
    let global = |path: PathBuf| Candidate {
        root: path.parent().unwrap_or(&path).to_path_buf(),
        path,
        global: true,
    };
    if config.load_global_claude
        && let Some(home) = &roots.home
    {
        candidates.push(global(home.join(".claude/CLAUDE.md")));
    }
    if config.load_global_agents {
        if let Some(home) = &roots.home {
            candidates.push(global(home.join(".agents/AGENTS.md")));
        }
        if let Some(codex) = &roots.codex_home {
            candidates.push(global(codex.join("AGENTS.md")));
        }
    }
    if config.load_project_instructions {
        let root = project_root(&roots.cwd);
        for dir in project_dirs(&roots.cwd) {
            candidates.extend(PROJECT_FILES.iter().map(|name| Candidate {
                path: dir.join(name),
                root: root.to_path_buf(),
                global: false,
            }));
        }
    }

    let mut seen = HashSet::new();
    let mut loaded = Loaded::default();
    for candidate in candidates {
        let Some(file) = read(&candidate.path, roots, &mut seen) else {
            continue;
        };
        let allowed = allowed_roots(&candidate);
        let reason = if candidate.global {
            format!("outside {}", label(&candidate.root, roots))
        } else {
            "outside project".to_string()
        };
        let mut imports = Vec::new();
        for line in unfenced(&file.content) {
            let Some(target) = import(line, &candidate.path, roots.home.as_deref()) else {
                continue;
            };
            let Ok(real) = target.canonicalize() else {
                continue;
            };
            if allowed.iter().any(|root| real.starts_with(root)) {
                imports.push(real);
            } else {
                let written = line.trim().trim_start_matches('@');
                loaded
                    .skipped
                    .push(format!("skipped import {written} ({reason})"));
            }
        }
        loaded.files.push(file);
        loaded.files.extend(
            imports
                .iter()
                .filter_map(|import| read(import, roots, &mut seen)),
        );
    }
    loaded
}

/// Where a candidate's imports may resolve to. A global file may also be a symlink into
/// a dotfiles checkout, so the directory it really lives in counts too.
fn allowed_roots(candidate: &Candidate) -> Vec<PathBuf> {
    let mut allowed: Vec<PathBuf> = candidate.root.canonicalize().into_iter().collect();
    if candidate.global
        && let Some(dir) = candidate
            .path
            .canonicalize()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        allowed.push(dir);
    }
    allowed
}

/// The lines of `content` outside fenced code blocks (``` or ~~~).
fn unfenced(content: &str) -> impl Iterator<Item = &str> {
    let mut fence: Option<char> = None;
    content.lines().filter(move |line| {
        let trimmed = line.trim_start();
        let marker = ['`', '~']
            .into_iter()
            .find(|c| trimmed.starts_with(&c.to_string().repeat(3)));
        match (fence, marker) {
            (None, Some(c)) => {
                fence = Some(c);
                false
            }
            (Some(open), Some(c)) if open == c => {
                fence = None;
                false
            }
            (None, None) => true,
            (Some(_), _) => false,
        }
    })
}

/// The git repo root, or `cwd` outside a repo.
pub fn project_root(cwd: &Path) -> &Path {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(cwd)
}

/// The project root and every directory down to `cwd`.
fn project_dirs(cwd: &Path) -> Vec<PathBuf> {
    let root = project_root(cwd);
    let mut dirs: Vec<PathBuf> = cwd
        .ancestors()
        .take_while(|dir| *dir != root)
        .map(Path::to_path_buf)
        .collect();
    dirs.push(root.to_path_buf());
    dirs.reverse();
    dirs
}

/// Read a file not seen before. Missing, unreadable and empty files are skipped.
fn read(path: &Path, roots: &Roots, seen: &mut HashSet<PathBuf>) -> Option<File> {
    let canonical = path.canonicalize().ok()?;
    if !canonical.is_file() || !seen.insert(canonical) {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    if content.trim().is_empty() {
        return None;
    }
    Some(File {
        path: path.to_path_buf(),
        label: label(path, roots),
        content,
    })
}

/// The target of a line that is only `@some/path`, relative to the importing file.
fn import(line: &str, from: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let target = line.trim().strip_prefix('@')?;
    if target.is_empty() || target.contains(char::is_whitespace) {
        return None;
    }
    if let Some(rest) = target.strip_prefix("~/") {
        return home.map(|home| home.join(rest));
    }
    Some(from.parent()?.join(target))
}

/// The path as shown to the user: `./`, `~/` or absolute.
pub fn label(path: &Path, roots: &Roots) -> String {
    if let Ok(rest) = path.strip_prefix(&roots.cwd) {
        return format!("./{}", rest.display());
    }
    if let Some(home) = &roots.home
        && let Ok(rest) = path.strip_prefix(home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: PathBuf,
        roots: Roots,
    }

    impl Fixture {
        /// home/, codex/ and a repo at home/repo with cwd at home/repo/sub.
        fn new() -> Self {
            // The temp dir is a symlink on macOS; resolve it so labels strip cleanly.
            let dir = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("bhai-instructions-{}", uuid::Uuid::new_v4()));
            let home = dir.join("home");
            let cwd = home.join("repo/sub");
            std::fs::create_dir_all(home.join("repo/.git")).unwrap();
            std::fs::create_dir_all(&cwd).unwrap();
            let roots = Roots {
                home: Some(home),
                codex_home: Some(dir.join("codex")),
                cwd,
            };
            Self { dir, roots }
        }

        fn write(&self, rel: &str, text: &str) -> PathBuf {
            let path = self.dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        }

        fn labels(&self, config: &Config) -> Vec<String> {
            load(config, &self.roots)
                .files
                .into_iter()
                .map(|f| f.label)
                .collect()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn write_all(f: &Fixture) {
        f.write("home/.claude/CLAUDE.md", "global claude");
        f.write("home/.agents/AGENTS.md", "global agents");
        f.write("codex/AGENTS.md", "codex agents");
        f.write("home/repo/CLAUDE.local.md", "root local");
        f.write("home/repo/AGENTS.md", "root agents");
        f.write("home/repo/sub/.claude/CLAUDE.md", "sub dot claude");
        f.write("home/repo/sub/AGENT.md", "sub agent");
        f.write("home/repo/sub/CLAUDE.md", "sub claude");
    }

    #[test]
    fn discovery_order_is_global_then_root_down_to_cwd() {
        let f = Fixture::new();
        write_all(&f);
        f.write("home/CLAUDE.md", "above the repo root, ignored");
        let files = load(&Config::default(), &f.roots).files;
        let labels: Vec<_> = files.iter().map(|f| f.label.as_str()).collect();
        let codex = f.dir.join("codex/AGENTS.md").display().to_string();
        assert_eq!(
            labels,
            [
                "~/.claude/CLAUDE.md",
                "~/.agents/AGENTS.md",
                codex.as_str(),
                "~/repo/AGENTS.md",
                "~/repo/CLAUDE.local.md",
                "./AGENT.md",
                "./CLAUDE.md",
                "./.claude/CLAUDE.md",
            ]
        );
        assert_eq!(files[0].content, "global claude");
    }

    #[test]
    fn toggles_skip_their_files() {
        let f = Fixture::new();
        write_all(&f);
        let only_claude = Config {
            load_global_agents: false,
            load_project_instructions: false,
            ..Config::default()
        };
        assert_eq!(f.labels(&only_claude), ["~/.claude/CLAUDE.md"]);

        let only_project = Config {
            load_global_claude: false,
            load_global_agents: false,
            ..Config::default()
        };
        assert_eq!(f.labels(&only_project).len(), 5);

        let none = Config {
            load_global_claude: false,
            load_global_agents: false,
            load_project_instructions: false,
            ..Config::default()
        };
        assert!(f.labels(&none).is_empty());
    }

    #[test]
    fn outside_a_repo_only_cwd_is_searched() {
        let f = Fixture::new();
        std::fs::remove_dir(f.dir.join("home/repo/.git")).unwrap();
        f.write("home/repo/CLAUDE.md", "not a repo root now");
        f.write("home/repo/sub/CLAUDE.md", "cwd");
        let project = Config {
            load_global_claude: false,
            load_global_agents: false,
            ..Config::default()
        };
        assert_eq!(f.labels(&project), ["./CLAUDE.md"]);
    }

    fn project_only() -> Config {
        Config {
            load_global_claude: false,
            load_global_agents: false,
            ..Config::default()
        }
    }

    #[test]
    fn imports_resolve_one_level_deep() {
        let f = Fixture::new();
        f.write(
            "home/repo/sub/CLAUDE.md",
            "intro\n@docs/style.md\n  @../top.md  \n@missing.md\nsee @inline.md here\n",
        );
        f.write("home/repo/sub/docs/style.md", "style\n@deeper.md\n");
        f.write("home/repo/sub/docs/deeper.md", "too deep");
        f.write("home/repo/top.md", "top");
        f.write("home/repo/sub/inline.md", "not a whole-line import");
        let loaded = load(&project_only(), &f.roots);
        let labels: Vec<_> = loaded.files.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, ["./CLAUDE.md", "./docs/style.md", "~/repo/top.md"]);
        assert!(loaded.skipped.is_empty());
    }

    #[test]
    fn project_imports_cannot_leave_the_project() {
        let f = Fixture::new();
        let outside = f.write("secret.txt", "secret");
        f.write("home/.ssh/id_rsa", "key");
        f.write("home/.env", "env");
        f.write(
            "home/repo/sub/CLAUDE.md",
            &format!("@~/.ssh/id_rsa\n@../../.env\n@{}\n", outside.display()),
        );
        let loaded = load(&project_only(), &f.roots);
        assert_eq!(loaded.files.len(), 1);
        assert_eq!(
            loaded.skipped,
            [
                "skipped import ~/.ssh/id_rsa (outside project)".to_string(),
                "skipped import ../../.env (outside project)".to_string(),
                format!("skipped import {} (outside project)", outside.display()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_of_the_project_are_skipped() {
        let f = Fixture::new();
        let outside = f.write("secret.txt", "secret");
        std::os::unix::fs::symlink(&outside, f.dir.join("home/repo/sub/link.md")).unwrap();
        f.write("home/repo/sub/CLAUDE.md", "@link.md\n");
        let loaded = load(&project_only(), &f.roots);
        assert_eq!(loaded.files.len(), 1);
        assert_eq!(loaded.skipped, ["skipped import link.md (outside project)"]);
    }

    #[test]
    fn global_imports_stay_in_their_config_dir() {
        let f = Fixture::new();
        f.write("home/.claude/CLAUDE.md", "@guides/rust.md\n@~/notes.md\n");
        f.write("home/.claude/guides/rust.md", "rust");
        f.write("home/notes.md", "notes");
        let only_claude = Config {
            load_global_agents: false,
            load_project_instructions: false,
            ..Config::default()
        };
        let loaded = load(&only_claude, &f.roots);
        let labels: Vec<_> = loaded.files.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, ["~/.claude/CLAUDE.md", "~/.claude/guides/rust.md"]);
        assert_eq!(
            loaded.skipped,
            ["skipped import ~/notes.md (outside ~/.claude)"]
        );
    }

    #[test]
    fn imports_inside_code_fences_are_ignored() {
        let f = Fixture::new();
        f.write("home/repo/sub/MainActor", "not an import");
        f.write("home/repo/sub/x.md", "x");
        f.write(
            "home/repo/sub/CLAUDE.md",
            "```swift\n@MainActor\n~~~\n@x.md\n```\n~~~\n@MainActor\n~~~\n@x.md\n",
        );
        assert_eq!(f.labels(&project_only()), ["./CLAUDE.md", "./x.md"]);
    }

    #[cfg(unix)]
    #[test]
    fn duplicates_are_loaded_once() {
        let f = Fixture::new();
        f.write("home/repo/sub/AGENTS.md", "shared");
        f.write("home/repo/sub/CLAUDE.md", "@AGENTS.md\n@./AGENTS.md\n");
        std::os::unix::fs::symlink(
            f.dir.join("home/repo/sub/AGENTS.md"),
            f.dir.join("home/repo/sub/AGENT.md"),
        )
        .unwrap();
        let project = Config {
            load_global_claude: false,
            load_global_agents: false,
            ..Config::default()
        };
        assert_eq!(f.labels(&project), ["./AGENTS.md", "./CLAUDE.md"]);
    }

    #[test]
    fn missing_everything_is_empty() {
        let roots = Roots {
            home: None,
            codex_home: None,
            cwd: std::env::temp_dir().join(format!("bhai-none-{}", uuid::Uuid::new_v4())),
        };
        let loaded = load(&Config::default(), &roots);
        assert!(loaded.files.is_empty() && loaded.skipped.is_empty());
    }

    #[test]
    fn import_lines() {
        let from = Path::new("/r/a/CLAUDE.md");
        let home = Some(Path::new("/h"));
        assert_eq!(import("@x.md", from, home), Some("/r/a/x.md".into()));
        assert_eq!(import(" @~/y.md ", from, home), Some("/h/y.md".into()));
        assert_eq!(import("@/abs.md", from, home), Some("/abs.md".into()));
        assert_eq!(import("@~/y.md", from, None), None);
        assert_eq!(import("@", from, home), None);
        assert_eq!(import("@a b", from, home), None);
        assert_eq!(import("text @x.md", from, home), None);
    }
}
