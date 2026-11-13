//! J2 job analytics (B4 T15): `jobAnalytics` (`GET /api/jobs/{job}/analytics?days=7|30`, `JobAnalytics` in web
//! `types/runtime.ts`). Any member reads. Also the window, outcome and turn reads J1's `jobOverview` KPIs share
//! (crate::routes::overview).
//!
//! Definitions (one place, both pages):
//! - a window is whole UTC days ending today (today included); the previous window is the same number of days
//!   before it;
//! - a case belongs to the day it started; it is **contained** when it resolved without going to staff, and
//!   **escalated** when it went to staff (status `handover` or an escalation reason);
//! - a **turn** is one qualifier call (`usage_daily.use = 'qualify'`, as `tenantUsage`); tokens are
//!   `tokens_in + tokens_out`;
//! - every figure is Production only (D54): cases `env = 'prod'`, usage without the staff test chat and the
//!   generator's sources; the Test env's traffic is reported apart in `test_env`;
//! - `csat` is the mean rating of the answered Production `case_csat` rows (0067) answered in the window.
use std::collections::{BTreeMap, HashMap};

use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::get;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::conversations::iso;
use crate::error::{ApiError, ApiQuery, ApiResult};
use crate::state::AppState;
use crate::tenancy::JobMember;
use crate::usage::report::FUNDING;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/jobs/{job}/analytics", get(analytics))
}

/// Production conversation usage: every source but the staff test chat and the generator's (D56).
pub(crate) const PROD_USAGE: &str = "source NOT IN ('staff_test', 'generator', 'discovery')";
/// A case that resolved without going to staff.
pub(crate) const CONTAINED: &str = "status = 'resolved' AND escalation_reason IS NULL";
/// A case that went to staff.
pub(crate) const ESCALATED: &str = "(status = 'handover' OR escalation_reason IS NOT NULL)";

// ---- shared with the overview -----------------------------------------------------------------------------------

/// Whole UTC days `from..=to` and the window of the same length before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Window {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub prev_from: NaiveDate,
    pub prev_to: NaiveDate,
}

impl Window {
    /// The `days` days ending `today` (included).
    pub fn ending(today: NaiveDate, days: i64) -> Self {
        let from = today - Duration::days(days - 1);
        Self {
            from,
            to: today,
            prev_from: from - Duration::days(days),
            prev_to: from - Duration::days(1),
        }
    }
}

/// The `days` window ending today in UTC (the database clock).
pub(crate) async fn window(c: &mut PgConnection, days: i64) -> ApiResult<Window> {
    let today: NaiveDate = sqlx::query_scalar("SELECT (now() AT TIME ZONE 'UTC')::date")
        .fetch_one(&mut *c)
        .await?;
    Ok(Window::ending(today, days))
}

/// Production case outcomes of the cases started in `from..=to`.
#[derive(Clone, Copy, Debug, Default, sqlx::FromRow)]
pub(crate) struct Outcomes {
    pub cases: i64,
    pub contained: i64,
    pub escalated: i64,
}

/// [`Outcomes`] of the job's Production cases started in `from..=to` (UTC days).
pub(crate) async fn outcomes(
    c: &mut PgConnection,
    job: Uuid,
    from: NaiveDate,
    to: NaiveDate,
) -> ApiResult<Outcomes> {
    Ok(sqlx::query_as::<_, Outcomes>(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) AS cases, count(*) FILTER (WHERE {CONTAINED}) AS contained, \
                count(*) FILTER (WHERE {ESCALATED}) AS escalated \
           FROM agent_case \
          WHERE job_id = $1 AND env = 'prod' \
            AND (started_at AT TIME ZONE 'UTC')::date BETWEEN $2 AND $3"
    )))
    .bind(job)
    .bind(from)
    .bind(to)
    .fetch_one(&mut *c)
    .await?)
}

/// Production turns (qualifier calls) of the job in `from..=to` (UTC days).
pub(crate) async fn turns(
    c: &mut PgConnection,
    job: Uuid,
    from: NaiveDate,
    to: NaiveDate,
) -> ApiResult<i64> {
    Ok(sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
        "SELECT COALESCE(sum(calls), 0)::int8 FROM usage_daily \
          WHERE job_id = $1 AND use = 'qualify' AND {PROD_USAGE} AND day BETWEEN $2 AND $3"
    )))
    .bind(job)
    .bind(from)
    .bind(to)
    .fetch_one(&mut *c)
    .await?)
}

