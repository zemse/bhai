//! Ending a session on a signal. Until a session takes them, the handler in `tools::bash`
//! kills what bhai started and bhai dies of the signal. Once [`take`] has registered, the
//! first of the signals it named goes to the session instead, to interrupt its turn, let
//! the history record that, and say how to resume; a second one still kills.

use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::mpsc;

/// How long an interrupted turn gets to end before bhai exits anyway.
pub const SETTLE: Duration = Duration::from_secs(10);

struct Taker {
    signals: Vec<i32>,
    tx: mpsc::UnboundedSender<i32>,
}

static TAKER: Mutex<Option<Taker>> = Mutex::new(None);

/// Have the first of `signals` sent here rather than killing bhai.
pub fn take(signals: &[i32]) -> mpsc::UnboundedReceiver<i32> {
    let (tx, rx) = mpsc::unbounded_channel();
    *TAKER.lock().unwrap_or_else(|e| e.into_inner()) = Some(Taker {
        signals: signals.to_vec(),
        tx,
    });
    rx
}

/// [`take`] a hangup and a terminate, and an interrupt too when no terminal turns ctrl+c
/// into a key. Elsewhere than unix nothing is taken and the receiver never yields.
pub fn take_ending(interrupt: bool) -> mpsc::UnboundedReceiver<i32> {
    #[cfg(unix)]
    let signals = match interrupt {
        true => vec![libc::SIGHUP, libc::SIGINT, libc::SIGTERM],
        false => vec![libc::SIGHUP, libc::SIGTERM],
    };
    #[cfg(not(unix))]
    let signals = {
        let _ = interrupt;
        Vec::new()
    };
    take(&signals)
}

/// Hand `signal` to the session that took it. False when none did, when it already had
/// one, or when it has stopped listening, and then the signal kills.
pub fn hand_over(signal: i32) -> bool {
    let mut taker = TAKER.lock().unwrap_or_else(|e| e.into_inner());
    if !taker.as_ref().is_some_and(|t| t.signals.contains(&signal)) {
        return false;
    }
    taker.take().is_some_and(|t| t.tx.send(signal).is_ok())
}

/// The signal's name, for the line that says why bhai stopped.
pub fn name(signal: i32) -> String {
    #[cfg(unix)]
    match signal {
        libc::SIGHUP => return "SIGHUP".to_string(),
        libc::SIGINT => return "SIGINT".to_string(),
        libc::SIGTERM => return "SIGTERM".to_string(),
        _ => {}
    }
    format!("signal {signal}")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn only_the_first_signal_taken_is_handed_over() {
        assert!(!hand_over(libc::SIGTERM), "nothing took it yet");
        let mut rx = take(&[libc::SIGHUP, libc::SIGTERM]);
        assert!(!hand_over(libc::SIGINT), "not one it took");
        assert!(hand_over(libc::SIGTERM));
        assert_eq!(rx.try_recv(), Ok(libc::SIGTERM));
        assert!(!hand_over(libc::SIGHUP), "a second one kills");

        let rx = take(&[libc::SIGTERM]);
        drop(rx);
        assert!(!hand_over(libc::SIGTERM), "no one is listening");
        assert_eq!(name(libc::SIGHUP), "SIGHUP");
    }
}
