//! bhai's own switches: `~/.config/bhai/config.toml`, then `.bhai/config.toml` in the
//! working directory on top, then command-line flags on top of both.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    /// `~/.claude/CLAUDE.md`.
    pub load_global_claude: bool,
    /// `~/.agents/AGENTS.md` and the Codex home's `AGENTS.md`.
    pub load_global_agents: bool,
    /// Instruction files from the repo root down to the working directory.
    pub load_project_instructions: bool,
    pub skills: bool,
    pub mcp: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            load_global_claude: true,
            load_global_agents: true,
            load_project_instructions: true,
            skills: true,
            mcp: false,
        }
    }
}

/// One config file: only the keys it sets override what came before.
#[derive(Debug, Default, Deserialize)]
struct Layer {
    load_global_claude: Option<bool>,
    load_global_agents: Option<bool>,
    load_project_instructions: Option<bool>,
    skills: Option<bool>,
    mcp: Option<bool>,
}

/// Command-line overrides.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Flags {
    pub no_global: bool,
    pub no_project: bool,
    /// Everything off.
    pub bare: bool,
}

impl Config {
    /// Read the global file under `home` (if any), then the project file under `cwd`.
    pub fn load(home: Option<&Path>, cwd: &Path) -> Result<Self> {
        let mut config = Self::default();
        if let Some(home) = home {
            config.apply_file(&home.join(".config/bhai/config.toml"))?;
        }
        config.apply_file(&cwd.join(".bhai/config.toml"))?;
        Ok(config)
    }

    pub fn with_flags(mut self, flags: Flags) -> Self {
        if flags.bare {
            return Self {
                load_global_claude: false,
                load_global_agents: false,
                load_project_instructions: false,
                skills: false,
                mcp: false,
            };
        }
        if flags.no_global {
            self.load_global_claude = false;
            self.load_global_agents = false;
        }
        if flags.no_project {
            self.load_project_instructions = false;
        }
        self
    }

    /// A missing file changes nothing; a malformed one is an error.
    fn apply_file(&mut self, path: &Path) -> Result<()> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Ok(());
        };
        let layer: Layer =
            toml::from_str(&text).with_context(|| format!("bad config {}", path.display()))?;
        self.apply(layer);
        Ok(())
    }

    fn apply(&mut self, layer: Layer) {
        let set = |field: &mut bool, value: Option<bool>| {
            if let Some(value) = value {
                *field = value;
            }
        };
        set(&mut self.load_global_claude, layer.load_global_claude);
        set(&mut self.load_global_agents, layer.load_global_agents);
        set(
            &mut self.load_project_instructions,
            layer.load_project_instructions,
        );
        set(&mut self.skills, layer.skills);
        set(&mut self.mcp, layer.mcp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bhai-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn defaults_when_no_files() {
        let dir = temp_dir();
        let config = Config::load(Some(&dir.join("home")), &dir.join("cwd")).unwrap();
        assert_eq!(config, Config::default());
        assert!(config.skills && !config.mcp);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn project_overrides_global_key_by_key() {
        let dir = temp_dir();
        let (home, cwd) = (dir.join("home"), dir.join("cwd"));
        write(
            &home.join(".config/bhai/config.toml"),
            "load_global_claude = false\nskills = false\nunknown = 3\n",
        );
        write(
            &cwd.join(".bhai/config.toml"),
            "skills = true\nmcp = true\n[other]\nx = 1\n",
        );
        let config = Config::load(Some(&home), &cwd).unwrap();
        assert_eq!(
            config,
            Config {
                load_global_claude: false,
                load_global_agents: true,
                load_project_instructions: true,
                skills: true,
                mcp: true,
            }
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn malformed_file_is_an_error() {
        let dir = temp_dir();
        write(&dir.join(".bhai/config.toml"), "skills = \"yes\"\n");
        assert!(Config::load(None, &dir).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn flags_override_the_files() {
        let base = Config {
            mcp: true,
            ..Config::default()
        };
        let no_global = base.with_flags(Flags {
            no_global: true,
            ..Flags::default()
        });
        assert!(!no_global.load_global_claude && !no_global.load_global_agents);
        assert!(no_global.load_project_instructions && no_global.mcp);

        let no_project = base.with_flags(Flags {
            no_project: true,
            ..Flags::default()
        });
        assert!(!no_project.load_project_instructions && no_project.load_global_claude);

        let bare = base.with_flags(Flags {
            bare: true,
            ..Flags::default()
        });
        assert!(
            !bare.load_global_claude
                && !bare.load_global_agents
                && !bare.load_project_instructions
                && !bare.skills
                && !bare.mcp
        );
    }
}
