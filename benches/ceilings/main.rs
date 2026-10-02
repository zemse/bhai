//! Criterion benches for the paths that grow with a session, each held to the ceiling
//! `ceilings.toml` sets on its median. `cargo bench` fails when one runs past it, and
//! `cargo bench -- --test` runs each once without timing it.

#![allow(dead_code, unused_imports)]

mod check;

// bhai is a binary crate, so the bench compiles its modules itself, at the paths
// main.rs declares them; none of them reaches into main.rs.
#[path = "../../src/agent.rs"]
mod agent;
#[path = "../../src/app.rs"]
mod app;
#[path = "../../src/askpass.rs"]
mod askpass;
#[path = "../../src/auth.rs"]
mod auth;
#[path = "../../src/branch.rs"]
mod branch;
#[path = "../../src/cache.rs"]
mod cache;
#[path = "../../src/childenv.rs"]
mod childenv;
#[path = "../../src/client.rs"]
mod client;
#[path = "../../src/clipboard.rs"]
mod clipboard;
#[path = "../../src/commands.rs"]
mod commands;
#[path = "../../src/compact.rs"]
mod compact;
#[path = "../../src/config.rs"]
mod config;
#[path = "../../src/debug.rs"]
mod debug;
#[cfg(feature = "dictation")]
#[path = "../../src/dictation.rs"]
mod dictation;
#[path = "../../src/diff.rs"]
mod diff;
#[path = "../../src/egress.rs"]
mod egress;
#[path = "../../src/entries.rs"]
mod entries;
#[path = "../../src/environment.rs"]
mod environment;
#[path = "../../src/external.rs"]
mod external;
#[path = "../../src/files.rs"]
mod files;
#[path = "../../src/frontmatter.rs"]
mod frontmatter;
#[path = "../../src/goal.rs"]
mod goal;
#[path = "../../src/identity.rs"]
mod identity;
#[path = "../../src/images.rs"]
mod images;
#[path = "../../src/input.rs"]
mod input;
#[path = "../../src/instructions.rs"]
mod instructions;
#[path = "../../src/judge.rs"]
mod judge;
#[path = "../../src/limits.rs"]
mod limits;
#[path = "../../src/links.rs"]
mod links;
#[path = "../../src/markdown.rs"]
mod markdown;
#[path = "../../src/mcp/mod.rs"]
mod mcp;
#[path = "../../src/memory.rs"]
mod memory;
#[path = "../../src/mermaid.rs"]
mod mermaid;
#[path = "../../src/models.rs"]
mod models;
#[path = "../../src/notify.rs"]
mod notify;
#[path = "../../src/ollama.rs"]
mod ollama;
#[path = "../../src/palette.rs"]
mod palette;
#[path = "../../src/permissions/mod.rs"]
mod permissions;
#[path = "../../src/plan.rs"]
mod plan;
#[path = "../../src/profile.rs"]
mod profile;
#[path = "../../src/prompt.rs"]
mod prompt;
#[path = "../../src/redact.rs"]
mod redact;
#[path = "../../src/sandbox.rs"]
mod sandbox;
#[path = "../../src/schedule.rs"]
mod schedule;
#[path = "../../src/schedules.rs"]
mod schedules;
#[path = "../../src/search.rs"]
mod search;
#[path = "../../src/server.rs"]
mod server;
#[path = "../../src/session.rs"]
mod session;
#[path = "../../src/sessions.rs"]
mod sessions;
#[path = "../../src/skills.rs"]
mod skills;
#[path = "../../src/speed.rs"]
mod speed;
#[path = "../../src/startup.rs"]
mod startup;
#[path = "../../src/statusline.rs"]
mod statusline;
#[path = "../../src/syntax.rs"]
mod syntax;
#[path = "../../src/title.rs"]
mod title;
#[path = "../../src/tokens.rs"]
mod tokens;
#[path = "../../src/tools/mod.rs"]
mod tools;
#[path = "../../src/trace.rs"]
mod trace;
#[path = "../../src/ui.rs"]
mod ui;
#[path = "../../src/websocket.rs"]
mod websocket;
#[path = "../../src/workflow.rs"]
mod workflow;
#[path = "../../src/worktrees.rs"]
mod worktrees;
#[path = "../../src/wrap.rs"]
mod wrap;

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use criterion::{BatchSize, Criterion};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};

