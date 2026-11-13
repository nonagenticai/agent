//! Business hours (F-14, B4 T16a): when a job's people are available. The `hours` of the job's `handover_policy`
//! value (0069; `null` = always staffed), read like the targets (crate::handover::policy: live for a prod item, the
//! draft for a test item).
//!
//! ```json
//! {"timezone": "Europe/Amsterdam",
//!  "weekly": {"mon": [{"from": "09:00", "to": "17:30"}], "tue": […], …, "sun": []},
//!  "use_tenant_calendar": true,
//!  "out_of_hours": "callback"}
//! ```
//!
//! - **Open / closed** ([`is_open`]) is decided on the wall clock of the policy's own timezone, never the database
//!   session's: [`openness`] converts with `AT TIME ZONE <policy timezone>` both ways. A window is `[from, to)`;
//!   `to` may be `24:00`. A holiday of the tenant calendar (crate::handover::calendar) is closed all day (the date in
//!   the policy's timezone) when `use_tenant_calendar`.
//! - **Out of hours** ([`closed_now`]): `case.create{handover|complaint}` queues the item `outside_hours`, its SLA
//!   clocks start at the next open time (`clock_starts_at`), and the reply says when someone will answer
//!   ([`out_of_hours_text`]): a call-back (`callback`) or a reply (`queue`).
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use super::calendar::Calendar;
use crate::error::ApiResult;

/// The days of a week, Monday first (`weekly` keys).
pub const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
/// The most windows one day holds.
pub const MAX_WINDOWS: usize = 6;
/// How far ahead [`is_open`] looks for the next open time (more than a year of holidays).
pub const LOOKAHEAD_DAYS: u64 = 400;
/// Minutes in a day (`24:00`).
const DAY_MIN: u16 = 1440;

/// One open window of a day, in minutes from local midnight: `[from, to)`, `to` ≤ 1440.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub from: u16,
    pub to: u16,
}

/// What a closed scope tells the customer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutOfHours {
    /// A colleague calls the customer back after the next open time.
    Callback,
    /// A colleague replies after the next open time.
    Queue,
}

impl OutOfHours {
    pub fn as_str(self) -> &'static str {
        match self {
            OutOfHours::Callback => "callback",
            OutOfHours::Queue => "queue",
        }
    }
}

/// A job's business hours.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hours {
    pub timezone: String,
    /// Monday first ([`DAYS`]), each day's windows sorted, never overlapping.
    pub weekly: [Vec<Window>; 7],
    pub use_tenant_calendar: bool,
    pub out_of_hours: OutOfHours,
}

fn hhmm(m: u16) -> String {
    format!("{:02}:{:02}", m / 60, m % 60)
}

/// `HH:MM` (00:00 to 24:00) as minutes from midnight.
fn parse_hhmm(s: &str) -> Option<u16> {
    let b = s.as_bytes();
    if b.len() != 5
        || b[2] != b':'
        || !b
            .iter()
            .enumerate()
            .all(|(i, c)| i == 2 || c.is_ascii_digit())
    {
        return None;
    }
    let h: u16 = s[..2].parse().ok()?;
    let m: u16 = s[3..].parse().ok()?;
    let total = h * 60 + m;
    (m < 60 && total <= DAY_MIN).then_some(total)
}

impl Hours {
    /// The windows of `date`'s weekday.
    pub fn day(&self, date: NaiveDate) -> &[Window] {
        &self.weekly[date.weekday().num_days_from_monday() as usize]
    }

    /// The stored value (every day present).
    pub fn to_value(&self) -> Value {
        let mut weekly = Map::new();
        for (i, d) in DAYS.into_iter().enumerate() {
            let ws: Vec<Value> = self.weekly[i]
                .iter()
                .map(|w| json!({"from": hhmm(w.from), "to": hhmm(w.to)}))
                .collect();
            weekly.insert(d.to_string(), Value::Array(ws));
        }
        json!({
            "timezone": self.timezone,
            "weekly": weekly,
            "use_tenant_calendar": self.use_tenant_calendar,
            "out_of_hours": self.out_of_hours.as_str(),
        })
    }
}

