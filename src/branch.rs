//! The checked-out branch, for the status bar. `.git/HEAD` is read rather than `git`
//! being run: the bar asks on a timer, and a process per ask buys nothing.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How stale the name on the bar may be. A checkout in another terminal shows up
/// within this.
const REFRESH: Duration = Duration::from_secs(2);

/// Characters of a detached head's commit the bar shows.
const SHORT: usize = 7;

/// The branch of the directory bhai runs in, re-read on a timer.
#[derive(Debug, Default)]
pub struct Branch {
    root: PathBuf,
    name: Option<String>,
    read_at: Option<Instant>,
}

impl Branch {
    /// The branch of `root`, read once now.
    pub fn at(root: PathBuf) -> Self {
        let mut branch = Self {
            root,
            name: None,
            read_at: None,
        };
        branch.refresh();
        branch
    }

    /// Read the name again once what is on screen is older than `REFRESH`.
    pub fn refresh(&mut self) {
        if self.read_at.is_some_and(|at| at.elapsed() < REFRESH) {
            return;
        }
        self.read_at = Some(Instant::now());
        self.name = head(&self.root);
    }

    /// The branch, or nothing outside a repository.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// A branch fixed to `name`, for tests that draw it.
    #[cfg(test)]
    pub fn named(name: &str) -> Self {
        Self {
            root: PathBuf::new(),
            name: Some(name.to_string()),
            read_at: Some(Instant::now()),
        }
    }
}

/// What `HEAD` names: the branch, or the short commit when the head is detached.
pub fn head(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(git_dir(dir)?.join("HEAD")).ok()?;
    let head = text.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(name) if !name.is_empty() => Some(name.to_string()),
        Some(_) => None,
        // A detached head has no name, so the commit is the closest thing to one.
        None => (!head.is_empty() && head.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| head.chars().take(SHORT).collect()),
    }
}

/// The repository's git directory: the nearest `.git` walking up from `dir`, or where
/// the `.git` file of a linked worktree points.
fn git_dir(dir: &Path) -> Option<PathBuf> {
    let found = dir
        .ancestors()
        .map(|up| up.join(".git"))
        .find(|path| path.exists())?;
    if found.is_dir() {
        return Some(found);
    }
    let pointer = std::fs::read_to_string(&found).ok()?;
    let path = PathBuf::from(pointer.trim().strip_prefix("gitdir:")?.trim());
    Some(match path.is_absolute() {
        true => path,
        // A worktree's pointer is relative to the file holding it.
        false => found.parent()?.join(path),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory with a `.git` holding `head`, and a nested directory inside it.
    fn repo(head: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("bhai-branch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), head).unwrap();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        root
    }

    #[test]
    fn the_branch_is_read_from_the_nearest_git_dir() {
        let root = repo("ref: refs/heads/feature/one\n");
        assert_eq!(head(&root).as_deref(), Some("feature/one"));
        assert_eq!(head(&root.join("src/deep")).as_deref(), Some("feature/one"));
        assert_eq!(Branch::at(root.clone()).name(), Some("feature/one"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_detached_head_shows_its_commit() {
        let root = repo("9f1c0b3a7d5e4f2c8a6b0d1e3f5a7c9b2d4e6f80\n");
        assert_eq!(head(&root).as_deref(), Some("9f1c0b3"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_worktree_follows_its_pointer() {
        let root = repo("ref: refs/heads/main\n");
        let tree = root.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::create_dir_all(root.join(".git/worktrees/side")).unwrap();
        std::fs::write(
            root.join(".git/worktrees/side/HEAD"),
            "ref: refs/heads/side\n",
        )
        .unwrap();
        std::fs::write(tree.join(".git"), "gitdir: ../.git/worktrees/side\n").unwrap();
        assert_eq!(head(&tree).as_deref(), Some("side"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nothing_outside_a_repository_or_from_a_broken_head() {
        let empty = std::env::temp_dir().join(format!("bhai-branch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(head(&empty), None);
        assert_eq!(Branch::at(empty.clone()).name(), None);
        std::fs::remove_dir_all(empty).unwrap();

        let root = repo("ref: refs/heads/\n");
        assert_eq!(head(&root), None);
        std::fs::write(root.join(".git/HEAD"), "not a ref at all").unwrap();
        assert_eq!(head(&root), None);
        std::fs::remove_dir_all(root).unwrap();
    }
}
