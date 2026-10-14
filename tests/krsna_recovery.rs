//! Public transcript resume and runtime summarisation on scripted models only.
//! Summary fidelity is scripted, not a durable job ledger or external-effect recovery.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bhai::agent::Saved;
use bhai::client::{Delta, Usage};
use bhai::compact::{self, Limits};
use bhai::identity::Identity;
use bhai::permissions::{Mode, Rules};
use bhai::sessions::{self, Header, Loaded, Writer};
use bhai::tools::BoxFuture;
use bhai::{AgentEvent, Cancel, Model, Policy, Registry, Roots, Runtime, SystemPrompt, Tool};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);
static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "krsna-recovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn roots(&self, id: &str) -> Roots {
        let cwd = self.0.join(id);
        let home = cwd.join("home");
        let codex = cwd.join("codex");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&codex).unwrap();
        Roots {
            home: Some(home),
            codex_home: Some(codex),
            cwd,
        }
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn message(text: &str) -> Value {
    json!({"type": "message", "role": "assistant",
        "content": [{"type": "output_text", "text": text}]})
}

fn call(id: &str) -> Value {
    json!({"type": "function_call", "name": "fixture_result",
        "call_id": id, "arguments": "{}"})
}

struct Reply {
    items: Vec<Value>,
    input_tokens: u64,
}

struct Script {
    name: String,
    instructions: String,
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Vec<Value>>>,
    seeds: Mutex<Vec<Vec<Value>>>,
    resets: Mutex<Vec<String>>,
}

impl Script {
    fn new(id: &str, replies: Vec<Reply>) -> Arc<Self> {
        Arc::new(Self {
            name: format!("ollama:{id}"),
            instructions: format!("fixture instructions {id}"),
            replies: Mutex::new(replies.into()),
            requests: Mutex::default(),
            seeds: Mutex::default(),
            resets: Mutex::default(),
        })
    }
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
            assert!(!cancel.load(Ordering::Relaxed));
            assert_eq!(instructions, self.instructions);
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0]["name"], "fixture_result");
            self.requests.lock().unwrap().push(input.to_vec());
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("script exhausted");
            on_delta(Delta::Usage(Usage {
                input: reply.input_tokens,
                ..Usage::default()
            }));
            Ok(reply.items)
        })
    }

    fn child(&self, _identity: &Identity) -> Arc<dyn Model> {
        panic!("delegation is disabled");
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn reports_cache(&self) -> bool {
        false
    }

    fn seed(&self, _instructions: &str, _tools: &[Value], input: &[Value]) {
        self.seeds.lock().unwrap().push(input.to_vec());
    }

    fn reset(&self, reason: &str) {
        self.resets.lock().unwrap().push(reason.to_string());
    }
}

struct FixtureResult {
    result: String,
    calls: Arc<AtomicUsize>,
}

impl Tool for FixtureResult {
    fn name(&self) -> &str {
        "fixture_result"
    }

    fn schema(&self) -> Value {
        json!({"type": "function", "name": self.name(), "description": "Report synthetic result",
            "parameters": {"type": "object", "properties": {}, "additionalProperties": false}})
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        if args == &json!({}) {
            Ok(self.result.clone())
        } else {
            Err("fixture takes no arguments".to_string())
        }
    }

    fn execute<'a>(&'a self, _args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (self.result.clone(), true)
        })
    }
}

fn fresh(roots: &Roots, session: &str, model: &Script) -> Saved {
    Saved {
        writer: Writer::create(
            &roots.cwd.join("transcripts"),
            Header::new(session, "fixture", model.name(), "", &roots.cwd),
        ),
        history: Vec::new(),
    }
}

fn reopen(roots: &Roots, loaded: &Loaded) -> Saved {
    Saved {
        writer: Writer::resume(&roots.cwd.join("transcripts"), loaded).unwrap(),
        history: loaded.items.clone(),
    }
}

fn load(roots: &Roots, session: &str) -> Loaded {
    sessions::load(&sessions::path(&roots.cwd.join("transcripts"), session)).unwrap()
}

