//! How fast the model is answering: the tokens that arrived in the last reading, scaled
//! to a second. A reading is short and the next one starts as it closes, so the number
//! says what the stream is doing now rather than what it averaged over the last several
//! seconds. It is measured on a clock that runs only while a call is streaming, so the
//! time spent running tools, waiting for an approval or sitting idle between turns is
//! not in it at all. What it counts is what comes out on screen.
//!
//! The other half of a call is the wait before it: the backend reading the prompt. No
//! backend says how far through one it is, so there is no progress to show, only how
//! much it was handed and how long it has been holding it. The moment the first token
//! lands both are known, and the two of them are a rate.

use std::time::{Duration, Instant};

/// How much streaming one reading covers, and so how often the number changes. Shorter
/// follows the stream more closely and swings more between chunks; the readout is
/// scaled to a second either way, so this can move without anything else moving.
pub(crate) const PERIOD: Duration = Duration::from_millis(250);

/// The streaming clock and what arrived on it. The clock runs only between `start` and
/// `end`, so readings carry on across calls when tool runs sit between them.
#[derive(Debug, Default)]
pub struct Speed {
    /// Streaming time before the running call.
    banked: Duration,
    /// When the running call started, while one is running.
    started: Option<Instant>,
    /// The clock at the first token, which the readings are counted off. Anchored there
    /// rather than at zero so the opening reading is a whole one of writing: a model
    /// that thinks for three seconds first would otherwise have its first number taken
    /// over whatever was left of the reading the first token landed in.
    origin: Option<Duration>,
    /// The reading `current` is filling.
    reading: u64,
    /// Tokens in it so far.
    current: u64,
    /// Tokens in the reading that closed last, which is the one shown. None until one
    /// has closed.
    last: Option<u64>,
    /// The prompt of the running call and when it went out, until its first token.
    sent: Option<(Instant, u64)>,
    /// The prompt of the last call that answered, and how long it took to say anything.
    read: Option<(u64, Duration)>,
}

impl Speed {
    /// A model call went out with `tokens` of prompt behind it. Until its first token
    /// comes back, that prompt is what the backend is working through.
    pub fn sending(&mut self, now: Instant, tokens: u64) {
        self.sent = Some((now, tokens));
    }

    /// A model call was sent, so the clock runs again.
    pub fn start(&mut self, now: Instant) {
        self.started.get_or_insert(now);
    }

    /// The call is over, so the clock stops until the next one is sent.
    pub fn end(&mut self, now: Instant) {
        if let Some(at) = self.started.take() {
            self.banked += now.saturating_duration_since(at);
        }
        // A call that ended without a token was never read: an interrupt, or an error.
        self.sent = None;
    }

    /// Tokens streamed out of the running call. The reasoning a call reports but never
    /// streamed is not among them: it was never on screen, and a rate that counted it
    /// would read faster than the text it is describing.
    pub fn streamed(&mut self, now: Instant, tokens: u64) {
        if self.started.is_some() {
            self.push(now, tokens);
        }
    }

    /// Tokens a second: what the last closed reading carried, over how long a reading
    /// is. Nothing until one has closed, so the readout appears with the first number
    /// it has rather than sitting at zero while the first reading fills.
    ///
    /// A stream that has stopped reads zero once the reading it stopped in has closed
    /// and another has gone by. It is only the streaming clock that has to move for
    /// that, so a tool run or an idle prompt leaves the last number where it was.
    pub fn rate(&self, now: Instant) -> Option<f64> {
        self.origin?;
        let tokens = match self
            .reading_at(self.clock(now))
            .saturating_sub(self.reading)
        {
            // Still filling the one `current` is in, so what is shown is the one before.
            0 => self.last?,
            // It has just closed, and nothing has arrived since.
            1 => self.current,
            // Whole readings have gone by without a token in them.
            _ => 0,
        };
        Some(tokens as f64 / PERIOD.as_secs_f64())
    }

    /// The prompt the running call is still being read, and how long that has taken so
    /// far. Nothing once its first token has come back, and nothing between calls.
    pub fn reading_prompt(&self, now: Instant) -> Option<(u64, Duration)> {
        let (at, tokens) = self.sent?;
        Some((tokens, now.saturating_duration_since(at)))
    }

    /// Prompt tokens a second over the wait for the last call's first token. It is the
    /// whole wait, so the queue and the network are in it as well as the reading: it is
    /// what the user sat through, not what the backend would claim for itself.
    pub fn prompt_rate(&self) -> Option<f64> {
        let (tokens, took) = self.read?;
        (took > Duration::ZERO).then(|| tokens as f64 / took.as_secs_f64())
    }

    /// Tokens in the readings still being counted, for tests that drive events rather
    /// than the clock.
    #[cfg(test)]
    pub fn counted(&self) -> u64 {
        self.current + self.last.unwrap_or(0)
    }

    /// Streaming time so far, the running call included.
    fn clock(&self, now: Instant) -> Duration {
        let running = self
            .started
            .map_or(Duration::ZERO, |at| now.saturating_duration_since(at));
        self.banked + running
    }

    /// Which reading a point on the streaming clock falls in.
    fn reading_at(&self, at: Duration) -> u64 {
        let origin = self.origin.unwrap_or(at);
        (at.saturating_sub(origin).as_nanos() / PERIOD.as_nanos()) as u64
    }

