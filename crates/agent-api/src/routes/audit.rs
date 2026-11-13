//! The audit logs (B1 T16; P3 T12): the tenant log for its members and the platform log for super_admins.
//!
//! - `GET /api/audit?q=&actor_type=&action=&job=&status=&before=&limit=&view=&family=&actor=` — any member:
//!   `Paged<AuditEvent>`, newest first, `limit` ≤ 200 (default 50), `before` = the id cursor. `action` is exact or a
//!   prefix (`user` matches `user.*`); `view=calls` = agent operation calls (`execute`), `changes` = everything else;
//!   `family` = a T4 family (crate::audit::read::TENANT_FAMILIES, `calls`, `other`); `q` matches actor, action,
//!   resource, names and detail text. Failed rows carry `outcome`: resolved by the first later success (not a call)
//!   on the same resource, else open; a failed call is open only when it raised an alert (`details.alert = true`).
//! - `GET /api/audit/summary` — any member: whole-log `AuditSummary`.
//! - `GET /api/platform/audit?action=&status=&family=&actor=&q=&tenant=&since=&before=&limit=` and `/summary` —
//!   super_admin, read as `agent_platform`: `Paged<PlatformAuditEvent>` / `PlatformAuditSummary`. `tenant` = the
//!   subject tenant (a uuid; 422 `invalid_tenant_id`), `since` = a date or RFC 3339 timestamp, inclusive, UTC (422
//!   `invalid_since`) (B1b F-04); `/summary` is the whole log. A failed event is resolved by a later
//!   `tenant.import|tenant.create|signup.approve` in the same thread (the subject label without a trailing
//!   parenthesis, signup and creation families only).
use std::collections::HashMap;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::audit::read::{
    AuditRow, PLATFORM_FAMILIES, ROW_SELECT, TENANT_FAMILIES, event_json, ilike, open_outcome,
    platform_family, resolved_outcome, tenant_family,
};
use crate::error::{ApiError, ApiQuery, ApiResult};
use crate::page::{id_cursor, limit, paged};
use crate::platform_db::begin_platform;
use crate::state::AppState;
use crate::tenancy::{Member, SuperAdmin};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/audit", get(list))
        .route("/api/audit/summary", get(summary))
        .route("/api/platform/audit", get(platform_list))
        .route("/api/platform/audit/summary", get(platform_summary))
}

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;

fn invalid(code: &'static str, msg: &str) -> ApiError {
    ApiError::Unprocessable(code, msg.into())
}

