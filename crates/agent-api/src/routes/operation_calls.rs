//! The operation-call API (B4 T13, J20 + J22): `operationCalls`, `operationStats`, `unknownOutcomes` and
//! `checkUnknownOutcome` over `operation_call` / `operation_call_check` (0062). Reads are any member;
//! `checkUnknownOutcome` is `operator` (plan "Global constraints").
//!
//! - **Enrichment:** `side_effect`, `idempotency` and `verify_read` come from the job's operation catalogue
//!   ([`PinnedCatalog`], built from the job's pinned template compile at its Live selection); `handling` comes from
//!   the compile's `catalogue.error_states` (`user_facing` → `reply`, anything else → `escalate`; `unknown_outcome`
//!   always escalates, an `ok` or `pending` call has none). A job whose template does not compile is enriched from
//!   the static built-ins, with an empty taxonomy (every failure escalates).
//! - **Columns → UI names:** `operation` → `op`, `instance_id` → `instance`, `outcome_code` (else the state) →
//!   `outcome`, `started_at` → `at`.
//! - **`operationStats`:** the last 30 days per `(op, route, instance)`: calls, errors (`error` + `unknown_outcome`),
//!   denied, `unknown`, p95 latency (`percentile_disc`, an observed value) and `error_codes` with their handling.
//! - **`checkUnknownOutcome`:** records the person's verdict (one per call; a second is 409 `already_checked`), a
//!   `state` event on the case (ids and codes, K50: the free note stays in `operation_call_check`) and clears the
//!   case's `unknown_outcome` alert once no other unknown outcome of the case is unchecked. It never retries the
//!   call: re-sending is a person's separate decision (12 §3.3 step 10).
use std::collections::{BTreeMap, BTreeSet};

use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::audit::{self, Event};
use crate::conversations::routes::case_row;
use crate::conversations::{iso, store};
use crate::design::{DesignOf, JobRef, Registry};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::platform::templates::{self, TemplatePin};
use crate::runtime::catalog::{OpDef, OperationCatalog, PinnedCatalog};
use crate::state::AppState;
use crate::tenancy::{self, JobCtx, JobMember, JobRequires, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/operation-calls", get(operation_calls))
        .route(
            "/api/jobs/{job}/operation-calls/unknown",
            get(unknown_outcomes),
        )
        .route(
            "/api/jobs/{job}/operation-calls/{call_id}/check",
            post(check_unknown_outcome),
        )
        .route("/api/jobs/{job}/operation-stats", get(operation_stats))
}

/// One `operationCalls` page.
pub const CALLS_PAGE: i64 = 25;
/// The most `unknownOutcomes` rows one read returns.
pub const UNKNOWN_MAX: i64 = 100;
/// The longest stored check note (0062 CHECK).
const NOTE_MAX: usize = 2000;

// ---- enrichment -------------------------------------------------------------------------------------------------

/// The job's catalogue and error taxonomy, as the calls are enriched with.
struct Enrich {
    catalog: PinnedCatalog,
    /// The error-state codes the agent answers the customer with (`user_facing`).
    user_facing: BTreeSet<String>,
}

impl Enrich {
    /// From the job's pinned template compile at the Live selection; the built-ins and no taxonomy when the template
    /// does not compile.
    async fn load(c: &mut PgConnection, reg: &Registry, j: &JobCtx) -> ApiResult<Enrich> {
        let job = JobRef {
            id: j.job_id,
            key: j.job_key.clone(),
        };
        // R15: the Live selection, a pre-KH release's legacy shape upgraded in memory
        let selection = crate::design::read::selection(&mut *c, reg, &job, &DesignOf::Live).await?;
        let pins: Value = sqlx::query_scalar("SELECT pins FROM jobs WHERE id = $1")
            .bind(j.job_id)
            .fetch_optional(&mut *c)
            .await?
            .ok_or_else(|| ApiError::Missing("job_not_found", "No such job.".into()))?;
        let pins: Vec<TemplatePin> = serde_json::from_value(pins).unwrap_or_default();
        let job_key = crate::release::compile::job_key(&pins);
        let mut e = match templates::compile_pinned(&mut *c, &pins, &selection, job_key).await {
            Ok(Ok(compiled)) => Ok(Enrich {
                catalog: PinnedCatalog::from_rows(&compiled.catalogue.operations),
                user_facing: compiled
                    .catalogue
                    .error_states
                    .iter()
                    .filter(|e| e.fields.get("user_facing") == Some(&Value::Bool(true)))
                    .map(|e| e.key.clone())
                    .collect(),
            }),
            Ok(Err(_)) | Err(ApiError::Conflict("template_not_tagged", _)) => Ok(Enrich {
                catalog: PinnedCatalog::builtins_only(),
                user_facing: BTreeSet::new(),
            }),
            Err(e) => Err(e),
        }?;
        // 0246: the job's own operations, as the runtime has them
        let own =
            crate::job_design::targets::job_ops::of_design(&mut *c, reg, &job, &DesignOf::Live)
                .await?;
        for (op, j) in own {
            if e.catalog.get(&op).is_none() {
                e.catalog.put_row(&j.row(&op));
            }
        }
        Ok(e)
    }

