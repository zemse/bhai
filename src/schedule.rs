//! When a schedule fires: the spec a user or the model writes (`at 17:30`,
//! `at 2026-10-04 09:00`, `in 20m`, `every 15m`, `cron */5 9-17 * * mon-fri`) and the
//! next fire after a given instant. Pure: the caller passes the instant and the time zone,
//! so nothing here reads the clock.
//!
//! Wall-clock forms (`at` and `cron`) follow local time across DST: a local time the
//! clocks skip fires at the next minute that exists, and one they repeat fires once, on
//! its first pass. `in` and `every` are elapsed time and ignore DST.

use std::fmt;
use std::str::FromStr;

use chrono::{
    DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, Offset,
    TimeZone, Timelike,
};
use serde::{Deserialize, Serialize};

/// The shortest `in` or `every`.
pub const MIN_INTERVAL: Duration = Duration::minutes(1);
/// The longest `in` or `every`, which also keeps the arithmetic far from overflow.
pub const MAX_INTERVAL: Duration = Duration::days(366);

pub const USAGE: &str = "a schedule is `at HH:MM`, `at YYYY-MM-DD HH:MM`, `in 20m`, \
`every 2h` (units s m h d w, combinable as 1h30m) or `cron <min> <hour> <day> <month> \
<weekday>`";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Spec {
    /// A wall-clock time, today or tomorrow, or on `date`. Fires once.
    At {
        date: Option<NaiveDate>,
        time: NaiveTime,
    },
    /// A delay. Fires once.
    In(Duration),
    /// An interval. Recurs.
    Every(Duration),
    /// Five fields in local time. Recurs.
    Cron(Cron),
}

impl Spec {
    /// Read a spec off the front of `text` and return it with the rest, trimmed, so
    /// `/remind in 20m check CI` splits into the spec and its prompt.
    pub fn split(text: &str) -> Result<(Spec, &str), String> {
        let mut rest = text.trim_start();
        let mut word = || {
            let w = rest.split_whitespace().next().unwrap_or("");
            rest = rest[w.len()..].trim_start();
            w
        };
        let spec = match word() {
            "at" => {
                let first = word();
                let (date, time) = match first.contains('-') {
                    true => (Some(date(first)?), word()),
                    false => (None, first),
                };
                Spec::At {
                    date,
                    time: clock(time)?,
                }
            }
            "in" => Spec::In(interval(word())?),
            "every" => Spec::Every(interval(word())?),
            "cron" => {
                let fields: [&str; 5] = std::array::from_fn(|_| word());
                Spec::Cron(Cron::parse(&fields.join(" "))?)
            }
            "" => return Err(format!("no schedule given: {USAGE}")),
            other => return Err(format!("`{other}` does not start a schedule: {USAGE}")),
        };
        Ok((spec, rest))
    }

    /// Whether it fires more than once.
    pub fn recurs(&self) -> bool {
        matches!(self, Spec::Every(_) | Spec::Cron(_))
    }

    /// The first fire strictly after `after`, in its time zone; `None` when there is none
    /// (an `at` with a date already past). A one-shot spec is asked once, at the instant
    /// it is set, since `in` and a dateless `at` count from it.
    pub fn next_after<Tz: TimeZone>(&self, after: &DateTime<Tz>) -> Option<DateTime<Tz>> {
        let tz = after.timezone();
        match self {
            Spec::In(d) | Spec::Every(d) => after.clone().checked_add_signed(*d),
            Spec::At {
                date: Some(date),
                time,
            } => resolve(&tz, date.and_time(*time)).filter(|t| t > after),
            Spec::At { date: None, time } => {
                let today = after.naive_local().date();
                [
                    today,
                    today.succ_opt()?,
                    today.checked_add_days(chrono::Days::new(2))?,
                ]
                .into_iter()
                .filter_map(|day| resolve(&tz, day.and_time(*time)))
                .find(|t| t > after)
            }
            Spec::Cron(cron) => cron.next_after(after),
        }
    }
}

impl FromStr for Spec {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match Spec::split(text)? {
            (spec, "") => Ok(spec),
            (_, rest) => Err(format!("`{rest}` follows the schedule: {USAGE}")),
        }
    }
}

impl TryFrom<String> for Spec {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        text.parse()
    }
}

impl From<Spec> for String {
    fn from(spec: Spec) -> String {
        spec.to_string()
    }
}

