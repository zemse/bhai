//! A git worktree for each child agent that asks for one, so children can edit at once,
//! and the registry that sees each one cleaned up however its child or session ends.
//!
//! An entry is written to `.bhai/worktrees.json` before its worktree exists, so a crash at
//! any point leaves something the next startup can find. A worktree with nothing to lose
//! (no uncommitted change, no commit the checkout lacks) is removed with its branch; any
//! other is kept and named, never removed with `--force`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The registry, under the project's `.bhai`.
pub const REGISTRY: &str = "worktrees.json";
/// Where the worktrees themselves go, under `.bhai`.
pub const DIR: &str = "worktrees";
const LOCK: &str = "worktrees.lock";

/// One worktree bhai made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub path: PathBuf,
    /// Where the child works: `path`, or the project's subdirectory inside it.
    pub workdir: PathBuf,
    pub branch: String,
    /// The commit it was made from.
    pub base: String,
    pub session: String,
    pub child: String,
    /// The bhai process the session runs in; once it is gone, so is every child of it.
    pub pid: u32,
    /// Its child has ended and it was kept, so nothing is working in it.
    #[serde(default)]
    pub kept: bool,
}

/// What became of a worktree when it was settled.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// It had nothing to lose, and is gone with its branch.
    Removed,
    /// It is still there, for the reason given.
    Kept(String),
    /// The registry no longer lists it: something else settled it first.
    Gone,
}

/// A project's worktrees: its `.bhai` and the directory bhai runs in.
#[derive(Debug, Clone)]
pub struct Place {
    pub bhai: PathBuf,
    pub project: PathBuf,
}

impl Place {
    /// For bhai running in `project`. Canonical, so the paths git prints and the ones
    /// kept here agree through a symlinked temp dir.
    pub fn new(project: &Path) -> Self {
        let project = project
            .canonicalize()
            .unwrap_or_else(|_| project.to_path_buf());
        Self {
            bhai: project.join(".bhai"),
            project,
        }
    }

    fn registry(&self) -> PathBuf {
        self.bhai.join(REGISTRY)
    }

    /// Held across every read-modify-write of the registry, by children of this process
    /// and other bhai processes in the project alike.
    fn lock(&self) -> Result<Lock, String> {
        crate::sessions::private_dir(&self.bhai).map_err(|e| e.to_string())?;
        let file = crate::sessions::private_append(&self.bhai.join(LOCK))
            .map_err(|e| format!("could not open the worktree lock: {e}"))?;
        Lock::take(file)
    }

    fn load(&self) -> Result<Vec<Entry>, String> {
        match std::fs::read_to_string(self.registry()) {
            Ok(text) if text.trim().is_empty() => Ok(Vec::new()),
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("{} does not parse: {e}", self.registry().display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("could not read {}: {e}", self.registry().display())),
        }
    }

    /// Written aside and renamed over, so a crash mid-write leaves the old list whole.
    fn save(&self, entries: &[Entry]) -> Result<(), String> {
        let text = serde_json::to_string_pretty(entries).map_err(|e| e.to_string())?;
        let aside = self
            .bhai
            .join(format!(".{REGISTRY}.{}.tmp", std::process::id()));
        crate::sessions::private_write(&aside, &text)
            .and_then(|()| std::fs::rename(&aside, self.registry()))
            .map_err(|e| format!("could not write {}: {e}", self.registry().display()))
    }

    /// A worktree for child `child` of `session`: the one it was kept in when it is
    /// continued, or a new one on a new branch from `HEAD`.
    pub fn lease(&self, session: &str, child: &str) -> Result<Lease, String> {
        {
            let _lock = self.lock()?;
            let mut entries = self.load()?;
            if let Some(entry) = entries
                .iter_mut()
                .find(|e| e.session == session && e.child == child && e.kept && e.path.is_dir())
            {
                (entry.kept, entry.pid) = (false, std::process::id());
                let entry = entry.clone();
                self.save(&entries)?;
                return Ok(Lease::new(self.clone(), entry));
            }
        }
        self.create(session, child)
            .map(|entry| Lease::new(self.clone(), entry))
    }