    /// The catalogue entry of `op`: exact, else its base operation (`case.create{return}` → `case.create`).
    fn def(&self, op: &str) -> Option<&OpDef> {
        self.catalog.get(op).or_else(|| {
            op.split_once('{')
                .and_then(|(base, _)| self.catalog.get(base))
        })
    }

    /// `invalid_state:in_transit` → `reply` when its base code is user-facing, else `escalate` (web `handlingOf`).
    fn handling_of(&self, code: &str) -> &'static str {
        let base = code.split(':').next().unwrap_or("").trim();
        if self.user_facing.contains(base) {
            "reply"
        } else {
            "escalate"
        }
    }

    /// The handling of one call (web `callHandling`): none for `ok` / `pending`.
    fn call_handling(&self, state: &str, outcome: &str) -> Option<&'static str> {
        match state {
            "ok" | "pending" => None,
            "unknown_outcome" => Some("escalate"),
            _ => Some(self.handling_of(outcome)),
        }
    }

    /// The user-facing codes, for the SQL `handling` filter.
    fn reply_codes(&self) -> Vec<String> {
        self.user_facing.iter().cloned().collect()
    }
}

// ---- the call rows ----------------------------------------------------------------------------------------------

/// One stored call with its case status and check.
#[derive(sqlx::FromRow)]
struct CallRow {
    call_id: String,
    case_id: Option<String>,
    operation: String,
    route: String,
    instance_id: Option<String>,
    template: Option<String>,
    env: String,
    state: String,
    outcome: String,
    latency_ms: Option<i64>,
    started_at: DateTime<Utc>,
    initiator: String,
    attempt: i32,
    confirm_ref: Option<String>,
    detail: Option<String>,
    release_id: Option<String>,
    draft_rev: Option<i64>,
    case_status: Option<String>,
    verdict: Option<String>,
    check_note: Option<String>,
    check_by: Option<String>,
    check_at: Option<DateTime<Utc>>,
}

/// What a call read selects (every filter optional).
#[derive(Default)]
struct Filter {
    op: Option<String>,
    case_id: Option<String>,
    route: Option<String>,
    outcome: Option<String>,
    env: Option<String>,
    instance: Option<String>,
    handling: Option<String>,
    before: Option<String>,
    call_id: Option<String>,
    /// Unchecked calls first (`unknownOutcomes`), then newest first.
    unchecked_first: bool,
    limit: i64,
}

