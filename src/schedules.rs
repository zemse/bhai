//! The schedules of a project, kept in `.bhai/schedules.json`, and the runner that fires
//! each into the session as it falls due.
//!
//! A fire is claimed under the store's lock before it is submitted: the row is moved on
//! or removed and the file saved first, so two bhai processes in one project never both
//! fire the same slot, and a crash between the two loses a fire rather than repeating it.
//! A slot that passed while no bhai ran is missed: a one-shot row fires late and says so,
//! a recurring one skips to its next slot and says so, rather than firing a backlog.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use chrono::{DateTime, Duration, Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Notify;

use crate::schedule::Spec;
use crate::session::{Event, Prompt, Session};
use crate::worktrees::Lock;

/// The store, under the project's `.bhai`.
pub const FILE: &str = "schedules.json";
const LOCK: &str = "schedules.lock";
/// How long a recurring row lives when nothing else is asked for.
pub const EXPIRY: Duration = Duration::days(7);
/// The longest prompt a row carries, in bytes.
pub const TEXT_MAX: usize = 8192;
/// The longest the runner sleeps before reading the store again: another process may
/// have added a row, and tokio's clock stops while the machine sleeps.
const NAP: std::time::Duration = std::time::Duration::from_secs(60);

/// Where the time comes from; tests put paused tokio time behind it.
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Who set a schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    User,
    Model,
}

/// One schedule as the store keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub id: String,
    pub spec: Spec,
    /// The prompt it fires.
    pub text: String,
    pub origin: Origin,
    pub created: DateTime<Utc>,
    /// When it fires next.
    pub next: DateTime<Utc>,
    /// Fires left before it ends; `None` is until it expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fires_left: Option<u32>,
    /// When a recurring row ends; every recurring row has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<DateTime<Utc>>,
    #[serde(default)]
    pub paused: bool,
}

/// A schedule to add.
#[derive(Debug, Clone)]
pub struct New {
    pub spec: Spec,
    pub text: String,
    pub origin: Origin,
    /// How many times a recurring one fires; `None` is until it expires.
    pub times: Option<u32>,
    /// How long a recurring one lives; `None` is [`EXPIRY`].
    pub lasts: Option<Duration>,
}

/// A row that fell due, claimed for the session.
#[derive(Debug, Clone, PartialEq)]
pub struct Fire {
    pub row: Row,
    /// The slot it fired for.
    pub due: DateTime<Utc>,
    /// When it fired.
    pub at: DateTime<Utc>,
    /// The slot passed while no bhai was running.
    pub missed: bool,
}

impl Fire {
    /// The prompt as the session gets it: framed as scheduled, with who set it and when,
    /// so the model does not take it for something the user just typed.
    pub fn prompt(&self) -> Prompt {
        let row = &self.row;
        let who = match row.origin {
            Origin::User => "the user",
            Origin::Model => "you",
        };
        let late = match self.missed {
            true => format!(
                "; it was due at {} while bhai was not running, so it runs late",
                local(self.due)
            ),
            false => String::new(),
        };
        let text = format!(
            "[scheduled: {who} set this at {} with `{}`, and it fired at {}{late}]\n\n{}",
            local(row.created),
            row.spec,
            local(self.at),
            row.text
        );
        let shown = match self.missed {
            true => format!(
                "(missed `{}`, due {}) {}",
                row.spec,
                local(self.due),
                row.text
            ),
            false => format!("(scheduled `{}`) {}", row.spec, row.text),
        };
        Prompt::shown_as(text, shown)
    }
}

/// What one pass over the store found.
#[derive(Debug, Default)]
struct Pass {
    fires: Vec<Fire>,
    notes: Vec<String>,
    /// The earliest fire still to come.
    wake: Option<DateTime<Utc>>,
}

/// A project's schedules.
pub struct Schedules {
    bhai: PathBuf,
    clock: Clock,
    /// Wakes the runner when a row changes here, so it never sleeps past a new one.
    poke: Notify,
}

impl Schedules {
    /// For bhai running in `project`.
    pub fn new(project: &Path) -> Self {
        Self {
            bhai: project.join(".bhai"),
            clock: Arc::new(Utc::now),
            poke: Notify::new(),
        }
    }

    pub fn with_clock(self, clock: Clock) -> Self {
        Self { clock, ..self }
    }

    pub fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    fn path(&self) -> PathBuf {
        self.bhai.join(FILE)
    }

