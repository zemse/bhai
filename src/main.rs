//! bhai: a very small coding agent. A few tools, one loop, one approval prompt.
//! Inference runs through the Codex CLI's ChatGPT-subscription credentials.

mod agent;
mod app;
mod auth;
mod branch;
mod cache;
mod client;
mod clipboard;
mod commands;
mod compact;
mod config;
mod debug;
mod diff;
mod entries;
mod frontmatter;
mod identity;
mod input;
mod instructions;
mod judge;
mod limits;
mod markdown;
mod mcp;
mod mermaid;
mod models;
mod ollama;
mod palette;
mod permissions;
mod profile;
mod prompt;
mod server;
mod session;
mod sessions;
mod skills;
mod speed;
mod statusline;
mod syntax;
mod title;
mod tokens;
mod tools;
mod ui;
mod workflow;
mod wrap;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TermEvent, KeyEvent, KeyboardEnhancementFlags, MouseEvent,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::{execute, terminal};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};

use crate::agent::{AgentEvent, Control, Delegation, Saved};
use crate::app::App;
use crate::compact::Limits;
use crate::config::{Config, Flags};
use crate::judge::{Judge, ModelJudge};
use crate::permissions::Policy;
use crate::prompt::SystemPrompt;
use crate::session::Session;

/// How often the UI wakes up when nothing is happening (keeps the spinner moving).
const TICK: Duration = Duration::from_millis(120);

enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Session(session::Event),
    /// The forwarder fell behind and the channel dropped events, so what the stream sets
    /// is read from the session instead.
    Resync,
    Tick,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "identities") {
        return identities();
    }
    if args.first().is_some_and(|a| a == "sessions") {
        let dir = std::env::current_dir()?.join(sessions::DIR);
        // `prune [n]` is the only way sessions are ever deleted: they are the debug record,
        // so nothing removes one on its own.
        if args.get(1).is_some_and(|a| a == "prune") {
            let keep = match args.get(2) {
                Some(n) => n
                    .parse()
                    .with_context(|| format!("`{n}` is not a number of sessions to keep"))?,
                None => sessions::PRUNE_KEEP,
            };
            print!("{}", sessions::prune_report(&sessions::prune(&dir, keep)));
            return Ok(());
        }
        print!("{}", sessions::report(&sessions::list(&dir)));
        return Ok(());
    }

    // `bhai usage` prints the plan's windows and credits, as `/usage` does.
    if args.first().is_some_and(|a| a == "usage") {
        let body = limits::fetch_now().await?;
        println!("{}", limits::report(&body, chrono::Local::now()));
        return Ok(());
    }

    // `bhai mcp approve <name>` records a `.mcp.json` server as it is defined now, which
    // is how a server that changed since its approval is accepted again.
    if args.first().is_some_and(|a| a == "mcp") {
        let name = match (args.get(1).map(String::as_str), args.get(2)) {
            (Some("approve"), Some(name)) => name,
            _ => bail!("usage: bhai mcp approve <server>"),
        };
        let cwd = std::env::current_dir()?;
        let roots = instructions::Roots::from_env(cwd.clone());
        let config = Config::load(roots.home.as_deref(), &roots.cwd)?;
        let server = mcp::servers::approve(&roots, &config.mcp_servers, name)?;
        let what = match &server.url {
            Some(url) => url.clone(),
            None => format!("{} {}", server.command, server.args.join(" "))
                .trim()
                .to_string(),
        };
        println!("approved {name} as `{what}` from {}", server.source);
        println!("it starts with the next bhai in this project");
        return Ok(());
    }

    // `bhai --probe [prompt]` does one non-interactive model call, for checking that
    // auth and the wire format still work without entering the TUI.
    if args.first().is_some_and(|a| a == "--probe") {
        let setup = load(Flags::default(), identity::DEFAULT).await?;
        let hub = setup.prompt.mcp.clone();
        let result = probe(setup, args.get(1).cloned()).await;
        shutdown(hub).await;
        return result;
    }
    // `bhai --cache-check [minutes]` sends a few calls on one prefix and checks the cache
    // served it; with minutes, one more call after that long checks it lasted.
    if args.first().is_some_and(|a| a == "--cache-check") {
        let wait = match args.get(1) {
            Some(m) => Some(
                m.trim_end_matches('m')
                    .parse::<u64>()
                    .map(|m| Duration::from_secs(m * 60))
                    .with_context(|| format!("--cache-check takes minutes, not {m}"))?,
            ),
            None => None,
        };
        let setup = load(Flags::default(), identity::DEFAULT).await?;
        let hub = setup.prompt.mcp.clone();
        let result = cache_check(setup, wait).await;
        shutdown(hub).await;
        if !result? {
            std::process::exit(1);
        }
        return Ok(());
    }
    // `bhai --judge-eval [file]` scores the judge against a file of cases.
    if args.first().is_some_and(|a| a == "--judge-eval") {
        let setup = load(Flags::default(), identity::DEFAULT).await?;
        let hub = setup.prompt.mcp.clone();
        let result = judge_eval(setup, args.get(1).cloned()).await;
        shutdown(hub).await;
        if !result? {
            std::process::exit(1);
        }
        return Ok(());
    }
    let args = match parse_args(&args) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!(
                "bhai: {e:#}\nusage: bhai [identities] [usage] [sessions [prune [n]]] [mcp approve <server>] [--probe [prompt]] [--cache-check [minutes]] [--judge-eval [file]] [--as <identity>] [--resume [id]] [--workflow <name> [input] [--workflow-yes]] [--model <name>] [--effort <level>] [--serve [port] [--headless]] [--profile] [--strict-cache] [--mode ask|auto|bypass] [--trust] [--no-global] [--no-project] [--bare]"
            );
            std::process::exit(2);
        }
    };
    let cwd = std::env::current_dir()?;
    let dir = cwd.join(sessions::DIR);
    let resumed = match &args.resume {
        Some(id) => Some(sessions::find(&dir, id.as_deref())?),
        None => None,
    };
    let mut warnings = Vec::new();
    let name = match &resumed {
        Some(loaded) => resumed_identity(&loaded.header, &cwd, &mut warnings),
        None => args
            .identity
            .clone()
            .unwrap_or_else(|| identity::DEFAULT.to_string()),
    };
    let Setup {
        prompt,
        policy,
        delegation,
        limits,
        judge,
        title: titled,
        choice,
        statusline,
    } = load(args.flags, &name).await?;
    // `--workflow` answers every approval but the workflow's own with no, so a call the
    // rules leave at `Ask` is refused for good. The agent is told that rather than told
    // to ask someone who is not there.
    let policy = match args.workflow.is_some() {
        true => policy.unattended(),
        false => policy,
    };
    let hub = prompt.mcp.clone();
    let identity = prompt.identity.clone();
    let client = client::Client::new(&choice)?;
    let defaults = (client.model().to_string(), client.effort().to_string());
    let mut client = client.with_overrides(identity.model.clone(), identity.effort.clone());
    // A resumed session goes back on the model it was last on: that is what its cached
    // prefix and its encrypted reasoning belong to. `--model` still wins over both.
    if let Some(loaded) = &resumed {
        client = client
            .with_overrides(Some(loaded.model.clone()), Some(loaded.effort.clone()))
            .with_session(&loaded.header.session);
    }
    // On a model that takes effort updates, the history's last one is the effort in
    // force, and `--effort` becomes the next one rather than a new request effort.
    let updates = client::takes_effort_updates(client.model())
        && args
            .model
            .as_ref()
            .is_none_or(|model| model == client.model());
    let client = match (&resumed, updates) {
        (Some(loaded), true) => {
            let announced = client::announced_effort(&loaded.items).map(str::to_string);
            client.with_effort_in_force(args.effort.clone().or(announced))
        }
        (None, true) => client.with_overrides(None, args.effort.clone()),
        (_, false) => client.with_overrides(args.model.clone(), args.effort.clone()),
    };
    let client = client
        .strict_cache(args.strict_cache)
        .with_window(limits.window)
        .log_headers(
            args.profile
                .then(|| profile::debug_dir().join("headers.jsonl")),
        );
    // Fail before taking over the terminal if the backend cannot serve the model.
    if let Err(e) = client.preflight().await {
        shutdown(hub.clone()).await;
        eprintln!("bhai: {e:#}");
        std::process::exit(1);
    }
    let saved = match resumed {
        Some(loaded) => {
            // Only `--model` can put a resumed session on another model now, so this is
            // the user being told what they asked for.
            if (loaded.model.as_str(), loaded.effort.as_str()) != (client.model(), client.effort())
            {
                warnings.push(format!(
                    "warning: the session ran on {} ({}), now {} ({}), so the cached prefix will differ",
                    loaded.model,
                    loaded.effort,
                    client.model(),
                    client.effort()
                ));
            }
            warnings.push(format!(
                "resumed session {} ({} items) on {} ({})",
                loaded.header.session,
                loaded.items.len(),
                client.model(),
                client.effort_in_force()
            ));
            warnings.extend(loaded.warnings.iter().map(|w| format!("warning: {w}")));
            Saved {
                writer: sessions::Writer::resume(&dir, &loaded)?,
                history: loaded.items,
            }
        }
        None => {
            let header = sessions::Header::new(
                client.session_id(),
                &identity.name,
                client.model(),
                client.effort(),
                &cwd,
            );
            Saved {
                writer: sessions::Writer::create(&dir, header),
                history: Vec::new(),
            }
        }
    };
    let mut notices = prompt.notices();
    notices.extend(warnings);
    if let Some(Err(e)) = statusline.as_deref().map(statusline::Template::parse) {
        notices.push(format!(
            "statusline in the global config does not parse, so the built-in bar is shown: {e}"
        ));
    }
    if args.trust {
        notices.push(policy.trust()?);
    }
    // The trust question, put to the user in the tui when the store does not know this
    // project. Until it is answered the session is in `ask`, whatever the config asked
    // for, since `auto` and `bypass` run code the project supplies.
    let held_back = policy.mode() != policy.wanted();
    let interactive = !args.headless && args.workflow.is_none();
    let trust_gate = (held_back && interactive).then(|| app::TrustGate {
        root: cwd.display().to_string(),
        rules: policy.repo_rules(),
        mode: policy.wanted(),
    });
    if trust_gate.is_none() {
        notices.extend(policy.trust_notice());
    }
    // Nothing will ask, so say why the mode is not the one that was asked for.
    if held_back && !interactive {
        notices.push(format!(
            "{} mode needs this project trusted; running in {}. Start with --trust to \
allow it.",
            policy.wanted(),
            policy.mode()
        ));
    }
    // A project file replacing `general` is silent otherwise, and what it says is the
    // system prompt of every turn.
    if identity.name != identity::DEFAULT || identity.source.starts_with("./") {
        notices.insert(
            0,
            format!("identity: {} ({})", identity.name, identity.source),
        );
    }
    let skills = prompt.skills.clone();
    let workflows = workflow::discover(&instructions::Roots::from_env(cwd.clone()));
    notices.extend(workflows.errors.iter().cloned());

    // Bind before taking over the terminal so a busy port is a plain error.
    let listener = match args.serve {
        Some(port) => match server::bind(port).await {
            Ok(listener) => Some(listener),
            Err(e) => {
                shutdown(hub).await;
                return Err(e);
            }
        },
        None => None,
    };
    let usage_log = args
        .profile
        .then(|| profile::debug_dir().join("usage.jsonl"));
    let history = saved.history.clone();
    let session_id = saved.writer.header.session.clone();
    let ollama_url = client.ollama_url().to_string();
    // `/statusline <request>` is one small call, on the judge's model like the title's.
    let designer: Arc<dyn statusline::Design> = Arc::new(statusline::ModelDesigner::new(
        client.clone(),
        judge.model.clone(),
    ));
    let (session, events) = start(
        client,
        prompt,
        policy,
        judge,
        // Only the terminal shows a title, so a run with no terminal does not pay for
        // one: `--headless` and `--workflow` both end without ever drawing a frame.
        titled && !args.headless && args.workflow.is_none(),
        usage_log,
        delegation,
        saved,
        limits,
    );
    // `--workflow` is a run of its own: no TUI, no turn, just the steps and their report.
    if let Some((name, input)) = args.workflow.clone() {
        for notice in &notices {
            eprintln!("bhai: {notice}");
        }
        let result = match workflow::find(&workflows.workflows, &name) {
            Ok(found) => headless_workflow(&session, found, input, args.workflow_yes).await,
            Err(e) => Err(e),
        };
        shutdown(hub).await;
        return result;
    }
    if args.headless {
        let listener = listener.expect("--headless is only accepted with --serve");
        session.entries().restore(&history);
        for notice in &notices {
            eprintln!("bhai: {notice}");
        }
        let token = server::mint();
        eprintln!(
            "bhai: debug server on http://{} ({}: {token})",
            listener.local_addr()?,
            server::TOKEN_HEADER
        );
        let result = server::serve(listener, session, token).await;
        shutdown(hub).await;
        return result;
    }

    let terminal = ratatui::init();
    // The terminal keeps the title it had, for as far as it is willing to put it back.
    title::push();
    // Mouse capture is what turns the wheel into scroll events. It also takes over
    // click-drag, so terminals need shift (or option) held to select text while bhai runs.
    let mouse = execute!(std::io::stdout(), EnableMouseCapture).is_ok();
    let paste = execute!(std::io::stdout(), EnableBracketedPaste).is_ok();
    // Disambiguated keys are how a terminal reports shift+enter; not all of them can.
    let keyboard = terminal::supports_keyboard_enhancement().unwrap_or(false)
        && execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok();
    // ratatui's own panic hook only leaves raw mode and the alternate screen. A crash is
    // exactly when the id is wanted, so the hint goes out after the panic message.
    let hook = std::panic::take_hook();
    let crashed = (dir.clone(), session_id.clone());
    std::panic::set_hook(Box::new(move |info| {
        release_modes(mouse, paste, keyboard);
        title::pop();
        hook(info);
        if let Some(hint) = sessions::exit_hint(&crashed.0, &crashed.1) {
            eprint!("{hint}");
        }
    }));
    let prompts = input::History::load(
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/bhai/history.jsonl")),
    );
    let result = run(
        terminal,
        mouse,
        session,
        events,
        listener,
        notices,
        skills,
        hub.clone(),
        &history,
        prompts,
        workflows,
        trust_gate,
        ollama_url,
        session_id.clone(),
        limits,
        statusline,
        designer,
        defaults,
    )
    .await;
    release_modes(mouse, paste, keyboard);
    title::pop();
    ratatui::restore();
    shutdown(hub).await;
    if let Some(hint) = sessions::exit_hint(&dir, &session_id) {
        eprint!("{hint}");
    }
    result
}

