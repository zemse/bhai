//! What bhai has running that the user did not start by typing: bash sessions kept past
//! their call, child agents, schedules still to fire, MCP servers, the egress proxy and
//! a headless browser while a render runs. Each source reads its own table; a snapshot
//! is all of them at once, with known secrets blanked.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

/// How often the session looks for a change in what runs.
pub const POLL: Duration = Duration::from_secs(1);

pub const BG_USAGE: &str = "/bg [list] | kill [bash|child|schedule|monitor] <id>";

/// The last lines of a bash session's output `/bg` shows.
const TAIL: usize = 5;

/// What a row is, in the order the list shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Bash,
    Child,
    Schedule,
    Monitor,
    Mcp,
    Proxy,
    Chrome,
}

impl Kind {
    const ALL: [Kind; 7] = [
        Kind::Bash,
        Kind::Child,
        Kind::Schedule,
        Kind::Monitor,
        Kind::Mcp,
        Kind::Proxy,
        Kind::Chrome,
    ];

    /// The word the list, `/bg` and the server use for it.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Bash => "bash",
            Kind::Child => "child",
            Kind::Schedule => "schedule",
            Kind::Monitor => "monitor",
            Kind::Mcp => "mcp",
            Kind::Proxy => "proxy",
            Kind::Chrome => "chrome",
        }
    }

    fn parse(word: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.name() == word)
    }
}

/// Why `/bg kill` stopped nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Nothing of that kind and id runs.
    Missing(String),
    /// Rows of more than one kind have that id.
    Ambiguous(String),
    /// It runs, but is not something `/bg kill` stops.
    Fixed(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Missing(why) | Refused::Ambiguous(why) | Refused::Fixed(why) => {
                f.write_str(why)
            }
        }
    }
}

/// What `/bg kill` was given: `<id>`, or `<kind> <id>` where ids of two kinds collide.
pub fn target(args: &str) -> Result<(Option<Kind>, String), String> {
    let words: Vec<&str> = args.split_whitespace().collect();
    match words[..] {
        [id] => Ok((None, id.to_string())),
        [kind, id] => match Kind::parse(kind) {
            Some(kind) => Ok((Some(kind), id.to_string())),
            None => Err(BG_USAGE.to_string()),
        },
        _ => Err(BG_USAGE.to_string()),
    }
}

/// The kind of the row `id` names: `kind` when it was given, else the one row of that
/// id among `rows`.
pub fn resolve(rows: &[Row], kind: Option<Kind>, id: &str) -> Result<Kind, Refused> {
    if let Some(kind) = kind {
        return Ok(kind);
    }
    let kinds: Vec<Kind> = rows.iter().filter(|r| r.id == id).map(|r| r.kind).collect();
    match kinds[..] {
        [] => Err(Refused::Missing(format!(
            "nothing in the background has id {id}; /bg lists what does"
        ))),
        [kind] => Ok(kind),
        _ => {
            let names: Vec<&str> = kinds.iter().map(|k| k.name()).collect();
            Err(Refused::Ambiguous(format!(
                "{id} is the id of a {}; say which, as /bg kill <kind> {id}",
                names.join(" and a ")
            )))
        }
    }
}

/// What `/bg` prints: a line for each row, with a bash session's latest output and any
/// other row's detail under it.
pub fn report(rows: &[Row], now: SystemTime) -> String {
    if rows.is_empty() {
        return "background: nothing runs".to_string();
    }
    let stops = if rows.iter().any(|row| row.kind == Kind::Monitor) {
        "a bash session, a child, a schedule or a monitor"
    } else {
        "a bash session, a child or a schedule"
    };
    let mut out = format!("background: {} · /bg kill <id> stops {stops}", rows.len());
    let width = rows.iter().map(|r| r.id.chars().count()).max().unwrap_or(0);
    for row in rows {
        out.push_str(&format!(
            "\n  {:<8} {:<width$}  {}  {}  {}",
            row.kind.name(),
            row.id,
            row.label,
            crate::bgview::age(row.started, now),
            row.state
        ));
        if let Some(pid) = row.pid {
            out.push_str(&format!("  pid {pid}"));
        }
        let lines: Vec<&str> = row.detail.lines().collect();
        let shown = match row.kind {
            Kind::Bash => &lines[lines.len().saturating_sub(TAIL)..],
            _ => &lines[..],
        };
        for line in shown {
            out.push_str(&format!("\n      {line}"));
        }
    }
    out
}

