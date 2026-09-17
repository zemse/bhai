//! Load a skill's full instructions. Needs no approval.

use serde_json::{Value, json};

use super::{BoxFuture, Tool, string_arg, truncate};
use crate::skills;

pub const NAME: &str = "skill";

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
            "description": "Load the full instructions of a skill from the system prompt's \
        skills list. Runs without approval.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The skill's name, as listed."
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        self.find(args).map(|skill| format!("skill {}", skill.name))
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            match self.find(args).and_then(load) {
                Ok(output) => (truncate(&output), true),
                Err(e) => (e, false),
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

    #[tokio::test]
    async fn returns_the_body_without_frontmatter() {
        let (tool, dir) = tool();
        let args = json!({"name": "pdf"});
        assert_eq!(tool.describe(&args).unwrap(), "skill pdf");
        let (out, ok) = tool.execute(&args).await;
        assert!(ok, "{out}");
        assert!(out.contains(&dir.display().to_string()), "{out}");
        assert!(out.ends_with("\n\n# PDF\nSee reference.md."), "{out}");
        assert!(!out.contains("description:"), "{out}");
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
