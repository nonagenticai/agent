//! One gate point's whole run (B5 T10): which questions are asked, the `gates` provider, the Laya call, the usage
//! visibility row and — for the runtime only — the verdict log. The playground (`evaluateGates`) and the runtime
//! hooks (T12) share it, so a designer's try and a customer's turn ask the same questions the same way.
//!
//! - [`select`]: the questions of the point, minus the disabled ones (F-17), the P2 questions that do not cover the
//!   tool and the choice / score questions without a visible option (fixture `evaluateGates`; `skipped` says why).
//! - The provider is resolved in its own short transaction (`providers::llm::resolve`, `Use::Gates`, the design
//!   `of` the run reads); no transaction is open during the Laya call. Unbound → every asked question `skipped`
//!   (allow), and so is a process without the provider keyring (T12: a runtime daemon started without
//!   `PROVIDER_SECRET_KEYS` cannot open a key, so it runs no gate). A binding that cannot serve (409 `provider_deleted` / kind mismatch / `secret_unreadable`, 503 without
//!   the keyring) is a Laya failure: the fail policy decides (`failed_closed` hands over). A database failure is the
//!   caller's error.
//! - Usage: one `usage_event` row per Laya call (`source = "gate"`), written with metering emit OFF whatever the
//!   deployment says: Laya is billed by license-authz on the Laya licence, so agent writes no outbox / billing row
//!   (plan "Global constraints", no double billing).
//! - The verdict log ([`log`]): one `gate_evaluation` row per runtime gate-point call that asked something with a
//!   bound provider (T12: an unbound point logs nothing), and
//!   one `gate_answer` row per asked question (the answer LABEL and key, never the customer or reply text). The
//!   playground writes none.
use std::collections::BTreeSet;

use agent_model::kinds::catalog::SideEffect;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::evaluate::{self, Outcome, PointResult};
use super::laya::{LayaClient, LayaError};
use super::library;
use crate::conversations::store;
use crate::design::{CaseDesign, DesignOf, JobRef};
use crate::error::{ApiError, ApiResult};
use crate::job_design::grant_covers;
use crate::job_design::types::{
    EffGateQuestion, GateAction, GateDecision, GatePoint, GateType, SkippedQuestion,
};
use crate::providers::llm::{self, Use};
use crate::runtime::gates::GateVerdict;
use crate::state::AppState;
use crate::usage::{self, LlmUsage, MeteringConfig, UsageEvent};

/// The provider-secret refusal of a process without the keyring (`providers::secret::keyring`).
const UNCONFIGURED: &str = "provider_secret_key_unconfigured";

/// The `usage_event.source` of a Laya call.
pub const USAGE_SOURCE: &str = "gate";

/// What a P2 run is asked about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolAsk<'a> {
    /// The playground: the tool the designer named, if any. A question listing tools is asked only for a tool it
    /// covers; one without tools is always asked (fixture `evaluateGates`).
    Named(Option<&'a str>),
    /// The runtime: the operation the runner is about to call (`op_key`) and its side effect. A question without
    /// tools is asked before every W / X / P operation, never before a read ([`evaluate::asked_for_tool`]).
    Call {
        op_key: &'a str,
        side_effect: SideEffect,
    },
}

/// The questions a run asks and the ones it does not (with why).
#[derive(Clone, Debug, Default)]
pub struct Selection<'a> {
    pub asked: Vec<&'a EffGateQuestion>,
    pub skipped: Vec<SkippedQuestion>,
}

fn tool_skip(q: &EffGateQuestion, tool: Option<ToolAsk<'_>>) -> Option<String> {
    let tools = q.tools.as_deref().filter(|t| !t.is_empty());
    let Some(tools) = tools else {
        return match tool {
            Some(ToolAsk::Call {
                side_effect: SideEffect::R,
                ..
            }) => Some("asked only before a W, X or P operation".to_owned()),
            _ => None,
        };
    };
    let named = match tool {
        Some(ToolAsk::Named(n)) => n.map(str::trim).filter(|n| !n.is_empty()),
        Some(ToolAsk::Call { op_key, .. }) => Some(op_key),
        None => None,
    };
    match named {
        Some(t) if tools.iter().any(|g| grant_covers(g, t)) => None,
        Some(_) => Some(format!("asked only for {}", tools.join(", "))),
        None => Some(format!(
            "asked only for {}; no tool given",
            tools.join(", ")
        )),
    }
}

