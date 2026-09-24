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

/// Why a call is the user's to approve and never the judge's. `auto` mode denies every
/// one of them without prompting, so which it was is what the agent is told: a denial
/// that names all of the causes at once is one the model can only answer by guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reserved {
    /// A path, or a word of a command, only the user may approve.
    Protected(String),
    /// An ask rule the user set, by its text.
    Asked(String),
    /// A command the tokenizer could not read, by the word that runs code in it.
    RunsCode(String),
    /// The judge does not run here at all: this is not `auto`, or the project is not
    /// trusted. Neither reaches the agent, since `auto` implies a trusted project and
    /// every other mode prompts.
    Untrusted,
}

impl Reserved {
    /// What the denial says it was.
    pub fn why(&self) -> String {
        match self {
            Reserved::Protected(what) => {
                format!("only the user may approve a call naming {what}")
            }
            Reserved::Asked(rule) => format!("the user's rule {rule} keeps this one for them"),
            Reserved::RunsCode(word) => {
                format!("the permission checker cannot read a command holding `{word}`")
            }
            Reserved::Untrusted => "this project is not trusted".to_string(),
        }
    }

    /// What the agent can do about it, if anything.
    pub fn how(&self) -> &'static str {
        match self {
            Reserved::RunsCode(_) => {
                " It reads that as running whatever follows it, so it cannot tell what the \
command would do. Without it, or split into separate commands, the same work may go through."
            }
            _ => "",
        }
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

/// What `auto` mode allows on its own in a project the trust store knows, from the
/// config. An untrusted project gets none of it, and cannot be in `auto` anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Relax {
    /// `auto_project_writes`: writes and edits inside the project root.
    pub writes: bool,
    /// `auto_project_commands`: the built-in build and test commands.
    pub commands: bool,
}

