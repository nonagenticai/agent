//! J7 gates (B5 T8, T8a; plan "Route table"): the questions a job asks — the library at the draft's pins (never
//! copied, plan R1) and the tenant's own — with this job's refinements, 7-day stats, changes and releases. Reads are
//! the DRAFT design; every write is a D54 draft through `design::track` (one ledger row + one audit row, `draft_rev`
//! bumped), page J7, area `gates`. A port of the fixture `areas/ontology.ts` J7 methods (codes, messages, labels).
//!
//! | Method | Route | Role | Ledger kind |
//! |---|---|---|---|
//! | jobGates | `GET /api/jobs/{job}/gates` | member | — |
//! | refineGateOption | `PUT …/gates/{gate}/options/{option}` | job_designer | `gate` (library option) / `overlay` (generated option) |
//! | setGateThreshold | `PUT …/gates/{gate}/threshold` `{value}` | job_designer | `gate` |
//! | setGateRule | `PUT …/gates/{gate}/rule` `{rule}` | job_designer | `gate` |
//! | addGateQuestion | `POST /api/jobs/{job}/gates` | job_designer | `gate_question` |
//! | deleteGateQuestion | `DELETE …/gates/{gate}` | job_designer | `gate_question` |
//! | setGateCustomerReply (T8a) | `PUT …/gates/{gate}/customer-reply` `{text}` | job_designer | `gate_setting` |
//! | setGateEnabled (T8a) | `PUT …/gates/{gate}/enabled` `{enabled}` | job_designer | `gate_setting` |
//! | reviewGates | `POST …/gates/review` | job_designer | `gate_setting` (reserved key `_reviewed`) |
//! | evaluateGates (T10) | `POST …/gates/evaluate` | member | — (play: calls Laya, writes one usage row, no verdict) |
//! | gateVerdicts (T10) | `GET …/gates/verdicts?limit=` | member | — |
//!
//! - A GENERATED option (GT/GS/GA/GO) is refined through B2's `overlay::save_gate_option` (plan R2), never a B5 row.
//! - `too_few_options` counts both B5 hides and B2 `gate_option` tombstones (review focus 4); safety questions keep
//!   their rule, threshold and options (review focus 6) and are always asked.
//! - The playground runs the draft when its questions at the point differ from live (fixture `shape()`), else
//!   live, unless `against` says; it asks Laya through `gates::engine` with the chosen design's `gates` binding and
//!   writes no verdict. `gateVerdicts` reads the runtime's verdict log, newest first, with the draft's question
//!   text.
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::{body, job_ref};
use crate::design::read::case_design;
use crate::design::{self, DesignOf, DraftRef, DraftWrite, JobRef, Label};
use crate::design::{CaseDesign, Env};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::job_design::base::{self, Bases};
use crate::job_design::gates::engine::{self, EvalInput, ToolAsk, UsageBy};
use crate::job_design::gates::library::{
    self, BaseOption, GateRows, MAX_REPLY_CHARS, Question, js_num, normalise_rule, rule_error,
    rule_short, threshold_error,
};
use crate::job_design::gates::state;
use crate::job_design::gates::stats::{self, Stats};
use crate::job_design::keys::is_gate_key;
use crate::job_design::targets::gate_settings::{self, GateSetting};
use crate::job_design::types::{
    Against, EffGateQuestion, GateDecision, GateEvalInput, GateEvaluation, GateOptionRefinement,
    GatePoint, GateQuestionInput, GateRule, GateRunPoint, GateType, GateVerdictRow, JobGates,
    QuestionOrigin,
};
use crate::release::overlay::{self, GateOptionPatch};
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, JobRequires, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/jobs/{job}/gates",
            get(job_gates).post(add_gate_question),
        )
        .route(
            "/api/jobs/{job}/gates/{gate}",
            axum::routing::delete(delete_gate_question),
        )
        .route(
            "/api/jobs/{job}/gates/{gate}/options/{option}",
            put(refine_gate_option),
        )
        .route(
            "/api/jobs/{job}/gates/{gate}/threshold",
            put(set_gate_threshold),
        )
        .route("/api/jobs/{job}/gates/{gate}/rule", put(set_gate_rule))
        .route(
            "/api/jobs/{job}/gates/{gate}/customer-reply",
            put(set_gate_customer_reply),
        )
        .route(
            "/api/jobs/{job}/gates/{gate}/enabled",
            put(set_gate_enabled),
        )
        .route(
            "/api/jobs/{job}/gates/review",
            axum::routing::post(review_gates),
        )
        .route(
            "/api/jobs/{job}/gates/evaluate",
            axum::routing::post(evaluate_gates),
        )
        .route("/api/jobs/{job}/gates/verdicts", get(gate_verdicts))
}

