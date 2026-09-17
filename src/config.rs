//! bhai's own switches: `~/.config/bhai/config.toml`, then `.bhai/config.toml` in the
//! working directory on top, then command-line flags on top of both. The project file
//! may only tighten permissions: its `permission_mode` and `allow` rules are ignored, so
//! a cloned repo cannot approve its own commands. MCP servers are likewise only read
//! from the global file.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::permissions::{Mode, Rule, Rules};

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// `~/.claude/CLAUDE.md`.
    pub load_global_claude: bool,
    /// `~/.agents/AGENTS.md` and the Codex home's `AGENTS.md`.
    pub load_global_agents: bool,
    /// Instruction files from the repo root down to the working directory.
    pub load_project_instructions: bool,
    pub skills: bool,
    /// Where skills are discovered from.
    pub skill_sources: Vec<Source>,
    pub mcp: bool,
    /// `[mcp.servers.<name>]` from the global file.
    pub mcp_servers: BTreeMap<String, McpServer>,
    pub permission_mode: Mode,
    pub permissions: Rules,
    /// Rules from Claude Code's `settings.json` files.
    pub import_claude_permissions: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            load_global_claude: true,
            load_global_agents: true,
            load_project_instructions: true,
            skills: true,
            skill_sources: Source::ALL.to_vec(),
            mcp: false,
            mcp_servers: BTreeMap::new(),
            permission_mode: Mode::Ask,
            permissions: Rules::default(),
            import_claude_permissions: true,
        }
    }
}

/// A place context comes from: the global Claude dir, the global agents dir, or the project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    GlobalClaude,
    GlobalAgents,
    Project,
}

impl Source {
    pub const ALL: [Source; 3] = [Source::GlobalClaude, Source::GlobalAgents, Source::Project];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    pub fn name(self) -> &'static str {
        match self {
            Source::GlobalClaude => "global_claude",
            Source::GlobalAgents => "global_agents",
            Source::Project => "project",
        }
    }
}

/// A stdio MCP server started with `command` and `args`, with `env` added.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct McpServer {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// One config file: only the keys it sets override what came before.
#[derive(Debug, Default, Deserialize)]
struct Layer {
    load_global_claude: Option<bool>,
    load_global_agents: Option<bool>,
    load_project_instructions: Option<bool>,
    skills: Option<SkillsLayer>,
    mcp: Option<McpLayer>,
    permission_mode: Option<Mode>,
    import_claude_permissions: Option<bool>,
    #[serde(default)]
    permissions: RulesLayer,
}

/// `skills = false`, or a `[skills]` table.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SkillsLayer {
    Enabled(bool),
    Table {
        enabled: Option<bool>,
        sources: Option<Vec<Source>>,
    },
}

/// `mcp = true`, or an `[mcp]` table with servers.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum McpLayer {
    Enabled(bool),
    Table {
        enabled: Option<bool>,
        #[serde(default)]
        servers: BTreeMap<String, McpServer>,
    },
}

/// `[permissions]`: rule lists add up across files.
#[derive(Debug, Default, Deserialize)]
struct RulesLayer {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
    #[serde(default)]
    ask: Vec<String>,
}

/// Command-line overrides.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Flags {
    pub no_global: bool,
    pub no_project: bool,
    /// Everything off.
    pub bare: bool,
    /// `--mode`.
    pub mode: Option<Mode>,
}

impl Config {
    /// Read the global file under `home` (if any), then the project file under `cwd`.
    pub fn load(home: Option<&Path>, cwd: &Path) -> Result<Self> {
        let mut config = Self::default();
        if let Some(home) = home {
            config.apply_file(&home.join(".config/bhai/config.toml"), true)?;
        }
        config.apply_file(&cwd.join(".bhai/config.toml"), false)?;
        Ok(config)
    }

