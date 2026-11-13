//! The case reads of the runtime area (B4 T12; web `areas/runtime.ts`, J19/J20): `cases` (paged, newest first),
//! `caseload` (every case someone still holds, not paged) and `caseDetail` (the case, its timeline and its raw
//! envelope). Any member reads. The writes `handOverCase` / `claimCase` live with the handover queue
//! (crate::conversations::routes), and every case row is that module's `CaseRow` ([`case_row`]).
//!
//! The envelope carries ids and codes only (K50): no message text, no customer name, no case title, no handover
//! reason. The timeline is the case's `case_event` rows (`TimelineEvent`), operation events completed from their
//! `operation_call` row, reply events from their delivered message.
use std::collections::HashMap;

use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::get;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, Row};

use crate::conversations::routes::case_row;
use crate::conversations::{iso, store};
use crate::design::JobRef;
use crate::error::{ApiError, ApiPath, ApiQuery, ApiResult};
use crate::state::AppState;
use crate::tenancy::JobMember;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/cases", get(cases))
        .route("/api/jobs/{job}/caseload", get(caseload))
        .route("/api/jobs/{job}/cases/{id}", get(case_detail))
}

/// The eight `agent_case.status` values (0060).
const STATUSES: [&str; 8] = [
    "open",
    "identity_required",
    "confirmation_required",
    "handover",
    "resolved",
    "not_eligible",
    "failed",
    "cancelled_offboard",
];

/// A `cases` page unless `limit` says otherwise.
const PAGE: i64 = 50;

/// The most cases `caseload` answers (it is not paged; a job never holds this many at once).
const CASELOAD_MAX: i64 = 200;

fn invalid(msg: &str) -> ApiError {
    ApiError::Unprocessable("invalid_query", msg.into())
}

/// `env=prod|test|all` (absent or empty = all).
fn env_filter(raw: Option<&str>) -> ApiResult<Option<&'static str>> {
    match raw.map(str::trim) {
        None | Some("" | "all") => Ok(None),
        Some("prod") => Ok(Some("prod")),
        Some("test") => Ok(Some("test")),
        Some(_) => Err(invalid("env must be prod, test or all")),
    }
}

/// `status=<CaseStatus>|all` (absent or empty = all).
fn status_filter(raw: Option<&str>) -> ApiResult<Option<&'static str>> {
    match raw.map(str::trim) {
        None | Some("" | "all") => Ok(None),
        Some(s) => STATUSES
            .iter()
            .copied()
            .find(|x| *x == s)
            .map(Some)
            .ok_or_else(|| invalid("status must be a case status or all")),
    }
}

// ---- cases ------------------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct CasesQuery {
    status: Option<String>,
    env: Option<String>,
    q: Option<String>,
    before: Option<String>,
    limit: Option<String>,
}

/// `GET /api/jobs/{job}/cases?status=&env=&q=&before=` — newest first (`CS-n` descending). `q` matches the case id,
/// customer name, title, activity or outcome (case-insensitive substring). `next` is the case number of the last
/// row: the next page holds the older cases.
async fn cases(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<CasesQuery>,
) -> ApiResult<Json<Value>> {
    let status = status_filter(q.status.as_deref())?;
    let env = env_filter(q.env.as_deref())?;
    let before = crate::page::id_cursor(q.before.as_deref())?;
    let limit = crate::page::limit(q.limit.as_deref(), PAGE, 200)?;
    let text = q.q.as_deref().map(str::trim).unwrap_or_default();
    if text.chars().count() > 200 {
        return Err(invalid("q is at most 200 characters"));
    }
    store::require_clean([text])?;
    let needle = (!text.is_empty()).then(|| text.to_lowercase());
    let mut tx = j.begin(&st.db).await?;
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT case_id, substr(case_id, 4)::int8 AS n FROM agent_case \
          WHERE job_id = $1 \
            AND ($2::text IS NULL OR status = $2) \
            AND ($3::text IS NULL OR env = $3) \
            AND ($4::text IS NULL \
                 OR strpos(lower(case_id), $4) > 0 \
                 OR strpos(lower(COALESCE(customer_name, '')), $4) > 0 \
                 OR strpos(lower(COALESCE(title, '')), $4) > 0 \
                 OR strpos(lower(COALESCE(activity, '')), $4) > 0 \
                 OR strpos(lower(COALESCE(outcome, '')), $4) > 0) \
            AND ($5::int8 IS NULL OR substr(case_id, 4)::int8 < $5) \
          ORDER BY substr(case_id, 4)::int8 DESC \
          LIMIT $6",
    )
    .bind(j.job_id)
    .bind(status)
    .bind(env)
    .bind(needle)
    .bind(before)
    .bind(limit + 1)
    .fetch_all(&mut *tx)
    .await?;
    let more = rows.len() as i64 > limit;
    let mut items = Vec::new();
    let mut next = None;
    for (id, n) in rows.iter().take(limit as usize) {
        items.push(case_row(&mut tx, &j, id).await?);
        next = Some(n.to_string());
    }
    tx.commit().await?;
    Ok(Json(crate::page::paged(
        items,
        if more { next } else { None },
    )))
}

