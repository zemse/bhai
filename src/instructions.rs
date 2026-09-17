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

/// Every enabled instruction file that exists, with `@path` imports one level deep.
pub fn load(config: &Config, roots: &Roots) -> Vec<File> {
    let mut candidates = Vec::new();
    if config.load_global_claude
        && let Some(home) = &roots.home
    {
        candidates.push(home.join(".claude/CLAUDE.md"));
    }
    if config.load_global_agents {
        if let Some(home) = &roots.home {
            candidates.push(home.join(".agents/AGENTS.md"));
        }
        if let Some(codex) = &roots.codex_home {
            candidates.push(codex.join("AGENTS.md"));
        }
    }
    if config.load_project_instructions {
        for dir in project_dirs(&roots.cwd) {
            candidates.extend(PROJECT_FILES.iter().map(|name| dir.join(name)));
        }
    }

    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for path in candidates {
        let Some(file) = read(&path, roots, &mut seen) else {
            continue;
        };
        let imports: Vec<PathBuf> = file
            .content
            .lines()
            .filter_map(|line| import(line, &path, roots.home.as_deref()))
            .collect();
        files.push(file);
        files.extend(
            imports
                .iter()
                .filter_map(|import| read(import, roots, &mut seen)),
        );
    }
    files
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
        let files = load(&Config::default(), &f.roots);
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

    #[test]
    fn imports_resolve_one_level_deep() {
        let f = Fixture::new();
        f.write(
            "home/repo/sub/CLAUDE.md",
            "intro\n@docs/style.md\n  @~/notes.md  \n@missing.md\nsee @inline.md here\n",
        );
        f.write("home/repo/sub/docs/style.md", "style\n@deeper.md\n");
        f.write("home/repo/sub/docs/deeper.md", "too deep");
        f.write("home/notes.md", "notes");
        f.write("home/repo/sub/inline.md", "not a whole-line import");
        let project = Config {
            load_global_claude: false,
            load_global_agents: false,
            ..Config::default()
        };
        assert_eq!(
            f.labels(&project),
            ["./CLAUDE.md", "./docs/style.md", "~/notes.md"]
        );
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
        assert!(load(&Config::default(), &roots).is_empty());
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
