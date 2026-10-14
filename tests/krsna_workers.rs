//! Host-only fake processes, not guest containment or a production worker adapter.
//! No built-in executable tools, workflow controls, listeners or real backends.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc as pipe};
use std::thread;
use std::time::{Duration, Instant};

use bhai::agent::Saved;
use bhai::client::Delta;
use bhai::identity::Identity;
use bhai::permissions::{Mode, Rules};
use bhai::sessions::{self, Header, Writer};
use bhai::tools::BoxFuture;
use bhai::{AgentEvent, Cancel, Model, Policy, Registry, Roots, Runtime, SystemPrompt, Tool};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tokio::time::{sleep, timeout};

const WAIT: Duration = Duration::from_secs(10);
const LINE_LIMIT: u64 = 4096;
const OUTPUT_LIMIT: usize = 64 * 1024;

struct Script {
    id: String,
    instructions: String,
    calls: AtomicUsize,
    cancelled: AtomicBool,
    completed: AtomicBool,
    commands: Mutex<mpsc::Receiver<String>>,
}

impl Model for Script {
    fn respond<'a>(
        &'a self,
        instructions: &'a str,
        tools: &'a [Value],
        input: &'a [Value],
        on_delta: &'a mut (dyn FnMut(Delta) + Send),
        cancel: &'a Arc<AtomicBool>,
    ) -> BoxFuture<'a, anyhow::Result<Vec<Value>>> {
        Box::pin(async move {
            assert_eq!(instructions, self.instructions);
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0]["name"], "fixture_identity");
            match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => Ok(vec![json!({
                    "type": "function_call", "name": "fixture_identity",
                    "call_id": format!("{}-call", self.id), "arguments": "{}"
                })]),
                1 => {
                    assert!(input.iter().any(|item| {
                        item["type"] == "function_call_output" && item["output"] == self.id
                    }));
                    on_delta(Delta::Text(format!("{}:live", self.id)));
                    let mut commands = self.commands.lock().await;
                    loop {
                        if cancel.load(Ordering::Relaxed) {
                            self.cancelled.store(true, Ordering::SeqCst);
                            anyhow::bail!("fixture cancelled");
                        }
                        tokio::select! {
                            command = commands.recv() => match command.as_deref() {
                                Some("progress") => on_delta(Delta::Text(format!("{}:progress", self.id))),
                                Some("release") => {
                                    self.completed.store(true, Ordering::SeqCst);
                                    on_delta(Delta::Text(format!("{}:done", self.id)));
                                    return Ok(vec![json!({
                                        "type": "message", "role": "assistant",
                                        "content": [{"type": "output_text", "text": format!("{}:done", self.id)}]
                                    })]);
                                }
                                _ => panic!("unexpected model command"),
                            },
                            _ = sleep(Duration::from_millis(5)) => {}
                        }
                    }
                }
                _ => panic!("unexpected model call"),
            }
        })
    }

    fn child(&self, _identity: &Identity) -> Arc<dyn Model> {
        panic!("delegation is disabled");
    }

    fn name(&self) -> &str {
        &self.id
    }

    fn reports_cache(&self) -> bool {
        false
    }
}

struct FixtureIdentity {
    id: String,
    calls: Arc<AtomicUsize>,
}

impl Tool for FixtureIdentity {
    fn name(&self) -> &str {
        "fixture_identity"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function", "name": self.name(), "description": "Report fixture identity",
            "parameters": {"type": "object", "properties": {}, "additionalProperties": false}
        })
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        if args == &json!({}) {
            Ok(self.id.clone())
        } else {
            Err("fixture takes no arguments".to_string())
        }
    }

    fn execute<'a>(&'a self, _args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            assert_eq!(self.calls.fetch_add(1, Ordering::SeqCst), 0);
            (self.id.clone(), true)
        })
    }
}

fn emit(value: Value) {
    let line = serde_json::to_string(&value).unwrap();
    assert!(line.len() < LINE_LIMIT as usize);
    let mut stderr = std::io::stderr().lock();
    writeln!(stderr, "{line}").unwrap();
    stderr.flush().unwrap();
}

