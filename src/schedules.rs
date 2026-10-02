//! The schedules of a project, kept in bhai's config directory under a name drawn from
//! the project's root, and the runner that fires each into the session as it falls due.
//!
//! The store is never in the project: a schedule starts a turn with nobody at the
//! keyboard, framed as the user's, and a clone can commit anything under `.bhai`.
//!
//! A fire is claimed under the store's lock before it is submitted: the row is moved on
//! or removed and the file saved first, so two bhai processes in one project never both
//! fire the same slot, and a crash between the two loses a fire rather than repeating it.
//! A slot that passed while no bhai ran is missed: a one-shot row fires late and says so,
//! a recurring one skips to its next slot and says so, rather than firing a backlog.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use chrono::{DateTime, Duration, Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::schedule::Spec;
use crate::session::{Event, Prompt, Session};
use crate::worktrees::Lock;

/// The stores, one per project, under bhai's config directory.
pub const DIR: &str = "schedules";
/// How long a recurring row lives when nothing else is asked for.
pub const EXPIRY: Duration = Duration::days(7);
/// The longest a recurring row may live.
pub const LASTS_MAX: Duration = Duration::days(30);
/// The most fires a recurring row may be given.
pub const TIMES_MAX: u32 = 1000;
/// The longest prompt a row carries, in bytes.
pub const TEXT_MAX: usize = 8192;
/// The most schedules the model may have set at once.
pub const MODEL_ROWS_MAX: usize = 10;
/// The longest prompt the model may leave itself, in bytes.
pub const MODEL_TEXT_MAX: usize = 1024;
/// `new` as the row it would be at `now`, its id still to be drawn, or why it cannot be
/// one.
fn prepare(new: New, now: DateTime<Utc>) -> Result<Row, String> {
    let model = new.origin == Origin::Model;
    let text = match model {
        true => crate::redact::apply(&new.text).into_owned(),
        false => new.text,
    };
    if model && text.len() > MODEL_TEXT_MAX {
        return Err(format!(
            "the prompt is {} bytes, over the {MODEL_TEXT_MAX} a schedule you set may carry",
            text.len()
        ));
    }
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
        (true, Some(lasts)) if lasts > LASTS_MAX => {
            return Err(format!(
                "a schedule lasts at most {} days",
                LASTS_MAX.num_days()
            ));
        }
        (true, lasts) => now.checked_add_signed(lasts.unwrap_or(EXPIRY)),
    };
    if expires.is_some_and(|end| next > end) {
        return Err(format!("`{}` would expire before it first fires", new.spec));
    }
    let fires_left = match (recurs, new.times) {
        (_, Some(0)) => return Err("a schedule has to fire at least once".to_string()),
        (true, Some(times)) if times > TIMES_MAX => {
            return Err(format!("a schedule fires at most {TIMES_MAX} times"));
        }
        (true, times) => times,
        (false, _) => None,
    };
    if model
        && let Some(gap) = shortest_gap(&new.spec, next, expires.unwrap_or(next))
        && gap < MODEL_GAP_MIN
    {
        return Err(format!(
            "`{}` fires {} minute(s) apart, under the {} minutes a schedule you set must leave",
            new.spec,
            gap.num_minutes(),
            MODEL_GAP_MIN.num_minutes()
        ));
    }
    Ok(Row {
        id: String::new(),
        spec: new.spec,
        text,
        origin: new.origin,
        created: now,
        next,
        fires_left,
        expires,
        paused: false,
    })
}

/// Whether the model may set one more schedule beside `rows`.
fn model_room(rows: &[Value], now: DateTime<Utc>) -> Result<(), String> {
    let set = rows
        .iter()
        .filter(|row| checked(row, now).is_ok_and(|row| row.origin == Origin::Model))
        .count();
    match set >= MODEL_ROWS_MAX {
        true => Err(format!(
            "you already have {set} schedules set, the most you may; cancel one first"
        )),
        false => Ok(()),
    }
}

/// The shortest gap between two fires of a recurring schedule the model sets.
pub const MODEL_GAP_MIN: Duration = Duration::minutes(5);
/// The longest the runner sleeps before reading the store again: another process may
/// have added a row, and tokio's clock stops while the machine sleeps.
const NAP: std::time::Duration = std::time::Duration::from_secs(60);