/// Turn mouse capture on or off while running, for `/mouse`; returns the state it left.
fn set_capture(on: bool) -> bool {
    let done = match on {
        true => execute!(std::io::stdout(), EnableMouseCapture).is_ok(),
        false => execute!(std::io::stdout(), DisableMouseCapture).is_ok(),
    };
    // A command that did not run leaves capture as it was.
    done == on
}

/// Turn off the terminal modes the TUI turned on.
fn release_modes(mouse: bool, paste: bool, keyboard: bool) {
    if keyboard {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    if paste {
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    }
    if mouse {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
}

/// Stop the MCP servers, if any were started.
async fn shutdown(hub: Option<Arc<mcp::Hub>>) {
    if let Some(hub) = hub {
        hub.shutdown().await;
    }
}

/// Command-line flags, apart from `--probe` and `--cache-check`.
#[derive(Debug, Default, PartialEq)]
struct Args {
    /// Port for the debug server, when `--serve` is given.
    serve: Option<u16>,
    headless: bool,
    /// Log every model call's usage to `.bhai/debug/usage.jsonl`.
    profile: bool,
    /// Refuse to send a request that breaks the prompt cache.
    strict_cache: bool,
    /// `--as`: the identity to run as.
    identity: Option<String>,
    /// `--model`: the model this session talks to, over the config and the identity.
    model: Option<String>,
    /// `--effort`: the reasoning effort, likewise.
    effort: Option<String>,
    /// Honour the repo-supplied allow rules as they are now.
    trust: bool,
    /// `--resume [id]`: continue a saved session, the latest when no id is given.
    resume: Option<Option<String>>,
    /// `--workflow <name> [input]`: run one workflow without the TUI.
    workflow: Option<(String, String)>,
    /// Answer the workflow confirmation with yes; without it the plan is only printed.
    workflow_yes: bool,
    flags: Flags,
}

/// Everything a session is built from once the config, the identity and the project's
/// instructions have been read.
struct Setup {
    prompt: SystemPrompt,
    policy: Policy,
    delegation: Delegation,
    limits: Limits,
    judge: judge::Settings,
    /// `title`: whether the session is named for the terminal's title.
    title: bool,
    /// The model the config asks for, before the identity and the flags have their say.
    choice: client::Choice,
    /// `statusline`: the status bar's template, which may not parse.
    statusline: Option<String>,
}

/// Config and instruction files for the working directory, as the system prompt for
/// the identity called `name`, the permission policy, and what child agents need.
/// Starts the MCP servers.
async fn load(flags: Flags, name: &str) -> Result<Setup> {
    let cwd = std::env::current_dir()?;
    let roots = instructions::Roots::from_env(cwd);
    let config = Config::load(roots.home.as_deref(), &roots.cwd)?.with_flags(flags);
    // Named once for the process: code already on screen is not repainted.
    if let Some(theme) = &config.code_theme {
        syntax::set_theme(theme);
    }
    let identities = identity::discover(&roots);
    let identity = identity::find(&identities, name)?;
    let hub = mcp::start(&config, &roots, &identity).await;
    let mut prompt = identity::build(&config, &roots, &identity, &identities).with_mcp(hub.clone());
    let bhai = roots.cwd.join(".bhai");
    let delegation = Delegation {
        identities,
        sessions: bhai.join("sessions"),
        cache_root: bhai,
        // Replaced with the session's own once there is a session to type into.
        mailboxes: agent::Mailboxes::default(),
        // Children reuse the session's MCP connections, narrowed to their identity.
        prompt: {
            let (config, roots) = (config.clone(), roots.clone());
            Arc::new(move |child: &identity::Identity| {
                let narrowed = hub.as_ref().map(|hub| Arc::new(hub.narrowed(child)));
                identity::build(&config, &roots, child, &[]).with_mcp(narrowed)
            })
        },
    };
    let limits = config.limits;
    let judge = config.judge.clone();
    let titled = config.title;
    let choice = config.choice.clone();
    let statusline = config.statusline.clone();
    let (policy, notices) = permissions(config, roots.home, roots.cwd);
    prompt.skipped.extend(notices);
    Ok(Setup {
        prompt,
        policy,
        delegation,
        limits,
        judge,
        title: titled,
        choice,
        statusline,
    })
}

/// The policy from the config, Claude Code's settings and remembered approvals, plus
/// notices about rules that were skipped.
fn permissions(config: Config, home: Option<PathBuf>, cwd: PathBuf) -> (Policy, Vec<String>) {
    let mut rules = config.permissions;
    let mut notices = Vec::new();
    if config.import_claude_permissions {
        let (claude, skipped) = permissions::settings::claude(home.as_deref(), &cwd);
        rules.extend(claude);
        notices.extend(skipped);
    }
    let store = cwd.join(permissions::settings::LOCAL);
    let (remembered, skipped) = permissions::settings::load_local(&store);
    rules.allow.extend(remembered);
    notices.extend(skipped);
    let trust = home
        .as_ref()
        .map(|home| permissions::Trust::new(&home.join(".config/bhai"), &cwd))
        .map(|trust| trust.with_claude(config.import_claude_permissions));
    let relax = permissions::Relax {
        writes: config.auto_project_writes,
        commands: config.auto_project_commands,
    };
    let mut policy = Policy::new(config.permission_mode, rules, home, cwd)
        .with_store(store)
        .with_relax(relax)
        .with_log(profile::debug_dir().join("permissions.jsonl"));
    if let Some(trust) = trust {
        policy = policy.with_trust(trust);
    }
    (policy, notices)
}

/// The identity a resumed session runs as: its own, or the default with a warning
/// when that no longer exists.
fn resumed_identity(
    header: &sessions::Header,
    cwd: &std::path::Path,
    warnings: &mut Vec<String>,
) -> String {
    let roots = instructions::Roots::from_env(cwd.to_path_buf());
    if identity::discover(&roots)
        .iter()
        .any(|i| i.name == header.identity)
    {
        return header.identity.clone();
    }
    warnings.push(format!(
        "warning: identity `{}` no longer exists, resuming as `{}`; the cached prefix will differ",
        header.identity,
        identity::DEFAULT
    ));
    identity::DEFAULT.to_string()
}

/// `bhai identities`: every identity with its baseline context cost.
fn identities() -> Result<()> {
    let roots = instructions::Roots::from_env(std::env::current_dir()?);
    let config = Config::load(roots.home.as_deref(), &roots.cwd)?;
    print!(
        "{}",
        identity::report(&identity::discover(&roots), &config, &roots)
    );
    Ok(())
}

fn parse_args(args: &[String]) -> Result<Args> {
    let mut parsed = Args::default();
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--serve" => {
                let port = args.next_if(|a| !a.starts_with("--"));
                parsed.serve = Some(match port {
                    Some(port) => port
                        .parse()
                        .map_err(|_| anyhow::anyhow!("bad port `{port}`"))?,
                    None => server::DEFAULT_PORT,
                });
            }
            "--headless" => parsed.headless = true,
            "--profile" => parsed.profile = true,
            "--strict-cache" => parsed.strict_cache = true,
            "--trust" => parsed.trust = true,
            "--mode" => {
                let mode = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--mode needs a value"))?;
                parsed.flags.mode = Some(mode.parse().map_err(anyhow::Error::msg)?);
            }
            "--model" => {
                let name = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--model needs a model name"))?;
                parsed.model = Some(name.clone());
            }
            "--effort" => {
                let level = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--effort needs a level"))?;
                parsed.effort = Some(level.clone());
            }
            "--as" => {
                let name = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--as needs an identity name"))?;
                parsed.identity = Some(name.clone());
            }
            "--resume" => {
                parsed.resume = Some(args.next_if(|a| !a.starts_with("--")).cloned());
            }
            "--workflow" => {
                let name = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--workflow needs a workflow name"))?;
                let input = args.next_if(|a| !a.starts_with("--")).cloned();
                parsed.workflow = Some((name.clone(), input.unwrap_or_default()));
            }
            "--workflow-yes" => parsed.workflow_yes = true,
            "--no-global" => parsed.flags.no_global = true,
            "--no-project" => parsed.flags.no_project = true,
            "--bare" => parsed.flags.bare = true,
            other => bail!("unknown argument `{other}`"),
        }
    }
    if parsed.headless && parsed.serve.is_none() {
        bail!("--headless needs --serve");
    }
    if parsed.workflow.is_some() && (parsed.serve.is_some() || parsed.headless) {
        bail!("--workflow runs on its own, so it takes no --serve or --headless");
    }
    if parsed.resume.is_some() && parsed.identity.is_some() {
        bail!("--resume keeps the session's identity, so it takes no --as");
    }
    Ok(parsed)
}