// Only a child selected by the parent starts a runtime.
#[tokio::test]
async fn worker_fixture() {
    let Some(id) = std::env::var_os("KRSNA_WORKER_FIXTURE") else {
        return;
    };
    let id = id.into_string().unwrap();
    assert!(matches!(id.as_str(), "alpha" | "beta"));
    timeout(WAIT, run_fixture(id))
        .await
        .expect("worker fixture timed out");
}

async fn run_fixture(id: String) {
    let roots = Roots {
        home: Some(PathBuf::from(std::env::var_os("HOME").unwrap())),
        codex_home: Some(PathBuf::from(std::env::var_os("CODEX_HOME").unwrap())),
        cwd: std::env::current_dir().unwrap(),
    };
    let loaded = bhai::instructions::load(&bhai::config::Config::default(), &roots);
    assert!(loaded.skipped.is_empty());
    assert_eq!(loaded.files.len(), 3);
    let instructions = loaded
        .files
        .iter()
        .map(|file| file.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    for source in ["home", "codex", "project"] {
        assert!(instructions.contains(&format!("{id}-{source}")));
    }
    let other = if id == "alpha" { "beta" } else { "alpha" };
    assert!(!instructions.contains(other));
    let session = format!("{id}-session");
    let transcripts = roots.cwd.join("transcripts");
    let transcript = sessions::path(&transcripts, &session);
    let (model_commands, rx_model) = mpsc::channel(1);
    let model = Arc::new(Script {
        id: id.clone(),
        instructions: instructions.clone(),
        calls: AtomicUsize::new(0),
        cancelled: AtomicBool::new(false),
        completed: AtomicBool::new(false),
        commands: Mutex::new(rx_model),
    });
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let mut runtime = Runtime::new(
        model.clone(),
        &session,
        SystemPrompt {
            text: instructions.clone(),
            instructions: loaded.files,
            ..Default::default()
        },
        Arc::new(Policy::new(
            Mode::Ask,
            Rules::default(),
            roots.home.clone(),
            roots.cwd.clone(),
        )),
    );
    assert!(runtime.delegation.is_none());
    runtime.registry = Some(Arc::new({
        let id = id.clone();
        let calls = Arc::clone(&tool_calls);
        move |_, _| {
            Registry::empty().with_tool(Box::new(FixtureIdentity {
                id: id.clone(),
                calls: Arc::clone(&calls),
            }))
        }
    }));
    runtime.saved = Some(Saved {
        writer: Writer::create(
            &transcripts,
            Header::new(&session, "fixture", &id, "", &roots.cwd),
        ),
        history: Vec::new(),
    });
    let (user, rx_user) = mpsc::channel(1);
    let (_control, rx_control) = mpsc::channel(1);
    // Runtime's event API is unbounded; this fixed script has a capped event consumer.
    let (events, mut rx_events) = mpsc::unbounded_channel();
    let cancel = Arc::new(Cancel::default());
    let task = tokio::spawn(runtime.run(rx_user, rx_control, events, Arc::clone(&cancel)));
    let (commands, mut rx_commands) = mpsc::channel(1);
    let reader = thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut input = stdin.lock();
        for _ in 0..8 {
            let mut line = String::new();
            let count = input.by_ref().take(32).read_line(&mut line).unwrap();
            if count == 0 {
                break;
            }
            assert!(line.ends_with('\n') && count < 32);
            if commands.blocking_send(line.trim().to_string()).is_err() {
                break;
            }
        }
    });
    emit(json!({"kind": "ready", "id": id, "pid": std::process::id(),
            "cwd": roots.cwd, "home": roots.home, "codex": roots.codex_home,
            "session": session, "transcript": transcript, "instructions": instructions}));
    user.send(format!("{id}-prompt").into()).await.unwrap();
    let mut event_count = 0;
    let mut commands_open = true;
    loop {
        tokio::select! {
            command = rx_commands.recv(), if commands_open => match command.as_deref() {
                Some("cancel") => cancel.stop(),
                Some("progress" | "release") => model_commands.send(command.unwrap()).await.unwrap(),
                None => commands_open = false,
                _ => panic!("unexpected worker command; workflow controls are not accepted"),
            },
            event = rx_events.recv() => {
                event_count += 1;
                assert!(event_count <= 64);
                match event.expect("runtime closed during turn") {
                    AgentEvent::Text(text) => emit(json!({"kind": "progress", "id": id, "text": text})),
                    AgentEvent::ToolOutput(text) => assert_eq!(text, id),
                    AgentEvent::TurnEnd => break,
                    AgentEvent::Approval { .. } => panic!("synthetic read-only tool asked for approval"),
                    AgentEvent::Error(error) | AgentEvent::TurnFailed(error) => panic!("runtime failed: {error}"),
                    _ => {}
                }
            }
        }
    }
    drop(user);
    task.await.unwrap();
    reader.join().unwrap();
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    let saved = sessions::load(&transcript).unwrap();
    assert!(saved.warnings.is_empty());
    assert_eq!(saved.header.session, session);
    assert_eq!(saved.header.cwd, roots.cwd);
    assert_eq!(saved.model, id);
    assert!(
        saved
            .items
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["output"] == id)
    );
    emit(
        json!({"kind": "finished", "id": id, "model_calls": model.calls.load(Ordering::SeqCst),
            "tool_calls": tool_calls.load(Ordering::SeqCst), "cancelled": model.cancelled.load(Ordering::SeqCst),
            "completed": model.completed.load(Ordering::SeqCst)}),
    );
}