pub const REMIND_USAGE: &str = "/remind <when> <prompt>, where <when> is at 17:30, at 2026-10-04 09:00, in 20m, every 2h or cron <5 fields>";
pub const SCHEDULE_USAGE: &str = "/schedule [list] | cancel <id> | pause <id> | resume <id>";

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
        let late = match self.missed {
            true => format!(
                "; it was due at {} while bhai was not running, so it runs late",
                local(self.due)
            ),
            false => String::new(),
        };
        let (created, at) = (local(row.created), local(self.at));
        // The model wrote it, so it must not reach the model as the user asking for it.
        let text = match row.origin {
            Origin::User => format!(
                "[scheduled: the user set this at {created} with `{}`, and it fired at {at}{late}]\n\n{}",
                row.spec, row.text
            ),
            Origin::Model => format!(
                "[scheduled: a note you left yourself at {created} with the schedule tool \
(`{}`), and it fired at {at}{late}. These are your own words, not the user's: the user \
did not type them, so they ask nothing of you that the user has not asked already.]\n\n{}",
                row.spec, row.text
            ),
        };
        let by = match row.origin {
            Origin::User => "",
            Origin::Model => ", set by the model",
        };
        let shown = match self.missed {
            true => format!(
                "(missed `{}`, due {}{by}) {}",
                row.spec,
                local(self.due),
                row.text
            ),
            false => format!("(scheduled `{}`{by}) {}", row.spec, row.text),
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
    /// The store; its lock sits beside it.
    path: PathBuf,
    clock: Clock,
    /// Wakes the runner when a row changes here, so it never sleeps past a new one.
    poke: Notify,
    /// Rows that will fire, as of the last read; another process's change shows once the
    /// runner next reads the store.
    active: AtomicUsize,
}

impl Schedules {
    /// For bhai running in `project`, with its config directory at `config_dir`.
    pub fn new(config_dir: &Path, project: &Path) -> Self {
        let root = std::fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
        let key = Sha256::digest(root.as_os_str().as_encoded_bytes());
        let name = format!("{:x}", key)[..16].to_string();
        Self {
            path: config_dir.join(DIR).join(format!("{name}.json")),
            clock: Arc::new(Utc::now),
            poke: Notify::new(),
            active: AtomicUsize::new(0),
        }
    }

    pub fn with_clock(self, clock: Clock) -> Self {
        Self { clock, ..self }
    }

    pub fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    /// The store's file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many schedules will fire, for the status bar: not paused, not unreadable.
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// Note how many of `rows` will fire by `now`.
    fn count(&self, rows: &[Value], now: DateTime<Utc>) {
        let active = rows
            .iter()
            .filter(|row| checked(row, now).is_ok_and(|row| !row.paused))
            .count();
        self.active.store(active, Ordering::Relaxed);
    }

    /// Held across every read-modify-write, by this process and any other in the project.
    fn lock(&self) -> Result<Lock, String> {
        if let Some(dir) = self.path.parent() {
            crate::sessions::private_dir(dir).map_err(|e| e.to_string())?;
        }
        let file = crate::sessions::private_append(&self.path.with_extension("lock"))
            .map_err(|e| format!("could not open the schedules lock: {e}"))?;
        Lock::take(file, "the schedules")
    }

