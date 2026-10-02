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
        Lock::take(file, "the worktree registry")
    }

    /// The registry's rows as written, each checked by [`Place::checked`] before use: a
    /// clone can commit `.bhai`, so nothing in it is taken on trust.
    fn load(&self) -> Result<Vec<Value>, String> {
        let unparsed =
            |e: serde_json::Error| format!("{} does not parse: {e}", self.registry().display());
        match std::fs::read_to_string(self.registry()) {
            Ok(text) if text.trim().is_empty() => Ok(Vec::new()),
            Ok(text) => serde_json::from_str(&text).map_err(unparsed),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("could not read {}: {e}", self.registry().display())),
        }
    }

    /// Written aside and renamed over, so a crash mid-write leaves the old list whole.
    fn save(&self, rows: &[Value]) -> Result<(), String> {
        let text = serde_json::to_string_pretty(rows).map_err(|e| e.to_string())?;
        let aside = self
            .bhai
            .join(format!(".{REGISTRY}.{}.tmp", std::process::id()));
        crate::sessions::private_write(&aside, &text)
            .and_then(|()| std::fs::rename(&aside, self.registry()))
            .map_err(|e| format!("could not write {}: {e}", self.registry().display()))
    }

    /// The rows that are entries bhai could have written.
    #[cfg(test)]
    fn entries(&self) -> Result<Vec<Entry>, String> {
        Ok(self
            .load()?
            .iter()
            .filter_map(|row| self.checked(row).ok())
            .collect())
    }

    /// `row` as an entry, or why it is not one bhai could have written. Every field that
    /// reaches git or a child is held to the shape `create` gives it.
    fn checked(&self, row: &Value) -> Result<Entry, String> {
        let entry: Entry = serde_json::from_value(row.clone()).map_err(|e| e.to_string())?;
        let dir = self.bhai.join(DIR);
        let name = entry
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| entry.path == dir.join(name))
            .filter(|name| {
                !name.starts_with(['.', '-'])
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            });
        if name.is_none() {
            return Err(format!(
                "its path {:?} is not one under {}",
                entry.path,
                dir.display()
            ));
        }
        // Lexically under it is not enough: a symlink there could lead anywhere.
        for at in [&dir, &entry.path] {
            if std::fs::symlink_metadata(at).is_ok() && at.canonicalize().ok().as_ref() != Some(at)
            {
                return Err(format!("{} leads out of {}", at.display(), dir.display()));
            }
        }
        let inside = entry.workdir.strip_prefix(&entry.path).is_ok_and(|rest| {
            rest.components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
        });
        if !inside {
            return Err(format!(
                "its workdir {:?} is not in its worktree",
                entry.workdir
            ));
        }
        if !branch_name(&entry.branch)
            || git(
                &self.project,
                &["check-ref-format", &format!("refs/heads/{}", entry.branch)],
            )
            .is_err()
        {
            return Err(format!(
                "its branch {:?} is not one bhai names",
                entry.branch
            ));
        }
        let hex = entry
            .base
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
        if !(hex && matches!(entry.base.len(), 40 | 64)) {
            return Err(format!("its base {:?} is not a full object id", entry.base));
        }
        if entry.pid == 0 {
            return Err("its pid is 0".to_string());
        }
        for (field, text) in [("session", &entry.session), ("child", &entry.child)] {
            if text.is_empty() || text.chars().any(char::is_control) {
                return Err(format!("its {field} {text:?} is not one bhai names"));
            }
        }
        Ok(entry)
    }

    /// What `git worktree list` says of each worktree: its path and the branch checked
    /// out there, `None` when it is detached.
    fn listing(&self) -> Result<Vec<(PathBuf, Option<String>)>, String> {
        let out = git(&self.project, &["worktree", "list", "--porcelain", "-z"])?;
        let mut listed: Vec<(PathBuf, Option<String>)> = Vec::new();
        for field in out.split('\0') {
            if let Some(path) = field.strip_prefix("worktree ") {
                listed.push((PathBuf::from(path), None));
            } else if let Some(branch) = field.strip_prefix("branch ")
                && let Some(last) = listed.last_mut()
            {
                last.1 = Some(branch.to_string());
            }
        }
        Ok(listed)
    }

    /// A worktree for child `child` of `session`: the one it was kept in when it is
    /// continued, or a new one on a new branch from `HEAD`.
    pub fn lease(&self, session: &str, child: &str) -> Result<Lease, String> {
        {
            let _lock = self.lock()?;
            let mut rows = self.load()?;
            let listing = self.listing().unwrap_or_default();
            for row in &mut rows {
                let Ok(mut entry) = self.checked(row) else {
                    continue;
                };
                let ours = ours(&listing, &entry);
                if entry.session == session
                    && entry.child == child
                    && entry.kept
                    && ours
                    && entry.path.is_dir()
                {
                    (entry.kept, entry.pid) = (false, std::process::id());
                    *row = serde_json::to_value(&entry).map_err(|e| e.to_string())?;
                    self.save(&rows)?;
                    return Ok(Lease::new(self.clone(), entry));
                }
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
        let row = serde_json::to_value(&entry).map_err(|e| e.to_string())?;
        // Held to what a sweep would hold it to, so it is never written to be skipped.
        self.checked(&row)
            .map_err(|e| format!("could not add a worktree: {e}"))?;
        {
            let _lock = self.lock()?;
            let mut rows = self.load()?;
            rows.push(row.clone());
            self.save(&rows)?;
        }
        let path = entry.path.to_string_lossy();
        let added = git(
            &self.project,
            &[
                "worktree",
                "add",
                "-b",
                &entry.branch,
                "--end-of-options",
                &path,
                &entry.base,
            ],
        );
        if let Err(e) = added {
            let _lock = self.lock()?;
            let mut rows = self.load()?;
            rows.retain(|r| *r != row);
            self.save(&rows)?;
            return Err(format!("could not add a worktree: {e}"));
        }
        Ok(entry)
    }

    /// Settle the worktree at `path`: removed when it has nothing to lose, kept otherwise.
    fn release(&self, path: &Path) -> Outcome {
        let Ok(_lock) = self.lock() else {
            return Outcome::Kept("the registry could not be locked".to_string());
        };
        let mut rows = match self.load() {
            Ok(rows) => rows,
            Err(e) => return Outcome::Kept(e),
        };
        let at = rows
            .iter()
            .position(|row| row.get("path").and_then(Value::as_str) == path.to_str());
        let Some(at) = at else {
            return Outcome::Gone;
        };
        let entry = match self.checked(&rows[at]) {
            Ok(entry) => entry,
            Err(e) => return Outcome::Kept(format!("its registry entry was changed: {e}")),
        };
        let listing = match self.listing() {
            Ok(listing) => listing,
            Err(e) => return Outcome::Kept(format!("git could not list its worktrees: {e}")),
        };
        let outcome = self.settle(&entry, &listing);
        match outcome {
            Outcome::Removed => {
                rows.remove(at);
            }
            _ => rows[at]["kept"] = Value::Bool(true),
        }
        let _ = self.save(&rows);
        outcome
    }

    /// Settle every entry `pick` chooses, in one hold of the lock. A row that is not an
    /// entry bhai could have written is left as it is and said.
    fn sweep(&self, pick: impl Fn(&Entry) -> bool) -> Result<Swept, String> {
        let _lock = self.lock()?;
        let rows = self.load()?;
        // Taken once, before any prune, so a worktree pruned for one entry still shows
        // whose it was for the next.
        let listing = self.listing()?;
        let mut left = Vec::new();
        let mut swept = Swept::default();
        for (at, row) in rows.into_iter().enumerate() {
            let mut entry = match self.checked(&row) {
                Ok(entry) => entry,
                Err(why) => {
                    swept.skipped.push(skipped(at, &why));
                    left.push(row);
                    continue;
                }
            };
            if !pick(&entry) {
                left.push(row);
                continue;
            }
            let outcome = self.settle(&entry, &listing);
            if outcome != Outcome::Removed {
                entry.kept = true;
                left.push(serde_json::to_value(&entry).map_err(|e| e.to_string())?);
            }
            swept.settled.push((entry, outcome));
        }
        self.save(&left)?;
        Ok(swept)
    }

    /// Remove `entry`'s worktree and branch if nothing would be lost, without touching
    /// the registry. Any doubt keeps it, and neither is touched unless `listing` has
    /// that branch checked out at that path.
    fn settle(&self, entry: &Entry, listing: &[(PathBuf, Option<String>)]) -> Outcome {
        let listed = listing.iter().find(|(path, _)| *path == entry.path);
        let ours = ours(listing, entry);
        let exists = entry.path.is_dir();
        if exists && listed.is_none() {
            return Outcome::Kept("git does not list it as a worktree".to_string());
        }
        let ahead = match self.ahead(entry, exists) {
            Ok(ahead) => ahead,
            Err(e) => return Outcome::Kept(format!("its commits could not be read: {e}")),
        };
        if !exists {
            // Deleted by hand: git still lists it until it is pruned.
            let _ = git(&self.project, &["worktree", "prune"]);
            let branch = self.branch_exists(entry);
            if ahead > 0 {
                return Outcome::Kept(format!(
                    "its directory is gone, but the branch has {}",
                    count(ahead, "commit")
                ));
            }
            if branch && !ours {
                return Outcome::Kept(format!(
                    "its directory is gone and git did not have branch {} checked out there, so \
the branch is left as it is",
                    entry.branch
                ));
            }
            if branch {
                let _ = self.drop_branch(entry);
            }
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
        if !ours {
            return Outcome::Kept(format!("it is not on branch {}", entry.branch));
        }
        let path = entry.path.to_string_lossy();
        let removed = git(
            &self.project,
            &["worktree", "remove", "--end-of-options", &path],
        );
        if let Err(e) = removed {
            return Outcome::Kept(format!("git worktree remove failed: {e}"));
        }
        let _ = self.drop_branch(entry);
        Outcome::Removed
    }

    fn branch_exists(&self, entry: &Entry) -> bool {
        let branch = format!("refs/heads/{}", entry.branch);
        git(
            &self.project,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &branch,
            ],
        )
        .is_ok()
    }

    /// Commits on the branch, or at the worktree's `HEAD` should the child have moved
    /// it, that neither the base nor the checkout's `HEAD` has.
    fn ahead(&self, entry: &Entry, exists: bool) -> Result<usize, String> {
        let mut tips = Vec::new();
        if self.branch_exists(entry) {
            tips.push(format!("refs/heads/{}", entry.branch));
        }
        if exists && let Ok(head) = git(&entry.path, &["rev-parse", "--verify", "HEAD"]) {
            tips.push(head);
        }
        if tips.is_empty() {
            return Ok(0);
        }
        let head = git(&self.project, &["rev-parse", "--verify", "HEAD"])?;
        let not = [format!("^{}", entry.base), format!("^{head}")];
        let mut args = vec!["rev-list", "--count", "--end-of-options"];
        args.extend(tips.iter().chain(&not).map(String::as_str));
        git(&self.project, &args)?
            .parse()
            .map_err(|e| format!("{e}"))
    }

    /// Only called once `ahead` is 0, so every commit on it is reachable elsewhere.
    fn drop_branch(&self, entry: &Entry) -> Result<String, String> {
        git(
            &self.project,
            &["branch", "-D", "--end-of-options", &entry.branch],
        )
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
        let swept = match self.sweep(|e| e.pid == own || !alive(e.pid)) {
            Ok(swept) => swept,
            Err(e) => return vec![format!("worktrees: {e}")],
        };
        let _ = git(&self.project, &["worktree", "prune"]);
        let removed = swept
            .settled
            .iter()
            .filter(|(_, o)| *o == Outcome::Removed)
            .count();
        let kept = swept.settled.len() - removed;
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
        lines.extend(
            swept
                .skipped
                .iter()
                .map(|line| format!("worktrees: {line}")),
        );
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
            Ok(swept) => swept
                .settled
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
        let rows = match self.load() {
            Ok(rows) => rows,
            Err(e) => return format!("worktrees: {e}"),
        };
        if rows.is_empty() {
            return "no worktrees: a child gets one when the `agent` call asks for it".to_string();
        }
        let mut out = vec![format!("{}:", count(rows.len(), "worktree"))];
        for (at, row) in rows.iter().enumerate() {
            let entry = match self.checked(row) {
                Ok(entry) => entry,
                Err(why) => {
                    out.push(format!("  {}", skipped(at, &why)));
                    continue;
                }
            };
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
        let swept = match self.sweep(|e| e.kept || !alive(e.pid)) {
            Ok(swept) => swept,
            Err(e) => return format!("worktrees: {e}"),
        };
        let removed = swept
            .settled
            .iter()
            .filter(|(_, o)| *o == Outcome::Removed)
            .count();
        let mut out = vec![format!("removed {}", count(removed, "worktree"))];
        out.extend(
            swept
                .settled
                .iter()
                .filter_map(|(entry, outcome)| match outcome {
                    Outcome::Kept(why) => Some(kept(entry, why)),
                    _ => None,
                }),
        );
        out.extend(swept.skipped);
        out.join("\n")
    }
}

/// What a sweep did: the entries it settled, and a line for each row it left alone.
#[derive(Default)]
struct Swept {
    settled: Vec<(Entry, Outcome)>,
    skipped: Vec<String>,
}

/// The line for row `at` of the registry, left alone for `why`.
fn skipped(at: usize, why: &str) -> String {
    format!("entry {} of .bhai/{REGISTRY} was left alone: {why}", at + 1)
}

/// Whether git has `entry`'s branch checked out at its path.
fn ours(listing: &[(PathBuf, Option<String>)], entry: &Entry) -> bool {
    let branch = format!("refs/heads/{}", entry.branch);
    listing
        .iter()
        .any(|(path, on)| *path == entry.path && on.as_deref() == Some(branch.as_str()))
}

/// A branch of the shape `create` names: `bhai/` and a name of plain characters, which
/// no git command can take for an option or a revision expression.
fn branch_name(branch: &str) -> bool {
    let Some(name) = branch.strip_prefix("bhai/") else {
        return false;
    };
    !name.is_empty()
        && !name.starts_with(['.', '-'])
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.ends_with(".lock")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
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
pub(crate) struct Lock(#[allow(dead_code)] std::fs::File);

impl Lock {
    #[allow(unsafe_code)]
    pub(crate) fn take(file: std::fs::File, what: &str) -> Result<Self, String> {
        use std::os::fd::AsRawFd as _;
        // SAFETY: the descriptor is the open file's, held for the call.
        let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0;
        match taken {
            true => Ok(Self(file)),
            false => Err(format!(
                "could not lock {what}: {}",
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
        let listed = place.entries().unwrap();
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
        assert!(place.entries().unwrap().is_empty());
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
        let listed = place.entries().unwrap();
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
        let listed = place.entries().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].kept);
        let again = place.lease("s", "c1").unwrap();
        assert_eq!(again.entry().path, listed[0].path);
        assert!(again.entry().path.join("b.txt").is_file());
        assert!(!place.entries().unwrap()[0].kept, "in use again");
        std::fs::remove_file(again.entry().path.join("b.txt")).unwrap();
        drop(again);
        assert!(place.entries().unwrap().is_empty());
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
        let mut rows = place.load().unwrap();
        rows[1]["pid"] = std::os::unix::process::parent_id().into();
        place.save(&rows).unwrap();
        std::mem::forget((mine, theirs));

        assert!(place.quit().is_empty());
        assert!(!mine_path.exists());
        assert!(theirs_entry.path.is_dir());
        let left = place.entries().unwrap();
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
        let entries = place.entries().unwrap();
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
        let left = place.entries().unwrap();
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
        let entries = place.entries().unwrap();
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
        assert_eq!(place.entries().unwrap(), vec![kept_entry]);
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
            place.entries().unwrap().is_empty(),
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
        assert_eq!(place.entries().unwrap(), vec![entry.clone()]);
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

    /// The registry sits in the project, which a clone can commit: every field of a row
    /// that is not one bhai could have written is kept from git, and a row that is, but
    /// names a branch and worktree git does not have together, touches neither.
    #[test]
    fn a_hostile_registry_reaches_no_git_command() {
        let dir = repo();
        let place = Place::new(&dir);
        let base = run(&dir, &["rev-parse", "HEAD"]);
        let outside = crate::tools::temp_dir().canonicalize().unwrap();
        let victim = outside.join("victim");
        std::fs::write(&victim, "keep\n").unwrap();
        let output = format!("--output={}", victim.display());
        let worktrees_dir = place.bhai.join(DIR);
        std::fs::create_dir_all(&worktrees_dir).unwrap();
        // The user's own branches and worktrees, all merged and clean.
        run(&dir, &["branch", "feature"]);
        run(&dir, &["branch", "bhai/mine"]);
        let foreign = outside.join("foreign");
        let foreign_arg = foreign.display().to_string();
        run(&dir, &["worktree", "add", "-q", &foreign_arg, "feature"]);
        let user = worktrees_dir.join("user");
        let user_arg = user.display().to_string();
        run(
            &dir,
            &["worktree", "add", "-q", "-b", "bhai/user", &user_arg],
        );
        std::os::unix::fs::symlink(&foreign, worktrees_dir.join("link")).unwrap();

        let row = |path: &Path, branch: &str, base: &str| {
            serde_json::json!({
                "path": path, "workdir": path, "branch": branch, "base": base,
                "session": "old", "child": "c0", "pid": 999_999_999u32, "kept": true,
            })
        };
        let ghost = worktrees_dir.join("ghost");
        let with = |mut row: Value, field: &str, value: Value| {
            row[field] = value;
            row
        };
        let valid = row(&ghost, "bhai/ghost", &base);
        let up = ghost.join("..").display().to_string();
        let hostile: Vec<(Value, &str)> = vec![
            (row(&ghost, "bhai/ghost", &output), "its base"),
            (row(&ghost, "bhai/ghost", "HEAD"), "its base"),
            (row(&ghost, "bhai/ghost", &base[..12]), "its base"),
            (row(&ghost, &output, &base), "its branch"),
            (row(&ghost, "feature", &base), "its branch"),
            (row(&ghost, "bhai/../feature", &base), "its branch"),
            (row(&ghost, "bhai/x@{-1}", &base), "its branch"),
            (row(&dir, "bhai/ghost", &base), "its path"),
            (row(&foreign, "bhai/ghost", &base), "its path"),
            (
                row(&worktrees_dir.join("../.."), "bhai/ghost", &base),
                "its path",
            ),
            (row(Path::new(&output), "bhai/ghost", &base), "its path"),
            (
                row(&worktrees_dir.join("-x"), "bhai/ghost", &base),
                "its path",
            ),
            (
                row(&worktrees_dir.join("link"), "bhai/ghost", &base),
                "leads out",
            ),
            (with(valid.clone(), "workdir", "/etc".into()), "its workdir"),
            (with(valid.clone(), "workdir", up.into()), "its workdir"),
            (with(valid.clone(), "pid", "1".into()), "u32"),
            (with(valid.clone(), "pid", (-1).into()), "u32"),
            (with(valid.clone(), "pid", 0.into()), "its pid is 0"),
            (with(valid, "child", "c\u{1b}[2J".into()), "its child"),
            ("--output".into(), "invalid type"),
        ];
        // Well formed, but git does not have that branch checked out there.
        let mut rows: Vec<Value> = hostile.iter().map(|(row, _)| row.clone()).collect();
        rows.push(row(&user, "bhai/mine", &base));
        rows.push(row(&ghost, "bhai/mine", &base));
        place.save(&rows).unwrap();

        let snapshot = |dir: &Path| {
            (
                branches(dir),
                run(dir, &["worktree", "list", "--porcelain"]),
                std::fs::read_to_string(&victim).unwrap(),
            )
        };
        let before = snapshot(&dir);
        let lines = place.startup();
        let cleaned = place.clean();
        let listed = place.list();
        // A continued child is not handed a hostile row's workdir.
        let lease = place.lease("old", "c0").unwrap();
        assert_eq!(lease.entry().path, worktrees_dir.join("old-c0"));
        assert_eq!(lease.finish(), Outcome::Removed);
        assert_eq!(snapshot(&dir), before);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep\n");
        assert!(foreign.join("a.txt").is_file() && user.join("a.txt").is_file());
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .chain(std::fs::read_dir(&outside).unwrap())
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with('-'))
            .collect();
        assert!(strays.is_empty(), "{strays:?}");

        for (at, (_, why)) in hostile.iter().enumerate() {
            let line = lines
                .iter()
                .find(|line| line.starts_with(&format!("worktrees: entry {} of", at + 1)))
                .unwrap_or_else(|| panic!("entry {}: {lines:?}", at + 1));
            assert!(line.contains(why), "{line}");
            assert!(!line.contains('\u{1b}'), "{line}");
            let line = &line["worktrees: ".len()..];
            assert!(cleaned.contains(line), "{cleaned}");
            assert!(listed.contains(line), "{listed}");
        }
        let kept = "2 worktrees of ended sessions kept with changes: /worktrees lists them";
        assert!(lines.contains(&kept.to_string()), "{lines:?}");
        assert!(
            cleaned.contains("it is not on branch bhai/mine"),
            "{cleaned}"
        );
        assert!(
            cleaned.contains("so the branch is left as it is"),
            "{cleaned}"
        );
        // Nor is any row dropped: they are the user's to look at.
        let left = place.load().unwrap();
        assert_eq!(left.len(), rows.len());
        assert_eq!(left[..hostile.len()], rows[..hostile.len()]);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