/// The questions of `point` a run asks, in design order.
pub fn select<'a>(
    questions: &'a [EffGateQuestion],
    point: GatePoint,
    tool: Option<ToolAsk<'_>>,
) -> Selection<'a> {
    let mut out = Selection::default();
    for q in questions.iter().filter(|q| q.point == point) {
        let reason = library::skip_reason(q)
            .map(str::to_owned)
            .or_else(|| {
                (point == GatePoint::P2)
                    .then(|| tool_skip(q, tool))
                    .flatten()
            })
            .or_else(|| {
                (q.kind != GateType::Noul && !q.options.iter().any(|o| !o.hidden)).then(|| {
                    format!(
                        "options are built per case from {}",
                        q.generated_from.as_deref().unwrap_or("the case")
                    )
                })
            });
        match reason {
            Some(reason) => out.skipped.push(SkippedQuestion {
                gate: q.key.clone(),
                reason,
            }),
            None => out.asked.push(q),
        }
    }
    out
}

/// Who a Laya call is recorded for (`usage_event`).
#[derive(Clone, Copy, Debug)]
pub struct UsageBy<'a> {
    /// A staff sub (the playground, a test chat) or `callers:<channel>`.
    pub actor: &'a str,
    pub activity: Option<&'a str>,
    pub conversation_id: Option<&'a str>,
    pub case_id: Option<&'a str>,
}

/// Where a runtime run is logged (the playground passes none).
#[derive(Clone, Copy, Debug)]
pub struct LogAt<'a> {
    /// The design the turn ran (D63): `Live{release_id}` → env `prod`, `Draft{draft_rev}` → env `test`
    /// (`RuntimeDesign::case`).
    pub case: &'a CaseDesign,
    pub conversation_id: &'a str,
    pub case_id: Option<&'a str>,
}

/// One gate-point run.
pub struct EvalInput<'a> {
    pub tenant: Uuid,
    pub job: &'a JobRef,
    /// The design whose `gates` binding answers (`Live` for prod, `Draft` for test).
    pub of: &'a DesignOf,
    /// The effective questions of that design (`library::effective`; T12: `JobDesign.gates`).
    pub questions: &'a [EffGateQuestion],
    pub point: GatePoint,
    /// P2 only.
    pub tool: Option<ToolAsk<'a>>,
    /// The point's state (`gates::state`): customer data, never logged or stored.
    pub state: Value,
    pub usage: UsageBy<'a>,
    /// `Some` = a runtime run: write the verdict log. `None` = the playground.
    pub log: Option<LogAt<'a>>,
}

/// What a run did.
#[derive(Clone, Debug, PartialEq)]
pub struct GateRun {
    pub point: GatePoint,
    /// The evaluation; its `skipped` lists the questions not asked first, then (for a failed or unbound point) the
    /// asked ones.
    pub result: PointResult,
    /// The keys of the asked questions, in design order.
    pub asked: Vec<String>,
    /// Whether Laya was called (one usage row).
    pub called: bool,
    /// The model of the call: Laya's answer, else the provider's.
    pub model: Option<String>,
    /// The `gate_evaluation.id` of a logged run.
    pub evaluation_id: Option<i64>,
}

impl GateRun {
    /// B4's verdict (D94 cause, the answers for the route hint, the evaluation id).
    pub fn verdict(&self) -> GateVerdict {
        self.result.verdict(self.evaluation_id)
    }
}

/// A bound, usable `gates` provider.
struct Bound {
    client: LayaClient,
    provider_id: String,
    funding: &'static str,
}

fn cannot_serve(code: &str) -> LayaError {
    LayaError::Unreachable(format!("the gates provider cannot serve ({code})"))
}

/// The `gates` provider of `of`: `Ok(None)` unbound, `Ok(Err(..))` bound but unusable (a gate failure), `Err` a
/// database failure.
async fn provider(
    st: &AppState,
    tenant: Uuid,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<Result<Option<Bound>, LayaError>> {
    let mut tx = store::begin_job(&st.db, tenant, job.id).await?;
    let resolved = match llm::resolve(&mut tx, st, tenant, job, Use::Gates, of).await {
        Ok(r) => Ok(r),
        // T12: a process without the provider keyring (a runtime daemon started without PROVIDER_SECRET_KEYS) cannot
        // reach a keyed binding: no Laya here means no gate (unbound, fail open), never a fail-closed handover
        Err(ApiError::Unavailable(UNCONFIGURED, ..)) => Ok(None),
        Err(ApiError::Conflict(code, _)) => Err(cannot_serve(code)),
        Err(ApiError::Unavailable(code, ..)) if code != "unavailable" => Err(cannot_serve(code)),
        Err(e) => return Err(e),
    };
    tx.commit().await?;
    let r = match resolved {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(Ok(None)),
        Err(e) => return Ok(Err(e)),
    };
    Ok(
        LayaClient::for_resolved(&r, &st.cfg.connector_policy, tenant)
            .await
            .map(|client| {
                Some(Bound {
                    client,
                    provider_id: r.provider_id.map_or_else(
                        || crate::turn::qualify::INTERNAL_PROVIDER.to_owned(),
                        |p| p.to_string(),
                    ),
                    funding: r.funding,
                })
            }),
    )
}

/// The metering label of a point's call (`usage_event.use`).
fn usage_label(p: GatePoint) -> &'static str {
    match p {
        GatePoint::P1 => "gate_p1",
        GatePoint::P2 => "gate_p2",
        GatePoint::R1 => "gate_r1",
    }
}