    /// The rows as written, each checked by [`checked`] before use.
    fn load(&self) -> Result<Vec<Value>, String> {
        let unparsed =
            |e: serde_json::Error| format!("{} does not parse: {e}", self.path.display());
        match std::fs::read_to_string(&self.path) {
            Ok(text) if text.trim().is_empty() => Ok(Vec::new()),
            Ok(text) => serde_json::from_str(&text).map_err(unparsed),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("could not read {}: {e}", self.path.display())),
        }
    }

    /// Written aside and renamed over, so a crash mid-write leaves the old rows whole.
    fn save(&self, rows: &[Value]) -> Result<(), String> {
        let text = serde_json::to_string_pretty(rows).map_err(|e| e.to_string())?;
        let aside = self
            .path
            .with_extension(format!("{}.tmp", std::process::id()));
        crate::sessions::private_write(&aside, &text)
            .and_then(|()| std::fs::rename(&aside, &self.path))
            .map_err(|e| format!("could not write {}: {e}", self.path.display()))
    }

    /// The line for row `at` of the store, left alone for `why`.
    fn skipped(&self, at: usize, why: &str) -> String {
        format!(
            "row {} of {} was left alone: {why}",
            at + 1,
            self.path.display()
        )
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
        self.count(&rows, self.now());
        self.poke.notify_one();
        Ok(done)
    }

    /// Add a schedule; its first fire counts from now. One the model sets is held to
    /// tighter bounds, and its prompt has the secrets bhai knows blanked out.
    pub fn add(&self, new: New) -> Result<Row, String> {
        let now = self.now();
        let mut row = prepare(new, now)?;
        self.edit(|rows| {
            if row.origin == Origin::Model {
                model_room(rows, now)?;
            }
            let taken = |id: &str| rows.iter().any(|row| row["id"] == id);
            row.id = std::iter::repeat_with(|| {
                uuid::Uuid::new_v4().simple().to_string()[..6].to_string()
            })
            .find(|id| !taken(id))
            .unwrap_or_default();
            let value = serde_json::to_value(&row).map_err(|e| e.to_string())?;
            checked(&value, now)?;
            rows.push(value);
            Ok(row)
        })
    }

    /// Why [`add`](Self::add) would refuse `new` now, so the user is not asked to approve
    /// a schedule that cannot be set. Another process may still fill the last slot first.
    pub fn vet(&self, new: New) -> Result<(), String> {
        let now = self.now();
        let row = prepare(new, now)?;
        match row.origin {
            Origin::Model => model_room(&self.load()?, now),
            Origin::User => Ok(()),
        }
    }

    /// `/remind <spec> <text>`: a schedule the user set, firing `text`.
    pub fn remind(&self, text: &str) -> Result<Row, String> {
        let (spec, text) = Spec::split(text)?;
        let text = text.trim();
        if text.is_empty() {
            return Err(format!(
                "`{spec}` has nothing to say when it fires: {REMIND_USAGE}"
            ));
        }
        self.add(New {
            spec,
            text: text.to_string(),
            origin: Origin::User,
            times: None,
            lasts: None,
        })
    }

    /// `/schedule` with what followed it: `list` (or nothing), `cancel <id>`, `pause <id>`
    /// or `resume <id>`. Returns what to show.
    pub fn command(&self, rest: &str) -> Result<String, String> {
        let (verb, id) = rest
            .trim()
            .split_once(char::is_whitespace)
            .map_or((rest.trim(), ""), |(verb, id)| (verb, id.trim()));
        let now = self.now();
        match (verb, id) {
            ("" | "list", "") => {
                let (rows, bad) = self.list()?;
                Ok(report(&rows, &bad, now))
            }
            ("cancel" | "pause" | "resume", "") => Err(SCHEDULE_USAGE.to_string()),
            ("cancel", id) => self
                .cancel(id)
                .map(|row| format!("cancelled {}", describe(&row, now))),
            ("pause", id) => self
                .pause(id, true)
                .map(|row| format!("paused {}", describe(&row, now))),
            ("resume", id) => self
                .pause(id, false)
                .map(|row| format!("resumed {}", describe(&row, now))),
            _ => Err(SCHEDULE_USAGE.to_string()),
        }
    }

    /// Every row that is a schedule, and a line for each row that is not.
    pub fn list(&self) -> Result<(Vec<Row>, Vec<String>), String> {
        let now = self.now();
        let mut found = (Vec::new(), Vec::new());
        let rows = self.load()?;
        self.count(&rows, now);
        for (at, row) in rows.iter().enumerate() {
            match checked(row, now) {
                Ok(row) => found.0.push(row),
                Err(why) => found.1.push(self.skipped(at, &why)),
            }
        }
        Ok(found)
    }

    /// Remove schedule `id`.
    pub fn cancel(&self, id: &str) -> Result<Row, String> {
        self.remove(id, Origin::User)
    }

    /// Remove schedule `id` for `by`: the user may remove any, the model only its own.
    pub fn remove(&self, id: &str, by: Origin) -> Result<Row, String> {
        let now = self.now();
        self.edit(|rows| {
            let at = find(rows, id)?;
            let row = checked(&rows[at], now)?;
            if by == Origin::Model && row.origin != Origin::Model {
                return Err(format!(
                    "{id} is the user's schedule; only they can cancel it"
                ));
            }
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
            let mut row = checked(&rows[at], now)?;
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
            let mut row = match checked(&value, now) {
                Ok(row) => row,
                Err(why) => {
                    if startup {
                        pass.notes.push(self.skipped(at, &why));
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
        self.count(&kept, now);
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
                    hub.publish(Event::Scheduled {
                        id: fire.row.id.clone(),
                        origin: fire.row.origin,
                        spec: fire.row.spec.to_string(),
                        text: fire.row.text.clone(),
                        missed: fire.missed,
                        queued: hub.working(),
                    });
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

/// The shortest gap between two fires of `spec` from its fire at `first` to `until`;
/// `None` for one that fires once. A cron is walked fire by fire, giving up past
/// [`GAP_WALK`] fires, which only a gap far under any minimum reaches.
fn shortest_gap(spec: &Spec, first: DateTime<Utc>, until: DateTime<Utc>) -> Option<Duration> {
    match spec {
        Spec::Every(step) => Some(*step),
        Spec::Cron(_) => {
            let mut at = first.with_timezone(&Local);
            let mut shortest: Option<Duration> = None;
            for _ in 0..GAP_WALK {
                let Some(next) = spec.next_after(&at).filter(|t| *t <= until) else {
                    break;
                };
                let gap = next - at;
                shortest = Some(shortest.map_or(gap, |s| s.min(gap)));
                at = next;
            }
            shortest
        }
        Spec::At { .. } | Spec::In(_) => None,
    }
}

/// Fires walked by [`shortest_gap`]: a gap of 5 minutes fills 30 days with 8640.
const GAP_WALK: usize = 10_000;

/// `row` as a schedule, or why it is not one bhai could have written by `now`.
fn checked(row: &Value, now: DateTime<Utc>) -> Result<Row, String> {
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
    if row.fires_left.is_some_and(|n| n > TIMES_MAX) {
        return Err(format!("it has over {TIMES_MAX} fires left"));
    }
    if row.created > now {
        return Err("it was set later than now".to_string());
    }
    if row.spec.recurs() {
        let Some(expires) = row.expires else {
            return Err("it recurs with no expiry".to_string());
        };
        if expires - row.created > LASTS_MAX {
            return Err(format!("it lasts over {} days", LASTS_MAX.num_days()));
        }
    }
    Ok(row)
}

/// What `/schedule list` shows.
pub fn report(rows: &[Row], bad: &[String], now: DateTime<Utc>) -> String {
    let mut out = match rows.len() {
        0 => "schedules: none. /remind <when> <prompt> sets one".to_string(),
        n => format!("schedules: {n}"),
    };
    for row in rows {
        out.push_str(&format!("\n  {}", describe(row, now)));
    }
    for line in bad {
        out.push_str(&format!("\n  {line}"));
    }
    out
}

/// One row on a line: its id, spec, next fire, how it ends, and its prompt.
pub fn describe(row: &Row, now: DateTime<Utc>) -> String {
    let mut out = format!("{} `{}`", row.id, row.spec);
    match row.paused {
        true => out.push_str(" paused"),
        false if row.next <= now => out.push_str(" due now"),
        false => out.push_str(&format!(" next {}", local(row.next))),
    }
    if let Some(n) = row.fires_left {
        out.push_str(&format!(", {n} fire(s) left"));
    }
    if let Some(end) = row.expires {
        out.push_str(&format!(", ends {}", local(end)));
    }
    if row.origin == Origin::Model {
        out.push_str(", set by the model");
    }
    let text: String = row.text.split_whitespace().collect::<Vec<_>>().join(" ");
    let clipped: String = text.chars().take(60).collect();
    match clipped.len() < text.len() {
        true => out.push_str(&format!(": {clipped}…")),
        false => out.push_str(&format!(": {text}")),
    }
    out
}

/// Where schedule `id` is among the rows.
fn find(rows: &[Value], id: &str) -> Result<usize, String> {
    rows.iter()
        .position(|row| row["id"] == id)
        .ok_or_else(|| format!("no schedule {id}"))
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

    /// The store of `dir`, with its config directory there too.
    fn schedules(dir: &Path) -> Schedules {
        Schedules::new(dir, dir).with_clock(paused_clock())
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
            let mode = std::fs::metadata(store.path())
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
        let at = shown
            .iter()
            .position(|e| matches!(e, crate::entries::Entry::User(t) if t == "(scheduled `in 20m`) check CI"))
            .unwrap_or_else(|| panic!("{shown:?}"));
        // Marked right above the prompt it started a turn with.
        assert!(
            matches!(&shown[at - 1], crate::entries::Entry::Info(t) if *t == format!("schedule {} fired", row.id)),
            "{shown:?}"
        );
        let (rows, bad) = schedules(&dir).list().unwrap();
        assert!(rows.is_empty() && bad.is_empty(), "{rows:?} {bad:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_schedule_the_model_set_fires_as_its_note_and_it_cancels_only_its_own() {
        let dir = crate::tools::temp_dir();
        let store = schedules(&dir);
        let model = |spec: &str, text: &str| New {
            origin: Origin::Model,
            ..new(spec, text)
        };
        for spec in ["every 4m", "cron 0,3 * * * *", "cron */1 9 * * *"] {
            let err = store.add(model(spec, "poll")).unwrap_err();
            assert!(err.contains("under the 5 minutes"), "{spec}: {err}");
        }
        assert!(store.add(model("every 5m", "poll")).is_ok());
        // The user is not held to it.
        let user = store.add(new("every 1m", "mine")).unwrap();
        assert!(
            store
                .remove(&user.id, Origin::Model)
                .unwrap_err()
                .contains("the user's schedule")
        );
        let long = model("in 20m", &"x".repeat(MODEL_TEXT_MAX + 1));
        assert!(store.vet(long.clone()).is_err() && store.add(long).is_err());
        assert!(
            store
                .add(new("in 20m", &"x".repeat(MODEL_TEXT_MAX + 1)))
                .is_ok()
        );
        store.cancel(&user.id).unwrap();

        let dir = crate::tools::temp_dir();
        let store = schedules(&dir);
        let row = store.add(model("in 20m", "check CI")).unwrap();
        let (session, mut rx) = session();
        session.run_schedules(store);
        let (text, _) = next_prompt(&mut rx).await;
        assert!(
            text.starts_with("[scheduled: a note you left yourself at "),
            "{text}"
        );
        assert!(text.contains("not the user's"), "{text}");
        assert!(text.ends_with("\n\ncheck CI"), "{text}");
        let shown = session.entries().list.clone();
        let at = shown
            .iter()
            .position(|e| matches!(e, crate::entries::Entry::User(t) if t == "(scheduled `in 20m`, set by the model) check CI"))
            .unwrap_or_else(|| panic!("{shown:?}"));
        assert!(
            matches!(&shown[at - 1], crate::entries::Entry::Info(t) if *t == format!("schedule {} (set by the model) fired", row.id)),
            "{shown:?}"
        );
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
        let mut events = session.subscribe();
        tokio::time::sleep(std::time::Duration::from_secs(5 * 60 + 1)).await;
        assert_eq!(session.queued(), ["(scheduled `in 5m`) look again"]);
        assert!(rx.try_recv().is_err());
        assert!(matches!(
            events.try_recv(),
            Ok(Event::Scheduled { queued: true, missed: false, ref spec, .. }) if spec == "in 5m"
        ));
        // It may land mid-answer, so only the queue and the prompt say it was scheduled.
        assert!(!infos(&session).iter().any(|i| i.contains("fired")));
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
        let path = schedules(dir).path().to_path_buf();
        crate::sessions::private_dir(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(&rows).unwrap()).unwrap();
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
            notes.iter().any(|n| n.starts_with("schedules: row 3 of ")
                && n.ends_with(".json was left alone: it recurs with no expiry")),
            "{notes:?}"
        );
        let saved: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(schedules(&dir).path()).unwrap())
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
        let store = schedules(&dir);
        crate::sessions::private_dir(store.path().parent().unwrap()).unwrap();
        std::fs::write(store.path(), "{ not json").unwrap();
        assert!(store.add(new("in 5m", "x")).is_err());
        let (session, _rx) = session();
        session.run_schedules(schedules(&dir));
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        assert!(infos(&session).iter().any(|i| i.contains("does not parse")));
        assert_eq!(std::fs::read_to_string(store.path()).unwrap(), "{ not json");
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
        assert!(checked(&good, base()).is_ok());
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
            ("fires_left", serde_json::json!(TIMES_MAX + 1)),
            ("expires", serde_json::json!("2100-01-01T00:00:00Z")),
            ("created", serde_json::json!("2026-10-03T12:01:00Z")),
        ] {
            let mut row = good.clone();
            row[field] = value;
            assert!(checked(&row, base()).is_err(), "{field}: {row}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn remind_and_the_schedule_command_set_list_pause_and_cancel() {
        let dir = crate::tools::temp_dir();
        let store = schedules(&dir);
        assert_eq!(store.active(), 0);
        let row = store.remind("in 20m  check CI ").unwrap();
        assert_eq!((row.text.as_str(), row.origin), ("check CI", Origin::User));
        assert!(
            store
                .remind("in 20m")
                .unwrap_err()
                .contains("nothing to say")
        );
        assert!(store.remind("soon check CI").is_err());
        let polled = store.remind("every 2h poll the queue").unwrap();
        assert_eq!(store.active(), 2);

        let list = store.command("").unwrap();
        assert_eq!(list, store.command(" list ").unwrap());
        assert!(list.starts_with("schedules: 2"), "{list}");
        assert!(
            list.contains(&format!("{} `in 20m` next ", row.id))
                && list.ends_with(": poll the queue"),
            "{list}"
        );
        assert!(list.contains(", ends "), "{list}");

        let paused = store.command(&format!("pause {}", polled.id)).unwrap();
        assert!(paused.starts_with(&format!("paused {} `every 2h` paused", polled.id)));
        assert_eq!(store.active(), 1);
        assert!(store.command(&format!("resume {}", polled.id)).is_ok());
        assert_eq!(store.active(), 2);
        assert!(
            store
                .command(&format!("cancel {}", row.id))
                .unwrap()
                .starts_with("cancelled ")
        );
        assert_eq!(store.active(), 1);
        assert_eq!(
            store.command(&format!("cancel {}", row.id)).unwrap_err(),
            format!("no schedule {}", row.id)
        );
        for bad in ["cancel", "drop x", "list x"] {
            assert_eq!(store.command(bad).unwrap_err(), SCHEDULE_USAGE, "{bad}");
        }
        store.command(&format!("cancel {}", polled.id)).unwrap();
        assert!(
            store
                .command("list")
                .unwrap()
                .starts_with("schedules: none")
        );
    }

    #[test]
    fn a_row_is_one_line_with_a_long_prompt_cut() {
        let row = Row {
            id: "abc123".to_string(),
            spec: "every 5m".parse().unwrap(),
            text: format!("look\n{}", "x".repeat(100)),
            origin: Origin::Model,
            created: base(),
            next: base() - Duration::minutes(1),
            fires_left: Some(3),
            expires: None,
            paused: false,
        };
        let line = describe(&row, base());
        assert!(
            line.starts_with("abc123 `every 5m` due now, 3 fire(s) left, set by the model: look x"),
            "{line}"
        );
        assert!(line.ends_with("x…") && !line.contains('\n'), "{line}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_schedule_cannot_last_or_fire_without_bound() {
        let dir = crate::tools::temp_dir();
        let store = schedules(&dir);
        let long = New {
            lasts: Some(LASTS_MAX + Duration::minutes(1)),
            ..new("every 1h", "x")
        };
        assert!(store.add(long).is_err());
        let many = New {
            times: Some(TIMES_MAX + 1),
            ..new("every 1h", "x")
        };
        assert!(store.add(many).is_err());
        let most = New {
            lasts: Some(LASTS_MAX),
            times: Some(TIMES_MAX),
            ..new("every 1h", "x")
        };
        assert!(store.add(most).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn the_store_lives_outside_the_project_one_per_project() {
        let dir = crate::tools::temp_dir();
        let (config, a, b) = (dir.join("config"), dir.join("a"), dir.join("b"));
        for d in [&a, &b] {
            std::fs::create_dir_all(d.join(".bhai")).unwrap();
        }
        // What a clone could commit; bhai never reads it.
        std::fs::write(
            a.join(".bhai").join("schedules.json"),
            serde_json::to_string(&vec![json_junk()]).unwrap(),
        )
        .unwrap();
        let store = Schedules::new(&config, &a).with_clock(paused_clock());
        assert!(store.path().starts_with(config.join(DIR)));
        assert_eq!(store.list().unwrap(), (Vec::new(), Vec::new()));
        store.add(new("in 5m", "only a")).unwrap();
        let other = Schedules::new(&config, &b).with_clock(paused_clock());
        assert_ne!(store.path(), other.path());
        assert!(other.list().unwrap().0.is_empty());
        assert_eq!(Schedules::new(&config, &a.join(".")).path(), store.path());
    }
}
