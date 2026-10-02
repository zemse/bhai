//! Spans at the loop's boundaries (a turn, a model call, a tool call, a child), so a
//! session can say where its time went. `--profile` writes each closed span as a line of
//! `.bhai/debug/trace.jsonl`; a build with the `otel` feature also exports them over
//! OTLP/HTTP when `OTEL_EXPORTER_OTLP_ENDPOINT` is set.
//!
//! A span carries names, ids, counts and timings, never a prompt, a tool's arguments or
//! its output: whatever reads the file or the collector sees the shape of the session,
//! not what was said in it. Only this crate's spans are kept, since a dependency's could
//! carry a request body.

use std::fs::File;
use std::io::Write as _;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Span, Subscriber, info_span};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;

/// Keeps the exporter alive; dropping it flushes what is still batched.
pub struct Guard {
    #[cfg(feature = "otel")]
    provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        #[cfg(feature = "otel")]
        if let Some(provider) = self.provider.take() {
            let _ = provider.shutdown();
        }
    }
}

/// Install the subscriber for this process. Nothing is installed when there is nowhere
/// to send a span, so a run without `--profile` pays nothing for them.
pub fn init(file: Option<&Path>) -> Result<Guard> {
    let file = match file {
        Some(path) => Some(Lines::open(path)?),
        None => None,
    };
    #[cfg(feature = "otel")]
    let (otel, provider) = match otlp()? {
        Some((layer, provider)) => (Some(layer), Some(provider)),
        None => (None, None),
    };
    #[cfg(not(feature = "otel"))]
    let otel: Option<tracing_subscriber::layer::Identity> = None;
    if file.is_none() && otel.is_none() {
        return Ok(Guard {
            #[cfg(feature = "otel")]
            provider,
        });
    }
    let subscriber = tracing_subscriber::registry()
        .with(file.with_filter(ours()))
        .with(otel.with_filter(ours()));
    tracing::subscriber::set_global_default(subscriber).context("could not install tracing")?;
    Ok(Guard {
        #[cfg(feature = "otel")]
        provider,
    })
}

fn ours() -> tracing_subscriber::filter::FilterFn<impl Fn(&tracing::Metadata<'_>) -> bool> {
    tracing_subscriber::filter::filter_fn(|meta| meta.target().starts_with("bhai"))
}

/// The OTLP layer, when the standard environment names a collector. The exporter reads
/// the endpoint and headers from that environment itself.
#[cfg(feature = "otel")]
#[allow(clippy::type_complexity)]
fn otlp<S>() -> Result<
    Option<(
        tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>,
        opentelemetry_sdk::trace::SdkTracerProvider,
    )>,
>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    use opentelemetry::trace::TracerProvider as _;
    let set = |name| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    if !set("OTEL_EXPORTER_OTLP_ENDPOINT") && !set("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT") {
        return Ok(None);
    }
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .build()
        .context("could not build the OTLP exporter")?;
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_service_name("bhai")
                .build(),
        )
        .build();
    let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("bhai"));
    Ok(Some((layer, provider)))
}

/// The span a child agent runs in. It is a root of its own rather than a child of the
/// call that started it, since a child outlives that call and its turn, and a parent
/// span would stay open until it ended; it follows from the call instead, which is the
/// lineage. Made where that call is the current span, before the child is spawned.
pub fn child(id: &str, identity: &str) -> Span {
    let span = info_span!(parent: None, "child", child.id = id, agent.identity = identity);
    span.follows_from(Span::current());
    span
}

/// Writes each span as one JSON line when it closes.
pub struct Lines {
    out: Mutex<File>,
    next: AtomicU64,
}

/// What a span has collected while open. `seq` numbers spans for the life of the file,
/// where a subscriber's ids are reused once a span closes.
struct Open {
    seq: u64,
    root: u64,
    start: chrono::DateTime<chrono::Local>,
    at: Instant,
    fields: Map<String, Value>,
    follows: Vec<u64>,
}

impl Lines {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            crate::sessions::private_dir(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        let out = crate::sessions::private_append(path)
            .with_context(|| format!("could not open {}", path.display()))?;
        Ok(Self {
            out: Mutex::new(out),
            next: AtomicU64::new(1),
        })
    }
}

struct Fields<'a>(&'a mut Map<String, Value>);

impl Visit for Fields<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), json!(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_string(), json!(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_string(), json!(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_string(), json!(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.insert(field.name().to_string(), json!(value));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), json!(format!("{value:?}")));
    }
}

