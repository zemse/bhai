//! Permission modes and the policy that decides, before the approval prompt, whether a
//! tool call runs, is rejected, or needs the user.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{RwLock, RwLockReadGuard};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub mod bash;
pub mod rules;
pub mod settings;
pub mod trust;

use crate::tools::{fetch, image_gen, patch, schedule, ssrf, view_image};
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
    /// Input typed into a session that is not a plain shell, or that holds keys which
    /// edit the line, by the session's command: nothing here can tell what it runs.
    Typed(String),
    /// A schedule the model sets, which starts a turn later with nobody watching.
    Scheduled,
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
            Reserved::Typed(command) => {
                format!("only the user may approve what is typed into `{command}`")
            }
            Reserved::Scheduled => {
                "only the user may approve a schedule, which starts a turn later with nobody watching".to_string()
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
    /// Where every decision is appended, if anywhere.
    log: Option<PathBuf>,
    /// Whether anything can answer an approval. A `--workflow` run answers every one of
    /// them with no, so a call left at `Ask` is refused for good rather than put to
    /// someone, and what the agent is told to do about it differs.
    attended: bool,
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
            log: None,
            attended: true,
        }
    }

    /// Nothing is there to answer an approval: every call the rules leave at `Ask` is
    /// refused, and no asking will change it.
    pub fn unattended(self) -> Self {
        Self {
            attended: false,
            ..self
        }
    }

    pub fn attended(&self) -> bool {
        self.attended
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

    pub fn with_log(self, log: PathBuf) -> Self {
        Self {
            log: Some(log),
            ..self
        }
    }

    /// Append what became of one call. A call the rules allow outright is as much a
    /// decision as one they refuse: without the allows the log says what was stopped and
    /// nothing about what ran, and in `bypass`, where nothing is stopped, it would say
    /// nothing at all.
    pub fn audit(&self, tool: &str, summary: &str, outcome: &str, by: &str, reason: &str) {
        let Some(path) = &self.log else {
            return;
        };
        let line = json!({
            "timestamp": chrono::Local::now().to_rfc3339(),
            "mode": self.mode().to_string(),
            "trusted": self.trusted(),
            "tool": tool,
            "summary": summary,
            "outcome": outcome,
            "by": by,
            "reason": (!reason.is_empty()).then_some(reason),
        });
        if let Some(dir) = path.parent() {
            let _ = crate::sessions::private_dir(dir);
        }
        if let Ok(mut file) = crate::sessions::private_append(path) {
            use std::io::Write;
            let _ = writeln!(file, "{line}");
        }
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
            ("write" | "edit" | image_gen::NAME, _, Some(Value::String(path))) => {
                let path = Path::new(path);
                if rules::is_protected(path, base.home) {
                    (None, None)
                } else {
                    let tool = if tool == "edit" { "edit" } else { "write" };
                    (
                        rules::exact_path(tool, path, base),
                        rules::dir_path(path, base),
                    )
                }
            }
            (patch::NAME, _, _) => match self.written(tool, args).first() {
                Some(path) if !rules::is_protected(path, base.home) => (
                    rules::exact_path("edit", path, base),
                    rules::dir_path(path, base),
                ),
                _ => (None, None),
            },
            ("mcp_call", _, _) => match mcp_name(args) {
                Some(name) => (
                    Rule::parse(&name).ok(),
                    name["mcp__".len()..]
                        .split_once("__")
                        .and_then(|(server, _)| Rule::parse(&format!("mcp__{server}")).ok()),
                ),
                None => (None, None),
            },
            (fetch::NAME, _, _) => match fetch::host(args) {
                Some(host) => (Rule::parse(&format!("Fetch(domain:{host})")).ok(), None),
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
                let how = match rule.is_raw() {
                    true => ", matched as text",
                    false => "",
                };
                out.push_str(&format!("\n  {}  ({source}{inactive}{how})", rule.text));
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
        (patch::NAME, ..) => patch::paths(args, cwd).unwrap_or_default(),
        ("write" | "edit" | image_gen::NAME, Some(path), _) => {
            resolve(cwd, path).into_iter().collect()
        }
        ("bash", _, Some(command)) => {
            let mut cwd = match text("workdir").filter(|w| !w.is_empty()) {
                Some(dir) => cwd.join(dir),
                None => cwd.to_path_buf(),
            };
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
            "bash" => self.check_bash(
                text("command").unwrap_or_default(),
                text("workdir").filter(|w| !w.is_empty()),
            ),
            "read" | "write" | "edit" => match text("path") {
                Some(path) => self.check_path(tool, Path::new(path), needs_approval),
                None => Decision::Ask,
            },
            // The same file in the transcript as a `read`, so `Read` rules decide it.
            view_image::NAME => match text("path") {
                Some(path) => self.check_path("read", Path::new(path), needs_approval),
                None => Decision::Ask,
            },
            // It saves a file, so `Write` and `Edit` rules decide it like a `write`.
            image_gen::NAME => match text("path") {
                Some(path) => self.check_path("write", Path::new(path), needs_approval),
                None => Decision::Ask,
            },
            "mcp_call" => match mcp_name(args) {
                Some(name) => self.check_other(&name, needs_approval),
                None => Decision::Ask,
            },
            patch::NAME => self.check_patch(args, needs_approval),
            crate::tools::stdin::NAME => self.check_typing(tool, &Typing::of(args)),
            fetch::NAME => match fetch::host(args) {
                Some(host) => self.check_fetch(&host, needs_approval),
                None => Decision::Ask,
            },
            schedule::NAME => self.check_other(tool, needs_approval && !schedule::reads_only(args)),
            _ => self.check_other(tool, needs_approval),
        }
    }

    /// `Fetch` rules decide a fetch by the host it names; the redirects it follows are
    /// the guard's to check, not the rules'.
    fn check_fetch(&self, host: &str, needs_approval: bool) -> Decision {
        let find = |rules: &[Rule]| {
            rules
                .iter()
                .find(|r| r.applies_to(fetch::NAME) && r.matches_domain(host))
                .cloned()
        };
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

    /// Each file a patch touches is decided as a `write` to it, so `Edit` and `Write`
    /// rules both reach it, and the strictest answer stands.
    fn check_patch(&self, args: &Value, needs_approval: bool) -> Decision {
        let Some(paths) = patch::paths(args, self.base.cwd) else {
            return Decision::Ask;
        };
        let bare = |rules: &[Rule]| rules.iter().find(|r| r.applies_to(patch::NAME)).cloned();
        if let Some(rule) = bare(&self.rules.deny) {
            return denied(&rule);
        }
        let mut decisions: Vec<Decision> = paths
            .iter()
            .map(|path| self.check_path("write", path, needs_approval))
            .collect();
        if bare(&self.rules.ask).is_some() {
            decisions.push(Decision::Ask);
        }
        if let Some(deny) = decisions.iter().find(|d| matches!(d, Decision::Deny(_))) {
            return deny.clone();
        }
        if decisions.iter().any(|d| matches!(d, Decision::Ask)) {
            return Decision::Ask;
        }
        decisions.into_iter().next().unwrap_or(Decision::Ask)
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

    /// Typing into a running command is a new command, and a poll only reads. What a
    /// shell is given is also every command it holds, so the stricter answer stands: a
    /// `Bash(...)` deny or ask rule and a protected path rule on it as they would on
    /// `bash`, while an allow for the typing alone is not enough, since nothing here
    /// knows where the shell has `cd`'d to.
    fn check_typing(&self, tool: &str, typing: &Typing) -> Decision {
        let own = self.check_other(tool, !matches!(typing, Typing::Nothing));
        let Typing::Shell { entered, .. } = typing else {
            return own;
        };
        if entered.trim().is_empty() {
            return own;
        }
        let bash = self.check_bash(entered, None);
        let rank = |d: &Decision| match d {
            Decision::Deny(_) => 2,
            Decision::Ask => 1,
            Decision::Allow(_) => 0,
        };
        match rank(&bash) > rank(&own) {
            true => bash,
            false => own,
        }
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
        // Before the approval check, not after it: a read needs no approval, but reading
        // a protected path puts it in the transcript, which is the thing being protected
        // against. `cat` on the same file has always asked; this is the read tool
        // answering the same way.
        if self.guarded() && rules::is_protected(path, base.home) {
            return Decision::Ask;
        }
        if !needs_approval {
            return Decision::Allow(String::new());
        }
        let allowed = find(&self.allow(), false)
            .map(|r| rule_reason(&r))
            .or_else(|| self.project_write(tool, path).then(project_reason));
        self.fallback(allowed)
    }

    fn check_bash(&self, command: &str, workdir: Option<&str>) -> Decision {
        let any = |rules: &[Rule]| {
            rules
                .iter()
                .find(|r| r.applies_to("bash") && r.is_any())
                .cloned()
        };
        // A rule kept as text is matched on the command as written, whatever the tokenizer
        // makes of it, so it decides the same way either side of the parse.
        if let Some(rule) = self.find_text(&self.rules.deny, command) {
            return denied(&rule);
        }
        let asked_as_text = self.find_text(&self.rules.ask, command).is_some();
        let Some(commands) = bash::parse(command) else {
            // Unparseable, so it is never allowed. A rule for every command decides it,
            // and so does a worded one over the raw text: the words the user denied are
            // still in there, whatever shape the tokenizer could not read.
            // An ask rule reaches it the same way a deny rule does: matching too much is
            // safe where the answer can only be deny or ask. Without this the shape the
            // tokenizer refused would be the one way past a rule the user wrote.
            let asked = asked_as_text
                || any(&self.rules.ask).is_some()
                || self.find_raw(&self.rules.ask, command).is_some();
            return match any(&self.rules.deny).or_else(|| self.find_raw(&self.rules.deny, command))
            {
                Some(rule) => denied(&rule),
                None if asked => Decision::Ask,
                None => self.fallback(None),
            };
        };
        for c in &commands {
            if let Some(rule) = self.find_loose(&self.rules.deny, c) {
                return denied(&rule);
            }
        }
        if asked_as_text
            || commands
                .iter()
                .any(|c| self.find_loose(&self.rules.ask, c).is_some())
        {
            return Decision::Ask;
        }
        if self.guarded() && commands.iter().any(bash::mentions_protected) {
            return Decision::Ask;
        }

        let mut reasons: Vec<String> = Vec::new();
        // A `workdir` is a `cd` before the first command: inside the project it is
        // followed, and anywhere else it is the `cd` this cannot name, so nothing runs
        // on a rule that was written for the project.
        let start = match workdir {
            None => self.base.cwd.to_path_buf(),
            Some(dir) => match self.cd_into(self.base.cwd, dir) {
                Some(dir) => dir,
                None => return self.fallback(None),
            },
        };
        // The rest of the chain runs wherever its `cd`s have left it, or nowhere this can
        // name once a `cd` it cannot follow has moved it.
        let mut cwd: Option<PathBuf> = Some(start);
        for c in &commands {
            // A `cd` in a pipeline moves only its own subshell, so it says nothing
            // about where the rest of the chain runs.
            if self.mode != Mode::Ask && !c.piped && c.words.first().is_some_and(|w| w == "cd") {
                let followed = bash::cd_target(&c.words)
                    .and_then(|target| self.cd_into(cwd.as_deref()?, target));
                if let Some(next) = followed {
                    cwd = Some(next);
                    let reason = "cd inside the project".to_string();
                    if !reasons.contains(&reason) {
                        reasons.push(reason);
                    }
                    continue;
                }
                // `cd ~`, `cd -`, a bare `cd` and a target outside the project all leave
                // the chain somewhere this cannot name. An allow rule for `cd` still lets
                // the move happen, but what follows it can no longer be called the
                // project's own work: it has to stand on a rule or on being read-only.
                let Some(rule) = self
                    .allow()
                    .iter()
                    .find(|r| r.applies_to("bash") && r.matches_words(&c.words, false))
                    .cloned()
                else {
                    return self.fallback(None);
                };
                cwd = None;
                let reason = rule_reason(&rule);
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
                .filter(|_| self.redirects_allowed(c, cwd.as_deref()))
                .map(rule_reason)
                .or_else(|| read_only.then(|| "read-only".to_string()))
                .or_else(|| self.project_command(c, cwd.as_deref()).then(project_reason));
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
    /// The first rule kept as text that appears in `command`.
    fn find_text(&self, rules: &[Rule], command: &str) -> Option<Rule> {
        rules
            .iter()
            .find(|r| r.applies_to("bash") && r.matches_text(command))
            .cloned()
    }

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
        // As in `check`, the `Read` or `Write` rules and the protected paths decide these.
        let tool = match tool {
            view_image::NAME => "read",
            image_gen::NAME => "write",
            _ => tool,
        };
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
                if let Some(rule) = self.find_text(&self.rules.ask, command) {
                    return Err(Reserved::Asked(rule.text));
                }
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
            patch::NAME => {
                let paths = patch::paths(args, self.base.cwd)
                    .ok_or_else(|| Reserved::Protected("no path".to_string()))?;
                if let Some(rule) = self.rules.ask.iter().find(|r| r.applies_to(patch::NAME)) {
                    return Err(Reserved::Asked(rule.text.clone()));
                }
                paths.iter().try_for_each(|path| {
                    if rules::is_protected(path, self.base.home) {
                        return Err(Reserved::Protected(path.display().to_string()));
                    }
                    match self
                        .rules
                        .ask
                        .iter()
                        .find(|r| r.applies_to("write") && r.matches_path(path, self.base, true))
                    {
                        Some(rule) => Err(Reserved::Asked(rule.text.clone())),
                        None => Ok(()),
                    }
                })
            }
            crate::tools::stdin::NAME => self.judgeable_typing(tool, &Typing::of(args)),
            schedule::NAME => Err(Reserved::Scheduled),
            // The guard refuses a private address when the fetch runs; one written into
            // the URL is kept from the judge too, so it is never what the judge approved.
            fetch::NAME => {
                let host =
                    fetch::host(args).ok_or_else(|| Reserved::Protected("no URL".to_string()))?;
                let inside = host == "localhost"
                    || host.ends_with(".localhost")
                    || ssrf::literal(&host).is_some_and(|ip| ssrf::refusal(ip).is_some());
                if inside {
                    return Err(Reserved::Protected(host));
                }
                match self
                    .rules
                    .ask
                    .iter()
                    .find(|r| r.applies_to(fetch::NAME) && r.matches_domain(&host))
                {
                    Some(rule) => Err(Reserved::Asked(rule.text.clone())),
                    None => Ok(()),
                }
            }
            // An MCP server the user has not approved is never connected, so an
            // `mcp_call` that gets this far names one they did approve.
            _ => match self.rules.ask.iter().find(|r| r.applies_to(tool)) {
                Some(rule) => Err(Reserved::Asked(rule.text.clone())),
                None => Ok(()),
            },
        }
    }

    /// What a shell is given reaches the judge only as `bash` running it would, and only
    /// once it is whole commands; anything else typed is the user's, since nothing says
    /// what the program does with it.
    fn judgeable_typing(&self, tool: &str, typing: &Typing) -> Result<(), Reserved> {
        if let Some(rule) = self.rules.ask.iter().find(|r| r.applies_to(tool)) {
            return Err(Reserved::Asked(rule.text.clone()));
        }
        match typing {
            Typing::Nothing | Typing::Interrupt => Ok(()),
            Typing::Shell { entered, open } => {
                if !entered.trim().is_empty() {
                    self.judgeable("bash", &json!({ "command": entered }))?;
                }
                match open {
                    Some(command) => Err(Reserved::Typed(command.clone())),
                    None => Ok(()),
                }
            }
            Typing::Into(command) => Err(Reserved::Typed(command.clone())),
        }
    }

    /// Whether the guards bhai supplies itself apply: a protected path, a command the
    /// tokenizer could not take apart. `bypass` is the user saying they do not want to be
    /// second-guessed, so it drops them. Their own deny and ask rules are not guards and
    /// stand in every mode.
    fn guarded(&self) -> bool {
        self.mode != Mode::Bypass
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
    fn project_command(&self, command: &bash::Command, cwd: Option<&Path>) -> bool {
        // With nowhere to resolve a relative path against, nothing can be shown to be the
        // project's own work.
        let Some(cwd) = cwd else {
            return false;
        };
        let inside = |arg: &str| rules::is_inside(&cwd.join(arg), self.base.cwd);
        self.relaxed()
            && self.relax.commands
            && command.nested().is_empty()
            && bash::is_project_command(&command.words, &inside)
            && self.redirects_inside(command, Some(cwd))
    }

    /// Whether every file the command redirects into is one a write would be allowed to
    /// make: a redirect is a write, so `auto` relaxes it on the same terms. A command
    /// that writes nowhere passes.
    fn redirects_inside(&self, command: &bash::Command, cwd: Option<&Path>) -> bool {
        command.writes.iter().all(|target| {
            self.write_target(cwd, target)
                .is_some_and(|path| self.project_write("write", &path))
        })
    }

    /// Whether every file the command redirects into is one the rules allow writing. An
    /// allow rule names a program, not the files a redirect points its output at, so the
    /// targets are checked as writes in their own right and a deny, an ask or a
    /// protected path still stops them.
    fn redirects_allowed(&self, command: &bash::Command, cwd: Option<&Path>) -> bool {
        command.writes.iter().all(|target| {
            self.write_target(cwd, target).is_some_and(|path| {
                matches!(self.check_path("write", &path, true), Decision::Allow(_))
            })
        })
    }

    /// The file a redirect target names. A leading `~/` is the home directory; `Path::join`
    /// would take it for a relative component and land it inside the project. Any other
    /// `~` form is an expansion nothing here can resolve, so there is no path to check
    /// and the redirect fails closed. So is a relative target with no directory to resolve
    /// it against.
    fn write_target(&self, cwd: Option<&Path>, target: &str) -> Option<PathBuf> {
        match target.strip_prefix("~/") {
            Some(rest) => Some(self.base.home?.join(rest)),
            None if target.starts_with('~') => None,
            None if Path::new(target).is_absolute() => Some(PathBuf::from(target)),
            None => Some(cwd?.join(target)),
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

/// What a `write_stdin` call types, as the rules read it.
#[derive(Debug, Clone, PartialEq)]
enum Typing {
    /// Nothing: a poll.
    Nothing,
    /// Ctrl-c and nothing else.
    Interrupt,
    /// Input for an interactive shell: `entered`, every line given to it since it last
    /// finished a command, read as one script, and an unfinished line after it left out
    /// until it is entered. `open` holds the shell's command when the last entered line
    /// leaves it inside a quote, a continuation or a heredoc, so it runs nothing yet and
    /// nothing here can say what the line that closes it will make of it.
    Shell {
        entered: String,
        open: Option<String>,
    },
    /// Anything else, by the session's command.
    Into(String),
}

impl Typing {
    fn of(args: &Value) -> Typing {
        let chars = args
            .get("chars")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let session = args
            .get("session_id")
            .and_then(Value::as_u64)
            .and_then(|id| u32::try_from(id).ok())
            .and_then(|id| crate::tools::bash::input_line(id, chars));
        match session {
            Some((command, unrun)) => Typing::read(&command, &unrun, chars),
            None => Typing::read("", chars, chars),
        }
    }

    /// `chars` typed into `command`, which leaves `unrun` as what the shell has been given
    /// since it last finished a command, `chars` included. A key that edits or completes
    /// the line, a tab among them, makes what runs something other than what was typed.
    fn read(command: &str, unrun: &str, chars: &str) -> Typing {
        match chars {
            "" => return Typing::Nothing,
            "\u{3}" => return Typing::Interrupt,
            _ => {}
        }
        let plain = !unrun
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\r');
        if !plain || !bash::is_shell(command) {
            return Typing::Into(command.to_string());
        }
        let typed = bash::as_typed(unrun);
        let entered = typed.rfind('\n').map_or(0, |at| at + 1);
        let open = (bash::finished(&typed) < entered).then(|| command.to_string());
        Typing::Shell {
            entered: typed[..entered].to_string(),
            open,
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
        policy.check(
            tool,
            &json!({ "path": path }),
            !["read", "view_image"].contains(&tool),
        )
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
            of("dd if=/dev/zero of=disk.img bs=1m && dd if=a of=/dev/null"),
            ["/p/disk.img"]
        );
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
            (&bypass, "cat ~/.ssh/id_rsa", allowed("read-only")),
            (&ask_mode, "cat ~/.ssh/id_rsa", Decision::Ask),
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
        // No rule names it, and `bypass` does not keep a shape it could not read for
        // the user; `ask` and `auto` still do.
        assert_eq!(bash(&worded, "ls $(x)"), allowed("bypass mode"));
        let asking = policy(Mode::Ask, &[], &["Bash(rm:*)"], &[]);
        assert_eq!(bash(&asking, "ls $(x)"), Decision::Ask);
        // An ask rule the user wrote is not a guard, so it still asks in `bypass`.
        let asked = policy(Mode::Bypass, &[], &[], &["Bash(ls:*)"]);
        assert_eq!(bash(&asked, "ls $(x)"), Decision::Ask);
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
            // Allow rules never open protected paths, and `ask` and `auto` keep them for
            // the user. `bypass` drops the guard: the user said not to be asked.
            (&auto, "write", "/home/u/repo/.git/config", Decision::Ask),
            (&ask_mode, "edit", "/home/u/.ssh/config", Decision::Ask),
            (&auto, "edit", "/home/u/repo/.env", Decision::Ask),
            (
                &bypass,
                "write",
                "/home/u/repo/.git/config",
                allowed("rule Write(.git/**)"),
            ),
            (
                &bypass,
                "edit",
                "/home/u/.ssh/config",
                allowed("bypass mode"),
            ),
            (&bypass, "edit", "/home/u/repo/.env", allowed("bypass mode")),
            (
                &bypass,
                "edit",
                "/home/u/repo/README.md",
                allowed("bypass mode"),
            ),
            (&ask_mode, "edit", "/home/u/repo/src/main.rs", Decision::Ask),
            (&ask_mode, "edit", "/home/u/repo/.git/HEAD", Decision::Ask),
            // Reads skip approval, but not on a protected path: the read is what puts
            // the file in the transcript.
            (&ask_mode, "read", "/home/u/repo/.env", Decision::Ask),
            (&auto, "read", "/home/u/.aws/credentials", Decision::Ask),
            (&bypass, "read", "/home/u/.ssh/id_rsa", allowed("")),
            (&ask_mode, "read", "/home/u/repo/src/main.rs", allowed("")),
            (
                &ask_mode,
                "read",
                "/home/u/repo/k.PEM",
                Decision::Deny("deny rule Read(*.pem)".to_string()),
            ),
            // An image is read into the transcript the same way.
            (
                &ask_mode,
                "view_image",
                "/home/u/repo/shot.png",
                allowed(""),
            ),
            (&auto, "view_image", "/home/u/repo/.env", Decision::Ask),
            (
                &ask_mode,
                "view_image",
                "/home/u/repo/k.pem",
                Decision::Deny("deny rule Read(*.pem)".to_string()),
            ),
            // A generated image is saved like a `write` of its path.
            (
                &auto,
                "image_gen",
                "/home/u/repo/src/logo.png",
                allowed("rule Edit(src/**)"),
            ),
            (
                &auto,
                "image_gen",
                "/home/u/repo/src/generated/a.png",
                Decision::Ask,
            ),
            (
                &auto,
                "image_gen",
                "/home/u/repo/secrets/a.png",
                Decision::Deny("deny rule Edit(secrets/**)".to_string()),
            ),
            (&auto, "image_gen", "/home/u/repo/.git/a.png", Decision::Ask),
            (&ask_mode, "image_gen", "/home/u/x.png", Decision::Ask),
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

    /// The backend is not trusted with the transcript it is served. A model that asks
    /// for a credential file must not get one without the user seeing the call, whatever
    /// the mode, and whichever tool it reaches for: the read tool and `cat` answer the
    /// same, or the cheaper one is the whole defence.
    #[test]
    fn a_hostile_backend_cannot_read_credentials() {
        let trusted = |mode| {
            let policy = policy(mode, &[], &[], &[]);
            let trust = Trust::new(Path::new("/home/u/.config/bhai"), Path::new("/home/u/repo"));
            policy.set_trusted(&trust.snapshot());
            policy
        };
        let commands = [
            "cat /home/u/.ssh/id_rsa",
            "cat /home/u/keys/id_ed25519",
            "cat /home/u/.aws/credentials",
            "cat /home/u/.netrc",
            "cat /home/u/.git-credentials",
            "cat /home/u/.npmrc",
            "cat /home/u/.pypirc",
            "cat /home/u/.kube/config",
            "cat /home/u/.docker/config.json",
            "cat /home/u/.cargo/credentials.toml",
            "head -c 64 /home/u/certs/server.pem",
            "find /home/u -name '*.pem' -type f",
        ];
        let paths = [
            "/home/u/.ssh/id_rsa",
            "/home/u/repo/.env",
            "/home/u/.codex/auth.json",
            "/home/u/.aws/credentials",
            "/home/u/.netrc",
            "/home/u/certs/server.pem",
        ];
        // `bypass` is the user saying they do not want to be asked, so the guarantee is
        // over the modes that weigh a call at all.
        for mode in [Mode::Ask, Mode::Auto] {
            let policy = trusted(mode);
            for command in commands {
                assert!(
                    !matches!(bash(&policy, command), Decision::Allow(_)),
                    "{command} in {mode}"
                );
            }
            assert_eq!(bash(&policy, "env"), Decision::Ask, "env in {mode}");
            for path in paths {
                assert_eq!(
                    file(&policy, "read", path),
                    Decision::Ask,
                    "{path} in {mode}"
                );
                // `auto` never prompts, so the judge must not be offered it either.
                for tool in ["read", "view_image"] {
                    assert!(
                        policy.judgeable(tool, &json!({ "path": path })).is_err(),
                        "{tool} {path} in {mode}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_patch_is_decided_by_every_file_it_touches() {
        let patch = |files: &[&str]| {
            let hunks: String = files
                .iter()
                .map(|f| format!("*** Add File: {f}\n+x\n"))
                .collect();
            json!({ "input": format!("*** Begin Patch\n{hunks}*** End Patch") })
        };
        let check =
            |policy: &Policy, files: &[&str]| policy.check("apply_patch", &patch(files), true);

        let bypass = policy(Mode::Bypass, &[], &["Write(//etc/**)"], &[]);
        assert_eq!(check(&bypass, &["a.txt"]), allowed("bypass mode"));
        assert_eq!(
            check(&bypass, &["a.txt", "/etc/hosts"]),
            Decision::Deny("deny rule Write(//etc/**)".to_string())
        );
        let bare = policy(Mode::Bypass, &[], &["apply_patch"], &[]);
        assert_eq!(
            check(&bare, &["a.txt"]),
            Decision::Deny("deny rule apply_patch".to_string())
        );

        let auto = policy(Mode::Auto, &["Edit(//home/u/repo/**)"], &[], &[]);
        assert_eq!(
            check(&auto, &["a.txt", "src/b.rs"]),
            allowed("rule Edit(//home/u/repo/**)")
        );
        assert_eq!(check(&auto, &["a.txt", "/tmp/x"]), Decision::Ask);
        let asked = policy(
            Mode::Auto,
            &["Edit(//home/u/repo/**)"],
            &[],
            &["Edit(/b.rs)"],
        );
        assert_eq!(check(&asked, &["a.txt", "b.rs"]), Decision::Ask);
        assert_eq!(
            asked.judgeable("apply_patch", &patch(&["a.txt", "b.rs"])),
            Err(Reserved::Asked("Edit(/b.rs)".to_string()))
        );

        assert_eq!(
            auto.written("apply_patch", &patch(&["a.txt", "../c.txt"])),
            [
                PathBuf::from("/home/u/repo/a.txt"),
                PathBuf::from("/home/u/c.txt")
            ]
        );
        let ask = policy(Mode::Ask, &[], &[], &[]);
        assert!(
            ask.offers("apply_patch", &patch(&["src/a.rs"]))
                .exact
                .is_some()
        );
        assert_eq!(
            ask.offers("apply_patch", &patch(&["src/a.rs", "/tmp/b"]))
                .exact,
            None,
            "one file's rule does not let the other through"
        );
    }

    #[test]
    fn a_fetch_deny_rule_covers_the_trailing_dot_and_unicode_spellings() {
        let call = |url: &str| json!({ "url": url });
        let bypass = policy(
            Mode::Bypass,
            &[],
            &[
                "Fetch(domain:evil.com)",
                "Fetch(domain:*.evil.com)",
                "Fetch(domain:bücher.de)",
            ],
            &[],
        );
        for (url, rule) in [
            ("https://evil.com./x", "Fetch(domain:evil.com)"),
            ("https://a.evil.com./", "Fetch(domain:*.evil.com)"),
            ("https://bücher.de/", "Fetch(domain:bücher.de)"),
            ("https://xn--bcher-kva.de./", "Fetch(domain:bücher.de)"),
        ] {
            assert_eq!(
                bypass.check("fetch", &call(url), true),
                Decision::Deny(format!("deny rule {rule}")),
                "{url}"
            );
        }
    }

    #[test]
    fn a_fetch_is_decided_by_its_domain_and_a_private_one_never_judged() {
        let call = |url: &str| json!({ "url": url });
        let docs = call("https://Docs.rs/serde");
        let auto = policy(
            Mode::Auto,
            &["Fetch(domain:docs.rs)"],
            &["Fetch(domain:*.evil.com)"],
            &["Fetch(domain:ask.me)"],
        );
        assert_eq!(
            auto.check("fetch", &docs, true),
            allowed("rule Fetch(domain:docs.rs)")
        );
        assert_eq!(
            auto.check("fetch", &call("http://a.evil.com/"), true),
            Decision::Deny("deny rule Fetch(domain:*.evil.com)".to_string())
        );
        assert_eq!(
            auto.check("fetch", &call("https://ask.me/"), true),
            Decision::Ask
        );
        assert_eq!(
            auto.check("fetch", &call("https://other.org/"), true),
            Decision::Ask
        );
        let ask = policy(Mode::Ask, &[], &[], &[]);
        assert_eq!(ask.check("fetch", &docs, true), Decision::Ask);
        assert_eq!(
            ask.offers("fetch", &docs).exact.as_deref(),
            Some("Fetch(domain:docs.rs)")
        );

        assert_eq!(auto.judgeable("fetch", &call("https://other.org/")), Ok(()));
        assert_eq!(
            auto.judgeable("fetch", &call("https://ask.me/x")),
            Err(Reserved::Asked("Fetch(domain:ask.me)".to_string()))
        );
        for url in [
            "http://127.0.0.1:8080/",
            "http://localhost/",
            "http://api.localhost/",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.1.2.3/",
            "http://[::1]/",
            "http://0x7f.1/",
        ] {
            assert!(
                matches!(
                    auto.judgeable("fetch", &call(url)),
                    Err(Reserved::Protected(_))
                ),
                "{url}"
            );
        }
    }

    #[test]
    fn a_schedule_the_model_sets_asks_and_is_never_judged() {
        let create = json!({ "action": "create", "when": "in 20m", "prompt": "x" });
        let list = json!({ "action": "list" });
        let cancel = json!({ "action": "cancel", "id": "abc123" });
        let ask = policy(Mode::Ask, &[], &[], &[]);
        assert_eq!(ask.check("schedule", &create, true), Decision::Ask);
        assert_eq!(ask.offers("schedule", &create), Offers::default());
        let auto = policy(Mode::Auto, &[], &[], &[]);
        assert_eq!(auto.check("schedule", &create, true), Decision::Ask);
        assert_eq!(
            auto.judgeable("schedule", &create),
            Err(Reserved::Scheduled)
        );
        for args in [&list, &cancel] {
            assert_eq!(
                auto.check("schedule", args, true),
                Decision::Allow(String::new())
            );
        }
        let denied = policy(Mode::Auto, &[], &["Schedule"], &[]);
        assert!(matches!(
            denied.check("schedule", &list, true),
            Decision::Deny(_)
        ));
        // Bypass lets everything run, and the user's own rule lets it run in `auto`.
        let bypass = policy(Mode::Bypass, &[], &[], &[]);
        assert!(matches!(
            bypass.check("schedule", &create, true),
            Decision::Allow(_)
        ));
        let allowed = policy(Mode::Auto, &["Schedule"], &[], &[]);
        assert_eq!(
            allowed.check("schedule", &create, true),
            Decision::Allow("rule Schedule".to_string())
        );
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
    fn typing_into_a_session_is_asked_about_and_a_poll_is_not() {
        let poll = json!({ "session_id": 1 });
        let typed = json!({ "session_id": 1, "chars": "y\n" });
        let p = policy(Mode::Auto, &[], &[], &[]);
        assert_eq!(p.check("write_stdin", &poll, false), allowed(""));
        assert_eq!(p.check("write_stdin", &typed, false), Decision::Ask);
        let allow = policy(Mode::Auto, &["write_stdin"], &[], &[]);
        assert_eq!(
            allow.check("write_stdin", &typed, false),
            allowed("rule write_stdin")
        );
        let deny = policy(Mode::Bypass, &[], &["write_stdin"], &[]);
        assert!(matches!(
            deny.check("write_stdin", &poll, false),
            Decision::Deny(_)
        ));
        let bypass = policy(Mode::Bypass, &[], &[], &[]);
        assert!(matches!(
            bypass.check("write_stdin", &typed, false),
            Decision::Allow(_)
        ));
    }

    #[test]
    fn a_line_typed_into_a_shell_is_read_as_the_commands_it_holds() {
        let shell = |line: &str| Typing::read("bash -i", line, line);
        let entered = |text: &str| Typing::Shell {
            entered: text.to_string(),
            open: None,
        };
        assert_eq!(shell(""), Typing::Nothing);
        assert_eq!(shell("\u{3}"), Typing::Interrupt);
        assert_eq!(shell("cat .env\n"), entered("cat .env\n"));
        // A line not yet entered runs nothing, so it waits for the write that enters it.
        assert_eq!(shell("ls\r\ngit pu"), entered("ls\n\n"));
        assert_eq!(shell("git pu"), entered(""));
        // The start of the line an earlier write left is part of what runs.
        assert_eq!(
            Typing::read("bash", "cat .env\n", "nv\n"),
            entered("cat .env\n")
        );
        // A line that leaves the shell inside a quote, a continuation or a heredoc runs
        // nothing yet, and only the user can say what the line closing it is for.
        for text in [
            "echo \"\n",
            "echo a \\\n",
            "cat <<EOF\nx\n",
            "ls\necho 'a\n",
        ] {
            assert_eq!(
                shell(text),
                Typing::Shell {
                    entered: text.to_string(),
                    open: Some("bash -i".to_string()),
                },
                "{text:?}"
            );
        }
        assert_eq!(shell("cat <<EOF\nx\nEOF\n"), entered("cat <<EOF\nx\nEOF\n"));
        // A tab completes and an escape edits, so the line is not what was typed.
        let tab = Typing::Into("bash -i".to_string());
        assert_eq!(shell("cat .e\t\n"), tab);
        assert_eq!(shell("cat .x\u{1b}[Denv\n"), tab);
        // Anything but a plain shell, including no session at all.
        assert_eq!(
            Typing::read("python3", "x\n", "x\n"),
            Typing::Into("python3".to_string())
        );
        assert_eq!(Typing::read("", "x\n", "x\n"), Typing::Into(String::new()));
    }

    /// Each write in turn into a session running `bash`, as `write_stdin` makes them: the
    /// decision on each, with what the shell has not yet run carried to the next.
    fn type_into_bash(p: &Policy, writes: &[&str]) -> Vec<Decision> {
        let mut unrun = String::new();
        writes
            .iter()
            .map(|chars| {
                let typing = Typing::read("bash", &format!("{unrun}{chars}"), chars);
                unrun = crate::tools::bash::unrun(&unrun, chars);
                let rules = p.rules();
                p.checker(&rules, p.mode())
                    .check_typing("write_stdin", &typing)
            })
            .collect()
    }

    #[test]
    fn a_command_split_across_lines_answers_to_a_deny_rule_as_a_whole() {
        let bypass = policy(Mode::Bypass, &[], &["Bash(rm:*)"], &[]);
        let denied = Decision::Deny("deny rule Bash(rm:*)".to_string());
        // An open quote joins the lines, so the middle one is `rm` at the front of a
        // command, not inside a word.
        for writes in [
            &["echo \"\n\"; rm -rf x; echo \"\n\"\n"][..],
            &["echo \"\n", "\"; rm -rf x; echo \"\n", "\"\n"],
            &["echo '\n", "'; rm -rf x; echo '\n", "'\n"],
            // A backslash at the end of a line continues it.
            &["echo a \\\n", "; rm -rf x\n"],
            &["echo a \\\n; rm -rf x\n"],
            // `\r` is enter on a terminal too.
            &["echo \"\r", "\"; rm -rf x\r"],
            // A line ending inside `$(` waits for the `)`.
            &["echo $(\n", "true); rm -rf x\n"],
        ] {
            let decisions = type_into_bash(&bypass, writes);
            assert_eq!(decisions.last(), Some(&denied), "{writes:?}: {decisions:?}");
        }
        // Inside a quoted heredoc `rm` is data, once the heredoc is whole; past its end
        // it is a command again.
        let heredoc = type_into_bash(&bypass, &["cat <<'EOF'\n", "x\n", "rm -rf x\nEOF\n"]);
        assert!(
            heredoc.iter().all(|d| matches!(d, Decision::Allow(_))),
            "{heredoc:?}"
        );
        // While open it is read word by word, which only ever asks more.
        let open = type_into_bash(&bypass, &["cat <<'EOF'\n", "rm -rf x\n"]);
        assert_eq!(open[1], denied);
        let past = type_into_bash(&bypass, &["cat <<'EOF'\n", "x\nEOF\nrm -rf x\n"]);
        assert_eq!(past.last(), Some(&denied), "{past:?}");
        // A finished command leaves what is carried, and a ctrl-c drops it.
        let after = type_into_bash(&bypass, &["echo \"\n", "\u{3}", "ls\n", "rm x\n"]);
        assert!(matches!(after[2], Decision::Allow(_)), "{after:?}");
        assert_eq!(after[3], denied);
    }

    #[test]
    fn a_line_left_inside_a_quote_is_only_the_users_to_approve() {
        let dir = std::env::temp_dir().join(format!("bhai-open-{}", uuid::Uuid::new_v4()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let p = Policy::new(Mode::Auto, Rules::default(), None, repo.clone())
            .with_trust(Trust::new(&dir.join("config"), &repo));
        p.trust().unwrap();
        let judgeable = |unrun: &str, chars: &str| {
            let rules = p.rules();
            p.checker(&rules, p.mode())
                .judgeable_typing("write_stdin", &Typing::read("bash", unrun, chars))
        };
        let typed = Err(Reserved::Typed("bash".to_string()));
        assert_eq!(judgeable("echo \"\n", "echo \"\n"), typed);
        assert_eq!(judgeable("cat <<'EOF'\n", "cat <<'EOF'\n"), typed);
        assert_eq!(judgeable("echo a \\\n", "echo a \\\n"), typed);
        // Once closed, it is judged as the whole command.
        assert!(judgeable("echo \"\nx\"\n", "x\"\n").is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_line_typed_into_a_shell_answers_to_the_bash_rules() {
        let shell = |line: &str| Typing::read("bash", line, line);
        let check = |p: &Policy, line: &str| {
            let rules = p.rules();
            p.checker(&rules, p.mode())
                .check_typing("write_stdin", &shell(line))
        };
        let bypass = policy(Mode::Bypass, &[], &["Bash(git push:*)"], &["Bash(rm:*)"]);
        assert_eq!(
            check(&bypass, "git push\n"),
            Decision::Deny("deny rule Bash(git push:*)".to_string())
        );
        assert_eq!(check(&bypass, "ls\nrm -rf x\n"), Decision::Ask);
        assert!(matches!(check(&bypass, "git status\n"), Decision::Allow(_)));

        // An allow rule for typing does not carry a line `bash` would ask about.
        let allow = policy(Mode::Auto, &["write_stdin"], &[], &[]);
        assert_eq!(check(&allow, "cat .env\n"), Decision::Ask);
        assert_eq!(check(&allow, "ls\n"), allowed("rule write_stdin"));
        // Nor does a line `bash` would allow carry the typing.
        let auto = policy(Mode::Auto, &[], &[], &[]);
        assert_eq!(check(&auto, "ls\n"), Decision::Ask);
    }

    #[test]
    fn typing_reaches_the_judge_only_as_the_command_would() {
        let dir = std::env::temp_dir().join(format!("bhai-typed-{}", uuid::Uuid::new_v4()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let rules = Rules {
            ask: rules(&["Bash(git push:*)"]),
            ..Rules::default()
        };
        let p = Policy::new(Mode::Auto, rules, None, repo.clone())
            .with_trust(Trust::new(&dir.join("config"), &repo));
        p.trust().unwrap();
        let judgeable = |command: &str, line: &str| {
            let rules = p.rules();
            p.checker(&rules, p.mode())
                .judgeable_typing("write_stdin", &Typing::read(command, line, line))
        };
        assert!(judgeable("bash", "cargo test\n").is_ok());
        assert!(judgeable("bash", "\u{3}").is_ok());
        assert_eq!(
            judgeable("bash", "cat .env\n"),
            Err(Reserved::Protected(".env".to_string()))
        );
        assert_eq!(
            judgeable("bash", "ls\ngit push\n"),
            Err(Reserved::Asked("Bash(git push:*)".to_string()))
        );
        assert_eq!(
            judgeable("bash", "eval \"$X\"\n"),
            Err(Reserved::RunsCode("eval".to_string()))
        );
        assert_eq!(
            judgeable("python3", "print(1)\n"),
            Err(Reserved::Typed("python3".to_string()))
        );
        std::fs::remove_dir_all(dir).unwrap();
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
        assert_eq!(
            offers("image_gen", json!({"path": "/home/u/repo/assets/logo.png"})),
            both("Write(/assets/logo.png)", "Edit(/assets/**)")
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
        assert!(
            command("for f in rg fd; do command -v $f; done").is_ok(),
            "a lookup the loop hides from the blunt split"
        );

        // An ask rule is the user saying they want to see it, so the judge does not get
        // to wave it through in the one mode where nothing else would stop it.
        let asking = Policy::new(
            Mode::Auto,
            Rules {
                ask: rules(&[
                    "Bash(git log -p:*)",
                    "Bash(cargo publish:*)",
                    "Write(gen/**)",
                    "Read(private/**)",
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
        assert_eq!(
            asking.judgeable("view_image", &json!({"path": repo.join("private/a.png")})),
            asked("Read(private/**)")
        );
        assert!(
            asking
                .judgeable("view_image", &json!({"path": repo.join("docs/a.png")}))
                .is_ok()
        );
        assert_eq!(
            asking.judgeable("image_gen", &json!({"path": repo.join("gen/a.png")})),
            asked("Write(gen/**)")
        );
        assert!(
            asking
                .judgeable("image_gen", &json!({"path": repo.join("assets/a.png")}))
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
    fn a_workdir_decides_a_command_as_a_cd_would() {
        let dir = std::env::temp_dir().join(format!("bhai-workdir-{}", uuid::Uuid::new_v4()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        let p = Policy::new(Mode::Auto, Rules::default(), None, repo.clone())
            .with_trust(Trust::new(&dir.join("config"), &repo));
        p.trust().unwrap();
        let in_dir = |command: &str, workdir: &str| {
            p.check(
                "bash",
                &json!({ "command": command, "workdir": workdir }),
                true,
            )
        };
        let project = allowed("auto, inside the project");
        assert_eq!(in_dir("cargo test", "sub"), project);
        assert_eq!(
            in_dir("cargo test", &repo.join("sub").display().to_string()),
            project
        );
        // Resolved against the workdir, the redirect lands outside the project.
        assert_eq!(in_dir("cargo test > ../../x", "sub"), Decision::Ask);
        assert_eq!(in_dir("cargo test", "../outside"), Decision::Ask);
        assert_eq!(
            in_dir("cargo test", &dir.join("outside").display().to_string()),
            Decision::Ask
        );
        let args = json!({ "command": "echo a > x", "workdir": "sub" });
        assert_eq!(p.written("bash", &args), [repo.join("sub").join("x")]);
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

    /// A rule whose shape `Rule::parse` refuses used to be dropped with a notice, so a
    /// deny rule stopped nothing. Kept as text, it stops what it names, wherever it is.
    #[test]
    fn a_rule_kept_as_text_denies_and_asks_on_the_command_as_written() {
        let deny = Rule::raw("Bash(rm -rf /)").unwrap();
        assert!(deny.is_raw());
        let p = Policy::new(
            Mode::Auto,
            Rules {
                deny: vec![deny],
                ask: vec![Rule::raw("Bash(npm run test?)").unwrap()],
                ..Rules::default()
            },
            None,
            std::env::temp_dir(),
        );
        for command in [
            "rm -rf /",
            "RM -RF /",
            "echo one && rm -rf / && echo two",
            // A shape the tokenizer refuses outright is still matched on its text.
            "for d in a b; do rm -rf / $d; done",
        ] {
            assert_eq!(
                bash(&p, command),
                Decision::Deny("deny rule Bash(rm -rf /)".to_string()),
                "{command}"
            );
        }
        assert_ne!(bash(&p, "rm -rf /tmp/x"), Decision::Ask, "not this one");
        // An ask rule kept as text is the user saying they want to see it, so the judge
        // is not offered it either.
        assert_eq!(bash(&p, "npm run test?"), Decision::Ask);
        assert!(matches!(
            p.judgeable("bash", &json!({"command": "npm run test?"})),
            Err(Reserved::Asked(_))
        ));
        // Only deny and ask: an allow rule that over-matched would approve what was not.
        assert!(Rule::raw("Read(*.pem)").is_none());
        assert!(Rule::raw("Bash").is_none());
        assert!(
            p.describe()
                .contains("Bash(rm -rf /)  (built in, matched as text)")
        );
    }

    /// `Bash(cd:*)` is common in a Claude Code settings file. It lets the move happen; it
    /// does not make wherever the move landed the project.
    #[test]
    fn an_allow_rule_for_cd_leaves_the_directory_unknown() {
        let dir = std::env::temp_dir().join(format!("bhai-policy-{}", uuid::Uuid::new_v4()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join("wordcount")).unwrap();
        let rules = Rules {
            allow: rules(&["Bash(cd:*)"]),
            ..Rules::default()
        };
        let p = Policy::new(Mode::Auto, rules, None, repo.clone())
            .with_trust(Trust::new(&dir.join("config"), &repo));
        p.trust().unwrap();

        // The rule used to carry the project with it: `cd ~` was not a shape the cwd
        // tracking could follow, so it fell through to the rule and `cargo test` ran as
        // the project's own work in the home directory.
        for command in [
            "cd ~ && cargo test",
            "cd - && cargo test",
            "cd && cargo test",
            "cd /etc && cargo test",
            "cd .. && echo x > out",
        ] {
            assert_eq!(bash(&p, command), Decision::Ask, "{command}");
        }
        // What stands on its own still stands: the move is allowed and `ls` reads.
        assert_eq!(
            bash(&p, "cd /etc && ls -la"),
            allowed("rule Bash(cd:*), read-only")
        );
        // A `cd` this can follow is still followed, rule or no rule.
        assert_eq!(
            bash(&p, "cd wordcount && cargo test"),
            allowed("cd inside the project, auto, inside the project")
        );
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