/// Spawn the agent behind a session. The returned receiver is subscribed before the
/// agent starts, so the TUI sees every event.
#[allow(clippy::too_many_arguments)]
fn start(
    client: client::Client,
    prompt: SystemPrompt,
    policy: Policy,
    settings: judge::Settings,
    titled: bool,
    usage_log: Option<PathBuf>,
    delegation: Delegation,
    saved: Saved,
    limits: Limits,
) -> (Arc<Session>, broadcast::Receiver<session::Event>) {
    let (tx_user, rx_user) = mpsc::channel::<String>(16);
    let (tx_control, rx_control) = mpsc::channel::<Control>(16);
    let (tx_agent, rx_agent) = mpsc::unbounded_channel::<AgentEvent>();
    let cancel = Arc::new(agent::Cancel::default());
    let policy = Arc::new(policy);
    // Built whatever the mode is, since `shift+tab` cycles into `auto` mid-session; the
    // policy is what decides that a call may reach it at all.
    let settings_model = settings.model.clone();
    let judge = settings.on.then(|| {
        let backend = Arc::new(ModelJudge::new(client.clone(), &settings));
        let root = std::env::current_dir().unwrap_or_default();
        Arc::new(
            Judge::new(backend, root, settings).with_log(profile::debug_dir().join("judge.jsonl")),
        )
    });
    // One small call names the session for the terminal's title. It is not the judge's
    // model: the cheapest one on the backend can write four words.
    let namer: Option<Arc<dyn title::Name>> = titled.then(|| {
        Arc::new(title::ModelNamer::new(
            client.clone(),
            settings_model.clone(),
            "low",
        )) as Arc<dyn title::Name>
    });
    let session = Session::new(
        client.model().to_string(),
        client.effort_in_force().to_string(),
        prompt.identity.name.clone(),
        tx_user,
        tx_control,
        Arc::clone(&cancel),
        Arc::clone(&policy),
        judge.clone(),
    );
    let events = session.subscribe();
    // The session hands out the mailboxes, so a message typed into a child's pane
    // reaches the child the agent is running.
    let delegation = Delegation {
        mailboxes: session.mailboxes(),
        ..delegation
    };
    // A panic in the loop or in a tool would otherwise end the task with the session still
    // marked working: no error, no `TurnEnd`, and a spinner that never stops. The panic
    // itself is reported by the hook; this is what lets the session say so and come back.
    // The windows and the credits before the first call, and then every minute while
    // the session sits idle, since otherwise only a model call brings them. A call
    // that fetched lately claims the slot, so the two never both ask.
    if client.provider() == client::Provider::Codex {
        let tx = tx_agent.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(limits::REFRESH);
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                every.tick().await;
                if !limits::due(chrono::Utc::now().timestamp()) {
                    continue;
                }
                if let Ok(body) = limits::fetch_now().await
                    && let Some(found) =
                        limits::RateLimits::from_usage(&body, chrono::Utc::now().timestamp())
                    && tx.send(AgentEvent::RateLimits(found)).is_err()
                {
                    break;
                }
            }
        });
    }
    let watch = tx_agent.clone();
    let loop_task = tokio::spawn(agent::run(
        client,
        prompt,
        policy,
        judge,
        namer,
        rx_user,
        rx_control,
        tx_agent,
        cancel,
        usage_log,
        Some(delegation),
        Some(saved),
        limits,
    ));
    tokio::spawn(session::watch(loop_task, watch));
    tokio::spawn(session::pump(Arc::clone(&session), rx_agent));
    (session, events)
}