/// An empty query value is "not given".
fn given(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

#[derive(Deserialize)]
struct Query {
    q: Option<String>,
    actor_type: Option<String>,
    action: Option<String>,
    job: Option<String>,
    status: Option<String>,
    before: Option<String>,
    limit: Option<String>,
    view: Option<String>,
    family: Option<String>,
    actor: Option<String>,
}

/// The rows of the scoped tenant matching the filters (newest first, at most `limit`).
const LIST_WHERE: &str = " WHERE ($1::text IS NULL OR e.actor_type = $1) \
   AND ($2::text IS NULL OR e.action = $2 OR left(e.action, length($2) + 1) = $2 || '.') \
   AND ($3::text IS NULL OR e.job = $3) \
   AND ($4::text IS NULL OR e.status = $4) \
   AND ($5::bigint IS NULL OR e.id < $5) \
   AND ($6::text IS NULL OR (($6 = 'calls') = (e.action = 'execute'))) \
   AND ($7::text IS NULL \
        OR ($7 = 'calls' AND e.action = 'execute') \
        OR ($7 = 'in' AND e.action <> 'execute' AND EXISTS ( \
              SELECT 1 FROM unnest($8::text[]) p (x) WHERE e.action = p.x OR left(e.action, length(p.x) + 1) = p.x || '.')) \
        OR ($7 = 'other' AND e.action <> 'execute' AND NOT EXISTS ( \
              SELECT 1 FROM unnest($8::text[]) p (x) WHERE e.action = p.x OR left(e.action, length(p.x) + 1) = p.x || '.'))) \
   AND ($9::text IS NULL OR e.actor_id = $9) \
   AND ($10::text IS NULL OR e.actor_id ILIKE $10 OR e.action ILIKE $10 OR e.resource_id ILIKE $10 \
        OR e.details::text ILIKE $10 OR a.display_name ILIKE $10 OR r.display_name ILIKE $10) \
 ORDER BY e.id DESC LIMIT $11";

/// Outcomes of the failed rows among `rows` (by row id).
async fn outcomes(conn: &mut PgConnection, rows: &[AuditRow]) -> ApiResult<HashMap<i64, Value>> {
    let mut out = HashMap::new();
    let mut ids = Vec::new();
    for r in rows.iter().filter(|r| r.failed()) {
        if r.is_call() {
            if r.details.get("alert") == Some(&Value::Bool(true)) {
                out.insert(r.id, open_outcome());
            }
        } else {
            ids.push(r.id);
        }
    }
    if ids.is_empty() {
        return Ok(out);
    }
    type Fix = (i64, i64, String, DateTime<Utc>);
    let fixes: Vec<Fix> = sqlx::query_as(
        "SELECT DISTINCT ON (f.id) f.id, s.id, s.action, s.at \
           FROM audit_event f \
           JOIN audit_event s ON s.resource_id = f.resource_id AND s.status = 'success' \
                             AND s.action <> 'execute' AND s.id > f.id \
          WHERE f.id = ANY ($1) \
          ORDER BY f.id, s.id",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    for id in ids {
        let v = match fixes.iter().find(|(f, _, _, _)| *f == id) {
            Some((_, by, action, at)) => resolved_outcome(*by, action, *at),
            None => open_outcome(),
        };
        out.insert(id, v);
    }
    Ok(out)
}

async fn list(
    Member(t): Member,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<Query>,
) -> ApiResult<Json<Value>> {
    let actor_type = given(&q.actor_type);
    if actor_type.is_some_and(|a| !matches!(a, "human" | "ai_agent" | "system")) {
        return Err(invalid(
            "invalid_query",
            "actor_type must be human, ai_agent or system",
        ));
    }
    let status = given(&q.status);
    if status.is_some_and(|s| !crate::readmodel::home::STATUSES.contains(&s)) {
        return Err(invalid(
            "invalid_query",
            "status must be success, failure, denied, error or fallback",
        ));
    }
    let view = given(&q.view);
    if view.is_some_and(|v| !matches!(v, "calls" | "changes")) {
        return Err(invalid("invalid_view", "view must be changes or calls"));
    }
    let (mode, prefixes): (Option<&str>, Vec<String>) = match given(&q.family) {
        None => (None, Vec::new()),
        Some("calls") => (Some("calls"), Vec::new()),
        Some("other") => (
            Some("other"),
            TENANT_FAMILIES
                .iter()
                .flat_map(|(_, ps)| ps.iter().map(|p| p.to_string()))
                .collect(),
        ),
        Some(f) => match TENANT_FAMILIES.iter().find(|(k, _)| *k == f) {
            Some((_, ps)) => (Some("in"), ps.iter().map(|p| p.to_string()).collect()),
            None => {
                return Err(invalid(
                    "invalid_family",
                    "family must be access, releases, connections, knowledge, platform, calls or other",
                ));
            }
        },
    };
    let lim = limit(q.limit.as_deref(), DEFAULT_LIMIT, MAX_LIMIT)?;
    let before = id_cursor(q.before.as_deref())?;
    let job = given(&q.job);
    let pattern = given(&q.q).map(ilike);
    let mut tx = t.begin(&st.db).await?;
    if let Some(j) = job {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM jobs WHERE key = $1)")
            .bind(j)
            .fetch_one(&mut *tx)
            .await?;
        if !exists {
            return Err(ApiError::Missing("job_not_found", format!("no job {j:?}")));
        }
    }
    let rows: Vec<AuditRow> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!("{ROW_SELECT}{LIST_WHERE}")))
            .bind(actor_type)
            .bind(given(&q.action))
            .bind(job)
            .bind(status)
            .bind(before)
            .bind(view)
            .bind(mode)
            .bind(&prefixes)
            .bind(given(&q.actor))
            .bind(pattern)
            .bind(lim + 1)
            .fetch_all(&mut *tx)
            .await?;
    let more = rows.len() as i64 > lim;
    let page: Vec<AuditRow> = rows
        .into_iter()
        .take(usize::try_from(lim).unwrap_or(0))
        .collect();
    let mut out = outcomes(&mut tx, &page).await?;
    tx.commit().await?;
    let next = if more {
        page.last().map(|r| r.id.to_string())
    } else {
        None
    };
    let items = page
        .iter()
        .map(|r| event_json(r, out.remove(&r.id)))
        .collect();
    Ok(Json(paged(items, next)))
}