fn clamp_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Run one gate point: select, resolve, ask Laya (one call, within the gate budget), record the usage of the call
/// and, with `input.log`, write the verdict log. A database failure is an error; a Laya failure is a verdict.
pub async fn evaluate(st: &AppState, input: EvalInput<'_>) -> ApiResult<GateRun> {
    let sel = select(input.questions, input.point, input.tool);
    let asked: Vec<String> = sel.asked.iter().map(|q| q.key.clone()).collect();
    let bound = if sel.asked.is_empty() {
        Ok(None)
    } else {
        provider(st, input.tenant, input.job, input.of).await?
    };
    // T12: an unbound point (no `gates` provider, or none reachable from this process) runs no gate and logs nothing
    let has_provider = !matches!(bound, Ok(None));
    let (mut result, call) = match bound {
        Err(e) => (PointResult::failed(&sel.asked, &e), None),
        Ok(None) => (
            evaluate::evaluate(None, input.state, &sel.asked).await,
            None,
        ),
        Ok(Some(b)) => (
            evaluate::evaluate(Some(&b.client), input.state, &sel.asked).await,
            Some(b),
        ),
    };
    let mut skipped = sel.skipped;
    skipped.append(&mut result.skipped);
    result.skipped = skipped;
    let mut run = GateRun {
        point: input.point,
        model: call.as_ref().map(|b| {
            result
                .model
                .clone()
                .unwrap_or_else(|| b.client.model().to_owned())
        }),
        result,
        asked,
        called: call.is_some(),
        evaluation_id: None,
    };
    let logged = input.log.filter(|_| has_provider && !run.asked.is_empty());
    if call.is_none() && logged.is_none() {
        return Ok(run);
    }
    let mut tx = store::begin_job(&st.db, input.tenant, input.job.id).await?;
    if let Some(b) = &call {
        usage::emit(
            &mut tx,
            &MeteringConfig::default(),
            &UsageEvent {
                job_id: Some(input.job.id),
                source: USAGE_SOURCE,
                use_: usage_label(input.point),
                activity: input.usage.activity,
                conversation_id: input.usage.conversation_id,
                case_id: input.usage.case_id,
                actor: input.usage.actor,
                usage: &LlmUsage {
                    provider_id: b.provider_id.clone(),
                    model: run.model.clone().unwrap_or_default(),
                    funding: b.funding.to_owned(),
                    tokens_in: clamp_i64(run.result.input_tokens),
                    tokens_out: clamp_i64(run.result.output_tokens),
                    latency_ms: clamp_i64(run.result.latency_ms),
                },
            },
        )
        .await?;
    }
    if let Some(at) = logged {
        let entry = LogEntry {
            job: input.job.id,
            case: at.case,
            conversation_id: at.conversation_id,
            case_id: at.case_id,
        };
        run.evaluation_id = Some(log(&mut tx, &entry, &run).await?);
    }
    tx.commit().await?;
    Ok(run)
}

// ---- the verdict log ------------------------------------------------------------------------------------------------

/// Where one logged run belongs.
#[derive(Clone, Copy, Debug)]
pub struct LogEntry<'a> {
    pub job: Uuid,
    /// The design the turn ran: `Live{release_id}` → env `prod` + `release_id`; `Draft{draft_rev}` → env `test` +
    /// `draft_rev`.
    pub case: &'a CaseDesign,
    pub conversation_id: &'a str,
    pub case_id: Option<&'a str>,
}

/// `gate_evaluation.outcome` of a run: `pass` (answered, nothing fired), `fired`, `failed_open`, `failed_closed`,
/// `skipped` (no provider bound).
pub fn outcome_text(r: &PointResult) -> &'static str {
    match r.outcome {
        Outcome::Answered if r.answers.iter().any(|a| a.fired) => "fired",
        Outcome::Answered => "pass",
        Outcome::Skipped => "skipped",
        Outcome::FailedOpen => "failed_open",
        Outcome::FailedClosed => "failed_closed",
    }
}