/// `n / d`, `None` without a denominator.
pub(crate) fn ratio(n: i64, d: i64) -> Option<f64> {
    if d > 0 {
        Some(n as f64 / d as f64)
    } else {
        None
    }
}

/// The job's north star is containment when its label names it (`settings`-free: the label is the setting).
pub(crate) fn names_containment(north_star: &str) -> bool {
    north_star.to_lowercase().contains("contain")
}

/// `x` rounded to 2 decimals.
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// The label of an escalation reason (the handover reasons of 0063, else the code in words).
pub(crate) fn reason_label(reason: &str) -> String {
    match reason {
        "identity_failed" => "Identity could not be confirmed".into(),
        "gate_blocked" => "A gate blocked the answer".into(),
        "customer_asked" => "Customer asked for a person".into(),
        "tool_failed" => "A system call failed".into(),
        "unknown_outcome" => "An operation's outcome is unknown".into(),
        "out_of_scope" => "Out of scope for the agent".into(),
        "staff_decision" => "Handed over by staff".into(),
        "gate_unavailable" => "A gate was unavailable".into(),
        "unspecified" => "No reason recorded".into(),
        other => {
            let words = other.replace('_', " ");
            let mut cs = words.chars();
            match cs.next() {
                Some(f) => f.to_uppercase().chain(cs).collect(),
                None => String::new(),
            }
        }
    }
}

// ---- jobAnalytics -----------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct AnalyticsQuery {
    days: Option<String>,
}

/// `days=7|30` (absent or empty = 30); 422 `invalid_query` otherwise.
fn days_param(raw: Option<&str>) -> ApiResult<i64> {
    match raw.map(str::trim) {
        None | Some("" | "30") => Ok(30),
        Some("7") => Ok(7),
        Some(_) => Err(ApiError::Unprocessable(
            "invalid_query",
            "days must be 7 or 30".into(),
        )),
    }
}

#[derive(sqlx::FromRow)]
struct DayRow {
    day: NaiveDate,
    turns: i64,
    cases: i64,
    contained: i64,
    escalated: i64,
    not_eligible: i64,
}

#[derive(sqlx::FromRow)]
struct ActivityRow {
    activity: String,
    cases: i64,
    contained: i64,
    escalated: i64,
    p50_ms: Option<i64>,
}

