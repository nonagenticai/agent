//! `GET /api/support/csat?from=&to=` (SW-6): the tenant-wide CSAT report for `support_manager` and `tenant_admin`.
//!
//! - **Roles**: checked FIRST, before the query is read, so a caller without the role always gets 403 (`missing_role:
//!   support_manager`), whatever the query. The comments are customer text and are shown to these roles only; they are
//!   never logged.
//! - **Window**: `[from, to)` on `case_csat.asked_at` (ISO-8601 instants or `YYYY-MM-DD`; default the last 30 days; a
//!   date-only `to` includes that whole day). The report is a COHORT of the requests asked in the window: `answered`,
//!   the mean, the distribution, the splits and the low scores all count only those requests, so `answered <= asked`
//!   and `response_rate = answered / asked` (null when nothing was asked) always hold. An answer that arrives after the
//!   window closes still counts for a request asked inside it.
//! - **Scope**: every job of the tenant, `env = 'prod'` only (test conversations are never reported). A request that
//!   was armed but never asked (`awaiting_confirmation`, `asked_at` NULL) is not in the cohort.
//! - `by_resolver`: `agent` (one row, no sub) and one row per staff member (`sub`, `name` from `users`, else the name the
//!   member last presented as staff, else the sub). `low_scores`: rating 1 or 2, newest answer first, at most 50.
use axum::extract::State;
use axum::response::Json;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;

use crate::auth::Role;
use crate::conversations::iso;
use crate::conversations::store::MEMBER_NAME_SQL;
use crate::error::{ApiError, ApiQuery, ApiResult};
use crate::state::AppState;
use crate::tenancy::{Member, missing_role};

/// The default window when `from` is absent.
pub const DEFAULT_DAYS: i64 = 30;
/// The most low scores listed.
pub const LOW_LIMIT: i64 = 50;

#[derive(Deserialize)]
pub struct CsatQuery {
    from: Option<String>,
    to: Option<String>,
}

fn bad(what: &str) -> ApiError {
    ApiError::BadRequest(
        "invalid_query",
        format!("{what} is an ISO-8601 instant or a YYYY-MM-DD date."),
    )
}

/// An instant: RFC 3339, or a date (midnight UTC; with `end_of_day` the start of the next day).
fn instant(s: &str, name: &str, end_of_day: bool) -> ApiResult<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    let d = NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| bad(name))?;
    let mut t = d.and_hms_opt(0, 0, 0).ok_or_else(|| bad(name))?.and_utc();
    if end_of_day {
        t += Duration::days(1);
    }
    Ok(t)
}

/// The window `[from, to)` of a query at `now`.
pub fn window(q: &CsatQuery, now: DateTime<Utc>) -> ApiResult<(DateTime<Utc>, DateTime<Utc>)> {
    fn given(v: &Option<String>) -> Option<&str> {
        v.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }
    let to = match given(&q.to) {
        Some(s) => instant(s, "to", true)?,
        None => now,
    };
    let from = match given(&q.from) {
        Some(s) => instant(s, "from", false)?,
        None => to - Duration::days(DEFAULT_DAYS),
    };
    if from >= to {
        return Err(ApiError::BadRequest(
            "invalid_query",
            "from must be before to.".into(),
        ));
    }
    Ok((from, to))
}

fn mean(v: Option<f64>) -> Value {
    v.map_or(Value::Null, |m| json!((m * 100.0).round() / 100.0))
}