// ---- caseload ---------------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct CaseloadQuery {
    env: Option<String>,
}

/// A case someone still holds: not closed, or closed with an alert (an unknown outcome a person must check).
const ACTIVE: &str =
    "(status NOT IN ('resolved', 'not_eligible', 'cancelled_offboard') OR alert IS NOT NULL)";

/// `GET /api/jobs/{job}/caseload?env=` — `Caseload` (web `types/runtime.ts`).
async fn caseload(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<CaseloadQuery>,
) -> ApiResult<Json<Value>> {
    let env = env_filter(q.env.as_deref())?;
    let mut tx = j.begin(&st.db).await?;
    let as_of: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *tx)
        .await?;
    let ids: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT case_id FROM agent_case \
          WHERE job_id = $1 AND {ACTIVE} AND ($2::text IS NULL OR env = $2) \
          ORDER BY updated_at DESC, substr(case_id, 4)::int8 DESC \
          LIMIT $3"
    )))
    .bind(j.job_id)
    .bind(env)
    .bind(CASELOAD_MAX)
    .fetch_all(&mut *tx)
    .await?;
    let (prod, test): (i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FILTER (WHERE env = 'prod'), count(*) FILTER (WHERE env = 'test') \
           FROM agent_case WHERE job_id = $1 AND {ACTIVE}"
    )))
    .bind(j.job_id)
    .fetch_one(&mut *tx)
    .await?;
    let (resolved, not_eligible, offboard): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE status = 'resolved'), count(*) FILTER (WHERE status = 'not_eligible'), \
                count(*) FILTER (WHERE status = 'cancelled_offboard') \
           FROM agent_case \
          WHERE job_id = $1 AND alert IS NULL \
            AND status IN ('resolved', 'not_eligible', 'cancelled_offboard') \
            AND updated_at >= $2::timestamptz - interval '7 days'",
    )
    .bind(j.job_id)
    .bind(as_of)
    .fetch_one(&mut *tx)
    .await?;
    let mut active = Vec::with_capacity(ids.len());
    for id in &ids {
        active.push(case_row(&mut tx, &j, id).await?);
    }
    let job = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    // `AgentFleet`: the one computation `agentFleet` serves too (bus_admin::fleet_at)
    let fleet = crate::bus_admin::fleet_at(&mut tx, &job, as_of).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "as_of": iso(as_of),
        "active": active,
        "closed_7d": {"resolved": resolved, "not_eligible": not_eligible, "cancelled_offboard": offboard},
        "env_counts": {"prod": prod, "test": test},
        "fleet": fleet,
    })))
}

// ---- caseDetail -------------------------------------------------------------------------------------------------

/// `GET /api/jobs/{job}/cases/{id}` — `CaseDetail` `{case, timeline, envelope}`; 404 `case_not_found`.
async fn case_detail(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let case = case_row(&mut tx, &j, &id).await?;
    let calls = op_calls(&mut tx, &id).await?;
    let timeline = timeline(&mut tx, &id, &case, &calls).await?;
    let route_hint = timeline
        .iter()
        .rev()
        .find(|e| e["kind"] == "qualified")
        .and_then(|e| e.get("route_hint"))
        .cloned()
        .unwrap_or(Value::Null);
    let envelope = envelope(&case, &calls, &timeline, route_hint);
    tx.commit().await?;
    Ok(Json(
        json!({"case": case, "timeline": timeline, "envelope": envelope}),
    ))
}