    /// `--bare` turns off everything that adds context, and the Claude Code rule import;
    /// the rest of the permissions stay as configured.
    pub fn with_flags(mut self, flags: Flags) -> Self {
        if let Some(mode) = flags.mode {
            self.permission_mode = mode;
        }
        if flags.bare {
            return Self {
                load_global_claude: false,
                load_global_agents: false,
                load_project_instructions: false,
                skills: false,
                mcp: false,
                import_claude_permissions: false,
                ..self
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

    /// A missing file changes nothing; a malformed one is an error. Only a `trusted`
    /// file may set the mode, add allow rules or turn the Claude Code import on.
    fn apply_file(&mut self, path: &Path, trusted: bool) -> Result<()> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Ok(());
        };
        let layer: Layer =
            toml::from_str(&text).with_context(|| format!("bad config {}", path.display()))?;
        let bad = |e| anyhow::anyhow!("bad config {}: {e}", path.display());
        let parse = |rules: &[String]| {
            rules
                .iter()
                .map(|r| Rule::parse(r).map(|r| r.with_source(path.display().to_string())))
                .collect::<Result<Vec<_>, _>>()
                .map_err(bad)
        };
        self.permissions
            .deny
            .extend(parse(&layer.permissions.deny)?);
        self.permissions.ask.extend(parse(&layer.permissions.ask)?);
        if trusted {
            self.permissions
                .allow
                .extend(parse(&layer.permissions.allow)?);
            if let Some(mode) = layer.permission_mode {
                self.permission_mode = mode;
            }
        }
        if let Some(import) = layer.import_claude_permissions
            && (trusted || !import)
        {
            self.import_claude_permissions = import;
        }
        if trusted && let Some(McpLayer::Table { servers, .. }) = &layer.mcp {
            self.mcp_servers.extend(servers.clone());
        }
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
        match layer.skills {
            Some(SkillsLayer::Enabled(enabled)) => self.skills = enabled,
            Some(SkillsLayer::Table { enabled, sources }) => {
                set(&mut self.skills, enabled);
                if let Some(sources) = sources {
                    self.skill_sources = sources;
                }
            }
            None => {}
        }
        match layer.mcp {
            Some(McpLayer::Enabled(enabled)) => self.mcp = enabled,
            Some(McpLayer::Table { enabled, .. }) => set(&mut self.mcp, enabled),
            None => {}
        }
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
                ..Config::default()
            }
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn permissions_add_up_and_the_project_can_only_tighten() {
        let dir = temp_dir();
        let (home, cwd) = (dir.join("home"), dir.join("cwd"));
        write(
            &home.join(".config/bhai/config.toml"),
            "permission_mode = \"auto\"\n[permissions]\nallow = [\"Bash(ls)\"]\ndeny = [\"Read(a)\"]\n",
        );
        write(
            &cwd.join(".bhai/config.toml"),
            "permission_mode = \"bypass\"\n[permissions]\nallow = [\"Bash\"]\ndeny = [\"Read(b)\"]\nask = [\"Edit\"]\n",
        );
        let config = Config::load(Some(&home), &cwd).unwrap();
        assert_eq!(config.permission_mode, Mode::Auto);
        let texts = |rules: &[Rule]| rules.iter().map(|r| r.text.clone()).collect::<Vec<_>>();
        assert_eq!(texts(&config.permissions.allow), ["Bash(ls)"]);
        assert_eq!(texts(&config.permissions.deny), ["Read(a)", "Read(b)"]);
        assert_eq!(texts(&config.permissions.ask), ["Edit"]);
        let project = cwd.join(".bhai/config.toml").display().to_string();
        assert_eq!(config.permissions.ask[0].source, project);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_project_can_only_turn_the_claude_import_off() {
        let dir = temp_dir();
        let (home, cwd) = (dir.join("home"), dir.join("cwd"));
        let global = home.join(".config/bhai/config.toml");
        let project = cwd.join(".bhai/config.toml");
        write(&global, "import_claude_permissions = false\n");
        write(&project, "import_claude_permissions = true\n");
        let config = Config::load(Some(&home), &cwd).unwrap();
        assert!(!config.import_claude_permissions);
        write(&global, "import_claude_permissions = true\n");
        write(&project, "import_claude_permissions = false\n");
        let config = Config::load(Some(&home), &cwd).unwrap();
        assert!(!config.import_claude_permissions);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bad_rules_and_modes_are_errors() {
        for text in [
            "[permissions]\ndeny = [\"Bash(ls\"]\n",
            "permission_mode = \"yolo\"\n",
        ] {
            let dir = temp_dir();
            write(&dir.join(".bhai/config.toml"), text);
            assert!(Config::load(None, &dir).is_err(), "{text}");
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn skill_sources_come_from_a_skills_table() {
        let dir = temp_dir();
        let (home, cwd) = (dir.join("home"), dir.join("cwd"));
        write(
            &home.join(".config/bhai/config.toml"),
            "[skills]\nsources = [\"project\", \"global_agents\"]\n",
        );
        let config = Config::load(Some(&home), &cwd).unwrap();
        assert!(config.skills);
        assert_eq!(
            config.skill_sources,
            [Source::Project, Source::GlobalAgents]
        );
        write(
            &cwd.join(".bhai/config.toml"),
            "[skills]\nenabled = false\n",
        );
        let config = Config::load(Some(&home), &cwd).unwrap();
        assert!(!config.skills);
        assert_eq!(config.skill_sources.len(), 2);
        write(
            &cwd.join(".bhai/config.toml"),
            "[skills]\nsources = [\"x\"]\n",
        );
        assert!(Config::load(Some(&home), &cwd).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mcp_servers_come_only_from_the_global_file() {
        let dir = temp_dir();
        let (home, cwd) = (dir.join("home"), dir.join("cwd"));
        write(
            &home.join(".config/bhai/config.toml"),
            "[mcp]\nenabled = true\n[mcp.servers.fs]\ncommand = \"fs-mcp\"\nargs = [\"/tmp\"]\n",
        );
        write(
            &cwd.join(".bhai/config.toml"),
            "[mcp.servers.evil]\ncommand = \"sh\"\n",
        );
        let config = Config::load(Some(&home), &cwd).unwrap();
        assert!(config.mcp);
        let names: Vec<_> = config.mcp_servers.keys().collect();
        assert_eq!(names, ["fs"]);
        assert_eq!(config.mcp_servers["fs"].args, ["/tmp"]);
        assert!(config.mcp_servers["fs"].env.is_empty());
        write(&cwd.join(".bhai/config.toml"), "mcp = false\n");
        assert!(!Config::load(Some(&home), &cwd).unwrap().mcp);
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
        let no_global = base.clone().with_flags(Flags {
            no_global: true,
            ..Flags::default()
        });
        assert!(!no_global.load_global_claude && !no_global.load_global_agents);
        assert!(no_global.load_project_instructions && no_global.mcp);

        let no_project = base.clone().with_flags(Flags {
            no_project: true,
            ..Flags::default()
        });
        assert!(!no_project.load_project_instructions && no_project.load_global_claude);

        let bare = base.with_flags(Flags {
            bare: true,
            mode: Some(Mode::Auto),
            ..Flags::default()
        });
        assert_eq!(bare.permission_mode, Mode::Auto);
        assert!(
            !bare.load_global_claude
                && !bare.load_global_agents
                && !bare.load_project_instructions
                && !bare.skills
                && !bare.mcp
                && !bare.import_claude_permissions
        );
    }
}
