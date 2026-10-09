//! CLI discovery uses model-list endpoints, never inference or a saved agent session.

use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::time::timeout;

fn codex_models() -> Value {
    json!({"models": [{
        "slug": "gpt-listed", "display_name": "Listed model", "visibility": "list",
        "default_reasoning_level": "medium", "context_window": 272000,
        "supported_reasoning_levels": [{"effort": "low"}, {"effort": "medium"}]
    }]})
}

struct Backend {
    codex_ok: bool,
    ollama_ok: bool,
    paths: Mutex<Vec<String>>,
}

async fn respond(
    State(backend): State<Arc<Backend>>,
    request: Request,
) -> (StatusCode, Json<Value>) {
    let path = request.uri().path();
    backend.paths.lock().unwrap().push(path.to_string());
    match path {
        "/codex/models" if backend.codex_ok => (StatusCode::OK, Json(codex_models())),
        "/api/tags" if backend.ollama_ok => (
            StatusCode::OK,
            Json(json!({"models": [{"name": "local:test"}]})),
        ),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "offline"})),
        ),
    }
}

struct Harness {
    dir: PathBuf,
    backend: Arc<Backend>,
    server: tokio::task::JoinHandle<()>,
    base: String,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Harness {
    async fn new(codex_ok: bool, ollama_ok: bool) -> Self {
        let backend = Arc::new(Backend {
            codex_ok,
            ollama_ok,
            paths: Mutex::default(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new()
            .fallback(respond)
            .with_state(Arc::clone(&backend));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let dir = std::env::temp_dir().join(format!("bhai-models-{}", uuid::Uuid::new_v4()));
        for path in [
            dir.join("home/.config/bhai"),
            dir.join("codex"),
            dir.join("project"),
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        let auth = json!({"tokens": {"access_token": "e30.eyJleHAiOjQxMDI0NDQ4MDB9.c2ln"}});
        std::fs::write(dir.join("codex/auth.json"), auth.to_string()).unwrap();
        std::fs::write(
            dir.join("home/.config/bhai/config.toml"),
            format!("model = \"not-discovered\"\nollama_url = \"{base}\"\n"),
        )
        .unwrap();
        Self {
            dir,
            backend,
            server,
            base,
        }
    }

    async fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bhai"));
        command
            .args(args)
            .current_dir(self.dir.join("project"))
            .env("HOME", self.dir.join("home"))
            .env("CODEX_HOME", self.dir.join("codex"))
            .env("BHAI_TEST_BASE_URL", &self.base)
            .env("BHAI_OLLAMA_URL", "http://127.0.0.1:1")
            .env_remove("BHAI_MODE")
            .env_remove("BHAI_MODEL")
            .env_remove("BHAI_EFFORT")
            .env_remove("BHAI_STARTUP_TIMING")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        timeout(Duration::from_secs(5), command.output())
            .await
            .unwrap()
            .unwrap()
    }

    fn assert_no_session(&self) {
        assert!(!self.dir.join("project/.bhai").exists());
        assert!(
            self.backend
                .paths
                .lock()
                .unwrap()
                .iter()
                .all(|path| path == "/codex/models" || path == "/api/tags")
        );
    }
}

#[tokio::test]
async fn text_and_json_list_exact_selectable_ids_and_efforts() {
    let harness = Harness::new(true, true).await;
    let text = harness.run(&["models"]).await;
    assert!(text.status.success(), "{text:?}");
    assert!(text.stderr.is_empty());
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(
        text.contains("- gpt-listed; efforts: low, medium; window: 272000"),
        "{text}"
    );
    assert!(text.contains("- ollama:local:test"), "{text}");
    let output = harness.run(&["models", "--json"]).await;
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty());
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["models"][0]["id"], "gpt-listed");
    assert_eq!(body["models"][0]["efforts"][0]["name"], "low");
    assert_eq!(body["models"][0]["default_effort"], "medium");
    assert_eq!(body["models"][0]["window"], 272000);
    assert_eq!(body["models"][1]["id"], "ollama:local:test");
    assert_eq!(body["models"][1]["efforts"], json!([]));
    assert_eq!(body["notes"], json!([]));
    assert_eq!(body["models"].as_array().unwrap().len(), 2);
    assert_eq!(harness.backend.paths.lock().unwrap().len(), 4);
    harness.assert_no_session();
}

#[tokio::test]
async fn cached_codex_results_and_unavailable_backends_are_labelled() {
    let harness = Harness::new(false, false).await;
    std::fs::write(
        harness.dir.join("codex/models_cache.json"),
        codex_models().to_string(),
    )
    .unwrap();
    let output = harness.run(&["--json", "models"]).await;
    assert!(output.status.success(), "{output:?}");
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["models"].as_array().unwrap().len(), 1);
    assert_eq!(body["models"][0]["id"], "gpt-listed");
    let notes = body["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|note| note.as_str().unwrap().contains("using cached models"))
    );
    assert!(
        notes
            .iter()
            .any(|note| note.as_str().unwrap().starts_with("ollama:"))
    );
    harness.assert_no_session();
}

#[tokio::test]
async fn no_models_exits_one_with_parseable_json_and_no_synthetic_default() {
    let harness = Harness::new(false, false).await;
    let output = harness.run(&["models", "--json"]).await;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["models"], json!([]));
    assert_eq!(body["notes"].as_array().unwrap().len(), 2);
    assert!(String::from_utf8_lossy(&output.stderr).contains("no models discovered"));
    harness.assert_no_session();
}

#[tokio::test]
async fn ollama_discovery_does_not_require_codex_login() {
    let harness = Harness::new(false, true).await;
    std::fs::remove_file(harness.dir.join("codex/auth.json")).unwrap();
    let output = harness.run(&["models", "--json"]).await;
    assert!(output.status.success(), "{output:?}");
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["models"].as_array().unwrap().len(), 1);
    assert_eq!(body["models"][0]["id"], "ollama:local:test");
    assert!(body["notes"][0].as_str().unwrap().starts_with("codex:"));
    assert_eq!(*harness.backend.paths.lock().unwrap(), ["/api/tags"]);
    harness.assert_no_session();
}