    fn create(&self, session: &str, child: &str) -> Result<Entry, String> {
        let base = git(&self.project, &["rev-parse", "--verify", "HEAD^{commit}"])
            .map_err(|e| format!("a worktree needs a git repository with a commit: {e}"))?;
        let prefix = git(&self.project, &["rev-parse", "--show-prefix"])?;
        let short: String = session
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(8)
            .collect();
        let name = format!("{short}-{child}");
        let path = self.bhai.join(DIR).join(&name);
        let workdir = match prefix.is_empty() {
            true => path.clone(),
            false => path.join(&prefix),
        };
        let entry = Entry {
            workdir,
            path,
            branch: format!("bhai/{name}"),
            base,
            session: session.to_string(),
            child: child.to_string(),
            pid: std::process::id(),
            kept: false,
        };
        let dir = self.bhai.join(DIR);
        crate::sessions::private_dir(&dir).map_err(|e| e.to_string())?;
        // So the checkout's own `git status` and searches pass over them.
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            let _ = std::fs::write(&ignore, "*\n");
        }
        {
            let _lock = self.lock()?;
            let mut entries = self.load()?;
            entries.push(entry.clone());
            self.save(&entries)?;
        }
        let path = entry.path.to_string_lossy();
        let added = git(
            &self.project,
            &["worktree", "add", "-b", &entry.branch, &path, &entry.base],
        );
        if let Err(e) = added {
            let _lock = self.lock()?;
            let mut entries = self.load()?;
            entries.retain(|e| e.path != entry.path);
            self.save(&entries)?;
            return Err(format!("could not add a worktree: {e}"));
        }
        Ok(entry)
    }

    /// Settle the worktree at `path`: removed when it has nothing to lose, kept otherwise.
    fn release(&self, path: &Path) -> Outcome {
        let Ok(_lock) = self.lock() else {
            return Outcome::Kept("the registry could not be locked".to_string());
        };
        let mut entries = match self.load() {
            Ok(entries) => entries,
            Err(e) => return Outcome::Kept(e),
        };
        let Some(at) = entries.iter().position(|e| e.path == path) else {
            return Outcome::Gone;
        };
        let outcome = self.settle(&entries[at]);
        match outcome {
            Outcome::Removed => {
                entries.remove(at);
            }
            _ => entries[at].kept = true,
        }
        let _ = self.save(&entries);
        outcome
    }

    /// Settle every entry `pick` chooses, in one hold of the lock.
    fn sweep(&self, pick: impl Fn(&Entry) -> bool) -> Result<Vec<(Entry, Outcome)>, String> {
        let _lock = self.lock()?;
        let entries = self.load()?;
        let mut left = Vec::new();
        let mut settled = Vec::new();
        for mut entry in entries {
            if !pick(&entry) {
                left.push(entry);
                continue;
            }
            let outcome = self.settle(&entry);
            if outcome != Outcome::Removed {
                entry.kept = true;
                left.push(entry.clone());
            }
            settled.push((entry, outcome));
        }
        self.save(&left)?;
        Ok(settled)
    }

    /// Remove `entry`'s worktree and branch if nothing would be lost, without touching
    /// the registry. Any doubt keeps it.
    fn settle(&self, entry: &Entry) -> Outcome {
        let exists = entry.path.is_dir();
        let ahead = match self.ahead(entry, exists) {
            Ok(ahead) => ahead,
            Err(e) => return Outcome::Kept(format!("its commits could not be read: {e}")),
        };
        if !exists {
            // Deleted by hand: git still lists it until it is pruned.
            let _ = git(&self.project, &["worktree", "prune"]);
            if ahead > 0 {
                return Outcome::Kept(format!(
                    "its directory is gone, but the branch has {}",
                    count(ahead, "commit")
                ));
            }
            let _ = self.drop_branch(entry);
            return Outcome::Removed;
        }
        let changes = match git(&entry.path, &["status", "--porcelain"]) {
            Ok(out) => out.lines().count(),
            Err(e) => return Outcome::Kept(format!("its status could not be read: {e}")),
        };
        if changes > 0 || ahead > 0 {
            let mut why = Vec::new();
            if changes > 0 {
                why.push(count(changes, "uncommitted change"));
            }
            if ahead > 0 {
                why.push(count(ahead, "commit"));
            }
            return Outcome::Kept(why.join(" and "));
        }
        let path = entry.path.to_string_lossy();
        if let Err(e) = git(&self.project, &["worktree", "remove", &path]) {
            return Outcome::Kept(format!("git worktree remove failed: {e}"));
        }
        let _ = self.drop_branch(entry);
        Outcome::Removed
    }

    /// Commits on the branch, or at the worktree's `HEAD` should the child have moved
    /// it, that neither the base nor the checkout's `HEAD` has.
    fn ahead(&self, entry: &Entry, exists: bool) -> Result<usize, String> {
        let mut tips = Vec::new();
        let branch = format!("refs/heads/{}", entry.branch);
        if git(
            &self.project,
            &["rev-parse", "--verify", "--quiet", &branch],
        )
        .is_ok()
        {
            tips.push(branch);
        }
        if exists && let Ok(head) = git(&entry.path, &["rev-parse", "--verify", "HEAD"]) {
            tips.push(head);
        }
        if tips.is_empty() {
            return Ok(0);
        }
        let head = git(&self.project, &["rev-parse", "--verify", "HEAD"])?;
        let mut args = vec!["rev-list", "--count"];
        args.extend(tips.iter().map(String::as_str));
        args.extend(["--not", &entry.base, &head]);
        git(&self.project, &args)?
            .parse()
            .map_err(|e| format!("{e}"))
    }

    /// Only called once `ahead` is 0, so every commit on it is reachable elsewhere.
    fn drop_branch(&self, entry: &Entry) -> Result<String, String> {
        git(&self.project, &["branch", "-D", &entry.branch])
    }

    /// At startup: settle what sessions that have ended left behind, then prune what
    /// git still lists of worktrees deleted by hand. A project bhai never made a
    /// worktree in is not touched. Returns lines for the startup notices.
    pub fn startup(&self) -> Vec<String> {
        if !self.registry().exists() {
            return Vec::new();
        }
        let own = std::process::id();
        // This process has only just started, so an entry under its pid is a dead
        // process's that had the same one.
        let settled = match self.sweep(|e| e.pid == own || !alive(e.pid)) {
            Ok(settled) => settled,
            Err(e) => return vec![format!("worktrees: {e}")],
        };
        let _ = git(&self.project, &["worktree", "prune"]);
        let removed = settled
            .iter()
            .filter(|(_, o)| *o == Outcome::Removed)
            .count();
        let kept = settled.len() - removed;
        let mut lines = Vec::new();
        if removed > 0 {
            lines.push(format!(
                "removed {} that ended sessions left with no changes",
                count(removed, "worktree")
            ));
        }
        if kept > 0 {
            lines.push(format!(
                "{} of ended sessions kept with changes: /worktrees lists them",
                count(kept, "worktree")
            ));
        }
        lines
    }

    /// When the session quits: settle every worktree this process made, and say what
    /// was kept.
    pub fn quit(&self) -> Vec<String> {
        if !self.registry().exists() {
            return Vec::new();
        }
        let own = std::process::id();
        match self.sweep(|e| e.pid == own) {
            Ok(settled) => settled
                .iter()
                .filter_map(|(entry, outcome)| match outcome {
                    Outcome::Kept(why) => Some(kept(entry, why)),
                    _ => None,
                })
                .collect(),
            Err(e) => vec![format!("worktrees: {e}")],
        }
    }

    /// `/worktrees`: every worktree the registry lists, and how it stands.
    pub fn list(&self) -> String {
        let entries = match self.load() {
            Ok(entries) => entries,
            Err(e) => return format!("worktrees: {e}"),
        };
        if entries.is_empty() {
            return "no worktrees: a child gets one when the `agent` call asks for it".to_string();
        }
        let mut out = vec![format!("{}:", count(entries.len(), "worktree"))];
        for entry in &entries {
            let state = match (entry.kept, alive(entry.pid)) {
                (false, true) => "its child is running",
                (true, _) => "kept",
                (false, false) => "its session ended",
            };
            out.push(format!(
                "  {} (branch {}, child {}): {state}",
                entry.path.display(),
                entry.branch,
                entry.child
            ));
        }
        out.push("/worktrees clean removes the ones that have no changes".to_string());
        out.join("\n")
    }

    /// `/worktrees clean`: remove every one no child is working in that has nothing to
    /// lose.
    pub fn clean(&self) -> String {
        let settled = match self.sweep(|e| e.kept || !alive(e.pid)) {
            Ok(settled) => settled,
            Err(e) => return format!("worktrees: {e}"),
        };
        let removed = settled
            .iter()
            .filter(|(_, o)| *o == Outcome::Removed)
            .count();
        let mut out = vec![format!("removed {}", count(removed, "worktree"))];
        out.extend(settled.iter().filter_map(|(entry, outcome)| match outcome {
            Outcome::Kept(why) => Some(kept(entry, why)),
            _ => None,
        }));
        out.join("\n")
    }
}