const AREA: &str = "gates";
const PAGE: &str = "J7";
/// `job_gate_question.question` CHECK (8..2000 characters).
const MIN_QUESTION_CHARS: usize = 8;
const MAX_QUESTION_CHARS: usize = 2000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThresholdInput {
    value: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleInput {
    rule: Option<GateRule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyInput {
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnabledInput {
    enabled: bool,
}

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

fn gate_not_found(gate: &str) -> ApiError {
    ApiError::Missing("gate_not_found", format!("This job has no gate {gate}."))
}

fn place(pairs: &[(&str, Option<&str>)]) -> DraftRef {
    let mut m = DraftRef::new();
    for (k, v) in pairs {
        m.insert(
            (*k).to_owned(),
            v.map_or(Value::Null, |s| Value::String(s.to_owned())),
        );
    }
    m
}

/// The wire text of a unit enum (`P2`, `handover`, `closed`).
fn wire<T: Serialize>(t: &T) -> ApiResult<String> {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .ok_or_else(|| internal("a gate enum does not serialise as a string".into()))
}

/// The DRAFT bases and the job's gate rows.
async fn draft(
    c: &mut PgConnection,
    st: &AppState,
    jr: &JobRef,
) -> ApiResult<(Arc<Bases>, GateRows)> {
    library::of_design(
        &mut *c,
        &st.design,
        st.compiler.as_ref(),
        jr,
        &DesignOf::Draft,
    )
    .await
}

/// Question `gate` of the draft (404 `gate_not_found`).
fn find(bases: &Bases, rows: &GateRows, gate: &str) -> ApiResult<Question> {
    library::question(bases, rows, gate).ok_or_else(|| gate_not_found(gate))
}

/// Question `gate` as the draft has it now (after a write), with its stats.
async fn gate_now(
    c: &mut PgConnection,
    st: &AppState,
    j: &JobCtx,
    gate: &str,
) -> ApiResult<EffGateQuestion> {
    let (bases, rows) = draft(&mut *c, st, &job_ref(j)).await?;
    let stats = stats::read(&mut *c, j.job_id, Utc::now()).await?;
    let q = library::question(&bases, &rows, gate)
        .ok_or_else(|| internal(format!("gate {gate} is gone after its own write")))?;
    Ok(library::eff_gate(&q, &rows, &stats))
}

/// Write the threshold / rule refinement of `gate` (both `None` = back to the template: no row).
async fn write_rule(
    c: &mut PgConnection,
    job: uuid::Uuid,
    gate: &str,
    threshold: Option<f64>,
    rule: Option<Value>,
    base_hash: Option<String>,
    by: &str,
) -> ApiResult<()> {
    if threshold.is_none() && rule.is_none() {
        sqlx::query("DELETE FROM job_gate_rule_refinement WHERE job_id = $1 AND gate = $2")
            .bind(job)
            .bind(gate)
            .execute(&mut *c)
            .await?;
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO job_gate_rule_refinement (job_id, gate, threshold_value, rule, base_hash, updated_by) \
         VALUES ($1, $2, $3::float8, $4, $5, $6) \
         ON CONFLICT (tenant_id, job_id, gate) DO UPDATE \
            SET threshold_value = EXCLUDED.threshold_value, rule = EXCLUDED.rule, \
                base_hash = EXCLUDED.base_hash, updated_by = EXCLUDED.updated_by, \
                updated_at = clock_timestamp()",
    )
    .bind(job)
    .bind(gate)
    .bind(threshold)
    .bind(rule)
    .bind(base_hash)
    .bind(by)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// The `base_hash` of a question's rule refinement: the library rule's hash, `None` for a tenant question.
fn rule_base_hash(bases: &Bases, q: &Question) -> ApiResult<Option<String>> {
    match q.origin {
        QuestionOrigin::Tenant => Ok(None),
        QuestionOrigin::Platform => {
            let item = bases
                .gate(&q.key)
                .ok_or_else(|| internal(format!("{} is not a library gate", q.key)))?;
            base::gate_rule_hash(item).map(Some)
        }
    }
}

// ---- read -----------------------------------------------------------------------------------------------------------

async fn job_gates(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<JobGates>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (bases, rows) = draft(&mut tx, &st, &jr).await?;
    let stats = stats::read(&mut tx, j.job_id, Utc::now()).await?;
    let questions = library::effective(&bases, &rows, &stats);
    let releases = library::releases(&mut tx, &jr).await?;
    let changes = library::changes(&mut tx, &st.design, &jr, &bases, &rows, &releases).await?;
    tx.commit().await?;
    Ok(Json(JobGates {
        questions,
        window_from: stats.window_from,
        changes,
        releases,
    }))
}

// ---- playground and verdict log (T10) ---------------------------------------------------------------------------------

/// `GateEvalInput.point` values.
const RUN_POINTS: [&str; 3] = ["request", "tool", "response"];
/// `gateVerdicts` default and maximum page.
const VERDICTS_DEFAULT: i64 = 50;
const VERDICTS_MAX: i64 = 500;
/// `GateVerdictRow.answer` of a question the gate service did not answer (a failed point).
pub const NO_ANSWER: &str = "no answer";

/// What decides whether the draft differs from live at a point (fixture `shape()`): per question its key, on/off,
/// rule, action, fail policy, tools and options (key, label, hidden).
fn shape(qs: &[EffGateQuestion], point: GatePoint) -> Vec<Value> {
    qs.iter()
        .filter(|q| q.point == point)
        .map(|q| {
            let options: Vec<Value> = q
                .options
                .iter()
                .map(|o| json!([o.key, o.label, o.hidden]))
                .collect();
            json!([q.key, q.enabled, q.rule, q.action, q.fail, q.tools, options])
        })
        .collect()
}

async fn evaluate_gates(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<Json<GateEvaluation>> {
    // the fixture's order: empty text, then an unknown point, then the body's shape
    if v.get("text")
        .and_then(Value::as_str)
        .is_some_and(|t| t.trim().is_empty())
    {
        return Err(ApiError::Unprocessable(
            "empty_text",
            "Type a message to evaluate.".into(),
        ));
    }
    if v.get("point")
        .is_some_and(|p| !p.as_str().is_some_and(|p| RUN_POINTS.contains(&p)))
    {
        return Err(ApiError::Unprocessable(
            "invalid_point",
            "The point is request, tool or response.".into(),
        ));
    }
    let input: GateEvalInput = body(v)?;
    let text = input.text.trim();
    let point = input.point.gate_point();
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let none = Stats::default();
    let (bases, rows) = draft(&mut tx, &st, &jr).await?;
    let draft_qs = library::effective(&bases, &rows, &none);
    let (bases, rows) = library::of_design(
        &mut tx,
        &st.design,
        st.compiler.as_ref(),
        &jr,
        &DesignOf::Live,
    )
    .await?;
    let live_qs = library::effective(&bases, &rows, &none);
    let release_id = match case_design(&mut tx, &jr, Env::Production).await? {
        CaseDesign::Live { release_id } => release_id,
        CaseDesign::Draft { .. } => None,
    };
    tx.commit().await?;
    let differs = shape(&draft_qs, point) != shape(&live_qs, point);
    let against = input.against.unwrap_or(if differs {
        Against::Draft
    } else {
        Against::Live
    });
    let (questions, of) = match against {
        Against::Draft => (&draft_qs, DesignOf::Draft),
        Against::Live => (&live_qs, DesignOf::Live),
    };
    let tool = input
        .tool
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let state = match input.point {
        GateRunPoint::Request => state::request_state::<&str>(&[], text),
        GateRunPoint::Tool => state::tool_state(text, tool.unwrap_or_default(), None),
        GateRunPoint::Response => state::response_state::<&str>(text, &[]),
    };
    let run = engine::evaluate(
        &st,
        EvalInput {
            tenant: j.t.tenant,
            job: &jr,
            of: &of,
            questions,
            point,
            tool: (point == GatePoint::P2).then_some(ToolAsk::Named(tool)),
            state,
            usage: UsageBy {
                actor: &j.t.caller.sub,
                activity: None,
                conversation_id: None,
                case_id: None,
            },
            log: None,
        },
    )
    .await?;
    let r = run.result;
    Ok(Json(GateEvaluation {
        point: input.point,
        gate_point: point,
        ran_against: against,
        draft_differs: differs,
        release_id,
        decision: r.decision,
        deciding: r.deciding,
        answers: r.answers,
        skipped: r.skipped,
        latency_ms: r.latency_ms,
    }))
}

#[derive(Debug, Deserialize)]
struct VerdictsQuery {
    limit: Option<String>,
}

#[derive(sqlx::FromRow)]
struct VerdictRow {
    evaluation_id: i64,
    gate: String,
    at: chrono::DateTime<Utc>,
    point: String,
    answer: Option<String>,
    confidence: Option<f64>,
    unsure: bool,
    decision: String,
    case_id: Option<String>,
    conversation_id: String,
}

/// A database text of a wire enum (`P1`, `handover`).
fn from_text<T: serde::de::DeserializeOwned>(s: &str) -> ApiResult<T> {
    serde_json::from_value(Value::String(s.to_owned()))
        .map_err(|_| internal(format!("the gate log holds an unknown value {s}")))
}

async fn gate_verdicts(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<VerdictsQuery>,
) -> ApiResult<Json<Vec<GateVerdictRow>>> {
    let limit = match q.limit.as_deref() {
        None => VERDICTS_DEFAULT,
        Some(s) => s
            .trim()
            .parse::<i64>()
            .ok()
            .filter(|n| (1..=VERDICTS_MAX).contains(n))
            .ok_or_else(|| ApiError::Unprocessable("invalid_limit", "limit is 1–500.".into()))?,
    };
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (bases, rows) = draft(&mut tx, &st, &jr).await?;
    let texts: std::collections::BTreeMap<String, String> = library::questions(&bases, &rows)
        .into_iter()
        .map(|q| (q.key, q.question))
        .collect();
    let log = sqlx::query_as::<_, VerdictRow>(
        "SELECT e.id AS evaluation_id, a.gate, a.at, e.point, a.answer, a.confidence, a.unsure, e.decision,                 e.case_id, e.conversation_id            FROM gate_evaluation e            JOIN gate_answer a ON a.evaluation_id = e.id           WHERE e.job_id = $1           ORDER BY e.at DESC, e.id DESC, a.gate           LIMIT $2",
    )
    .bind(j.job_id)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut out = Vec::with_capacity(log.len());
    for r in log {
        out.push(GateVerdictRow {
            id: format!("{}:{}", r.evaluation_id, r.gate),
            at: r.at,
            point: from_text::<GatePoint>(&r.point)?,
            question: texts
                .get(&r.gate)
                .cloned()
                .unwrap_or_else(|| r.gate.clone()),
            answer: r.answer.unwrap_or_else(|| NO_ANSWER.to_owned()),
            confidence: r.confidence.unwrap_or(0.0),
            below_threshold: r.unsure,
            action: from_text::<GateDecision>(&r.decision)?,
            case_id: r.case_id.unwrap_or_default(),
            conversation_id: r.conversation_id,
            gate: r.gate,
        });
    }
    Ok(Json(out))
}

// ---- refinements ----------------------------------------------------------------------------------------------------

async fn refine_gate_option(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, gate, option)): ApiPath<(String, String, String)>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<Json<EffGateQuestion>> {
    let r: GateOptionRefinement = body(v)?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (bases, rows) = draft(&mut tx, &st, &jr).await?;
    let q = find(&bases, &rows, &gate)?;
    let op: BaseOption = q.option(&option).cloned().ok_or_else(|| {
        ApiError::Missing(
            "option_not_found",
            format!("{gate} has no option {option}."),
        )
    })?;
    if r.hidden == Some(true) {
        if q.safety {
            return Err(ApiError::Conflict(
                "safety_gate",
                format!("{gate} is a safety gate; its options cannot be hidden."),
            ));
        }
        let visible_others = q
            .options
            .iter()
            .filter(|o| o.key != option && !library::option_hidden(&q, o, &rows))
            .count();
        if visible_others < 2 {
            return Err(ApiError::Conflict(
                "too_few_options",
                "A question keeps at least two visible options.".into(),
            ));
        }
    }
    // absent keeps; null, blank or the base value reverts
    let trimmed = |x: &Option<String>| {
        x.as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    };
    if op.generated {
        // B2's overlay place (plan R2): the patch names the value the item should end with
        let patch = GateOptionPatch {
            label: r
                .label
                .as_ref()
                .map(|l| trimmed(l).unwrap_or_else(|| op.label.clone())),
            description: r
                .description
                .as_ref()
                .map(|d| trimmed(d).unwrap_or_else(|| op.description.clone())),
            hidden: r.hidden,
        };
        overlay::save_gate_option(
            &mut tx,
            &st.design,
            st.compiler.as_ref(),
            &j,
            &library::overlay_key(&gate, &option),
            patch,
        )
        .await?;
    } else {
        let cur = rows
            .options
            .get(&(gate.clone(), option.clone()))
            .cloned()
            .unwrap_or_default();
        let pick = |x: &Option<Option<String>>, base: &str, cur: Option<String>| match x {
            None => cur,
            Some(v) => trimmed(v).filter(|t| t.as_str() != base),
        };
        let label = pick(&r.label, op.label.as_str(), cur.label);
        let description = pick(&r.description, op.description.as_str(), cur.description);
        let hidden = r.hidden.unwrap_or(cur.hidden);
        let item = bases
            .gate_option(&gate, &option)
            .ok_or_else(|| internal(format!("{gate} · {option} is not a library option")))?;
        let base_hash = base::gate_option_hash(item)?;
        let text = match r.hidden {
            Some(h) => format!(
                "{gate} · “{}” {}",
                op.label,
                if h { "hidden" } else { "offered again" }
            ),
            None => format!(
                "{gate} · “{}” relabelled “{}”",
                op.label,
                r.label
                    .as_ref()
                    .and_then(trimmed)
                    .unwrap_or_else(|| op.label.clone())
            ),
        };
        let w = DraftWrite {
            area: AREA,
            page: PAGE,
            kind: "gate",
            r#ref: place(&[
                ("gate", Some(gate.as_str())),
                ("option", Some(option.as_str())),
            ]),
            label: Label::Now(text),
        };
        let job = j.job_id;
        let by = j.t.caller.sub.clone();
        let (g, o) = (gate.clone(), option.clone());
        design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
            Box::pin(async move {
                if label.is_none() && description.is_none() && !hidden {
                    sqlx::query(
                        "DELETE FROM job_gate_option_refinement \
                          WHERE job_id = $1 AND gate = $2 AND option = $3",
                    )
                    .bind(job)
                    .bind(&g)
                    .bind(&o)
                    .execute(&mut *c)
                    .await?;
                } else {
                    sqlx::query(
                        "INSERT INTO job_gate_option_refinement \
                                (job_id, gate, option, label, description, hidden, base_hash, updated_by) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                         ON CONFLICT (tenant_id, job_id, gate, option) DO UPDATE \
                            SET label = EXCLUDED.label, description = EXCLUDED.description, \
                                hidden = EXCLUDED.hidden, base_hash = EXCLUDED.base_hash, \
                                updated_by = EXCLUDED.updated_by, updated_at = clock_timestamp()",
                    )
                    .bind(job)
                    .bind(&g)
                    .bind(&o)
                    .bind(&label)
                    .bind(&description)
                    .bind(hidden)
                    .bind(&base_hash)
                    .bind(&by)
                    .execute(&mut *c)
                    .await?;
                }
                Ok::<(), ApiError>(())
            })
        })
        .await?;
    }
    let out = gate_now(&mut tx, &st, &j, &gate).await?;
    tx.commit().await?;
    Ok(Json(out))
}