/// The case's operation calls, oldest first, as timeline `operation` fields (`op`, `instance`, `outcome`, …).
async fn op_calls(c: &mut PgConnection, case_id: &str) -> ApiResult<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT call_id, operation, route, instance_id, template, state, outcome_code, latency_ms, detail, \
                confirm_ref, step, object_change, started_at \
           FROM operation_call WHERE case_id = $1 \
          ORDER BY started_at, call_id",
    )
    .bind(case_id)
    .fetch_all(&mut *c)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let state: String = r.get("state");
            let outcome = r
                .get::<Option<String>, _>("outcome_code")
                .unwrap_or_else(|| state.clone());
            let mut v = json!({
                "call_id": r.get::<String, _>("call_id"),
                "op": r.get::<String, _>("operation"),
                "route": r.get::<String, _>("route"),
                "instance": r.get::<Option<String>, _>("instance_id"),
                "template": r.get::<Option<String>, _>("template"),
                "state": state,
                "outcome": outcome,
                "latency_ms": r.get::<Option<i64>, _>("latency_ms"),
                "started_at": iso(r.get("started_at")),
            });
            for k in ["detail", "confirm_ref"] {
                if let Some(s) = r.get::<Option<String>, _>(k) {
                    v[k] = Value::String(s);
                }
            }
            for k in ["step", "object_change"] {
                if let Some(x) = r.get::<Option<Value>, _>(k) {
                    v[k] = x;
                }
            }
            v
        })
        .collect())
}

/// The case's `case_event` rows as `TimelineEvent`s: `{kind, at, ...body}`, with the fields the web type needs
/// filled where a writer stored them under another name (operation `operation`/`code`, alert `alert`) or not at
/// all (an operation's call row, a reply's text, a staff actor's name).
async fn timeline(
    c: &mut PgConnection,
    case_id: &str,
    case: &Value,
    calls: &[Value],
) -> ApiResult<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT kind, at, body, by_sub FROM case_event WHERE case_id = $1 ORDER BY seq",
    )
    .bind(case_id)
    .fetch_all(&mut *c)
    .await?;
    let by_call: HashMap<&str, &Value> = calls
        .iter()
        .filter_map(|v| v["call_id"].as_str().map(|id| (id, v)))
        .collect();
    let mut names: HashMap<String, String> = HashMap::new();
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let kind: String = r.get("kind");
        let at: DateTime<Utc> = r.get("at");
        let mut m: Map<String, Value> = match r.get::<Value, _>("body") {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        match kind.as_str() {
            "operation" => {
                let call = m
                    .get("call_id")
                    .and_then(Value::as_str)
                    .and_then(|id| by_call.get(id))
                    .and_then(|v| v.as_object());
                if let Some(call) = call {
                    for (k, v) in call {
                        if k != "started_at" {
                            m.insert(k.clone(), v.clone());
                        }
                    }
                }
                let op = m.get("operation").cloned().unwrap_or(json!(""));
                m.entry("op").or_insert(op);
                let outcome = m
                    .get("code")
                    .filter(|v| v.is_string())
                    .or_else(|| m.get("state"))
                    .cloned()
                    .unwrap_or(json!(""));
                m.entry("outcome").or_insert(outcome);
                for k in ["instance", "template", "latency_ms"] {
                    m.entry(k).or_insert(Value::Null);
                }
            }
            "reply" if !m.contains_key("text") => {
                let msg = m
                    .get("message_id")
                    .and_then(Value::as_str)
                    .and_then(|s| uuid::Uuid::parse_str(s).ok());
                let text: Option<String> = match msg {
                    Some(id) => {
                        sqlx::query_scalar(
                            "SELECT text FROM conversation_message WHERE message_id = $1",
                        )
                        .bind(id)
                        .fetch_optional(&mut *c)
                        .await?
                    }
                    None => None,
                };
                m.insert("text".into(), Value::String(text.unwrap_or_default()));
            }
            "state" if !m.contains_key("status") => {
                m.insert("status".into(), case["status"].clone());
            }
            "alert" => {
                let code = m.get("alert").cloned().unwrap_or(json!(""));
                m.entry("code").or_insert(code);
                m.entry("text").or_insert(json!(""));
            }
            _ => {}
        }
        if kind == "state" {
            m.entry("note").or_insert(json!(""));
        }
        if let Some(sub) = r.get::<Option<String>, _>("by_sub")
            && !m.contains_key("by")
        {
            let name = match names.get(&sub) {
                Some(n) => n.clone(),
                None => {
                    let n = store::member_name(&mut *c, &sub).await?;
                    names.insert(sub.clone(), n.clone());
                    n
                }
            };
            m.insert("by".into(), Value::String(name));
        }
        m.insert("kind".into(), Value::String(kind));
        m.insert("at".into(), Value::String(iso(at)));
        out.push(Value::Object(m));
    }
    Ok(out)
}

