//! The user's own editor, for writing the draft at length (`ctrl+g`).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// The editor to run: `$VISUAL`, then `$EDITOR`, then `vi`, as git picks one.
pub fn command() -> String {
    pick(std::env::var("VISUAL").ok(), std::env::var("EDITOR").ok())
}

fn pick(visual: Option<String>, editor: Option<String>) -> String {
    [visual, editor]
        .into_iter()
        .flatten()
        .find(|command| !command.trim().is_empty())
        .unwrap_or_else(|| "vi".to_string())
}

/// Open `draft` in `command` and return what the editor left behind. The command goes
/// through `sh`, so one with flags or quotes (`code --wait`) runs as it would in a shell.
/// The caller owns the terminal: the editor inherits it as it is.
pub fn edit(command: &str, draft: &str) -> Result<String> {
    let path = std::env::temp_dir().join(format!("bhai-prompt-{}.md", uuid::Uuid::new_v4()));
    let result = run(command, draft, &path);
    let _ = std::fs::remove_file(&path);
    result
}

fn run(command: &str, draft: &str, path: &Path) -> Result<String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    // A draft can hold anything the user was about to send, and /tmp is shared on Linux.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(path)
        .and_then(|mut file| file.write_all(draft.as_bytes()))
        .with_context(|| format!("writing {}", path.display()))?;
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{command} \"$@\""))
        .arg(command)
        .arg(path)
        .status()
        .with_context(|| format!("running {command}"))?;
    if !status.success() {
        bail!("{command} exited with {status}; the draft is unchanged");
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    // Editors end the file with a newline the draft did not have.
    Ok(text.trim_end_matches(['\n', '\r']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_wins_then_editor_then_vi() {
        let some = |s: &str| Some(s.to_string());
        assert_eq!(pick(some("code --wait"), some("nano")), "code --wait");
        assert_eq!(pick(None, some("nano")), "nano");
        assert_eq!(pick(some(" "), some("nano")), "nano");
        assert_eq!(pick(None, None), "vi");
    }

    #[test]
    fn the_draft_comes_back_as_the_editor_left_it() {
        // An "editor" that appends a line, the way a user would, and a trailing newline.
        let text = edit("f() { printf ' and more\\n\\n' >> \"$1\"; }; f", "a draft").unwrap();
        assert_eq!(text, "a draft and more");
    }

    #[test]
    fn a_failing_editor_is_an_error() {
        let err = edit("false", "kept").unwrap_err();
        assert!(err.to_string().contains("unchanged"), "{err}");
    }

    #[test]
    fn the_file_is_gone_afterwards() {
        let text = edit("f() { printf '%s' \"$1\" > \"$1\"; }; f", "").unwrap();
        assert!(text.contains("bhai-prompt-"), "{text}");
        assert!(!std::path::Path::new(&text).exists());
    }
}