/// Run one workflow without the TUI: print the plan, then each step and the report.
/// The confirmation is answered by `--workflow-yes`; any other approval is rejected,
/// since nothing is there to answer it.
async fn headless_workflow(
    session: &Arc<Session>,
    workflow: Arc<workflow::Workflow>,
    input: String,
    yes: bool,
) -> Result<()> {
    let mut events = session.subscribe();
    session
        .workflow(workflow, input)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        };
        match event {
            session::Event::Approval {
                id, tool, command, ..
            } => {
                let confirmation = tool == workflow::TOOL;
                let accept = yes && confirmation;
                if !accept {
                    println!("[rejected] {command}");
                }
                if confirmation && !yes {
                    println!("bhai: pass --workflow-yes to run it");
                }
                session.answer(
                    match accept {
                        true => permissions::Answer::Accept(None),
                        false => permissions::Answer::Reject,
                    },
                    Some(id),
                );
            }
            session::Event::Info(message) => println!("{message}"),
            session::Event::ToolStart { summary, .. } => println!("$ {summary}"),
            session::Event::ToolOutput(output) => println!("{output}"),
            session::Event::Error(message) => eprintln!("[error] {message}"),
            session::Event::TurnEnd => break,
            _ => {}
        }
    }
    Ok(())
}

