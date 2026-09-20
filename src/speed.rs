//! How fast the model is answering: output tokens a second over the last few seconds it
//! was actually streaming. Time spent running tools, waiting for an approval or waiting
//! for the next prompt is left out, so the number says how quickly the model writes
//! rather than how quickly the turn is going.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How much streaming time the rate is averaged over.
const WINDOW: Duration = Duration::from_secs(10);

/// Streaming time before the rate is worth showing: below it a single chunk reads as a
/// wild number.
const MIN: Duration = Duration::from_millis(1500);

/// The streaming clock and what arrived on it. The clock runs only between `start` and
/// `end`, so the window spans several calls when tool runs sit between them.
#[derive(Debug, Default)]
pub struct Speed {
    /// Streaming time before the running call.
    banked: Duration,
    /// When the running call started, while one is running.
    started: Option<Instant>,
    /// Tokens seen, each stamped with the clock, back to the start of the window.
    samples: VecDeque<(Duration, u64)>,
    /// Tokens counted from the running call's deltas, for its usage to correct.
    streamed: u64,
}

impl Speed {
    /// A model call was sent. The wait for the first token counts as streaming, since
    /// the model is working through it.
    pub fn start(&mut self, now: Instant) {
        self.started.get_or_insert(now);
    }

    /// The call is over, so the clock stops until the next one is sent.
    pub fn end(&mut self, now: Instant) {
        if let Some(at) = self.started.take() {
            self.banked += now.saturating_duration_since(at);
        }
        self.streamed = 0;
    }

    /// Tokens streamed out of the running call.
    pub fn streamed(&mut self, now: Instant, tokens: u64) {
        if self.started.is_some() {
            self.streamed += tokens;
            self.push(now, tokens);
        }
    }

    /// What the call really produced, which includes the reasoning the stream only
    /// summarised. The difference goes in as if it arrived now, so a model that thinks
    /// for ten seconds and then writes a line does not read as a slow one.
    pub fn reported(&mut self, now: Instant, output: u64) {
        if self.started.is_some() {
            let missing = output.saturating_sub(self.streamed);
            self.streamed = self.streamed.max(output);
            self.push(now, missing);
        }
    }

    /// Tokens a second over the window, or nothing until there is enough of it.
    pub fn rate(&self, now: Instant) -> Option<f64> {
        let clock = self.clock(now);
        let span = clock.min(WINDOW);
        if span < MIN {
            return None;
        }
        let from = clock - span;
        let tokens: u64 = self
            .samples
            .iter()
            .filter(|(at, _)| *at > from)
            .map(|(_, tokens)| tokens)
            .sum();
        (tokens > 0).then(|| tokens as f64 / span.as_secs_f64())
    }

    /// Tokens stamped in the window, for tests that drive events rather than the clock.
    #[cfg(test)]
    pub fn counted(&self) -> u64 {
        self.samples.iter().map(|(_, tokens)| tokens).sum()
    }

    /// Streaming time so far, the running call included.
    fn clock(&self, now: Instant) -> Duration {
        let running = self
            .started
            .map_or(Duration::ZERO, |at| now.saturating_duration_since(at));
        self.banked + running
    }

    /// Stamp `tokens` with the clock and drop what has fallen out of the window.
    fn push(&mut self, now: Instant, tokens: u64) {
        if tokens == 0 {
            return;
        }
        let at = self.clock(now);
        self.samples.push_back((at, tokens));
        let from = at.saturating_sub(WINDOW);
        while self.samples.front().is_some_and(|(at, _)| *at <= from) {
            self.samples.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock the test moves by hand.
    struct Clock(Instant);

    impl Clock {
        fn new() -> Self {
            Clock(Instant::now())
        }

        fn at(&self, secs: f64) -> Instant {
            self.0 + Duration::from_secs_f64(secs)
        }
    }

    #[test]
    fn the_rate_is_tokens_over_the_streaming_time() {
        let clock = Clock::new();
        let mut speed = Speed::default();
        speed.start(clock.at(0.0));
        assert_eq!(speed.rate(clock.at(1.0)), None, "too early to say");
        for i in 1..=4 {
            speed.streamed(clock.at(i as f64), 50);
        }
        // Four seconds of streaming, two hundred tokens.
        assert_eq!(speed.rate(clock.at(4.0)), Some(50.0));
        // The clock keeps running while the model thinks, so the rate falls.
        assert_eq!(speed.rate(clock.at(8.0)), Some(25.0));
    }

    #[test]
    fn a_tool_run_between_calls_is_not_counted() {
        let clock = Clock::new();
        let mut speed = Speed::default();
        speed.start(clock.at(0.0));
        speed.streamed(clock.at(2.0), 100);
        speed.end(clock.at(2.0));
        // A minute of tool output moves the wall clock, not the streaming one.
        speed.start(clock.at(62.0));
        speed.streamed(clock.at(64.0), 100);
        assert_eq!(speed.rate(clock.at(64.0)), Some(50.0), "four seconds of it");
        speed.end(clock.at(64.0));
        // Idle between turns does not decay it either.
        assert_eq!(speed.rate(clock.at(600.0)), Some(50.0));
    }

    #[test]
    fn only_the_last_ten_seconds_of_streaming_count() {
        let clock = Clock::new();
        let mut speed = Speed::default();
        speed.start(clock.at(0.0));
        speed.streamed(clock.at(1.0), 1000);
        for i in 2..=20 {
            speed.streamed(clock.at(i as f64), 10);
        }
        // The burst at one second has fallen out of the window.
        assert_eq!(speed.rate(clock.at(20.0)), Some(10.0));
        assert_eq!(speed.samples.len(), 10);
    }

    #[test]
    fn the_usage_count_makes_up_for_reasoning_the_stream_never_showed() {
        let clock = Clock::new();
        let mut speed = Speed::default();
        speed.start(clock.at(0.0));
        // Ten seconds of thinking, then a two hundred token summary.
        speed.streamed(clock.at(10.0), 200);
        assert_eq!(speed.rate(clock.at(10.0)), Some(20.0));
        speed.reported(clock.at(10.0), 2000);
        assert_eq!(speed.rate(clock.at(10.0)), Some(200.0));
        // A count below what was streamed takes nothing back off.
        speed.reported(clock.at(10.0), 1);
        assert_eq!(speed.rate(clock.at(10.0)), Some(200.0));
    }

    #[test]
    fn nothing_is_counted_outside_a_call() {
        let clock = Clock::new();
        let mut speed = Speed::default();
        speed.streamed(clock.at(1.0), 500);
        speed.reported(clock.at(1.0), 500);
        speed.start(clock.at(2.0));
        speed.streamed(clock.at(4.0), 100);
        assert_eq!(speed.rate(clock.at(4.0)), Some(50.0));
        // Two ends in a row, and a second start, leave the clock alone.
        speed.end(clock.at(4.0));
        speed.end(clock.at(9.0));
        assert_eq!(speed.rate(clock.at(9.0)), Some(50.0));
    }
}