impl<S> Layer<S> for Lines
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: LayerContext<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let seq = self.next.fetch_add(1, Ordering::Relaxed);
        let root = span
            .parent()
            .and_then(|parent| parent.extensions().get::<Open>().map(|open| open.root))
            .unwrap_or(seq);
        let mut fields = Map::new();
        attrs.record(&mut Fields(&mut fields));
        span.extensions_mut().insert(Open {
            seq,
            root,
            start: chrono::Local::now(),
            at: Instant::now(),
            fields,
            follows: Vec::new(),
        });
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: LayerContext<'_, S>) {
        if let Some(span) = ctx.span(id)
            && let Some(open) = span.extensions_mut().get_mut::<Open>()
        {
            values.record(&mut Fields(&mut open.fields));
        }
    }

    fn on_follows_from(&self, id: &Id, follows: &Id, ctx: LayerContext<'_, S>) {
        let Some(from) = ctx
            .span(follows)
            .and_then(|span| span.extensions().get::<Open>().map(|open| open.seq))
        else {
            return;
        };
        if let Some(span) = ctx.span(id)
            && let Some(open) = span.extensions_mut().get_mut::<Open>()
        {
            open.follows.push(from);
        }
    }

    fn on_close(&self, id: Id, ctx: LayerContext<'_, S>) {
        let Some(span) = ctx.span(&id) else {
            return;
        };
        let parent = span
            .parent()
            .and_then(|parent| parent.extensions().get::<Open>().map(|open| open.seq));
        let Some(open) = span.extensions_mut().remove::<Open>() else {
            return;
        };
        let mut line = json!({
            "name": span.name(),
            "id": open.seq,
            "parent": parent,
            "root": open.root,
            "start": open.start.to_rfc3339(),
            "ms": open.at.elapsed().as_secs_f64() * 1000.0,
            "fields": open.fields,
        });
        if !open.follows.is_empty() {
            line["follows"] = json!(open.follows);
        }
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(out, "{line}");
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Spans recorded into a file while the guard is held, on this thread only.
    pub struct Recorded {
        pub path: std::path::PathBuf,
        _guard: tracing::subscriber::DefaultGuard,
    }

    impl Recorded {
        pub fn start() -> Self {
            // With a single dispatcher alive, tracing works out a callsite's interest
            // from whichever thread hits it first, and another test's thread, which has
            // none, would switch the span off for good. A second one that never goes
            // away makes it ask every dispatcher.
            static OTHER: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
            OTHER.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new()));
            let path = crate::tools::temp_dir().join("debug").join("trace.jsonl");
            let lines = Lines::open(&path).unwrap();
            let subscriber = tracing_subscriber::registry().with(lines.with_filter(ours()));
            let guard = tracing::subscriber::set_default(subscriber);
            Self {
                path,
                _guard: guard,
            }
        }

        pub fn spans(&self) -> Vec<Value> {
            std::fs::read_to_string(&self.path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    #[test]
    fn a_closed_span_is_one_line_with_its_parent_and_fields() {
        let recorded = Recorded::start();
        {
            let turn = info_span!(parent: None, "turn", steps = tracing::field::Empty);
            let _in = turn.enter();
            let call = info_span!("tool.call", tool = "bash", ok = tracing::field::Empty);
            call.record("ok", true);
            turn.record("steps", 2u64);
        }
        let spans = recorded.spans();
        assert_eq!(spans.len(), 2, "{spans:?}");
        let (call, turn) = (&spans[0], &spans[1]);
        assert_eq!(call["name"], "tool.call");
        assert_eq!(call["fields"], json!({"tool": "bash", "ok": true}));
        assert_eq!(call["parent"], turn["id"]);
        assert_eq!(call["root"], turn["id"]);
        assert_eq!(turn["parent"], Value::Null);
        assert_eq!(turn["fields"]["steps"], 2);
        assert!(turn["ms"].as_f64().unwrap() >= 0.0);
        let mode = std::fs::metadata(&recorded.path).unwrap();
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
            0o600
        );
    }

    #[test]
    fn a_child_is_a_root_that_follows_from_the_call_that_started_it() {
        let recorded = Recorded::start();
        let child = {
            let call = info_span!("tool.call", tool = "agent");
            let _in = call.enter();
            child("a1b2c3", "explorer")
        };
        drop(child);
        let spans = recorded.spans();
        let call = spans.iter().find(|s| s["name"] == "tool.call").unwrap();
        let child = spans.iter().find(|s| s["name"] == "child").unwrap();
        assert_eq!(child["parent"], Value::Null);
        assert_eq!(child["root"], child["id"]);
        assert_eq!(child["follows"], json!([call["id"]]));
        assert_eq!(child["fields"]["child.id"], "a1b2c3");
    }

    #[test]
    fn a_dependencys_spans_are_not_written() {
        let recorded = Recorded::start();
        drop(tracing::info_span!(target: "hyper::client", "request", body = "secret"));
        drop(info_span!("turn"));
        let spans = recorded.spans();
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert_eq!(spans[0]["name"], "turn");
    }
}
