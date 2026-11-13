//! The tenant holiday calendar (F-14, B4 T16a): B2 ledger kind `tenant_calendar`, one place per tenant (ref `{}`,
//! key `tenant-calendar`), working row `tenant_calendar` (0066). A tenant-scope kind (D69, like `llm_use`): one draft
//! row for every job of the tenant, recorded in each job's draft by `design::track_tenant`; each job's Production
//! gets it when that job publishes. One line in `design::registry()` ([`register`]).
//!
//! | kind | key | working rows | places |
//! |---|---|---|---|
//! | `tenant_calendar` | `tenant-calendar` | `tenant_calendar` | `{}` when the tenant has a row |
//!
//! The value is `{timezone, holidays: [{date, label, closed: true}]}` ([`Calendar`]). A holiday is closed all day for
//! a job whose business hours say `use_tenant_calendar` (crate::handover::hours). `saveTenantCalendar` writes the row
//! only inside the `mutate` closure of `design::track_tenant` through [`write`] (tests/design_antidrift.rs: this
//! module is the kind's only other writer), so a save is a draft and never live (D54).
use std::sync::Arc;

use chrono::NaiveDate;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::design::{self, DesignOf, DesignTarget, DraftRef, JobRef, Place, Registry, Scope, Snap};
use crate::error::{ApiError, ApiResult};
use crate::release::targets::SET_BY;

/// The ledger kind.
pub const KIND: &str = "tenant_calendar";
/// The place key (fixture `TARGETS.tenant_calendar.key`).
pub const KEY: &str = "tenant-calendar";
/// The most holidays one calendar holds (a year of days).
pub const MAX_HOLIDAYS: usize = 366;
/// The longest holiday label, in characters.
pub const MAX_LABEL: usize = 80;
/// The longest timezone name, in characters.
pub const MAX_TIMEZONE: usize = 64;

/// Register the `tenant_calendar` target (one line in `design::registry()`).
pub fn register(r: &mut Registry) {
    r.register(Arc::new(TenantCalendarTarget));
}

/// One closed day.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holiday {
    pub date: NaiveDate,
    pub label: String,
}

/// A tenant calendar: the timezone people name it in and the closed days, sorted by date, one per date.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Calendar {
    pub timezone: String,
    pub holidays: Vec<Holiday>,
}

impl Calendar {
    /// `date` is a holiday.
    pub fn closed_on(&self, date: NaiveDate) -> bool {
        self.holidays
            .binary_search_by_key(&date, |h| h.date)
            .is_ok()
    }

    /// The stored value (`{timezone, holidays}`).
    pub fn to_value(&self) -> Value {
        json!({
            "timezone": self.timezone,
            "holidays": self.holidays.iter().map(|h| json!({
                "date": h.date.format("%Y-%m-%d").to_string(),
                "label": h.label,
                "closed": true,
            })).collect::<Vec<_>>(),
        })
    }
}

/// A `YYYY-MM-DD` date (exactly that form).
pub fn parse_date(s: &str) -> Option<NaiveDate> {
    let b = s.as_bytes();
    let shape = b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    if !shape {
        return None;
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// Why `v` (`{timezone, holidays}`) cannot be saved (`Err(message)`), else the calendar with its holidays sorted.
/// The timezone's existence is checked against the database ([`super::hours::known_timezone`]).
pub fn validate(v: &Value) -> Result<Calendar, String> {
    let Some(o) = v.as_object() else {
        return Err("the calendar must be an object".into());
    };
    let timezone = match o.get("timezone").and_then(Value::as_str).map(str::trim) {
        Some(t) if !t.is_empty() && t.chars().count() <= MAX_TIMEZONE => t.to_string(),
        _ => return Err("timezone is required".into()),
    };
    let rows = match o.get("holidays") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        Some(_) => return Err("holidays must be a list".into()),
    };
    if rows.len() > MAX_HOLIDAYS {
        return Err(format!("at most {MAX_HOLIDAYS} holidays"));
    }
    let mut holidays = Vec::with_capacity(rows.len());
    for (i, h) in rows.iter().enumerate() {
        let n = i + 1;
        let Some(h) = h.as_object() else {
            return Err(format!("holiday {n}: must be an object"));
        };
        let Some(date) = h.get("date").and_then(Value::as_str).and_then(parse_date) else {
            return Err(format!("holiday {n}: date must be YYYY-MM-DD"));
        };
        let label = match h.get("label") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.trim().to_string(),
            Some(_) => return Err(format!("holiday {n}: label must be text")),
        };
        if label.chars().count() > MAX_LABEL {
            return Err(format!("holiday {n}: label is longer than {MAX_LABEL}"));
        }
        match h.get("closed") {
            None | Some(Value::Bool(true)) => {}
            Some(_) => return Err(format!("holiday {n}: closed must be true")),
        }
        holidays.push(Holiday { date, label });
    }
    holidays.sort_by_key(|h| h.date);
    if let Some(w) = holidays.windows(2).find(|w| w[0].date == w[1].date) {
        return Err(format!("{} is listed twice", w[0].date.format("%Y-%m-%d")));
    }
    Ok(Calendar { timezone, holidays })
}