/// Failed rows of the whole log that are still open (newest first).
const OPEN_WHERE: &str = " WHERE e.status IN ('failure', 'error') AND ( \
        (e.action = 'execute' AND e.details->>'alert' = 'true') \
     OR (e.action <> 'execute' AND NOT EXISTS ( \
            SELECT 1 FROM audit_event s WHERE s.resource_id = e.resource_id AND s.status = 'success' \
               AND s.action <> 'execute' AND s.id > e.id)))";

async fn summary(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let open: Vec<AuditRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{ROW_SELECT}{OPEN_WHERE} ORDER BY e.id DESC"
    )))
    .fetch_all(&mut *tx)
    .await?;
    let by_action: Vec<(String, i64)> =
        sqlx::query_as("SELECT action, count(*) FROM audit_event GROUP BY action")
            .fetch_all(&mut *tx)
            .await?;
    let calls: Vec<(String, i64)> = sqlx::query_as(
        "SELECT status, count(*) FROM audit_event WHERE action = 'execute' GROUP BY status",
    )
    .fetch_all(&mut *tx)
    .await?;
    type Actor = (String, Option<String>, String, bool, i64);
    let actors: Vec<Actor> = sqlx::query_as(
        "SELECT e.actor_id, max(coalesce(nullif(btrim(a.display_name), ''), nullif(btrim(a.email), ''))), \
                e.actor_type, bool_or(e.platform_actor), count(*) \
           FROM audit_event e LEFT JOIN users a ON a.sub = e.actor_id \
          GROUP BY e.actor_id, e.actor_type ORDER BY count(*) DESC, e.actor_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let total: i64 = by_action.iter().map(|(_, n)| n).sum();
    let call = |s: &str| {
        calls
            .iter()
            .filter(|(status, _)| status == s)
            .map(|(_, n)| *n)
            .sum::<i64>()
    };
    let calls_total: i64 = calls.iter().map(|(_, n)| n).sum();
    let families: Vec<Value> = TENANT_FAMILIES
        .iter()
        .map(|(f, _)| {
            let count: i64 = by_action
                .iter()
                .filter(|(a, _)| tenant_family(a) == *f)
                .map(|(_, n)| n)
                .sum();
            let open_n = open
                .iter()
                .filter(|r| !r.is_call() && tenant_family(&r.action) == *f)
                .count();
            json!({"family": f, "count": count, "open": open_n})
        })
        .collect();
    Ok(Json(json!({
        "total": total,
        "changes": total - calls_total,
        "calls": {
            "total": calls_total,
            "success": call("success"),
            "denied": call("denied"),
            "error": call("error") + call("failure"),
            "fallback": call("fallback"),
        },
        "open": open.iter().take(20).map(|r| event_json(r, Some(open_outcome()))).collect::<Vec<_>>(),
        "families": families,
        "actors": actors
            .into_iter()
            .map(|(id, name, kind, platform, count)| json!({
                "actor_id": id, "actor_name": name, "actor_type": kind, "platform": platform, "count": count,
            }))
            .collect::<Vec<_>>(),
    })))
}

// ---- platform log -----------------------------------------------------------------------------------------------