/// The session's place, until [`settle`] has settled it.
static SESSION: Mutex<Option<Place>> = Mutex::new(None);

/// Make `place` the one [`settle`] settles, by a quit or by a signal.
pub fn settle_at_exit(place: Place) {
    *SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(place);
}

/// As the session quits or bhai dies of a signal: settle every worktree its children
/// made, and say on stderr which were kept and how to take their work. Only the first
/// call settles; one that comes while it runs waits for it to finish.
pub fn settle() {
    let mut session = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(place) = session.take() {
        for line in place.quit() {
            eprintln!("bhai: {line}");
        }
    }
}

/// A worktree a child is working in. Settled by `finish` when the child ends, or when
/// it is dropped otherwise: a task aborted, a panic, a runtime shut down under it.
pub struct Lease {
    place: Place,
    entry: Entry,
    settled: bool,
}

impl Lease {
    fn new(place: Place, entry: Entry) -> Self {
        Self {
            place,
            entry,
            settled: false,
        }
    }

    pub fn entry(&self) -> &Entry {
        &self.entry
    }

    /// Paths and the bash working directory moved into the worktree, for the child's
    /// registry.
    pub fn rooted(&self) -> Rooted {
        Rooted {
            project: self.place.project.clone(),
            workdir: self.entry.workdir.clone(),
        }
    }