/// A stored value as a [`Calendar`] (`None` = no value, or one that no longer validates).
pub fn of_value(v: &Value) -> Option<Calendar> {
    validate(v).ok()
}

/// One-line text: `Europe/Amsterdam · 3 holidays`, or `no calendar`.
pub fn calendar_text(c: Option<&Calendar>) -> String {
    match c {
        None => "no calendar".to_string(),
        Some(c) => {
            let n = c.holidays.len();
            format!(
                "{} \u{b7} {n} holiday{}",
                c.timezone,
                if n == 1 { "" } else { "s" }
            )
        }
    }
}

/// Write the tenant's working calendar (insert or replace). Only inside a `track_tenant` closure or `set`.
pub async fn write(c: &mut PgConnection, tenant: Uuid, cal: &Calendar, by: &str) -> ApiResult<()> {
    let v = cal.to_value();
    sqlx::query(
        "INSERT INTO tenant_calendar (tenant_id, timezone, holidays, updated_by) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (tenant_id) DO UPDATE \
            SET timezone = EXCLUDED.timezone, holidays = EXCLUDED.holidays, updated_by = EXCLUDED.updated_by, \
                updated_at = clock_timestamp()",
    )
    .bind(tenant)
    .bind(cal.timezone.clone())
    .bind(v["holidays"].clone())
    .bind(by.to_owned())
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Remove the working row (no calendar). Only inside `set`.
async fn delete_row(c: &mut PgConnection, tenant: Uuid) -> ApiResult<()> {
    sqlx::query("DELETE FROM tenant_calendar WHERE tenant_id = $1")
        .bind(tenant)
        .execute(&mut *c)
        .await?;
    Ok(())
}

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

/// The tenant calendar job `job` runs in the design `of` (`Live` for prod, `Draft` for test), through
/// `design::read` with `reg`; `None` = no calendar.
pub async fn read(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<Option<Calendar>> {
    let v = design::read::value(c, reg, job, of, KIND, &DraftRef::new()).await?;
    Ok(v.as_ref().and_then(of_value))
}

#[derive(sqlx::FromRow)]
struct JobRow {
    id: Uuid,
    key: String,
}

/// `tenantCalendar`: `{live, draft, drafted}`. `draft` = the tenant's working row; `live` = the value in the active
/// release of the tenant's most recently applied job (the newest live value; a job never released counts last);
/// `drafted` = the keys of the jobs whose draft ledger has the calendar (their Production is still on an older value
/// until they publish).
pub async fn view(c: &mut PgConnection, reg: &Registry, tenant: Uuid) -> ApiResult<Value> {
    let at = Place { tenant, job: None };
    let draft = TenantCalendarTarget
        .get(&mut *c, at, &DraftRef::new())
        .await?;
    let newest: Option<JobRow> = sqlx::query_as(
        "SELECT j.id, j.key FROM jobs j \
           LEFT JOIN job_release r ON r.tenant_id = j.tenant_id AND r.job_id = j.id \
                                  AND r.release_id = j.current_release_id \
          WHERE j.tenant_id = $1 \
          ORDER BY COALESCE(r.last_applied_at, r.released_at) DESC NULLS LAST, j.id \
          LIMIT 1",
    )
    .bind(tenant)
    .fetch_optional(&mut *c)
    .await?;
    let live = match newest {
        Some(j) => {
            let jr = JobRef {
                id: j.id,
                key: j.key,
            };
            read(&mut *c, reg, &jr, &DesignOf::Live).await?
        }
        None => None,
    };
    let drafted: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT j.key FROM draft_ledger d JOIN jobs j ON j.tenant_id = d.tenant_id AND j.id = d.job_id \
          WHERE d.tenant_id = $1 AND d.kind = 'tenant_calendar' ORDER BY j.key",
    )
    .bind(tenant)
    .fetch_all(&mut *c)
    .await?;
    Ok(json!({
        "live": live.as_ref().map(Calendar::to_value),
        "draft": draft.as_ref().and_then(of_value).as_ref().map(Calendar::to_value),
        "drafted": drafted,
    }))
}

/// `tenant_calendar` (F-14): the tenant's holidays. Tenant-wide: one working row for every job.
pub struct TenantCalendarTarget;

impl DesignTarget for TenantCalendarTarget {
    fn kind(&self) -> &'static str {
        KIND
    }
    fn scope(&self) -> Scope {
        Scope::Tenant
    }
    fn key(&self, _r: &DraftRef) -> String {
        KEY.to_string()
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        _r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let row: Option<(String, Value)> = sqlx::query_as(
                "SELECT timezone, holidays FROM tenant_calendar WHERE tenant_id = $1",
            )
            .bind(at.tenant)
            .fetch_optional(&mut *c)
            .await?;
            Ok(row.map(|(tz, h)| json!({"timezone": tz, "holidays": h})))
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        _r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            match v {
                None => delete_row(c, at.tenant).await,
                Some(value) => {
                    // a restore writes back exactly what `get` returned (validated when it was saved)
                    let cal = validate(value).map_err(|e| {
                        internal(format!("tenant_calendar value does not validate: {e}"))
                    })?;
                    write(c, at.tenant, &cal, SET_BY).await
                }
            }
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let n: i64 =
                sqlx::query_scalar("SELECT count(*) FROM tenant_calendar WHERE tenant_id = $1")
                    .bind(at.tenant)
                    .fetch_one(&mut *c)
                    .await?;
            Ok(Some(if n > 0 {
                vec![DraftRef::new()]
            } else {
                Vec::new()
            }))
        })
    }
    /// `Europe/Amsterdam · 3 holidays`, or `no calendar`.
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        Some(calendar_text(v.as_ref().and_then(of_value).as_ref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_sorts_and_refuses() {
        let c = validate(&json!({"timezone": " Europe/Amsterdam ", "holidays": [
            {"date": "2026-12-26", "label": "Boxing Day", "closed": true},
            {"date": "2026-12-25", "label": " Christmas "},
        ]}))
        .unwrap();
        assert_eq!(c.timezone, "Europe/Amsterdam");
        assert_eq!(c.holidays[0].label, "Christmas");
        assert!(c.closed_on(NaiveDate::from_ymd_opt(2026, 12, 26).unwrap()));
        assert!(!c.closed_on(NaiveDate::from_ymd_opt(2026, 12, 27).unwrap()));
        assert_eq!(c.to_value()["holidays"][0]["date"], "2026-12-25");
        assert_eq!(of_value(&c.to_value()), Some(c.clone()));
        assert_eq!(
            calendar_text(Some(&c)),
            "Europe/Amsterdam \u{b7} 2 holidays"
        );
        assert_eq!(calendar_text(None), "no calendar");

        for (v, msg) in [
            (json!({"holidays": []}), "timezone is required"),
            (
                json!({"timezone": "UTC", "holidays": [{"date": "2026-2-30"}]}),
                "holiday 1: date must be YYYY-MM-DD",
            ),
            (
                json!({"timezone": "UTC", "holidays": [{"date": "2026-02-30"}]}),
                "holiday 1: date must be YYYY-MM-DD",
            ),
            (
                json!({"timezone": "UTC", "holidays": [{"date": "2026-01-01"}, {"date": "2026-01-01"}]}),
                "2026-01-01 is listed twice",
            ),
            (
                json!({"timezone": "UTC", "holidays": [{"date": "2026-01-01", "closed": false}]}),
                "holiday 1: closed must be true",
            ),
            (
                json!({"timezone": "UTC", "holidays": [{"date": "2026-01-01", "label": "x".repeat(81)}]}),
                "holiday 1: label is longer than 80",
            ),
        ] {
            assert_eq!(validate(&v), Err(msg.to_string()), "{v}");
        }
        let many: Vec<Value> = (0..367)
            .map(|i| {
                let d = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Duration::days(i);
                json!({"date": d.format("%Y-%m-%d").to_string()})
            })
            .collect();
        assert_eq!(
            validate(&json!({"timezone": "UTC", "holidays": many})),
            Err("at most 366 holidays".to_string())
        );
    }
}
