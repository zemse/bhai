//! The environment bash and stdio MCP children get: what bhai inherited, less the names
//! that look like credentials. `env` is on the read-only list, so anything left in
//! would land in history, the session file and the backend. A `-l` profile can export a
//! secret again; this only covers what bhai itself inherited.

use std::ffi::OsString;
use std::sync::OnceLock;

/// Name parts (split on `_`) that mark a variable as a credential.
const SECRET_PARTS: [&str; 10] = [
    "KEY",
    "TOKEN",
    "SECRET",
    "PASS",
    "PASSWORD",
    "PASSWD",
    "AUTH",
    "COOKIE",
    "CREDENTIAL",
    "CREDENTIALS",
];

/// `[bash] pass_env`: names let through whatever they look like.
static PASS: OnceLock<Vec<String>> = OnceLock::new();

/// Named once for the process, like the code theme.
pub fn set_pass(names: &[String]) {
    let _ = PASS.set(names.to_vec());
}

fn looks_secret(name: &str) -> bool {
    name.split('_')
        .any(|part| SECRET_PARTS.iter().any(|p| p.eq_ignore_ascii_case(part)))
}

fn withheld_from(vars: impl Iterator<Item = OsString>, pass: &[String]) -> Vec<OsString> {
    vars.filter(|name| {
        name.to_str()
            .is_some_and(|n| looks_secret(n) && !pass.iter().any(|p| p == n))
    })
    .collect()
}

/// Names in the current environment to drop from a child's.
pub fn withheld() -> Vec<OsString> {
    let pass = PASS.get().map(Vec::as_slice).unwrap_or_default();
    withheld_from(std::env::vars_os().map(|(name, _)| name), pass)
}

/// Remove the withheld names from `command`. Call it before the caller sets its own `env`.
pub fn scrub(command: &mut tokio::process::Command) {
    for name in withheld() {
        command.env_remove(name);
    }
}

#[cfg(target_os = "macos")]
const UTF8: &str = "en_US.UTF-8";
#[cfg(not(target_os = "macos"))]
const UTF8: &str = "C.UTF-8";

/// Fixed for a bash child so nothing waits on a terminal that is not there: no pager, no
/// git credential prompt, no colour codes in what the model reads.
const FIXED: [(&str, &str); 7] = [
    ("TERM", "dumb"),
    ("NO_COLOR", "1"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("LANG", UTF8),
    ("LC_ALL", UTF8),
];

/// Set the fixed non-interactive variables on `command`, over whatever bhai inherited.
pub fn non_interactive(command: &mut tokio::process::Command) {
    command.envs(FIXED);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn credential_names_are_withheld() {
        for name in [
            "GITHUB_TOKEN",
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "SSH_AUTH_SOCK",
            "DB_PASSWORD",
            "MY_COOKIE",
            "GOOGLE_CREDENTIALS",
            "token",
        ] {
            assert!(looks_secret(name), "{name}");
        }
    }

    #[test]
    fn ordinary_names_stay() {
        for name in ["PATH", "HOME", "KEYBOARD", "AUTHOR", "PASSTHROUGH", "TERM"] {
            assert!(!looks_secret(name), "{name}");
        }
    }

    #[test]
    fn pass_env_lets_a_name_through() {
        let vars = || names(&["PATH", "GITHUB_TOKEN", "NPM_TOKEN"]).into_iter();
        assert_eq!(
            withheld_from(vars(), &[]),
            names(&["GITHUB_TOKEN", "NPM_TOKEN"])
        );
        assert_eq!(
            withheld_from(vars(), &["NPM_TOKEN".to_string()]),
            names(&["GITHUB_TOKEN"])
        );
    }
}