/// Drive the real agent loop without the TUI, rejecting every command. Checks auth,
/// the wire format and the tool-result replay path without executing anything, so it
/// runs in `ask` mode with no rules whatever the config says.
async fn probe(setup: Setup, prompt: Option<String>) -> Result<()> {
    let system = setup.prompt;
    let (tx_user, rx_user) = mpsc::channel::<String>(1);
    let (_tx_control, rx_control) = mpsc::channel::<Control>(1);
    let (tx_agent, mut rx_agent) = mpsc::unbounded_channel::<AgentEvent>();
    let cancel = Arc::new(agent::Cancel::default());
    let client = client::Client::new(&setup.choice)?
        .with_overrides(
            system.identity.model.clone(),
            system.identity.effort.clone(),
        )
        .with_window(setup.limits.window);
    client.preflight().await?;
    tokio::spawn(agent::run(
        client,
        system,
        Arc::new(Policy::default()),
        None,
        None,
        rx_user,
        rx_control,
        tx_agent,
        Arc::clone(&cancel),
        None,
        None,
        None,
        Limits::default(),
    ));

    tx_user
        .send(prompt.unwrap_or_else(|| "Reply with just: ok".to_string()))
        .await?;

    while let Some(event) = rx_agent.recv().await {
        match event {
            AgentEvent::Text(delta) => {
                print!("{delta}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            AgentEvent::Reasoning(_) => {}
            AgentEvent::Approval { command, reply, .. } => {
                println!("\n[would run] {command}\n[probe rejects it]");
                let _ = reply.send(permissions::Answer::Reject);
            }
            AgentEvent::ToolStart { summary, .. } => println!("\n$ {summary}"),
            AgentEvent::ToolOutput(output) => println!("{output}"),
            AgentEvent::ToolRejected {
                summary,
                by,
                reason,
                ..
            } => match reason.is_empty() {
                true => println!("[{}] {summary}", by.label()),
                false => println!("[{}] {summary} ({reason})", by.label()),
            },
            AgentEvent::Info(message) => println!("[info] {message}"),
            AgentEvent::Compacted {
                notice, summary, ..
            } => {
                println!("[info] {notice}");
                if let Some(summary) = summary {
                    println!("{summary}");
                }
            }
            AgentEvent::Cleared => println!("[info] history cleared"),
            AgentEvent::Effort(effort) => println!("[info] effort {effort}"),
            AgentEvent::Usage(u) => println!(
                "\n[usage] input={} cached={} output={} reasoning={}",
                u.input, u.cached, u.output, u.reasoning
            ),
            AgentEvent::ChildUsage(u) => {
                println!("\n[child usage] input={} output={}", u.input, u.output)
            }
            AgentEvent::ChildStarted {
                id, description, ..
            } => println!("\n[child {id}] {description}"),
            AgentEvent::ChildEnded { id, ok } => {
                println!("[child {id}] {}", if ok { "done" } else { "failed" })
            }
            // A probe has no panes, so what a child says is flattened under its id.
            AgentEvent::Child { id, event } => match *event {
                AgentEvent::ToolStart { summary, .. } => println!("[child {id}] $ {summary}"),
                AgentEvent::ToolOutput(output) => println!("[child {id}] {output}"),
                AgentEvent::Error(message) => println!("[child {id}] [error] {message}"),
                _ => {}
            },
            AgentEvent::Cache(Some(found)) => {
                println!("\n[cache break] {}: {}", found.field, found.detail)
            }
            AgentEvent::Judging(Some(call)) => println!("[judging] {call}"),
            AgentEvent::Resumed(what) => println!("\n[resumed] {what}"),
            AgentEvent::Titled(name) => println!("[title] {name}"),
            // A probe prints what the model says, and the prompt behind it is a tui
            // readout: the size is already in the usage it prints when the call ends.
            AgentEvent::Sending(_)
            | AgentEvent::Cache(None)
            | AgentEvent::Call(_)
            | AgentEvent::Item(_)
            | AgentEvent::Judging(None)
            | AgentEvent::Streaming(_)
            | AgentEvent::ToolProgress(_) => {}
            AgentEvent::CacheHit(hit) => {
                if let (Some(expected), Some(ratio)) = (hit.expected_cached, hit.hit_ratio) {
                    println!("[cache] expected={expected} hit={:.0}%", ratio * 100.0);
                }
            }
            AgentEvent::CacheStalled(misses) => {
                println!("[cache] missed {misses} calls in a row")
            }
            AgentEvent::RateLimits(limits) => {
                for w in limits.windows() {
                    println!("[rate limit] {} {:.0}%", w.label(), w.used_percent);
                }
            }
            AgentEvent::Error(message) | AgentEvent::TurnFailed(message) => {
                println!("\n[error] {message}")
            }
            AgentEvent::TurnEnd => break,
        }
    }
    Ok(())
}

/// Calls `--cache-check` makes on one prefix.
const CACHE_CHECK_CALLS: usize = 3;
/// The estimated prefix `--cache-check` pads up to, with a margin over the cache minimum.
const CACHE_CHECK_PREFIX: u64 = cache::MIN_CACHED * 3 / 2;
const FILLER: &str = "This line only pads the prompt past the prompt cache minimum.\n";

/// One `--cache-check` call's result.
struct CacheRow {
    usage: client::Usage,
    hit: cache::Hit,
}

/// Send a few tiny calls with the session's real instructions and tools through the
/// real client, and report how much of each the cache served. `false` when call 2 or
/// later got nothing from the cache. With `wait`, one more call is sent that long after
/// the others and judged even past `CACHE_TTL`, to measure how long the backend keeps it.
async fn cache_check(setup: Setup, wait: Option<Duration>) -> Result<bool> {
    let (system, policy, delegation) = (setup.prompt, setup.policy, setup.delegation);
    let client = client::Client::new(&setup.choice)?
        .with_overrides(
            system.identity.model.clone(),
            system.identity.effort.clone(),
        )
        .with_window(setup.limits.window);
    client.preflight().await?;
    // Only the Codex backend reports how much of the input its cache served, so on any
    // other one there is nothing to measure and three live calls would prove nothing.
    if client.provider() != client::Provider::Codex {
        println!(
            "cache-check: nothing to check, the {} backend reports no cached count.",
            client.provider().name()
        );
        return Ok(true);
    }
    let cancel = Arc::new(agent::Cancel::default());
    // The same tool list a session offers, the agent tool included; it is never run.
    let mut registry = tools::Registry::for_prompt(&system);
    if system.identity.allows_tool(tools::agent::NAME) {
        registry = registry.with_agent(tools::agent::Agent {
            transcripts: delegation.sessions.clone(),
            delegation,
            model: Arc::new(client.clone()),
            policy: Arc::new(policy),
            tx: mpsc::unbounded_channel().0,
            cancel: Arc::clone(&cancel),
            children: agent::Children::default(),
            judge: None,
            results: mpsc::unbounded_channel().0,
            slots: Arc::new(tokio::sync::Semaphore::new(tools::agent::MAX_RUNNING)),
        });
        registry = registry.with_models(tools::models::Models {
            current: Arc::new(client.clone()),
        });
    }
    let tools = registry.schemas();
    // The cache matches on the prefix whatever the key, so without a prefix of its own a
    // waited call could find one that a concurrent check, or a session, kept warm.
    let instructions = match wait {
        Some(_) => format!("cache-check {}\n{}", uuid::Uuid::new_v4(), system.text),
        None => system.text.clone(),
    };
    let mut input = cache_check_prefix(&system, &tools);
    let mut monitor = cache::CacheMonitor::default();
    let mut rows = Vec::new();
    let calls = CACHE_CHECK_CALLS + usize::from(wait.is_some());
    for call in 1..=calls {
        let waited = wait.filter(|_| call > CACHE_CHECK_CALLS);
        if let Some(wait) = waited {
            println!("waiting {} minutes", wait.as_secs() / 60);
            tokio::time::sleep(wait).await;
        }
        input.push(user_message(&format!(
            "Call {call} of {calls}. Reply with just: ok"
        )));
        let mut usage = None;
        let mut on_delta = |delta: client::Delta| match delta {
            client::Delta::Cache(found) => {
                if let Some(found) = &found {
                    println!("[cache break] {}: {}", found.field, found.detail);
                }
                monitor.sent(found.as_ref(), std::time::Instant::now());
            }
            client::Delta::Usage(u) => usage = Some(u),
            _ => {}
        };
        let items = client
            .respond(&instructions, &tools, &input, &mut on_delta, &cancel.flag())
            .await?;
        let usage = usage.ok_or_else(|| anyhow::anyhow!("call {call} reported no usage"))?;
        let mut hit = monitor.observe(&usage, std::time::Instant::now());
        if waited.is_some() {
            let expected = rows
                .last()
                .map(|row: &CacheRow| cache::expected_cached(row.usage.input));
            hit = cache::Hit {
                expected_cached: expected,
                hit_ratio: expected.map(|e| usage.cached as f64 / e as f64),
            };
        }
        rows.push(CacheRow { usage, hit });
        input.extend(items);
    }
    print!("{}", cache_table(&rows));
    Ok(cache_check_passed(&rows))
}

/// The input `--cache-check` starts from: empty, or one filler message when the
/// instructions and tools are estimated under `CACHE_CHECK_PREFIX` tokens.
fn cache_check_prefix(
    system: &SystemPrompt,
    tools: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let estimated = profile::build(system, tools, &[], &[], &tokens::ByteEstimate).estimated_tokens;
    let Some(missing) = CACHE_CHECK_PREFIX.checked_sub(estimated).filter(|m| *m > 0) else {
        return Vec::new();
    };
    let lines = (missing * 4).div_ceil(FILLER.len() as u64) as usize;
    vec![user_message(&FILLER.repeat(lines))]
}

fn user_message(text: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "message",
        "role": "user",
        "content": [{ "type": "input_text", "text": text }],
    })
}