/// Why `v` cannot be saved as a policy's `hours` (`Err(message)`, each prefixed `hours:`), else the hours. The
/// timezone's existence is checked against the database ([`known_timezone`]).
pub fn validate(v: &Value) -> Result<Hours, String> {
    let Some(o) = v.as_object() else {
        return Err("hours: must be an object or null".into());
    };
    let timezone = match o.get("timezone").and_then(Value::as_str).map(str::trim) {
        Some(t) if !t.is_empty() && t.chars().count() <= super::calendar::MAX_TIMEZONE => {
            t.to_string()
        }
        _ => return Err("hours: timezone is required".into()),
    };
    let Some(weekly) = o.get("weekly").and_then(Value::as_object) else {
        return Err("hours: weekly must be an object of days".into());
    };
    if let Some(k) = weekly.keys().find(|k| !DAYS.contains(&k.as_str())) {
        return Err(format!("hours: unknown day {k}"));
    }
    let mut days: [Vec<Window>; 7] = Default::default();
    for (i, d) in DAYS.into_iter().enumerate() {
        let rows = match weekly.get(d) {
            None | Some(Value::Null) => continue,
            Some(Value::Array(a)) => a,
            Some(_) => return Err(format!("hours: {d} must be a list of windows")),
        };
        if rows.len() > MAX_WINDOWS {
            return Err(format!("hours: {d} has more than {MAX_WINDOWS} windows"));
        }
        let mut ws = Vec::with_capacity(rows.len());
        for w in rows {
            let time = |k: &str| w.get(k).and_then(Value::as_str).and_then(parse_hhmm);
            match (time("from"), time("to")) {
                (Some(from), Some(to)) if from < to && from < DAY_MIN => {
                    ws.push(Window { from, to })
                }
                _ => {
                    return Err(format!(
                        "hours: {d} windows need from before to, HH:MM between 00:00 and 24:00"
                    ));
                }
            }
        }
        ws.sort_by_key(|w| w.from);
        if ws.windows(2).any(|p| p[1].from < p[0].to) {
            return Err(format!("hours: {d} windows overlap"));
        }
        days[i] = ws;
    }
    if days.iter().all(Vec::is_empty) {
        return Err("hours: no open window in the week".into());
    }
    let use_tenant_calendar = match o.get("use_tenant_calendar") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err("hours: use_tenant_calendar must be true or false".into()),
    };
    let out_of_hours = match o.get("out_of_hours").and_then(Value::as_str) {
        None => OutOfHours::Queue,
        Some("queue") => OutOfHours::Queue,
        Some("callback") => OutOfHours::Callback,
        Some(_) => return Err("hours: out_of_hours must be callback or queue".into()),
    };
    Ok(Hours {
        timezone,
        weekly: days,
        use_tenant_calendar,
        out_of_hours,
    })
}

/// A stored `hours` value as [`Hours`] (`None` = `null`, absent, or a value that no longer validates).
pub fn of_value(v: Option<&Value>) -> Option<Hours> {
    v.filter(|h| !h.is_null()).and_then(|h| validate(h).ok())
}

/// Whether the scope is open at a local wall-clock time, and when it opens next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Openness {
    pub open: bool,
    /// Closed: the next open time (local wall clock of the policy's timezone); `None` when open (or never opens).
    pub next_open: Option<NaiveDateTime>,
}

const OPEN: Openness = Openness {
    open: true,
    next_open: None,
};

fn time_of(m: u16) -> NaiveTime {
    NaiveTime::from_hms_opt(u32::from(m / 60), u32::from(m % 60), 0).unwrap_or_default()
}

/// Open or closed at `at` (the local wall clock of `hours.timezone`). `hours = None` = always open. A holiday of
/// `calendar` is closed all day when `hours.use_tenant_calendar`.
pub fn is_open(hours: Option<&Hours>, calendar: Option<&Calendar>, at: NaiveDateTime) -> Openness {
    let Some(h) = hours else {
        return OPEN;
    };
    let holiday = |d: NaiveDate| h.use_tenant_calendar && calendar.is_some_and(|c| c.closed_on(d));
    let today = at.date();
    let now = (at.time().num_seconds_from_midnight() / 60) as u16;
    if !holiday(today) && h.day(today).iter().any(|w| w.from <= now && now < w.to) {
        return OPEN;
    }
    for i in 0..=LOOKAHEAD_DAYS {
        let Some(d) = today.checked_add_days(chrono::Days::new(i)) else {
            break;
        };
        if holiday(d) {
            continue;
        }
        if let Some(w) = h.day(d).iter().find(|w| i > 0 || w.from > now) {
            return Openness {
                open: false,
                next_open: Some(d.and_time(time_of(w.from))),
            };
        }
    }
    Openness {
        open: false,
        next_open: None,
    }
}

