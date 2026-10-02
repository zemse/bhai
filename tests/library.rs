//! The embedding API on a scripted model, without credentials or a model server.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bhai::client::Delta;
use bhai::identity::Identity;
use bhai::permissions::{Answer, Mode, Rules};
use bhai::tools::BoxFuture;
use bhai::{AgentEvent, Cancel, Model, Policy, Registry, Runtime, SystemPrompt, Tool};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::timeout;

#[derive(Clone)]
struct Script {
    replies: Arc<Mutex<VecDeque<Vec<Value>>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Script {
    fn tool() -> Self {
        Self {
            replies: Arc::new(Mutex::new(VecDeque::from([
                vec![json!({
                    "type": "function_call", "name": "host_echo", "call_id": "echo-1",
                    "arguments": "{\"text\":\"from host\"}"
                })],
                vec![json!({
                    "type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": "done"}]
                })],
            ]))),
            requests: Arc::default(),
        }
    }
}

impl Model for Script {
    fn respond<'a>(
        &'a self,
        instructions: &'a str,
        tools: &'a [Value],
        input: &'a [Value],
        on_delta: &'a mut (dyn FnMut(Delta) + Send),
        _cancel: &'a Arc<AtomicBool>,
    ) -> BoxFuture<'a, anyhow::Result<Vec<Value>>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(json!({
                "instructions": instructions, "tools": tools, "input": input
            }));
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("script exhausted");
            if reply[0]["type"] == "message" {
                on_delta(Delta::Text("done".to_string()));
            }
            Ok(reply)
        })
    }

    fn child(&self, _identity: &Identity) -> Arc<dyn Model> {
        Arc::new(self.clone())
    }

    fn name(&self) -> &str {
        "ollama:script"
    }

    fn reports_cache(&self) -> bool {
        false
    }
}

struct Echo(Arc<AtomicUsize>);

impl Tool for Echo {
    fn name(&self) -> &str {
        "host_echo"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function", "name": self.name(), "description": "Echo text",
            "parameters": {"type": "object", "properties": {"text": {"type": "string"}},
                "required": ["text"], "additionalProperties": false}
        })
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        args["text"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "text missing".to_string())
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            (args["text"].as_str().unwrap().to_string(), true)
        })
    }
}

async fn drive(answer: Answer) -> (Vec<AgentEvent>, Script, usize) {
    let model = Script::tool();
    let calls = Arc::new(AtomicUsize::new(0));
    let roots = bhai::Roots {
        home: None,
        codex_home: None,
        cwd: std::env::temp_dir(),
    };
    let policy = Arc::new(Policy::new(
        Mode::Ask,
        Rules::default(),
        roots.home,
        roots.cwd,
    ));
    let mut runtime = Runtime::new(
        Arc::new(model.clone()),
        "host-session",
        SystemPrompt {
            text: "host instructions".to_string(),
            ..Default::default()
        },
        policy,
    );
    runtime.registry = Some(Arc::new({
        let calls = Arc::clone(&calls);
        move |_, _| Registry::empty().with_tool(Box::new(Echo(Arc::clone(&calls))))
    }));
    let (user, rx_user) = mpsc::channel(1);
    let (_control, rx_control) = mpsc::channel(1);
    let (events, mut rx_events) = mpsc::unbounded_channel();
    let task = tokio::spawn(runtime.run(rx_user, rx_control, events, Arc::new(Cancel::default())));
    user.send("use the host tool".into()).await.unwrap();
    let observed = timeout(Duration::from_secs(10), async {
        let mut observed = Vec::new();
        while let Some(event) = rx_events.recv().await {
            match event {
                AgentEvent::Approval { tool, reply, .. } => {
                    assert_eq!(tool, "host_echo");
                    reply.send(answer).unwrap();
                }
                AgentEvent::TurnEnd => break,
                AgentEvent::Error(ref error) | AgentEvent::TurnFailed(ref error) => {
                    panic!("runtime failed: {error}");
                }
                event => observed.push(event),
            }
        }
        observed
    })
    .await
    .expect("turn did not finish");
    drop(user);
    timeout(Duration::from_secs(10), task)
        .await
        .expect("runtime did not stop")
        .unwrap();
    (observed, model, calls.load(Ordering::SeqCst))
}

#[tokio::test]
async fn host_registry_replaces_builtins_and_runs_through_approval() {
    let (events, model, calls) = drive(Answer::Accept(None)).await;
    assert_eq!(calls, 1);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolOutput(text) if text == "from host"))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Text(text) if text == "done"))
    );
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(request["instructions"], "host instructions");
        assert_eq!(request["tools"].as_array().unwrap().len(), 1);
        assert_eq!(request["tools"][0]["name"], "host_echo");
    }
    assert!(
        requests[1]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["output"] == "from host")
    );
}

#[tokio::test]
async fn rejected_host_tool_does_not_execute() {
    let (events, _, calls) = drive(Answer::Reject).await;
    assert_eq!(calls, 0);
    assert!(events.iter().any(
        |event| matches!(event, AgentEvent::ToolRejected { tool, .. } if tool == "host_echo")
    ));
}

#[tokio::test]
async fn independent_runtimes_keep_model_and_tool_state_separate() {
    let (accepted, rejected) = tokio::join!(drive(Answer::Accept(None)), drive(Answer::Reject));
    assert_eq!(accepted.2, 1);
    assert_eq!(rejected.2, 0);
    assert_eq!(accepted.1.requests.lock().unwrap().len(), 2);
    assert_eq!(rejected.1.requests.lock().unwrap().len(), 2);
}

#[test]
fn registry_construction_is_available_outside_unit_tests() {
    let empty = Registry::empty();
    assert!(empty.names().is_empty());
    assert!(empty.schemas().is_empty());
    assert!(empty.get("bash").is_none());
    let builtins = Registry::for_prompt(&SystemPrompt::default());
    assert!(builtins.get("bash").is_some());
    assert!(builtins.get("read").is_some());
}