fn decision_text(d: GateDecision) -> &'static str {
    match d {
        GateDecision::Allow => "allow",
        GateDecision::Block => "block",
        GateDecision::Handover => "handover",
    }
}

fn point_text(p: GatePoint) -> &'static str {
    match p {
        GatePoint::P1 => "P1",
        GatePoint::P2 => "P2",
        GatePoint::R1 => "R1",
    }
}

fn action_text(a: GateAction) -> &'static str {
    match a {
        GateAction::Handover => "handover",
        GateAction::Block => "block",
        GateAction::Annotate => "annotate",
    }
}

/// The `gate_answer` columns of a run, one entry per asked question.
#[derive(Default)]
struct AnswerColumns {
    gate: Vec<String>,
    answer: Vec<Option<String>>,
    value: Vec<Option<String>>,
    measure: Vec<Option<f64>>,
    confidence: Vec<Option<f64>>,
    fired: Vec<bool>,
    unsure: Vec<bool>,
    action: Vec<Option<String>>,
}

fn answer_columns(run: &GateRun) -> AnswerColumns {
    let mut c = AnswerColumns::default();
    let mut seen = BTreeSet::new();
    for a in &run.result.answers {
        seen.insert(a.gate.as_str());
        c.gate.push(a.gate.clone());
        c.answer.push(Some(a.answer.clone()));
        c.value.push(Some(a.value.clone()));
        c.measure.push(run.result.measures.get(&a.gate).copied());
        c.confidence.push(Some(a.confidence));
        c.fired.push(a.fired);
        c.unsure.push(a.unsure);
        c.action.push(a.action.map(|x| action_text(x).to_owned()));
    }
    // a failed point: every asked question, unanswered (the 7-day stats count fail_open / fail_closed per question)
    if matches!(
        run.result.outcome,
        Outcome::FailedOpen | Outcome::FailedClosed
    ) {
        for k in run.asked.iter().filter(|k| !seen.contains(k.as_str())) {
            c.gate.push(k.clone());
            c.answer.push(None);
            c.value.push(None);
            c.measure.push(None);
            c.confidence.push(None);
            c.fired.push(false);
            c.unsure.push(false);
            c.action.push(None);
        }
    }
    c
}

/// Write the verdict log of one runtime gate-point run on `c`, which must be scoped to the job's tenant
/// (`app.current_tenant`; `store::begin_job` scopes the job too): one `gate_evaluation` row and its `gate_answer`
/// rows (answered questions; for a failed point every asked question, unanswered). Stores labels, keys and numbers,
/// never the customer or reply text. Returns the `gate_evaluation.id` (`GateVerdict.evaluation_id`). As
/// `agent_runtime` (0145: SELECT, INSERT) or `agent_app`.
pub async fn log(c: &mut PgConnection, e: &LogEntry<'_>, run: &GateRun) -> ApiResult<i64> {
    let (env, release_id, draft_rev) = match e.case {
        CaseDesign::Live { release_id } => ("prod", release_id.as_deref(), None),
        CaseDesign::Draft { draft_rev } => ("test", None, Some(*draft_rev)),
    };
    let r = &run.result;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO gate_evaluation (job_id, point, env, release_id, draft_rev, conversation_id, case_id, \
                                      outcome, decision, deciding, model, latency_ms, error) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
         RETURNING id",
    )
    .bind(e.job)
    .bind(point_text(run.point))
    .bind(env)
    .bind(release_id)
    .bind(draft_rev)
    .bind(e.conversation_id)
    .bind(e.case_id)
    .bind(outcome_text(r))
    .bind(decision_text(r.decision))
    .bind(&r.deciding)
    .bind(run.model.as_deref())
    .bind(i32::try_from(r.latency_ms).unwrap_or(i32::MAX))
    .bind(r.error)
    .fetch_one(&mut *c)
    .await?;
    let a = answer_columns(run);
    if a.gate.is_empty() {
        return Ok(id);
    }
    sqlx::query(
        "INSERT INTO gate_answer (evaluation_id, job_id, gate, answer, value, measure, confidence, fired, unsure, \
                                  action) \
         SELECT $1, $2, u.* \
           FROM unnest($3::text[], $4::text[], $5::text[], $6::float8[], $7::float8[], $8::bool[], $9::bool[], \
                       $10::text[]) AS u",
    )
    .bind(id)
    .bind(e.job)
    .bind(&a.gate)
    .bind(&a.answer)
    .bind(&a.value)
    .bind(&a.measure)
    .bind(&a.confidence)
    .bind(&a.fired)
    .bind(&a.unsure)
    .bind(&a.action)
    .execute(&mut *c)
    .await?;
    Ok(id)
}
