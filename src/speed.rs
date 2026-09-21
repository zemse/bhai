//! How fast the model is answering: tokens a second over the last few seconds it was
//! actually writing. It is measured between one token and another, so the time spent
//! running tools, waiting for an approval, thinking before the first token or sitting
//! idle after the last is not in it at all. What it counts is what comes out on screen,
//! so the number says what the text is doing.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How much streaming time the rate is averaged over.
const WINDOW: Duration = Duration::from_secs(10);

/// Writing between two tokens before the rate is worth showing. Short, because the
/// span is measured token to token and is honest at any length: it is only there so a
/// single chunk landing either side of a very short span cannot read as a wild number.
/// Longer, and the readout stays blank through the opening seconds of every answer,
/// which is when it is being looked at.
const MIN: Duration = Duration::from_millis(500);

/// The streaming clock and what arrived on it. The clock runs only between `start` and
/// `end`, so the window spans several calls when tool runs sit between them.
#[derive(Debug, Default)]
pub struct Speed {
    /// Streaming time before the running call.
    banked: Duration,
    /// When the running call started, while one is running.
    started: Option<Instant>,
    /// Tokens seen, each stamped with the clock. The front is the window's left edge:
    /// the newest stamp at or before it, kept so the span the rest cover is known.
    samples: VecDeque<(Duration, u64)>,
}

impl Speed {
    /// A model call was sent, so the clock runs again. The wait for the first token
    /// runs it too, which only matters for where later tokens are stamped: nothing is
    /// measured over a stretch no token arrived in.
    pub fn start(&mut self, now: Instant) {
        self.started.get_or_insert(now);
    }

    /// The call is over, so the clock stops until the next one is sent.
    pub fn end(&mut self, now: Instant) {
        if let Some(at) = self.started.take() {
            self.banked += now.saturating_duration_since(at);
        }
    }

    /// Tokens streamed out of the running call. The reasoning a call reports but never
    /// streamed is not among them: it was never on screen, and a rate that counted it
    /// would read faster than the text it is describing.
    pub fn streamed(&mut self, now: Instant, tokens: u64) {
        if self.started.is_some() {
            self.push(now, tokens);
        }
    }

    /// Tokens a second over the window, or nothing until there is enough of it.
    ///
    /// The span runs from one token to another, never from the clock to a token: the
    /// front sample is the edge, and what it carried arrived before the span opens, so
    /// only the rest are counted. Measured from the clock instead, the seconds a model
    /// spends thinking before it writes anything sit in the denominator with nothing
    /// against them, and a stream at a flat rate reads far below it and climbs for a
    /// whole window, which is not what the text on screen is doing.
    ///
    /// The window ends at the last token rather than at now, so silence holds the
    /// number where it was: a model that has stopped writing is not writing slowly.
    pub fn rate(&self) -> Option<f64> {
        let (last, _) = *self.samples.back()?;
        let (first, _) = *self.samples.front()?;
        let span = (last - first).min(WINDOW);
        if span < MIN {
            return None;
        }
        let tokens: u64 = self.samples.iter().skip(1).map(|(_, n)| n).sum();
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

    /// Stamp `tokens` with the clock and drop what has fallen out of the window, save
    /// for the newest stamp at or before it: that one is the edge the rest are measured
    /// from, and its own tokens are not counted again.
    fn push(&mut self, now: Instant, tokens: u64) {
        if tokens == 0 {
            return;
        }
        let at = self.clock(now);
        match self.samples.back_mut() {
            // The same instant is the same sample; two of them span no time at all.
            Some((back, count)) if *back == at => *count += tokens,
            _ => self.samples.push_back((at, tokens)),
        }
        let from = at.saturating_sub(WINDOW);
        while self.samples.len() > 1 && self.samples[1].0 <= from {
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
        assert_eq!(speed.rate(), None, "too early to say");
        for i in 1..=4 {
            speed.streamed(clock.at(i as f64), 50);
        }
        // Four seconds of streaming, two hundred tokens.
        assert_eq!(speed.rate(), Some(50.0));
    }

    #[test]
    fn a_steady_stream_reads_steady_from_the_first_reading() {
        // It used to be measured from the clock to the last token, so the seconds
        // before the first one sat in the denominator with nothing against them: a
        // stream at a flat 28 tok/s started near 1 and took a whole window to climb
        // to 28, which is not what the text on screen was doing.
        let clock = Clock::new();
        let mut speed = Speed::default();
        speed.start(clock.at(0.0));
        // Three seconds of nothing, then twenty-eight tokens every second.
        let token = |n: f64| clock.at(3.0 + n);
        speed.streamed(token(0.0), 28);
        // One token is no span to measure over.
        assert_eq!(speed.rate(), None);
        for n in 1..=20 {
            speed.streamed(token(n as f64), 28);
            assert_eq!(speed.rate(), Some(28.0), "at {n} seconds of writing");
        }
    }

    #[test]
    fn silence_holds_the_rate_rather_than_decaying_it() {
        let clock = Clock::new();
        let mut speed = Speed::default();
        speed.start(clock.at(0.0));
        for i in 1..=4 {
            speed.streamed(clock.at(i as f64), 50);
        }
        // A minute of thinking, or of a request being retried, moves the streaming clock
        // but brings no token with it, and the rate stays where the last one left it.
        assert_eq!(speed.rate(), Some(50.0));
        // It starts writing again, and only then does the quiet stretch count.
        speed.streamed(clock.at(64.0), 50);
        assert_eq!(speed.rate(), Some(5.0));
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
        assert_eq!(speed.rate(), Some(50.0), "four seconds of it");
        speed.end(clock.at(64.0));
        // Idle between turns does not decay it either.
        assert_eq!(speed.rate(), Some(50.0));
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
        assert_eq!(speed.rate(), Some(10.0));
        // Ten seconds of samples, and the one at the edge they are measured from.
        assert_eq!(speed.samples.len(), 11);
    }

    #[test]
    fn thinking_before_the_first_token_is_not_in_the_denominator() {
        let clock = Clock::new();
        let mut speed = Speed::default();
        speed.start(clock.at(0.0));
        // Ten seconds of thinking, then a summary written at fifty a second. The
        // thinking is not slow writing, it is not writing, so it is not in the number.
        speed.streamed(clock.at(10.0), 50);
        assert_eq!(speed.rate(), None, "one token spans nothing");
        speed.streamed(clock.at(12.0), 100);
        assert_eq!(speed.rate(), Some(50.0));
    }

    #[test]
    fn nothing_is_counted_outside_a_call() {
        let clock = Clock::new();
        let mut speed = Speed::default();
        speed.streamed(clock.at(1.0), 500);
        speed.start(clock.at(2.0));
        speed.streamed(clock.at(4.0), 100);
        speed.streamed(clock.at(6.0), 100);
        assert_eq!(speed.rate(), Some(50.0));
        // Two ends in a row, and a second start, leave the clock alone.
        speed.end(clock.at(4.0));
        speed.end(clock.at(9.0));
        assert_eq!(speed.rate(), Some(50.0));
    }
}