/// `GET /api/jobs/{job}/analytics?days=7|30` — `JobAnalytics`.
async fn analytics(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<AnalyticsQuery>,
) -> ApiResult<Json<Value>> {
    let n = days_param(q.days.as_deref())?;
    let job = j.job_id;
    let mut tx = j.begin(&st.db).await?;
    let as_of: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *tx)
        .await?;
    let w = window(&mut tx, n).await?;
    let (north_star, draft_rev): (String, i64) =
        sqlx::query_as("SELECT north_star, draft_rev FROM jobs WHERE id = $1")
            .bind(job)
            .fetch_one(&mut *tx)
            .await?;

    let days = sqlx::query_as::<_, DayRow>(sqlx::AssertSqlSafe(format!(
        "WITH d AS (SELECT generate_series($2::date, $3::date, interval '1 day')::date AS day), \
              c AS (SELECT (started_at AT TIME ZONE 'UTC')::date AS day, count(*) AS cases, \
                           count(*) FILTER (WHERE {CONTAINED}) AS contained, \
                           count(*) FILTER (WHERE {ESCALATED}) AS escalated, \
                           count(*) FILTER (WHERE status = 'not_eligible') AS not_eligible \
                      FROM agent_case \
                     WHERE job_id = $1 AND env = 'prod' \
                       AND (started_at AT TIME ZONE 'UTC')::date BETWEEN $2 AND $3 \
                     GROUP BY 1), \
              u AS (SELECT day, sum(calls) AS turns FROM usage_daily \
                     WHERE job_id = $1 AND use = 'qualify' AND {PROD_USAGE} AND day BETWEEN $2 AND $3 \
                     GROUP BY day) \
         SELECT d.day, COALESCE(u.turns, 0)::int8 AS turns, COALESCE(c.cases, 0)::int8 AS cases, \
                COALESCE(c.contained, 0)::int8 AS contained, COALESCE(c.escalated, 0)::int8 AS escalated, \
                COALESCE(c.not_eligible, 0)::int8 AS not_eligible \
           FROM d LEFT JOIN c ON c.day = d.day LEFT JOIN u ON u.day = d.day \
          ORDER BY d.day"
    )))
    .bind(job)
    .bind(w.from)
    .bind(w.to)
    .fetch_all(&mut *tx)
    .await?;
    let cur = days.iter().fold(Outcomes::default(), |a, d| Outcomes {
        cases: a.cases + d.cases,
        contained: a.contained + d.contained,
        escalated: a.escalated + d.escalated,
    });
    let prev = outcomes(&mut tx, job, w.prev_from, w.prev_to).await?;
    let prev_turns = turns(&mut tx, job, w.prev_from, w.prev_to).await?;

    let reasons: Vec<(String, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT COALESCE(escalation_reason, 'unspecified') AS reason, count(*) AS n FROM agent_case \
          WHERE job_id = $1 AND env = 'prod' AND {ESCALATED} \
            AND (started_at AT TIME ZONE 'UTC')::date BETWEEN $2 AND $3 \
          GROUP BY 1 ORDER BY n DESC, reason"
    )))
    .bind(job)
    .bind(w.from)
    .bind(w.to)
    .fetch_all(&mut *tx)
    .await?;

    let csat = |from: NaiveDate, to: NaiveDate| {
        sqlx::query_as::<_, (Option<f64>, i64)>(
            "SELECT avg(rating)::float8, count(*) FROM case_csat \
              WHERE job_id = $1 AND env = 'prod' AND answered_at IS NOT NULL \
                AND (answered_at AT TIME ZONE 'UTC')::date BETWEEN $2 AND $3",
        )
        .bind(job)
        .bind(from)
        .bind(to)
    };
    let (score, responses) = csat(w.from, w.to).fetch_one(&mut *tx).await?;
    let (prev_score, _) = csat(w.prev_from, w.prev_to).fetch_one(&mut *tx).await?;

    let by_activity = sqlx::query_as::<_, ActivityRow>(sqlx::AssertSqlSafe(format!(
        "SELECT COALESCE(activity, 'unqualified') AS activity, count(*) AS cases, \
                count(*) FILTER (WHERE {CONTAINED}) AS contained, \
                count(*) FILTER (WHERE {ESCALATED}) AS escalated, \
                percentile_disc(0.5) WITHIN GROUP (ORDER BY duration_ms) AS p50_ms \
           FROM agent_case \
          WHERE job_id = $1 AND env = 'prod' \
            AND (started_at AT TIME ZONE 'UTC')::date BETWEEN $2 AND $3 \
          GROUP BY 1 ORDER BY cases DESC, activity"
    )))
    .bind(job)
    .bind(w.from)
    .bind(w.to)
    .fetch_all(&mut *tx)
    .await?;

    let lanes: Vec<(String, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT funding, COALESCE(sum(tokens_in + tokens_out), 0)::int8 FROM usage_daily \
          WHERE job_id = $1 AND {PROD_USAGE} AND day BETWEEN $2 AND $3 GROUP BY funding"
    )))
    .bind(job)
    .bind(w.from)
    .bind(w.to)
    .fetch_all(&mut *tx)
    .await?;
    let mut funding = serde_json::Map::new();
    for lane in FUNDING {
        let t = lanes
            .iter()
            .filter(|(f, _)| f == lane)
            .map(|(_, t)| *t)
            .sum::<i64>();
        funding.insert(lane.to_string(), json!(t));
    }
    let tokens: i64 = lanes.iter().map(|(_, t)| *t).sum();

    let release_windows = release_windows(&mut tx, job).await?;
    let test_env = test_env(&mut tx, job, draft_rev).await?;
    tx.commit().await?;

    let containment = ratio(cur.contained, cur.cases);
    let north_value = if names_containment(&north_star) {
        containment
    } else {
        None
    };
    Ok(Json(json!({
        "days": days.iter().map(|d| json!({
            "day": d.day.to_string(),
            "turns": d.turns,
            "cases": d.cases,
            "contained": d.contained,
            "escalated": d.escalated,
            "not_eligible": d.not_eligible,
        })).collect::<Vec<_>>(),
        "previous": {
            "turns": prev_turns,
            "cases": prev.cases,
            "contained": prev.contained,
            "escalated": prev.escalated,
        },
        "escalation_reasons": reasons.iter().map(|(r, n)| json!({
            "reason": r,
            "label": reason_label(r),
            "count": n,
        })).collect::<Vec<_>>(),
        "csat": {
            "score": score.map(round2),
            "responses": responses,
            "previous": prev_score.map(round2),
        },
        "north_star": {
            "label": north_star,
            "value": north_value,
            "target": Value::Null,
            "unit": "pct",
        },
        "by_activity": by_activity.iter().map(|a| json!({
            "activity": a.activity,
            "cases": a.cases,
            "contained": a.contained,
            "escalated": a.escalated,
            "p50_ms": a.p50_ms,
        })).collect::<Vec<_>>(),
        "tokens": tokens,
        "funding": Value::Object(funding),
        "release_windows": release_windows,
        "as_of": iso(as_of),
        "test_env": test_env,
    })))
}