/// The call read behind every route. The `handling` filter (`$8`) reads the outcome's base code against the
/// user-facing codes (`$9`) exactly as [`Enrich::handling_of`] does.
const CALLS_SQL: &str = "\
SELECT oc.call_id, oc.case_id, oc.operation, oc.route, oc.instance_id, oc.template, oc.env, oc.state, \
       COALESCE(oc.outcome_code, oc.state) AS outcome, oc.latency_ms, oc.started_at, oc.initiator, oc.attempt, \
       oc.confirm_ref, oc.detail, oc.release_id, oc.draft_rev, ac.status AS case_status, \
       k.verdict, k.note AS check_note, k.by_sub AS check_by, k.at AS check_at \
  FROM operation_call oc \
  LEFT JOIN agent_case ac \
         ON ac.tenant_id = oc.tenant_id AND ac.job_id = oc.job_id AND ac.case_id = oc.case_id \
  LEFT JOIN operation_call_check k \
         ON k.tenant_id = oc.tenant_id AND k.job_id = oc.job_id AND k.call_id = oc.call_id \
 WHERE oc.job_id = $1 \
   AND ($2::text IS NULL OR oc.operation = $2 OR starts_with(oc.operation, $2 || '{')) \
   AND ($3::text IS NULL OR oc.case_id = $3) \
   AND ($4::text IS NULL OR oc.route = $4) \
   AND ($5::text IS NULL OR oc.state = $5 OR oc.outcome_code = $5) \
   AND ($6::text IS NULL OR oc.env = $6) \
   AND ($7::text IS NULL OR oc.instance_id = $7) \
   AND ($8::text IS NULL \
        OR ($8 = 'escalate' AND (oc.state = 'unknown_outcome' OR (oc.state IN ('error', 'denied') \
            AND NOT btrim(split_part(COALESCE(oc.outcome_code, oc.state), ':', 1)) = ANY($9::text[])))) \
        OR ($8 = 'reply' AND oc.state IN ('error', 'denied') \
            AND btrim(split_part(COALESCE(oc.outcome_code, oc.state), ':', 1)) = ANY($9::text[]))) \
   AND ($10::text IS NULL OR (oc.started_at, oc.call_id) < \
        (SELECT b.started_at, b.call_id FROM operation_call b WHERE b.job_id = $1 AND b.call_id = $10)) \
   AND ($11::text IS NULL OR oc.call_id = $11) \
 ORDER BY CASE WHEN $12 THEN k.call_id IS NOT NULL ELSE false END, oc.started_at DESC, oc.call_id DESC \
 LIMIT $13";

async fn calls(
    c: &mut PgConnection,
    j: &JobCtx,
    en: &Enrich,
    f: &Filter,
) -> ApiResult<Vec<CallRow>> {
    Ok(sqlx::query_as::<_, CallRow>(CALLS_SQL)
        .bind(j.job_id)
        .bind(f.op.clone())
        .bind(f.case_id.clone())
        .bind(f.route.clone())
        .bind(f.outcome.clone())
        .bind(f.env.clone())
        .bind(f.instance.clone())
        .bind(f.handling.clone())
        .bind(en.reply_codes())
        .bind(f.before.clone())
        .bind(f.call_id.clone())
        .bind(f.unchecked_first)
        .bind(f.limit)
        .fetch_all(&mut *c)
        .await?)
}

/// The web `OperationCall` of a row (the check's `by_name` from the members' names).
async fn view(c: &mut PgConnection, en: &Enrich, r: &CallRow) -> ApiResult<Value> {
    let def = en.def(&r.operation);
    let check = match (&r.verdict, &r.check_by, r.check_at) {
        (Some(verdict), Some(by), Some(at)) => Some(json!({
            "call_id": r.call_id,
            "verdict": verdict,
            "note": r.check_note.clone().unwrap_or_default(),
            "by": by,
            "by_name": store::member_name(&mut *c, by).await?,
            "at": iso(at),
        })),
        _ => None,
    };
    let mut v = json!({
        "call_id": r.call_id,
        "case_id": r.case_id.clone().unwrap_or_default(),
        "op": r.operation,
        "route": r.route,
        "instance": r.instance_id,
        "template": r.template,
        "env": r.env,
        "state": r.state,
        "outcome": r.outcome,
        "latency_ms": r.latency_ms,
        "at": iso(r.started_at),
        "initiator": r.initiator,
        "attempt": r.attempt,
        "side_effect": def.map(|d| serde_json::to_value(d.side_effect).unwrap_or(Value::Null)),
        "handling": en.call_handling(&r.state, &r.outcome),
        "idempotency": def.map(|d| serde_json::to_value(d.idempotency).unwrap_or(Value::Null)),
        "confirm_ref": r.confirm_ref,
        "verify_read": def.and_then(|d| d.verify_read.clone()),
        "detail": r.detail,
        "draft_rev": r.draft_rev,
        "case_status": r.case_status,
        "check": check,
    });
    if let Some(rel) = &r.release_id {
        v["release_id"] = Value::String(rel.clone());
    }
    Ok(v)
}

