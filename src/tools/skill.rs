//! Load a skill's full instructions. Needs no approval.

use serde_json::{Value, json};

use super::{BoxFuture, Tool, string_arg};
use crate::skills;

pub const NAME: &str = "register_skills";

pub struct Skill {
    pub skills: Vec<skills::Skill>,
}

impl Tool for Skill {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": NAME,
            "description": "Load a listed skill's full instructions by name. When starting work \
        in another repository, set `from` to its absolute directory and omit `name` to discover \
        local skills in a tool result. Pass the returned `from` and a `name` to load one. \
        Discovery does not change the system prompt. Runs without approval.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The skill's name, as listed. Omit to discover repo skills."
                    },
                    "from": {
                        "type": "string",
                        "description": "Absolute repository directory (or a directory inside it). Omit to load a startup skill."
                    }
                },
                "required": [],
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        if let Some(repo) = repo_arg(args)? {
            return Ok(match name_arg(args)? {
                Some(name) => format!("register_skills {name} from {repo}"),
                None => format!("discover skills in {repo}"),
            });
        }
        self.find(args)
            .map(|skill| format!("register_skills {}", skill.name))
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let args = args.clone();
            let skills = self.skills.clone();
            let result = tokio::task::spawn_blocking(move || {
                let tool = Skill { skills };
                match repo_arg(&args)? {
                    Some(repo) => repo_skill(repo, name_arg(&args)?),
                    // Not truncated: instructions with a hole in the middle are worse than long ones.
                    None => tool.find(&args).and_then(load),
                }
            })
            .await;
            match result {
                Ok(Ok(output)) => (output, true),
                Ok(Err(e)) => (e, false),
                Err(e) => (format!("Could not load skills: {e}"), false),
            }
        })
    }
}

impl Skill {
    fn find(&self, args: &Value) -> Result<&skills::Skill, String> {
        let name = string_arg(args, "name")
            .ok_or_else(|| "missing required string field `name`.".to_string())?;
        self.skills.iter().find(|s| s.name == name).ok_or_else(|| {
            let names: Vec<_> = self.skills.iter().map(|s| s.name.as_str()).collect();
            format!(
                "No skill named `{name}`. Available skills: {}.",
                names.join(", ")
            )
        })
    }
}

fn name_arg(args: &Value) -> Result<Option<&str>, String> {
    match args.get("name") {
        None => Ok(None),
        Some(Value::String(name)) if !name.is_empty() => Ok(Some(name)),
        _ => Err("`name` must be a nonempty string.".to_string()),
    }
}

fn repo_arg(args: &Value) -> Result<Option<&str>, String> {
    match args.get("from") {
        None => Ok(None),
        Some(Value::String(repo)) if std::path::Path::new(repo).is_absolute() => Ok(Some(repo)),
        _ => Err("`from` must be an absolute directory path.".to_string()),
    }
}

fn repo_skill(repo: &str, name: Option<&str>) -> Result<String, String> {
    let dir = std::path::Path::new(repo)
        .canonicalize()
        .map_err(|e| format!("Could not open repository {repo}: {e}"))?;
    if !dir.is_dir() {
        return Err(format!("Not a directory: {}", dir.display()));
    }
    let root = crate::instructions::project_root(&dir).to_path_buf();
    let roots = crate::instructions::Roots {
        home: None,
        codex_home: None,
        cwd: root.clone(),
    };
    let discovered = skills::discover(&roots, &[crate::config::Source::Project]);
    if let Some(name) = name {
        return discovered
            .skills
            .iter()
            .find(|skill| skill.name == name)
            .ok_or_else(|| format!("No skill named `{name}` in {}.", root.display()))
            .and_then(load);
    }
    let entries: Vec<_> = discovered
        .skills
        .iter()
        .map(|skill| json!({"name": skill.name, "listing": skill.entry(), "source": skill.source}))
        .collect();
    Ok(json!({
        "from": root,
        "skills": entries,
        "shadowed": discovered.shadowed,
        "usage": "Call register_skills with the returned from path and a listed name to load its instructions. Repo skills apply only when working in this repo; startup skills remain unchanged."
    })
    .to_string())
}