/// One `platform_audit` row.
#[derive(Clone, Debug, sqlx::FromRow)]
struct PlatformRow {
    id: i64,
    at: DateTime<Utc>,
    actor_id: String,
    actor_type: String,
    action: String,
    subject_tenant: Option<uuid::Uuid>,
    subject_label: String,
    status: String,
    details: Value,
}

/// Actions after which the subject IS a tenant.
const CREATES: [&str; 3] = ["tenant.import", "tenant.create", "signup.approve"];

/// The thread of an event (signup and creation families only): its label without a trailing `(…)`, lower-cased.
fn thread_key(r: &PlatformRow) -> Option<String> {
    if !matches!(platform_family(&r.action), "signup" | "creation") {
        return None;
    }
    let label = r.subject_label.trim_end();
    let base = match (label.ends_with(')'), label.rfind('(')) {
        (true, Some(i)) => &label[..i],
        _ => label,
    };
    Some(base.trim().to_lowercase())
}

/// The outcome of a failed event against the whole log.
fn platform_outcome(r: &PlatformRow, all: &[PlatformRow]) -> Option<Value> {
    if r.status != "failed" {
        return None;
    }
    let key = thread_key(r);
    let fix = key.as_ref().and_then(|k| {
        all.iter()
            .filter(|x| {
                x.status == "ok"
                    && CREATES.contains(&x.action.as_str())
                    && x.id > r.id
                    && thread_key(x).as_ref() == Some(k)
            })
            .min_by_key(|x| x.id)
    });
    Some(match fix {
        Some(f) => resolved_outcome(f.id, &f.action, f.at),
        None => open_outcome(),
    })
}

fn platform_json(r: &PlatformRow, all: &[PlatformRow]) -> Value {
    let mut v = json!({
        "id": r.id.to_string(),
        "at": r.at,
        "actor_id": r.actor_id,
        "actor_type": r.actor_type,
        "action": r.action,
        "subject_tenant": r.subject_tenant,
        "subject_label": r.subject_label,
        "status": r.status,
        "details": r.details,
        "actor_name": null,
    });
    if let Some(o) = platform_outcome(r, all) {
        v["outcome"] = o;
    }
    v
}

/// The whole platform log, oldest first (read as `agent_platform`).
async fn platform_rows(st: &AppState) -> ApiResult<Vec<PlatformRow>> {
    let mut tx = begin_platform(&st.db).await?;
    let rows = sqlx::query_as::<_, PlatformRow>(
        "SELECT id, at, actor_id, actor_type, action, subject_tenant, subject_label, status, details \
           FROM platform_audit ORDER BY id",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}

#[derive(Deserialize)]
struct PlatformQuery {
    action: Option<String>,
    status: Option<String>,
    family: Option<String>,
    actor: Option<String>,
    q: Option<String>,
    before: Option<String>,
    limit: Option<String>,
    tenant: Option<String>,
    since: Option<String>,
}

/// `?tenant=`: a bare, non-nil uuid (any tenant, archived or unknown included: it matches its history or nothing), else 422
/// `invalid_tenant_id`.
fn subject_tenant(raw: Option<&str>) -> ApiResult<Option<uuid::Uuid>> {
    raw.map(|s| {
        crate::tenancy::tenant_id(s).map_err(|_| {
            invalid(
                "invalid_tenant_id",
                "tenant must be a bare, non-nil tenant uuid",
            )
        })
    })
    .transpose()
}

/// `?since=`: an RFC 3339 timestamp, or a `YYYY-MM-DD` date (its midnight, UTC); inclusive. Else 422
/// `invalid_since`.
fn since(raw: Option<&str>) -> ApiResult<Option<DateTime<Utc>>> {
    let Some(s) = raw else {
        return Ok(None);
    };
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(Some(t.with_timezone(&Utc)));
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|t| Some(t.and_utc()))
        .ok_or_else(|| {
            invalid(
                "invalid_since",
                "since must be a date (YYYY-MM-DD) or an RFC 3339 timestamp",
            )
        })
}