    /// Held across every read-modify-write, by this process and any other in the project.
    fn lock(&self) -> Result<Lock, String> {
        crate::sessions::private_dir(&self.bhai).map_err(|e| e.to_string())?;
        let file = crate::sessions::private_append(&self.bhai.join(LOCK))
            .map_err(|e| format!("could not open the schedules lock: {e}"))?;
        Lock::take(file, "the schedules")
    }

    /// The rows as written, each checked by [`checked`] before use, since a clone can
    /// commit `.bhai` and a schedule is a way into the session.
    fn load(&self) -> Result<Vec<Value>, String> {
        let unparsed =
            |e: serde_json::Error| format!("{} does not parse: {e}", self.path().display());
        match std::fs::read_to_string(self.path()) {
            Ok(text) if text.trim().is_empty() => Ok(Vec::new()),
            Ok(text) => serde_json::from_str(&text).map_err(unparsed),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("could not read {}: {e}", self.path().display())),
        }
    }

    /// Written aside and renamed over, so a crash mid-write leaves the old rows whole.
    fn save(&self, rows: &[Value]) -> Result<(), String> {
        let text = serde_json::to_string_pretty(rows).map_err(|e| e.to_string())?;
        let aside = self
            .bhai
            .join(format!(".{FILE}.{}.tmp", std::process::id()));
        crate::sessions::private_write(&aside, &text)
            .and_then(|()| std::fs::rename(&aside, self.path()))
            .map_err(|e| format!("could not write {}: {e}", self.path().display()))
    }

    /// Change the rows under the lock, and save them when `change` succeeds.
    fn edit<T>(
        &self,
        change: impl FnOnce(&mut Vec<Value>) -> Result<T, String>,
    ) -> Result<T, String> {
        let _lock = self.lock()?;
        let mut rows = self.load()?;
        let done = change(&mut rows)?;
        self.save(&rows)?;
        self.poke.notify_one();
        Ok(done)
    }

    /// Add a schedule; its first fire counts from now.
    pub fn add(&self, new: New) -> Result<Row, String> {
        let now = self.now();
        let next = new
            .spec
            .next_after(&now.with_timezone(&Local))
            .map(|t| t.with_timezone(&Utc))
            .ok_or_else(|| format!("`{}` has no time left to fire at", new.spec))?;
        let recurs = new.spec.recurs();
        let expires = match (recurs, new.lasts) {
            (false, _) => None,
            (true, Some(lasts)) if lasts <= Duration::zero() => {
                return Err("a schedule has to last some time".to_string());
            }
            (true, lasts) => now.checked_add_signed(lasts.unwrap_or(EXPIRY)),
        };
        if expires.is_some_and(|end| next > end) {
            return Err(format!("`{}` would expire before it first fires", new.spec));
        }
        let fires_left = match (recurs, new.times) {
            (_, Some(0)) => return Err("a schedule has to fire at least once".to_string()),
            (true, times) => times,
            (false, _) => None,
        };
        self.edit(|rows| {
            let taken = |id: &str| rows.iter().any(|row| row["id"] == id);
            let id = std::iter::repeat_with(|| {
                uuid::Uuid::new_v4().simple().to_string()[..6].to_string()
            })
            .find(|id| !taken(id))
            .unwrap_or_default();
            let row = Row {
                id,
                spec: new.spec,
                text: new.text,
                origin: new.origin,
                created: now,
                next,
                fires_left,
                expires,
                paused: false,
            };
            let value = serde_json::to_value(&row).map_err(|e| e.to_string())?;
            checked(&value)?;
            rows.push(value);
            Ok(row)
        })
    }

    /// Every row that is a schedule, and a line for each row that is not.
    pub fn list(&self) -> Result<(Vec<Row>, Vec<String>), String> {
        let mut found = (Vec::new(), Vec::new());
        for (at, row) in self.load()?.iter().enumerate() {
            match checked(row) {
                Ok(row) => found.0.push(row),
                Err(why) => found.1.push(skipped(at, &why)),
            }
        }
        Ok(found)
    }

    /// Remove schedule `id`.
    pub fn cancel(&self, id: &str) -> Result<Row, String> {
        self.edit(|rows| {
            let at = find(rows, id)?;
            let row = checked(&rows[at])?;
            rows.remove(at);
            Ok(row)
        })
    }

    /// Hold schedule `id` back from firing, or let it fire again. A recurring row that
    /// comes back after its slot moves to the next one; a one-shot row fires at once.
    pub fn pause(&self, id: &str, paused: bool) -> Result<Row, String> {
        let now = self.now();
        self.edit(|rows| {
            let at = find(rows, id)?;
            let mut row = checked(&rows[at])?;
            row.paused = paused;
            if !paused && row.spec.recurs() && row.next <= now {
                row.next = following(&row, now)
                    .ok_or_else(|| format!("{id} has no time left to fire at"))?;
            }
            rows[at] = serde_json::to_value(&row).map_err(|e| e.to_string())?;
            Ok(row)
        })
    }

    /// Claim every row that is due, and move on or remove each. At `startup` a due row
    /// is one that was missed, and a row that is not a schedule is named.
    fn pass(&self, startup: bool) -> Result<Pass, String> {
        let _lock = self.lock()?;
        let rows = self.load()?;
        let now = self.now();
        let mut pass = Pass::default();
        let mut kept = Vec::with_capacity(rows.len());
        let mut changed = false;
        for (at, value) in rows.into_iter().enumerate() {
            let mut row = match checked(&value) {
                Ok(row) => row,
                Err(why) => {
                    if startup {
                        pass.notes.push(skipped(at, &why));
                    }
                    kept.push(value);
                    continue;
                }
            };
            if row.paused {
                kept.push(value);
                continue;
            }
            let name = format!("{} (`{}`)", row.id, row.spec);
            if let Some(end) = row.expires.filter(|end| *end <= now) {
                pass.notes.push(format!("{name} expired at {}", local(end)));
                changed = true;
                continue;
            }
            if row.next > now {
                pass.wake = Some(pass.wake.map_or(row.next, |w| w.min(row.next)));
                kept.push(value);
                continue;
            }
            changed = true;
            let due = row.next;
            let recurs = row.spec.recurs();
            if recurs && startup {
                let Some(next) =
                    following(&row, now).filter(|t| row.expires.is_none_or(|end| *t <= end))
                else {
                    pass.notes.push(format!(
                        "{name} was due at {} while bhai was not running, and expires before its next slot",
                        local(due)
                    ));
                    continue;
                };
                pass.notes.push(format!(
                    "{name} was due at {} while bhai was not running; it fires next at {}",
                    local(due),
                    local(next)
                ));
                row.next = next;
            } else {
                pass.fires.push(Fire {
                    row: row.clone(),
                    due,
                    at: now,
                    missed: startup,
                });
                if !recurs {
                    continue;
                }
                row.fires_left = row.fires_left.map(|n| n.saturating_sub(1));
                if row.fires_left == Some(0) {
                    pass.notes.push(format!("{name} has fired its last time"));
                    continue;
                }
                match following(&row, now).filter(|t| row.expires.is_none_or(|end| *t <= end)) {
                    Some(next) => row.next = next,
                    None => {
                        pass.notes
                            .push(format!("{name} has fired its last time before it expires"));
                        continue;
                    }
                }
            }
            pass.wake = Some(pass.wake.map_or(row.next, |w| w.min(row.next)));
            kept.push(serde_json::to_value(&row).map_err(|e| e.to_string())?);
        }
        if changed {
            self.save(&kept)?;
        }
        Ok(pass)
    }
}

