//! The system prompt. Kept short on purpose: the model is already trained to be a
//! coding agent, so this only states what this particular harness can and cannot do.
//! Instruction files and then the skills listing go after the static text, so the
//! cacheable prefix stays first.

use std::fmt::Write as _;
use std::sync::Arc;

use crate::identity::Identity;
use crate::instructions::File;
use crate::mcp::Hub;
use crate::skills::Skill;

/// The system prompt and the instruction files and skills appended to it.
#[derive(Debug, Clone, Default)]
pub struct SystemPrompt {
    pub text: String,
    pub sources: Vec<Source>,
    /// The skills listed, which the `skill` tool loads.
    pub skills: Vec<Skill>,
    /// Bytes the skills listing adds to the prompt.
    pub skills_bytes: usize,
    /// The MCP servers, which `mcp_search` and `mcp_call` reach.
    pub mcp: Option<Arc<Hub>>,
    /// Bytes the MCP server lines add to the prompt.
    pub mcp_bytes: usize,
    /// Imports refused while loading the instruction files.
    pub skipped: Vec<String>,
    /// The identity the prompt was built for.
    pub identity: Identity,
}

/// One appended instruction file.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub label: String,
    /// Bytes its section adds to the prompt, heading included.
    pub bytes: usize,
}

impl SystemPrompt {
    /// The startup lines: `loaded: ~/.claude/CLAUDE.md (~0.8k tok), skills (~52 tok)`,
    /// then one per skipped import.
    pub fn notices(&self) -> Vec<String> {
        let mut list: Vec<String> = self
            .sources
            .iter()
            .map(|s| format!("{} ({})", s.label, tokens(s.bytes)))
            .collect();
        if self.skills_bytes > 0 {
            list.push(format!("skills ({})", tokens(self.skills_bytes)));
        }
        if self.mcp_bytes > 0 {
            list.push(format!("mcp ({})", tokens(self.mcp_bytes)));
        }
        let mut lines = Vec::new();
        if !list.is_empty() {
            lines.push(format!("loaded: {}", list.join(", ")));
        }
        lines.extend(self.skipped.iter().cloned());
        lines.extend(self.mcp.iter().flat_map(|hub| hub.notices()));
        lines
    }

    /// Append the MCP server lines; they go last, after the skills listing.
    pub fn with_mcp(mut self, hub: Option<Arc<Hub>>) -> Self {
        if let Some(hub) = &hub {
            let section = hub.prompt_section();
            self.mcp_bytes = section.len();
            self.text.push_str(&section);
        }
        self.mcp = hub;
        self
    }
}

/// Approximate tokens (bytes/4), like `~52 tok` or `~2.1k tok`.
fn tokens(bytes: usize) -> String {
    let tokens = bytes.div_ceil(4);
    if tokens < 1000 {
        format!("~{tokens} tok")
    } else {
        format!("~{:.1}k tok", tokens as f64 / 1000.0)
    }
}

pub fn system_prompt(files: &[File], skills: Vec<Skill>) -> SystemPrompt {
    let mut text = base();
    let mut sources = Vec::new();
    for file in files {
        let start = text.len();
        let _ = write!(
            text,
            "\n\n# Instructions from {}\n\n{}",
            file.label,
            file.content.trim_end()
        );
        sources.push(Source {
            label: file.label.clone(),
            bytes: text.len() - start,
        });
    }
    let start = text.len();
    if !skills.is_empty() {
        text.push_str(
            "\n\n# Skills\n\nSkills are task-specific instructions. Before using one, call the \
`skill` tool with its name to load its full instructions.\n",
        );
        for skill in &skills {
            let _ = write!(text, "\n{}", skill.entry());
        }
    }
    SystemPrompt {
        skills_bytes: text.len() - start,
        text,
        sources,
        skills,
        mcp: None,
        mcp_bytes: 0,
        skipped: Vec::new(),
        identity: Identity::default(),
    }
}

