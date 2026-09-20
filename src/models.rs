//! What `/model` offers: the models each backend will serve, the reasoning efforts each
//! one takes, and the picker that walks the two lists.
//!
//! The Codex list is the backend's own (`GET {base}/models`, as openai/codex reads it in
//! `codex-rs/model-provider/src/models_endpoint.rs`), falling back to the list that CLI
//! last cached in `~/.codex/models_cache.json`, and then to the model this session is
//! already on. Ollama's list is whatever `ollama list` shows, read from `/api/tags`.
//!
//! Only the Codex backend takes a reasoning effort: bhai's Ollama request body has
//! nowhere to put one, so a local model is picked and run, with no second question.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use serde_json::Value;

use crate::client::{self, Provider};
use crate::ollama;

/// Give up on a backend that has not answered the list by then.
const TIMEOUT: Duration = Duration::from_secs(10);
/// The `client_version` the models endpoint insists on, when the Codex CLI's own cache
/// does not say which version it last asked as.
const CLIENT_VERSION: &str = "0.154.0";
/// The efforts a Codex model takes when the list could not be read; what every model in
/// the gpt-5 family has offered.
const FALLBACK_EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];

/// One model on offer.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    /// What `--model` would be given: `gpt-5.5`, or `ollama:<name>`.
    pub id: String,
    pub label: String,
    /// The one-line description the row shows, empty when the backend gives none.
    pub detail: String,
    /// The reasoning efforts it takes, in order; empty when it takes none.
    pub efforts: Vec<Effort>,
    /// The effort the backend runs it at unless told otherwise.
    pub default_effort: Option<String>,
    /// Its context window in tokens, when the backend says.
    pub window: Option<u64>,
}

impl Model {
    /// The effort to start on: the session's, when this model takes it, else the
    /// backend's default, else the first it offers.
    fn effort_row(&self, current: &str) -> usize {
        self.efforts
            .iter()
            .position(|e| e.name == current)
            .or_else(|| {
                let default = self.default_effort.as_deref()?;
                self.efforts.iter().position(|e| e.name == default)
            })
            .unwrap_or(0)
    }
}

/// One reasoning effort a model takes.
#[derive(Debug, Clone, PartialEq)]
pub struct Effort {
    pub name: String,
    pub detail: String,
}

/// The models on offer, and a note for each backend that could not be asked.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalogue {
    pub models: Vec<Model>,
    pub notes: Vec<String>,
}

/// Ask both backends what they will serve. `current` is the session's model, which is
/// listed even when its backend cannot be reached, since it is demonstrably servable.
pub async fn load(ollama_url: &str, current: &str) -> Catalogue {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_default();
    let mut found = Catalogue::default();

    match tokio::time::timeout(TIMEOUT, codex(&http)).await {
        Ok(Ok(models)) => found.models.extend(models),
        Ok(Err(e)) => found.notes.push(format!("codex: {e:#}")),
        Err(_) => found
            .notes
            .push("codex: the model list timed out".to_string()),
    }
    match tokio::time::timeout(TIMEOUT, tags(&http, ollama_url)).await {
        Ok(Ok(models)) => found.models.extend(models),
        Ok(Err(e)) => found.notes.push(format!("ollama: {e:#}")),
        Err(_) => found
            .notes
            .push("ollama: the model list timed out".to_string()),
    }
    if !found.models.iter().any(|m| m.id == current) {
        found.models.insert(0, running(current));
    }
    found
}

/// The model this session is on, for when its backend did not list it.
fn running(current: &str) -> Model {
    Model {
        id: current.to_string(),
        label: current.to_string(),
        detail: "the model this session is on".to_string(),
        efforts: match Provider::of(current) {
            Provider::Codex => FALLBACK_EFFORTS.iter().map(|e| effort(e, "")).collect(),
            Provider::Ollama => Vec::new(),
        },
        default_effort: None,
        window: None,
    }
}

