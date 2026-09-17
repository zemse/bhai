//! The system prompt. Kept short on purpose: the model is already trained to be a
//! coding agent, so this only states what this particular harness can and cannot do.
//! Instruction files go after the static text, so the cacheable prefix stays first.

use std::fmt::Write as _;

use crate::instructions::File;

/// The system prompt and the instruction files appended to it.
#[derive(Debug, Clone)]
pub struct SystemPrompt {
    pub text: String,
    pub sources: Vec<Source>,
}

/// One appended instruction file.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub label: String,
    /// Bytes its section adds to the prompt, heading included.
    pub bytes: usize,
}

impl SystemPrompt {
    /// The startup line, like `loaded: ~/.claude/CLAUDE.md (3.1k), ./CLAUDE.md (0.4k)`.
    pub fn loaded(&self) -> Option<String> {
        if self.sources.is_empty() {
            return None;
        }
        let list: Vec<String> = self
            .sources
            .iter()
            .map(|s| format!("{} ({:.1}k)", s.label, s.bytes as f64 / 1000.0))
            .collect();
        Some(format!("loaded: {}", list.join(", ")))
    }
}

pub fn system_prompt(files: &[File]) -> SystemPrompt {
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
    SystemPrompt { text, sources }
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
        let bare = system_prompt(&[]);
        assert_eq!(bare.text, base());
        assert!(bare.sources.is_empty());
        assert_eq!(bare.loaded(), None);

        let files = [
            file("~/.claude/CLAUDE.md", "be terse\n"),
            file("./CLAUDE.md", &"x".repeat(420)),
        ];
        let prompt = system_prompt(&files);
        assert!(prompt.text.starts_with(&bare.text));
        assert!(
            prompt
                .text
                .contains("# Instructions from ~/.claude/CLAUDE.md\n\nbe terse\n\n#")
        );
        assert!(prompt.text.ends_with(&"x".repeat(420)));
        let added: usize = prompt.sources.iter().map(|s| s.bytes).sum();
        assert_eq!(bare.text.len() + added, prompt.text.len());
        assert_eq!(
            prompt.loaded().unwrap(),
            "loaded: ~/.claude/CLAUDE.md (0.1k), ./CLAUDE.md (0.5k)"
        );
    }
}