/// A query value: absent or blank → `None`.
fn given(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

// ---- operationCalls ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct CallsQuery {
    op: Option<String>,
    case_id: Option<String>,
    route: Option<String>,
    outcome: Option<String>,
    env: Option<String>,
    instance: Option<String>,
    handling: Option<String>,
    before: Option<String>,
    limit: Option<String>,
}

/// `operationCalls`: newest first; `next` is the last row's `call_id` when another page follows.
async fn operation_calls(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<CallsQuery>,
) -> ApiResult<Json<Value>> {
    let limit = crate::page::limit(q.limit.as_deref(), CALLS_PAGE, 100)?;
    let f = Filter {
        op: given(q.op),
        case_id: given(q.case_id),
        route: given(q.route),
        outcome: given(q.outcome),
        env: given(q.env),
        instance: given(q.instance),
        handling: given(q.handling),
        before: given(q.before),
        limit: limit + 1,
        ..Filter::default()
    };
    let mut tx = j.begin(&st.db).await?;
    let en = Enrich::load(&mut tx, &st.design, &j).await?;
    let mut rows = calls(&mut tx, &j, &en, &f).await?;
    let more = rows.len() as i64 > limit;
    rows.truncate(usize::try_from(limit).unwrap_or(0));
    let mut items = Vec::with_capacity(rows.len());
    for r in &rows {
        items.push(view(&mut tx, &en, r).await?);
    }
    tx.commit().await?;
    let next = if more {
        rows.last().map(|r| r.call_id.clone())
    } else {
        None
    };
    Ok(Json(crate::page::paged(items, next)))
}

// ---- operationStats ---------------------------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct StatRow {
    operation: String,
    route: String,
    instance_id: Option<String>,
    template: Option<String>,
    calls: i64,
    errors: i64,
    denied: i64,
    unknown: i64,
    p95_ms: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct CodeRow {
    operation: String,
    route: String,
    instance_id: Option<String>,
    code: String,
    n: i64,
}

/// `operationStats`: the last 30 days per `(op, route, instance)`.
async fn operation_stats(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let en = Enrich::load(&mut tx, &st.design, &j).await?;
    let stats = sqlx::query_as::<_, StatRow>(
        "SELECT operation, route, instance_id, \
                (array_agg(template ORDER BY started_at DESC))[1] AS template, \
                count(*) AS calls, \
                count(*) FILTER (WHERE state IN ('error', 'unknown_outcome')) AS errors, \
                count(*) FILTER (WHERE state = 'denied') AS denied, \
                count(*) FILTER (WHERE state = 'unknown_outcome') AS unknown, \
                percentile_disc(0.95) WITHIN GROUP (ORDER BY latency_ms) AS p95_ms \
           FROM operation_call \
          WHERE job_id = $1 AND started_at >= now() - interval '30 days' \
          GROUP BY operation, route, instance_id \
          ORDER BY operation, route, instance_id NULLS FIRST",
    )
    .bind(j.job_id)
    .fetch_all(&mut *tx)
    .await?;
    let codes = sqlx::query_as::<_, CodeRow>(
        "SELECT operation, route, instance_id, \
                CASE WHEN state = 'unknown_outcome' THEN 'unknown_outcome' \
                     ELSE COALESCE(outcome_code, state) END AS code, \
                count(*) AS n \
           FROM operation_call \
          WHERE job_id = $1 AND started_at >= now() - interval '30 days' \
            AND state IN ('error', 'unknown_outcome') \
          GROUP BY 1, 2, 3, 4 \
          ORDER BY 1, 2, 3, n DESC, code",
    )
    .bind(j.job_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut by: BTreeMap<(String, String, Option<String>), Vec<Value>> = BTreeMap::new();
    for c in codes {
        let handling = if c.code == "unknown_outcome" {
            "escalate"
        } else {
            en.handling_of(&c.code)
        };
        by.entry((c.operation, c.route, c.instance_id))
            .or_default()
            .push(json!({"code": c.code, "count": c.n, "handling": handling}));
    }
    let items = stats
        .into_iter()
        .map(|s| {
            let side_effect = en
                .def(&s.operation)
                .map(|d| serde_json::to_value(d.side_effect).unwrap_or(Value::Null));
            let error_codes = by
                .remove(&(s.operation.clone(), s.route.clone(), s.instance_id.clone()))
                .unwrap_or_default();
            json!({
                "op": s.operation,
                "route": s.route,
                "instance": s.instance_id,
                "template": s.template,
                "calls": s.calls,
                "errors": s.errors,
                "denied": s.denied,
                "unknown": s.unknown,
                "p95_ms": s.p95_ms,
                "side_effect": side_effect,
                "error_codes": error_codes,
            })
        })
        .collect();
    Ok(Json(crate::page::paged(items, None)))
}

// ---- unknownOutcomes --------------------------------------------------------------------------------------------

/// `unknownOutcomes`: every `unknown_outcome` call of a case, unchecked first, then newest first.
async fn unknown_outcomes(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let en = Enrich::load(&mut tx, &st.design, &j).await?;
    let f = Filter {
        outcome: Some("unknown_outcome".into()),
        unchecked_first: true,
        limit: UNKNOWN_MAX,
        ..Filter::default()
    };
    let rows = calls(&mut tx, &j, &en, &f).await?;
    let mut items = Vec::with_capacity(rows.len());
    for r in rows.iter().filter(|r| r.state == "unknown_outcome") {
        let Some(case_id) = &r.case_id else {
            continue;
        };
        let call = view(&mut tx, &en, r).await?;
        let case = case_row(&mut tx, &j, case_id).await?;
        items.push(json!({"call": call, "case": case}));
    }
    tx.commit().await?;
    Ok(Json(crate::page::paged(items, None)))
}

// ---- checkUnknownOutcome ----------------------------------------------------------------------------------------

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Verdict {
    Landed,
    NotLanded,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Landed => "landed",
            Verdict::NotLanded => "not_landed",
        }
    }
}

