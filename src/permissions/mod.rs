//! Permission modes and the policy that decides, before the approval prompt, whether a
//! tool call runs, is rejected, or needs the user.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod bash;
pub mod rules;

use rules::Base;
pub use rules::Rule;

/// How much the policy may decide on its own. Deny rules reject in every mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Every call that needs approval prompts.
    #[default]
    Ask,
    /// Allow rules and read-only commands run; everything else prompts.
    Auto,
    /// Everything runs, except ask rules and protected paths, which prompt.
    Bypass,
}

impl Mode {
    const ALL: [Mode; 3] = [Mode::Ask, Mode::Auto, Mode::Bypass];

    pub fn next(self) -> Self {
        Self::ALL[(self as usize + 1) % Self::ALL.len()]
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Ask => "ask",
            Mode::Auto => "auto",
            Mode::Bypass => "bypass",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|m| m.as_str() == s)
            .ok_or_else(|| format!("unknown mode `{s}`, expected ask, auto or bypass"))
    }
}

/// The `[permissions]` rule lists.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rules {
    pub allow: Vec<Rule>,
    pub deny: Vec<Rule>,
    pub ask: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Run without prompting; the reason is shown in the transcript.
    Allow(String),
    /// Reject without prompting.
    Deny(String),
    Ask,
}

#[derive(Debug, Default)]
pub struct Policy {
    mode: AtomicU8,
    rules: Rules,
    home: Option<PathBuf>,
    cwd: PathBuf,
}

impl Policy {
    pub fn new(mode: Mode, rules: Rules, home: Option<PathBuf>, cwd: PathBuf) -> Self {
        Self {
            mode: AtomicU8::new(mode as u8),
            rules,
            home,
            cwd,
        }
    }

    pub fn mode(&self) -> Mode {
        Mode::ALL[self.mode.load(Ordering::Relaxed) as usize]
    }

    pub fn set_mode(&self, mode: Mode) {
        self.mode.store(mode as u8, Ordering::Relaxed);
    }

    /// Decide a call to `tool`. Tools that skip approval only answer to deny and ask
    /// rules; their `Allow` carries no reason and needs no notice.
    pub fn check(&self, tool: &str, args: &Value, needs_approval: bool) -> Decision {
        let text = |key| args.get(key).and_then(Value::as_str);
        match tool {
            "bash" => self.check_bash(text("command").unwrap_or_default()),
            "read" | "write" | "edit" => match text("path") {
                Some(path) => self.check_path(tool, Path::new(path), needs_approval),
                None => Decision::Ask,
            },
            _ => self.check_other(tool, needs_approval),
        }
    }

    fn check_other(&self, tool: &str, needs_approval: bool) -> Decision {
        let find = |rules: &[Rule]| rules.iter().find(|r| r.applies_to(tool)).cloned();
        if let Some(rule) = find(&self.rules.deny) {
            return denied(&rule);
        }
        if find(&self.rules.ask).is_some() {
            return Decision::Ask;
        }
        if !needs_approval {
            return Decision::Allow(String::new());
        }
        self.fallback(find(&self.rules.allow).map(|r| rule_reason(&r)))
    }

    fn check_path(&self, tool: &str, path: &Path, needs_approval: bool) -> Decision {
        let base = self.base();
        let find = |rules: &[Rule], fold: bool| {
            rules
                .iter()
                .find(|r| r.applies_to(tool) && r.matches_path(path, base, fold))
                .cloned()
        };
        if let Some(rule) = find(&self.rules.deny, true) {
            return denied(&rule);
        }
        if find(&self.rules.ask, true).is_some() {
            return Decision::Ask;
        }
        if !needs_approval {
            return Decision::Allow(String::new());
        }
        if rules::is_protected(path, self.home.as_deref()) {
            return Decision::Ask;
        }
        self.fallback(find(&self.rules.allow, false).map(|r| rule_reason(&r)))
    }