impl fmt::Display for Spec {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Spec::At { date: None, time } => write!(f, "at {}", time.format("%H:%M")),
            Spec::At {
                date: Some(date),
                time,
            } => write!(f, "at {} {}", date.format("%Y-%m-%d"), time.format("%H:%M")),
            Spec::In(d) => write!(f, "in {}", Span(*d)),
            Spec::Every(d) => write!(f, "every {}", Span(*d)),
            Spec::Cron(cron) => write!(f, "cron {}", cron.text),
        }
    }
}

/// A duration as the shortest `1d2h30m` that says it.
struct Span(Duration);

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut secs = self.0.num_seconds();
        if secs == 0 {
            return f.write_str("0s");
        }
        for (unit, size) in [
            ("w", 604_800),
            ("d", 86_400),
            ("h", 3_600),
            ("m", 60),
            ("s", 1),
        ] {
            if secs >= size {
                write!(f, "{}{unit}", secs / size)?;
                secs %= size;
            }
        }
        Ok(())
    }
}

/// `20m`, `2h`, `1d`, `1h30m`: whole numbers, each with a unit of s, m, h, d or w.
fn interval(text: &str) -> Result<Duration, String> {
    if text.is_empty() {
        return Err(format!("no interval given: {USAGE}"));
    }
    let bad = || format!("`{text}` is not an interval such as 20m, 2h, 1d or 1h30m");
    let mut total = Duration::zero();
    let mut rest = text;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let n: i64 = rest[..digits].parse().map_err(|_| bad())?;
        let unit = match rest.as_bytes().get(digits).map(u8::to_ascii_lowercase) {
            Some(b's') => Duration::seconds(1),
            Some(b'm') => Duration::minutes(1),
            Some(b'h') => Duration::hours(1),
            Some(b'd') => Duration::days(1),
            Some(b'w') => Duration::weeks(1),
            _ => return Err(bad()),
        };
        total = n
            .checked_mul(unit.num_seconds())
            // Duration::seconds panics above i64::MAX / 1000.
            .filter(|s| *s <= MAX_INTERVAL.num_seconds())
            .and_then(|s| total.checked_add(&Duration::seconds(s)))
            .filter(|t| *t <= MAX_INTERVAL)
            .ok_or_else(|| format!("`{text}` is longer than {}", Span(MAX_INTERVAL)))?;
        rest = &rest[digits + 1..];
    }
    if total < MIN_INTERVAL {
        return Err(format!("`{text}` is under the 1 minute minimum"));
    }
    Ok(total)
}

/// `17:30` or `9:05`.
fn clock(text: &str) -> Result<NaiveTime, String> {
    let bad = || format!("`{text}` is not a time such as 17:30");
    let (h, m) = text.split_once(':').ok_or_else(bad)?;
    if !(1..=2).contains(&h.len()) || m.len() != 2 {
        return Err(bad());
    }
    let h = h.parse().map_err(|_| bad())?;
    let m = m.parse().map_err(|_| bad())?;
    NaiveTime::from_hms_opt(h, m, 0).ok_or_else(bad)
}

/// `2026-10-04`.
fn date(text: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map_err(|_| format!("`{text}` is not a date such as 2026-10-04"))
}

/// The instant a local time names: the first pass of a repeated one, and for a skipped
/// one the next minute that exists.
fn resolve<Tz: TimeZone>(tz: &Tz, local: NaiveDateTime) -> Option<DateTime<Tz>> {
    let mut local = local;
    // No zone has a gap longer than a day.
    for _ in 0..=24 * 60 {
        let (a, b) = match tz.from_local_datetime(&local) {
            LocalResult::Single(t) => (Some(t), None),
            LocalResult::Ambiguous(a, b) => (Some(a), Some(b)),
            LocalResult::None => (None, None),
        };
        // chrono 0.4's `Local` hands a repeated time back later first, and reads the
        // minute a change ends on in both offsets (02:00 as -04:00 too when clocks fall
        // back), so only an instant the zone itself puts at that offset counts.
        let first = [a, b]
            .into_iter()
            .flatten()
            .filter(|t| t.offset().fix() == tz.offset_from_utc_datetime(&t.naive_utc()).fix())
            .min();
        if first.is_some() {
            return first;
        }
        local = local.with_second(0)?.with_nanosecond(0)? + Duration::minutes(1);
    }
    None
}

