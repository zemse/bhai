//! Trust in the allow rules a repo supplies: `trust.json` in bhai's config directory maps
//! each project root to a hash of the files those rules come from.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
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

    /// Whether the stored hash matches the files as they are now.
    pub fn is_trusted(&self) -> bool {
        read(&self.store).is_ok_and(|entries| entries.get(&self.root) == Some(&self.hash()))
    }

    /// Record the files as they are now.
    pub fn trust(&self) -> Result<()> {
        let mut entries = read(&self.store)?;
        entries.insert(self.root.clone(), self.hash());
        write(&self.store, &entries)
    }

    /// Forget the project; false if it had no entry.
    pub fn untrust(&self) -> Result<bool> {
        let mut entries = read(&self.store)?;
        if entries.remove(&self.root).is_none() {
            return Ok(false);
        }
        write(&self.store, &entries).map(|()| true)
    }

    /// Each allow rule the files hold, with the file it is in.
    pub fn allow_rules(&self) -> Vec<(&'static str, String)> {
        self.sources()
            .flat_map(|source| {
                settings::allow_texts(&self.cwd.join(source))
                    .into_iter()
                    .map(move |rule| (source, rule))
            })
            .collect()
    }

    /// The allow rules the files hold now, parsed as startup loads them.
    pub fn load_allow(&self) -> Vec<Rule> {
        let mut rules = settings::load_local(&self.cwd.join(settings::LOCAL)).0;
        if self.claude {
            let (claude, _) = settings::claude(None, &self.cwd);
            rules.extend(claude.allow.into_iter().filter(|r| r.repo));
        }
        rules
    }

    /// The files whose allow rules are imported.
    fn sources(&self) -> impl Iterator<Item = &'static str> {
        let claude = self.claude;
        SOURCES
            .into_iter()
            .filter(move |&source| claude || source != settings::CLAUDE_LOCAL)
    }

    /// sha256 of the files, each prefixed with its length; a missing file is empty.
    fn hash(&self) -> String {
        let mut hasher = Sha256::new();
        for source in SOURCES {
            let bytes = std::fs::read(self.cwd.join(source)).unwrap_or_default();
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
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
        assert!(!trust.is_trusted());
        assert!(!trust.untrust().unwrap());
        trust.trust().unwrap();
        assert!(trust.is_trusted());
        let local = repo.join(settings::CLAUDE_LOCAL);
        std::fs::write(&local, r#"{"permissions": {"allow": ["Bash"]}}"#).unwrap();
        assert!(!trust.is_trusted());
        assert_eq!(
            trust.allow_rules(),
            [(settings::CLAUDE_LOCAL, "Bash".to_string())]
        );
        trust.trust().unwrap();
        assert!(trust.is_trusted());
        assert!(trust.untrust().unwrap());
        assert!(!trust.is_trusted());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