fn effort(name: &str, detail: &str) -> Effort {
    Effort {
        name: name.to_string(),
        detail: detail.to_string(),
    }
}

/// The Codex backend's list, or the one the Codex CLI cached when it cannot be reached.
async fn codex(http: &reqwest::Client) -> Result<Vec<Model>> {
    let cached = cache();
    let fetched = fetch(http, &cached).await;
    match (fetched, cached) {
        (Ok(body), _) => Ok(codex_models(&body)),
        (Err(e), Some(body)) => {
            let models = codex_models(&body);
            match models.is_empty() {
                true => Err(e),
                false => Ok(models),
            }
        }
        (Err(e), None) => Err(e),
    }
}

/// `GET {base}/models`, with the ChatGPT credentials the Codex CLI leaves behind.
async fn fetch(http: &reqwest::Client, cached: &Option<Value>) -> Result<Value> {
    let auth = crate::auth::load(http).await?;
    let version = cached
        .as_ref()
        .and_then(|body| body.get("client_version"))
        .and_then(Value::as_str)
        .unwrap_or(CLIENT_VERSION);
    let mut req = http
        .get(format!(
            "{}/models?client_version={version}",
            client::BASE_URL
        ))
        .bearer_auth(&auth.access_token)
        .header("originator", client::ORIGINATOR);
    if let Some(account_id) = &auth.account_id {
        req = req.header("ChatGPT-Account-ID", account_id);
    }
    let resp = req.send().await.context("could not ask for the models")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("the models endpoint answered {status}");
    }
    serde_json::from_str(&body).context("the models endpoint did not answer JSON")
}

/// What the Codex CLI last wrote to `~/.codex/models_cache.json`.
fn cache() -> Option<Value> {
    let path = crate::auth::codex_home().ok()?.join("models_cache.json");
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The models of a `{"models": [...]}` body, listable ones only, most capable first.
pub fn codex_models(body: &Value) -> Vec<Model> {
    let mut models: Vec<(i64, Model)> = body
        .get("models")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        // `hide` is the backend's way of keeping a model out of a picker such as this.
        .filter(|m| m.get("visibility").and_then(Value::as_str) != Some("hide"))
        .filter(|m| m.get("supported_in_api").and_then(Value::as_bool) != Some(false))
        .filter_map(|m| {
            let slug = m.get("slug").and_then(Value::as_str)?;
            let string = |key: &str| m.get(key).and_then(Value::as_str).unwrap_or_default();
            let efforts = m
                .get("supported_reasoning_levels")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(|level| {
                    let name = level.get("effort").and_then(Value::as_str)?;
                    let detail = level.get("description").and_then(Value::as_str);
                    Some(effort(name, detail.unwrap_or_default()))
                })
                .collect();
            let priority = m
                .get("priority")
                .and_then(Value::as_i64)
                .unwrap_or(i64::MAX);
            Some((
                priority,
                Model {
                    id: slug.to_string(),
                    label: match string("display_name") {
                        "" => slug.to_string(),
                        name => name.to_string(),
                    },
                    detail: string("description").to_string(),
                    efforts,
                    default_effort: m
                        .get("default_reasoning_level")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    window: m.get("context_window").and_then(Value::as_u64),
                },
            ))
        })
        .collect();
    models.sort_by_key(|(priority, _)| *priority);
    models.into_iter().map(|(_, model)| model).collect()
}

/// What Ollama has pulled, from `/api/tags`.
async fn tags(http: &reqwest::Client, url: &str) -> Result<Vec<Model>> {
    let endpoint = format!("{}/api/tags", url.trim_end_matches('/'));
    let body: Value = http
        .get(&endpoint)
        .send()
        .await
        .with_context(|| format!("no server at {url}. Start one with `ollama serve`."))?
        .json()
        .await
        .with_context(|| format!("{endpoint} did not answer JSON"))?;
    Ok(ollama_models(&body))
}

/// The models of an `/api/tags` body, as `ollama:<name>` ids.
pub fn ollama_models(body: &Value) -> Vec<Model> {
    body.get("models")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|m| {
            let name = m.get("name").and_then(Value::as_str)?;
            let detail = m
                .get("details")
                .map(|d| {
                    [d.get("parameter_size"), d.get("quantization_level")]
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" · ")
                })
                .unwrap_or_default();
            Some(Model {
                id: format!("{}{name}", ollama::PREFIX),
                label: name.to_string(),
                detail,
                // Nothing in bhai's Ollama request body carries a reasoning effort.
                efforts: Vec::new(),
                default_effort: None,
                window: None,
            })
        })
        .collect()
}