/// Five cron fields as bit sets. Day of month and day of week follow Vixie cron: when
/// both are restricted, a day matching either fires; when either starts with `*`, the
/// other alone decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    /// As written, for display.
    text: String,
    minute: u64,
    hour: u64,
    day: u64,
    month: u64,
    /// Sunday is 0; a written 7 lands there too.
    weekday: u64,
    day_star: bool,
    weekday_star: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// Searched this far ahead before a cron counts as never firing again; a Feb 29 can be
/// eight years off across a century that is not a leap year.
const HORIZON_DAYS: u64 = 366 * 9;

impl Cron {
    pub fn parse(text: &str) -> Result<Self, String> {
        let fields: Vec<&str> = text.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields[..] else {
            return Err(format!(
                "`{text}`: a cron has five fields, minute hour day month weekday"
            ));
        };
        let mut weekday_bits = field(weekday, "weekday", 0, 7, &WEEKDAYS)?;
        if weekday_bits & 1 << 7 != 0 {
            weekday_bits = (weekday_bits & !(1 << 7)) | 1;
        }
        let cron = Cron {
            text: fields.join(" "),
            minute: field(minute, "minute", 0, 59, &[])?,
            hour: field(hour, "hour", 0, 23, &[])?,
            day: field(day, "day", 1, 31, &[])?,
            month: field(month, "month", 1, 12, &MONTHS)?,
            weekday: weekday_bits,
            day_star: day.starts_with('*'),
            weekday_star: weekday.starts_with('*'),
        };
        // Only a day-of-month the months never reach can make a cron dead: a weekday,
        // alone or ORed in, comes round every week.
        let reachable = (1..=12u32)
            .filter(|m| cron.month & 1 << m != 0)
            .any(|m| cron.day.trailing_zeros() <= longest_month(m));
        if cron.weekday_star && !cron.day_star && !reachable {
            return Err(format!(
                "`{text}` never fires: no month it names has that day"
            ));
        }
        Ok(cron)
    }

    fn day_matches(&self, date: NaiveDate) -> bool {
        if self.month & 1 << date.month() == 0 {
            return false;
        }
        let day = self.day & 1 << date.day() != 0;
        let weekday = self.weekday & 1 << date.weekday().num_days_from_sunday() != 0;
        match (self.day_star, self.weekday_star) {
            (false, false) => day || weekday,
            _ => day && weekday,
        }
    }

    fn next_after<Tz: TimeZone>(&self, after: &DateTime<Tz>) -> Option<DateTime<Tz>> {
        let tz = after.timezone();
        let start = after.naive_local();
        let mut date = start.date();
        for _ in 0..HORIZON_DAYS {
            if self.day_matches(date) {
                for h in bits(self.hour) {
                    for m in bits(self.minute) {
                        let local = date.and_hms_opt(h, m, 0)?;
                        if local <= start {
                            continue;
                        }
                        // A slot already fired on the first pass of a repeated hour, or
                        // one a gap moved behind `after`, is passed over.
                        if let Some(t) = resolve(&tz, local).filter(|t| t > after) {
                            return Some(t);
                        }
                    }
                }
            }
            date = date.succ_opt()?;
        }
        None
    }
}