/// One thing running.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    pub kind: Kind,
    /// Unique within its kind: a bash session's id, a child's, a schedule's, a server's
    /// name, a browser's process group.
    pub id: String,
    pub label: String,
    /// `None` where nothing recorded it.
    #[serde(serialize_with = "unix_secs")]
    pub started: Option<SystemTime>,
    pub state: String,
    /// The process group, where it is one bhai started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// More about it; a bash session's latest output.
    pub detail: String,
}

impl Row {
    /// With every known secret blanked from what it says.
    fn redacted(self) -> Self {
        Self {
            label: crate::redact::apply(&self.label).into_owned(),
            detail: crate::redact::apply(&self.detail).into_owned(),
            ..self
        }
    }

    /// What makes two snapshots differ for the count's listeners.
    fn key(&self) -> (Kind, &str, &str) {
        (self.kind, &self.id, &self.state)
    }
}

fn unix_secs<S: serde::Serializer>(at: &Option<SystemTime>, s: S) -> Result<S::Ok, S::Error> {
    match at.and_then(|at| at.duration_since(SystemTime::UNIX_EPOCH).ok()) {
        Some(since) => s.serialize_some(&since.as_secs()),
        None => s.serialize_none(),
    }
}

/// The wall-clock time `at` stands for.
fn wall(at: Instant) -> SystemTime {
    SystemTime::now()
        .checked_sub(at.elapsed())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// A table of things running, read afresh on each snapshot.
pub trait Source: Send + Sync {
    fn rows(&self) -> Vec<Row>;
}

/// The rows of every source, ordered by kind and redacted.
pub fn snapshot(rows: impl IntoIterator<Item = Row>) -> Vec<Row> {
    let mut rows: Vec<Row> = rows.into_iter().map(Row::redacted).collect();
    rows.sort_by_key(|row| row.kind);
    rows
}

/// Whether `now` lists something `before` did not, or drops or changes the state of one.
pub fn changed(before: &[Row], now: &[Row]) -> bool {
    before.len() != now.len() || before.iter().zip(now).any(|(a, b)| a.key() != b.key())
}

/// The process-wide sources: bash sessions, render browsers, the egress proxy, and the
/// session's MCP servers when there are any.
pub fn sources(hub: Option<Arc<crate::mcp::Hub>>) -> Vec<Box<dyn Source>> {
    let mut sources: Vec<Box<dyn Source>> = vec![Box::new(Bash), Box::new(Chrome), Box::new(Proxy)];
    if let Some(hub) = hub {
        sources.push(Box::new(Mcp(hub)));
    }
    sources
}

/// Bash sessions kept running past their call.
pub struct Bash;

impl Source for Bash {
    fn rows(&self) -> Vec<Row> {
        crate::tools::bash::kept().into_iter().map(bash).collect()
    }
}

fn bash(session: crate::tools::bash::Running) -> Row {
    Row {
        kind: Kind::Bash,
        id: session.id.to_string(),
        label: match session.tty {
            true => format!("{}  (tty)", session.command),
            false => session.command,
        },
        started: Some(SystemTime::now() - session.age),
        state: match session.exited {
            true => "exited",
            false => "running",
        }
        .to_string(),
        pid: session.group,
        detail: session.tail,
    }
}

/// Headless browsers a rendered fetch has open.
pub struct Chrome;

impl Source for Chrome {
    fn rows(&self) -> Vec<Row> {
        crate::tools::browser::running()
            .into_iter()
            .map(|browser| Row {
                kind: Kind::Chrome,
                id: browser.group.to_string(),
                label: browser.url,
                started: Some(wall(browser.started)),
                state: "rendering".to_string(),
                pid: Some(browser.group),
                detail: String::new(),
            })
            .collect()
    }
}

/// The egress proxy, once it runs.
pub struct Proxy;

impl Source for Proxy {
    fn rows(&self) -> Vec<Row> {
        crate::egress::active()
            .map(|proxy| Row {
                kind: Kind::Proxy,
                id: "egress".to_string(),
                label: format!("egress proxy on 127.0.0.1:{}", proxy.port()),
                started: Some(wall(proxy.started())),
                state: "running".to_string(),
                pid: None,
                detail: proxy.describe().trim().to_string(),
            })
            .into_iter()
            .collect()
    }
}

/// The MCP servers connected now.
pub struct Mcp(pub Arc<crate::mcp::Hub>);

impl Source for Mcp {
    fn rows(&self) -> Vec<Row> {
        self.0
            .running()
            .into_iter()
            .map(|server| Row {
                kind: Kind::Mcp,
                id: server.name.clone(),
                label: server.name,
                started: None,
                state: "connected".to_string(),
                pid: None,
                detail: format!("{}, {} tools", server.source, server.tools.len()),
            })
            .collect()
    }
}

/// A running child agent, `started` when its pane opened.
pub fn child(row: &crate::session::ChildRow, started: Instant) -> Row {
    Row {
        kind: Kind::Child,
        id: row.id.clone(),
        label: row.description.clone(),
        started: Some(wall(started)),
        state: "running".to_string(),
        pid: None,
        detail: format!(
            "{}, {} steps, quiet {}s",
            row.identity,
            row.steps,
            row.idle().as_secs()
        ),
    }
}

/// The schedules that will fire; a paused one is not running, and an unreadable one
/// is named by `/schedule` instead.
pub fn schedules(store: &crate::schedules::Schedules) -> Vec<Row> {
    let Ok((rows, _)) = store.list() else {
        return Vec::new();
    };
    let now = store.now();
    rows.iter()
        .filter(|row| !row.paused)
        .map(|row| Row {
            kind: Kind::Schedule,
            id: row.id.clone(),
            label: row.text.lines().next().unwrap_or_default().to_string(),
            started: Some(row.created.into()),
            state: "scheduled".to_string(),
            pid: None,
            detail: crate::schedules::describe(row, now),
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A source whose rows the test sets.
    #[derive(Default)]
    pub(crate) struct Fake(pub Arc<Mutex<Vec<Row>>>);

    impl Source for Fake {
        fn rows(&self) -> Vec<Row> {
            self.0.lock().unwrap().clone()
        }
    }

    pub(crate) fn row(kind: Kind, id: &str, state: &str) -> Row {
        Row {
            kind,
            id: id.to_string(),
            label: format!("{id} label"),
            started: None,
            state: state.to_string(),
            pid: None,
            detail: String::new(),
        }
    }

    #[test]
    fn a_snapshot_is_ordered_by_kind_and_redacted() {
        crate::redact::register("background-test-secret-91c2");
        let mut leaky = row(Kind::Bash, "3", "running");
        leaky.label = "curl -H background-test-secret-91c2".to_string();
        leaky.detail = "token background-test-secret-91c2\n".to_string();
        let rows = snapshot([row(Kind::Mcp, "fs", "connected"), leaky]);
        assert_eq!(rows[0].kind, Kind::Bash);
        assert_eq!(rows[0].label, "curl -H [REDACTED]");
        assert_eq!(rows[0].detail, "token [REDACTED]\n");
        assert_eq!(rows[1].kind, Kind::Mcp);
    }

    #[test]
    fn a_change_is_a_row_come_gone_or_in_another_state() {
        let a = vec![row(Kind::Bash, "1", "running")];
        let mut same = a.clone();
        same[0].detail = "more output".to_string();
        assert!(!changed(&a, &same));
        assert!(changed(&a, &[row(Kind::Bash, "1", "exited")]));
        assert!(changed(&a, &[row(Kind::Bash, "2", "running")]));
        assert!(changed(&a, &[]));
        assert!(changed(&[], &a));
    }

    #[test]
    fn rows_serialize_with_started_as_unix_seconds() {
        let mut at = row(Kind::Chrome, "77", "rendering");
        at.started = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        at.pid = Some(77);
        let json = serde_json::to_value(&at).unwrap();
        assert_eq!(json["kind"], "chrome");
        assert_eq!(json["started"], 1_700_000_000);
        assert_eq!(json["pid"], 77);
        let json = serde_json::to_value(row(Kind::Mcp, "fs", "connected")).unwrap();
        assert!(json["started"].is_null());
        assert!(json.get("pid").is_none());
    }

    #[tokio::test]
    async fn schedules_that_will_fire_are_rows_and_paused_ones_are_not() {
        let dir = crate::tools::temp_dir();
        let store = crate::schedules::Schedules::new(&dir, &dir);
        let kept = store.remind("in 20m check CI\nand the logs").unwrap();
        let paused = store.remind("every 2h poll the queue").unwrap();
        store.pause(&paused.id, true).unwrap();
        let rows = schedules(&store);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            (rows[0].kind, rows[0].id.as_str(), rows[0].label.as_str()),
            (Kind::Schedule, kept.id.as_str(), "check CI")
        );
        assert!(
            rows[0].detail.contains("`in 20m` next "),
            "{}",
            rows[0].detail
        );
        assert_eq!(rows[0].started, Some(kept.created.into()));
    }

    #[test]
    fn bg_lists_each_row_with_its_id_and_a_bash_sessions_last_lines() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(report(&[], now), "background: nothing runs");
        let mut bash = row(Kind::Bash, "3", "running");
        bash.label = "npm run dev".to_string();
        bash.started = Some(now - Duration::from_secs(125));
        bash.pid = Some(4242);
        bash.detail = (1..=7).map(|n| format!("line {n}\n")).collect();
        let mut schedule = row(Kind::Schedule, "s9", "scheduled");
        schedule.detail = "s9 `in 20m` next 17:30".to_string();
        assert_eq!(
            report(&[bash, schedule], now),
            "background: 2 · /bg kill <id> stops a bash session, a child or a schedule\
             \n  bash     3   npm run dev  2m  running  pid 4242\
             \n      line 3\n      line 4\n      line 5\n      line 6\n      line 7\
             \n  schedule s9  s9 label  -  scheduled\
             \n      s9 `in 20m` next 17:30"
        );
    }

    #[test]
    fn a_kill_names_an_id_and_a_kind_only_where_two_kinds_share_it() {
        assert_eq!(target("3"), Ok((None, "3".to_string())));
        assert_eq!(
            target(" child  a1b2 "),
            Ok((Some(Kind::Child), "a1b2".to_string()))
        );
        for bad in ["", "dog 3", "bash 3 4"] {
            assert_eq!(target(bad), Err(BG_USAGE.to_string()), "{bad:?}");
        }
        let rows = [
            row(Kind::Bash, "7", "running"),
            row(Kind::Schedule, "7", "scheduled"),
            row(Kind::Mcp, "fs", "connected"),
        ];
        assert_eq!(resolve(&rows, None, "fs"), Ok(Kind::Mcp));
        assert_eq!(resolve(&rows, Some(Kind::Bash), "7"), Ok(Kind::Bash));
        assert_eq!(
            resolve(&rows, None, "7"),
            Err(Refused::Ambiguous(
                "7 is the id of a bash and a schedule; say which, as /bg kill <kind> 7".to_string()
            ))
        );
        assert!(matches!(
            resolve(&rows, None, "9"),
            Err(Refused::Missing(_))
        ));
    }

    #[test]
    fn a_tty_session_says_so_and_an_exited_one_waits_for_its_poll() {
        let row = bash(crate::tools::bash::Running {
            id: 4,
            command: "python3".to_string(),
            tty: true,
            group: Some(1234),
            age: Duration::from_secs(5),
            exited: true,
            tail: ">>> ".to_string(),
        });
        assert_eq!(
            (row.label.as_str(), row.state.as_str(), row.pid),
            ("python3  (tty)", "exited", Some(1234))
        );
        assert_eq!(row.detail, ">>> ");
    }
}