/// Which list the picker is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Models,
    /// The efforts of the model at that row.
    Efforts(usize),
}

/// What a keystroke did to the picker.
#[derive(Debug, Clone, PartialEq)]
pub enum Choice {
    /// Still picking.
    Waiting,
    /// Closed without choosing.
    Closed,
    /// Switch to this model.
    Picked {
        model: String,
        effort: String,
        window: Option<u64>,
    },
}

/// The `/model` picker: the models, then the efforts of the one chosen. A model that
/// takes no effort is picked in one step, since there is no second question to ask.
pub struct Picker {
    /// `None` while the backends are still being asked.
    pub catalogue: Option<Catalogue>,
    pub stage: Stage,
    pub selected: usize,
    /// The model and effort the session is on, marked in the lists.
    current: (String, String),
    /// Where the backends' answer lands, filled in by the task that asks them.
    incoming: Arc<Mutex<Option<Catalogue>>>,
}

impl Picker {
    /// A picker for a session on `model` at `effort`, before the lists have arrived.
    pub fn new(model: &str, effort: &str) -> Self {
        Self {
            catalogue: None,
            stage: Stage::Models,
            selected: 0,
            current: (model.to_string(), effort.to_string()),
            incoming: Arc::new(Mutex::new(None)),
        }
    }

    /// Ask both backends in the background; `poll` picks the answer up.
    pub fn ask(&self, ollama_url: &str) {
        let (into, url) = (Arc::clone(&self.incoming), ollama_url.to_string());
        let current = self.current.0.clone();
        tokio::spawn(async move {
            let found = load(&url, &current).await;
            *into.lock().unwrap_or_else(|e| e.into_inner()) = Some(found);
        });
    }

    /// Take the backends' answer if it has arrived. Returns whether it just did.
    pub fn poll(&mut self) -> bool {
        if self.catalogue.is_some() {
            return false;
        }
        let found = self
            .incoming
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        match found {
            Some(found) => {
                self.fill(found);
                true
            }
            None => false,
        }
    }

    /// Show `found`, with the model the session is on highlighted.
    pub fn fill(&mut self, found: Catalogue) {
        self.selected = found
            .models
            .iter()
            .position(|m| m.id == self.current.0)
            .unwrap_or(0);
        self.catalogue = Some(found);
    }

    fn models(&self) -> &[Model] {
        self.catalogue.as_ref().map_or(&[], |c| &c.models)
    }