fn longest_month(month: u32) -> u32 {
    match month {
        2 => 29,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn bits(set: u64) -> impl Iterator<Item = u32> {
    (0..64).filter(move |i| set & 1 << i != 0)
}

/// One cron field: a comma list of `*`, `n`, `a-b`, each with an optional `/step`, where
/// `n/step` runs from n to `max`. `names` stand for `min`, `min + 1`, and so on.
fn field(text: &str, what: &str, min: u32, max: u32, names: &[&str]) -> Result<u64, String> {
    let bad = |why: String| format!("cron {what} `{text}`: {why}");
    let value = |v: &str| -> Result<u32, String> {
        let lower = v.to_ascii_lowercase();
        let n = match names.iter().position(|name| *name == lower) {
            Some(i) => min + i as u32,
            None => v
                .parse()
                .map_err(|_| bad(format!("`{v}` is not a number")))?,
        };
        match (min..=max).contains(&n) {
            true => Ok(n),
            false => Err(bad(format!("{n} is outside {min}-{max}"))),
        }
    };
    let mut set = 0u64;
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => match step.parse::<u32>() {
                Ok(s) if s > 0 => (range, Some(s)),
                _ => return Err(bad(format!("`{step}` is not a step"))),
            },
            None => (item, None),
        };
        let (lo, hi) = match range.split_once('-') {
            _ if range == "*" => (min, max),
            Some((lo, hi)) => (value(lo)?, value(hi)?),
            None if step.is_some() => (value(range)?, max),
            None => {
                let n = value(range)?;
                (n, n)
            }
        };
        if lo > hi {
            return Err(bad(format!("{lo}-{hi} runs backwards")));
        }
        for n in (lo..=hi).step_by(step.unwrap_or(1) as usize) {
            set |= 1 << n;
        }
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, Utc};

    /// US Eastern for 2026 alone: clocks go 02:00 to 03:00 on March 8 and 02:00 back to
    /// 01:00 on November 1, so DST edges are testable without the machine's zone.
    #[derive(Debug, Clone, Copy)]
    struct Eastern;

    #[derive(Debug, Clone, Copy)]
    struct EasternOffset(FixedOffset);

    impl Offset for EasternOffset {
        fn fix(&self) -> FixedOffset {
            self.0
        }
    }

    impl fmt::Display for EasternOffset {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            self.0.fmt(f)
        }
    }

    const EST: i32 = -5 * 3600;
    const EDT: i32 = -4 * 3600;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    fn offset(secs: i32) -> EasternOffset {
        EasternOffset(FixedOffset::east_opt(secs).unwrap())
    }

    impl TimeZone for Eastern {
        type Offset = EasternOffset;

        fn from_offset(_: &EasternOffset) -> Self {
            Eastern
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> LocalResult<EasternOffset> {
            self.offset_from_local_datetime(&local.and_hms_opt(12, 0, 0).unwrap())
        }

        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<EasternOffset> {
            let spring = utc(2026, 3, 8, 2, 0);
            let fall = utc(2026, 11, 1, 1, 0);
            if *local >= spring && *local < spring + Duration::hours(1) {
                LocalResult::None
            } else if *local >= fall && *local < fall + Duration::hours(1) {
                LocalResult::Ambiguous(offset(EDT), offset(EST))
            } else if *local >= spring && *local < fall {
                LocalResult::Single(offset(EDT))
            } else {
                LocalResult::Single(offset(EST))
            }
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> EasternOffset {
            self.offset_from_utc_datetime(&utc.and_hms_opt(12, 0, 0).unwrap())
        }

        fn offset_from_utc_datetime(&self, at: &NaiveDateTime) -> EasternOffset {
            // 2026-03-08 07:00 UTC and 2026-11-01 06:00 UTC.
            match *at >= utc(2026, 3, 8, 7, 0) && *at < utc(2026, 11, 1, 6, 0) {
                true => offset(EDT),
                false => offset(EST),
            }
        }
    }

    /// Eastern read the way chrono 0.4's `Local` reads it: a repeated time's two
    /// instants come back later first, and the minute each change ends on is read in
    /// the old offset as well.
    #[derive(Debug, Clone, Copy)]
    struct EasternLikeLocal;

    impl TimeZone for EasternLikeLocal {
        type Offset = EasternOffset;

        fn from_offset(_: &EasternOffset) -> Self {
            EasternLikeLocal
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> LocalResult<EasternOffset> {
            Eastern.offset_from_local_date(local)
        }

        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<EasternOffset> {
            if *local == utc(2026, 3, 8, 2, 0) {
                return LocalResult::Single(offset(EST));
            }
            if *local == utc(2026, 11, 1, 2, 0) {
                return LocalResult::Ambiguous(offset(EST), offset(EDT));
            }
            match Eastern.offset_from_local_datetime(local) {
                LocalResult::Ambiguous(earlier, later) => LocalResult::Ambiguous(later, earlier),
                other => other,
            }
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> EasternOffset {
            Eastern.offset_from_utc_date(utc)
        }

        fn offset_from_utc_datetime(&self, at: &NaiveDateTime) -> EasternOffset {
            Eastern.offset_from_utc_datetime(at)
        }
    }

    /// A local time in Eastern that is not ambiguous or skipped.
    fn et(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Eastern> {
        Eastern
            .from_local_datetime(&utc(y, mo, d, h, mi))
            .single()
            .unwrap()
    }

    fn spec(text: &str) -> Spec {
        text.parse().unwrap()
    }

    fn next<Tz: TimeZone>(text: &str, after: DateTime<Tz>) -> Option<DateTime<Tz>> {
        spec(text).next_after(&after)
    }

    /// The first `n` fires of a recurring spec after `after`.
    fn fires<Tz: TimeZone>(text: &str, after: DateTime<Tz>, n: usize) -> Vec<DateTime<Tz>> {
        let spec = spec(text);
        std::iter::successors(spec.next_after(&after), |t| spec.next_after(t))
            .take(n)
            .collect()
    }

    #[test]
    fn parses_each_form_and_prints_it_back() {
        for (text, shown) in [
            ("at 17:30", "at 17:30"),
            ("at 9:05", "at 09:05"),
            ("at 2026-10-04 09:00", "at 2026-10-04 09:00"),
            ("in 20m", "in 20m"),
            ("in 2h", "in 2h"),
            ("in 1d", "in 1d"),
            ("in 90m", "in 1h30m"),
            ("every 15m", "every 15m"),
            ("every 1h30m", "every 1h30m"),
            ("every 60s", "every 1m"),
            ("cron */5 9-17 * * MON-fri", "cron */5 9-17 * * MON-fri"),
            ("  cron  0   0 1  jan,jul  * ", "cron 0 0 1 jan,jul *"),
        ] {
            let parsed = spec(text);
            assert_eq!(parsed.to_string(), shown, "{text}");
            assert_eq!(spec(shown), parsed, "{shown}");
        }
        assert!(spec("every 1m").recurs() && spec("cron * * * * *").recurs());
        assert!(!spec("in 1m").recurs() && !spec("at 10:00").recurs());
    }

    #[test]
    fn rejects_what_does_not_parse() {
        for text in [
            "",
            "soon",
            "at",
            "at 25:00",
            "at 17:60",
            "at 1730",
            "at 17:3",
            "at 2026-02-30 10:00",
            "at 2026-10-04",
            "in",
            "in 20",
            "in m",
            "in 20x",
            "in -5m",
            "in 59s",
            "every 30s",
            "every 0m",
            "in 367d",
            "in 99999999999999999w",
            "in 10000000000000000s",
            "in 100000000000000h",
            "every 1h99999999999999999s",
            "cron * * * *",
            "cron 60 * * * *",
            "cron * 24 * * *",
            "cron * * 0 * *",
            "cron * * * 13 *",
            "cron * * * * 8",
            "cron */0 * * * *",
            "cron 5-1 * * * *",
            "cron * * * foo *",
            "cron 0 0 31 2 *",
            "cron 0 0 30,31 feb *",
            "cron 0 0 31 4,6,9,11 *",
        ] {
            assert!(text.parse::<Spec>().is_err(), "{text} parsed");
        }
        assert_eq!(
            "in 20m extra".parse::<Spec>(),
            Err(format!("`extra` follows the schedule: {USAGE}"))
        );
    }

    #[test]
    fn split_hands_back_the_prompt() {
        let (s, rest) = Spec::split("in 20m  check CI on main").unwrap();
        assert_eq!(
            (s.to_string().as_str(), rest),
            ("in 20m", "check CI on main")
        );
        let (s, rest) = Spec::split("at 2026-10-04 09:00 standup").unwrap();
        assert_eq!(
            (s.to_string().as_str(), rest),
            ("at 2026-10-04 09:00", "standup")
        );
        let (s, rest) = Spec::split("cron 0 9 * * 1 weekly review").unwrap();
        assert_eq!(
            (s.to_string().as_str(), rest),
            ("cron 0 9 * * 1", "weekly review")
        );
        let (_, rest) = Spec::split("every 1h").unwrap();
        assert_eq!(rest, "");
    }

    #[test]
    fn survives_a_serde_round_trip() {
        let s = spec("cron 0 9 * * mon");
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"cron 0 9 * * mon\"");
        assert_eq!(serde_json::from_str::<Spec>(&json).unwrap(), s);
        assert!(serde_json::from_str::<Spec>("\"cron 0 0 31 2 *\"").is_err());
    }

    #[test]
    fn at_is_today_or_tomorrow() {
        let noon = et(2026, 10, 3, 12, 0);
        assert_eq!(next("at 17:30", noon), Some(et(2026, 10, 3, 17, 30)));
        assert_eq!(next("at 09:00", noon), Some(et(2026, 10, 4, 9, 0)));
        // Exactly now is past: it does not fire twice in one instant.
        assert_eq!(next("at 12:00", noon), Some(et(2026, 10, 4, 12, 0)));
        assert_eq!(
            next("at 00:00", et(2026, 12, 31, 23, 59)),
            Some(et(2027, 1, 1, 0, 0))
        );
    }

    #[test]
    fn at_a_date_fires_only_if_ahead() {
        let noon = et(2026, 10, 3, 12, 0);
        assert_eq!(
            next("at 2026-10-04 09:00", noon),
            Some(et(2026, 10, 4, 9, 0))
        );
        assert_eq!(next("at 2026-10-03 11:59", noon), None);
        assert_eq!(next("at 2026-10-03 12:00", noon), None);
    }

    #[test]
    fn in_and_every_count_elapsed_time() {
        let noon = et(2026, 10, 3, 12, 0);
        assert_eq!(next("in 20m", noon), Some(et(2026, 10, 3, 12, 20)));
        assert_eq!(next("in 1d", noon), Some(et(2026, 10, 4, 12, 0)));
        assert_eq!(
            fires("every 15m", noon, 3),
            [
                et(2026, 10, 3, 12, 15),
                et(2026, 10, 3, 12, 30),
                et(2026, 10, 3, 12, 45)
            ]
        );
        // A day of elapsed time across the fall back lands an hour earlier on the clock.
        assert_eq!(
            next("in 1d", et(2026, 10, 31, 12, 0)),
            Some(et(2026, 11, 1, 11, 0))
        );
        let before = et(2026, 3, 8, 1, 30);
        assert_eq!(next("in 1h", before), Some(et(2026, 3, 8, 3, 30)));
    }

    #[test]
    fn cron_fields() {
        let start = et(2026, 10, 3, 8, 58); // a Saturday
        assert_eq!(
            fires("cron */5 9-17 * * mon-fri", start, 3),
            [
                et(2026, 10, 5, 9, 0),
                et(2026, 10, 5, 9, 5),
                et(2026, 10, 5, 9, 10)
            ]
        );
        assert_eq!(
            fires("cron 30 8,12 * * *", start, 3),
            [
                et(2026, 10, 3, 12, 30),
                et(2026, 10, 4, 8, 30),
                et(2026, 10, 4, 12, 30)
            ]
        );
        // 7 is Sunday as well as 0.
        assert_eq!(next("cron 0 0 * * 7", start), Some(et(2026, 10, 4, 0, 0)));
        assert_eq!(next("cron 0 0 * * SUN", start), Some(et(2026, 10, 4, 0, 0)));
        // `n/step` runs to the top of the range.
        assert_eq!(
            fires("cron 50/5 9 * * *", start, 3),
            [
                et(2026, 10, 3, 9, 50),
                et(2026, 10, 3, 9, 55),
                et(2026, 10, 4, 9, 50)
            ]
        );
        assert_eq!(
            fires("cron 0 0 1 jan,jul *", start, 2),
            [et(2027, 1, 1, 0, 0), et(2027, 7, 1, 0, 0)]
        );
        // Both day fields restricted: either one fires.
        assert_eq!(
            fires("cron 0 0 13 * fri", start, 3),
            [
                et(2026, 10, 9, 0, 0),
                et(2026, 10, 13, 0, 0),
                et(2026, 10, 16, 0, 0)
            ]
        );
        // A starred day of month leaves the weekday alone to decide.
        assert_eq!(
            next("cron 0 0 */2 * fri", start),
            Some(et(2026, 10, 9, 0, 0))
        );
        // The current minute has started, so it is not the next fire.
        assert_eq!(
            next("cron * * * * *", et(2026, 10, 3, 8, 58)),
            Some(et(2026, 10, 3, 8, 59))
        );
    }

    #[test]
    fn cron_month_ends() {
        let start = et(2026, 1, 1, 0, 0);
        assert_eq!(
            fires("cron 0 0 31 * *", start, 4),
            [
                et(2026, 1, 31, 0, 0),
                et(2026, 3, 31, 0, 0),
                et(2026, 5, 31, 0, 0),
                et(2026, 7, 31, 0, 0)
            ]
        );
        assert_eq!(
            fires("cron 0 0 30 * *", start, 2),
            [et(2026, 1, 30, 0, 0), et(2026, 3, 30, 0, 0)]
        );
        // Feb 29 waits for a leap year.
        let leap = Utc.from_utc_datetime(&utc(2028, 2, 29, 0, 0));
        let found = spec("cron 0 0 29 2 *")
            .next_after(&Utc.from_utc_datetime(&utc(2026, 1, 1, 0, 0)))
            .unwrap();
        assert_eq!(found, leap);
        // And across 2100, which is not one.
        let after_2096 = spec("cron 0 0 29 feb *")
            .next_after(&Utc.from_utc_datetime(&utc(2096, 3, 1, 0, 0)))
            .unwrap();
        assert_eq!(after_2096, Utc.from_utc_datetime(&utc(2104, 2, 29, 0, 0)));
        // Day 31 ORed with a weekday is not dead.
        assert!("cron 0 0 31 2 mon".parse::<Spec>().is_ok());
    }

    #[test]
    fn a_skipped_local_time_fires_at_the_next_valid_minute() {
        skipped_local_time_fires_at_the_next_valid_minute_in(Eastern);
        skipped_local_time_fires_at_the_next_valid_minute_in(EasternLikeLocal);
    }

    fn skipped_local_time_fires_at_the_next_valid_minute_in<Tz: TimeZone>(tz: Tz) {
        let local = |d, h, mi| tz.from_local_datetime(&utc(2026, 3, d, h, mi)).unwrap();
        let at_utc = |d, h, mi| {
            Utc.from_utc_datetime(&utc(2026, 3, d, h, mi))
                .with_timezone(&tz)
        };
        let before = local(7, 12, 0);
        // 02:30 does not exist on March 8; 03:00 EDT is the next minute that does.
        assert_eq!(next("at 02:30", before.clone()), Some(at_utc(8, 7, 0)));
        assert_eq!(next("at 02:00", before.clone()), Some(at_utc(8, 7, 0)));
        assert_eq!(next("at 2026-03-08 02:30", before), Some(at_utc(8, 7, 0)));
        // Every skipped slot collapses into that one fire, then the schedule resumes.
        assert_eq!(
            fires("cron */20 2,3 * * *", local(8, 1, 50), 4),
            [
                at_utc(8, 7, 0),
                at_utc(8, 7, 20),
                at_utc(8, 7, 40),
                at_utc(9, 6, 0),
            ]
        );
        assert_eq!(local(8, 3, 0), at_utc(8, 7, 0));
    }

    #[test]
    fn a_repeated_local_time_fires_once() {
        repeated_local_time_fires_once_in(Eastern);
        repeated_local_time_fires_once_in(EasternLikeLocal);
    }

    fn repeated_local_time_fires_once_in<Tz: TimeZone>(tz: Tz) {
        let local = |mo, d, h, mi| tz.from_local_datetime(&utc(2026, mo, d, h, mi)).unwrap();
        let at_utc = |d, h, mi| {
            Utc.from_utc_datetime(&utc(2026, 11, d, h, mi))
                .with_timezone(&tz)
        };
        let before = local(10, 31, 12, 0);
        // 01:30 happens at 05:30 UTC (EDT) and again at 06:30 UTC (EST): only the first.
        assert_eq!(next("at 01:30", before.clone()), Some(at_utc(1, 5, 30)));
        // 02:00 happens once, at 07:00 UTC.
        assert_eq!(next("at 02:00", before.clone()), Some(at_utc(1, 7, 0)));
        assert_eq!(next("at 2026-11-01 01:30", before), Some(at_utc(1, 5, 30)));
        // Asked from the first pass, the next minute is a minute away, not an hour.
        assert_eq!(next("at 01:30", at_utc(1, 5, 10)), Some(at_utc(1, 5, 30)));
        assert_eq!(
            next("cron * * * * *", at_utc(1, 5, 10)),
            Some(at_utc(1, 5, 11))
        );
        assert_eq!(
            fires("cron 0,30 1,2 * * *", local(11, 1, 0, 45), 5),
            [
                at_utc(1, 5, 0),
                at_utc(1, 5, 30),
                at_utc(1, 7, 0),
                at_utc(1, 7, 30),
                at_utc(2, 6, 0),
            ]
        );
        // Asked from inside the second pass, a slot from the first is not fired again.
        let second_pass = at_utc(1, 6, 10);
        assert_eq!(
            next("cron 15 1 * * *", second_pass.clone()),
            Some(local(11, 2, 1, 15))
        );
        assert_eq!(next("at 01:15", second_pass), Some(local(11, 2, 1, 15)));
        // An elapsed interval walks through both passes.
        assert_eq!(
            fires("every 30m", at_utc(1, 5, 0), 3),
            [at_utc(1, 5, 30), at_utc(1, 6, 0), at_utc(1, 6, 30)]
        );
    }
}
