//! Observed output throughput, measured at the backend rather than the renderer.
//! Samples count complete text streams, not separately tokenized transport chunks.
//! Hidden reasoning and protocol tokens are not observable output.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub(crate) const PERIOD: Duration = Duration::from_millis(250);

/// Source-clock output measurements, also carried to attached clients.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Output {
    /// A new attempt is waiting for its first nonempty output.
    Waiting,
    /// The attempt stopped without another sample, including cancellation or failure.
    Stopped,
    /// Cumulative observed tokens and time since the first output arrived.
    Sample {
        elapsed: Duration,
        tokens: u64,
        finished: bool,
    },
}

/// One attempt's text, sampled no more than four times a second.
#[derive(Debug, Default)]
pub(crate) struct Meter {
    first: Option<Instant>,
    last: Option<Instant>,
    sampled: Option<Instant>,
    streams: HashMap<String, String>,
}

impl Meter {
    pub(crate) fn push(
        &mut self,
        now: Instant,
        model: &str,
        stream: String,
        text: &str,
    ) -> Option<Output> {
        if text.is_empty() {
            return None;
        }
        self.first.get_or_insert(now);
        self.last = Some(now);
        self.streams.entry(stream).or_default().push_str(text);
        if self
            .sampled
            .is_some_and(|at| now.saturating_duration_since(at) < PERIOD)
        {
            return None;
        }
        self.sampled = Some(now);
        self.sample(model, false)
    }

    pub(crate) fn finish(&self, model: &str) -> Option<Output> {
        self.sample(model, true)
    }

    fn sample(&self, model: &str, finished: bool) -> Option<Output> {
        let elapsed = self.last?.saturating_duration_since(self.first?);
        let tokenizer = crate::tokens::for_model(model);
        Some(Output::Sample {
            elapsed,
            tokens: self
                .streams
                .values()
                .map(|text| tokenizer.count(text) as u64)
                .sum(),
            finished,
        })
    }
}

/// The latest source measurement and the local time it was delivered.
#[derive(Debug, Default)]
pub struct Speed {
    accepting: bool,
    latest: Option<(Duration, u64, bool)>,
    received: Option<Instant>,
    previous: (Duration, u64),
    recent: Option<(Duration, u64)>,
    sent: Option<(Instant, u64)>,
    read: Option<(u64, Duration)>,
    prompt: u64,
}

impl Speed {
    pub fn sending(&mut self, now: Instant, tokens: u64) {
        self.prompt = tokens;
        self.sent = Some((now, tokens));
        self.clear_output();
    }

    /// Request lifetime is only a guard, not the output clock.
    pub fn start(&mut self, _now: Instant) {
        self.accepting = true;
    }

    /// Errors and cancellation freeze at the last observed output, not at cleanup.
    pub fn end(&mut self, _now: Instant) {
        self.accepting = false;
        self.sent = None;
        if let Some((_, _, finished)) = &mut self.latest {
            *finished = true;
        }
    }

    pub fn observe(&mut self, now: Instant, output: Output) {
        if !self.accepting {
            return;
        }
        match output {
            Output::Waiting => {
                self.clear_output();
                self.sent.get_or_insert((now, self.prompt));
            }
            Output::Stopped => {
                self.sent = None;
                if let Some((_, _, finished)) = &mut self.latest {
                    *finished = true;
                }
            }
            Output::Sample {
                elapsed,
                tokens,
                finished,
            } => {
                if tokens == 0 && finished {
                    self.sent = None;
                }
                if let Some((sent, prompt)) = self.sent.take() {
                    self.read = Some((prompt, now.saturating_duration_since(sent)));
                }
                if elapsed > self.previous.0 {
                    self.recent = Some((
                        elapsed - self.previous.0,
                        tokens.saturating_sub(self.previous.1),
                    ));
                }
                self.previous = (elapsed, tokens);
                self.latest = Some((elapsed, tokens, finished));
                self.received = Some(now);
            }
        }
    }

    /// Live readings use source sample intervals. Silence decays the rate without
    /// declaring the response finished; a finished call keeps its observed average.
    pub fn rate(&self, now: Instant) -> Option<f64> {
        let (elapsed, tokens, finished) = self.latest?;
        if finished {
            return (elapsed >= PERIOD).then(|| tokens as f64 / elapsed.as_secs_f64());
        }
        let age = now.saturating_duration_since(self.received?);
        let (span, tokens) = self.recent?;
        let span = span + age.saturating_sub(PERIOD);
        Some(tokens as f64 / span.as_secs_f64())
    }

    pub fn finished(&self) -> bool {
        self.latest.is_some_and(|(_, _, finished)| finished)
    }

    pub fn quiet(&self, now: Instant) -> bool {
        self.latest.is_some_and(|(_, _, finished)| !finished)
            && self
                .received
                .is_some_and(|at| now.saturating_duration_since(at) >= PERIOD * 2)
    }

    pub fn reading_prompt(&self, now: Instant) -> Option<(u64, Duration)> {
        let (at, tokens) = self.sent?;
        Some((tokens, now.saturating_duration_since(at)))
    }

    /// This includes queueing, reasoning and network time, not just prompt evaluation.
    pub fn prompt_rate(&self) -> Option<f64> {
        let (tokens, took) = self.read?;
        (took > Duration::ZERO).then(|| tokens as f64 / took.as_secs_f64())
    }