async fn set_gate_threshold(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, gate)): ApiPath<(String, String)>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<Json<EffGateQuestion>> {
    let ThresholdInput { value } = body(v)?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (bases, rows) = draft(&mut tx, &st, &jr).await?;
    let q = find(&bases, &rows, &gate)?;
    if q.safety || q.threshold_value.is_none() {
        return Err(ApiError::Conflict(
            "threshold_not_tunable",
            format!("{gate} acts on any true; it has no threshold to tune."),
        ));
    }
    if let Some(e) = value.and_then(|x| threshold_error(q.kind, q.options.len(), x)) {
        return Err(ApiError::Unprocessable("invalid_threshold", e));
    }
    // the threshold tunes the template rule and replaces a rule refinement (the later edit wins)
    let stored = value.filter(|x| Some(*x) != q.threshold_value);
    let base_hash = rule_base_hash(&bases, &q)?;
    let text = match value {
        None => format!("{gate} · threshold back to the template value"),
        Some(x) => format!("{gate} · threshold → {}", js_num(x)),
    };
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: "gate",
        r#ref: place(&[("gate", Some(gate.as_str())), ("option", None)]),
        label: Label::Now(text),
    };
    let job = j.job_id;
    let by = j.t.caller.sub.clone();
    let g = gate.clone();
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(async move { write_rule(c, job, &g, stored, None, base_hash, &by).await })
    })
    .await?;
    let out = gate_now(&mut tx, &st, &j, &gate).await?;
    tx.commit().await?;
    Ok(Json(out))
}

