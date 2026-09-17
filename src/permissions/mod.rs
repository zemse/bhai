//! Permission modes and the policy that decides, before the approval prompt, whether a
//! tool call runs, is rejected, or needs the user.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{RwLock, RwLockReadGuard};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod bash;
pub mod rules;
pub mod settings;
pub mod trust;

use rules::Base;
pub use rules::Rule;
pub use trust::Trust;

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

impl Rules {
    pub fn extend(&mut self, other: Rules) {
        self.allow.extend(other.allow);
        self.deny.extend(other.deny);
        self.ask.extend(other.ask);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Run without prompting; the reason is shown in the transcript.
    Allow(String),
    /// Reject without prompting.
    Deny(String),
    Ask,
}

/// Which offered rule an approval should remember.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Remember {
    Exact,
    Prefix,
}

impl Remember {
    pub fn as_str(self) -> &'static str {
        match self {
            Remember::Exact => "exact",
            Remember::Prefix => "prefix",
        }
    }
}

/// The user's answer to an approval prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Reject,
    Accept(Option<Remember>),
}

impl Answer {
    pub fn accepted(self) -> bool {
        self != Answer::Reject
    }

    pub fn remember(self) -> Option<Remember> {
        match self {
            Answer::Accept(remember) => remember,
            Answer::Reject => None,
        }
    }
}

/// Allow rules an approval prompt may offer to remember, as rule text.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Offers {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exact: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
}

impl Offers {
    pub fn get(&self, remember: Remember) -> Option<&str> {
        match remember {
            Remember::Exact => self.exact.as_deref(),
            Remember::Prefix => self.prefix.as_deref(),
        }
    }
}

#[derive(Debug, Default)]
pub struct Policy {
    mode: AtomicU8,
    /// Allow rules grow as approvals are remembered.
    rules: RwLock<Rules>,
    home: Option<PathBuf>,
    cwd: PathBuf,
    /// Where remembered rules are saved; `None` keeps them for this session only.
    store: Option<PathBuf>,
    trust: Option<Trust>,
    /// Whether repo-supplied allow rules apply.
    trusted: AtomicBool,
}

impl Policy {
    pub fn new(mode: Mode, rules: Rules, home: Option<PathBuf>, cwd: PathBuf) -> Self {
        Self {
            mode: AtomicU8::new(mode as u8),
            rules: RwLock::new(rules),
            home,
            cwd,
            store: None,
            trust: None,
            trusted: AtomicBool::new(false),
        }
    }

    pub fn with_store(self, store: PathBuf) -> Self {
        Self {
            store: Some(store),
            ..self
        }
    }

    pub fn with_trust(self, trust: Trust) -> Self {
        let trusted = trust.is_trusted();
        let policy = Self {
            trust: Some(trust),
            ..self
        };
        if trusted {
            policy.set_trusted();
        }
        policy
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
        let rules = self.rules();
        self.checker(&rules, self.mode())
            .check(tool, args, needs_approval)
    }

    /// The rules the approval prompt for this call may offer: each one is offered only
    /// if, once added, it would let this very call run in `auto` mode.
    pub fn offers(&self, tool: &str, args: &Value) -> Offers {
        let base = self.base();
        let (exact, prefix) = match (tool, args.get("command"), args.get("path")) {
            ("bash", Some(Value::String(command)), _) => (
                rules::exact_command(command),
                rules::prefix_command(command),
            ),
            ("write" | "edit", _, Some(Value::String(path))) => {
                let path = Path::new(path);
                if rules::is_protected(path, base.home) {
                    (None, None)
                } else {
                    (
                        rules::exact_path(tool, path, base),
                        rules::dir_path(path, base),
                    )
                }
            }
            ("mcp_call", _, _) => match mcp_name(args) {
                Some(name) => (
                    Rule::parse(&name).ok(),
                    name["mcp__".len()..]
                        .split_once("__")
                        .and_then(|(server, _)| Rule::parse(&format!("mcp__{server}")).ok()),
                ),
                None => (None, None),
            },
            _ => (None, None),
        };
        let works = |rule: Option<Rule>| {
            let rule = rule?;
            let mut rules = self.rules().clone();
            rules.allow.insert(0, rule.clone());
            let decision = self.checker(&rules, Mode::Auto).check(tool, args, true);
            matches!(decision, Decision::Allow(_)).then_some(rule.text)
        };
        Offers {
            exact: works(exact),
            prefix: works(prefix),
        }
    }