struct Directory(PathBuf);

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    output: Option<pipe::Receiver<Value>>,
    reader: Option<thread::JoinHandle<()>>,
    deadline: Instant,
}

impl Worker {
    fn spawn(root: &Path, id: &str) -> Self {
        let cwd = root.join(id);
        let home = cwd.join("home");
        let codex = cwd.join("codex");
        for dir in [cwd.join(".git"), home.join(".claude"), codex.clone()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for (path, source) in [
            (cwd.join("AGENTS.md"), "project"),
            (home.join(".claude/CLAUDE.md"), "home"),
            (codex.join("AGENTS.md"), "codex"),
        ] {
            std::fs::write(path, format!("{id}-{source}")).unwrap();
        }
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "worker_fixture", "--nocapture", "--quiet"])
            .current_dir(&cwd)
            .env_clear()
            .env("HOME", &home)
            .env("CODEX_HOME", &codex)
            .env("TMPDIR", &cwd)
            .env("KRSNA_WORKER_FIXTURE", id)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let stderr = child.stderr.take().unwrap();
        let (output, rx) = pipe::sync_channel(16);
        let reader = thread::spawn(move || {
            let mut stderr = BufReader::new(stderr);
            let mut total = 0;
            loop {
                let mut line = Vec::new();
                let count = stderr
                    .by_ref()
                    .take(LINE_LIMIT + 1)
                    .read_until(b'\n', &mut line)
                    .unwrap();
                if count == 0 {
                    break;
                }
                total += count;
                assert!(count <= LINE_LIMIT as usize && total <= OUTPUT_LIMIT);
                assert_eq!(line.last(), Some(&b'\n'));
                let value =
                    serde_json::from_slice(&line).expect("worker emitted non-protocol output");
                if output.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            output: Some(rx),
            reader: Some(reader),
            deadline: Instant::now() + WAIT,
        }
    }

    fn receive(&self, kind: &str) -> Value {
        let value = self
            .output
            .as_ref()
            .unwrap()
            .recv_timeout(self.deadline.saturating_duration_since(Instant::now()))
            .expect("worker output timed out");
        assert_eq!(value["kind"], kind);
        value
    }

    fn send(&mut self, command: &str) {
        assert!(matches!(command, "progress" | "cancel" | "release"));
        writeln!(self.input.as_mut().unwrap(), "{command}").unwrap();
        self.input.as_mut().unwrap().flush().unwrap();
    }

    fn finish(&mut self) {
        self.input.take();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < self.deadline, "worker exit timed out");
            thread::sleep(Duration::from_millis(5));
        }
        self.output.take();
        self.reader.take().unwrap().join().unwrap();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.output.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[test]
fn independent_worker_processes_keep_roots_state_and_cancellation_separate() {
    let parent_cwd = std::env::current_dir().unwrap();
    let parent_home = std::env::var_os("HOME");
    let parent_codex = std::env::var_os("CODEX_HOME");
    let path = std::env::temp_dir().join(format!(
        "krsna-workers-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&path).unwrap();
    let directory = Directory(path.canonicalize().unwrap());
    let mut alpha = Worker::spawn(&directory.0, "alpha");
    let mut beta = Worker::spawn(&directory.0, "beta");
    let a = alpha.receive("ready");
    let b = beta.receive("ready");
    assert_ne!(a["pid"], b["pid"]);
    assert_eq!(a["pid"], alpha.child.id());
    assert_eq!(b["pid"], beta.child.id());
    assert_ne!(alpha.child.id(), std::process::id());
    assert_ne!(beta.child.id(), std::process::id());
    for (metadata, id) in [(&a, "alpha"), (&b, "beta")] {
        let cwd = directory.0.join(id);
        assert_eq!(metadata["id"], id);
        assert_eq!(metadata["cwd"], cwd.to_str().unwrap());
        assert_eq!(metadata["home"], cwd.join("home").to_str().unwrap());
        assert_eq!(metadata["codex"], cwd.join("codex").to_str().unwrap());
        assert_eq!(metadata["session"], format!("{id}-session"));
        assert_eq!(
            metadata["transcript"],
            sessions::path(&cwd.join("transcripts"), &format!("{id}-session"))
                .to_str()
                .unwrap()
        );
        for source in ["home", "codex", "project"] {
            assert!(
                metadata["instructions"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("{id}-{source}"))
            );
        }
    }
    assert_eq!(alpha.receive("progress")["text"], "alpha:live");
    assert_eq!(beta.receive("progress")["text"], "beta:live");
    // Both model calls remain live while progress alternates across the pipes.
    for _ in 0..2 {
        assert!(alpha.child.try_wait().unwrap().is_none());
        assert!(beta.child.try_wait().unwrap().is_none());
        alpha.send("progress");
        assert_eq!(alpha.receive("progress")["text"], "alpha:progress");
        beta.send("progress");
        assert_eq!(beta.receive("progress")["text"], "beta:progress");
    }
    alpha.send("cancel");
    alpha.input.take();
    let cancelled = alpha.receive("finished");
    assert_eq!(cancelled["cancelled"], true);
    assert_eq!(cancelled["completed"], false);
    alpha.finish();
    assert!(beta.child.try_wait().unwrap().is_none());
    beta.send("release");
    beta.input.take();
    assert_eq!(beta.receive("progress")["text"], "beta:done");
    let completed = beta.receive("finished");
    assert_eq!(completed["cancelled"], false);
    assert_eq!(completed["completed"], true);
    beta.finish();
    for (result, id) in [(&cancelled, "alpha"), (&completed, "beta")] {
        assert_eq!(result["id"], id);
        assert_eq!(result["model_calls"], 2);
        assert_eq!(result["tool_calls"], 1);
        let saved = sessions::load(&sessions::path(
            &directory.0.join(id).join("transcripts"),
            &format!("{id}-session"),
        ))
        .unwrap();
        assert!(saved.warnings.is_empty());
        assert_eq!(saved.header.cwd, directory.0.join(id));
        assert_eq!(saved.header.session, format!("{id}-session"));
        assert_eq!(saved.model, id);
        let text = serde_json::to_string(&saved.items).unwrap();
        let other = if id == "alpha" { "beta" } else { "alpha" };
        assert!(!text.contains(other));
        assert!(text.contains(&format!("{id}-prompt")));
        assert_eq!(text.contains(&format!("{id}:done")), id == "beta");
    }
    assert_eq!(std::env::current_dir().unwrap(), parent_cwd);
    assert_eq!(std::env::var_os("HOME"), parent_home);
    assert_eq!(std::env::var_os("CODEX_HOME"), parent_codex);
    drop(alpha);
    drop(beta);
    drop(directory);
    assert!(!path.exists());
}
