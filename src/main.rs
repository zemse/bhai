//! bhai: a very small coding agent. A few tools, one loop, one approval prompt.
//! Inference runs through the Codex CLI's ChatGPT-subscription credentials.

mod agent;
mod app;
mod auth;
mod client;
mod config;
mod frontmatter;
mod identity;
mod instructions;
mod mcp;
mod permissions;
mod profile;
mod prompt;
mod server;
mod session;
mod skills;
mod tools;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Result, bail};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as TermEvent, KeyEvent, MouseEvent,
};
use ratatui::crossterm::execute;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};

use crate::agent::{AgentEvent, Control, Delegation};
use crate::app::App;
use crate::config::{Config, Flags};
use crate::permissions::Policy;
use crate::prompt::SystemPrompt;
use crate::session::Session;

/// How often the UI wakes up when nothing is happening (keeps the spinner moving).
const TICK: Duration = Duration::from_millis(120);

enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Session(session::Event),
    Tick,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "identities") {
        return identities();
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
        let (prompt, _, _) = load(Flags::default(), identity::DEFAULT).await?;
        let hub = prompt.mcp.clone();
        let result = probe(prompt, args.get(1).cloned()).await;
        shutdown(hub).await;
        return result;
    }
    let args = match parse_args(&args) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!(
                "bhai: {e:#}\nusage: bhai [identities] [--probe [prompt]] [--as <identity>] [--serve [port] [--headless]] [--profile] [--mode ask|auto|bypass] [--no-global] [--no-project] [--bare]"
            );
            std::process::exit(2);
        }
    };
    let (prompt, policy, delegation) = load(
        args.flags,
        args.identity.as_deref().unwrap_or(identity::DEFAULT),
    )
    .await?;
    let hub = prompt.mcp.clone();
    let identity = prompt.identity.clone();
    let model = client::Client::new()?
        .with_overrides(identity.model.clone(), identity.effort.clone())
        .model()
        .to_string();
    let mut notices = prompt.notices();
    if identity.name != identity::DEFAULT {
        notices.insert(
            0,
            format!("identity: {} ({})", identity.name, identity.source),
        );
    }
    let skills = prompt.skills.clone();

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
    let (session, events) = start(model, prompt, policy, usage_log, delegation);
    if args.headless {
        let listener = listener.expect("--headless is only accepted with --serve");
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
    let result = run(
        terminal,
        session,
        events,
        listener,
        notices,
        skills,
        hub.clone(),
    )
    .await;
    if mouse {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();
    shutdown(hub).await;
    result
}

/// Stop the MCP servers, if any were started.
async fn shutdown(hub: Option<Arc<mcp::Hub>>) {
    if let Some(hub) = hub {
        hub.shutdown().await;
    }
}

/// Command-line flags, apart from `--probe`.
#[derive(Debug, Default, PartialEq)]
struct Args {
    /// Port for the debug server, when `--serve` is given.
    serve: Option<u16>,
    headless: bool,
    /// Log every model call's usage to `.bhai/debug/usage.jsonl`.
    profile: bool,
    /// `--as`: the identity to run as.
    identity: Option<String>,
    flags: Flags,
}

/// Config and instruction files for the working directory, as the system prompt for
/// the identity called `name`, the permission policy, and what child agents need.
/// Starts the MCP servers.
async fn load(flags: Flags, name: &str) -> Result<(SystemPrompt, Policy, Delegation)> {
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
    let (policy, notices) = permissions(config, roots.home, roots.cwd);
    prompt.skipped.extend(notices);
    Ok((prompt, policy, delegation))
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
    let policy = Policy::new(config.permission_mode, rules, home, cwd).with_store(store);
    (policy, notices)
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
            "--no-global" => parsed.flags.no_global = true,
            "--no-project" => parsed.flags.no_project = true,
            "--bare" => parsed.flags.bare = true,
            other => bail!("unknown argument `{other}`"),
        }
    }
    if parsed.headless && parsed.serve.is_none() {
        bail!("--headless needs --serve");
    }
    Ok(parsed)
}

