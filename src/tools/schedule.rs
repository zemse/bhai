//! `schedule`, for the main agent: leave itself a prompt that comes back into this
//! session later ("check CI in 20m"), list the schedules, or cancel one it set. A schedule
//! starts a turn with nobody watching, so every one it sets is the user's to approve and
//! never the judge's, and the user's own schedules are not its to cancel.

use std::sync::Arc;

use serde_json::{Value, json};

use super::{BoxFuture, Tool, string_arg};
use crate::schedule::Spec;
use crate::schedules::{self, New, Origin, Schedules};

pub const NAME: &str = "schedule";

pub struct Schedule {
    pub schedules: Arc<Schedules>,
}

/// One call, as its arguments ask for it.
enum Action {
    Create { spec: Spec, text: String },
    List,
    Cancel(String),
}

impl Action {
    fn of(args: &Value) -> Result<Self, String> {
        match string_arg(args, "action") {
            Some("create") => {
                let when = string_arg(args, "when")
                    .ok_or_else(|| "`create` needs `when`, such as `in 20m`.".to_string())?;
                let spec = when.trim().parse()?;
                let text = string_arg(args, "prompt").unwrap_or_default().trim();
                if text.is_empty() {
                    return Err("`create` needs a `prompt` to come back with.".to_string());
                }
                Ok(Action::Create {
                    spec,
                    text: crate::redact::apply(text).into_owned(),
                })
            }
            Some("list") => Ok(Action::List),
            Some("cancel") => match string_arg(args, "id").map(str::trim) {
                Some(id) if !id.is_empty() => Ok(Action::Cancel(id.to_string())),
                _ => Err("`cancel` needs the `id` of a schedule you set.".to_string()),
            },
            _ => Err("`action` must be `create`, `list` or `cancel`.".to_string()),
        }
    }
}

/// Whether a call with `args` changes nothing the user has to approve: everything but
/// `create`. The permission rules read it.
pub fn reads_only(args: &Value) -> bool {
    string_arg(args, "action") != Some("create")
}

fn new(spec: Spec, text: String) -> New {
    New {
        spec,
        text,
        origin: Origin::Model,
        times: None,
        lasts: None,
    }
}

impl Tool for Schedule {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "name": NAME,
            "description": format!("Come back to this session later. `create` sets `prompt` \
        to arrive as a new message at `when`, as a note from you rather than from the user: \
        `in 20m`, `at 17:30`, `at 2026-10-04 09:00`, `every 2h` or `cron <5 fields>` in local \
        time. Use it to check on something slow, such as CI or a deploy, instead of waiting. \
        The user approves every one. A recurring one fires at least {} minutes apart and \
        ends after {} days; you may have {} set at once, and a prompt is at most {} bytes. \
        `list` shows every schedule with its id; `cancel` removes one you set.",
                schedules::MODEL_GAP_MIN.num_minutes(),
                schedules::EXPIRY.num_days(),
                schedules::MODEL_ROWS_MAX,
                schedules::MODEL_TEXT_MAX
            ),
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["create", "list", "cancel"] },
                    "when": {
                        "type": "string",
                        "description": "For `create`: when it fires, such as `in 20m`."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "For `create`: what to tell yourself when it fires, \
        with what you will need to pick the work up again."
                    },
                    "id": { "type": "string", "description": "For `cancel`: the schedule's id." }
                },
                "required": ["action"],
                "additionalProperties": false
            }
        })
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn describe(&self, args: &Value) -> Result<String, String> {
        match Action::of(args)? {
            Action::Create { spec, text } => {
                let summary = format!("schedule `{spec}`: {}", one_line(&text));
                self.schedules.vet(new(spec, text))?;
                Ok(summary)
            }
            Action::List => Ok("schedule list".to_string()),
            Action::Cancel(id) => Ok(format!("schedule cancel {id}")),
        }
    }

    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, (String, bool)> {
        Box::pin(async move {
            let action = match Action::of(args) {
                Ok(action) => action,
                Err(e) => return (e, false),
            };
            let store = &self.schedules;
            match action {
                Action::Create { spec, text } => match store.add(new(spec, text)) {
                    Ok(row) => (
                        format!(
                            "Set {}. It comes back to this session as your own note; \
cancel it with that id.",
                            schedules::describe(&row, store.now())
                        ),
                        true,
                    ),
                    Err(e) => (format!("Not set: {e}"), false),
                },
                Action::List => match store.list() {
                    Ok((rows, bad)) => (listed(&rows, bad.len(), store.now()), true),
                    Err(e) => (e, false),
                },
                Action::Cancel(id) => match store.remove(&id, Origin::Model) {
                    Ok(row) => (
                        format!("Cancelled {}.", schedules::describe(&row, store.now())),
                        true,
                    ),
                    Err(e) => (format!("Not cancelled: {e}"), false),
                },
            }
        })
    }
}