/// The call a check is about.
#[derive(sqlx::FromRow)]
struct CheckedCall {
    state: String,
    case_id: Option<String>,
    operation: String,
    instance_id: Option<String>,
}

#[derive(Deserialize)]
struct CheckBody {
    verdict: Verdict,
    #[serde(default)]
    note: String,
}

/// `checkUnknownOutcome` (operator): the check, a `state` event on the case and the cleared alert. Never retries.
async fn check_unknown_outcome(
    JobRequires(j, _): JobRequires<role::Operator>,
    State(st): State<AppState>,
    ApiPath((_, call_id)): ApiPath<(String, String)>,
    ApiJson(b): ApiJson<CheckBody>,
) -> ApiResult<Json<Value>> {
    store::require_clean([b.note.as_str()])?;
    let note: String = b.note.trim().chars().take(NOTE_MAX).collect();
    let verdict = b.verdict.as_str();
    let mut tx = j.begin(&st.db).await?;
    // lock order: the tenants row first (the audit insert's FK takes it), then the case
    tenancy::lock_tenant_row(&mut tx, j.t.tenant).await?;
    let CheckedCall {
        state,
        case_id,
        operation,
        instance_id,
    } = sqlx::query_as::<_, CheckedCall>(
        "SELECT state, case_id, operation, instance_id FROM operation_call WHERE call_id = $1",
    )
    .bind(call_id.clone())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| {
        ApiError::Missing(
            "call_not_found",
            format!("This job has no operation call {call_id}."),
        )
    })?;
    if state != "unknown_outcome" {
        return Err(ApiError::Conflict(
            "not_unknown_outcome",
            format!("{call_id} ended {state}; only unknown outcomes are checked."),
        ));
    }
    let status: Option<String> = match &case_id {
        Some(id) => Some(
            sqlx::query_scalar("SELECT status FROM agent_case WHERE case_id = $1 FOR UPDATE")
                .bind(id.clone())
                .fetch_one(&mut *tx)
                .await?,
        ),
        None => None,
    };
    crate::members::names::actor_seen(&mut tx, &j.t.caller).await?;
    let who = store::member_name(&mut tx, &j.t.caller.sub).await?;
    // one check per call: a concurrent second check loses the PK race and answers the same 409
    let stored: Option<String> = sqlx::query_scalar(
        "INSERT INTO operation_call_check (tenant_id, job_id, call_id, verdict, note, by_sub) \
         VALUES (app_current_tenant(), $1, $2, $3, NULLIF($4, ''), $5) \
         ON CONFLICT (tenant_id, job_id, call_id) DO NOTHING RETURNING call_id",
    )
    .bind(j.job_id)
    .bind(call_id.clone())
    .bind(verdict)
    .bind(note)
    .bind(j.t.caller.sub.clone())
    .fetch_optional(&mut *tx)
    .await?;
    if stored.is_none() {
        return Err(ApiError::Conflict(
            "already_checked",
            format!("{call_id} was already checked."),
        ));
    }
    if let (Some(case_id), Some(status)) = (&case_id, &status) {
        let sys = instance_id.as_deref().unwrap_or("the system");
        let said = match b.verdict {
            Verdict::Landed => {
                format!("{who} checked {sys}: {operation} ({call_id}) went through.")
            }
            Verdict::NotLanded => format!(
                "{who} checked {sys}: {operation} ({call_id}) did not go through; safe to send again."
            ),
        };
        sqlx::query(
            "INSERT INTO case_event (tenant_id, job_id, case_id, kind, body, by_sub) \
             VALUES (app_current_tenant(), $1, $2, 'state', $3, $4)",
        )
        .bind(j.job_id)
        .bind(case_id.clone())
        .bind(json!({
            "status": status,
            "check": verdict,
            "call_id": call_id,
            "operation": operation,
            "note": said,
            "by": who,
        }))
        .bind(j.t.caller.sub.clone())
        .execute(&mut *tx)
        .await?;
        // the alert stays while another unknown outcome of the case is unchecked
        sqlx::query(
            "UPDATE agent_case SET alert = NULL, updated_at = now() \
              WHERE case_id = $1 AND alert = 'unknown_outcome' \
                AND NOT EXISTS ( \
                    SELECT 1 FROM operation_call oc \
                     WHERE oc.case_id = $1 AND oc.state = 'unknown_outcome' \
                       AND NOT EXISTS (SELECT 1 FROM operation_call_check k \
                                        WHERE k.tenant_id = oc.tenant_id AND k.job_id = oc.job_id \
                                          AND k.call_id = oc.call_id))",
        )
        .bind(case_id.clone())
        .execute(&mut *tx)
        .await?;
    }
    let mut ev = Event::new("operation_call.check", "operation_call", call_id.clone())
        .job(&j.job_key)
        .detail("verdict", verdict)
        .detail("operation", operation.clone());
    if let Some(id) = &case_id {
        ev = ev.detail("case_id", id.clone());
    }
    audit::record(&mut tx, &j.t.actor(), ev).await?;
    let en = Enrich::load(&mut tx, &st.design, &j).await?;
    let f = Filter {
        call_id: Some(call_id.clone()),
        limit: 1,
        ..Filter::default()
    };
    let rows = calls(&mut tx, &j, &en, &f).await?;
    let row = rows
        .first()
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("checked call {call_id} is gone")))?;
    let call = view(&mut tx, &en, row).await?;
    let case = match &case_id {
        Some(id) => case_row(&mut tx, &j, id).await?,
        None => Value::Null,
    };
    tx.commit().await?;
    Ok(Json(json!({"call": call, "case": case})))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enrich(user_facing: &[&str]) -> Enrich {
        Enrich {
            catalog: PinnedCatalog::builtins_only(),
            user_facing: user_facing.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn handling_reads_the_base_code_and_unknown_always_escalates() {
        let en = enrich(&["not_found", "invalid_state"]);
        assert_eq!(en.handling_of("invalid_state:in_transit"), "reply");
        assert_eq!(en.handling_of("not_found"), "reply");
        assert_eq!(en.handling_of("timeout"), "escalate");
        assert_eq!(en.call_handling("ok", "ok"), None);
        assert_eq!(en.call_handling("pending", "pending"), None);
        assert_eq!(
            en.call_handling("unknown_outcome", "not_found"),
            Some("escalate")
        );
        assert_eq!(en.call_handling("denied", "not_found"), Some("reply"));
        assert_eq!(en.call_handling("error", "timeout"), Some("escalate"));
    }

    #[test]
    fn a_variant_falls_back_to_its_base_operation() {
        let en = enrich(&[]);
        assert!(en.def("case.create{handover}").is_some());
        assert!(en.def("knowledge.search{faq}").is_some());
        assert!(en.def("nothing.here{x}").is_none());
    }
}