/// Spawn the agent behind a session. The returned receiver is subscribed before the
/// agent starts, so the TUI sees every event.
fn start(
    model: String,
    prompt: SystemPrompt,
    policy: Policy,
    usage_log: Option<PathBuf>,
    delegation: Delegation,
) -> (Arc<Session>, broadcast::Receiver<session::Event>) {
    let (tx_user, rx_user) = mpsc::channel::<String>(16);
    let (tx_control, rx_control) = mpsc::channel::<Control>(16);
    let (tx_agent, rx_agent) = mpsc::unbounded_channel::<AgentEvent>();
    let cancel = Arc::new(AtomicBool::new(false));
    let policy = Arc::new(policy);
    let session = Session::new(
        model,
        prompt.identity.name.clone(),
        tx_user,
        tx_control,
        Arc::clone(&cancel),
        Arc::clone(&policy),
    );
    let events = session.subscribe();
    tokio::spawn(agent::run(
        prompt,
        policy,
        rx_user,
        rx_control,
        tx_agent,
        cancel,
        usage_log,
        Some(delegation),
    ));
    tokio::spawn(session::pump(Arc::clone(&session), rx_agent));
    (session, events)
}

/// Drive the real agent loop without the TUI, rejecting every command. Checks auth,
/// the wire format and the tool-result replay path without executing anything, so it
/// runs in `ask` mode with no rules whatever the config says.
async fn probe(system: SystemPrompt, prompt: Option<String>) -> Result<()> {
    let (tx_user, rx_user) = mpsc::channel::<String>(1);
    let (_tx_control, rx_control) = mpsc::channel::<Control>(1);
    let (tx_agent, mut rx_agent) = mpsc::unbounded_channel::<AgentEvent>();
    let cancel = Arc::new(AtomicBool::new(false));
    tokio::spawn(agent::run(
        system,
        Arc::new(Policy::default()),
        rx_user,
        rx_control,
        tx_agent,
        Arc::clone(&cancel),
        None,
        None,
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
            AgentEvent::Info(message) => println!("[info] {message}"),
            AgentEvent::Usage(u) => println!(
                "\n[usage] input={} cached={} output={} reasoning={}",
                u.input, u.cached, u.output, u.reasoning
            ),
            AgentEvent::ChildUsage(u) => {
                println!("\n[child usage] input={} output={}", u.input, u.output)
            }
            AgentEvent::Error(message) => println!("\n[error] {message}"),
            AgentEvent::TurnEnd => break,
        }
    }
    Ok(())
}

async fn run(
    mut terminal: ratatui::DefaultTerminal,
    session: Arc<Session>,
    mut events: broadcast::Receiver<session::Event>,
    listener: Option<TcpListener>,
    notices: Vec<String>,
    skills: Vec<skills::Skill>,
    hub: Option<Arc<mcp::Hub>>,
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
    app.entries
        .extend(notices.into_iter().map(app::Entry::Info));
    if let Some(listener) = listener {
        let addr = listener.local_addr()?;
        app.entries
            .push(app::Entry::Info(format!("debug server on http://{addr}")));
        tokio::spawn(server::serve(listener, session));
    }
    while !app.quit {
        terminal.draw(|frame| ui::render(frame, &mut app))?;
        let Some(event) = rx_event.recv().await else {
            break;
        };
        match event {
            Event::Key(key) => app.on_key(key),
            Event::Mouse(mouse) => app.on_mouse(mouse),
            Event::Session(event) => app.on_event(event),
            Event::Tick => app.tick(),
        }
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
    fn profile_flag() {
        let args = ["--profile"].map(String::from);
        assert!(parse_args(&args).unwrap().profile);
        assert!(!parse_args(&[]).unwrap().profile);
    }
}