pub async fn get_csat(
    Member(t): Member,
    State(st): State<AppState>,
    q: Result<ApiQuery<CsatQuery>, ApiError>,
) -> ApiResult<Json<Value>> {
    if !(t.effective.has(Role::TenantAdmin) || t.effective.has(Role::SupportManager)) {
        return Err(missing_role(Role::SupportManager));
    }
    let ApiQuery(q) = q?;
    let (from, to) = window(&q, Utc::now())?;
    let mut tx = t.begin(&st.db).await?;

    let cohort = "FROM case_csat c WHERE c.env = 'prod' AND c.asked_at IS NOT NULL \
                  AND c.asked_at >= $1 AND c.asked_at < $2";
    let all = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) AS asked, count(c.rating) AS answered, avg(c.rating)::float8 AS mean, \
                count(*) FILTER (WHERE c.rating = 1) AS r1, count(*) FILTER (WHERE c.rating = 2) AS r2, \
                count(*) FILTER (WHERE c.rating = 3) AS r3, count(*) FILTER (WHERE c.rating = 4) AS r4, \
                count(*) FILTER (WHERE c.rating = 5) AS r5 {cohort}"
    )))
    .bind(from)
    .bind(to)
    .fetch_one(&mut *tx)
    .await?;
    let asked: i64 = all.get("asked");
    let answered: i64 = all.get("answered");
    let mut distribution = serde_json::Map::new();
    for k in 1..=5 {
        distribution.insert(
            k.to_string(),
            json!(all.get::<i64, _>(format!("r{k}").as_str())),
        );
    }

    let by_channel = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT c.channel, count(*) AS asked, count(c.rating) AS answered, avg(c.rating)::float8 AS mean \
         {cohort} GROUP BY c.channel ORDER BY count(*) DESC, c.channel"
    )))
    .bind(from)
    .bind(to)
    .fetch_all(&mut *tx)
    .await?
    .iter()
    .map(|r| {
        json!({
            "channel": r.get::<String, _>("channel"),
            "asked": r.get::<i64, _>("asked"),
            "answered": r.get::<i64, _>("answered"),
            "mean": mean(r.get("mean")),
        })
    })
    .collect::<Vec<_>>();

    let by_resolver = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT c.asked_by, c.asked_by_sub, \
                CASE WHEN c.asked_by_sub IS NOT NULL THEN \
                     COALESCE((SELECT {MEMBER_NAME_SQL} FROM users WHERE users.sub = c.asked_by_sub), \
                              agent_actor_name(c.asked_by_sub), c.asked_by_sub) END AS name, \
                count(*) AS asked, count(c.rating) AS answered, avg(c.rating)::float8 AS mean \
         {cohort} GROUP BY c.asked_by, c.asked_by_sub ORDER BY count(*) DESC, c.asked_by, c.asked_by_sub"
    )))
    .bind(from)
    .bind(to)
    .fetch_all(&mut *tx)
    .await?
    .iter()
    .map(|r| {
        json!({
            "asked_by": r.get::<String, _>("asked_by"),
            "sub": r.get::<Option<String>, _>("asked_by_sub"),
            "name": r.get::<Option<String>, _>("name"),
            "asked": r.get::<i64, _>("asked"),
            "answered": r.get::<i64, _>("answered"),
            "mean": mean(r.get("mean")),
        })
    })
    .collect::<Vec<_>>();

    let low_scores = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT c.case_id, c.conversation_id, j.key AS job_key, c.channel, c.rating, c.comment, c.answered_at \
           FROM case_csat c JOIN jobs j ON j.tenant_id = c.tenant_id AND j.id = c.job_id \
          WHERE c.env = 'prod' AND c.asked_at IS NOT NULL AND c.asked_at >= $1 AND c.asked_at < $2 \
            AND c.rating <= 2 \
          ORDER BY c.answered_at DESC, c.case_id LIMIT {LOW_LIMIT}"
    )))
    .bind(from)
    .bind(to)
    .fetch_all(&mut *tx)
    .await?
    .iter()
    .map(|r| {
        json!({
            "case_id": r.get::<String, _>("case_id"),
            "conversation_id": r.get::<String, _>("conversation_id"),
            "job_key": r.get::<String, _>("job_key"),
            "channel": r.get::<String, _>("channel"),
            "rating": r.get::<i16, _>("rating"),
            "comment": r.get::<Option<String>, _>("comment"),
            "answered_at": iso(r.get::<DateTime<Utc>, _>("answered_at")),
        })
    })
    .collect::<Vec<_>>();
    tx.commit().await?;

    Ok(Json(json!({
        "from": iso(from),
        "to": iso(to),
        "asked": asked,
        "answered": answered,
        "response_rate": if asked == 0 { Value::Null } else { json!(answered as f64 / asked as f64) },
        "mean": mean(all.get("mean")),
        "distribution": Value::Object(distribution),
        "by_channel": by_channel,
        "by_resolver": by_resolver,
        "low_scores": low_scores,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(from: Option<&str>, to: Option<&str>) -> CsatQuery {
        CsatQuery {
            from: from.map(Into::into),
            to: to.map(Into::into),
        }
    }

    #[test]
    fn the_window_defaults_to_thirty_days_and_takes_dates_or_instants() {
        let now = DateTime::parse_from_rfc3339("2026-10-27T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (f, t) = window(&q(None, None), now).unwrap();
        assert_eq!((t, now - f), (now, Duration::days(30)));
        let (f, t) = window(&q(Some("2026-10-01"), Some("2026-10-02")), now).unwrap();
        assert_eq!(iso(f), "2026-10-01T00:00:00.000Z");
        assert_eq!(iso(t), "2026-10-03T00:00:00.000Z");
        let (f, _) = window(&q(Some("2026-10-01T05:00:00+02:00"), None), now).unwrap();
        assert_eq!(iso(f), "2026-10-01T03:00:00.000Z");
        assert_eq!(
            window(&q(Some("nope"), None), now).unwrap_err().code(),
            "invalid_query"
        );
        assert_eq!(
            window(&q(Some("2026-10-05"), Some("2026-10-01")), now)
                .unwrap_err()
                .code(),
            "invalid_query"
        );
    }
}