    fn clear_output(&mut self) {
        self.latest = None;
        self.received = None;
        self.previous = (Duration::ZERO, 0);
        self.recent = None;
    }

    #[cfg(test)]
    pub fn counted(&self) -> u64 {
        self.latest.map_or(0, |(_, tokens, _)| tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ms: u64, tokens: u64, finished: bool) -> Output {
        Output::Sample {
            elapsed: Duration::from_millis(ms),
            tokens,
            finished,
        }
    }

    #[test]
    fn request_waits_tools_and_completion_bookkeeping_are_not_output_time() {
        let now = Instant::now();
        let mut speed = Speed::default();
        speed.sending(now, 1000);
        speed.start(now);
        assert_eq!(speed.rate(now + Duration::from_secs(4)), None);
        speed.observe(now + Duration::from_secs(4), sample(0, 1, false));
        speed.observe(now + Duration::from_secs(5), sample(1000, 51, true));
        assert_eq!(speed.rate(now + Duration::from_secs(50)), Some(51.0));
        speed.end(now + Duration::from_secs(50));
        speed.sending(now + Duration::from_secs(100), 2000);
        speed.start(now + Duration::from_secs(100));
        assert_eq!(speed.rate(now + Duration::from_secs(110)), None);
        speed.observe(now + Duration::from_secs(110), sample(0, 1, false));
        speed.observe(now + Duration::from_secs(111), sample(1000, 31, true));
        assert_eq!(speed.rate(now + Duration::from_secs(120)), Some(31.0));
    }

    #[test]
    fn silence_does_not_end_output_and_a_retry_resets_it() {
        let now = Instant::now();
        let mut speed = Speed::default();
        speed.start(now);
        speed.observe(now, sample(0, 1, false));
        speed.observe(now + PERIOD, sample(250, 15, false));
        let quiet = now + Duration::from_secs(2);
        assert!(speed.rate(quiet).unwrap() < 56.0);
        assert!(speed.quiet(quiet));
        assert!(!speed.finished());
        speed.observe(quiet, sample(2000, 16, false));
        assert!(!speed.quiet(quiet));
        speed.observe(quiet + PERIOD, sample(2250, 30, false));
        assert_eq!(speed.rate(quiet + PERIOD), Some(56.0));
        speed.observe(quiet + PERIOD, Output::Waiting);
        assert_eq!(speed.rate(quiet + PERIOD), None);
    }

    #[test]
    fn buffered_delivery_uses_source_time_and_short_output_has_no_invented_rate() {
        let now = Instant::now();
        let mut speed = Speed::default();
        speed.start(now);
        speed.observe(now, sample(0, 1, false));
        speed.observe(now, sample(1000, 51, false));
        assert_eq!(speed.rate(now), Some(50.0));
        speed.observe(now, sample(1000, 51, true));
        assert_eq!(speed.rate(now), Some(51.0));
        speed.observe(now, Output::Waiting);
        speed.observe(now, sample(0, 20, true));
        assert_eq!(speed.rate(now), None);
        speed.end(now);
        speed.observe(now, sample(1000, 999, true));
        assert_eq!(speed.counted(), 20);
    }

    #[test]
    fn the_recorded_counting_reply_excludes_its_completion_tail() {
        let now = Instant::now();
        let first = now + Duration::from_micros(916500);
        let last = now + Duration::from_micros(1915481);
        let terminal = now + Duration::from_micros(2225472);
        let text = (1..=30)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let mut meter = Meter::default();
        let mut speed = Speed::default();
        speed.start(now);
        speed.observe(
            first,
            meter.push(first, "gpt-6-luna", "text".into(), "1").unwrap(),
        );
        speed.observe(
            last,
            meter
                .push(last, "gpt-6-luna", "text".into(), &text[1..])
                .unwrap(),
        );
        speed.observe(terminal, meter.finish("gpt-6-luna").unwrap());
        let tokens = crate::tokens::for_model("gpt-6-luna").count(&text);
        let expected = tokens as f64 / last.duration_since(first).as_secs_f64();
        assert_eq!(
            speed.rate(terminal + Duration::from_secs(2)),
            Some(expected)
        );
        assert!(speed.finished());
        assert!(!speed.quiet(terminal + Duration::from_secs(2)));
    }

    #[test]
    fn complete_streams_are_counted_independently_of_chunk_boundaries() {
        let now = Instant::now();
        let mut meter = Meter::default();
        assert!(meter.push(now, "gpt-6-luna", "text".into(), "").is_none());
        for (index, ch) in "hello world".chars().enumerate() {
            meter.push(
                now + Duration::from_millis(index as u64 * 100),
                "gpt-6-luna",
                "text".into(),
                &ch.to_string(),
            );
        }
        meter.push(
            now + Duration::from_secs(1),
            "gpt-6-luna",
            "tool".into(),
            "{}",
        );
        assert_eq!(meter.finish("gpt-6-luna"), Some(sample(1000, 3, true)));
        let mut bytes = Meter::default();
        for ch in "hello".chars() {
            bytes.push(now, "ollama:unknown", "text".into(), &ch.to_string());
        }
        assert_eq!(bytes.finish("ollama:unknown"), Some(sample(0, 2, true)));
    }
}