async fn set_gate_rule(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, gate)): ApiPath<(String, String)>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<Json<EffGateQuestion>> {
    let RuleInput { rule } = body(v)?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (bases, rows) = draft(&mut tx, &st, &jr).await?;
    let q = find(&bases, &rows, &gate)?;
    if q.safety {
        return Err(ApiError::Conflict(
            "safety_gate",
            format!("{gate} is a safety gate; its rule is fixed by the template (D29)."),
        ));
    }
    let next = rule.as_ref().map(normalise_rule);
    if let Some(e) = next
        .as_ref()
        .and_then(|n| rule_error(q.kind, &q.options, n))
    {
        return Err(ApiError::Unprocessable("invalid_rule", e));
    }
    let same = next.as_ref().is_none_or(|n| *n == q.rule);
    let text = match (&next, same) {
        (Some(n), false) => format!("{gate} · rule → {}", rule_short(n)),
        _ => format!("{gate} · rule back to the template"),
    };
    let stored = if same {
        None
    } else {
        next.as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| internal(format!("a gate rule does not serialise: {e}")))?
    };
    let base_hash = rule_base_hash(&bases, &q)?;
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: "gate",
        r#ref: place(&[("gate", Some(gate.as_str())), ("option", None)]),
        label: Label::Now(text),
    };
    let job = j.job_id;
    let by = j.t.caller.sub.clone();
    let g = gate.clone();
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(async move { write_rule(c, job, &g, None, stored, base_hash, &by).await })
    })
    .await?;
    let out = gate_now(&mut tx, &st, &j, &gate).await?;
    tx.commit().await?;
    Ok(Json(out))
}

