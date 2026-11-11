//! Raw Codex SSE timing probe. Run with `cargo run --example stream_timing`.
//! Redirect stdout to a JSONL file so terminal rendering does not dominate timing.
//! Characters in one delta share its arrival time; neither chunks nor chars are tokens.
//! `cargo run --example stream_timing -- --model gpt-6-luna > stream-timing.jsonl`
//! `first_text` and `text_done` bound visible output, not hidden token generation.
//! All events, including reasoning and tool arguments, retain their backend event type.
//! `response_end` is the protocol end; `probe_end` is local, not remote socket EOF.

#[allow(dead_code)]
#[path = "../src/auth.rs"]
mod auth;
#[allow(dead_code)]
#[path = "../src/childenv.rs"]
mod childenv;
#[allow(dead_code)]
#[path = "../src/redact.rs"]
mod redact;

use std::io::{self, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Local;
use clap::Parser;
use futures_util::StreamExt;
use serde_json::{Value, json};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "gpt-6-luna")]
    model: String,
    #[arg(long, default_value = "low")]
    effort: String,
    #[arg(
        long,
        default_value = "Count from 1 to 30, separated by spaces. Nothing else."
    )]
    prompt: String,
}

#[derive(Default)]
struct Sse {
    pending: Vec<u8>,
    data: Vec<String>,
}

impl Sse {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
            let line = self.pending.drain(..=end).collect::<Vec<_>>();
            let line = std::str::from_utf8(&line)?.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(self.data.join("\n"));
                    self.data.clear();
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                self.data
                    .push(data.strip_prefix(' ').unwrap_or(data).to_string());
            }
        }
        Ok(events)
    }
}

struct Probe {
    start: Instant,
    previous: Option<Duration>,
    first_text: Option<Duration>,
    last_text: Option<Duration>,
    chars: usize,
    deltas: usize,
    longest_gap_ms: f64,
}

impl Probe {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            previous: None,
            first_text: None,
            last_text: None,
            chars: 0,
            deltas: 0,
            longest_gap_ms: 0.0,
        }
    }

    fn event(
        &mut self,
        out: &mut impl Write,
        at: Duration,
        wall: &str,
        event: Value,
    ) -> Result<bool> {
        let kind = event["type"].as_str().unwrap_or("unknown");
        let gap = self
            .previous
            .map(|previous| (at - previous).as_secs_f64() * 1000.0);
        self.previous = Some(at);
        emit(
            out,
            at,
            wall,
            json!({"kind":"event", "event_type":kind, "event_gap_ms":gap, "event":event}),
        )?;
        if kind.ends_with(".delta")
            && let Some(text) = event["delta"].as_str().filter(|text| !text.is_empty())
        {
            if kind == "response.output_text.delta" {
                if self.first_text.is_none() {
                    self.first_text = Some(at);
                    emit(out, at, wall, json!({"kind":"first_text"}))?;
                }
                if let Some(last) = self.last_text {
                    self.longest_gap_ms =
                        self.longest_gap_ms.max((at - last).as_secs_f64() * 1000.0);
                }
                self.last_text = Some(at);
                self.chars += text.chars().count();
                self.deltas += 1;
            }
            for (index, ch) in text.chars().enumerate() {
                emit(
                    out,
                    at,
                    wall,
                    json!({"kind":"char", "event_type":kind, "delta_char_index":index, "char":ch.to_string()}),
                )?;
            }
        }
        if kind == "response.output_text.done" {
            emit(
                out,
                at,
                wall,
                json!({"kind":"text_done", "item_id":event["item_id"], "content_index":event["content_index"]}),
            )?;
        }
        if matches!(
            kind,
            "response.completed" | "response.incomplete" | "response.failed" | "error"
        ) {
            emit(
                out,
                at,
                wall,
                json!({
                    "kind":"response_end", "terminal_event":kind,
                    "first_text_ms":self.first_text.map(ms), "last_text_ms":self.last_text.map(ms),
                    "text_span_ms":self.first_text.zip(self.last_text).map(|(first,last)| ms(last-first)),
                    "text_deltas":self.deltas, "text_chars":self.chars,
                    "longest_text_gap_ms":self.longest_gap_ms,
                    "usage":event.pointer("/response/usage"),
                }),
            )?;
            if matches!(kind, "response.failed" | "error") {
                bail!("backend reported {kind}; see timestamped event");
            }
            return Ok(true);
        }
        Ok(false)
    }
}

fn ms(at: Duration) -> f64 {
    at.as_secs_f64() * 1000.0
}