// ---- release windows --------------------------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct RelRow {
    release_id: String,
    state: String,
    applied_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct RelStats {
    release_id: String,
    cases: i64,
    contained: i64,
    escalated: i64,
    p50_ms: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct RelActivity {
    release_id: String,
    activity: String,
    cases: i64,
    contained: i64,
}

/// `release_windows`: every release of the job, newest cut first, with the outcomes of the Production cases that
/// ran on it (`agent_case.release_id`) since it went live. `until` is when the next release went live (null for the
/// current one); `changes` are the labels of the drafts the cut bundled; `moves` the (at most 3) activities whose
/// containment moved most against the release applied before it.
async fn release_windows(c: &mut PgConnection, job: Uuid) -> ApiResult<Vec<Value>> {
    let rels = sqlx::query_as::<_, RelRow>(
        "SELECT release_id, state, COALESCE(last_applied_at, released_at) AS applied_at FROM job_release \
          WHERE job_id = $1 ORDER BY approved_at DESC, release_id DESC",
    )
    .bind(job)
    .fetch_all(&mut *c)
    .await?;
    if rels.is_empty() {
        return Ok(Vec::new());
    }
    let stats: HashMap<String, RelStats> = sqlx::query_as::<_, RelStats>(sqlx::AssertSqlSafe(format!(
        "SELECT release_id, count(*) AS cases, count(*) FILTER (WHERE {CONTAINED}) AS contained, \
                count(*) FILTER (WHERE {ESCALATED}) AS escalated, \
                percentile_disc(0.5) WITHIN GROUP (ORDER BY duration_ms) AS p50_ms \
           FROM agent_case WHERE job_id = $1 AND env = 'prod' AND release_id IS NOT NULL \
          GROUP BY release_id"
    )))
    .bind(job)
    .fetch_all(&mut *c)
    .await?
    .into_iter()
    .map(|s| (s.release_id.clone(), s))
    .collect();
    let csat: HashMap<String, (Option<f64>, i64)> =
        sqlx::query_as::<_, (String, Option<f64>, i64)>(
            "SELECT release_id, avg(rating)::float8, count(*) FROM case_csat \
              WHERE job_id = $1 AND env = 'prod' AND answered_at IS NOT NULL AND release_id IS NOT NULL \
              GROUP BY release_id",
        )
        .bind(job)
        .fetch_all(&mut *c)
        .await?
        .into_iter()
        .map(|(r, s, n)| (r, (s, n)))
        .collect();
    let tokens: HashMap<String, i64> = sqlx::query_as::<_, (String, i64)>(
        "SELECT a.release_id, COALESCE(sum(u.tokens_in + u.tokens_out), 0)::int8 \
           FROM usage_event u \
           JOIN agent_case a ON a.tenant_id = u.tenant_id AND a.job_id = u.job_id AND a.case_id = u.case_id \
          WHERE u.job_id = $1 AND a.env = 'prod' AND a.release_id IS NOT NULL \
          GROUP BY a.release_id",
    )
    .bind(job)
    .fetch_all(&mut *c)
    .await?
    .into_iter()
    .collect();
    let mut activities: HashMap<String, BTreeMap<String, (i64, i64)>> = HashMap::new();
    for a in sqlx::query_as::<_, RelActivity>(sqlx::AssertSqlSafe(format!(
        "SELECT release_id, COALESCE(activity, 'unqualified') AS activity, count(*) AS cases, \
                count(*) FILTER (WHERE {CONTAINED}) AS contained \
           FROM agent_case WHERE job_id = $1 AND env = 'prod' AND release_id IS NOT NULL \
          GROUP BY 1, 2"
    )))
    .bind(job)
    .fetch_all(&mut *c)
    .await?
    {
        activities
            .entry(a.release_id)
            .or_default()
            .insert(a.activity, (a.cases, a.contained));
    }
    let mut changes: HashMap<String, Vec<String>> = HashMap::new();
    for (r, label) in sqlx::query_as::<_, (String, String)>(
        "SELECT release_id, label FROM release_bundled_draft WHERE job_id = $1 ORDER BY at, ledger_id",
    )
    .bind(job)
    .fetch_all(&mut *c)
    .await?
    {
        let list = changes.entry(r).or_default();
        if !list.contains(&label) {
            list.push(label);
        }
    }

    // went live: oldest first, to find each release's successor and predecessor
    let mut live: Vec<(&str, DateTime<Utc>)> = rels
        .iter()
        .filter(|r| r.state != "approved")
        .filter_map(|r| r.applied_at.map(|at| (r.release_id.as_str(), at)))
        .collect();
    live.sort_by_key(|(id, at)| (*at, *id));
    let empty = BTreeMap::new();
    let mut out = Vec::with_capacity(rels.len());
    for r in &rels {
        let status = match r.state.as_str() {
            "current" => "current",
            "approved" => "approved",
            _ => "earlier",
        };
        let pos = live.iter().position(|(id, _)| *id == r.release_id);
        let until = match (status, pos) {
            ("earlier", Some(i)) => live.get(i + 1).map(|(_, at)| iso(*at)),
            _ => None,
        };
        let before = pos
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| live.get(i))
            .map(|(id, _)| *id);
        let s = stats.get(&r.release_id);
        let cases = s.map_or(0, |s| s.cases);
        let (csat_score, csat_n) = csat.get(&r.release_id).copied().unwrap_or((None, 0));
        let mine = activities.get(&r.release_id).unwrap_or(&empty);
        let theirs = before.and_then(|b| activities.get(b)).unwrap_or(&empty);
        let applied_at = if status == "approved" {
            None
        } else {
            r.applied_at.map(iso)
        };
        let spent = tokens.get(&r.release_id).copied().unwrap_or(0);
        let tokens_per_case = ratio(spent, cases).map(|x| x.round() as i64);
        out.push(json!({
            "release_id": r.release_id,
            "status": status,
            "applied_at": applied_at,
            "until": until,
            "cases": cases,
            "contained": s.map_or(0, |s| s.contained),
            "escalated": s.map_or(0, |s| s.escalated),
            "csat": csat_score.map(round2),
            "csat_responses": csat_n,
            "p50_ms": s.and_then(|s| s.p50_ms),
            "tokens_per_case": tokens_per_case,
            "changes": changes.get(&r.release_id).cloned().unwrap_or_default(),
            "moves": moves(mine, theirs),
        }));
    }
    Ok(out)
}

