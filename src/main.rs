//! bhai: a very small coding agent. One tool (bash), one loop, one approval prompt.
//! Inference runs through the Codex CLI's ChatGPT-subscription credentials.

mod agent;
mod app;
mod auth;
mod bash;
mod client;
mod prompt;
mod ui;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as TermEvent, KeyEvent, MouseEvent,
};
use ratatui::crossterm::execute;
use tokio::sync::mpsc;

use crate::agent::AgentEvent;
use crate::app::App;

/// How often the UI wakes up when nothing is happening (keeps the spinner moving).
const TICK: Duration = Duration::from_millis(120);

enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Agent(AgentEvent),
    Tick,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Fail before taking over the terminal if there is nothing to authenticate with.
    let http = reqwest::Client::new();
    if let Err(e) = auth::load(&http).await {
        eprintln!("bhai: {e:#}");
        std::process::exit(1);
    }
    let model = client::Client::new()?.model().to_string();

    // `bhai --probe [prompt]` does one non-interactive model call, for checking that
    // auth and the wire format still work without entering the TUI.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--probe") {
        return probe(args.get(1).cloned()).await;
    }

    let terminal = ratatui::init();
    // Mouse capture is what turns the wheel into scroll events. It also takes over
    // click-drag, so terminals need shift (or option) held to select text while bhai runs.
    let mouse = execute!(std::io::stdout(), EnableMouseCapture).is_ok();
    let result = run(terminal, model).await;
    if mouse {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();
    result
}

/// Drive the real agent loop without the TUI, rejecting every command. Checks auth,
/// the wire format and the tool-result replay path without executing anything.
async fn probe(prompt: Option<String>) -> Result<()> {
    let (tx_user, rx_user) = mpsc::channel::<String>(1);
    let (tx_agent, mut rx_agent) = mpsc::unbounded_channel::<AgentEvent>();
    let cancel = Arc::new(AtomicBool::new(false));
    tokio::spawn(agent::run(rx_user, tx_agent, Arc::clone(&cancel)));

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
            AgentEvent::Approval { command, reply } => {
                println!("\n[would run] {command}\n[probe rejects it]");
                let _ = reply.send(false);
            }
            AgentEvent::ToolStart(command) => println!("\n$ {command}"),
            AgentEvent::ToolOutput(output) => println!("{output}"),
            AgentEvent::ToolRejected(command) => println!("[rejected] {command}"),
            AgentEvent::Usage { input, output } => {
                println!("\n[usage] input={input} output={output}");
            }
            AgentEvent::Error(message) => println!("\n[error] {message}"),
            AgentEvent::TurnEnd => break,
        }
    }
    Ok(())
}

async fn run(mut terminal: ratatui::DefaultTerminal, model: String) -> Result<()> {
    let (tx_event, mut rx_event) = mpsc::unbounded_channel::<Event>();
    let (tx_user, rx_user) = mpsc::channel::<String>(16);
    let (tx_agent, mut rx_agent) = mpsc::unbounded_channel::<AgentEvent>();
    let cancel = Arc::new(AtomicBool::new(false));

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

    let agent = tokio::spawn(agent::run(rx_user, tx_agent, Arc::clone(&cancel)));

    let forward_tx = tx_event.clone();
    tokio::spawn(async move {
        while let Some(event) = rx_agent.recv().await {
            if forward_tx.send(Event::Agent(event)).is_err() {
                break;
            }
        }
    });

    let mut app = App::new(model, tx_user, cancel);
    while !app.quit {
        terminal.draw(|frame| ui::render(frame, &mut app))?;
        let Some(event) = rx_event.recv().await else {
            break;
        };
        match event {
            Event::Key(key) => app.on_key(key),
            Event::Mouse(mouse) => app.on_mouse(mouse),
            Event::Agent(event) => app.on_agent(event),
            Event::Tick => app.tick(),
        }
    }

    agent.abort();
    Ok(())
}