fn emit(out: &mut impl Write, at: Duration, wall: &str, mut row: Value) -> Result<()> {
    row["at"] = json!(wall);
    row["elapsed_ms"] = json!(ms(at));
    let encoded = serde_json::to_string(&row)?;
    writeln!(out, "{}", redact::apply(&encoded))?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(180))
        .build()?;
    let auth = auth::load(&http).await?;
    let mut request = http
        .post("https://chatgpt.com/backend-api/codex/responses")
        .bearer_auth(&auth.access_token)
        .header("originator", "codex_cli_rs")
        .header("User-Agent", "bhai-stream-timing/0.0.1 (codex_cli_rs)")
        .header("OpenAI-Beta", "responses=experimental")
        .header("Accept", "text/event-stream");
    if let Some(account) = &auth.account_id {
        request = request.header("ChatGPT-Account-ID", account);
    }
    let request = request.json(&json!({
        "model":args.model, "instructions":"Follow the user's request.",
        "input":[{"role":"user", "content":args.prompt}],
        "reasoning":{"effort":args.effort, "summary":"auto"},
        "tools":[], "stream":true, "store":false,
    }));
    let mut out = io::BufWriter::new(io::stdout());
    let mut probe = Probe::new();
    emit(
        &mut out,
        Duration::ZERO,
        &Local::now().to_rfc3339(),
        json!({"kind":"request_start", "model":args.model, "transport":"sse"}),
    )?;
    out.flush()?;
    let response = request.send().await?;
    emit(
        &mut out,
        probe.start.elapsed(),
        &Local::now().to_rfc3339(),
        json!({"kind":"headers", "status":response.status().as_u16()}),
    )?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await?;
        bail!("request failed with {status}: {}", redact::apply(&body));
    }
    let mut stream = response.bytes_stream();
    let mut sse = Sse::default();
    let mut terminal = false;
    while let Some(chunk) = stream.next().await {
        let at = probe.start.elapsed();
        let wall = Local::now().to_rfc3339();
        let chunk = chunk.context("reading response stream")?;
        emit(
            &mut out,
            at,
            &wall,
            json!({"kind":"transport_chunk", "bytes":chunk.len()}),
        )?;
        for data in sse.push(&chunk)? {
            if data == "[DONE]" {
                continue;
            }
            terminal |= probe.event(&mut out, at, &wall, serde_json::from_str(&data)?)?;
        }
        out.flush()?;
        if terminal {
            break;
        }
    }
    emit(
        &mut out,
        probe.start.elapsed(),
        &Local::now().to_rfc3339(),
        json!({"kind":"probe_end", "terminal_received":terminal}),
    )?;
    out.flush()?;
    if !terminal {
        bail!("stream closed without a terminal response event");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragmented_unicode_and_multiline_sse() {
        let mut sse = Sse::default();
        let bytes = "data: hello 🌍\r\ndata: world\r\n\r\n".as_bytes();
        let mut events = Vec::new();
        for byte in bytes {
            events.extend(sse.push(&[*byte]).unwrap());
        }
        assert_eq!(events, ["hello 🌍\nworld"]);
        assert!(sse.push(b":keepalive\n\n").unwrap().is_empty());
    }

    #[test]
    fn reasoning_and_empty_deltas_do_not_start_visible_text() {
        let mut probe = Probe::new();
        let mut out = Vec::new();
        for event in [
            json!({"type":"response.reasoning_summary_text.delta", "delta":"thinking"}),
            json!({"type":"response.function_call_arguments.delta", "delta":"{}"}),
            json!({"type":"response.output_text.delta", "delta":""}),
        ] {
            assert!(
                !probe
                    .event(&mut out, Duration::ZERO, "wall", event)
                    .unwrap()
            );
        }
        assert!(probe.first_text.is_none());
        assert_eq!(probe.chars, 0);
        assert!(
            probe
                .event(
                    &mut out,
                    Duration::ZERO,
                    "wall",
                    json!({"type":"response.failed"})
                )
                .is_err()
        );
    }

    #[test]
    fn text_boundaries_are_not_request_boundaries() {
        let mut probe = Probe::new();
        let mut out = Vec::new();
        for (time, event) in [
            (100, json!({"type":"response.created"})),
            (
                500,
                json!({"type":"response.output_text.delta", "delta":"a🌍"}),
            ),
            (
                700,
                json!({"type":"response.output_text.delta", "delta":"b"}),
            ),
            (710, json!({"type":"response.output_text.done"})),
            (
                900,
                json!({"type":"response.completed", "response":{"usage":{"output_tokens":3}}}),
            ),
        ] {
            let done = probe
                .event(&mut out, Duration::from_millis(time), "wall", event)
                .unwrap();
            assert_eq!(done, time == 900);
        }
        let rows: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let chars: Vec<_> = rows.iter().filter(|row| row["kind"] == "char").collect();
        assert_eq!(chars.len(), 3);
        assert_eq!(chars[0]["elapsed_ms"], chars[1]["elapsed_ms"]);
        assert_eq!(chars[1]["char"], "🌍");
        let summary = rows.last().unwrap();
        assert_eq!(summary["first_text_ms"], 500.0);
        assert_eq!(summary["text_span_ms"], 200.0);
        assert_eq!(summary["longest_text_gap_ms"], 200.0);
    }
}