// ---- tenant questions -----------------------------------------------------------------------------------------------

/// A customer reply as saved: trimmed; blank or over [`MAX_REPLY_CHARS`] is 422 `invalid_reply`.
fn reply_text(t: &str) -> ApiResult<String> {
    let t = t.trim();
    if t.is_empty() {
        return Err(ApiError::Unprocessable(
            "invalid_reply",
            "Write the reply, or use the default.".into(),
        ));
    }
    if t.chars().count() > MAX_REPLY_CHARS {
        return Err(ApiError::Unprocessable(
            "invalid_reply",
            format!("A customer reply is at most {MAX_REPLY_CHARS} characters."),
        ));
    }
    Ok(t.to_owned())
}

async fn add_gate_question(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<EffGateQuestion>)> {
    let i: GateQuestionInput = body(v)?;
    let key = i.key.trim().to_uppercase();
    if !is_gate_key(&key) {
        return Err(ApiError::Unprocessable(
            "invalid_key",
            "Key: 2–8 capital letters or digits, starting with a letter.".into(),
        ));
    }
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (bases, rows) = draft(&mut tx, &st, &jr).await?;
    if bases.gate(&key).is_some() || rows.questions.contains_key(&key) {
        return Err(ApiError::Conflict(
            "gate_exists",
            format!("This job already asks {key}."),
        ));
    }
    let text = i.question.trim().to_owned();
    let n = text.chars().count();
    if n < MIN_QUESTION_CHARS {
        return Err(ApiError::Unprocessable(
            "invalid_question",
            "Write the question in full.".into(),
        ));
    }
    if n > MAX_QUESTION_CHARS {
        return Err(ApiError::Unprocessable(
            "invalid_question",
            format!("A question is at most {MAX_QUESTION_CHARS} characters."),
        ));
    }
    let (yes, no) = (i.criteria.yes.trim(), i.criteria.no.trim());
    if yes.is_empty() || no.is_empty() {
        return Err(ApiError::Unprocessable(
            "invalid_criteria",
            "Describe both the true and the false case.".into(),
        ));
    }
    let rule = normalise_rule(&i.rule);
    let options = [("true", yes), ("false", no)].map(|(k, d)| BaseOption {
        key: k.to_owned(),
        label: k.to_owned(),
        description: d.to_owned(),
        generated: false,
    });
    if let Some(e) = rule_error(GateType::Noul, &options, &rule) {
        return Err(ApiError::Unprocessable("invalid_rule", e));
    }
    let customer_reply = i.customer_reply.as_deref().map(reply_text).transpose()?;
    let tools = match (i.point, i.tools) {
        (GatePoint::P2, Some(t)) if !t.is_empty() => Some(t),
        _ => None,
    };
    let options = json!(
        options
            .iter()
            .map(|o| json!({"key": o.key, "label": o.label, "description": o.description, "generated": false}))
            .collect::<Vec<_>>()
    );
    let point = wire(&i.point)?;
    let fail = wire(&i.fail)?;
    let action = wire(&i.action)?;
    let effect = format!("Fires → {action}.");
    let rule = serde_json::to_value(&rule)
        .map_err(|e| internal(format!("a gate rule does not serialise: {e}")))?;
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: "gate_question",
        r#ref: place(&[("gate", Some(key.as_str()))]),
        label: Label::Now(format!("{key} · question added")),
    };
    let job = j.job_id;
    let by = j.t.caller.sub.clone();
    let k = key.clone();
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(async move {
            sqlx::query(
                "INSERT INTO job_gate_question \
                        (job_id, key, point, type, question, options, threshold, threshold_value, fail, effect, \
                         rule, action, tools, customer_reply, updated_by) \
                 VALUES ($1, $2, $3, 'noul', $4, $5, 'any true', NULL, $6, $7, $8, $9, $10, $11, $12)",
            )
            .bind(job)
            .bind(&k)
            .bind(&point)
            .bind(&text)
            .bind(&options)
            .bind(&fail)
            .bind(&effect)
            .bind(&rule)
            .bind(&action)
            .bind(&tools)
            .bind(&customer_reply)
            .bind(&by)
            .execute(&mut *c)
            .await?;
            Ok::<(), ApiError>(())
        })
    })
    .await?;
    let out = gate_now(&mut tx, &st, &j, &key).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