/// `tz` names a timezone Postgres knows (`pg_timezone_names`).
pub async fn known_timezone(c: &mut PgConnection, tz: &str) -> ApiResult<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM pg_timezone_names WHERE name = $1)",
    )
    .bind(tz.to_owned())
    .fetch_one(&mut *c)
    .await?)
}

/// `at` on the wall clock of `tz` (explicit zone, never the session's).
pub async fn local_time(
    c: &mut PgConnection,
    tz: &str,
    at: DateTime<Utc>,
) -> ApiResult<NaiveDateTime> {
    Ok(
        sqlx::query_scalar::<_, NaiveDateTime>("SELECT ($1::timestamptz AT TIME ZONE $2::text)")
            .bind(at)
            .bind(tz.to_owned())
            .fetch_one(&mut *c)
            .await?,
    )
}

/// The instant the wall-clock time `local` of `tz` names (a time skipped by a DST change moves forward, as Postgres
/// does).
pub async fn instant_of(
    c: &mut PgConnection,
    tz: &str,
    local: NaiveDateTime,
) -> ApiResult<DateTime<Utc>> {
    Ok(
        sqlx::query_scalar::<_, DateTime<Utc>>("SELECT ($1::timestamp AT TIME ZONE $2::text)")
            .bind(local)
            .bind(tz.to_owned())
            .fetch_one(&mut *c)
            .await?,
    )
}

/// A scope found closed: when it opens and what it offers meanwhile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Closed {
    pub next_open: DateTime<Utc>,
    /// The next open time on the policy's wall clock, for people: `Mon 26 Oct 09:00 (Europe/Amsterdam)`.
    pub next_open_local: String,
    pub out_of_hours: OutOfHours,
}

/// `local` for people, in `tz`: `Mon 26 Oct 09:00 (Europe/Amsterdam)`.
pub fn local_text(local: NaiveDateTime, tz: &str) -> String {
    format!("{} ({tz})", local.format("%a %-d %b %H:%M"))
}

/// The scope at instant `now`: `None` = open (or no hours, or never opens again), else [`Closed`].
pub async fn closed_now(
    c: &mut PgConnection,
    hours: Option<&Hours>,
    calendar: Option<&Calendar>,
    now: DateTime<Utc>,
) -> ApiResult<Option<Closed>> {
    let Some(h) = hours else {
        return Ok(None);
    };
    let local = local_time(&mut *c, &h.timezone, now).await?;
    let o = is_open(Some(h), calendar, local);
    let (false, Some(next)) = (o.open, o.next_open) else {
        return Ok(None);
    };
    Ok(Some(Closed {
        next_open: instant_of(&mut *c, &h.timezone, next).await?,
        next_open_local: local_text(next, &h.timezone),
        out_of_hours: h.out_of_hours,
    }))
}