    /// The rows on show: the models, or the efforts of the one being decided.
    fn rows(&self) -> Vec<(String, String, bool)> {
        let models = self.models();
        match self.stage {
            Stage::Models => models
                .iter()
                .map(|m| (m.label.clone(), m.detail.clone(), m.id == self.current.0))
                .collect(),
            Stage::Efforts(at) => models
                .get(at)
                .map(|m| {
                    m.efforts
                        .iter()
                        .map(|e| {
                            let mut detail = e.detail.clone();
                            if m.default_effort.as_deref() == Some(e.name.as_str()) {
                                let dash = if detail.is_empty() { "" } else { " · " };
                                detail = format!("{detail}{dash}the model's default");
                            }
                            let on = m.id == self.current.0 && e.name == self.current.1;
                            (e.name.clone(), detail, on)
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    pub fn on_key(&mut self, code: KeyCode) -> Choice {
        let rows = self.rows().len();
        match code {
            KeyCode::Esc | KeyCode::Char('q') => match self.stage {
                // Escape out of the efforts goes back to the models, not out of the
                // picker: the model has not been switched to yet.
                Stage::Efforts(at) => {
                    self.stage = Stage::Models;
                    self.selected = at;
                    Choice::Waiting
                }
                Stage::Models => Choice::Closed,
            },
            _ if rows == 0 => Choice::Waiting,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = (self.selected + rows - 1) % rows;
                Choice::Waiting
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1) % rows;
                Choice::Waiting
            }
            KeyCode::Enter => self.accept(),
            _ => Choice::Waiting,
        }
    }

    /// Enter on a row: a model that takes an effort asks for one, anything else is the
    /// switch itself.
    fn accept(&mut self) -> Choice {
        match self.stage {
            Stage::Models => {
                let Some(model) = self.models().get(self.selected) else {
                    return Choice::Waiting;
                };
                if model.efforts.is_empty() {
                    return Choice::Picked {
                        model: model.id.clone(),
                        // Kept as it is: nothing on this backend reads it.
                        effort: self.current.1.clone(),
                        window: model.window,
                    };
                }
                let at = self.selected;
                self.selected = model.effort_row(&self.current.1);
                self.stage = Stage::Efforts(at);
                Choice::Waiting
            }
            Stage::Efforts(at) => {
                let Some(model) = self.models().get(at) else {
                    return Choice::Waiting;
                };
                let Some(effort) = model.efforts.get(self.selected) else {
                    return Choice::Waiting;
                };
                Choice::Picked {
                    model: model.id.clone(),
                    effort: effort.name.clone(),
                    window: model.window,
                }
            }
        }
    }

    /// The title of the list on show.
    fn title(&self) -> String {
        match self.stage {
            Stage::Models => " pick a model ".to_string(),
            Stage::Efforts(at) => match self.models().get(at) {
                Some(model) => format!(" {} · pick an effort ", model.label),
                None => " pick an effort ".to_string(),
            },
        }
    }

    /// Rows the list needs, the notes and the hint line included.
    pub fn height(&self) -> u16 {
        let notes = self.catalogue.as_ref().map_or(0, |c| c.notes.len());
        let rows = self.rows().len().max(1) + notes;
        rows as u16 + 2
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let dim = Style::new().fg(Color::DarkGray);
        let hint = match self.stage {
            Stage::Models => " ↑↓ pick · enter choose · esc close ",
            Stage::Efforts(_) => " ↑↓ pick · enter switch · esc back ",
        };
        let block = Block::bordered()
            .title(self.title())
            .title_bottom(Line::styled(hint, dim).right_aligned())
            .border_style(Style::new().fg(Color::Cyan));
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);

        let Some(catalogue) = &self.catalogue else {
            let waiting = Paragraph::new(Line::styled(" asking the backends…", dim));
            frame.render_widget(waiting, inner);
            return;
        };
        let rows = self.rows();
        let width = inner.width as usize;
        let height = inner.height as usize;
        // The notes say which backend is missing from the list, so they stay in view.
        let notes = catalogue.notes.len().min(height.saturating_sub(1));
        let listed = height.saturating_sub(notes);
        let top = self
            .selected
            .saturating_sub(listed.saturating_sub(1))
            .min(rows.len().saturating_sub(listed.max(1)));
        let mut lines: Vec<Line> = rows
            .iter()
            .enumerate()
            .skip(top)
            .take(listed)
            .map(|(i, (name, detail, on))| row(name, detail, *on, i == self.selected, width))
            .collect();
        if rows.is_empty() {
            lines.push(Line::styled(" nothing on offer", dim));
        }
        lines.extend(
            catalogue
                .notes
                .iter()
                .take(notes)
                .map(|note| Line::styled(format!(" {note}"), Style::new().fg(Color::Yellow))),
        );
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// One picker row: the name, a mark when it is what the session is on, then the detail.
fn row(name: &str, detail: &str, on: bool, selected: bool, width: usize) -> Line<'static> {
    let mark = if on { "·" } else { " " };
    let mut text = format!(" {mark} {name:<24} {detail}");
    text.truncate(
        text.char_indices()
            .nth(width)
            .map_or(text.len(), |(at, _)| at),
    );
    if selected {
        let style = Style::new().fg(Color::Black).bg(Color::Cyan);
        return Line::styled(format!("{text:<width$}"), style);
    }
    let cut = name.chars().count() + 4;
    let head: String = text.chars().take(cut).collect();
    let tail: String = text.chars().skip(cut).collect();
    let head = match on {
        true => Span::styled(head, Style::new().fg(Color::Cyan).bold()),
        false => Span::styled(head, Style::new().fg(Color::Cyan)),
    };
    Line::from(vec![
        head,
        Span::styled(tail, Style::new().fg(Color::DarkGray)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn codex_body() -> Value {
        json!({"models": [
            {
                "slug": "gpt-5.5",
                "display_name": "GPT-5.5",
                "description": "Proven previous-generation model.",
                "default_reasoning_level": "medium",
                "supported_reasoning_levels": [
                    {"effort": "low", "description": "Fast"},
                    {"effort": "medium", "description": "Balanced"},
                    {"effort": "high", "description": "Deeper"},
                ],
                "visibility": "list",
                "supported_in_api": true,
                "priority": 12,
                "context_window": 272000,
            },
            {
                "slug": "gpt-6-astra",
                "display_name": "GPT-6-Astra",
                "default_reasoning_level": "low",
                "supported_reasoning_levels": [{"effort": "low"}, {"effort": "max"}],
                "visibility": "list",
                "priority": 1,
            },
            {"slug": "gpt-reserve", "visibility": "hide", "priority": 3},
            {"slug": "not-in-api", "visibility": "list", "supported_in_api": false},
        ]})
    }

    #[test]
    fn codex_models_are_listed_most_capable_first() {
        let models = codex_models(&codex_body());
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gpt-6-astra", "gpt-5.5"], "hidden models stay hidden");
        let gpt55 = &models[1];
        assert_eq!(gpt55.label, "GPT-5.5");
        assert_eq!(gpt55.window, Some(272_000));
        assert_eq!(gpt55.default_effort.as_deref(), Some("medium"));
        let efforts: Vec<_> = gpt55.efforts.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(efforts, ["low", "medium", "high"]);
        assert_eq!(gpt55.efforts[0].detail, "Fast");
    }

    #[test]
    fn ollama_models_carry_the_prefix_and_no_effort() {
        let body = json!({"models": [
            {"name": "gemma4:e2b", "details": {"parameter_size": "2B", "quantization_level": "Q4"}},
            {"name": "qwen3:8b"},
        ]});
        let models = ollama_models(&body);
        assert_eq!(models[0].id, "ollama:gemma4:e2b");
        assert_eq!(models[0].label, "gemma4:e2b");
        assert_eq!(models[0].detail, "2B · Q4");
        assert!(
            models.iter().all(|m| m.efforts.is_empty()),
            "no effort reaches Ollama"
        );
        assert_eq!(models[1].id, "ollama:qwen3:8b");
    }

    #[test]
    fn garbage_is_skipped_rather_than_taken() {
        assert!(codex_models(&json!({})).is_empty());
        assert!(codex_models(&json!({"models": "lots"})).is_empty());
        assert!(codex_models(&json!({"models": [{"display_name": "no slug"}]})).is_empty());
        assert!(ollama_models(&json!({"models": [{"size": 12}]})).is_empty());
    }

    fn picked() -> Catalogue {
        let mut models = codex_models(&codex_body());
        models.extend(ollama_models(&json!({"models": [{"name": "gemma4:e2b"}]})));
        Catalogue {
            models,
            notes: Vec::new(),
        }
    }

    fn picker(model: &str, effort: &str) -> Picker {
        let mut picker = Picker::new(model, effort);
        picker.fill(picked());
        picker
    }

    #[test]
    fn the_list_opens_on_the_model_the_session_is_on() {
        let picker = picker("gpt-5.5", "high");
        assert_eq!(picker.selected, 1);
        assert_eq!(picker.stage, Stage::Models);
        let rows = picker.rows();
        assert_eq!(rows[1].0, "GPT-5.5");
        assert!(rows[1].2, "the running model is marked");
        assert!(!rows[0].2);
    }

    #[test]
    fn a_model_with_efforts_asks_for_one_before_switching() {
        let mut picker = picker("gpt-5.5", "high");
        picker.selected = 0; // gpt-6-astra
        assert_eq!(picker.on_key(KeyCode::Enter), Choice::Waiting);
        assert_eq!(picker.stage, Stage::Efforts(0));
        // low and max, opened on neither the session's `high` nor a match, so the
        // model's own default.
        assert_eq!(picker.selected, 0);
        let efforts: Vec<_> = picker.rows().iter().map(|r| r.0.clone()).collect();
        assert_eq!(efforts, ["low", "max"]);
        picker.on_key(KeyCode::Down);
        assert_eq!(
            picker.on_key(KeyCode::Enter),
            Choice::Picked {
                model: "gpt-6-astra".to_string(),
                effort: "max".to_string(),
                window: None,
            }
        );
    }

    #[test]
    fn the_effort_list_opens_on_the_one_the_session_runs_at() {
        let mut picker = picker("gpt-5.5", "high");
        picker.on_key(KeyCode::Enter);
        assert_eq!(picker.stage, Stage::Efforts(1));
        assert_eq!(picker.rows()[picker.selected].0, "high");
        // The default is called out, so `medium` is visibly the model's own.
        assert!(picker.rows()[1].1.contains("the model's default"));
    }

    #[test]
    fn a_model_that_takes_no_effort_is_picked_in_one_step() {
        let mut picker = picker("gpt-5.5", "high");
        picker.selected = 2; // ollama:gemma4:e2b
        assert_eq!(
            picker.on_key(KeyCode::Enter),
            Choice::Picked {
                model: "ollama:gemma4:e2b".to_string(),
                effort: "high".to_string(),
                window: None,
            },
            "the session's effort is kept, since Ollama is never sent one"
        );
    }

    #[test]
    fn escape_backs_out_of_the_efforts_then_closes() {
        let mut picker = picker("gpt-5.5", "high");
        picker.on_key(KeyCode::Enter);
        assert_eq!(picker.on_key(KeyCode::Esc), Choice::Waiting);
        assert_eq!(picker.stage, Stage::Models);
        assert_eq!(picker.selected, 1, "back on the model it was deciding");
        assert_eq!(picker.on_key(KeyCode::Esc), Choice::Closed);
    }

    #[test]
    fn keys_do_nothing_until_the_lists_arrive() {
        let mut picker = Picker::new("gpt-5.5", "high");
        assert_eq!(picker.on_key(KeyCode::Down), Choice::Waiting);
        assert_eq!(picker.on_key(KeyCode::Enter), Choice::Waiting);
        assert_eq!(picker.selected, 0);
        assert_eq!(picker.on_key(KeyCode::Esc), Choice::Closed);
    }

    #[test]
    fn the_running_model_is_listed_even_when_its_backend_is_not() {
        let listed = running("ollama:gemma4:e2b");
        assert!(listed.efforts.is_empty());
        let listed = running("gpt-5.5");
        let names: Vec<_> = listed.efforts.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, FALLBACK_EFFORTS);
    }
}