fn cache_table(rows: &[CacheRow]) -> String {
    let mut table = format!(
        "{:>4} {:>8} {:>8} {:>8} {:>6}  verdict\n",
        "call", "input", "cached", "expected", "ratio"
    );
    for (i, row) in rows.iter().enumerate() {
        let (expected, ratio) = match (row.hit.expected_cached, row.hit.hit_ratio) {
            (Some(e), Some(r)) => (e.to_string(), format!("{:.0}%", r * 100.0)),
            _ => ("-".to_string(), "-".to_string()),
        };
        let verdict = if i == 0 {
            "first"
        } else if row.hit.hit_ratio.is_none() {
            "not judged"
        } else if row.hit.miss() {
            "MISS"
        } else {
            "hit"
        };
        table.push_str(&format!(
            "{:>4} {:>8} {:>8} {:>8} {:>6}  {verdict}\n",
            i + 1,
            row.usage.input,
            row.usage.cached,
            expected,
            ratio
        ));
    }
    table
}

/// The cases `--judge-eval` reads when it is given no file.
const JUDGE_CASES: &str = "tests/fixtures/judge-cases.jsonl";

/// Run a file of cases through the real judge, exactly as the approval path decides on
/// it, and score every verdict against what the case expected. `false` when any case
/// came back wrong, so the command's exit status is the score.
async fn judge_eval(setup: Setup, path: Option<String>) -> Result<bool> {
    let (system, settings) = (setup.prompt, setup.judge);
    let path = PathBuf::from(path.unwrap_or_else(|| JUDGE_CASES.to_string()));
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cases = judge::cases(&text)?;
    if cases.is_empty() {
        bail!("no cases in {}", path.display());
    }
    let client = client::Client::new(&setup.choice)?
        .with_overrides(
            system.identity.model.clone(),
            system.identity.effort.clone(),
        )
        .with_window(setup.limits.window);
    client.preflight().await?;
    let backend = ModelJudge::new(client, &settings);
    let outcomes = judge::eval(&backend, &cases, settings.timeout).await;
    print!("{}", judge::report(&outcomes));
    Ok(outcomes.iter().all(judge::Outcome::correct))
}

fn cache_check_passed(rows: &[CacheRow]) -> bool {
    rows.iter().skip(1).all(|row| row.usage.cached > 0)
}

#[allow(clippy::too_many_arguments)]
async fn run(
    mut terminal: ratatui::DefaultTerminal,
    mouse: bool,
    session: Arc<Session>,
    mut events: broadcast::Receiver<session::Event>,
    listener: Option<TcpListener>,
    notices: Vec<String>,
    skills: Vec<skills::Skill>,
    hub: Option<Arc<mcp::Hub>>,
    history: &[serde_json::Value],
    prompts: input::History,
    workflows: workflow::Found,
    trust_gate: Option<app::TrustGate>,
    ollama_url: String,
    session_id: String,
    limits: Limits,
    statusline: Option<String>,
    designer: Arc<dyn statusline::Design>,
    defaults: (String, String),
) -> Result<()> {
    let (tx_event, mut rx_event) = mpsc::unbounded_channel::<Event>();

    // Terminal input lives on its own thread; crossterm's reader is blocking.
    let input_tx = tx_event.clone();
    std::thread::spawn(move || {
        loop {
            let event = match event::poll(TICK) {
                Ok(true) => match event::read() {
                    Ok(TermEvent::Key(key)) => Event::Key(key),
                    Ok(TermEvent::Mouse(mouse)) => Event::Mouse(mouse),
                    Ok(TermEvent::Paste(text)) => Event::Paste(text),
                    Ok(_) => Event::Tick,
                    Err(_) => break,
                },
                Ok(false) => Event::Tick,
                Err(_) => break,
            };
            if input_tx.send(event).is_err() {
                break;
            }
        }
    });

    let forward_tx = tx_event.clone();
    tokio::spawn(async move {
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if forward_tx.send(Event::Resync).is_err() {
                        break;
                    }
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            if forward_tx.send(Event::Session(event)).is_err() {
                break;
            }
        }
    });

    let mut app = App::new(Arc::clone(&session));
    app.mouse = mouse;
    app.skills = skills;
    app.mcp = hub;
    app.workflows = workflows;
    app.history = prompts;
    app.trust_gate = trust_gate;
    app.ollama_url = ollama_url;
    app.session_id = session_id;
    app.limits = limits;
    // One that does not parse was already reported with the notices.
    app.statusline = statusline.and_then(|t| statusline::Template::parse(&t).ok());
    app.config_path = std::env::var_os("HOME").map(|home| config::global_path(home.as_ref()));
    app.defaults = defaults;
    app.designer = Some(designer);
    {
        let mut entries = app.entries();
        entries.restore(history);
        entries
            .list
            .extend(notices.into_iter().map(app::Entry::Info));
    }
    if let Some(listener) = listener {
        let addr = listener.local_addr()?;
        let token = server::mint();
        app.entries().push(app::Entry::Info(format!(
            "debug server on http://{addr} ({}: {token})",
            server::TOKEN_HEADER
        )));
        tokio::spawn(server::serve(listener, session, token));
    }
    // The tab says where the session is running until the model says what it is doing.
    let root = std::env::current_dir().unwrap_or_default();
    title::set(&title::compose(&root, None));
    // Mouse motion arrives in floods, so it only redraws when the hover changes.
    let mut dirty = true;
    let mut captured = mouse;
    while !app.quit {
        if dirty {
            terminal.draw(|frame| ui::render(frame, &mut app))?;
        }
        let Some(event) = rx_event.recv().await else {
            break;
        };
        dirty = apply(&mut app, event, &root);
        // Whatever else is already waiting is applied before the next draw. A streaming
        // turn sends an event per delta, and a frame per delta is a frame wasted:
        // rendering the transcript costs the same however little of it changed.
        while !app.quit {
            let Ok(event) = rx_event.try_recv() else {
                break;
            };
            dirty |= apply(&mut app, event, &root);
        }
        // `/mouse` hands the pointer back to the terminal, and takes it again.
        if app.mouse != captured {
            captured = set_capture(app.mouse);
        }
    }

    // Leave capture where `release_modes` expects to find it.
    if captured != mouse {
        set_capture(mouse);
    }
    Ok(())
}