    /// What the child is told before its task.
    pub fn note(&self) -> String {
        let (project, workdir) = (self.place.project.display(), self.entry.workdir.display());
        format!(
            "You work in a git worktree of your own at {workdir}, on branch {}, which \
stands in for {project}: make every change in it. A path you give under {project} is taken \
to mean the same place in the worktree, and `bash` starts there, but a command that \
`cd`s into {project} reaches the user's checkout. Commit your work on the branch when it is \
done, so it can be merged.",
            self.entry.branch
        )
    }

    pub fn finish(mut self) -> Outcome {
        self.settled = true;
        self.place.release(&self.entry.path)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.settled {
            let _ = self.place.release(&self.entry.path);
        }
    }
}

/// The line that says where a kept worktree is and how to take its work.
pub fn kept(entry: &Entry, why: &str) -> String {
    format!(
        "worktree {} (branch {}) was kept: {why}. Commit there if needed, then `git merge {}`; \
`/worktrees clean` removes it once merged",
        entry.path.display(),
        entry.branch,
        entry.branch
    )
}

/// What a child's report says of its worktree.
pub fn report(entry: &Entry, outcome: &Outcome) -> Option<String> {
    match outcome {
        Outcome::Removed => Some(format!(
            "Its worktree had no changes and was removed with branch {}.",
            entry.branch
        )),
        Outcome::Kept(why) => Some(format!("Its {}.", kept(entry, why))),
        Outcome::Gone => None,
    }
}

/// A child's calls moved into its worktree: a path under the project (but not under its
/// `.bhai`) to the same place in the worktree, a relative one onto the worktree, and
/// `bash` started there when the call names no directory.
#[derive(Debug, Clone, PartialEq)]
pub struct Rooted {
    pub project: PathBuf,
    pub workdir: PathBuf,
}

impl Rooted {
    pub fn args(&self, tool: &str, args: Value) -> Value {
        let start = self.workdir.display().to_string();
        Self::moved(tool, args, &start, |p| self.path(p))
    }

    /// Rooted `args` as the same call in the checkout, for the permission rules: they
    /// resolve against the project, so a worktree path would pass every rule anchored there.
    pub fn checked(&self, tool: &str, args: &Value) -> Value {
        let start = self.project.display().to_string();
        Self::moved(tool, args.clone(), &start, |p| self.back(p))
    }

    fn moved(
        tool: &str,
        mut args: Value,
        start: &str,
        path: impl Fn(&str) -> Option<String>,
    ) -> Value {
        use crate::tools::{bash, edit, patch, read, view_image, write};
        let Some(object) = args.as_object_mut() else {
            return args;
        };
        match tool {
            bash::NAME => {
                let given = object
                    .get("workdir")
                    .and_then(Value::as_str)
                    .filter(|d| !d.is_empty());
                let dir = match given {
                    Some(dir) => path(dir),
                    None => Some(start.to_string()),
                };
                if let Some(dir) = dir {
                    object.insert("workdir".to_string(), Value::String(dir));
                }
            }
            read::NAME | write::NAME | edit::NAME | view_image::NAME => {
                let moved = object.get("path").and_then(Value::as_str).and_then(&path);
                if let Some(moved) = moved {
                    object.insert("path".to_string(), Value::String(moved));
                }
            }
            patch::NAME => {
                if let Some(input) = object.get("input").and_then(Value::as_str) {
                    let input = Self::patch(input, &path);
                    object.insert("input".to_string(), Value::String(input));
                }
            }
            _ => {}
        }
        args
    }

    /// `path` moved into the worktree, or `None` when it stays where it is.
    fn path(&self, path: &str) -> Option<String> {
        let given = Path::new(path);
        let rest = match given.is_absolute() {
            false => given,
            true => {
                let rest = given.strip_prefix(&self.project).ok()?;
                if rest.starts_with(".bhai") {
                    return None;
                }
                rest
            }
        };
        let moved = match rest.as_os_str().is_empty() {
            true => self.workdir.clone(),
            false => self.workdir.join(rest),
        };
        Some(moved.display().to_string())
    }

