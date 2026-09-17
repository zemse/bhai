//! Trust in the allow rules a repo supplies: `trust.json` in bhai's config directory maps
//! each project root to a hash of the files those rules come from.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{Rule, settings};

/// The trust store, under bhai's config directory.
const FILE: &str = "trust.json";

/// The project files whose allow rules need trust.
const SOURCES: [&str; 2] = [settings::LOCAL, settings::CLAUDE_LOCAL];

/// One project's entry in the trust store.
#[derive(Debug, Clone)]
pub struct Trust {
    store: PathBuf,
    cwd: PathBuf,
    /// The canonical project root, as the store keys it.
    root: String,
    /// Whether rules from `.claude/settings.local.json` are imported.
    claude: bool,
}

impl Trust {
    pub fn new(config_dir: &Path, cwd: &Path) -> Self {
        let root = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        Self {
            store: config_dir.join(FILE),
            cwd: cwd.to_path_buf(),
            root: root.display().to_string(),
            claude: true,
        }
    }

    pub fn with_claude(self, claude: bool) -> Self {
        Self { claude, ..self }
    }

    /// Whether the stored hash matches `snapshot`.
    pub fn matches(&self, snapshot: &Snapshot) -> bool {
        read(&self.store).is_ok_and(|entries| entries.get(&self.root) == Some(&snapshot.hash()))
    }

    /// Record the files as they are now, and return what was recorded.
    pub fn trust(&self) -> Result<Snapshot> {
        let snapshot = self.snapshot();
        let mut entries = read(&self.store)?;
        entries.insert(self.root.clone(), snapshot.hash());
        write(&self.store, &entries).map(|()| snapshot)
    }

    /// Forget the project; false if it had no entry.
    pub fn untrust(&self) -> Result<bool> {
        let mut entries = read(&self.store)?;
        if entries.remove(&self.root).is_none() {
            return Ok(false);
        }
        write(&self.store, &entries).map(|()| true)
    }

    /// The files as they are now, each read once.
    pub fn snapshot(&self) -> Snapshot {
        let files = SOURCES.map(|source| std::fs::read(self.cwd.join(source)).ok());
        Snapshot {
            cwd: self.cwd.clone(),
            claude: self.claude,
            files,
        }
    }
}

/// The files' bytes from one read, so the hash, the rules and the listing agree.
#[derive(Debug, Clone)]
pub struct Snapshot {
    cwd: PathBuf,
    claude: bool,
    /// Each of [`SOURCES`]; `None` if it could not be read.
    files: [Option<Vec<u8>>; 2],
}

impl Snapshot {
    /// Each allow rule the files hold, with the file it is in.
    pub fn allow_rules(&self) -> Vec<(&'static str, String)> {
        self.sources()
            .flat_map(|(source, settings)| {
                settings::allow_texts(&settings)
                    .into_iter()
                    .map(move |rule| (source, rule))
            })
            .collect()
    }

    /// The allow rules the files hold, parsed as startup loads them.
    pub fn load_allow(&self) -> Vec<Rule> {
        let mut rules = Vec::new();
        for (source, settings) in self.sources() {
            let path = self.cwd.join(source);
            if source == settings::LOCAL {
                rules.extend(settings::local_rules(&path, Ok(settings)).0);
            } else {
                rules.extend(settings::claude_repo_allow(&path, &settings));
            }
        }
        rules
    }

    /// The imported files, parsed; one that cannot be read or parsed is empty.
    fn sources(&self) -> impl Iterator<Item = (&'static str, Value)> {
        SOURCES
            .into_iter()
            .zip(&self.files)
            .filter(|&(source, _)| self.claude || source != settings::CLAUDE_LOCAL)
            .map(|(source, bytes)| {
                let path = self.cwd.join(source);
                let parsed = bytes.as_deref().map(|b| settings::parse(&path, b));
                (source, parsed.and_then(Result::ok).unwrap_or_default())
            })
    }

    /// sha256 of the files, each prefixed with its length; a missing file is empty.
    fn hash(&self) -> String {
        let mut hasher = Sha256::new();
        for bytes in &self.files {
            let bytes = bytes.as_deref().unwrap_or_default();
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        }
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// The store's entries, or none if there is no file.
fn read(path: &Path) -> Result<BTreeMap<String, String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("bad trust store {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write(path: &Path, entries: &BTreeMap<String, String>) -> Result<()> {
    let mut text = serde_json::to_string_pretty(entries)?;
    text.push('\n');
    settings::write_atomic(path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_follows_the_files() {
        let dir = std::env::temp_dir().join(format!("bhai-trust-{}", uuid::Uuid::new_v4()));
        let (config, repo) = (dir.join("config"), dir.join("repo"));
        std::fs::create_dir_all(repo.join(".claude")).unwrap();
        let trust = Trust::new(&config, &repo);
        assert!(!trust.matches(&trust.snapshot()));
        assert!(!trust.untrust().unwrap());
        trust.trust().unwrap();
        assert!(trust.matches(&trust.snapshot()));
        let local = repo.join(settings::CLAUDE_LOCAL);
        std::fs::write(&local, r#"{"permissions": {"allow": ["Bash"]}}"#).unwrap();
        assert!(!trust.matches(&trust.snapshot()));
        assert_eq!(
            trust.snapshot().allow_rules(),
            [(settings::CLAUDE_LOCAL, "Bash".to_string())]
        );
        trust.trust().unwrap();
        assert!(trust.matches(&trust.snapshot()));
        assert!(trust.untrust().unwrap());
        assert!(!trust.matches(&trust.snapshot()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_snapshot_keeps_the_bytes_it_read() {
        let dir = std::env::temp_dir().join(format!("bhai-trust-{}", uuid::Uuid::new_v4()));
        let (config, repo) = (dir.join("config"), dir.join("repo"));
        let (local, claude) = (
            repo.join(settings::LOCAL),
            repo.join(settings::CLAUDE_LOCAL),
        );
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
        std::fs::write(&local, r#"{"permissions": {"allow": ["Bash(make:*)"]}}"#).unwrap();
        std::fs::write(&claude, r#"{"permissions": {"allow": ["Bash(npm test)"]}}"#).unwrap();
        let trust = Trust::new(&config, &repo);
        let snapshot = trust.trust().unwrap();

        // A write after the read changes neither the rules nor the recorded hash.
        std::fs::write(&local, r#"{"permissions": {"allow": ["Bash(curl:*)"]}}"#).unwrap();
        std::fs::write(&claude, "{}").unwrap();
        assert_eq!(
            snapshot.allow_rules(),
            [
                (settings::LOCAL, "Bash(make:*)".to_string()),
                (settings::CLAUDE_LOCAL, "Bash(npm test)".to_string()),
            ]
        );
        let loaded: Vec<_> = snapshot.load_allow().into_iter().map(|r| r.text).collect();
        assert_eq!(loaded, ["Bash(make:*)", "Bash(npm test)"]);
        assert!(snapshot.load_allow().iter().all(|r| r.repo));
        assert!(trust.matches(&snapshot));
        assert!(!trust.matches(&trust.snapshot()));
        let without_claude = trust.with_claude(false).snapshot();
        assert_eq!(
            without_claude.allow_rules(),
            [(settings::LOCAL, "Bash(curl:*)".to_string())]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