/// The body of `SKILL.md`, read fresh, headed by the directory its relative paths use.
fn load(skill: &skills::Skill) -> Result<String, String> {
    let path = skill.file();
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("Could not read {}: {e}", path.display()))?;
    let body = skills::parse(&text).map_or(text.as_str(), |(_, body)| body);
    Ok(format!(
        "Skill `{}` from {}. Paths in it are relative to that directory; open referenced \
files with `read`.\n\n{}",
        skill.name,
        skill.dir.display(),
        body.trim_end()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> (Skill, std::path::PathBuf) {
        let dir = super::super::temp_dir().join("pdf");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: pdf\ndescription: Read PDFs.\n---\n\n# PDF\nSee reference.md.\n",
        )
        .unwrap();
        let skill = skills::Skill {
            name: "pdf".to_string(),
            description: "Read PDFs.".to_string(),
            dir: dir.clone(),
            source: "~/.claude/skills".to_string(),
        };
        (
            Skill {
                skills: vec![skill],
            },
            dir,
        )
    }

    #[test]
    fn schema_uses_register_skills_and_from() {
        let (tool, _) = tool();
        let schema = tool.schema();
        assert_eq!(schema["name"], "register_skills");
        let properties = &schema["parameters"]["properties"];
        assert_eq!(properties["from"]["type"], "string");
        assert_eq!(properties["name"]["type"], "string");
        assert!(properties.get("repo").is_none());
    }

    #[tokio::test]
    async fn returns_the_body_without_frontmatter() {
        let (tool, dir) = tool();
        let args = json!({"name": "pdf"});
        assert_eq!(tool.describe(&args).unwrap(), "register_skills pdf");
        let (out, ok) = tool.execute(&args).await;
        assert!(ok, "{out}");
        assert!(out.contains(&dir.display().to_string()), "{out}");
        assert!(out.ends_with("\n\n# PDF\nSee reference.md."), "{out}");
        assert!(!out.contains("description:"), "{out}");
    }

    #[tokio::test]
    async fn a_long_skill_arrives_whole() {
        let (tool, dir) = tool();
        let body = "line of instructions\n".repeat(2_000);
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: pdf\ndescription: Read PDFs.\n---\n\n{body}"),
        )
        .unwrap();
        let (out, ok) = tool.execute(&json!({"name": "pdf"})).await;
        assert!(ok, "{out}");
        assert!(out.len() > 40_000, "{}", out.len());
        assert!(!out.contains("bytes trimmed"), "{out}");
        assert_eq!(out.matches("line of instructions").count(), 2_000);
    }

    fn repo_skill_file(repo: &std::path::Path, source: &str, body: &str) {
        let dir = repo.join(source).join("pdf");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: pdf\ndescription: Local PDF skill.\n---\n{body}"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn discovery_preserves_prompt_and_schema_and_scopes_loading() {
        let (tool, dir) = tool();
        let workspace = dir.join("workspace");
        let a = workspace.join("a");
        let b = workspace.join("b");
        std::fs::create_dir_all(a.join(".git")).unwrap();
        std::fs::create_dir_all(a.join("sub")).unwrap();
        // A .git file is also a repo boundary (worktrees).
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(b.join(".git"), "gitdir: elsewhere").unwrap();
        repo_skill_file(&a, ".claude/skills", "claude body");
        repo_skill_file(&a, ".agents/skills", "repo a body");
        repo_skill_file(&b, ".claude/skills", "repo b body");
        let prompt = crate::prompt::system_prompt(&[], tool.skills.clone());
        let before = prompt.text.clone();
        let registry = super::super::Registry::for_prompt(&prompt);
        let schemas = registry.schemas();
        let tool = registry.get(NAME).unwrap();
        let (out, ok) = tool.execute(&json!({"from": a.join("sub")})).await;
        assert!(ok, "{out}");
        let discovered: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(discovered["from"], json!(a.canonicalize().unwrap()));
        assert_eq!(discovered["skills"].as_array().unwrap().len(), 1);
        assert_eq!(discovered["skills"][0]["name"], "pdf");
        assert!(
            discovered["skills"][0]["listing"]
                .as_str()
                .unwrap()
                .contains("Local PDF")
        );
        assert_eq!(discovered["shadowed"].as_array().unwrap().len(), 1);
        assert!(!out.contains("repo a body"));
        let (out, ok) = tool
            .execute(&json!({"from": discovered["from"], "name": "pdf"}))
            .await;
        assert!(ok && out.ends_with("repo a body"), "{out}");
        let (out, ok) = tool.execute(&json!({"from": b, "name": "pdf"})).await;
        assert!(ok && out.ends_with("repo b body"), "{out}");
        let (out, ok) = tool.execute(&json!({"name": "pdf"})).await;
        assert!(ok && out.contains("# PDF"), "{out}");
        assert_eq!(registry.schemas(), schemas);
        assert_eq!(prompt.text, before);
        assert_eq!(prompt.skills.len(), 1);
        // Lookup still works after the registry is recreated from the unchanged prompt.
        let fresh = super::super::Registry::for_prompt(&prompt);
        let (out, ok) = fresh
            .get(NAME)
            .unwrap()
            .execute(&json!({"from": a, "name": "pdf"}))
            .await;
        assert!(ok && out.ends_with("repo a body"), "{out}");
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[tokio::test]
    async fn workspace_discovery_is_not_recursive_and_needs_no_startup_skills() {
        let dir = super::super::temp_dir();
        let repo = dir.join("nested");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        repo_skill_file(&repo, ".agents/skills", "nested body");
        let registry = super::super::Registry::new(Vec::new());
        let tool = registry.get(NAME).unwrap();
        let (out, ok) = tool.execute(&json!({"from": dir})).await;
        assert!(ok, "{out}");
        let discovered: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(discovered["skills"], json!([]));
        let (out, ok) = tool.execute(&json!({"from": repo, "name": "pdf"})).await;
        assert!(ok && out.ends_with("nested body"), "{out}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn invalid_repos_and_repo_names_fail_without_global_fallback() {
        let (tool, dir) = tool();
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        for args in [
            json!({"from": "relative"}),
            json!({"from": 42}),
            json!({"from": dir.join("missing")}),
            json!({"from": dir.join("SKILL.md")}),
            json!({"from": empty, "name": "pdf"}),
            json!({"from": empty, "name": false}),
        ] {
            let (out, ok) = tool.execute(&args).await;
            assert!(!ok, "{args}: {out}");
        }
    }

    #[tokio::test]
    async fn unknown_and_missing_names_fail() {
        let (tool, _) = tool();
        let err = tool.describe(&json!({"name": "nope"})).unwrap_err();
        assert!(err.contains("Available skills: pdf."), "{err}");
        assert!(tool.describe(&json!({})).is_err());
        let (out, ok) = tool.execute(&json!({"name": "nope"})).await;
        assert!(!ok && out.contains("No skill"), "{out}");
    }
}