use crate::app::{App, Entry};
use crate::client::Usage;
use crate::profile::Call;
use crate::prompt::SystemPrompt;
use crate::session::Session;

/// Turns in the long transcript, seven entries each.
const TRANSCRIPT_TURNS: usize = 400;
const HISTORY_ITEMS: usize = 10_000;

fn main() {
    let ceilings = check::ceilings(check::CEILINGS).unwrap_or_else(|problem| {
        eprintln!("benches/ceilings/ceilings.toml: {problem}");
        std::process::exit(1);
    });
    let started = SystemTime::now();
    let mut c = Criterion::default().configure_from_args();
    render(&mut c);
    wrap_line(&mut c);
    profile_build(&mut c);
    c.final_summary();

    // Only medians this run wrote count: a filtered run, `cargo test` and `--list`
    // leave the others as old as they were, or absent.
    let home = criterion_home();
    let mut over = Vec::new();
    for id in check::BENCHES {
        let path = home.join(id).join("new/estimates.json");
        let fresh = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .is_ok_and(|modified| modified >= started);
        if !fresh {
            continue;
        }
        let median = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| check::median_micros(&text))
            .unwrap_or_else(|| panic!("no median in {}", path.display()));
        if let Some(line) = check::breach(id, median, ceilings[id]) {
            over.push(line);
        }
    }
    if !over.is_empty() {
        for line in &over {
            eprintln!("{line}");
        }
        std::process::exit(1);
    }
}

fn render(c: &mut Criterion) {
    let mut group = c.benchmark_group("render");
    group.sample_size(20);
    // The first frame of a resumed session: every entry is laid out once.
    group.bench_function("transcript_cold_120x40", |b| {
        b.iter_batched(
            || {
                (
                    transcript(),
                    Terminal::new(TestBackend::new(120, 40)).unwrap(),
                )
            },
            |(mut app, mut terminal)| {
                terminal.draw(|frame| ui::render(frame, &mut app)).unwrap();
                (app, terminal)
            },
            BatchSize::LargeInput,
        );
    });
    // Every frame after it, following the tail with the rows cached.
    let mut app = transcript();
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| ui::render(frame, &mut app)).unwrap();
    group.bench_function("transcript_tail_120x40", |b| {
        b.iter(|| {
            terminal
                .draw(|frame| ui::render(frame, &mut app))
                .unwrap()
                .area
        });
    });
    group.finish();
}

fn wrap_line(c: &mut Criterion) {
    let mut group = c.benchmark_group("wrap");
    group.sample_size(20);
    // A minified bundle or a log with no newlines, as a tool might print it.
    let word = "lorem ipsum dolor sit amet, ünïcödé 漢字 consectetur ";
    let line = word.repeat((1 << 20) / word.len());
    group.bench_function("line_1mib", |b| {
        b.iter(|| wrap::wrap(black_box(&line), 120).len());
    });
    group.finish();
}

fn profile_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("profile");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(8));
    let prompt = SystemPrompt {
        text: "You are bhai, a coding agent. ".repeat(200),
        ..SystemPrompt::default()
    };
    let tools: Vec<Value> = (0..20)
        .map(|i| json!({"type": "function", "name": format!("tool_{i}"), "parameters": {}}))
        .collect();
    let (history, calls) = history(HISTORY_ITEMS);
    group.bench_function("build_10k", |b| {
        b.iter(|| {
            profile::build(&prompt, &tools, &history, &calls, &tokens::O200k)
                .items
                .len()
        });
    });
    group.finish();
}