async fn delete_gate_question(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, gate)): ApiPath<(String, String)>,
) -> ApiResult<StatusCode> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (bases, rows) = draft(&mut tx, &st, &jr).await?;
    let q = find(&bases, &rows, &gate)?;
    if q.origin != QuestionOrigin::Tenant {
        return Err(ApiError::Conflict(
            "platform_question",
            format!(
                "{gate} comes from the {} template layer; only questions your team added can be deleted.",
                q.layer
            ),
        ));
    }
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: "gate_question",
        r#ref: place(&[("gate", Some(gate.as_str()))]),
        label: Label::Now(format!("{gate} · question deleted")),
    };
    let job = j.job_id;
    let g = gate.clone();
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(async move {
            sqlx::query("DELETE FROM job_gate_question WHERE job_id = $1 AND key = $2")
                .bind(job)
                .bind(&g)
                .execute(&mut *c)
                .await?;
            Ok::<(), ApiError>(())
        })
    })
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- T8a: per-job on/off and customer reply ------------------------------------------------------------------------

/// Write `gate`'s setting `s` as one `gate_setting` draft labelled `text`.
async fn save_setting(
    tx: &mut PgConnection,
    st: &AppState,
    j: &JobCtx,
    gate: &str,
    s: GateSetting,
    text: String,
) -> ApiResult<()> {
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: "gate_setting",
        r#ref: place(&[("gate", Some(gate))]),
        label: Label::Now(text),
    };
    let job = j.job_id;
    let by = j.t.caller.sub.clone();
    let g = gate.to_owned();
    design::track::<(), _>(&mut *tx, &st.design, j, w, move |c| {
        Box::pin(async move { gate_settings::write(c, job, &g, &s, &by).await })
    })
    .await?;
    Ok(())
}