/// What `list` answers: each schedule on a line, the model's marked as set by it.
fn listed(
    rows: &[schedules::Row],
    unreadable: usize,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let mut out = match rows.len() {
        0 => "No schedules are set.".to_string(),
        n => format!("{n} schedule(s):"),
    };
    for row in rows {
        out.push_str(&format!("\n- {}", schedules::describe(row, now)));
    }
    if unreadable > 0 {
        out.push_str(&format!(
            "\n{unreadable} row(s) of the store could not be read; the user can see them with /schedule."
        ));
    }
    out
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(dir: &std::path::Path) -> Schedule {
        Schedule {
            schedules: Arc::new(Schedules::new(dir, dir, "test-session")),
        }
    }

    #[tokio::test]
    async fn the_model_sets_lists_and_cancels_its_own_schedules() {
        let dir = super::super::temp_dir();
        let tool = tool(&dir);
        assert!(tool.needs_approval());
        let args = json!({"action": "create", "when": "in 20m", "prompt": "check\n CI"});
        assert_eq!(tool.describe(&args).unwrap(), "schedule `in 20m`: check CI");
        let (out, ok) = tool.execute(&args).await;
        assert!(ok, "{out}");
        assert!(
            out.contains("`in 20m` next ") && out.contains("set by the model"),
            "{out}"
        );
        let (rows, _) = tool.schedules.list().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].origin, rows[0].text.as_str()),
            (Origin::Model, "check\n CI")
        );

        let user = tool.schedules.remind("in 1h the user's own").unwrap();
        let (out, ok) = tool.execute(&json!({"action": "list"})).await;
        assert!(ok && out.starts_with("2 schedule(s):"), "{out}");
        assert!(
            out.contains(&user.id) && out.contains("the user's own"),
            "{out}"
        );

        // The user's own is not the model's to cancel.
        let cancel = |id: &str| json!({"action": "cancel", "id": id});
        assert_eq!(
            tool.describe(&cancel(&user.id)).unwrap(),
            format!("schedule cancel {}", user.id)
        );
        let (out, ok) = tool.execute(&cancel(&user.id)).await;
        assert!(!ok && out.contains("the user's schedule"), "{out}");
        let (out, ok) = tool.execute(&cancel(&rows[0].id)).await;
        assert!(ok && out.starts_with("Cancelled "), "{out}");
        assert_eq!(tool.schedules.list().unwrap().0, [user]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_schedule_the_model_sets_is_bounded_before_it_is_asked_about() {
        let dir = super::super::temp_dir();
        let tool = tool(&dir);
        let create =
            |when: &str, prompt: &str| json!({"action": "create", "when": when, "prompt": prompt});
        for (args, why) in [
            (json!({"action": "create", "prompt": "x"}), "needs `when`"),
            (create("in 20m", "  "), "needs a `prompt`"),
            (create("soon", "x"), "does not start a schedule"),
            (create("every 4m", "poll"), "under the 5 minutes"),
            (create("cron */2 * * * *", "poll"), "under the 5 minutes"),
            (
                create("in 20m", &"x".repeat(schedules::MODEL_TEXT_MAX + 1)),
                "over the 1024",
            ),
            (json!({"action": "cancel"}), "needs the `id`"),
            (json!({"action": "pause"}), "must be"),
        ] {
            let err = tool.describe(&args).unwrap_err();
            assert!(err.contains(why), "{args}: {err}");
        }
        assert!(tool.describe(&create("every 5m", "poll")).is_ok());
        assert!(
            tool.describe(&create("cron 0,30 9-17 * * mon-fri", "poll"))
                .is_ok()
        );

        for i in 0..schedules::MODEL_ROWS_MAX {
            let (out, ok) = tool.execute(&create("in 1h", &format!("note {i}"))).await;
            assert!(ok, "{out}");
        }
        let full = create("in 2h", "one more");
        let err = tool.describe(&full).unwrap_err();
        assert!(err.contains("already have 10"), "{err}");
        let (out, ok) = tool.execute(&full).await;
        assert!(!ok && out.contains("already have 10"), "{out}");
        // The user's own do not count against the model.
        assert!(tool.schedules.remind("in 1h mine").is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_secret_never_reaches_the_store() {
        crate::redact::register("schedule-test-secret-5e1f");
        let dir = super::super::temp_dir();
        let tool = tool(&dir);
        let args = json!({"action": "create", "when": "in 20m",
            "prompt": "retry with schedule-test-secret-5e1f"});
        assert_eq!(
            tool.describe(&args).unwrap(),
            "schedule `in 20m`: retry with [REDACTED]"
        );
        let (out, ok) = tool.execute(&args).await;
        assert!(ok, "{out}");
        let stored = std::fs::read_to_string(tool.schedules.path()).unwrap();
        assert!(!stored.contains("schedule-test-secret-5e1f"), "{stored}");
        assert!(stored.contains("retry with [REDACTED]"), "{stored}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_create_asks() {
        assert!(!reads_only(&json!({"action": "create"})));
        assert!(reads_only(&json!({"action": "list"})));
        assert!(reads_only(&json!({"action": "cancel", "id": "x"})));
    }
}