async fn run(
    model: Arc<Script>,
    roots: &Roots,
    session: &str,
    saved: Saved,
    limits: Limits,
    calls: Arc<AtomicUsize>,
    prompt: &str,
) -> Vec<AgentEvent> {
    let mut runtime = Runtime::new(
        model.clone(),
        session,
        SystemPrompt {
            text: model.instructions.clone(),
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
    runtime.saved = Some(saved);
    runtime.limits = limits;
    let result = format!(
        "{}-result:fixture-only",
        model.name().strip_prefix("ollama:").unwrap()
    );
    runtime.registry = Some(Arc::new(move |_, _| {
        Registry::empty().with_tool(Box::new(FixtureResult {
            result: result.clone(),
            calls: Arc::clone(&calls),
        }))
    }));
    let (user, rx_user) = mpsc::channel(1);
    // No workflow or other control commands are accepted by this fixture.
    let (_control, rx_control) = mpsc::channel(1);
    // Runtime's event API is unbounded; the finite script and consumer are capped.
    let (events, mut rx_events) = mpsc::unbounded_channel();
    let cancel = Arc::new(Cancel::default());
    let task = tokio::spawn(runtime.run(rx_user, rx_control, events, Arc::clone(&cancel)));
    let observed = timeout(WAIT, async {
        user.send(prompt.into()).await.unwrap();
        let mut observed = Vec::new();
        loop {
            let event = rx_events.recv().await.expect("runtime closed during turn");
            assert!(observed.len() < 64);
            match event {
                AgentEvent::TurnEnd => break,
                AgentEvent::Approval { .. } => panic!("read-only fixture requested approval"),
                AgentEvent::Error(error) | AgentEvent::TurnFailed(error) => {
                    panic!("runtime failed: {error}")
                }
                other => observed.push(other),
            }
        }
        observed
    })
    .await;
    drop(user);
    if observed.is_err() {
        cancel.stop();
    }
    timeout(WAIT, task)
        .await
        .expect("runtime did not stop")
        .unwrap();
    assert!(model.replies.lock().unwrap().is_empty());
    observed.expect("turn did not finish")
}

#[tokio::test]
async fn completed_checkpoint_resumes_compacts_and_reopens_without_replaying_tools() {
    let directory = Directory::new();
    let roots = directory.roots("alpha");
    let session = "alpha-session";
    let calls = Arc::new(AtomicUsize::new(0));
    let completed = format!("alpha-result:fixture-only {}", "old-context ".repeat(800));
    let first = Script::new(
        "alpha",
        vec![
            Reply {
                items: vec![call("completed-call")],
                input_tokens: 1,
            },
            Reply {
                items: vec![message(&completed)],
                input_tokens: 1,
            },
        ],
    );
    run(
        first.clone(),
        &roots,
        session,
        fresh(&roots, session, &first),
        Limits::default(),
        Arc::clone(&calls),
        "alpha-original-task",
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let checkpoint = load(&roots, session);
    assert!(checkpoint.warnings.is_empty());
    assert_eq!(checkpoint.header.session, session);
    assert_eq!(checkpoint.header.cwd, roots.cwd);
    assert!(
        checkpoint
            .items
            .iter()
            .any(|item| item["type"] == "function_call_output"
                && item["output"] == "alpha-result:fixture-only")
    );

    let summary = "alpha-session: completed alpha-result:fixture-only; do not rerun the fixture";
    let limits = Limits {
        window: Some(1000),
        ..Limits::default()
    };
    // Reported usage crosses the tiny trigger; the runtime makes the summary call.
    let resumed = Script::new(
        "alpha",
        vec![
            Reply {
                items: vec![message("alpha-continued")],
                input_tokens: 850,
            },
            Reply {
                items: vec![message(summary)],
                input_tokens: 1,
            },
        ],
    );
    let events = run(
        resumed.clone(),
        &roots,
        session,
        reopen(&roots, &checkpoint),
        limits,
        Arc::clone(&calls),
        "alpha-follow-up",
    )
    .await;
    assert_eq!(*resumed.seeds.lock().unwrap(), [checkpoint.items.clone()]);
    assert_eq!(*resumed.resets.lock().unwrap(), ["compaction: summary"]);
    assert!(events.iter().any(|event| matches!(event, AgentEvent::Compacted { summary: Some(text), freed, .. } if text == summary && *freed > 0)));
    let requests = resumed.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].last().unwrap(), &compact::request(None));
    assert!(
        serde_json::to_string(&requests[1])
            .unwrap()
            .contains("alpha-result:fixture-only")
    );
    drop(requests);
    let compacted = load(&roots, session);
    assert!(compacted.warnings.is_empty());
    assert_eq!(compacted.header, checkpoint.header);
    let tokenizer = bhai::tokens::for_model(resumed.name());
    assert!(compact::estimate(&checkpoint.items, tokenizer) > limits.target(resumed.name()));
    assert!(compact::estimate(&compacted.items, tokenizer) < limits.target(resumed.name()));
    let text = serde_json::to_string(&compacted.items).unwrap();
    assert!(text.contains(compact::SUMMARY_PREFIX));
    assert!(text.contains(summary));
    assert!(text.contains("alpha-original-task"));
    assert!(text.contains("alpha-follow-up"));
    assert!(!text.contains("old-context"));
    let raw =
        std::fs::read_to_string(sessions::path(&roots.cwd.join("transcripts"), session)).unwrap();
    let records: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let records: Vec<_> = records
        .iter()
        .filter(|record| record["type"] == "compaction")
        .collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["stage"], "summary");
    assert!(records[0]["before"].as_u64().unwrap() > records[0]["after"].as_u64().unwrap());

    let again = Script::new(
        "alpha",
        vec![Reply {
            items: vec![message("alpha-confirmed")],
            input_tokens: 1,
        }],
    );
    run(
        again.clone(),
        &roots,
        session,
        reopen(&roots, &compacted),
        limits,
        Arc::clone(&calls),
        "alpha-confirm-result",
    )
    .await;
    assert_eq!(*again.seeds.lock().unwrap(), [compacted.items.clone()]);
    assert!(
        serde_json::to_string(&again.requests.lock().unwrap()[0])
            .unwrap()
            .contains(summary)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let reloaded = load(&roots, session);
    assert!(reloaded.warnings.is_empty());
    assert_eq!(reloaded.header, checkpoint.header);

    // A host-only second runtime proves conversation separation, not cwd sandboxing.
    let other_roots = directory.roots("beta");
    let other = Script::new(
        "beta",
        vec![
            Reply {
                items: vec![call("beta-call")],
                input_tokens: 1,
            },
            Reply {
                items: vec![message("beta-only")],
                input_tokens: 1,
            },
        ],
    );
    let other_calls = Arc::new(AtomicUsize::new(0));
    run(
        other.clone(),
        &other_roots,
        "beta-session",
        fresh(&other_roots, "beta-session", &other),
        Limits::default(),
        Arc::clone(&other_calls),
        "beta-task",
    )
    .await;
    assert!(other.seeds.lock().unwrap().is_empty());
    let other_requests = other.requests.lock().unwrap();
    assert_eq!(other_requests.len(), 2);
    let other_text = serde_json::to_string(&*other_requests).unwrap();
    assert!(!other_text.contains("alpha"));
    assert!(other_text.contains("beta-result:fixture-only"));
    assert_eq!(other_calls.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let other_checkpoint = load(&other_roots, "beta-session");
    assert_eq!(other_checkpoint.header.cwd, other_roots.cwd);
    assert!(
        !serde_json::to_string(&other_checkpoint.items)
            .unwrap()
            .contains("alpha")
    );
    assert_eq!(load(&roots, session).items, reloaded.items);
}

#[tokio::test]
async fn unanswered_checkpoint_call_is_warned_and_not_automatically_replayed() {
    let directory = Directory::new();
    let roots = directory.roots("pending");
    let session = "pending-session";
    let model = Script::new(
        "pending",
        vec![Reply {
            items: vec![message("reconciliation required")],
            input_tokens: 1,
        }],
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let tool = FixtureResult {
        result: "pending-result:fixture-only".to_string(),
        calls: Arc::clone(&calls),
    };
    let mut header = Header::new(session, "fixture", model.name(), "", &roots.cwd);
    header.prefix = sessions::prefix(model.name(), &model.instructions, &[tool.schema()]);
    let mut writer = Writer::create(&roots.cwd.join("transcripts"), header);
    writer
        .append(&compact::user_message(
            "pending-task: reconcile before further action",
        ))
        .unwrap();
    // Simulate a crash-left call record, not an observed external effect.
    writer.append(&call("uncertain-call")).unwrap();
    writer.sync().unwrap();
    drop(writer);
    let path = sessions::path(&roots.cwd.join("transcripts"), session);
    let loaded = load(&roots, session);
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("tool call with no result"));
    assert_eq!(loaded.items.len(), 1);
    assert!(loaded.len < std::fs::metadata(&path).unwrap().len());
    let saved = reopen(&roots, &loaded);
    // Resume can append preserved authorization recovery after truncating the tail.
    let truncated = load(&roots, session);
    assert!(truncated.warnings.is_empty());
    assert_eq!(truncated.items, loaded.items);
    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("uncertain-call")
    );
    run(
        model.clone(),
        &roots,
        session,
        saved,
        Limits::default(),
        Arc::clone(&calls),
        "status only; do not retry",
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    assert!(
        model.requests.lock().unwrap()[0]
            .iter()
            .all(|item| item["type"] != "function_call")
    );
    let recovered = load(&roots, session);
    assert!(recovered.warnings.is_empty());
    assert_eq!(recovered.header.session, session);
    assert!(
        !std::fs::read_to_string(path)
            .unwrap()
            .contains("uncertain-call")
    );
    // Loader warnings are not an uncertainty ledger or evidence that an effect did not run.
}
