//! The `@` file finder: the project's files, walked the way git sees them, offered in
//! the `/` menu's place and filtered by what has been typed after the `@`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::commands::Item;

/// Files a walk keeps; a tree bigger than this is cut, and the menu says so.
const MAX_FILES: usize = 20_000;
/// Rows the finder offers; the menu scrolls through them.
const MAX_MATCHES: usize = 50;
/// How old a walk can be before the next `@` walks again, to see new files.
const STALE: Duration = Duration::from_secs(10);

/// The path being typed at the cursor, given the text before it: an `@` that starts a
/// word, then anything but whitespace, so an email address keeps the menu shut.
pub fn typing(before: &str) -> Option<&str> {
    let (head, path) = before.rsplit_once('@')?;
    let opens = head.is_empty() || head.ends_with(char::is_whitespace);
    (opens && !path.contains(char::is_whitespace)).then_some(path)
}

#[derive(Default)]
struct Walk {
    /// Paths relative to the root, `/` separated, with their lowercase form.
    paths: Vec<(String, String)>,
    running: bool,
    done: Option<Instant>,
    truncated: bool,
    /// Bumped by each finished walk, so `poll` can tell a new one from the last.
    generation: u64,
}

/// The files under one root, walked on a thread of its own when first asked for.
pub struct Finder {
    root: PathBuf,
    walk: Arc<Mutex<Walk>>,
    seen: u64,
}

impl Finder {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            walk: Arc::default(),
            seen: 0,
        }
    }

    /// Start a walk unless one is running or the last is fresh.
    pub fn want(&self) {
        let mut walk = self.walk.lock().unwrap_or_else(|e| e.into_inner());
        if walk.running || walk.done.is_some_and(|at| at.elapsed() < STALE) {
            return;
        }
        walk.running = true;
        let root = self.root.clone();
        let slot = Arc::clone(&self.walk);
        std::thread::spawn(move || {
            let (paths, truncated) = list(&root);
            let mut walk = slot.lock().unwrap_or_else(|e| e.into_inner());
            walk.paths = paths;
            walk.truncated = truncated;
            walk.running = false;
            walk.done = Some(Instant::now());
            walk.generation += 1;
        });
    }

    /// Whether a walk has finished since the last call.
    pub fn poll(&mut self) -> bool {
        let generation = self
            .walk
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .generation;
        std::mem::replace(&mut self.seen, generation) != generation
    }

    /// The files matching `typed`, best first: the name holding it, then the path, then
    /// the letters in order anywhere. Empty until the first walk is done.
    pub fn matches(&self, typed: &str) -> Vec<Item> {
        let walk = self.walk.lock().unwrap_or_else(|e| e.into_inner());
        let typed = typed.to_lowercase();
        let mut found: Vec<(u8, &str)> = walk
            .paths
            .iter()
            .filter_map(|(path, lower)| {
                let name = lower.rsplit('/').next().unwrap_or(lower);
                let rank = match () {
                    _ if name.contains(&typed) => 0,
                    _ if lower.contains(&typed) => 1,
                    _ if in_order(lower, &typed) => 2,
                    _ => return None,
                };
                Some((rank, path.as_str()))
            })
            .collect();
        found.sort_by(|a, b| (a.0, a.1.len(), a.1).cmp(&(b.0, b.1.len(), b.1)));
        let help = match walk.truncated {
            true => format!("first {MAX_FILES} files only"),
            false => String::new(),
        };
        found
            .into_iter()
            .take(MAX_MATCHES)
            .map(|(_, path)| Item {
                name: path.to_string(),
                args: String::new(),
                help: help.clone(),
                skill: false,
                file: true,
            })
            .collect()
    }
}

/// Every file under `root` that git would not ignore, hidden ones left out as ripgrep
/// does, and whether the cap cut the list short.
fn list(root: &std::path::Path) -> (Vec<(String, String)>, bool) {
    let mut paths = Vec::new();
    // Outside a repository a `.gitignore` still says what is not worth offering.
    let walker = ignore::WalkBuilder::new(root).require_git(false).build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let path = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if paths.len() == MAX_FILES {
            return (paths, true);
        }
        let lower = path.to_lowercase();
        paths.push((path, lower));
    }
    (paths, false)
}

/// Whether the chars of `needle` appear in `hay` in order.
fn in_order(hay: &str, needle: &str) -> bool {
    let mut hay = hay.chars();
    needle.chars().all(|c| hay.any(|h| h == c))
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A tree with an ignored directory and a hidden file, walked to the end.
    pub fn finder() -> Finder {
        let root = std::env::temp_dir().join(format!("bhai-files-{}", uuid::Uuid::new_v4()));
        for (path, text) in [
            (".gitignore", "target/\n"),
            ("src/main.rs", ""),
            ("src/app.rs", ""),
            ("src/tools/bash.rs", ""),
            ("README.md", ""),
            ("documentation/tools.md", ""),
            ("target/debug/bhai", ""),
            (".env", "SECRET=1"),
        ] {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let mut finder = Finder::new(root);
        finder.want();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !finder.poll() {
            assert!(Instant::now() < deadline, "the walk never finished");
            std::thread::yield_now();
        }
        finder
    }

    fn names(finder: &Finder, typed: &str) -> Vec<String> {
        finder.matches(typed).into_iter().map(|i| i.name).collect()
    }

    #[test]
    fn the_finder_opens_on_an_at_word_wherever_it_is_typed() {
        assert_eq!(typing("@"), Some(""));
        assert_eq!(typing("look at @src/ma"), Some("src/ma"));
        assert_eq!(typing("one\n@a"), Some("a"));
        assert_eq!(typing("@src/main.rs and"), None);
        assert_eq!(typing("mail me@host"), None);
        assert_eq!(typing("no at"), None);
    }

    #[test]
    fn the_walk_skips_what_git_ignores_and_hidden_files() {
        let mut finder = finder();
        assert_eq!(
            names(&finder, ""),
            [
                "README.md",
                "src/app.rs",
                "src/main.rs",
                "src/tools/bash.rs",
                "documentation/tools.md"
            ]
        );
        assert!(!finder.poll(), "nothing new since");
        // Fresh, so a second `@` does not walk again.
        finder.want();
        assert!(!finder.walk.lock().unwrap().running);
    }

    #[test]
    fn the_name_beats_the_path_and_the_path_beats_scattered_letters() {
        let finder = finder();
        assert_eq!(
            names(&finder, "ma"),
            ["src/main.rs", "documentation/tools.md"]
        );
        assert_eq!(
            names(&finder, "TOOLS"),
            ["documentation/tools.md", "src/tools/bash.rs"]
        );
        assert_eq!(
            names(&finder, "rs"),
            ["src/app.rs", "src/main.rs", "src/tools/bash.rs"]
        );
        assert_eq!(names(&finder, "srcbash"), ["src/tools/bash.rs"]);
        assert!(names(&finder, "zzz").is_empty());
        assert!(
            finder
                .matches("app")
                .iter()
                .all(|i| i.file && i.help.is_empty())
        );
    }

    #[test]
    fn nothing_is_offered_before_the_walk() {
        let finder = Finder::new(std::env::temp_dir().join("bhai-files-none"));
        assert!(finder.matches("").is_empty());
    }
}
