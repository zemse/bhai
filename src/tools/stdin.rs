//! `write_stdin`: read more from a command `bash` left running as a session, or type
//! into one that has a terminal. Typing is a new command as far as the user is
//! concerned, so it is ruled on like one; a poll that types nothing is not.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::{Value, json};

use super::{BoxFuture, Live, Tool, bash, string_arg};

pub const NAME: &str = "write_stdin";

/// How long a call waits after typing, and the bounds on it.
const AFTER_WRITE: Duration = Duration::from_millis(500);
const WRITE_MIN: Duration = Duration::from_millis(250);
const WRITE_MAX: Duration = Duration::from_secs(30);
/// How long a call that types nothing waits, and the bounds on it: a poll every quarter
/// second would spend a model call on each.
const POLL: Duration = Duration::from_secs(60);
const POLL_MIN: Duration = Duration::from_secs(5);
const POLL_MAX: Duration = Duration::from_secs(1800);

pub struct WriteStdin;

impl Tool for WriteStdin {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": NAME,
            "description": "Poll a command `bash` left running as a session, or type into it. \
        Returns the output printed since the session was last read, then either its exit code \
        or `Process running with session ID N` again. With `chars` empty it reads output, \
        waiting until exit or the deadline, not returning for each output chunk. Typing \
        needs a session started with `tty: true`; one without a terminal only \
        takes \"\\u0003\" (ctrl-c). The user approves what is typed.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "integer",
                        "description": "The session ID a `bash` result gave."
                    },
                    "chars": {
                        "type": "string",
                        "description": "What to type, with \"\\n\" for enter and \"\\u0003\" \
        for ctrl-c. Empty to only read output."
                    },
                    "yield_time_ms": {
                        "type": "integer",
                        "description": "How long to wait before returning, unless \
        the command exits first: 250 to 30000 after typing (default 500), 5000 to 1800000 for \
        a poll (default 60000). A poll returns early when the command exits and streams \
        output while waiting; it does not keep polling after exit. For builds and installs, \
        wait for the expected remaining duration (often 300000 to 600000). Increase the wait \
        when successive polls show little progress; use result timestamps to track elapsed time."
                    }
                },
                "required": ["session_id"],
                "additionalProperties": false
            }
        })
    }

    /// The policy asks about a call that types something; see `permissions`.
    fn needs_approval(&self) -> bool {
        false
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        let (id, chars, _) = parse(args)?;
        let (command, _) = bash::check_input(id, chars)?;
        // A shell runs the line earlier writes left unfinished along with `chars`, so the
        // summary, which is what the user, the judge and its cache see, names it whole.
        let entered = bash::input_line(id, chars)
            .map(|(_, line)| line)
            .filter(|line| line.len() > chars.len());
        Ok(match (chars.is_empty(), entered) {
            (true, _) => format!("poll session {id}: {command}"),
            (false, None) => format!("type {chars:?} into session {id}: {command}"),
            (false, Some(line)) => {
                format!("type {chars:?} into session {id}, entering {line:?}: {command}")
            }
        })
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        static NEVER: AtomicBool = AtomicBool::new(false);
        let live = Live {
            progress: &|_| {},
            cancel: &NEVER,
            conversation: None,
        };
        self.execute_live(args, live)
    }

    fn execute_live<'a>(
        &'a self,
        args: &'a Value,
        live: Live<'a>,
    ) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let checked = parse(args).and_then(|(id, chars, wait)| {
                bash::check_input(id, chars)?;
                Ok((id, chars, wait))
            });
            match checked {
                Ok((id, chars, wait)) => (bash::write(id, chars, wait, live).await, true),
                Err(e) => (e, false),
            }
        })
    }
}

/// The session, what to type, and how long to wait.
fn parse(args: &Value) -> Result<(u32, &str, Duration), String> {
    let id = args
        .get("session_id")
        .and_then(Value::as_u64)
        .and_then(|id| u32::try_from(id).ok())
        .ok_or_else(|| "missing required integer field `session_id`.".to_string())?;
    let chars = match args.get("chars") {
        None | Some(Value::Null) => "",
        Some(Value::String(_)) => string_arg(args, "chars").unwrap_or_default(),
        Some(_) => return Err("`chars` must be a string.".to_string()),
    };
    let wait = match chars.is_empty() {
        true => bash::parse_millis(args, POLL, POLL_MIN, POLL_MAX)?,
        false => bash::parse_millis(args, AFTER_WRITE, WRITE_MIN, WRITE_MAX)?,
    };
    Ok((id, chars, wait))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_names_a_session_and_clamps_its_wait() {
        assert!(parse(&json!({})).is_err());
        assert!(parse(&json!({"session_id": "1"})).is_err());
        assert!(parse(&json!({"session_id": 1, "chars": 1})).is_err());
        let poll = json!({"session_id": 3});
        assert_eq!(parse(&poll).unwrap(), (3, "", POLL));
        let (_, _, wait) = parse(&json!({"session_id": 3, "yield_time_ms": 10})).unwrap();
        assert_eq!(wait, POLL_MIN);
        let args = json!({"session_id": 3, "yield_time_ms": 600_000});
        assert_eq!(parse(&args).unwrap().2, Duration::from_secs(600));
        let args = json!({"session_id": 3, "yield_time_ms": 9_999_999});
        assert_eq!(parse(&args).unwrap().2, POLL_MAX);
        let typed = json!({"session_id": 3, "chars": "x\n"});
        assert_eq!(parse(&typed).unwrap(), (3, "x\n", AFTER_WRITE));
        let args = json!({"session_id": 3, "chars": "x", "yield_time_ms": 999_999});
        assert_eq!(parse(&args).unwrap().2, WRITE_MAX);
    }

    #[test]
    fn a_session_that_does_not_exist_is_refused_before_anyone_is_asked() {
        let err = WriteStdin
            .describe(&json!({"session_id": 4_000_000_000u32}))
            .unwrap_err();
        assert!(err.contains("no running session"), "{err}");
    }
}