/// Apply one event to the view; returns whether it has to be drawn again.
fn apply(app: &mut App, event: Event, root: &std::path::Path) -> bool {
    match event {
        Event::Key(key) => {
            app.on_key(key);
            true
        }
        Event::Mouse(mouse) => app.on_mouse(mouse),
        Event::Paste(text) => {
            app.on_paste(&text);
            true
        }
        Event::Session(event) => {
            // The terminal is written to from the thread that draws it, never from the
            // task that named the session.
            if let session::Event::Titled(name) = &event {
                title::set(&title::compose(root, Some(name)));
            }
            app.on_event(event);
            true
        }
        Event::Resync => {
            app.resync();
            true
        }
        Event::Tick => {
            app.tick();
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The draw loop applies a whole batch and then draws once, so `apply` says whether
    /// the view changed and the batch takes the union.
    #[test]
    fn a_batch_of_events_is_one_redraw() {
        let mut app = App::detached();
        let root = std::path::PathBuf::new();
        let batch = vec![
            Event::Session(session::Event::User("go".to_string())),
            Event::Session(session::Event::Text("hi".to_string())),
            Event::Tick,
            Event::Session(session::Event::TurnEnd),
        ];
        let dirty = batch
            .into_iter()
            .map(|event| apply(&mut app, event, &root))
            .fold(false, |dirty, next| dirty | next);
        // One draw covers the batch, and every event in it has already been applied.
        assert!(dirty);
        assert!(!app.working);
    }

    fn parse(args: &[&str]) -> Result<(Option<u16>, bool)> {
        parse_args(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
            .map(|a| (a.serve, a.headless))
    }

    fn row(input: u64, cached: u64, expected: Option<u64>) -> CacheRow {
        CacheRow {
            usage: client::Usage {
                input,
                cached,
                ..client::Usage::default()
            },
            hit: cache::Hit {
                expected_cached: expected,
                hit_ratio: expected.map(|e| cached as f64 / e as f64),
            },
        }
    }

    #[test]
    fn cache_check_table_and_verdict() {
        let rows = [
            row(1500, 0, None),
            row(1520, 1408, Some(1408)),
            row(1540, 128, Some(1408)),
        ];
        let table = cache_table(&rows);
        let lines: Vec<_> = table.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[1].ends_with("  first"), "{table}");
        assert!(lines[2].contains(" 100%  hit"), "{table}");
        assert!(lines[3].contains("  9%  MISS"), "{table}");
        assert!(cache_check_passed(&rows));
        assert!(!cache_check_passed(&[
            row(1500, 0, None),
            row(1520, 0, Some(1408))
        ]));
    }

    #[test]
    fn cache_check_pads_a_short_prefix_past_the_minimum() {
        let system = prompt::system_prompt(&[], Vec::new());
        let short = SystemPrompt {
            text: "Be brief.".to_string(),
            ..system
        };
        let input = cache_check_prefix(&short, &[]);
        assert_eq!(input.len(), 1);
        let padded =
            profile::build(&short, &[], &input, &[], &tokens::ByteEstimate).estimated_tokens;
        assert!(padded >= CACHE_CHECK_PREFIX, "{padded}");

        let tools = tools::Registry::for_prompt(&short).schemas();
        let long = SystemPrompt {
            text: FILLER.repeat(200),
            ..short
        };
        assert!(cache_check_prefix(&long, &tools).is_empty());
    }

    #[test]
    fn serve_flags() {
        assert_eq!(parse(&[]).unwrap(), (None, false));
        assert_eq!(parse(&["--serve"]).unwrap(), (Some(7878), false));
        assert_eq!(parse(&["--serve", "9000"]).unwrap(), (Some(9000), false));
        assert_eq!(
            parse(&["--serve", "--headless"]).unwrap(),
            (Some(7878), true)
        );
        assert_eq!(
            parse(&["--serve", "--profile"]).unwrap(),
            (Some(7878), false)
        );
        assert!(parse(&["--headless"]).is_err());
        assert!(parse(&["--serve", "nope"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
    }

    #[test]
    fn instruction_flags() {
        let flags = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            parse_args(&args).unwrap().flags
        };
        assert_eq!(flags(&[]), Flags::default());
        assert!(flags(&["--no-global"]).no_global);
        assert!(flags(&["--no-project"]).no_project);
        let bare = flags(&["--bare", "--profile"]);
        assert!(bare.bare && !bare.no_global);
    }

    #[test]
    fn model_flags() {
        let picked = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            parse_args(&args).map(|a| (a.model, a.effort))
        };
        assert_eq!(picked(&[]).unwrap(), (None, None));
        assert_eq!(
            picked(&["--model", "ollama:gemma4:e2b", "--effort", "low"]).unwrap(),
            (
                Some("ollama:gemma4:e2b".to_string()),
                Some("low".to_string())
            )
        );
        assert!(picked(&["--model"]).is_err());
        assert!(picked(&["--effort"]).is_err());
    }

    #[test]
    fn mode_flag() {
        let mode = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            parse_args(&args).map(|a| a.flags.mode)
        };
        assert_eq!(mode(&[]).unwrap(), None);
        assert_eq!(
            mode(&["--mode", "auto"]).unwrap(),
            Some(permissions::Mode::Auto)
        );
        assert!(mode(&["--mode"]).is_err());
        assert!(mode(&["--mode", "yolo"]).is_err());
    }

    #[test]
    fn as_flag() {
        let identity = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            parse_args(&args).map(|a| a.identity)
        };
        assert_eq!(identity(&[]).unwrap(), None);
        assert_eq!(
            identity(&["--as", "router"]).unwrap().as_deref(),
            Some("router")
        );
        assert!(identity(&["--as"]).is_err());
    }

    #[test]
    fn resume_flag() {
        let resume = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            parse_args(&args).map(|a| a.resume)
        };
        assert_eq!(resume(&[]).unwrap(), None);
        assert_eq!(resume(&["--resume"]).unwrap(), Some(None));
        assert_eq!(
            resume(&["--resume", "abc", "--profile"]).unwrap(),
            Some(Some("abc".to_string()))
        );
        assert!(resume(&["--resume", "--as", "router"]).is_err());
    }

    #[test]
    fn profile_flag() {
        let args = ["--profile"].map(String::from);
        assert!(parse_args(&args).unwrap().profile);
        assert!(!parse_args(&[]).unwrap().profile);
    }

    #[test]
    fn strict_cache_flag() {
        let args = ["--strict-cache"].map(String::from);
        assert!(parse_args(&args).unwrap().strict_cache);
        assert!(!parse_args(&[]).unwrap().strict_cache);
    }
}