impl Default for Relax {
    fn default() -> Self {
        Self {
            writes: true,
            commands: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct Policy {
    mode: AtomicU8,
    /// The mode asked for by the config or `--mode`, which trust may not allow yet.
    wanted: AtomicU8,
    /// Allow rules grow as approvals are remembered.
    rules: RwLock<Rules>,
    home: Option<PathBuf>,
    cwd: PathBuf,
    /// Where remembered rules are saved; `None` keeps them for this session only.
    store: Option<PathBuf>,
    trust: Option<Trust>,
    /// Whether repo-supplied allow rules apply.
    trusted: AtomicBool,
    /// What `auto` mode relaxes once the project is trusted.
    relax: Relax,
}

impl Policy {
    pub fn new(mode: Mode, rules: Rules, home: Option<PathBuf>, cwd: PathBuf) -> Self {
        Self {
            mode: AtomicU8::new(mode as u8),
            wanted: AtomicU8::new(mode as u8),
            rules: RwLock::new(rules),
            home,
            cwd,
            store: None,
            trust: None,
            trusted: AtomicBool::new(false),
            relax: Relax::default(),
        }
    }

    pub fn with_store(self, store: PathBuf) -> Self {
        Self {
            store: Some(store),
            ..self
        }
    }

    pub fn with_relax(self, relax: Relax) -> Self {
        Self { relax, ..self }
    }

    pub fn with_trust(self, trust: Trust) -> Self {
        let snapshot = trust.snapshot();
        let trusted = trust.matches(&snapshot);
        let policy = Self {
            trust: Some(trust),
            ..self
        };
        if trusted {
            policy.set_trusted(&snapshot);
        }
        // An untrusted project cannot be in the mode the config asked for.
        policy.set_mode(policy.wanted());
        policy
    }

    pub fn mode(&self) -> Mode {
        Mode::ALL[self.mode.load(Ordering::Relaxed) as usize]
    }

    /// Set the mode, as far as trust allows. Returns the mode actually in force; the one
    /// asked for is remembered, so trusting the project later puts it there.
    pub fn set_mode(&self, mode: Mode) -> Mode {
        self.wanted.store(mode as u8, Ordering::Relaxed);
        let mode = match self.offers_mode(mode) {
            true => mode,
            false => Mode::Ask,
        };
        self.mode.store(mode as u8, Ordering::Relaxed);
        mode
    }

    /// The mode asked for, which is the one in force unless trust held it back.
    pub fn wanted(&self) -> Mode {
        Mode::ALL[self.wanted.load(Ordering::Relaxed) as usize]
    }

    /// The allow rules this project's own settings files ship, which trust would honour.
    pub fn repo_rules(&self) -> usize {
        self.trust
            .as_ref()
            .map_or(0, |trust| trust.snapshot().allow_rules().len())
    }

    /// The modes the user may choose right now. An untrusted project only has `ask`:
    /// `auto` and `bypass` run code the project supplies, which is the thing trust is
    /// about, so offering them before the question is answered would be theatre.
    pub fn modes(&self) -> &'static [Mode] {
        match self.trusted() {
            true => &Mode::ALL,
            false => &[Mode::Ask],
        }
    }

    pub fn offers_mode(&self, mode: Mode) -> bool {
        self.modes().contains(&mode)
    }

    /// The next mode the user may choose, wrapping round what `modes` offers.
    pub fn next_mode(&self) -> Mode {
        let modes = self.modes();
        let at = modes.iter().position(|m| *m == self.mode()).unwrap_or(0);
        modes[(at + 1) % modes.len()]
    }

    /// Decide a call to `tool`. Tools that skip approval only answer to deny and ask
    /// rules; their `Allow` carries no reason and needs no notice.
    pub fn check(&self, tool: &str, args: &Value, needs_approval: bool) -> Decision {
        let rules = self.rules();
        self.checker(&rules, self.mode())
            .check(tool, args, needs_approval)
    }

    /// Whether a call the rules left at `Ask` may go to the auto-approval judge, and
    /// what keeps it from the judge when it may not. Only in `auto` mode in a trusted
    /// project, and never for what the user must decide themselves: a protected path, an
    /// ask rule they set, or a command the tokenizer refuses, which is how `sudo` and
    /// everything it cannot read are kept out. `auto` never prompts, so in that mode
    /// those are denied rather than asked.
    ///
    /// Where the call would land is not one of those. The judge rules on that, against
    /// what the user asked for: a shell redirect anywhere on the machine already reached
    /// it, so keeping the `write` that does the same thing from it decided nothing and
    /// left a task about the machine with no way through at all.
    pub fn judgeable(&self, tool: &str, args: &Value) -> Result<(), Reserved> {
        let rules = self.rules();
        self.checker(&rules, self.mode()).judgeable(tool, args)
    }

    /// See `written`, from the project root.
    pub fn written(&self, tool: &str, args: &Value) -> Vec<PathBuf> {
        written(tool, args, &self.cwd, self.home.as_deref())
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
            let keep = self.trust.as_ref().filter(|t| {
                let snapshot = t.snapshot();
                t.matches(&snapshot) || snapshot.allow_rules().is_empty()
            });
            settings::remember(store, text)?;
            if let Some(trust) = keep {
                self.set_trusted(&trust.trust()?);
            }
        }
        Ok(self.store.as_deref())
    }

    /// Honour the repo-supplied allow rules as the files are now, for `/trust`.
    pub fn trust(&self) -> anyhow::Result<String> {
        let trust = self.trust.as_ref().context("no trust store")?;
        let snapshot = trust.trust()?;
        self.set_trusted(&snapshot);
        // The mode the config asked for was held back while this was untrusted.
        self.set_mode(self.wanted());
        let rules = snapshot.allow_rules();
        let mut out = match rules.len() {
            0 => format!("trusted this project; mode: {}", self.mode()),
            n => format!(
                "trusted this project and the {n} allow rules it ships; mode: {}",
                self.mode()
            ),
        };
        for (source, rule) in rules {
            out.push_str(&format!("\n  {rule}  ({source})"));
        }
        Ok(out)
    }

    /// Stop honouring the repo-supplied allow rules, for `/untrust`.
    pub fn untrust(&self) -> anyhow::Result<String> {
        let trust = self.trust.as_ref().context("no trust store")?;
        self.trusted.store(false, Ordering::Relaxed);
        self.set_mode(self.wanted());
        Ok(match trust.untrust()? {
            true => "untrusted this project's allow rules".to_string(),
            false => "this project was not trusted".to_string(),
        })
    }

    /// The startup notice when the repo ships allow rules that are not trusted.
    pub fn trust_notice(&self) -> Option<String> {
        let trust = self.trust.as_ref()?;
        let count = trust.snapshot().allow_rules().len();
        (count > 0 && !self.trusted())
            .then(|| format!("this repo ships {count} allow rules; /trust to honour them"))
    }

    /// Honour the repo-supplied allow rules, replacing the loaded ones with those in
    /// `snapshot`, so the rules that apply are the ones trusted.
    fn set_trusted(&self, snapshot: &trust::Snapshot) {
        let fresh = snapshot.load_allow();
        let mut rules = self.rules.write().unwrap_or_else(|e| e.into_inner());
        rules.allow.retain(|r| !r.repo);
        let fresh: Vec<Rule> = fresh
            .into_iter()
            .filter(|f| !rules.allow.iter().any(|r| r.text == f.text))
            .collect();
        rules.allow.extend(fresh);
        self.trusted.store(true, Ordering::Relaxed);
    }

    /// Whether this project's own files are trusted. With no trust store there is
    /// nowhere to record an answer and no repo-supplied rules to honour, so the question
    /// does not arise and nothing is held back.
    pub fn trusted(&self) -> bool {
        self.trust.is_none() || self.trusted.load(Ordering::Relaxed)
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
        if mode == Mode::Auto {
            let on: Vec<&str> = [
                (
                    self.relax.writes,
                    "writes and edits inside the project root",
                ),
                (self.relax.commands, "build and test commands"),
            ]
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, what)| *what)
            .collect();
            out.push_str(&match (on.is_empty(), trusted) {
                (true, _) => "\nno relaxations: both are off in the config".to_string(),
                (false, true) => format!(
                    "\nallowed with no rule, this project being trusted: {}",
                    on.join(", ")
                ),
                (false, false) => format!(
                    "\nwould run with no rule if you trust this project: {}",
                    on.join(", ")
                ),
            });
            out.push_str("\nanything else the rules leave open goes to the judge");
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
            relax: self.relax,
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

/// The files a call would change, resolved against `cwd`, for the judge to be told where
/// each one lands. A bash chain is followed through its `cd`s, wherever they go; a path
/// that cannot be resolved without the shell, such as `~user/x`, is left out, and so is
/// everything in a command the tokenizer cannot read.
pub fn written(tool: &str, args: &Value, cwd: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let text = |key| args.get(key).and_then(Value::as_str);
    let resolve = |cwd: &Path, target: &str| match target.strip_prefix("~/") {
        Some(rest) => home.map(|home| home.join(rest)),
        None if target.starts_with('~') => None,
        None => Some(cwd.join(target)),
    };
    match (tool, text("path"), text("command")) {
        ("write" | "edit", Some(path), _) => resolve(cwd, path).into_iter().collect(),
        ("bash", _, Some(command)) => {
            let mut cwd = cwd.to_path_buf();
            let mut paths = Vec::new();
            for c in bash::parse(command).unwrap_or_default() {
                if !c.piped
                    && let Some(target) = bash::cd_target(&c.words)
                {
                    cwd = cwd.join(target);
                    continue;
                }
                let named = bash::written_args(c.unwrapped());
                for target in c.writes.iter().map(String::as_str).chain(named) {
                    paths.extend(resolve(&cwd, target));
                }
            }
            paths
        }
        _ => Vec::new(),
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
    relax: Relax,
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
        let allowed = find(&self.allow(), false)
            .map(|r| rule_reason(&r))
            .or_else(|| self.project_write(tool, path).then(project_reason));
        self.fallback(allowed)
    }

    fn check_bash(&self, command: &str) -> Decision {
        let any = |rules: &[Rule]| {
            rules
                .iter()
                .find(|r| r.applies_to("bash") && r.is_any())
                .cloned()
        };
        let Some(commands) = bash::parse(command) else {
            // Unparseable, so it is never allowed. A rule for every command decides it,
            // and so does a worded one over the raw text: the words the user denied are
            // still in there, whatever shape the tokenizer could not read.
            return match any(&self.rules.deny).or_else(|| self.find_raw(&self.rules.deny, command))
            {
                Some(rule) => denied(&rule),
                None => Decision::Ask,
            };
        };
        for c in &commands {
            if let Some(rule) = self.find_loose(&self.rules.deny, c) {
                return denied(&rule);
            }
        }
        if commands
            .iter()
            .any(|c| self.find_loose(&self.rules.ask, c).is_some() || bash::mentions_protected(c))
        {
            return Decision::Ask;
        }

        let mut reasons: Vec<String> = Vec::new();
        // The rest of the chain runs wherever its `cd`s have left it.
        let mut cwd = self.base.cwd.to_path_buf();
        for c in &commands {
            // A `cd` in a pipeline moves only its own subshell, so it says nothing
            // about where the rest of the chain runs.
            if self.mode != Mode::Ask
                && !c.piped
                && let Some(target) = bash::cd_target(&c.words)
            {
                // A `cd` out of the project decides nothing about what follows it.
                let Some(next) = self.cd_into(&cwd, target) else {
                    return self.fallback(None);
                };
                cwd = next;
                let reason = "cd inside the project".to_string();
                if !reasons.contains(&reason) {
                    reasons.push(reason);
                }
                continue;
            }
            // A redirect writes a file, whatever the program on its left does.
            let read_only =
                self.mode != Mode::Ask && c.writes.is_empty() && bash::is_read_only(&c.words);
            let reason = self
                .allow()
                .iter()
                .find(|r| r.applies_to("bash") && r.matches_words(&c.words, false))
                .filter(|_| self.redirects_allowed(c, &cwd))
                .map(rule_reason)
                .or_else(|| read_only.then(|| "read-only".to_string()))
                .or_else(|| self.project_command(c, &cwd).then(project_reason));
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

    /// Deny and ask rules see the program behind wrappers and paths, and behind a
    /// `find -exec`, in any case.
    fn find_loose(&self, rules: &[Rule], c: &bash::Command) -> Option<Rule> {
        let forms = loose_forms(c);
        rules
            .iter()
            .find(|r| r.applies_to("bash") && forms.iter().any(|w| r.matches_words(w, true)))
            .cloned()
    }

    /// A worded rule over the raw text of a command the tokenizer refused. Every suffix
    /// is tried, since nothing here knows where the program starts; matching too much is
    /// safe where the answer can only be deny or ask.
    fn find_raw(&self, rules: &[Rule], command: &str) -> Option<Rule> {
        let words = bash::raw_words(command);
        rules
            .iter()
            .find(|r| {
                r.applies_to("bash") && (0..words.len()).any(|i| r.matches_words(&words[i..], true))
            })
            .cloned()
    }

    /// See `Policy::judgeable`.
    fn judgeable(&self, tool: &str, args: &Value) -> Result<(), Reserved> {
        if !self.relaxed() {
            return Err(Reserved::Untrusted);
        }
        let text = |key| args.get(key).and_then(Value::as_str);
        // An ask rule is the user saying they want to see this one. The judge waving it
        // through would leave the rule tightening nothing in the mode that needs it.
        let asked = |path: &Path| {
            self.rules
                .ask
                .iter()
                .find(|r| r.applies_to(tool) && r.matches_path(path, self.base, true))
                .map(|r| Reserved::Asked(r.text.clone()))
        };
        match tool {
            "bash" => {
                let command = text("command").unwrap_or_default();
                match bash::parse(command) {
                    Some(commands) => commands.iter().try_for_each(|c| {
                        if let Some(word) = bash::protected_mention(c) {
                            return Err(Reserved::Protected(word));
                        }
                        match self.find_loose(&self.rules.ask, c) {
                            Some(rule) => Err(Reserved::Asked(rule.text)),
                            None => Ok(()),
                        }
                    }),
                    // A command the tokenizer could not take apart is still the judge's
                    // to rule on: it reads the text as written, and a variable or a
                    // substitution is most of what the tokenizer refuses. What it must
                    // not be handed is a command that runs its arguments as shell code
                    // or as someone else, or one naming a protected path, since the
                    // shape those hide behind is the reason they are the user's alone.
                    None => {
                        if let Some(rule) = self.find_raw(&self.rules.ask, command) {
                            return Err(Reserved::Asked(rule.text));
                        }
                        match bash::reserved(command) {
                            Some(bash::Reserved::Program(word)) => Err(Reserved::RunsCode(word)),
                            Some(bash::Reserved::Path(word)) => Err(Reserved::Protected(word)),
                            None => Ok(()),
                        }
                    }
                }
            }
            // A protected path is the user's alone. Everywhere else, including the rest
            // of the machine, is the judge's to rule on against what the user asked for.
            "read" | "write" | "edit" => match text("path") {
                Some(path) => {
                    let path = Path::new(path);
                    match rules::is_protected(path, self.base.home) {
                        true => Err(Reserved::Protected(path.display().to_string())),
                        false => asked(path).map_or(Ok(()), Err),
                    }
                }
                None => Err(Reserved::Protected("no path".to_string())),
            },
            // An MCP server the user has not approved is never connected, so an
            // `mcp_call` that gets this far names one they did approve.
            _ => match self.rules.ask.iter().find(|r| r.applies_to(tool)) {
                Some(rule) => Err(Reserved::Asked(rule.text.clone())),
                None => Ok(()),
            },
        }
    }

    /// Whether `auto` mode's relaxations apply. They run the project's own code, so they
    /// wait on trust; an untrusted project cannot be in `auto` in the first place, and
    /// this keeps that true even if something sets the mode behind the policy's back.
    fn relaxed(&self) -> bool {
        self.mode == Mode::Auto && self.trusted
    }

    /// A write or edit whose target really is inside the project root.
    fn project_write(&self, tool: &str, path: &Path) -> bool {
        self.relaxed()
            && self.relax.writes
            && matches!(tool, "write" | "edit")
            && rules::is_inside(path, self.base.cwd)
    }

    /// Where a `cd` leaves the chain, or `None` when it leaves the project root.
    fn cd_into(&self, cwd: &Path, target: &str) -> Option<PathBuf> {
        let next = cwd.join(target);
        (next.is_dir() && rules::is_inside(&next, self.base.cwd)).then_some(next)
    }

    /// A build or test command the project runs on itself, from `cwd`.
    fn project_command(&self, command: &bash::Command, cwd: &Path) -> bool {
        let inside = |arg: &str| rules::is_inside(&cwd.join(arg), self.base.cwd);
        self.relaxed()
            && self.relax.commands
            && command.nested().is_empty()
            && bash::is_project_command(&command.words, &inside)
            && self.redirects_inside(command, cwd)
    }

    /// Whether every file the command redirects into is one a write would be allowed to
    /// make: a redirect is a write, so `auto` relaxes it on the same terms. A command
    /// that writes nowhere passes.
    fn redirects_inside(&self, command: &bash::Command, cwd: &Path) -> bool {
        command.writes.iter().all(|target| {
            self.write_target(cwd, target)
                .is_some_and(|path| self.project_write("write", &path))
        })
    }

    /// Whether every file the command redirects into is one the rules allow writing. An
    /// allow rule names a program, not the files a redirect points its output at, so the
    /// targets are checked as writes in their own right and a deny, an ask or a
    /// protected path still stops them.
    fn redirects_allowed(&self, command: &bash::Command, cwd: &Path) -> bool {
        command.writes.iter().all(|target| {
            self.write_target(cwd, target).is_some_and(|path| {
                matches!(self.check_path("write", &path, true), Decision::Allow(_))
            })
        })
    }

    /// The file a redirect target names. A leading `~/` is the home directory; `Path::join`
    /// would take it for a relative component and land it inside the project. Any other
    /// `~` form is an expansion nothing here can resolve, so there is no path to check
    /// and the redirect fails closed.
    fn write_target(&self, cwd: &Path, target: &str) -> Option<PathBuf> {
        match target.strip_prefix("~/") {
            Some(rest) => Some(self.base.home?.join(rest)),
            None if target.starts_with('~') => None,
            None => Some(cwd.join(target)),
        }
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
/// wrapper, every later word may be the program, since wrapper flags can take values, and a
/// command run as an argument (`find ... -exec git push \;`) is added the same way.
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
    for nested in command.nested() {
        forms.extend(loose_forms(&nested));
    }
    forms
}

fn project_reason() -> String {
    "auto, inside the project".to_string()
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

    /// An allow rule names a program. It says nothing about where a redirect points
    /// that program's output, so the target is checked as the write it is.
    #[test]
    fn an_allow_rule_does_not_carry_a_redirect_with_it() {
        let allow = Rules {
            allow: rules(&["Bash(cargo test)", "Bash(git log:*)"])
                .into_iter()
                .map(Rule::by_user)
                .collect(),
            ..Rules::default()
        };
        let p = Policy::new(
            Mode::Ask,
            allow,
            Some("/home/u".into()),
            "/home/u/repo".into(),
        );
        assert_eq!(bash(&p, "cargo test"), allowed("rule Bash(cargo test)"));
        assert_eq!(bash(&p, "git log -p"), allowed("rule Bash(git log:*)"));
        assert_eq!(bash(&p, "cargo test > ~/.zshrc"), Decision::Ask);
        assert_eq!(bash(&p, "cargo test > /etc/cron.d/evil"), Decision::Ask);
        assert_eq!(bash(&p, "git log > ~/.profile"), Decision::Ask);
        // The pattern would carry only the words, so there is no exact rule to offer.
        assert!(rules::exact_command("cargo test > /etc/cron.d/evil").is_none());
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
        // With nothing to trust, the cycle is the whole set.
        let cycle = |mode: Mode| {
            let policy = Policy::default();
            policy.set_mode(mode);
            policy.next_mode()
        };
        assert_eq!(cycle(Mode::Ask), Mode::Auto);
        assert_eq!(cycle(Mode::Auto), Mode::Bypass);
        assert_eq!(cycle(Mode::Bypass), Mode::Ask);
        assert_eq!("bypass".parse(), Ok(Mode::Bypass));
        assert!("yolo".parse::<Mode>().is_err());
        assert_eq!(serde_json::to_value(Mode::Auto).unwrap(), json!("auto"));
        let policy = Policy::default();
        policy.set_mode(policy.next_mode());
        assert_eq!(policy.mode(), Mode::Auto);
    }

    #[test]
    fn written_follows_the_chain_to_every_file_it_changes() {
        let (cwd, home) = (Path::new("/p"), Path::new("/h"));
        let of = |command: &str| {
            written("bash", &json!({ "command": command }), cwd, Some(home))
                .into_iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            of("cd sub && sort a > b 2> err && timeout 5 rm -rf -- -x old"),
            ["/p/sub/b", "/p/sub/err", "/p/sub/-x", "/p/sub/old"]
        );
        assert_eq!(
            of("cp -r a b ~/dst && mv x y && chmod 644 f"),
            ["/h/dst", "/p/x", "/p/y", "/p/f"]
        );
        // A piped `cd` moves only its subshell; `~user` needs the shell; `$x` is unread.
        assert_eq!(of("cd sub | cat && touch f ~bob/g"), ["/p/f"]);
        assert!(of("touch $x").is_empty());
        assert!(of("git status && ls > /dev/null").is_empty());
        assert_eq!(
            written("edit", &json!({ "path": "src/a.rs" }), cwd, None),
            [PathBuf::from("/p/src/a.rs")]
        );
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
            (
                &auto,
                "git log | npm test",
                allowed("rule Bash(git log:*), auto, inside the project"),
            ),
            (&auto, "curl example.com", Decision::Ask),
            (&auto, "git status; rm -rf ~", deny_rm.clone()),
            (&auto, "timeout 5 /bin/RM x", deny_rm.clone()),
            (&bypass, "env -u FOO rm x", deny_rm.clone()),
            (&bypass, "timeout -s KILL 5 rm x", deny_rm.clone()),
            (&bypass, "xargs -I X rm X", deny_rm.clone()),
            (&bypass, r"find . -name x -exec rm {} \;", deny_rm.clone()),
            (
                &auto,
                "git push origin main",
                Decision::Deny("deny rule Bash(git push:*)".to_string()),
            ),
            // Ask beats allow.
            (&auto, "git log -p", Decision::Ask),
            (&auto, "cat .env", Decision::Ask),
            // A shape the tokenizer cannot read still names what the rule denies.
            (&auto, "ls $(rm x)", deny_rm.clone()),
            (&auto, "rm -rf $DIR", deny_rm.clone()),
            (&auto, "cargo test", allowed("auto, inside the project")),
            (&ask_mode, "ls", Decision::Ask),
            (&ask_mode, "rm x", deny_rm.clone()),
            (&bypass, "npm test", allowed("bypass mode")),
            (&bypass, "ls", allowed("read-only")),
            (&bypass, "rm x", deny_rm),
            (&bypass, "cat ~/.ssh/id_rsa", Decision::Ask),
            // A redirect is a write, so the program on its left is not read-only.
            (&bypass, "echo x > out", allowed("bypass mode")),
            (&auto, "echo x > out", Decision::Ask),
            (&auto, "echo x > ~/.ssh/authorized_keys", Decision::Ask),
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
    fn nested_commands_only_add_deny_and_ask() {
        let rule = "Bash(git push:*)";
        let deny_policy = policy(Mode::Bypass, &[], &[rule], &[]);
        let deny = Decision::Deny(format!("deny rule {rule}"));
        for command in [
            r"find . -name x -exec git push \;",
            r"find . -execdir /usr/bin/git push origin main \;",
            "xargs -n1 git push",
            "xargs -n1 -0 git push",
            // A shell keyword is an ordinary word to the tokenizer, so without it in
            // WRAPPERS no form of the command starts at the program the rule names.
            "if true; then git push origin main; fi",
            "while true; do git push; done",
            "if false; then ls; else git push; fi",
        ] {
            assert_eq!(bash(&deny_policy, command), deny, "{command}");
        }
        let ask_policy = policy(Mode::Bypass, &[], &[], &[rule]);
        assert_eq!(
            bash(&ask_policy, r"find . -exec git push \;"),
            Decision::Ask
        );
        // The allow side only ever sees the command as written.
        let allow = policy(Mode::Auto, &["Bash(git push:*)"], &[], &[]);
        assert_eq!(bash(&allow, r"find . -exec git push \;"), Decision::Ask);
        assert_eq!(bash(&allow, "xargs -n1 git push"), Decision::Ask);
        // A refused program behind `-exec` is not granted by an allow rule on `find`.
        let allow = policy(Mode::Auto, &["Bash(find:*)"], &[], &[]);
        assert!(matches!(bash(&allow, "find . -name x"), Decision::Allow(_)));
        assert_eq!(
            bash(&allow, r"find . -exec sudo rm -rf / \;"),
            Decision::Ask
        );
    }

    #[test]
    fn a_bare_deny_covers_unparseable_commands() {
        let bare = policy(Mode::Bypass, &[], &["Bash"], &[]);
        assert_eq!(
            bash(&bare, "ls $(x)"),
            Decision::Deny("deny rule Bash".to_string())
        );
        // A worded rule covers one too: the tokenizer refused the shape, not the words.
        let worded = policy(Mode::Bypass, &[], &["Bash(rm:*)", "Bash(curl:*)"], &[]);
        assert_eq!(
            bash(&worded, "rm -rf $DIR"),
            Decision::Deny("deny rule Bash(rm:*)".to_string())
        );
        assert_eq!(
            bash(&worded, "curl $URL"),
            Decision::Deny("deny rule Bash(curl:*)".to_string())
        );
        assert_eq!(bash(&worded, "ls $(x)"), Decision::Ask);
    }

    #[test]
    fn wildcard_rules_match_each_command_of_a_chain() {
        let rule = "Bash(git -C * push origin main)";
        let deny_policy = policy(Mode::Bypass, &["Bash(echo:*)"], &[rule], &[]);
        let deny = Decision::Deny(format!("deny rule {rule}"));
        for command in [
            "echo hi && git -C x push origin main",
            "echo hi; /usr/bin/GIT -C x push origin main",
            "timeout 5 git -C x push origin main | cat",
        ] {
            assert_eq!(bash(&deny_policy, command), deny, "{command}");
        }
        let allow = policy(Mode::Auto, &["Bash(git -C * status)"], &[], &[]);
        assert_eq!(
            bash(&allow, "git -C x status"),
            allowed("rule Bash(git -C * status)")
        );
        assert_eq!(bash(&allow, "git -C x status && rm y"), Decision::Ask);
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
        // Relaxations off, so the call this remembers a rule for is one that asks.
        let policy = Policy::new(Mode::Auto, Rules::default(), None, dir.clone())
            .with_store(store.clone())
            .with_relax(Relax {
                writes: false,
                commands: false,
            });
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

    #[test]
    fn a_trusted_project_relaxes_auto_mode() {
        let dir = std::env::temp_dir().join(format!("bhai-policy-{}", uuid::Uuid::new_v4()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("outside"), repo.join("away")).unwrap();
        let policy = |deny: &[&str]| {
            let deny = Rules {
                deny: rules(deny),
                ..Rules::default()
            };
            Policy::new(Mode::Auto, deny, None, repo.clone())
                .with_trust(Trust::new(&dir.join("config"), &repo))
        };
        let inside = repo.join("src/a.rs");
        let inside = inside.to_str().unwrap();

        // These run the project's own code, so an untrusted checkout gains nothing, and
        // cannot even be in `auto`: the mode falls back until the question is answered.
        let untrusted = policy(&[]);
        assert_eq!(untrusted.mode(), Mode::Ask);
        assert_eq!(untrusted.modes(), [Mode::Ask]);
        assert_eq!(untrusted.set_mode(Mode::Auto), Mode::Ask);
        assert_eq!(untrusted.next_mode(), Mode::Ask);
        assert_eq!(bash(&untrusted, "cargo test"), Decision::Ask);
        assert_eq!(file(&untrusted, "write", inside), Decision::Ask);

        // Trusting it puts the project in the mode the config asked for.
        let p = policy(&[]);
        p.trust().unwrap();
        assert_eq!(p.mode(), Mode::Auto);
        assert_eq!(p.modes(), Mode::ALL);
        assert_eq!(
            file(&p, "write", inside),
            allowed("auto, inside the project")
        );
        assert_eq!(bash(&p, "cargo test"), allowed("auto, inside the project"));
        assert_eq!(
            bash(&p, "cargo test && npm run build"),
            allowed("auto, inside the project")
        );
        // `sudo` and anything else the tokenizer refuses never reaches the relaxation.
        assert_eq!(bash(&p, "sudo cargo test"), Decision::Ask);
        assert_eq!(bash(&p, "curl example.com"), Decision::Ask);
        // A redirect is a write, so it is relaxed on a write's terms: inside the
        // project yes, anywhere else no, whatever the command on its left is.
        assert_eq!(
            bash(&p, "cargo test > out.txt"),
            allowed("auto, inside the project")
        );
        assert_eq!(bash(&p, "cargo test > /tmp/out.txt"), Decision::Ask);
        assert_eq!(bash(&p, "cargo test > ../out.txt"), Decision::Ask);
        assert_eq!(bash(&p, "cargo test > .env"), Decision::Ask);
        // `~` is a shell expansion, not a directory of that name in the project.
        assert_eq!(bash(&p, "cargo test > ~/x"), Decision::Ask);
        assert_eq!(bash(&p, "cargo test > ~/.zshrc"), Decision::Ask);
        assert_eq!(bash(&p, "cargo test > ~x/y"), Decision::Ask);
        // A symlink out of the project, a path outside it and a protected path still ask.
        let away = repo.join("away/x.rs");
        assert_eq!(file(&p, "edit", away.to_str().unwrap()), Decision::Ask);
        let outside = dir.join("outside/x.rs");
        assert_eq!(file(&p, "write", outside.to_str().unwrap()), Decision::Ask);
        let env = repo.join(".env");
        assert_eq!(file(&p, "write", env.to_str().unwrap()), Decision::Ask);

        // Only in `auto`, and only what the config leaves on.
        p.set_mode(Mode::Ask);
        assert_eq!(file(&p, "write", inside), Decision::Ask);
        assert_eq!(bash(&p, "cargo test"), Decision::Ask);
        p.set_mode(Mode::Auto);
        let off = policy(&[]).with_relax(Relax {
            writes: false,
            commands: false,
        });
        off.trust().unwrap();
        assert_eq!(file(&off, "write", inside), Decision::Ask);
        assert_eq!(bash(&off, "cargo test"), Decision::Ask);

        // Deny rules win over the relaxation.
        let denied = policy(&["Bash(cargo test:*)", "Edit(/src/**)"]);
        denied.trust().unwrap();
        denied.trust().unwrap();
        assert_eq!(
            bash(&denied, "cargo test"),
            Decision::Deny("deny rule Bash(cargo test:*)".to_string())
        );
        assert_eq!(
            file(&denied, "write", inside),
            Decision::Deny("deny rule Edit(/src/**)".to_string())
        );

        let described = p.describe();
        assert!(
            described.contains("this project being trusted"),
            "{described}"
        );
        assert!(described.contains("goes to the judge"), "{described}");
        let untrusted = untrusted.describe();
        assert!(untrusted.starts_with("permission mode: ask"), "{untrusted}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_auto_in_a_trusted_project_reaches_the_judge() {
        let dir = std::env::temp_dir().join(format!("bhai-judgeable-{}", uuid::Uuid::new_v4()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        let p = Policy::new(Mode::Auto, Rules::default(), None, repo.clone())
            .with_trust(Trust::new(&dir.join("config"), &repo));
        let command = |c: &str| p.judgeable("bash", &json!({ "command": c }));
        let path = |tool: &str, path: &PathBuf| p.judgeable(tool, &json!({ "path": path }));

        // An untrusted project never reaches it, however harmless the call.
        assert_eq!(command("cargo clippy"), Err(Reserved::Untrusted));
        p.trust().unwrap();
        assert!(command("cargo clippy").is_ok());
        assert!(path("write", &repo.join("src/a.rs")).is_ok());

        // The categories that always reach the user instead, each naming what it was.
        assert_eq!(
            command("sudo cargo clippy"),
            Err(Reserved::RunsCode("sudo".to_string()))
        );
        assert_eq!(
            command("cat .env"),
            Err(Reserved::Protected(".env".to_string()))
        );
        assert_eq!(
            path("edit", &repo.join(".env")),
            Err(Reserved::Protected(repo.join(".env").display().to_string()))
        );
        assert!(p.judgeable("write", &json!({})).is_err(), "no path");

        // Where the call lands is the judge's to rule on, not a category of its own:
        // the redirect that writes the same file already reached it.
        assert!(
            path("write", &PathBuf::from("/etc/hosts")).is_ok(),
            "outside"
        );
        assert!(
            command("echo x > /etc/hosts").is_ok(),
            "the same by redirect"
        );

        // A command the tokenizer cannot take apart is the judge's, unless its text
        // names one of those categories, which is all that can be read off it.
        assert!(
            command("cat $HOME/.cargo/config.toml").is_ok(),
            "an expansion"
        );
        assert!(command("cd $(git rev-parse --show-toplevel) && cargo build").is_ok());
        assert!(
            command("for f in src/*.rs; do wc -l $f; done").is_ok(),
            "a loop"
        );
        assert_eq!(
            command("ls $(sudo rm x)"),
            Err(Reserved::RunsCode("sudo".to_string())),
            "sudo behind a substitution"
        );
        assert_eq!(
            command("sh -c \"$SCRIPT\""),
            Err(Reserved::RunsCode("sh".to_string())),
            "a shell reading its argument"
        );
        assert!(
            matches!(
                command("cat $HOME/.ssh/id_ed25519"),
                Err(Reserved::Protected(_))
            ),
            "a protected path"
        );
        assert!(
            matches!(command("cat $HOME/.e*"), Err(Reserved::Protected(_))),
            "a glob that may reach one"
        );
        // A probe of what is installed, which `command` in the refused list used to take
        // the whole chain down with.
        assert!(command("command -v gpg || true").is_ok(), "a lookup");

        // An ask rule is the user saying they want to see it, so the judge does not get
        // to wave it through in the one mode where nothing else would stop it.
        let asking = Policy::new(
            Mode::Auto,
            Rules {
                ask: rules(&[
                    "Bash(git log -p:*)",
                    "Bash(cargo publish:*)",
                    "Write(gen/**)",
                ]),
                ..Rules::default()
            },
            None,
            repo.clone(),
        )
        .with_trust(Trust::new(&dir.join("config"), &repo));
        asking.trust().unwrap();
        let asked = |rule: &str| Err(Reserved::Asked(rule.to_string()));
        assert!(
            asking
                .judgeable("bash", &json!({"command": "cargo clippy"}))
                .is_ok()
        );
        assert_eq!(
            asking.judgeable("bash", &json!({"command": "git log -p"})),
            asked("Bash(git log -p:*)")
        );
        assert_eq!(
            asking.judgeable("bash", &json!({"command": "cargo publish"})),
            asked("Bash(cargo publish:*)")
        );
        assert_eq!(
            asking.judgeable("bash", &json!({"command": "cargo publish $CRATE"})),
            asked("Bash(cargo publish:*)")
        );
        assert_eq!(
            asking.judgeable("write", &json!({"path": repo.join("gen/x.rs")})),
            asked("Write(gen/**)")
        );
        assert!(
            asking
                .judgeable("write", &json!({"path": repo.join("src/x.rs")}))
                .is_ok()
        );

        // What the judge is there to rule on: a scratch file, and reading outside.
        assert!(path("write", &dir.join("outside/x.rs")).is_ok(), "scratch");
        assert!(
            path("write", &PathBuf::from("/tmp/notes.json")).is_ok(),
            "scratch"
        );
        assert!(
            command("cargo test > /tmp/out.txt").is_ok(),
            "a scratch redirect"
        );
        assert!(
            path("read", &PathBuf::from("/etc/hosts")).is_ok(),
            "read outside"
        );
        assert!(
            matches!(
                path("read", &repo.join(".env")),
                Err(Reserved::Protected(_))
            ),
            "read protected"
        );

        // Neither other mode ever asks the judge, trusted or not.
        for mode in [Mode::Ask, Mode::Bypass] {
            p.set_mode(mode);
            assert_eq!(command("cargo clippy"), Err(Reserved::Untrusted), "{mode}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_cd_inside_the_project_decides_the_rest_of_the_chain() {
        let dir = std::env::temp_dir().join(format!("bhai-policy-{}", uuid::Uuid::new_v4()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join("wordcount")).unwrap();
        std::fs::create_dir_all(repo.join("ripgrep")).unwrap();
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        let policy = |deny: &[&str]| {
            let deny = Rules {
                deny: rules(deny),
                ..Rules::default()
            };
            let p = Policy::new(Mode::Auto, deny, None, repo.clone())
                .with_trust(Trust::new(&dir.join("config"), &repo));
            p.trust().unwrap();
            p
        };
        let p = policy(&[]);
        let root = repo.display().to_string();

        // The bash prompts left in the field run, with the run directory substituted.
        let project = allowed("cd inside the project, auto, inside the project");
        for command in [
            format!("cd {root} && python -m pytest -q"),
            format!("cd {root} && python3 -m pytest -q"),
            format!("cd {root}/wordcount && cargo test"),
            "cd wordcount && cd .. && cargo build".to_string(),
            "cd wordcount && python3 run.py".to_string(),
        ] {
            assert_eq!(bash(&p, &command), project, "{command}");
        }
        let read_only = allowed("cd inside the project, read-only");
        for command in [
            format!("cd {root}/ripgrep && find . -type f -name '*.rs' | wc -l"),
            format!("cd {root}/ripgrep && ls -la"),
            format!("cd {root}/ripgrep && git ls-files '*.rs' | wc -l"),
        ] {
            assert_eq!(bash(&p, &command), read_only, "{command}");
        }
        assert_eq!(
            bash(&p, &format!("cargo new {root}/wordcount --bin")),
            allowed("auto, inside the project")
        );

        // A `cd` this cannot follow, or one that leaves the project, decides nothing.
        for command in [
            r#"python3 -c "from test_calc import test_add; test_add()""#.to_string(),
            "cd /etc && rm -rf x".to_string(),
            "cd ~ && cargo test".to_string(),
            "cd - && cargo test".to_string(),
            "cd && cargo test".to_string(),
            "cd wordcount && python3 -m pip install x".to_string(),
            // The `cd` runs in a subshell of its own, so `run.py` is outside the project.
            "cd wordcount | python3 ../run.py".to_string(),
            "cd missing && cargo test".to_string(),
            format!("cd {root}/../outside && ls"),
            format!("cargo new {}/x --bin", dir.join("outside").display()),
        ] {
            assert_eq!(bash(&p, &command), Decision::Ask, "{command}");
        }

        // Deny rules win inside a chain, and `ask` mode gains none of this.
        let denied = policy(&["Bash(cargo test:*)"]);
        assert_eq!(
            bash(&denied, "cd wordcount && cargo test"),
            Decision::Deny("deny rule Bash(cargo test:*)".to_string())
        );
        p.set_mode(Mode::Ask);
        assert_eq!(
            bash(&p, &format!("cd {root}/ripgrep && ls -la")),
            Decision::Ask
        );
        assert_eq!(bash(&p, "cd wordcount && cargo test"), Decision::Ask);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A policy for `repo` as `main` builds it, with its trust store under `dir`.
    fn repo_policy(dir: &Path, repo: &Path) -> Policy {
        let store = repo.join(settings::LOCAL);
        let (mut rules, _) = settings::claude(None, repo);
        rules.allow.extend(settings::load_local(&store).0);
        // Relaxations off, so these tests see what the rules alone decide.
        Policy::new(Mode::Ask, rules, None, repo.to_path_buf())
            .with_store(store)
            .with_relax(Relax {
                writes: false,
                commands: false,
            })
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
        assert!(trusted.contains("the 1 allow rules it ships"), "{trusted}");
        assert!(trusted.contains("mode: auto"), "{trusted}");
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
