//! The date, timezone, directory and shell the model works in. They change under a
//! session (a long one runs past midnight), so they ride in the history rather than the
//! instructions: the first turn carries all of them, a later one only what changed, and
//! the cached prefix never moves.

use serde_json::{Value, json};

/// The tag the item's text opens with, and what marks the item as this one.
pub const OPEN: &str = "<environment_context>";
const CLOSE: &str = "</environment_context>";

/// The values the model is told, in the order it is told them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Environment {
    pub cwd: String,
    /// The shell the `bash` tool runs, not the user's login shell.
    pub shell: String,
    pub current_date: String,
    pub timezone: String,
}

impl Environment {
    /// The machine as it is now.
    pub fn current() -> Self {
        let now = chrono::Local::now();
        Environment {
            cwd: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "unknown".to_string()),
            shell: "bash".to_string(),
            current_date: now.format("%Y-%m-%d").to_string(),
            timezone: timezone().unwrap_or_else(|| now.format("%:z").to_string()),
        }
    }

    fn fields(&self) -> [(&'static str, &str); 4] {
        [
            ("cwd", &self.cwd),
            ("shell", &self.shell),
            ("current_date", &self.current_date),
            ("timezone", &self.timezone),
        ]
    }

    fn set(&mut self, key: &str, value: &str) {
        let field = match key {
            "cwd" => &mut self.cwd,
            "shell" => &mut self.shell,
            "current_date" => &mut self.current_date,
            "timezone" => &mut self.timezone,
            _ => return,
        };
        *field = value.to_string();
    }
}

/// The IANA name of the local zone: `TZ` when set, else where `/etc/localtime` points.
fn timezone() -> Option<String> {
    let named = std::env::var("TZ")
        .ok()
        .map(|tz| tz.trim_start_matches(':').to_string())
        .filter(|tz| !tz.is_empty() && !tz.starts_with('/'));
    named.or_else(|| {
        let target = std::fs::read_link("/etc/localtime").ok()?;
        let target = target.to_str()?;
        let (_, name) = target.split_once("zoneinfo/")?;
        Some(name.to_string())
    })
}

/// The item to put before the next turn: every value when the history carries none, the
/// ones that differ when it does, nothing when it is current. Read from the history each
/// time, so a compaction or a `/clear` that dropped the last one sends it whole again.
pub fn update(history: &[Value], now: &Environment) -> Option<Value> {
    let told = told(history);
    let changed: Vec<(&str, &str)> = match &told {
        None => now.fields().to_vec(),
        Some(told) => now
            .fields()
            .into_iter()
            .zip(told.fields())
            .filter(|(now, told)| now.1 != told.1)
            .map(|(now, _)| now)
            .collect(),
    };
    if changed.is_empty() {
        return None;
    }
    let mut text = format!("{OPEN}\n");
    for (key, value) in changed {
        text.push_str(&format!("  <{key}>{value}</{key}>\n"));
    }
    text.push_str(CLOSE);
    Some(item(&text))
}

/// Everything the history has told the model, as one item, for a compaction to keep
/// in place of the ones it folds away.
pub fn restated(history: &[Value]) -> Option<Value> {
    let told = told(history)?;
    update(&[], &told)
}

/// A developer message, so nothing that reads the history for what the user said takes
/// it for that.
fn item(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "developer",
        "content": [{ "type": "input_text", "text": text }],
    })
}

/// Whether `item` is one of these.
pub fn is_context(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("developer") && text(item).starts_with(OPEN)
}

fn text(item: &Value) -> &str {
    item.pointer("/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// What the history has told the model so far, each later item over the ones before.
fn told(history: &[Value]) -> Option<Environment> {
    let mut told = None;
    for item in history.iter().filter(|item| is_context(item)) {
        let env = told.get_or_insert_with(Environment::default);
        for line in text(item).lines() {
            let Some((key, rest)) = line
                .trim()
                .strip_prefix('<')
                .and_then(|l| l.split_once('>'))
            else {
                continue;
            };
            if let Some(value) = rest.strip_suffix(&format!("</{key}>")) {
                env.set(key, value);
            }
        }
    }
    told
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(date: &str) -> Environment {
        Environment {
            cwd: "/work/bhai".to_string(),
            shell: "bash".to_string(),
            current_date: date.to_string(),
            timezone: "Asia/Kolkata".to_string(),
        }
    }

    fn user(text: &str) -> Value {
        crate::compact::user_message(text)
    }

    #[test]
    fn the_first_turn_carries_every_value() {
        let item = update(&[user("hi")], &env("2026-10-01")).unwrap();
        assert_eq!(item["role"], "developer");
        assert_eq!(
            text(&item),
            "<environment_context>\n  <cwd>/work/bhai</cwd>\n  <shell>bash</shell>\n  \
<current_date>2026-10-01</current_date>\n  <timezone>Asia/Kolkata</timezone>\n\
</environment_context>"
        );
        assert!(is_context(&item));
    }

    #[test]
    fn nothing_is_sent_while_it_is_current() {
        let history = vec![update(&[], &env("2026-10-01")).unwrap(), user("hi")];
        assert_eq!(update(&history, &env("2026-10-01")), None);
    }

    #[test]
    fn a_later_turn_carries_only_what_changed() {
        let mut history = vec![update(&[], &env("2026-10-01")).unwrap(), user("hi")];
        let next = update(&history, &env("2026-10-02")).unwrap();
        assert_eq!(
            text(&next),
            "<environment_context>\n  <current_date>2026-10-02</current_date>\n\
</environment_context>"
        );
        history.push(next);
        // The diff is read back over the full item, so the date is current again.
        assert_eq!(update(&history, &env("2026-10-02")), None);
        let mut moved = env("2026-10-02");
        moved.cwd = "/work/other".to_string();
        assert_eq!(
            text(&update(&history, &moved).unwrap()),
            "<environment_context>\n  <cwd>/work/other</cwd>\n</environment_context>"
        );
    }

    #[test]
    fn a_user_message_that_quotes_the_tag_is_not_one() {
        let history = vec![user(
            "<environment_context>\n  <current_date>2020-01-01</current_date>",
        )];
        assert!(!is_context(&history[0]));
        assert!(update(&history, &env("2026-10-01")).is_some());
    }

    #[test]
    fn the_current_one_has_a_date_and_a_zone() {
        let now = Environment::current();
        assert_eq!(now.current_date.len(), 10);
        assert!(!now.timezone.is_empty());
        assert_eq!(now.shell, "bash");
    }
}