/// What the customer reads when their handover was queued out of hours (`when` = [`Closed::next_open_local`]).
pub fn out_of_hours_text(mode: OutOfHours, when: &str) -> String {
    match mode {
        OutOfHours::Callback => {
            format!("We're closed right now. A colleague will call you back after {when}.")
        }
        OutOfHours::Queue => {
            format!("We're closed right now; a colleague will reply after {when}.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    fn office() -> Hours {
        validate(&json!({
            "timezone": "Europe/Amsterdam",
            "weekly": {"mon": [{"from": "09:00", "to": "12:00"}, {"from": "13:00", "to": "17:00"}],
                       "tue": [{"from": "09:00", "to": "17:00"}], "fri": [{"from": "20:00", "to": "24:00"}]},
            "use_tenant_calendar": true,
            "out_of_hours": "callback",
        }))
        .unwrap()
    }

    #[test]
    fn open_closed_and_next_open() {
        let h = office();
        // 2026-10-19 is a Monday
        assert_eq!(is_open(Some(&h), None, at("2026-10-19 09:00")), OPEN);
        assert_eq!(is_open(Some(&h), None, at("2026-10-19 16:59")), OPEN);
        assert_eq!(
            is_open(Some(&h), None, at("2026-10-19 12:30")).next_open,
            Some(at("2026-10-19 13:00")),
            "the next window of the same day"
        );
        assert_eq!(
            is_open(Some(&h), None, at("2026-10-19 17:00")).next_open,
            Some(at("2026-10-20 09:00")),
            "a window ends exclusive"
        );
        assert_eq!(
            is_open(Some(&h), None, at("2026-10-20 18:00")).next_open,
            Some(at("2026-10-23 20:00")),
            "Wednesday and Thursday are closed"
        );
        assert_eq!(is_open(Some(&h), None, at("2026-10-23 23:59")), OPEN);
        assert_eq!(is_open(None, None, at("2026-10-24 03:00")), OPEN);
    }

    #[test]
    fn holiday_is_closed_all_day_only_with_the_calendar() {
        let cal = super::super::calendar::validate(
            &json!({"timezone": "Europe/Amsterdam", "holidays": [{"date": "2026-10-20"}]}),
        )
        .unwrap();
        let mut h = office();
        let o = is_open(Some(&h), Some(&cal), at("2026-10-20 10:00"));
        assert!(!o.open);
        assert_eq!(o.next_open, Some(at("2026-10-23 20:00")));
        h.use_tenant_calendar = false;
        assert_eq!(is_open(Some(&h), Some(&cal), at("2026-10-20 10:00")), OPEN);
    }

    #[test]
    fn validate_follows_the_shape() {
        let h = office();
        assert_eq!(validate(&h.to_value()), Ok(h.clone()));
        assert_eq!(h.to_value()["weekly"]["fri"][0]["to"], "24:00");
        assert_eq!(of_value(Some(&Value::Null)), None);
        for (v, msg) in [
            (json!("9-5"), "hours: must be an object or null"),
            (json!({"weekly": {}}), "hours: timezone is required"),
            (
                json!({"timezone": "UTC"}),
                "hours: weekly must be an object of days",
            ),
            (
                json!({"timezone": "UTC", "weekly": {"monday": []}}),
                "hours: unknown day monday",
            ),
            (
                json!({"timezone": "UTC", "weekly": {"mon": []}}),
                "hours: no open window in the week",
            ),
            (
                json!({"timezone": "UTC", "weekly": {"mon": [{"from": "17:00", "to": "09:00"}]}}),
                "hours: mon windows need from before to, HH:MM between 00:00 and 24:00",
            ),
            (
                json!({"timezone": "UTC", "weekly": {"mon": [{"from": "9:00", "to": "17:00"}]}}),
                "hours: mon windows need from before to, HH:MM between 00:00 and 24:00",
            ),
            (
                json!({"timezone": "UTC", "weekly": {"tue": [{"from": "09:00", "to": "12:00"},
                                                             {"from": "11:00", "to": "13:00"}]}}),
                "hours: tue windows overlap",
            ),
            (
                json!({"timezone": "UTC", "weekly": {"mon": [{"from": "09:00", "to": "17:00"}]},
                       "out_of_hours": "voicemail"}),
                "hours: out_of_hours must be callback or queue",
            ),
        ] {
            assert_eq!(validate(&v), Err(msg.to_string()), "{v}");
        }
    }

    #[test]
    fn texts_name_the_local_time() {
        let when = local_text(at("2026-10-26 09:00"), "Europe/Amsterdam");
        assert_eq!(when, "Mon 26 Oct 09:00 (Europe/Amsterdam)");
        assert_eq!(
            out_of_hours_text(OutOfHours::Callback, &when),
            "We're closed right now. A colleague will call you back after Mon 26 Oct 09:00 (Europe/Amsterdam)."
        );
        assert_eq!(
            out_of_hours_text(OutOfHours::Queue, &when),
            "We're closed right now; a colleague will reply after Mon 26 Oct 09:00 (Europe/Amsterdam)."
        );
    }
}