    /// Allow `text` for the rest of the session, then save it if there is a store.
    /// Returns where it was saved.
    pub fn remember(&self, text: &str) -> anyhow::Result<Option<&Path>> {
        let source = match &self.store {
            Some(store) => store.display().to_string(),
            None => "this session".to_string(),
        };
        let rule = Rule::parse(text)
            .map_err(anyhow::Error::msg)?
            .with_source(source)
            .by_user();
        {
            let mut rules = self.rules.write().unwrap_or_else(|e| e.into_inner());
            if !rules.allow.iter().any(|r| r.text == rule.text && !r.repo) {
                rules.allow.push(rule);
            }
        }
        if let Some(store) = &self.store {
            // The user's own approvals keep a trusted project trusted, and make a
            // project with no repo-supplied allow rules trusted.
            let keep = self
                .trust
                .as_ref()
                .filter(|t| t.is_trusted() || t.allow_rules().is_empty());
            settings::remember(store, text)?;
            if let Some(trust) = keep {
                trust.trust()?;
                self.set_trusted();
            }
        }
        Ok(self.store.as_deref())
    }

    /// Honour the repo-supplied allow rules as the files are now, for `/trust`.
    pub fn trust(&self) -> anyhow::Result<String> {
        let trust = self.trust.as_ref().context("no trust store")?;
        trust.trust()?;
        self.set_trusted();
        let rules = trust.allow_rules();
        let mut out = format!("trusted {} repo-supplied allow rules", rules.len());
        for (source, rule) in rules {
            out.push_str(&format!("\n  {rule}  ({source})"));
        }
        Ok(out)
    }

    /// Stop honouring the repo-supplied allow rules, for `/untrust`.
    pub fn untrust(&self) -> anyhow::Result<String> {
        let trust = self.trust.as_ref().context("no trust store")?;
        self.trusted.store(false, Ordering::Relaxed);
        Ok(match trust.untrust()? {
            true => "untrusted this project's allow rules".to_string(),
            false => "this project was not trusted".to_string(),
        })
    }

    /// The startup notice when the repo ships allow rules that are not trusted.
    pub fn trust_notice(&self) -> Option<String> {
        let trust = self.trust.as_ref()?;
        let count = trust.allow_rules().len();
        (count > 0 && !self.trusted())
            .then(|| format!("this repo ships {count} allow rules; /trust to honour them"))
    }

    /// Honour the repo-supplied allow rules, replacing the loaded ones with the files as
    /// they are now, so the rules that apply are the ones trusted.
    fn set_trusted(&self) {
        let Some(trust) = &self.trust else { return };
        let fresh = trust.load_allow();
        let mut rules = self.rules.write().unwrap_or_else(|e| e.into_inner());
        rules.allow.retain(|r| !r.repo);
        let fresh: Vec<Rule> = fresh
            .into_iter()
            .filter(|f| !rules.allow.iter().any(|r| r.text == f.text))
            .collect();
        rules.allow.extend(fresh);
        self.trusted.store(true, Ordering::Relaxed);
    }

    fn trusted(&self) -> bool {
        self.trusted.load(Ordering::Relaxed)
    }

    /// What `/permissions` prints: the mode, then each rule, where it came from and
    /// whether the mode ignores it.
    pub fn describe(&self) -> String {
        let rules = self.rules();
        let (mode, trusted) = (self.mode(), self.trusted());
        let mut out = format!("permission mode: {mode}");
        if mode == Mode::Ask {
            out.push_str(
                " (only your own and remembered allow rules apply; read-only commands still ask)",
            );
        }
        for (name, list) in [
            ("deny", &rules.deny),
            ("ask", &rules.ask),
            ("allow", &rules.allow),
        ] {
            if list.is_empty() {
                out.push_str(&format!("\n{name}: none"));
                continue;
            }
            out.push_str(&format!("\n{name}:"));
            for rule in list {
                let source = match rule.source.as_str() {
                    "" => "built in",
                    source => source,
                };
                let inactive = if name != "allow" {
                    String::new()
                } else if rule.repo && !trusted {
                    ", untrusted".to_string()
                } else if !allows_in(rule, mode, trusted) {
                    format!(", inactive in {mode} mode")
                } else {
                    String::new()
                };
                out.push_str(&format!("\n  {}  ({source}{inactive})", rule.text));
            }
        }
        out
    }

