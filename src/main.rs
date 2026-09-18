//! bhai: a very small coding agent. A few tools, one loop, one approval prompt.
//! Inference runs through the Codex CLI's ChatGPT-subscription credentials.

mod agent;
mod app;
mod auth;
mod cache;
mod client;
mod clipboard;
mod compact;
mod config;
mod diff;
mod entries;
mod frontmatter;
mod identity;
mod input;
mod instructions;
mod limits;
mod markdown;
mod mcp;
mod permissions;
mod profile;
mod prompt;
mod server;
mod session;
mod sessions;
mod skills;
mod tokens;
mod tools;
mod ui;
mod workflow;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Result, bail};
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
        print!("{}", sessions::report(&sessions::list(&dir)));
        return Ok(());
    }

    // Fail before taking over the terminal if there is nothing to authenticate with.
    let http = reqwest::Client::new();
    if let Err(e) = auth::load(&http).await {
        eprintln!("bhai: {e:#}");
        std::process::exit(1);
    }

    // `bhai --probe [prompt]` does one non-interactive model call, for checking that
    // auth and the wire format still work without entering the TUI.
    if args.first().is_some_and(|a| a == "--probe") {
        let (prompt, _, _, _) = load(Flags::default(), identity::DEFAULT).await?;
        let hub = prompt.mcp.clone();
        let result = probe(prompt, args.get(1).cloned()).await;
        shutdown(hub).await;
        return result;
    }
    // `bhai --cache-check` sends a few calls on one prefix and checks the cache served it.
    if args.first().is_some_and(|a| a == "--cache-check") {
        let (prompt, policy, delegation, _) = load(Flags::default(), identity::DEFAULT).await?;
        let hub = prompt.mcp.clone();
        let result = cache_check(prompt, policy, delegation).await;
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
                "bhai: {e:#}\nusage: bhai [identities] [sessions] [--probe [prompt]] [--cache-check] [--as <identity>] [--resume [id]] [--workflow <name> [input] [--workflow-yes]] [--serve [port] [--headless]] [--profile] [--strict-cache] [--mode ask|auto|bypass] [--trust] [--no-global] [--no-project] [--bare]"
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
    let (prompt, policy, delegation, limits) = load(args.flags, &name).await?;
    let hub = prompt.mcp.clone();
    let identity = prompt.identity.clone();
    let mut client =
        client::Client::new()?.with_overrides(identity.model.clone(), identity.effort.clone());
    if let Some(loaded) = &resumed {
        client = client.with_session(&loaded.header.session);
    }
    let client = client.strict_cache(args.strict_cache).log_headers(
        args.profile
            .then(|| profile::debug_dir().join("headers.jsonl")),
    );
    let saved = match resumed {
        Some(loaded) => {
            let header = &loaded.header;
            if (header.model.as_str(), header.effort.as_str()) != (client.model(), client.effort())
            {
                warnings.push(format!(
                    "warning: the session ran on {} ({}), now {} ({}), so the cached prefix will differ",
                    header.model,
                    header.effort,
                    client.model(),
                    client.effort()
                ));
            }
            warnings.push(format!(
                "resumed session {} ({} items)",
                header.session,
                loaded.items.len()
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
    if args.trust {
        notices.push(policy.trust()?);
    }
    notices.extend(policy.trust_notice());
    if identity.name != identity::DEFAULT {
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
    let (session, events) = start(client, prompt, policy, usage_log, delegation, saved, limits);
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
        eprintln!("bhai: debug server on http://{}", listener.local_addr()?);
        let result = server::serve(listener, session).await;
        shutdown(hub).await;
        return result;
    }

    let terminal = ratatui::init();
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
    // ratatui's own panic hook only leaves raw mode and the alternate screen.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        release_modes(mouse, paste, keyboard);
        hook(info);
    }));
    let prompts = input::History::load(
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/bhai/history.jsonl")),
    );
    let result = run(
        terminal,
        session,
        events,
        listener,
        notices,
        skills,
        hub.clone(),
        &history,
        prompts,
        workflows,
    )
    .await;
    release_modes(mouse, paste, keyboard);
    ratatui::restore();
    shutdown(hub).await;
    result
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

/// Config and instruction files for the working directory, as the system prompt for
/// the identity called `name`, the permission policy, and what child agents need.
/// Starts the MCP servers.
async fn load(flags: Flags, name: &str) -> Result<(SystemPrompt, Policy, Delegation, Limits)> {
    let cwd = std::env::current_dir()?;
    let roots = instructions::Roots::from_env(cwd);
    let config = Config::load(roots.home.as_deref(), &roots.cwd)?.with_flags(flags);
    let identities = identity::discover(&roots);
    let identity = identity::find(&identities, name)?;
    let hub = mcp::start(&config, &roots, &identity).await;
    let mut prompt = identity::build(&config, &roots, &identity, &identities).with_mcp(hub.clone());
    let delegation = Delegation {
        identities,
        sessions: roots.cwd.join(".bhai").join("sessions"),
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
    let (policy, notices) = permissions(config, roots.home, roots.cwd);
    prompt.skipped.extend(notices);
    Ok((prompt, policy, delegation, limits))
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
    let mut policy = Policy::new(config.permission_mode, rules, home, cwd).with_store(store);
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
fn start(
    client: client::Client,
    prompt: SystemPrompt,
    policy: Policy,
    usage_log: Option<PathBuf>,
    delegation: Delegation,
    saved: Saved,
    limits: Limits,
) -> (Arc<Session>, broadcast::Receiver<session::Event>) {
    let (tx_user, rx_user) = mpsc::channel::<String>(16);
    let (tx_control, rx_control) = mpsc::channel::<Control>(16);
    let (tx_agent, rx_agent) = mpsc::unbounded_channel::<AgentEvent>();
    let cancel = Arc::new(AtomicBool::new(false));
    let policy = Arc::new(policy);
    let session = Session::new(
        client.model().to_string(),
        prompt.identity.name.clone(),
        tx_user,
        tx_control,
        Arc::clone(&cancel),
        Arc::clone(&policy),
    );
    let events = session.subscribe();
    tokio::spawn(agent::run(
        client,
        prompt,
        policy,
        rx_user,
        rx_control,
        tx_agent,
        cancel,
        usage_log,
        Some(delegation),
        Some(saved),
        limits,
    ));
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
            session::Event::ToolStart(command) => println!("$ {command}"),
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
async fn probe(system: SystemPrompt, prompt: Option<String>) -> Result<()> {
    let (tx_user, rx_user) = mpsc::channel::<String>(1);
    let (_tx_control, rx_control) = mpsc::channel::<Control>(1);
    let (tx_agent, mut rx_agent) = mpsc::unbounded_channel::<AgentEvent>();
    let cancel = Arc::new(AtomicBool::new(false));
    let client = client::Client::new()?.with_overrides(
        system.identity.model.clone(),
        system.identity.effort.clone(),
    );
    tokio::spawn(agent::run(
        client,
        system,
        Arc::new(Policy::default()),
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
            AgentEvent::ToolStart(command) => println!("\n$ {command}"),
            AgentEvent::ToolOutput(output) => println!("{output}"),
            AgentEvent::ToolRejected(command) => println!("[rejected] {command}"),
            AgentEvent::Info(message) | AgentEvent::Compacted(message) => {
                println!("[info] {message}")
            }
            AgentEvent::Usage(u) => println!(
                "\n[usage] input={} cached={} output={} reasoning={}",
                u.input, u.cached, u.output, u.reasoning
            ),
            AgentEvent::ChildUsage(u) => {
                println!("\n[child usage] input={} output={}", u.input, u.output)
            }
            AgentEvent::Cache(Some(found)) => {
                println!("\n[cache break] {}: {}", found.field, found.detail)
            }
            AgentEvent::Cache(None)
            | AgentEvent::Call(_)
            | AgentEvent::Item(_)
            | AgentEvent::ToolProgress(_) => {}
            AgentEvent::CacheHit(hit) => {
                if let (Some(expected), Some(ratio)) = (hit.expected_cached, hit.hit_ratio) {
                    println!("[cache] expected={expected} hit={:.0}%", ratio * 100.0);
                }
            }
            AgentEvent::RateLimits(limits) => {
                for w in limits.windows() {
                    println!("[rate limit] {} {:.0}%", w.label(), w.used_percent);
                }
            }
            AgentEvent::Error(message) => println!("\n[error] {message}"),
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
/// later got nothing from the cache.
async fn cache_check(system: SystemPrompt, policy: Policy, delegation: Delegation) -> Result<bool> {
    let client = client::Client::new()?.with_overrides(
        system.identity.model.clone(),
        system.identity.effort.clone(),
    );
    let cancel = Arc::new(AtomicBool::new(false));
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
            slots: Arc::new(tokio::sync::Semaphore::new(tools::agent::MAX_RUNNING)),
        });
    }
    let tools = registry.schemas();
    let mut input = cache_check_prefix(&system, &tools);
    let mut monitor = cache::CacheMonitor::default();
    let mut rows = Vec::new();
    for call in 1..=CACHE_CHECK_CALLS {
        input.push(user_message(&format!(
            "Call {call} of {CACHE_CHECK_CALLS}. Reply with just: ok"
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
            .respond(&system.text, &tools, &input, &mut on_delta, &cancel)
            .await?;
        let usage = usage.ok_or_else(|| anyhow::anyhow!("call {call} reported no usage"))?;
        let hit = monitor.observe(&usage, std::time::Instant::now());
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

fn cache_check_passed(rows: &[CacheRow]) -> bool {
    rows.iter().skip(1).all(|row| row.usage.cached > 0)
}

#[allow(clippy::too_many_arguments)]
async fn run(
    mut terminal: ratatui::DefaultTerminal,
    session: Arc<Session>,
    mut events: broadcast::Receiver<session::Event>,
    listener: Option<TcpListener>,
    notices: Vec<String>,
    skills: Vec<skills::Skill>,
    hub: Option<Arc<mcp::Hub>>,
    history: &[serde_json::Value],
    prompts: input::History,
    workflows: workflow::Found,
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
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            };
            if forward_tx.send(Event::Session(event)).is_err() {
                break;
            }
        }
    });

    let mut app = App::new(Arc::clone(&session));
    app.skills = skills;
    app.mcp = hub;
    app.workflows = workflows;
    app.history = prompts;
    {
        let mut entries = app.entries();
        entries.restore(history);
        entries
            .list
            .extend(notices.into_iter().map(app::Entry::Info));
    }
    if let Some(listener) = listener {
        let addr = listener.local_addr()?;
        app.entries()
            .push(app::Entry::Info(format!("debug server on http://{addr}")));
        tokio::spawn(server::serve(listener, session));
    }
    // Mouse motion arrives in floods, so it only redraws when the hover changes.
    let mut dirty = true;
    while !app.quit {
        if dirty {
            terminal.draw(|frame| ui::render(frame, &mut app))?;
        }
        let Some(event) = rx_event.recv().await else {
            break;
        };
        dirty = match event {
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
                app.on_event(event);
                true
            }
            Event::Tick => {
                app.tick();
                true
            }
        };
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