/// Fire `schedules` into `session` as each falls due, until the session is gone or takes
/// no more messages. The first pass catches up on what was missed while no bhai ran.
pub async fn run(session: Weak<Session>, schedules: Arc<Schedules>) {
    let mut startup = true;
    loop {
        let pass = schedules.pass(startup);
        let Some(hub) = session.upgrade() else {
            return;
        };
        let wake = match pass {
            Ok(pass) => {
                for note in pass.notes {
                    hub.publish(Event::Info(format!("schedules: {note}")));
                }
                for fire in pass.fires {
                    // Queued behind a running turn, or started; only a closed agent fails.
                    if hub.submit(fire.prompt()).is_err() {
                        return;
                    }
                }
                pass.wake
            }
            Err(e) => {
                if startup {
                    hub.publish(Event::Info(format!("schedules: {e}")));
                }
                None
            }
        };
        drop(hub);
        startup = false;
        let nap = wake
            .and_then(|t| (t - schedules.now()).to_std().ok())
            .map_or(NAP, |d| d.min(NAP));
        tokio::select! {
            () = tokio::time::sleep(nap) => {}
            () = schedules.poke.notified() => {}
        }
    }
}

/// The slot after `now` for a recurring row. `every` keeps its phase from the slot it
/// was set for; the wall-clock forms ask the spec.
fn following(row: &Row, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match &row.spec {
        Spec::Every(step) => {
            let behind = (now - row.next).num_seconds().max(0);
            let steps = behind / step.num_seconds().max(1) + 1;
            row.next
                .checked_add_signed(step.checked_mul(i32::try_from(steps).ok()?)?)
        }
        spec => spec
            .next_after(&now.with_timezone(&Local))
            .map(|t| t.with_timezone(&Utc)),
    }
}

