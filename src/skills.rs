//! Skills: `<name>/SKILL.md` directories whose name and description go in the system
//! prompt, with the body loaded on demand through the `skill` tool.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::Source;
use crate::frontmatter;
use crate::instructions::{self, Roots};

/// Skill roots under the home directory and the project root, lowest precedence first,
/// with the source that turns each on under the home directory.
const SKILL_DIRS: [(&str, Source); 2] = [
    (".claude/skills", Source::GlobalClaude),
    (".agents/skills", Source::GlobalAgents),
];

/// Descriptions longer than this are cut in the listing.
const MAX_DESCRIPTION: usize = 250;

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The skill directory, which holds `SKILL.md` and any files it refers to.
    pub dir: PathBuf,
    /// The skills root it was found in, as shown to the user.
    pub source: String,
}

impl Skill {
    pub fn file(&self) -> PathBuf {
        self.dir.join("SKILL.md")
    }

    /// Its line in the system prompt listing.
    pub fn entry(&self) -> String {
        let description = self
            .description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if description.len() <= MAX_DESCRIPTION {
            return format!("- {}: {description}", self.name);
        }
        let mut cut = MAX_DESCRIPTION;
        while !description.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("- {}: {}...", self.name, description[..cut].trim_end())
    }
}

/// Every skill found in the enabled sources, sorted by name; project skills replace
/// global ones of the same name.
pub fn discover(roots: &Roots, sources: &[Source]) -> Vec<Skill> {
    let mut bases = Vec::new();
    if let Some(home) = &roots.home {
        bases.push((home.clone(), None));
    }
    if sources.contains(&Source::Project) {
        let project = instructions::project_root(&roots.cwd).to_path_buf();
        bases.push((project, Some(Source::Project)));
    }

    let mut found = BTreeMap::new();
    for (base, project) in bases {
        for (dir, global) in SKILL_DIRS {
            if !sources.contains(&project.unwrap_or(global)) {
                continue;
            }
            let root = base.join(dir);
            for skill in scan(&root, &instructions::label(&root, roots)) {
                found.insert(skill.name.clone(), skill);
            }
        }
    }
    found.into_values().collect()
}

/// The skills directly under one root, in directory name order.
fn scan(root: &Path, source: &str) -> Vec<Skill> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    dirs.sort();
    dirs.into_iter()
        .filter_map(|dir| {
            let text = std::fs::read_to_string(dir.join("SKILL.md")).ok()?;
            let (front, _) = parse(&text)?;
            Some(Skill {
                name: front.name,
                description: front.description,
                dir,
                source: source.to_string(),
            })
        })
        .collect()
}

#[derive(Debug, PartialEq)]
pub struct Frontmatter {
    pub name: String,
    pub description: String,
}