    /// Count `tokens` into the reading the clock is in now, closing the one being
    /// filled if it has moved on.
    fn push(&mut self, now: Instant, tokens: u64) {
        if tokens == 0 {
            return;
        }
        if let Some((sent, prompt)) = self.sent.take() {
            self.read = Some((prompt, now.saturating_duration_since(sent)));
        }
        let at = self.clock(now);
        self.origin.get_or_insert(at);
        let reading = self.reading_at(at);
        if reading != self.reading {
            // Only the reading immediately before this one is the one to show; if whole
            // readings went by in between, the last of them carried nothing.
            self.last = Some(if reading == self.reading + 1 {
                self.current
            } else {
                0
            });
            self.current = 0;
            self.reading = reading;
        }
        self.current += tokens;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock the test moves by hand, in readings rather than seconds: what the tests
    /// are about is where a token falls relative to a reading, and that is what has to
    /// hold when `PERIOD` changes.
    struct Clock(Instant);

    impl Clock {
        fn new() -> Self {
            Clock(Instant::now())
        }

        fn at(&self, readings: f64) -> Instant {
            self.0 + PERIOD.mul_f64(readings)
        }
    }

    /// What a reading carrying `tokens` reads as, once scaled to a second.
    fn shows(tokens: u64) -> Option<f64> {
        Some(tokens as f64 / PERIOD.as_secs_f64())
    }

    #[test]
    fn the_rate_is_what_the_last_reading_carried() {
        let clock = Clock::new();
        let mut speed = Speed::default();
        speed.start(clock.at(0.0));
        assert_eq!(speed.rate(clock.at(0.0)), None, "nothing has streamed");
        speed.streamed(clock.at(0.2), 10);
        speed.streamed(clock.at(0.6), 15);
        assert_eq!(
            speed.rate(clock.at(0.8)),
            None,
            "the first one is still filling"
        );
        // It closes, and what it carried is the number.
        speed.streamed(clock.at(1.2), 1);
        assert_eq!(speed.rate(clock.at(1.2)), shows(25));
    }

    #[test]
    fn the_number_holds_through_a_reading_and_changes_at_the_next() {
        let clock = Clock::new();
        let mut speed = Speed::default();
        speed.start(clock.at(0.0));
        speed.streamed(clock.at(0.0), 25);
        speed.streamed(clock.at(1.0), 40);
        // What is shown through the second reading is what the first one carried, even
        // as the second fills: a number that moved with every frame would not be read.
        for step in 0..5 {
            let now = clock.at(1.0 + f64::from(step) * 0.2);
            assert_eq!(speed.rate(now), shows(25), "part way through the second");
        }
        // The second closes on the clock alone, with no token to close it.
        assert_eq!(speed.rate(clock.at(2.0)), shows(40));
    }

    #[test]
    fn a_stream_that_stops_reads_zero_rather_than_hanging() {
        let clock = Clock::new();
        let mut speed = Speed::default();
        speed.start(clock.at(0.0));
        speed.streamed(clock.at(0.0), 30);
        assert_eq!(speed.rate(clock.at(1.0)), shows(30));
        // A reading with nothing in it is a stream writing nothing, and says so.
        assert_eq!(speed.rate(clock.at(2.0)), shows(0));
        assert_eq!(speed.rate(clock.at(18.0)), shows(0));
        // Writing again picks it straight back up.
        speed.streamed(clock.at(18.2), 20);
        speed.streamed(clock.at(19.2), 1);
        assert_eq!(speed.rate(clock.at(19.2)), shows(20));
    }

    #[test]
    fn thinking_before_the_first_token_is_not_in_the_first_reading() {
        let clock = Clock::new();
        let mut speed = Speed::default();
        speed.start(clock.at(0.0));
        // Six and a bit readings of thinking, then twenty tokens.
        speed.streamed(clock.at(6.6), 20);
        // The first reading runs from that first token, so all twenty are in it rather
        // than only those that fell in what was left of the reading it landed in.
        assert_eq!(speed.rate(clock.at(7.6)), shows(20));
    }

    #[test]
    fn a_tool_run_between_calls_is_not_counted() {
        let clock = Clock::new();
        let mut speed = Speed::default();
        speed.start(clock.at(0.0));
        speed.streamed(clock.at(0.0), 30);
        speed.streamed(clock.at(1.0), 10);
        speed.end(clock.at(1.0));
        // Minutes of tool output move the wall clock, not the streaming one, so the
        // reading it stopped in is still the one being filled.
        assert_eq!(speed.rate(clock.at(500.0)), shows(30));
        speed.start(clock.at(500.0));
        speed.streamed(clock.at(500.2), 5);
        assert_eq!(
            speed.rate(clock.at(500.2)),
            shows(30),
            "the same reading still"
        );
        // One reading of streaming has passed over the two calls, not five hundred.
        speed.streamed(clock.at(501.0), 1);
        assert_eq!(speed.rate(clock.at(501.0)), shows(15), "10 then 5");
    }

    #[test]
    fn nothing_is_counted_outside_a_call() {
        let clock = Clock::new();
        let mut speed = Speed::default();
        speed.streamed(clock.at(1.0), 500);
        assert_eq!(speed.rate(clock.at(1.0)), None);
        speed.start(clock.at(2.0));
        speed.streamed(clock.at(2.0), 40);
        speed.streamed(clock.at(3.0), 1);
        assert_eq!(speed.rate(clock.at(3.0)), shows(40));
        // Two ends in a row, and a second start, leave the clock alone.
        speed.end(clock.at(3.0));
        speed.end(clock.at(9.0));
        speed.start(clock.at(9.0));
        assert_eq!(speed.rate(clock.at(9.0)), shows(40));
    }
}