/// `row` as a schedule, or why it is not one bhai could have written.
fn checked(row: &Value) -> Result<Row, String> {
    let row: Row = serde_json::from_value(row.clone()).map_err(|e| e.to_string())?;
    if row.id.is_empty() || row.id.len() > 16 || !row.id.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return Err(format!("its id {:?} is not one bhai names", row.id));
    }
    if row.text.trim().is_empty() {
        return Err("its prompt is empty".to_string());
    }
    if row.text.len() > TEXT_MAX {
        return Err(format!("its prompt is over {TEXT_MAX} bytes"));
    }
    if row.fires_left == Some(0) {
        return Err("it has no fires left".to_string());
    }
    if row.spec.recurs() && row.expires.is_none() {
        return Err("it recurs with no expiry".to_string());
    }
    Ok(row)
}

/// Where schedule `id` is among the rows.
fn find(rows: &[Value], id: &str) -> Result<usize, String> {
    rows.iter()
        .position(|row| row["id"] == id)
        .ok_or_else(|| format!("no schedule {id}"))
}

/// The line for row `at` of the store, left alone for `why`.
fn skipped(at: usize, why: &str) -> String {
    format!("row {} of .bhai/{FILE} was left alone: {why}", at + 1)
}

/// An instant as the local wall clock reads it.
fn local(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Cancel, UserInput};
    use crate::permissions::Policy;
    use crate::session::Submitted;
    use tokio::sync::mpsc;

    fn session() -> (Arc<Session>, mpsc::Receiver<UserInput>) {
        let (tx_user, rx_user) = mpsc::channel(8);
        let (tx_control, _) = mpsc::channel(1);
        (
            Session::new(
                "m".to_string(),
                "medium".to_string(),
                "general".to_string(),
                tx_user,
                tx_control,
                Arc::new(Cancel::default()),
                Arc::new(Policy::default()),
                None,
            ),
            rx_user,
        )
    }

    fn base() -> DateTime<Utc> {
        "2026-10-03T12:00:00Z".parse().unwrap()
    }

    /// `base()` plus however far paused tokio time has moved since this was made.
    fn paused_clock() -> Clock {
        let start = tokio::time::Instant::now();
        Arc::new(move || base() + Duration::from_std(start.elapsed()).unwrap())
    }

    fn schedules(project: &Path) -> Schedules {
        Schedules::new(project).with_clock(paused_clock())
    }

    fn new(spec: &str, text: &str) -> New {
        New {
            spec: spec.parse().unwrap(),
            text: text.to_string(),
            origin: Origin::User,
            times: None,
            lasts: None,
        }
    }

    /// The next prompt the agent is sent, and how far paused time had to go for it.
    async fn next_prompt(rx: &mut mpsc::Receiver<UserInput>) -> (String, Duration) {
        let start = tokio::time::Instant::now();
        let input = tokio::time::timeout(std::time::Duration::from_secs(86_400), rx.recv())
            .await
            .expect("nothing fired")
            .unwrap();
        (input.text, Duration::from_std(start.elapsed()).unwrap())
    }

    fn infos(session: &Session) -> Vec<String> {
        session
            .entries()
            .list
            .iter()
            .filter_map(|e| match e {
                crate::entries::Entry::Info(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn a_schedule_fires_at_its_time_framed_as_scheduled_and_is_gone() {
        let dir = crate::tools::temp_dir();
        let store = schedules(&dir);
        let row = store.add(new("in 20m", "check CI")).unwrap();
        assert_eq!(row.next, base() + Duration::minutes(20));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.join(".bhai").join(FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let (session, mut rx) = session();
        session.run_schedules(store);
        let (text, waited) = next_prompt(&mut rx).await;
        assert_eq!(waited, Duration::minutes(20));
        assert!(
            text.starts_with("[scheduled: the user set this at "),
            "{text}"
        );
        assert!(text.contains("with `in 20m`"), "{text}");
        assert!(!text.contains("late"), "{text}");
        assert!(text.ends_with("\n\ncheck CI"), "{text}");
        let shown = session.entries().list.clone();
        assert!(
            shown.iter().any(|e| matches!(e, crate::entries::Entry::User(t) if t == "(scheduled `in 20m`) check CI")),
            "{shown:?}"
        );
        let (rows, bad) = schedules(&dir).list().unwrap();
        assert!(rows.is_empty() && bad.is_empty(), "{rows:?} {bad:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_fire_during_a_turn_queues_behind_it() {
        let dir = crate::tools::temp_dir();
        let store = schedules(&dir);
        store.add(new("in 5m", "look again")).unwrap();
        let (session, mut rx) = session();
        assert_eq!(session.submit("busy".to_string()), Ok(Submitted::Started));
        assert_eq!(rx.recv().await.unwrap().text, "busy");
        session.run_schedules(store);
        tokio::time::sleep(std::time::Duration::from_secs(5 * 60 + 1)).await;
        assert_eq!(session.queued(), ["(scheduled `in 5m`) look again"]);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_recurring_schedule_fires_until_its_count_runs_out() {
        let dir = crate::tools::temp_dir();
        let store = schedules(&dir);
        store
            .add(New {
                times: Some(2),
                ..new("every 10m", "poll")
            })
            .unwrap();
        let (session, mut rx) = session();
        session.run_schedules(store);
        assert_eq!(next_prompt(&mut rx).await.1, Duration::minutes(10));
        // The first turn is still running, so the second queues.
        tokio::time::sleep(std::time::Duration::from_secs(10 * 60 + 1)).await;
        assert_eq!(session.queued().len(), 1);
        assert!(
            infos(&session)
                .iter()
                .any(|i| i.contains("has fired its last time"))
        );
        let (rows, _) = schedules(&dir).list().unwrap();
        assert!(rows.is_empty(), "{rows:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_recurring_schedule_ends_at_its_expiry() {
        let dir = crate::tools::temp_dir();
        let store = schedules(&dir);
        let row = store.add(new("every 1d", "daily")).unwrap();
        assert_eq!(row.expires, Some(base() + EXPIRY));
        let short = store
            .add(New {
                lasts: Some(Duration::minutes(25)),
                ..new("every 10m", "brief")
            })
            .unwrap();
        assert_eq!(short.expires, Some(base() + Duration::minutes(25)));
        assert!(
            store
                .add(New {
                    lasts: Some(Duration::minutes(5)),
                    ..new("every 10m", "never")
                })
                .is_err()
        );
        let (session, mut rx) = session();
        session.run_schedules(store);
        assert!(next_prompt(&mut rx).await.0.ends_with("brief"));
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        assert_eq!(session.queued(), ["(scheduled `every 10m`) brief"]);
        let (rows, _) = schedules(&dir).list().unwrap();
        assert_eq!(
            rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            ["daily"]
        );
        assert!(
            infos(&session)
                .iter()
                .any(|i| i.contains(&short.id) && i.contains("before it expires"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_schedule_does_not_fire() {
        let dir = crate::tools::temp_dir();
        let store = Arc::new(schedules(&dir));
        let row = store.add(new("in 10m", "never sent")).unwrap();
        let (session, mut rx) = session();
        tokio::spawn(run(Arc::downgrade(&session), Arc::clone(&store)));
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        assert_eq!(store.cancel(&row.id).unwrap().text, "never sent");
        assert!(store.cancel(&row.id).is_err());
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_paused_schedule_waits_and_a_new_one_wakes_the_runner() {
        let dir = crate::tools::temp_dir();
        let store = Arc::new(schedules(&dir));
        let held = store.add(new("in 2m", "held")).unwrap();
        store.pause(&held.id, true).unwrap();
        let (session, mut rx) = session();
        tokio::spawn(run(Arc::downgrade(&session), Arc::clone(&store)));
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        // Added while the runner sleeps a full nap; it still fires on time.
        store.add(new("in 1m", "added")).unwrap();
        let (text, waited) = next_prompt(&mut rx).await;
        assert!(text.ends_with("added"), "{text}");
        assert_eq!(waited, Duration::minutes(1));
        tokio::time::sleep(std::time::Duration::from_secs(600)).await;
        assert!(session.queued().is_empty());
        store.pause(&held.id, false).unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        assert_eq!(session.queued(), ["(scheduled `in 2m`) held"]);
    }

    /// What a bhai that stopped at `base()` left behind: rows due at 11:00.
    fn stale(dir: &Path) {
        let row = |id: &str, spec: &str, text: &str| {
            let spec: Spec = spec.parse().unwrap();
            let expires = spec.recurs().then(|| base() + EXPIRY);
            serde_json::to_value(Row {
                id: id.to_string(),
                spec,
                text: text.to_string(),
                origin: Origin::User,
                created: base() - Duration::hours(2),
                next: base() - Duration::hours(1),
                fires_left: None,
                expires,
                paused: false,
            })
            .unwrap()
        };
        let rows = vec![
            row("once01", "in 1h", "late one"),
            row("every1", "every 25m", "recurring"),
            json_junk(),
        ];
        crate::sessions::private_dir(&dir.join(".bhai")).unwrap();
        std::fs::write(
            dir.join(".bhai").join(FILE),
            serde_json::to_string_pretty(&rows).unwrap(),
        )
        .unwrap();
    }

    fn json_junk() -> Value {
        serde_json::json!({ "id": "junk", "spec": "every 1m", "text": "x", "origin": "user",
            "created": "2026-10-03T00:00:00Z", "next": "2026-10-03T00:00:00Z" })
    }

    #[tokio::test(start_paused = true)]
    async fn on_restart_a_missed_once_fires_and_a_missed_recurring_skips_ahead() {
        let dir = crate::tools::temp_dir();
        stale(&dir);
        let (session, mut rx) = session();
        session.run_schedules(schedules(&dir));
        let (text, waited) = next_prompt(&mut rx).await;
        assert_eq!(waited, Duration::zero());
        assert!(
            text.contains("while bhai was not running, so it runs late"),
            "{text}"
        );
        assert!(text.ends_with("late one"), "{text}");
        assert!(session.entries().list.iter().any(
            |e| matches!(e, crate::entries::Entry::User(t) if t.starts_with("(missed `in 1h`, due "))
        ));
        let notes = infos(&session);
        assert!(
            notes.iter().any(|n| n.contains("every1")
                && n.contains("while bhai was not running; it fires next at")),
            "{notes:?}"
        );
        // An unreadable row is named, and kept as it was.
        assert!(
            notes.iter().any(|n| n.contains(
                "row 3 of .bhai/schedules.json was left alone: it recurs with no expiry"
            )),
            "{notes:?}"
        );
        let saved: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(dir.join(".bhai").join(FILE)).unwrap())
                .unwrap();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[1], json_junk());
        let (rows, bad) = schedules(&dir).list().unwrap();
        assert_eq!(bad.len(), 1);
        // 11:00 every 25m keeps its phase: 12:15 is the next slot after noon.
        assert_eq!(rows[0].next, base() + Duration::minutes(15));
        // The recurring one fires at that slot, behind the late turn, and is not missed.
        tokio::time::sleep(std::time::Duration::from_secs(15 * 60 - 1)).await;
        assert!(session.queued().is_empty());
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert_eq!(session.queued(), ["(scheduled `every 25m`) recurring"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_store_that_does_not_parse_is_reported_and_left_alone() {
        let dir = crate::tools::temp_dir();
        crate::sessions::private_dir(&dir.join(".bhai")).unwrap();
        std::fs::write(dir.join(".bhai").join(FILE), "{ not json").unwrap();
        let store = schedules(&dir);
        assert!(store.add(new("in 5m", "x")).is_err());
        let (session, _rx) = session();
        session.run_schedules(schedules(&dir));
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        assert!(infos(&session).iter().any(|i| i.contains("does not parse")));
        assert_eq!(
            std::fs::read_to_string(dir.join(".bhai").join(FILE)).unwrap(),
            "{ not json"
        );
    }

    #[test]
    fn rows_bhai_could_not_have_written_are_refused() {
        let good = serde_json::to_value(Row {
            id: "abc123".to_string(),
            spec: "every 5m".parse().unwrap(),
            text: "ok".to_string(),
            origin: Origin::Model,
            created: base(),
            next: base(),
            fires_left: Some(3),
            expires: Some(base() + EXPIRY),
            paused: false,
        })
        .unwrap();
        assert!(checked(&good).is_ok());
        for (field, value) in [
            ("id", serde_json::json!("../x")),
            ("id", serde_json::json!("")),
            ("text", serde_json::json!("  ")),
            ("text", serde_json::json!("x".repeat(TEXT_MAX + 1))),
            ("fires_left", serde_json::json!(0)),
            ("expires", Value::Null),
            ("spec", serde_json::json!("every 10s")),
            ("origin", serde_json::json!("someone")),
            ("next", serde_json::json!("soon")),
        ] {
            let mut row = good.clone();
            row[field] = value;
            assert!(checked(&row).is_err(), "{field}: {row}");
        }
    }
}