    fn rules(&self) -> RwLockReadGuard<'_, Rules> {
        self.rules.read().unwrap_or_else(|e| e.into_inner())
    }

    fn checker<'a>(&'a self, rules: &'a Rules, mode: Mode) -> Checker<'a> {
        Checker {
            rules,
            mode,
            trusted: self.trusted(),
            base: self.base(),
        }
    }

    fn base(&self) -> Base<'_> {
        Base {
            home: self.home.as_deref(),
            cwd: &self.cwd,
        }
    }
}

/// Whether an allow rule counts in `mode`: `ask` only honours the user's own rules, and
/// repo-supplied rules need trust.
fn allows_in(rule: &Rule, mode: Mode, trusted: bool) -> bool {
    (trusted || !rule.repo) && (mode != Mode::Ask || rule.user)
}

/// The lowercase `mcp__server__tool` name an `mcp_call` runs, as rules name it.
fn mcp_name(args: &Value) -> Option<String> {
    let name = args.get("name").and_then(Value::as_str)?;
    name.starts_with("mcp__").then(|| name.to_lowercase())
}

/// One decision against a fixed set of rules and a mode.
struct Checker<'a> {
    rules: &'a Rules,
    mode: Mode,
    trusted: bool,
    base: Base<'a>,
}

impl Checker<'_> {
    fn check(&self, tool: &str, args: &Value, needs_approval: bool) -> Decision {
        let text = |key| args.get(key).and_then(Value::as_str);
        match tool {
            "bash" => self.check_bash(text("command").unwrap_or_default()),
            "read" | "write" | "edit" => match text("path") {
                Some(path) => self.check_path(tool, Path::new(path), needs_approval),
                None => Decision::Ask,
            },
            "mcp_call" => match mcp_name(args) {
                Some(name) => self.check_other(&name, needs_approval),
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
        self.fallback(find(&self.allow()).map(|r| rule_reason(&r)))
    }

    fn check_path(&self, tool: &str, path: &Path, needs_approval: bool) -> Decision {
        let base = self.base;
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
        if rules::is_protected(path, base.home) {
            return Decision::Ask;
        }
        self.fallback(find(&self.allow(), false).map(|r| rule_reason(&r)))
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
            let read_only = self.mode != Mode::Ask && bash::is_read_only(&c.words);
            let reason = self
                .allow()
                .iter()
                .find(|r| r.applies_to("bash") && r.matches_words(&c.words, false))
                .map(rule_reason)
                .or_else(|| read_only.then(|| "read-only".to_string()));
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

    /// The allow rules this mode honours.
    fn allow(&self) -> Vec<Rule> {
        self.rules
            .allow
            .iter()
            .filter(|r| allows_in(r, self.mode, self.trusted))
            .cloned()
            .collect()
    }

    /// What the mode makes of a call no deny, ask or protected check stopped.
    fn fallback(&self, allowed: Option<String>) -> Decision {
        match (self.mode, allowed) {
            (_, Some(reason)) => Decision::Allow(reason),
            (Mode::Bypass, None) => Decision::Allow("bypass mode".to_string()),
            (Mode::Ask | Mode::Auto, None) => Decision::Ask,
        }
    }
}

/// The words as written, and again from the real program with its path dropped. Behind a
/// wrapper, every later word may be the program, since wrapper flags can take values.
fn loose_forms(command: &bash::Command) -> Vec<Vec<String>> {
    let mut forms = vec![command.words.clone()];
    let wrapped = command.unwrapped().len() < command.words.len();
    let starts = if wrapped {
        1..command.words.len()
    } else {
        0..1
    };
    for words in starts.map(|i| &command.words[i..]) {
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
    fn mcp_calls_are_checked_by_their_mcp_name() {
        let call = |name: &str| json!({ "name": name, "arguments": {} });
        let p = policy(
            Mode::Auto,
            &["mcp__github__get_issue", "mcp__fs"],
            &["mcp__github__delete*"],
            &["mcp__fs__write"],
        );
        let check = |name: &str| p.check("mcp_call", &call(name), true);
        assert_eq!(
            check("mcp__github__get_issue"),
            allowed("rule mcp__github__get_issue")
        );
        assert!(matches!(
            check("mcp__github__delete_repo"),
            Decision::Deny(_)
        ));
        assert_eq!(check("mcp__github__create_issue"), Decision::Ask);
        assert!(matches!(check("mcp__fs__read"), Decision::Allow(_)));
        assert_eq!(check("mcp__fs__write"), Decision::Ask);
        assert_eq!(p.check("mcp_call", &json!({}), true), Decision::Ask);

        let offers = p.offers("mcp_call", &call("mcp__GitHub__create_issue"));
        assert_eq!(offers.exact.as_deref(), Some("mcp__github__create_issue"));
        assert_eq!(offers.prefix.as_deref(), Some("mcp__github"));
        assert_eq!(
            p.offers("mcp_call", &call("mcp__fs__write")),
            Offers::default()
        );
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
            (&bypass, "env -u FOO rm x", deny_rm.clone()),
            (&bypass, "timeout -s KILL 5 rm x", deny_rm.clone()),
            (&bypass, "xargs -I X rm X", deny_rm.clone()),
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
    fn ask_mode_honours_only_the_users_allow_rules() {
        let mut allow = rules(&["Bash(npm test)", "Write(docs/**)"]);
        allow.extend(
            rules(&["Bash(cargo build)", "Bash(git log:*)", "Write(src/**)"])
                .into_iter()
                .map(Rule::by_user),
        );
        let rules = Rules {
            allow,
            deny: rules(&["Bash(cargo build --release)"]),
            ask: rules(&["Bash(git log -p:*)"]),
        };
        let at = |mode| {
            Policy::new(
                mode,
                rules.clone(),
                Some("/home/u".into()),
                "/home/u/repo".into(),
            )
        };
        let (ask, auto) = (at(Mode::Ask), at(Mode::Auto));
        let user_rule = allowed("rule Bash(cargo build)");
        let cases = [
            (&ask, "cargo build", user_rule.clone()),
            (&auto, "cargo build", user_rule),
            (&ask, "npm test", Decision::Ask),
            (&auto, "npm test", allowed("rule Bash(npm test)")),
            (&ask, "ls", Decision::Ask),
            (&auto, "ls", allowed("read-only")),
            (&ask, "git log && ls", Decision::Ask),
            (&ask, "git log -p", Decision::Ask),
            (&ask, "git log .env", Decision::Ask),
            (
                &ask,
                "cargo build --release",
                Decision::Deny("deny rule Bash(cargo build --release)".to_string()),
            ),
        ];
        for (policy, command, want) in cases {
            assert_eq!(
                bash(policy, command),
                want,
                "{command} in {}",
                policy.mode()
            );
        }
        let files = [
            (&ask, "/home/u/repo/src/a.rs", allowed("rule Write(src/**)")),
            (&ask, "/home/u/repo/docs/a.md", Decision::Ask),
            (
                &auto,
                "/home/u/repo/docs/a.md",
                allowed("rule Write(docs/**)"),
            ),
            (&ask, "/home/u/repo/src/.env", Decision::Ask),
        ];
        for (policy, path, want) in files {
            assert_eq!(
                file(policy, "write", path),
                want,
                "{path} in {}",
                policy.mode()
            );
        }

        ask.remember("Bash(npm test:*)").unwrap();
        assert_eq!(bash(&ask, "npm test"), allowed("rule Bash(npm test:*)"));
        let described = ask.describe();
        assert!(
            described.contains("only your own and remembered"),
            "{described}"
        );
        assert!(
            described.contains("Bash(npm test)  (built in, inactive in ask mode)"),
            "{described}"
        );
        assert!(
            described.contains("Bash(cargo build)  (built in)\n"),
            "{described}"
        );
        assert!(
            described.contains("Bash(npm test:*)  (this session)"),
            "{described}"
        );
        assert!(!auto.describe().contains("inactive"));
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

    #[test]
    fn offers_only_rules_that_would_let_the_call_run() {
        let policy = policy(
            Mode::Ask,
            &[],
            &["Bash(git push:*)"],
            &["Bash(git log -p:*)"],
        );
        let offers = |tool, args: Value| policy.offers(tool, &args);
        let both = |exact: &str, prefix: &str| Offers {
            exact: Some(exact.to_string()),
            prefix: Some(prefix.to_string()),
        };
        assert_eq!(
            offers("bash", json!({"command": "git log --oneline"})),
            both("Bash(git log --oneline)", "Bash(git log:*)")
        );
        // The prefix would still hit the ask rule; a deny rule wins over both.
        assert_eq!(
            offers("bash", json!({"command": "git log -p"})),
            Offers::default()
        );
        assert_eq!(
            offers("bash", json!({"command": "git push"})),
            Offers::default()
        );
        assert_eq!(
            offers("bash", json!({"command": "ls | wc -l"})),
            Offers::default()
        );
        assert_eq!(
            offers("edit", json!({"path": "/home/u/repo/src/a.rs"})),
            both("Edit(/src/a.rs)", "Edit(/src/**)")
        );
        assert_eq!(
            offers("write", json!({"path": "/home/u/repo/.env"})),
            Offers::default()
        );
        assert_eq!(offers("skill", json!({"name": "x"})), Offers::default());
    }

    #[test]
    fn a_remembered_rule_applies_to_later_calls_and_is_saved() {
        let dir = std::env::temp_dir().join(format!("bhai-policy-{}", uuid::Uuid::new_v4()));
        let store = dir.join(settings::LOCAL);
        let policy =
            Policy::new(Mode::Auto, Rules::default(), None, dir.clone()).with_store(store.clone());
        assert_eq!(bash(&policy, "cargo test --all"), Decision::Ask);
        let rule = policy
            .offers("bash", &json!({"command": "cargo test"}))
            .prefix;
        assert_eq!(rule.as_deref(), Some("Bash(cargo test:*)"));
        assert_eq!(
            policy.remember("Bash(cargo test:*)").unwrap(),
            Some(store.as_path())
        );
        policy.remember("Bash(cargo test:*)").unwrap();
        assert_eq!(
            bash(&policy, "cargo test --all"),
            allowed("rule Bash(cargo test:*)")
        );
        let (saved, _) = settings::load_local(&store);
        assert_eq!(saved.len(), 1);
        let described = policy.describe();
        assert_eq!(described.matches("Bash(cargo test:*)").count(), 1);
        assert!(
            described.contains(&store.display().to_string()),
            "{described}"
        );
        assert!(policy.remember("Bash(ls").is_err());
        std::fs::remove_dir_all(dir).unwrap();

        let memory = Policy::default();
        assert_eq!(memory.remember("Bash(ls)").unwrap(), None);
        assert!(memory.describe().contains("Bash(ls)  (this session)"));
    }

    /// A policy for `repo` as `main` builds it, with its trust store under `dir`.
    fn repo_policy(dir: &Path, repo: &Path) -> Policy {
        let store = repo.join(settings::LOCAL);
        let (mut rules, _) = settings::claude(None, repo);
        rules.allow.extend(settings::load_local(&store).0);
        Policy::new(Mode::Ask, rules, None, repo.to_path_buf())
            .with_store(store)
            .with_trust(Trust::new(&dir.join("config"), repo))
    }

    fn write_settings(path: &Path, value: Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, value.to_string()).unwrap();
    }

    #[test]
    fn untrusted_repos_lose_their_allow_rules_but_keep_deny() {
        let dir = std::env::temp_dir().join(format!("bhai-policy-{}", uuid::Uuid::new_v4()));
        let repo = dir.join("repo");
        write_settings(
            &repo.join(settings::LOCAL),
            json!({"permissions": {"allow": ["Bash(make:*)"]}}),
        );
        write_settings(
            &repo.join(settings::CLAUDE_LOCAL),
            json!({"permissions": {"allow": ["Bash(npm test)"], "deny": ["Bash(rm:*)"]}}),
        );
        let policy = repo_policy(&dir, &repo);
        policy.set_mode(Mode::Auto);
        let deny_rm = Decision::Deny("deny rule Bash(rm:*)".to_string());
        assert_eq!(bash(&policy, "make all"), Decision::Ask);
        assert_eq!(bash(&policy, "npm test"), Decision::Ask);
        assert_eq!(bash(&policy, "rm x"), deny_rm);
        assert_eq!(
            policy.trust_notice().as_deref(),
            Some("this repo ships 2 allow rules; /trust to honour them")
        );
        let described = policy.describe();
        assert!(described.contains("Bash(make:*)  ("), "{described}");
        assert_eq!(described.matches(", untrusted)").count(), 2, "{described}");

        let trusted = policy.trust().unwrap();
        assert!(trusted.contains("Bash(npm test)"), "{trusted}");
        assert_eq!(bash(&policy, "make all"), allowed("rule Bash(make:*)"));
        assert_eq!(bash(&policy, "rm x"), deny_rm);
        assert_eq!(policy.trust_notice(), None);
        assert!(repo_policy(&dir, &repo).trusted());

        // An edit bhai did not make untrusts the project again.
        write_settings(
            &repo.join(settings::LOCAL),
            json!({"permissions": {"allow": ["Bash(make:*)", "Bash(curl:*)"]}}),
        );
        let reloaded = repo_policy(&dir, &repo);
        assert!(!reloaded.trusted());
        assert_eq!(
            reloaded.trust_notice().unwrap(),
            "this repo ships 3 allow rules; /trust to honour them"
        );
        reloaded.untrust().unwrap();
        policy.untrust().unwrap();
        assert_eq!(bash(&policy, "make all"), Decision::Ask);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trusting_reloads_the_repo_allow_rules_from_disk() {
        let dir = std::env::temp_dir().join(format!("bhai-policy-{}", uuid::Uuid::new_v4()));
        let repo = dir.join("repo");
        write_settings(
            &repo.join(settings::LOCAL),
            json!({"permissions": {"allow": ["Bash(make:*)"]}}),
        );
        write_settings(
            &repo.join(settings::CLAUDE_LOCAL),
            json!({"permissions": {"allow": ["Bash(npm test)"]}}),
        );
        let policy = repo_policy(&dir, &repo);
        policy.set_mode(Mode::Auto);
        write_settings(
            &repo.join(settings::LOCAL),
            json!({"permissions": {"allow": ["Bash(cargo fmt)"]}}),
        );
        write_settings(&repo.join(settings::CLAUDE_LOCAL), json!({}));

        let trusted = policy.trust().unwrap();
        assert!(trusted.starts_with("trusted 1 "), "{trusted}");
        assert_eq!(bash(&policy, "make all"), Decision::Ask);
        assert_eq!(bash(&policy, "npm test"), Decision::Ask);
        assert_eq!(bash(&policy, "cargo fmt"), allowed("rule Bash(cargo fmt)"));
        let described = policy.describe();
        assert!(!described.contains("Bash(make:*)"), "{described}");

        // Auto-trust on remember reloads too: the old rule does not come back.
        policy.untrust().unwrap();
        let fresh = repo_policy(&dir, &repo);
        write_settings(&repo.join(settings::LOCAL), json!({}));
        fresh.remember("Bash(ls)").unwrap();
        assert!(fresh.trusted());
        assert_eq!(bash(&fresh, "cargo fmt"), Decision::Ask);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remembering_an_approval_keeps_trust_as_it_was() {
        let dir = std::env::temp_dir().join(format!("bhai-policy-{}", uuid::Uuid::new_v4()));
        let repo = dir.join("repo");
        write_settings(
            &repo.join(settings::LOCAL),
            json!({"permissions": {"allow": ["Bash(make:*)"]}}),
        );
        let policy = repo_policy(&dir, &repo);
        policy.trust().unwrap();
        policy.remember("Bash(cargo test:*)").unwrap();
        assert!(repo_policy(&dir, &repo).trusted());

        // Untrusted with allow rules: the user's approval does not vouch for them.
        policy.untrust().unwrap();
        policy.remember("Bash(cargo fmt)").unwrap();
        assert!(!repo_policy(&dir, &repo).trusted());
        assert_eq!(bash(&policy, "cargo fmt"), allowed("rule Bash(cargo fmt)"));
        assert_eq!(bash(&policy, "make all"), Decision::Ask);
        // Approving a rule the repo also ships applies it as the user's own.
        policy.remember("Bash(make:*)").unwrap();
        assert!(matches!(bash(&policy, "make all"), Decision::Allow(_)));

        // Untrusted with no allow rules: the resulting file is the user's own.
        let fresh = dir.join("fresh");
        std::fs::create_dir_all(&fresh).unwrap();
        let policy = repo_policy(&dir, &fresh);
        assert!(!policy.trusted());
        assert_eq!(policy.trust_notice(), None);
        policy.remember("Bash(ls)").unwrap();
        assert!(policy.trusted());
        assert!(repo_policy(&dir, &fresh).trusted());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