/// The (at most 3) activities whose containment rate moved most from `before` to `mine` (both with cases).
fn moves(mine: &BTreeMap<String, (i64, i64)>, before: &BTreeMap<String, (i64, i64)>) -> Vec<Value> {
    let mut m: Vec<(String, f64, f64, i64)> = mine
        .iter()
        .filter_map(|(act, (cases, contained))| {
            let (bc, bk) = before.get(act)?;
            let to = ratio(*contained, *cases)?;
            let from = ratio(*bk, *bc)?;
            (to != from).then(|| (act.clone(), from, to, *cases))
        })
        .collect();
    m.sort_by(|a, b| {
        (b.2 - b.1)
            .abs()
            .total_cmp(&(a.2 - a.1).abs())
            .then_with(|| a.0.cmp(&b.0))
    });
    m.truncate(3);
    m.into_iter()
        .map(|(activity, from, to, cases)| {
            json!({"activity": activity, "from": round2(from), "to": round2(to), "cases": cases})
        })
        .collect()
}

// ---- test env ---------------------------------------------------------------------------------------------------

/// `test_env`: the staff test chats (`env = 'test'`) since the current release went live (else since the job was
/// created), on draft revision `draft_rev`, and the labels of the unpublished draft changes being tried.
async fn test_env(c: &mut PgConnection, job: Uuid, draft_rev: i64) -> ApiResult<Value> {
    let since: DateTime<Utc> = sqlx::query_scalar(
        "SELECT COALESCE((SELECT COALESCE(r.last_applied_at, r.released_at) FROM job_release r \
                           WHERE r.tenant_id = j.tenant_id AND r.job_id = j.id \
                             AND r.release_id = j.current_release_id), j.created_at) \
           FROM jobs j WHERE j.id = $1",
    )
    .bind(job)
    .fetch_one(&mut *c)
    .await?;
    let (chats, contained, escalated): (i64, i64, i64) =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT count(DISTINCT conversation_id), count(*) FILTER (WHERE {CONTAINED}), \
                    count(*) FILTER (WHERE {ESCALATED}) \
               FROM agent_case WHERE job_id = $1 AND env = 'test' AND started_at >= $2"
        )))
        .bind(job)
        .bind(since)
        .fetch_one(&mut *c)
        .await?;
    let mut trying: Vec<String> = Vec::new();
    for label in sqlx::query_scalar::<_, String>(
        "SELECT label FROM draft_ledger WHERE job_id = $1 ORDER BY at, id",
    )
    .bind(job)
    .fetch_all(&mut *c)
    .await?
    {
        if !trying.contains(&label) {
            trying.push(label);
        }
    }
    Ok(json!({
        "draft_rev": draft_rev,
        "since": iso(since),
        "chats": chats,
        "contained": contained,
        "escalated": escalated,
        "trying": trying,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn windows_are_whole_days_and_the_previous_one_adjoins() {
        let w = Window::ending(d("2026-10-21"), 7);
        assert_eq!(
            (w.from, w.to, w.prev_from, w.prev_to),
            (
                d("2026-10-15"),
                d("2026-10-21"),
                d("2026-10-08"),
                d("2026-10-14")
            )
        );
        let w = Window::ending(d("2026-10-21"), 30);
        assert_eq!((w.from, w.prev_to), (d("2026-09-22"), d("2026-09-21")));
    }

    #[test]
    fn days_accepts_7_and_30_only() {
        assert_eq!(days_param(None).unwrap(), 30);
        assert_eq!(days_param(Some("7")).unwrap(), 7);
        assert_eq!(days_param(Some("90")).unwrap_err().code(), "invalid_query");
    }

    #[test]
    fn moves_rank_by_the_size_of_the_change() {
        let before: BTreeMap<String, (i64, i64)> = [
            ("a".to_string(), (10, 5)),
            ("b".to_string(), (10, 5)),
            ("c".to_string(), (10, 5)),
        ]
        .into();
        let mine: BTreeMap<String, (i64, i64)> = [
            ("a".to_string(), (10, 6)),
            ("b".to_string(), (10, 1)),
            ("c".to_string(), (10, 5)),
            ("new".to_string(), (4, 4)),
        ]
        .into();
        let m = moves(&mine, &before);
        assert_eq!(m.len(), 2, "{m:?}");
        assert_eq!(m[0]["activity"], "b");
        assert_eq!(
            (m[0]["from"].clone(), m[0]["to"].clone()),
            (json!(0.5), json!(0.1))
        );
        assert_eq!(m[1]["activity"], "a");
    }

    #[test]
    fn reason_labels_cover_codes_and_fall_back_to_words() {
        assert_eq!(
            reason_label("customer_asked"),
            "Customer asked for a person"
        );
        assert_eq!(reason_label("upstream_unavailable"), "Upstream unavailable");
    }
}