    /// `path` under the worktree as the same place in the checkout, or `None` when it is
    /// not under the worktree. A relative one is taken against the worktree, where it runs.
    fn back(&self, path: &str) -> Option<String> {
        // Lexically, as the rules read paths: `..` out of the worktree leaves it.
        let mut given = PathBuf::new();
        for part in self.workdir.join(path).components() {
            match part {
                std::path::Component::ParentDir => {
                    given.pop();
                }
                std::path::Component::CurDir => {}
                part => given.push(part),
            }
        }
        let rest = given.strip_prefix(&self.workdir).ok()?;
        let back = match rest.as_os_str().is_empty() {
            true => self.project.clone(),
            false => self.project.join(rest),
        };
        Some(back.display().to_string())
    }

    fn patch(input: &str, path: impl Fn(&str) -> Option<String>) -> String {
        const HEADERS: [&str; 4] = [
            "*** Add File: ",
            "*** Delete File: ",
            "*** Update File: ",
            "*** Move to: ",
        ];
        let mut out: Vec<String> = Vec::new();
        for line in input.split('\n') {
            let moved = HEADERS.iter().find_map(|header| {
                let given = line.strip_prefix(header)?;
                Some(format!("{header}{}", path(given.trim_end())?))
            });
            out.push(moved.unwrap_or_else(|| line.to_string()));
        }
        out.join("\n")
    }
}

/// `n thing`, or `n things`.
fn count(n: usize, thing: &str) -> String {
    match n {
        1 => format!("1 {thing}"),
        _ => format!("{n} {thing}s"),
    }
}

/// Run git in `dir` and return its trimmed stdout, or its stderr as the error. The
/// variables that would point it at another repository are cleared.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    match out.status.success() {
        true => Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()),
        false => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    }
}

/// Whether process `pid` is running. One that exists but belongs to another user is.
#[allow(unsafe_code)]
fn alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists; nothing is sent.
    let found = unsafe { libc::kill(pid, 0) } == 0;
    found || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// An exclusive `flock` on the lock file, let go when the file closes.