    fn check_bash(&self, command: &str) -> Decision {
        let any = |rules: &[Rule]| {
            rules
                .iter()
                .find(|r| r.applies_to("bash") && r.is_any())
                .cloned()
        };
        let Some(commands) = bash::parse(command) else {
            // Unparseable: only a rule for every command can decide it, and never allow.
            return match any(&self.rules.deny) {
                Some(rule) => denied(&rule),
                None => Decision::Ask,
            };
        };
        // Deny and ask rules also see the program behind wrappers and paths, in any case.
        let find_loose = |rules: &[Rule], c: &bash::Command| {
            let forms = loose_forms(c);
            rules
                .iter()
                .find(|r| r.applies_to("bash") && forms.iter().any(|w| r.matches_words(w, true)))
                .cloned()
        };
        for c in &commands {
            if let Some(rule) = find_loose(&self.rules.deny, c) {
                return denied(&rule);
            }
        }
        if commands
            .iter()
            .any(|c| find_loose(&self.rules.ask, c).is_some() || bash::mentions_protected(c))
        {
            return Decision::Ask;
        }

        let mut reasons: Vec<String> = Vec::new();
        for c in &commands {
            let reason = self
                .rules
                .allow
                .iter()
                .find(|r| r.applies_to("bash") && r.matches_words(&c.words, false))
                .map(rule_reason)
                .or_else(|| bash::is_read_only(&c.words).then(|| "read-only".to_string()));
            match reason {
                Some(reason) => {
                    if !reasons.contains(&reason) {
                        reasons.push(reason);
                    }
                }
                None => return self.fallback(None),
            }
        }
        self.fallback(Some(reasons.join(", ")))
    }

    /// What the mode makes of a call no deny, ask or protected check stopped.
    fn fallback(&self, allowed: Option<String>) -> Decision {
        match (self.mode(), allowed) {
            (Mode::Ask, _) => Decision::Ask,
            (_, Some(reason)) => Decision::Allow(reason),
            (Mode::Bypass, None) => Decision::Allow("bypass mode".to_string()),
            (Mode::Auto, None) => Decision::Ask,
        }
    }

    fn base(&self) -> Base<'_> {
        Base {
            home: self.home.as_deref(),
            cwd: &self.cwd,
        }
    }
}

/// The words as written, and again from the real program with its path dropped.
fn loose_forms(command: &bash::Command) -> Vec<Vec<String>> {
    let mut forms = vec![command.words.clone()];
    for words in [&command.words[..], command.unwrapped()] {
        let mut words = words.to_vec();
        if let Some(first) = words.first_mut() {
            *first = bash::basename(first).to_string();
        }
        forms.push(words);
    }
    forms
}

fn rule_reason(rule: &Rule) -> String {
    format!("rule {}", rule.text)
}