async fn platform_list(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<PlatformQuery>,
) -> ApiResult<Json<Value>> {
    let family = given(&q.family);
    if family.is_some_and(|f| f != "other" && !PLATFORM_FAMILIES.iter().any(|(k, _)| *k == f)) {
        return Err(invalid(
            "invalid_family",
            "family must be signup, creation, keys, catalogue or other",
        ));
    }
    let status = given(&q.status);
    if status.is_some_and(|s| !matches!(s, "ok" | "failed")) {
        return Err(invalid("invalid_query", "status must be ok or failed"));
    }
    let lim = limit(q.limit.as_deref(), DEFAULT_LIMIT, MAX_LIMIT)?;
    let before = id_cursor(q.before.as_deref())?;
    let action = given(&q.action);
    let actor = given(&q.actor);
    let needle = given(&q.q).map(str::to_lowercase);
    let tenant = subject_tenant(given(&q.tenant))?;
    let since_at = since(given(&q.since))?;
    let all = platform_rows(&st).await?;
    let mut hits = all.iter().rev().filter(|r| {
        before.is_none_or(|b| r.id < b)
            && tenant.is_none_or(|t| r.subject_tenant == Some(t))
            && since_at.is_none_or(|s| r.at >= s)
            && action.is_none_or(|a| crate::audit::read::matches_prefix(&r.action, a))
            && status.is_none_or(|s| r.status == s)
            && family.is_none_or(|f| platform_family(&r.action) == f)
            && actor.is_none_or(|a| r.actor_id == a)
            && needle.as_deref().is_none_or(|n| {
                r.subject_label.to_lowercase().contains(n)
                    || r.action.contains(n)
                    || r.details.to_string().to_lowercase().contains(n)
            })
    });
    let take = usize::try_from(lim).unwrap_or(0);
    let page: Vec<&PlatformRow> = hits.by_ref().take(take).collect();
    let next = if hits.next().is_some() {
        page.last().map(|r| r.id.to_string())
    } else {
        None
    };
    Ok(Json(paged(
        page.iter().map(|r| platform_json(r, &all)).collect(),
        next,
    )))
}

async fn platform_summary(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let all = platform_rows(&st).await?;
    let open: Vec<&PlatformRow> = all
        .iter()
        .rev()
        .filter(|r| platform_outcome(r, &all).is_some_and(|o| o["state"] == "open"))
        .collect();
    let mut families: Vec<Value> = PLATFORM_FAMILIES
        .iter()
        .map(|(k, _)| *k)
        .chain(std::iter::once("other"))
        .map(|f| {
            let rows: Vec<&PlatformRow> = all
                .iter()
                .filter(|r| platform_family(&r.action) == f)
                .collect();
            json!({
                "family": f,
                "count": rows.len(),
                "failed": rows.iter().filter(|r| r.status == "failed").count(),
                "open": open.iter().filter(|r| platform_family(&r.action) == f).count(),
            })
        })
        .collect();
    families.retain(|f| f["family"] != "other" || f["count"] != 0);
    let mut actors: Vec<(String, String, usize)> = Vec::new();
    for r in &all {
        match actors.iter_mut().find(|(id, _, _)| *id == r.actor_id) {
            Some(a) => a.2 += 1,
            None => actors.push((r.actor_id.clone(), r.actor_type.clone(), 1)),
        }
    }
    actors.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    let mut actions: Vec<&str> = all.iter().map(|r| r.action.as_str()).collect();
    actions.sort_unstable();
    actions.dedup();
    Ok(Json(json!({
        "total": all.len(),
        "failed": all.iter().filter(|r| r.status == "failed").count(),
        "open": open.iter().map(|r| platform_json(r, &all)).collect::<Vec<_>>(),
        "families": families,
        "actors": actors
            .into_iter()
            .map(|(id, kind, count)| json!({"actor_id": id, "actor_name": null, "actor_type": kind, "count": count}))
            .collect::<Vec<_>>(),
        "actions": actions,
    })))
}