async fn set_gate_customer_reply(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, gate)): ApiPath<(String, String)>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<Json<EffGateQuestion>> {
    let ReplyInput { text } = body(v)?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (bases, rows) = draft(&mut tx, &st, &jr).await?;
    let q = find(&bases, &rows, &gate)?;
    let reply = text.as_deref().map(reply_text).transpose()?;
    // the question's own text is the base: saving it back is no setting
    let stored = reply.filter(|t| Some(t) != q.customer_reply.as_ref());
    let label = if stored.is_some() {
        format!("{gate} · customer reply set")
    } else {
        format!("{gate} · customer reply back to the default")
    };
    let cur = rows.settings.get(&gate).cloned().unwrap_or_default();
    let s = GateSetting {
        enabled: cur.enabled,
        customer_reply: stored,
    };
    save_setting(&mut tx, &st, &j, &gate, s, label).await?;
    let out = gate_now(&mut tx, &st, &j, &gate).await?;
    tx.commit().await?;
    Ok(Json(out))
}

async fn set_gate_enabled(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, gate)): ApiPath<(String, String)>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<Json<EffGateQuestion>> {
    let EnabledInput { enabled } = body(v)?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let (bases, rows) = draft(&mut tx, &st, &jr).await?;
    let q = find(&bases, &rows, &gate)?;
    if !enabled && q.safety {
        return Err(ApiError::Conflict(
            "safety_gate",
            format!("{gate} is a safety gate; it is always asked (D29)."),
        ));
    }
    let cur = rows.settings.get(&gate).cloned().unwrap_or_default();
    let s = GateSetting {
        enabled: (!enabled).then_some(false),
        customer_reply: cur.customer_reply,
    };
    let label = format!("{gate} · turned {}", if enabled { "on" } else { "off" });
    save_setting(&mut tx, &st, &j, &gate, s, label).await?;
    let out = gate_now(&mut tx, &st, &j, &gate).await?;
    tx.commit().await?;
    Ok(Json(out))
}

/// `POST /api/jobs/{job}/gates/review`: the tenant read the gate questions and accepts them as they are. One
/// `gate_setting` draft at the reserved key [`gate_settings::REVIEWED_KEY`] (`{enabled: true}`; the row's
/// `updated_at` / `updated_by` are the review's time and actor), written through [`save_setting`] so it is one
/// ledger row + one `job.draft_change` audit row, in this transaction. Idempotent: a second review rewrites the same
/// value and only moves `updated_at` / `updated_by`. The marker changes no question and no bundle byte; it makes the
/// job's gate rows non-default, which clears `gates_unreviewed`.
async fn review_gates(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
) -> ApiResult<StatusCode> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let s = GateSetting {
        enabled: Some(true),
        customer_reply: None,
    };
    let label = "Gate questions · marked reviewed".to_owned();
    save_setting(&mut tx, &st, &j, gate_settings::REVIEWED_KEY, s, label).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