struct Lock(#[allow(dead_code)] std::fs::File);

impl Lock {
    #[allow(unsafe_code)]
    fn take(file: std::fs::File) -> Result<Self, String> {
        use std::os::fd::AsRawFd as _;
        // SAFETY: the descriptor is the open file's, held for the call.
        let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0;
        match taken {
            true => Ok(Self(file)),
            false => Err(format!(
                "could not lock the worktree registry: {}",
                std::io::Error::last_os_error()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository with one commit, in a temp dir of its own.
    fn repo() -> PathBuf {
        let dir = crate::tools::temp_dir();
        run(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        run(&dir, &["add", "."]);
        commit(&dir, "first");
        dir
    }

    fn run(dir: &Path, args: &[&str]) -> String {
        git(dir, args).unwrap_or_else(|e| panic!("git {args:?}: {e}"))
    }

    fn commit(dir: &Path, message: &str) {
        run(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qam",
                message,
            ],
        );
    }

    fn branches(dir: &Path) -> String {
        run(dir, &["branch", "--format=%(refname:short)"])
    }

    fn worktrees(dir: &Path) -> usize {
        run(dir, &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count()
    }

    #[test]
    fn a_clean_worktree_goes_with_its_branch_when_its_child_ends() {
        let dir = repo();
        let place = Place::new(&dir);
        let lease = place.lease("session-1", "a1b2c3").unwrap();
        let entry = lease.entry().clone();
        // Listed before its child has done anything, and private.
        let listed = place.load().unwrap();
        assert_eq!(listed, vec![entry.clone()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(place.registry())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(entry.path.join("a.txt").is_file());
        assert_eq!(entry.branch, "bhai/session1-a1b2c3");
        assert!(branches(&dir).contains("bhai/session1-a1b2c3"));
        // The checkout's own status does not see it.
        let status = run(&dir, &["status", "--porcelain", "--untracked-files=all"]);
        assert!(!status.contains(".bhai/worktrees/"), "{status}");
        // Ignored build output is not a change.
        std::fs::create_dir_all(entry.path.join("target")).unwrap();
        std::fs::write(entry.path.join("target/out"), "x").unwrap();

        assert_eq!(lease.finish(), Outcome::Removed);
        assert!(!entry.path.exists());
        assert!(!branches(&dir).contains("bhai/"));
        assert_eq!(worktrees(&dir), 1);
        assert!(place.load().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dirty_worktree_is_kept_and_named_and_never_forced() {
        let dir = repo();
        let place = Place::new(&dir);
        let lease = place.lease("s", "c1").unwrap();
        let entry = lease.entry().clone();
        std::fs::write(entry.path.join("a.txt"), "two\n").unwrap();
        std::fs::write(entry.path.join("new.txt"), "new\n").unwrap();

        let outcome = lease.finish();
        assert_eq!(outcome, Outcome::Kept("2 uncommitted changes".to_string()));
        assert_eq!(
            std::fs::read_to_string(entry.path.join("a.txt")).unwrap(),
            "two\n"
        );
        let line = report(&entry, &outcome).unwrap();
        assert!(
            line.contains(&format!("`git merge {}`", entry.branch)),
            "{line}"
        );
        assert!(line.contains(&entry.path.display().to_string()), "{line}");
        let listed = place.load().unwrap();
        assert!(listed[0].kept, "{listed:?}");

        // `/worktrees clean` leaves it while it still has changes.
        let cleaned = place.clean();
        assert!(cleaned.starts_with("removed 0 worktrees"), "{cleaned}");
        assert!(entry.path.join("new.txt").is_file());

        // Committed and merged, it has nothing left to lose.
        run(&entry.path, &["add", "."]);
        commit(&entry.path, "work");
        let listed = place.list();
        assert!(listed.contains("kept"), "{listed}");
        assert_eq!(
            place.clean().lines().nth(1).unwrap_or_default(),
            kept(&entry, "1 commit")
        );
        run(&dir, &["merge", "-q", "--ff-only", &entry.branch]);
        assert_eq!(place.clean(), "removed 1 worktree");
        assert!(!entry.path.exists());
        assert!(!branches(&dir).contains("bhai/"));
        assert_eq!(
            place.list(),
            "no worktrees: a child gets one when the `agent` call asks for it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dropped_lease_is_settled_and_a_continued_child_gets_its_kept_worktree_back() {
        let dir = repo();
        let place = Place::new(&dir);
        {
            let lease = place.lease("s", "c1").unwrap();
            std::fs::write(lease.entry().path.join("b.txt"), "b\n").unwrap();
            // Dropped without `finish`, as an aborted task drops it.
        }
        let listed = place.load().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].kept);
        let again = place.lease("s", "c1").unwrap();
        assert_eq!(again.entry().path, listed[0].path);
        assert!(again.entry().path.join("b.txt").is_file());
        assert!(!place.load().unwrap()[0].kept, "in use again");
        std::fs::remove_file(again.entry().path.join("b.txt")).unwrap();
        drop(again);
        assert!(place.load().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A commit made on a detached `HEAD` would be lost with the worktree, though the
    /// branch itself has nothing new.
    #[test]
    fn a_commit_off_the_branch_keeps_the_worktree() {
        let dir = repo();
        let place = Place::new(&dir);
        let lease = place.lease("s", "c1").unwrap();
        let path = lease.entry().path.clone();
        run(&path, &["checkout", "-q", "--detach"]);
        std::fs::write(path.join("a.txt"), "detached\n").unwrap();
        commit(&path, "detached");
        assert_eq!(lease.finish(), Outcome::Kept("1 commit".to_string()));
        assert!(path.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quitting_settles_this_process_alone() {
        let dir = repo();
        let place = Place::new(&dir);
        let mine = place.lease("s", "c1").unwrap();
        let theirs = place.lease("t", "c2").unwrap();
        std::fs::write(theirs.entry().path.join("t.txt"), "t\n").unwrap();
        let (mine_path, theirs_entry) = (mine.entry().path.clone(), theirs.entry().clone());
        // The process that owns `t` is another, still running: this test's parent.
        let mut entries = place.load().unwrap();
        entries[1].pid = std::os::unix::process::parent_id();
        place.save(&entries).unwrap();
        std::mem::forget((mine, theirs));

        assert!(place.quit().is_empty());
        assert!(!mine_path.exists());
        assert!(theirs_entry.path.is_dir());
        let left = place.load().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].session, "t");
        assert!(!left[0].kept, "its child is still running in that process");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Where `crashes` points its process at.
    const CRASH: &str = "BHAI_TEST_WORKTREE_CRASH";

    /// Run by `a_crash_is_swept_at_the_next_startup` in a process of its own, which
    /// leases three worktrees and aborts, so neither a drop guard nor a quit runs.
    /// Without the variable it does nothing.
    #[test]
    fn crashes() {
        let Some(dir) = std::env::var_os(CRASH) else {
            return;
        };
        let place = Place::new(Path::new(&dir));
        let leases: Vec<Lease> = ["c1", "c2", "c3"]
            .into_iter()
            .map(|child| place.lease("s", child).unwrap())
            .collect();
        std::fs::write(leases[1].entry().path.join("a.txt"), "changed\n").unwrap();
        std::process::abort();
    }

    #[test]
    fn a_crash_is_swept_at_the_next_startup() {
        let dir = repo();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "worktrees::tests::crashes", "--nocapture"])
            .env(CRASH, &dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success());
        let place = Place::new(&dir);
        let entries = place.load().unwrap();
        assert_eq!(entries.len(), 3, "{entries:?}");
        assert!(entries.iter().all(|e| !e.kept && !alive(e.pid)));
        let (clean_entry, dirty_entry, gone_entry) =
            (entries[0].clone(), entries[1].clone(), entries[2].clone());
        // One deleted by hand since, which git still lists until it is pruned.
        std::fs::remove_dir_all(&gone_entry.path).unwrap();

        let lines = place.startup();
        assert_eq!(
            lines,
            vec![
                "removed 2 worktrees that ended sessions left with no changes".to_string(),
                "1 worktree of ended sessions kept with changes: /worktrees lists them".to_string(),
            ]
        );
        assert!(!clean_entry.path.exists());
        assert!(dirty_entry.path.join("a.txt").is_file());
        // Pruned, so git lists the checkout and the kept one.
        assert_eq!(worktrees(&dir), 2);
        let branches = branches(&dir);
        assert!(!branches.contains(&clean_entry.branch), "{branches}");
        assert!(!branches.contains(&gone_entry.branch), "{branches}");
        assert!(branches.contains(&dirty_entry.branch), "{branches}");
        let left = place.load().unwrap();
        assert_eq!(left.len(), 1);
        assert!(left[0].kept);
        let listed = place.list();
        assert!(
            listed.contains(&format!(
                "{} (branch {}",
                dirty_entry.path.display(),
                dirty_entry.branch
            )),
            "{listed}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Where `signalled` points its process at.
    const SIGNALLED: &str = "BHAI_TEST_WORKTREE_SIGNALLED";

    /// Run by `a_signal_settles_the_worktrees_before_bhai_dies` in a process of its own,
    /// which leases two worktrees, changes one, says so and waits to be signalled. Without
    /// the variable it does nothing.
    #[test]
    fn signalled() {
        let Some(dir) = std::env::var_os(SIGNALLED) else {
            return;
        };
        let dir = PathBuf::from(dir);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            crate::tools::bash::kill_all_on_signal().unwrap();
            let place = Place::new(&dir);
            settle_at_exit(place.clone());
            let leases = [
                place.lease("s", "c1").unwrap(),
                place.lease("s", "c2").unwrap(),
            ];
            std::fs::write(leases[1].entry().path.join("a.txt"), "changed\n").unwrap();
            std::fs::write(dir.join("ready"), "").unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            drop(leases);
        });
    }

    #[test]
    fn a_signal_settles_the_worktrees_before_bhai_dies() {
        use std::os::unix::process::ExitStatusExt as _;
        let dir = repo();
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "worktrees::tests::signalled", "--nocapture"])
            .env(SIGNALLED, &dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let begun = std::time::Instant::now();
        while !dir.join("ready").exists() {
            assert!(begun.elapsed().as_secs() < 20, "the child never got ready");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let place = Place::new(&dir);
        let entries = place.load().unwrap();
        assert_eq!(entries.len(), 2, "{entries:?}");
        #[allow(unsafe_code)]
        // SAFETY: only sends a signal, to the child started above.
        unsafe {
            libc::kill(i32::try_from(child.id()).unwrap(), libc::SIGTERM);
        }
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.signal(), Some(libc::SIGTERM));

        assert!(!entries[0].path.exists());
        assert!(!branches(&dir).contains(&entries[0].branch));
        let kept_entry = Entry {
            kept: true,
            ..entries[1].clone()
        };
        assert_eq!(place.load().unwrap(), vec![kept_entry]);
        assert!(entries[1].path.join("a.txt").is_file());
        let stderr = String::from_utf8_lossy(&out.stderr);
        let line = format!("bhai: {}", kept(&entries[1], "1 uncommitted change"));
        assert!(stderr.contains(&line), "{stderr}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_project_without_worktrees_is_not_touched() {
        let dir = crate::tools::temp_dir();
        let place = Place::new(&dir);
        assert!(place.startup().is_empty());
        assert!(place.quit().is_empty());
        assert!(!place.bhai.exists());
        // And outside a repository there is nothing to make one from.
        let err = place.lease("s", "c1").err().unwrap();
        assert!(
            err.starts_with("a worktree needs a git repository with a commit"),
            "{err}"
        );
        assert!(
            place.load().unwrap().is_empty(),
            "the entry it wrote first is gone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_subdirectory_works_in_the_same_place_in_the_worktree() {
        let dir = repo();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/s.txt"), "s\n").unwrap();
        run(&dir, &["add", "."]);
        commit(&dir, "sub");
        let place = Place::new(&dir.join("sub"));
        let lease = place.lease("s", "c1").unwrap();
        let entry = lease.entry().clone();
        assert_eq!(entry.workdir, entry.path.join("sub"));
        // A branch already there fails `worktree add`, after the entry was written.
        run(&dir, &["branch", "bhai/s-c2"]);
        let err = place.lease("s", "c2").err().unwrap();
        assert!(err.starts_with("could not add a worktree"), "{err}");
        assert_eq!(place.load().unwrap(), vec![entry.clone()]);
        assert!(entry.workdir.join("s.txt").is_file());
        assert_eq!(lease.finish(), Outcome::Removed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn calls_are_moved_into_the_worktree() {
        let rooted = Rooted {
            project: PathBuf::from("/p"),
            workdir: PathBuf::from("/p/.bhai/worktrees/s-c1"),
        };
        let args = |tool: &str, args: Value| rooted.args(tool, args);
        assert_eq!(
            args("bash", serde_json::json!({"command": "ls"})),
            serde_json::json!({"command": "ls", "workdir": "/p/.bhai/worktrees/s-c1"})
        );
        assert_eq!(
            args(
                "bash",
                serde_json::json!({"command": "ls", "workdir": "/p/src"})
            )["workdir"],
            "/p/.bhai/worktrees/s-c1/src"
        );
        assert_eq!(
            args(
                "bash",
                serde_json::json!({"command": "ls", "workdir": "/tmp"})
            )["workdir"],
            "/tmp"
        );
        assert_eq!(
            args(
                "write",
                serde_json::json!({"path": "/p/a.rs", "content": ""})
            )["path"],
            "/p/.bhai/worktrees/s-c1/a.rs"
        );
        assert_eq!(
            args("read", serde_json::json!({"path": "src/a.rs"}))["path"],
            "/p/.bhai/worktrees/s-c1/src/a.rs"
        );
        // Its own worktree, and the rest of `.bhai`, stay where they are.
        for path in [
            "/p/.bhai/worktrees/s-c1/a.rs",
            "/p/.bhai/MEMORY.md",
            "/etc/hosts",
        ] {
            assert_eq!(
                args("edit", serde_json::json!({"path": path}))["path"],
                path
            );
        }
        let patch = "*** Begin Patch\n*** Update File: /p/a.rs\n*** Move to: b.rs\n@@\n-x\n+y\n*** Add File: /tmp/c\n+z\n*** End Patch";
        assert_eq!(
            args("apply_patch", serde_json::json!({"input": patch}))["input"],
            "*** Begin Patch\n*** Update File: /p/.bhai/worktrees/s-c1/a.rs\n*** Move to: /p/.bhai/worktrees/s-c1/b.rs\n@@\n-x\n+y\n*** Add File: /tmp/c\n+z\n*** End Patch"
        );
        assert_eq!(
            args("web_search", serde_json::json!({"query": "/p/a"})),
            serde_json::json!({"query": "/p/a"})
        );
    }

    #[test]
    fn the_rules_see_a_call_as_it_would_be_in_the_checkout() {
        let rooted = Rooted {
            project: PathBuf::from("/p"),
            workdir: PathBuf::from("/p/.bhai/worktrees/s-c1"),
        };
        let checked = |tool: &str, args: Value| rooted.checked(tool, &rooted.args(tool, args));
        assert_eq!(
            checked("bash", serde_json::json!({"command": "ls"})),
            serde_json::json!({"command": "ls", "workdir": "/p"})
        );
        for (given, seen) in [
            ("/p/secrets/key", "/p/secrets/key"),
            ("secrets/key", "/p/secrets/key"),
            ("/p/.bhai/worktrees/s-c1/secrets/key", "/p/secrets/key"),
            (
                "/p/.bhai/worktrees/s-c1/src/../secrets/key",
                "/p/secrets/key",
            ),
            ("/p/.bhai/worktrees/s-c1", "/p"),
            // Out of the worktree, a path is what it is.
            (
                "/p/.bhai/worktrees/s-c1/../s-c2/a",
                "/p/.bhai/worktrees/s-c1/../s-c2/a",
            ),
            ("/p/.bhai/MEMORY.md", "/p/.bhai/MEMORY.md"),
            ("/etc/hosts", "/etc/hosts"),
        ] {
            assert_eq!(
                checked("edit", serde_json::json!({"path": given}))["path"],
                seen,
                "{given}"
            );
        }
        let patch = "*** Begin Patch\n*** Update File: /p/.bhai/worktrees/s-c1/a.rs\n*** Move to: b.rs\n@@\n-x\n+y\n*** End Patch";
        assert_eq!(
            checked("apply_patch", serde_json::json!({"input": patch}))["input"],
            "*** Begin Patch\n*** Update File: /p/a.rs\n*** Move to: /p/b.rs\n@@\n-x\n+y\n*** End Patch"
        );
    }
}
