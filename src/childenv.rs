//! The environment bash and stdio MCP children get: what bhai inherited, less the names
//! that look like credentials. `env` is on the read-only list, so anything left in
//! would land in history, the session file and the backend. A `-l` profile can export a
//! secret again; this only covers what bhai itself inherited. `[bash] env` and `path`
//! add to it, and may not put back what it takes out.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const TERMINAL_SOCKET_ENV: &str = "BHAI_TERMINAL_SOCKET";

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

/// Named once for the process, like the code theme. What is then withheld is also
/// registered for redaction, so the two lists cannot drift apart.
pub fn set_pass(names: &[String]) {
    let _ = PASS.set(names.to_vec());
    crate::redact::register_withheld_env();
}

fn looks_secret(name: &str) -> bool {
    name.split('_')
        .any(|part| SECRET_PARTS.iter().any(|p| p.eq_ignore_ascii_case(part)))
}

fn withheld_from(vars: impl Iterator<Item = OsString>, pass: &[String]) -> Vec<OsString> {
    vars.filter(|name| {
        name.to_str().is_some_and(|n| {
            n == TERMINAL_SOCKET_ENV || (looks_secret(n) && !pass.iter().any(|p| p == n))
        })
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

/// `[bash] env` and `path` from one config file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Extra {
    pub env: BTreeMap<String, String>,
    /// Directories put in front of `PATH`, in order; each absolute or under `~`.
    pub path: Vec<String>,
}

/// `[bash] env` and `path`: the global file's, and the project file's, which applies only
/// while the project is trusted.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    pub user: Extra,
    pub project: Extra,
}

impl Settings {
    /// What applies: the project's over the user's when `trusted`, the user's alone if not.
    fn merged(&self, trusted: bool) -> Extra {
        let mut extra = self.user.clone();
        if trusted {
            extra.env.extend(self.project.env.clone());
            extra.path.splice(0..0, self.project.path.iter().cloned());
        }
        extra
    }
}

/// Whether the project is trusted now.
type Trusted = Box<dyn Fn() -> bool + Send + Sync>;

struct Configured {
    settings: Settings,
    home: Option<PathBuf>,
    trusted: Trusted,
}

static CONFIGURED: OnceLock<Configured> = OnceLock::new();

/// Named once for the process, like `pass_env`. Trust is read again for every command, so
/// `/trust` and `/untrust` take effect on the next one.
pub fn set_configured(
    settings: Settings,
    home: Option<&Path>,
    trusted: impl Fn() -> bool + Send + Sync + 'static,
) {
    let _ = CONFIGURED.set(Configured {
        settings,
        home: home.map(Path::to_path_buf),
        trusted: Box::new(trusted),
    });
}

/// Why `name` may not be set by `[bash] env`, if it may not.
pub fn refused(name: &str, pass: &[String]) -> Option<String> {
    let shaped = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !shaped {
        return Some(format!("{name:?} is not a variable name"));
    }
    if name == "PATH" {
        return Some("PATH is set with `path`, which prepends to it".to_string());
    }
    if FIXED.iter().any(|(fixed, _)| *fixed == name) || name.starts_with("BHAI_") {
        return Some(format!("{name} is fixed by bhai for every command"));
    }
    if !withheld_from([OsString::from(name)].into_iter(), pass).is_empty() {
        return Some(format!(
            "{name} looks like a credential, which bash never sees unless `pass_env` names it"
        ));
    }
    None
}

/// Why `dir` may not go on `PATH`, if it may not.
pub fn refused_dir(dir: &str) -> Option<String> {
    if !(dir.starts_with('/') || dir == "~" || dir.starts_with("~/")) {
        return Some(format!("{dir:?} is neither absolute nor under ~"));
    }
    if dir.contains([':', '\0']) {
        return Some(format!("{dir:?} holds a ':' or a NUL"));
    }
    None
}

/// The configured environment for one command, with trust as it is now.
pub fn configured() -> Extra {
    let Some(configured) = CONFIGURED.get() else {
        return Extra::default();
    };
    let project = &configured.settings.project;
    // The store is read only when there is something for it to let in.
    let trusted = (!project.env.is_empty() || !project.path.is_empty()) && (configured.trusted)();
    let mut extra = configured.settings.merged(trusted);
    extra.path = extra
        .path
        .iter()
        .map(|dir| expand(dir, configured.home.as_deref()))
        .collect();
    extra
}

fn expand(dir: &str, home: Option<&Path>) -> String {
    match (dir.strip_prefix('~'), home) {
        (Some(rest), Some(home)) => format!("{}{rest}", home.display()),
        _ => dir.to_string(),
    }
}

impl Extra {
    /// Set `env` on `command`. Call it after `scrub` and before `non_interactive`, so what
    /// bhai fixes is never overridden.
    pub fn apply(&self, command: &mut tokio::process::Command) {
        command.envs(&self.env);
    }

    /// `script` with `path` put in front of `PATH`. It is done in the script because a
    /// login shell's profile rebuilds `PATH` (macOS's `path_helper` moves anything it did
    /// not list to the end). One line, so the script's line numbers stay as they were.
    pub fn script(&self, script: &str) -> String {
        if self.path.is_empty() {
            return script.to_string();
        }
        let dirs: Vec<String> = self.path.iter().map(|dir| quote(dir)).collect();
        format!(
            "export PATH={}\"${{PATH:+:$PATH}}\"; {script}",
            dirs.join(":")
        )
    }
}

/// `text` as one single-quoted shell word.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
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
    fn native_terminal_control_is_never_forwarded_to_tools() {
        let socket = TERMINAL_SOCKET_ENV;
        assert_eq!(
            withheld_from(names(&[socket]).into_iter(), &[socket.to_string()]),
            names(&[socket])
        );
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

    fn extra(env: &[(&str, &str)], path: &[&str]) -> Extra {
        Extra {
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            path: path.iter().map(|d| d.to_string()).collect(),
        }
    }

    #[test]
    fn a_trusted_project_goes_first_and_an_untrusted_one_not_at_all() {
        let settings = Settings {
            user: extra(&[("A", "user"), ("B", "user")], &["/u1", "/u2"]),
            project: extra(&[("B", "project")], &["/p1", "/p2"]),
        };
        assert_eq!(settings.merged(false), settings.user);
        assert_eq!(
            settings.merged(true),
            extra(
                &[("A", "user"), ("B", "project")],
                &["/p1", "/p2", "/u1", "/u2"]
            )
        );
    }

    #[test]
    fn path_goes_in_front_in_order_with_the_home_expanded() {
        let home = Path::new("/home/me");
        assert_eq!(expand("~/.local/bin", Some(home)), "/home/me/.local/bin");
        assert_eq!(expand("~", Some(home)), "/home/me");
        assert_eq!(expand("/opt/~x", Some(home)), "/opt/~x");
        assert_eq!(expand("~/bin", None), "~/bin");
        let script = extra(&[], &["/a b", "/it's"]).script("node -v");
        assert_eq!(
            script,
            r#"export PATH='/a b':'/it'\''s'"${PATH:+:$PATH}"; node -v"#
        );
        assert_eq!(extra(&[], &[]).script("node -v"), "node -v");
    }

    #[test]
    fn env_may_not_name_what_bhai_withholds_or_fixes() {
        for name in [
            "GITHUB_TOKEN",
            TERMINAL_SOCKET_ENV,
            "TERM",
            "LC_ALL",
            "BHAI_X",
            "PATH",
        ] {
            assert!(refused(name, &[]).is_some(), "{name}");
        }
        for name in ["", "1A", "A-B", "A=B"] {
            assert!(refused(name, &[]).is_some(), "{name:?}");
        }
        assert_eq!(refused("NODE_ENV", &[]), None);
        assert_eq!(refused("NPM_TOKEN", &["NPM_TOKEN".to_string()]), None);
        assert!(refused(TERMINAL_SOCKET_ENV, &[TERMINAL_SOCKET_ENV.to_string()]).is_some());
        assert_eq!(refused_dir("/opt/node/bin"), None);
        assert_eq!(refused_dir("~/bin"), None);
        for dir in ["bin", "./bin", "~other/bin", "/a:/b", ""] {
            assert!(refused_dir(dir).is_some(), "{dir:?}");
        }
    }

    /// `set_var` is unsafe, so the test runs itself again as a child with the secret set.
    #[test]
    fn withheld_values_are_redacted_once_the_pass_list_is_set() {
        const NAME: &str =
            "childenv::tests::withheld_values_are_redacted_once_the_pass_list_is_set";
        if std::env::var_os("BHAI_TEST_CHILD").is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture"])
                .env("BHAI_TEST_CHILD", "1")
                .env("BHAI_TEST_REDACT_TOKEN", "childenv-withheld-4c1e9a")
                .env("BHAI_TEST_REDACT_PLAIN", "childenv-plain-value")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success() && stdout.contains("1 passed"),
                "{stdout}{}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        set_pass(&[]);
        let out = crate::redact::apply("childenv-withheld-4c1e9a childenv-plain-value");
        assert_eq!(out, "[REDACTED] childenv-plain-value");
    }
}