/// An app showing a long session: prompts, markdown answers with code, commands, their
/// output and diffs.
fn transcript() -> App {
    let (tx_user, _) = tokio::sync::mpsc::channel(1);
    let (tx_control, _) = tokio::sync::mpsc::channel(1);
    let session = Session::new(
        "gpt-5.5".to_string(),
        "medium".to_string(),
        "general".to_string(),
        tx_user,
        tx_control,
        Arc::default(),
        Arc::default(),
        None,
    );
    let app = App::new(session);
    {
        let mut entries = app.entries();
        for turn in 0..TRANSCRIPT_TURNS {
            entries.push(Entry::User(format!(
                "turn {turn}: fix the failing test in src/wrap.rs and explain why it broke"
            )));
            entries.push(Entry::Command {
                tool: "bash".to_string(),
                summary: "cargo test wrap".to_string(),
            });
            entries.push(Entry::Output(
                (0..30)
                    .map(|i| format!("test wrap::tests::case_{i} ... ok\n"))
                    .collect(),
            ));
            entries.push(Entry::Command {
                tool: "edit".to_string(),
                summary: "src/wrap.rs".to_string(),
            });
            entries.push(Entry::Diff(
                "@@ -10,4 +10,4 @@\n fn wrap() {\n-    let width = 0;\n+    let width = 1;\n }\n"
                    .to_string(),
            ));
            entries.push(Entry::Assistant(format!(
                "## Turn {turn}\n\nThe width was **zero**, so `split_at_width` never advanced. \
                 A [link](https://example.com) and a list:\n\n- one\n- two\n\n\
                 ```rust\nfn wrap(text: &str, width: usize) -> Vec<String> {{\n    \
                 joined(text, width.max(1)).into_iter().map(|(s, _)| s).collect()\n}}\n```\n"
            )));
            entries.push(Entry::Done(format!("{turn}s")));
        }
    }
    app
}

/// `items` history items in turns of user, call, output, answer, with the two model
/// calls each turn makes.
fn history(items: usize) -> (Vec<Value>, Vec<Call>) {
    let mut history = Vec::with_capacity(items);
    let mut calls = Vec::new();
    let usage = |input: u64| Usage {
        input,
        cached: input * 9 / 10,
        output: 80,
        ..Usage::default()
    };
    while history.len() + 4 <= items {
        let n = history.len();
        history.push(json!({"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": format!("turn {n}: run the tests")}]}));
        calls.push(Call {
            usage: usage(200 * n as u64 + 100),
            sent: n + 1,
            outputs: 1,
        });
        history.push(
            json!({"type": "function_call", "name": "bash", "call_id": format!("c{n}"),
            "arguments": "{\"command\":\"cargo test\"}"}),
        );
        history.push(
            json!({"type": "function_call_output", "call_id": format!("c{n}"),
            "output": "test result: ok. 412 passed; 0 failed\n".repeat(8)}),
        );
        calls.push(Call {
            usage: usage(200 * n as u64 + 300),
            sent: n + 3,
            outputs: 1,
        });
        history.push(json!({"type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": "All 412 tests pass."}]}));
    }
    (history, calls)
}

/// Where criterion writes, found the way it finds it.
fn criterion_home() -> PathBuf {
    if let Some(home) = std::env::var_os("CRITERION_HOME") {
        return PathBuf::from(home);
    }
    if let Some(target) = std::env::var_os("CARGO_TARGET_DIR") {
        return Path::new(&target).join("criterion");
    }
    let metadata = std::env::var_os("CARGO").and_then(|cargo| {
        let output = std::process::Command::new(cargo)
            .args(["metadata", "--format-version", "1", "--no-deps"])
            .output()
            .ok()?;
        let value: Value = serde_json::from_slice(&output.stdout).ok()?;
        Some(PathBuf::from(value["target_directory"].as_str()?))
    });
    metadata.map_or_else(
        || PathBuf::from("target/criterion"),
        |t| t.join("criterion"),
    )
}