fn denied(rule: &Rule) -> Decision {
    Decision::Deny(format!("deny rule {}", rule.text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules(list: &[&str]) -> Vec<Rule> {
        list.iter().map(|r| Rule::parse(r).unwrap()).collect()
    }

    fn policy(mode: Mode, allow: &[&str], deny: &[&str], ask: &[&str]) -> Policy {
        let rules = Rules {
            allow: rules(allow),
            deny: rules(deny),
            ask: rules(ask),
        };
        Policy::new(mode, rules, Some("/home/u".into()), "/home/u/repo".into())
    }

    fn bash(policy: &Policy, command: &str) -> Decision {
        policy.check("bash", &json!({ "command": command }), true)
    }

    fn file(policy: &Policy, tool: &str, path: &str) -> Decision {
        policy.check(tool, &json!({ "path": path }), tool != "read")
    }

    fn allowed(reason: &str) -> Decision {
        Decision::Allow(reason.to_string())
    }

    #[test]
    fn modes_cycle_and_parse() {
        assert_eq!(Mode::default(), Mode::Ask);
        assert_eq!(Mode::Ask.next(), Mode::Auto);
        assert_eq!(Mode::Auto.next(), Mode::Bypass);
        assert_eq!(Mode::Bypass.next(), Mode::Ask);
        assert_eq!("bypass".parse(), Ok(Mode::Bypass));
        assert!("yolo".parse::<Mode>().is_err());
        assert_eq!(serde_json::to_value(Mode::Auto).unwrap(), json!("auto"));
        let policy = Policy::default();
        policy.set_mode(policy.mode().next());
        assert_eq!(policy.mode(), Mode::Auto);
    }

    #[test]
    fn bash_decisions() {
        let allow = ["Bash(git log:*)", "Bash(cargo build)"];
        let deny = ["Bash(rm:*)", "Bash(git push:*)"];
        let ask = ["Bash(git log -p:*)"];
        let auto = policy(Mode::Auto, &allow, &deny, &ask);
        let ask_mode = policy(Mode::Ask, &allow, &deny, &ask);
        let bypass = policy(Mode::Bypass, &allow, &deny, &ask);
        let deny_rm = Decision::Deny("deny rule Bash(rm:*)".to_string());
        let cases = [
            (&auto, "git log --oneline", allowed("rule Bash(git log:*)")),
            (&auto, "ls && git status", allowed("read-only")),
            (
                &auto,
                "cargo build && git log",
                allowed("rule Bash(cargo build), rule Bash(git log:*)"),
            ),
            (&auto, "git log | npm test", Decision::Ask),
            (&auto, "git status; rm -rf ~", deny_rm.clone()),
            (&auto, "timeout 5 /bin/RM x", deny_rm.clone()),
            (
                &auto,
                "git push origin main",
                Decision::Deny("deny rule Bash(git push:*)".to_string()),
            ),
            // Ask beats allow.
            (&auto, "git log -p", Decision::Ask),
            (&auto, "ls $(rm x)", Decision::Ask),
            (&auto, "cat .env", Decision::Ask),
            (&auto, "cargo test", Decision::Ask),
            (&ask_mode, "ls", Decision::Ask),
            (&ask_mode, "rm x", deny_rm.clone()),
            (&bypass, "npm test", allowed("bypass mode")),
            (&bypass, "ls", allowed("read-only")),
            (&bypass, "rm x", deny_rm),
            (&bypass, "cat ~/.ssh/id_rsa", Decision::Ask),
            (&bypass, "echo x > out", Decision::Ask),
        ];
        for (policy, command, want) in cases {
            assert_eq!(
                bash(policy, command),
                want,
                "{command} in {}",
                policy.mode()
            );
        }
    }

    #[test]
    fn a_bare_deny_covers_unparseable_commands() {
        let policy = policy(Mode::Bypass, &[], &["Bash"], &[]);
        assert_eq!(
            bash(&policy, "ls $(x)"),
            Decision::Deny("deny rule Bash".to_string())
        );
    }

    #[test]
    fn file_decisions() {
        let allow = ["Edit(src/**)", "Write(//tmp/**)", "Write(.git/**)"];
        let deny = ["Edit(secrets/**)", "Read(*.pem)"];
        let ask = ["Write(src/generated/**)"];
        let auto = policy(Mode::Auto, &allow, &deny, &ask);
        let ask_mode = policy(Mode::Ask, &allow, &deny, &ask);
        let bypass = policy(Mode::Bypass, &allow, &deny, &ask);
        let cases = [
            (
                &auto,
                "edit",
                "/home/u/repo/src/main.rs",
                allowed("rule Edit(src/**)"),
            ),
            (
                &auto,
                "write",
                "/home/u/repo/src/main.rs",
                allowed("rule Edit(src/**)"),
            ),
            (&auto, "write", "/tmp/x", allowed("rule Write(//tmp/**)")),
            (&auto, "edit", "/tmp/x", Decision::Ask),
            (
                &auto,
                "write",
                "/home/u/repo/src/generated/x.rs",
                Decision::Ask,
            ),
            (
                &auto,
                "write",
                "/home/u/repo/secrets/k",
                Decision::Deny("deny rule Edit(secrets/**)".to_string()),
            ),
            // Allow rules never open protected paths.
            (&auto, "write", "/home/u/repo/.git/config", Decision::Ask),
            (&bypass, "write", "/home/u/repo/.git/config", Decision::Ask),
            (&bypass, "edit", "/home/u/.ssh/config", Decision::Ask),
            (&bypass, "edit", "/home/u/repo/.env", Decision::Ask),
            (
                &bypass,
                "edit",
                "/home/u/repo/README.md",
                allowed("bypass mode"),
            ),
            (&ask_mode, "edit", "/home/u/repo/src/main.rs", Decision::Ask),
            (&ask_mode, "edit", "/home/u/repo/.git/HEAD", Decision::Ask),
            // Reads skip approval: only deny and ask rules touch them.
            (&ask_mode, "read", "/home/u/repo/.env", allowed("")),
            (
                &ask_mode,
                "read",
                "/home/u/repo/k.PEM",
                Decision::Deny("deny rule Read(*.pem)".to_string()),
            ),
        ];
        for (policy, tool, path, want) in cases {
            assert_eq!(
                file(policy, tool, path),
                want,
                "{tool} {path} in {}",
                policy.mode()
            );
        }
    }

    #[test]
    fn other_tools_follow_bare_rules() {
        let args = json!({ "name": "x" });
        let deny = policy(Mode::Bypass, &[], &["Skill"], &[]);
        assert_eq!(
            deny.check("skill", &args, false),
            Decision::Deny("deny rule Skill".to_string())
        );
        let ask = policy(Mode::Bypass, &[], &[], &["Skill"]);
        assert_eq!(ask.check("skill", &args, false), Decision::Ask);
        assert_eq!(Policy::default().check("skill", &args, false), allowed(""));
    }
}