/// Split `SKILL.md` into its frontmatter and body. `None` without a frontmatter `name`.
pub fn parse(text: &str) -> Option<(Frontmatter, &str)> {
    let (front, body) = frontmatter::split(text)?;
    let name = frontmatter::value(&front, "name").filter(|n| !n.is_empty())?;
    let description = frontmatter::value(&front, "description").unwrap_or_default();
    Some((Frontmatter { name, description }, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: PathBuf,
        roots: Roots,
    }

    impl Fixture {
        /// home/ and a repo at home/repo with cwd at home/repo/sub.
        fn new() -> Self {
            let dir = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("bhai-skills-{}", uuid::Uuid::new_v4()));
            let home = dir.join("home");
            let cwd = home.join("repo/sub");
            std::fs::create_dir_all(home.join("repo/.git")).unwrap();
            std::fs::create_dir_all(&cwd).unwrap();
            let roots = Roots {
                home: Some(home),
                codex_home: None,
                cwd,
            };
            Self { dir, roots }
        }

        fn skill(&self, rel: &str, name: &str, description: &str) -> PathBuf {
            let path = self.dir.join(rel).join("SKILL.md");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let text = format!("---\nname: {name}\ndescription: {description}\n---\nbody\n");
            std::fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn front(text: &str) -> Option<(String, String)> {
        parse(text).map(|(f, _)| (f.name, f.description))
    }

    #[test]
    fn plain_and_quoted_values() {
        let text =
            "---\nname: pdf\ndescription: Read PDFs: text and tables.\n---\n\n# PDF\nuse it\n";
        let (f, body) = parse(text).unwrap();
        assert_eq!(f.name, "pdf");
        assert_eq!(f.description, "Read PDFs: text and tables.");
        assert_eq!(body, "# PDF\nuse it\n");

        let quoted = "---\nname: \"yt\"\ndescription: \"Say \\\"hi\\\": now\"\nother: 1\n---\nx";
        assert_eq!(front(quoted), Some(("yt".into(), "Say \"hi\": now".into())));
        let single = "---\r\nname: 'a'\r\ndescription: 'it''s'\r\n---\r\nx";
        assert_eq!(front(single), Some(("a".into(), "it's".into())));
    }

    #[test]
    fn block_scalars_and_continuations_are_joined() {
        let folded =
            "---\nname: a\ndescription: >\n  first line\n  second line\n\nmetadata:\n  x: 1\n---\n";
        assert_eq!(
            front(folded),
            Some(("a".into(), "first line second line".into()))
        );
        let literal = "---\ndescription: |-\n  one\n  two\nname: b\n---\n";
        assert_eq!(front(literal), Some(("b".into(), "one\ntwo".into())));
        let plain = "---\nname: c\ndescription: starts here\n  and goes on\n---\n";
        assert_eq!(
            front(plain),
            Some(("c".into(), "starts here and goes on".into()))
        );
        let empty_block = "---\nname: d\ndescription: |\n---\n";
        assert_eq!(front(empty_block), Some(("d".into(), String::new())));
    }

    #[test]
    fn malformed_frontmatter_is_none() {
        assert_eq!(front("no frontmatter"), None);
        assert_eq!(front("---\nname: a\nno end"), None);
        assert_eq!(front("---\ndescription: d\n---\n"), None);
        assert_eq!(front("---\nname:\n---\n"), None);
        assert_eq!(front("---\nnamespace: x\n---\n"), None);
        assert_eq!(
            front("---\nname: a\n---\n"),
            Some(("a".into(), String::new()))
        );
    }

    #[test]
    fn project_skills_override_global_ones() {
        let f = Fixture::new();
        f.skill("home/.claude/skills/one", "one", "global one");
        f.skill("home/.agents/skills/two", "two", "global two");
        f.skill("home/repo/.claude/skills/one-dir", "one", "project one");
        f.skill("home/repo/.agents/skills/three", "three", "project three");
        f.skill("home/.claude/skills/nameless", "", "skipped");
        std::fs::create_dir_all(f.dir.join("home/.claude/skills/no-file")).unwrap();

        let skills = discover(&f.roots, &Source::ALL);
        let found: Vec<_> = skills
            .iter()
            .map(|s| (s.name.as_str(), s.description.as_str(), s.source.as_str()))
            .collect();
        assert_eq!(
            found,
            [
                ("one", "project one", "~/repo/.claude/skills"),
                ("three", "project three", "~/repo/.agents/skills"),
                ("two", "global two", "~/.agents/skills"),
            ]
        );
        assert_eq!(
            skills[0].dir,
            f.dir.join("home/repo/.claude/skills/one-dir")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_skill_dirs_are_followed() {
        let f = Fixture::new();
        let file = f.skill("elsewhere/linked", "linked", "via a symlink");
        std::fs::create_dir_all(f.dir.join("home/.claude/skills")).unwrap();
        std::os::unix::fs::symlink(
            file.parent().unwrap(),
            f.dir.join("home/.claude/skills/linked"),
        )
        .unwrap();
        let skills = discover(&f.roots, &Source::ALL);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "linked");
        assert_eq!(
            skills[0].file(),
            f.dir.join("home/.claude/skills/linked/SKILL.md")
        );
    }

    #[test]
    fn disabled_sources_are_not_scanned() {
        let f = Fixture::new();
        f.skill("home/.claude/skills/a", "a", "");
        f.skill("home/.agents/skills/b", "b", "");
        f.skill("home/repo/.agents/skills/c", "c", "");
        let names = |sources: &[Source]| -> Vec<String> {
            discover(&f.roots, sources)
                .into_iter()
                .map(|s| s.name)
                .collect()
        };
        assert_eq!(names(&[Source::GlobalAgents]), ["b"]);
        assert_eq!(names(&[Source::GlobalClaude, Source::Project]), ["a", "c"]);
        assert!(names(&[]).is_empty());
    }

    #[test]
    fn missing_roots_find_nothing() {
        let roots = Roots {
            home: None,
            codex_home: None,
            cwd: std::env::temp_dir().join(format!("bhai-none-{}", uuid::Uuid::new_v4())),
        };
        assert!(discover(&roots, &Source::ALL).is_empty());
    }

    #[test]
    fn long_descriptions_are_cut_in_the_entry() {
        let skill = |description: &str| Skill {
            name: "s".to_string(),
            description: description.to_string(),
            dir: PathBuf::from("/s"),
            source: "~/.claude/skills".to_string(),
        };
        assert_eq!(skill("one\n  two").entry(), "- s: one two");
        let entry = skill(&"é".repeat(300)).entry();
        assert!(entry.ends_with("é..."), "{entry}");
        assert!(entry.len() <= "- s: ".len() + MAX_DESCRIPTION + 3);
    }
}