/// The raw envelope (web `envelopeOf`): ids and codes only (K50) — no message text, customer name, title or reason.
fn envelope(case: &Value, calls: &[Value], timeline: &[Value], route_hint: Value) -> Value {
    let op_calls: Vec<Value> = calls
        .iter()
        .map(|v| json!({"call_id": v["call_id"], "op": v["op"], "route": v["route"], "outcome": v["outcome"]}))
        .collect();
    let gates: Vec<Value> = timeline
        .iter()
        .filter(|e| e["kind"] == "gate")
        .map(|e| {
            let keys: Vec<Value> = e["verdicts"]
                .as_array()
                .map(|vs| vs.iter().map(|v| v["key"].clone()).collect())
                .unwrap_or_default();
            json!({"point": e["point"], "verdict": e["verdict"], "keys": keys})
        })
        .collect();
    json!({
        "case_id": case["case_id"],
        "conversation_id": case["conversation_id"],
        "tenant_id": case["tenant_id"],
        "job": case["job"],
        "env": case["env"],
        "channel": case["channel"],
        "release": case["release_id"],
        "draft_rev": case["draft_rev"],
        "activity": case["activity"],
        "request_type": case["request_type"],
        "identity_level": case["identity_level"],
        "status": case["status"],
        "outcome": case["outcome"],
        "escalation_reason": case["escalation_reason"],
        "alert": case["alert"],
        "route_hint": route_hint,
        "op_calls": op_calls,
        "gates": gates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_accept_the_web_values_and_refuse_others() {
        assert_eq!(env_filter(None).unwrap(), None);
        assert_eq!(env_filter(Some("all")).unwrap(), None);
        assert_eq!(env_filter(Some("test")).unwrap(), Some("test"));
        assert_eq!(env_filter(Some("dev")).unwrap_err().code(), "invalid_query");
        assert_eq!(status_filter(Some("")).unwrap(), None);
        assert_eq!(status_filter(Some("handover")).unwrap(), Some("handover"));
        assert_eq!(
            status_filter(Some("closed")).unwrap_err().code(),
            "invalid_query"
        );
    }

    #[test]
    fn envelope_carries_ids_and_codes_only() {
        let case = json!({
            "case_id": "CS-1", "conversation_id": "conv-1", "tenant_id": "t", "job": "j1", "env": "test",
            "channel": "staff", "release_id": null, "draft_rev": 3, "activity": "support:track", "request_type": "track",
            "identity_level": 1, "status": "handover", "outcome": null, "escalation_reason": "staff_decision",
            "alert": null, "title": "Where is my parcel?", "customer_name": "Femke",
        });
        let calls = vec![json!({
            "call_id": "opc_1", "op": "case.create{handover}", "route": "fallback", "outcome": "ok",
            "detail": "manual handover by Ann",
        })];
        let timeline = vec![
            json!({"kind": "message", "text": "Where is my parcel?"}),
            json!({"kind": "gate", "point": "P1", "verdict": "pass", "verdicts": [{"key": "GT", "label": "secret label"}]}),
        ];
        let e = envelope(&case, &calls, &timeline, Value::Null).to_string();
        for leaked in ["parcel", "Femke", "Ann", "secret label"] {
            assert!(!e.contains(leaked), "{leaked} in {e}");
        }
        for kept in ["CS-1", "case.create{handover}", "\"GT\"", "staff_decision"] {
            assert!(e.contains(kept), "{kept} missing from {e}");
        }
    }
}