fn base() -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let os = std::env::consts::OS;

    format!(
        "You are bhai, a coding agent running in a terminal on the user's machine.

Environment:
- Working directory: {cwd}
- Operating system: {os}
- Shell: bash

Your tools are `bash`, `read`, `write` and `edit`. Use `read` to view files, `edit` for \
targeted changes and `write` for new files; use `bash` for everything else (searching with \
`rg`, building, testing). Do not describe an edit you have not actually applied.

Rules:
- Every command, write and edit is shown to the user, who accepts or rejects it before it \
runs. A rejected call did not execute; take the rejection as direction and change course \
rather than retrying the same thing.
- Use absolute paths. Relative paths are a common source of mistakes.
- Prefer small, checkable commands over one long chain, so a rejection is cheap.
- Never run anything interactive (editors, pagers, REPLs, `git rebase -i`); it will hang \
until the 120-second timeout kills it.
- Read before you write. Look at a file before editing it, and verify after editing.
- Do not commit to git unless the user asks for it.

Answer in plain text for a terminal: short, specific, no markdown headers or bullet-point \
padding. Say what you did and what you found. When a task is done, stop calling tools and \
report the result."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn file(label: &str, content: &str) -> File {
        File {
            path: PathBuf::from(label),
            label: label.to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn files_are_appended_after_the_static_text() {
        let bare = system_prompt(&[], Vec::new());
        assert_eq!(bare.text, base());
        assert!(bare.sources.is_empty());
        assert!(bare.notices().is_empty());

        let files = [
            file("~/.claude/CLAUDE.md", "be terse\n"),
            file("./CLAUDE.md", &"x".repeat(4200)),
        ];
        let prompt = system_prompt(&files, Vec::new());
        assert!(prompt.text.starts_with(&bare.text));
        assert!(
            prompt
                .text
                .contains("# Instructions from ~/.claude/CLAUDE.md\n\nbe terse\n\n#")
        );
        assert!(prompt.text.ends_with(&"x".repeat(4200)));
        let added: usize = prompt.sources.iter().map(|s| s.bytes).sum();
        assert_eq!(bare.text.len() + added, prompt.text.len());
        assert_eq!(
            prompt.notices(),
            ["loaded: ~/.claude/CLAUDE.md (~13 tok), ./CLAUDE.md (~1.1k tok)"]
        );
        assert_eq!(prompt.skills_bytes, 0);
    }

    #[test]
    fn skills_are_listed_after_the_files() {
        let skill = Skill {
            name: "pdf".to_string(),
            description: "Read PDFs.".to_string(),
            dir: PathBuf::from("/s/pdf"),
            source: "~/.claude/skills".to_string(),
        };
        let files = [file("./CLAUDE.md", "be terse")];
        let without = system_prompt(&files, Vec::new());
        let prompt = system_prompt(&files, vec![skill]);
        assert!(prompt.text.starts_with(&without.text));
        assert!(prompt.text.ends_with(
            "`skill` tool with its name to load its full instructions.\n\n- pdf: Read PDFs."
        ));
        assert_eq!(without.text.len() + prompt.skills_bytes, prompt.text.len());
        assert_eq!(prompt.skills.len(), 1);
        assert_eq!(
            prompt.notices()[0],
            format!(
                "loaded: ./CLAUDE.md ({}), skills (~{} tok)",
                tokens(without.sources[0].bytes),
                prompt.skills_bytes.div_ceil(4)
            )
        );
    }

    #[test]
    fn skipped_imports_follow_the_loaded_line() {
        let mut prompt = system_prompt(&[], Vec::new());
        prompt.skipped = vec!["skipped import ~/.ssh/id_rsa (outside project)".to_string()];
        assert_eq!(
            prompt.notices(),
            ["skipped import ~/.ssh/id_rsa (outside project)"]
        );
    }
}
