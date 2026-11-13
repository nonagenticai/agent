//! B5's in-process gate hooks (T12, plan R9 and the B4 owner's rulings): B4's seams answered by Laya through the
//! one gate engine ([`super::engine`], the playground's too).
//!
//! - [`RequestPoint`] — P1 ([`RequestGate`]): the drain asks it on every turn before the qualifier (and the probe).
//! - [`ToolPoint`] — P2 (the runner's async [`ToolGate`], step 5b): asked PER CALL with the actual operation and its
//!   arguments, never per turn. First the toolset (the same [`DesignToolsets`] the assistant's offer reads, ruling
//!   5): refused `not_entitled` when the design has a toolset for the call's role × activity and it excludes the
//!   operation, and (fail-closed) when the design has toolsets for the role but the call has no activity or one the
//!   design has no toolset for; a role the design has no toolset for at all defers to B4 (allowed). An empty
//!   activity (a case planned before agent#65) first takes the activity of the case's context space from the
//!   design; only when none resolves is it refused. Then the P2
//!   questions covering the operation: handover → its handover reason
//!   (`gate_blocked` / `gate_unavailable`), block → `gate_blocked`.
//! - [`ReplyPoint`] — R1 ([`ReplyGate`]): the interaction daemon asks it before the reply is stored.
//! - [`QualifierHint`] (T10a) is the route hinter.
//!
//! Each point reads the case's design (`RuntimeDesign.job_design.gates`: Live in Production, Draft in Test), asks
//! only enabled questions (F-17, the engine skips the disabled ones) and logs its verdict (`gate_evaluation`,
//! `engine::log`). A database error inside a hook is a failed point: it hands over (`gate_unavailable`) when a
//! question the point would ask fails closed, and allows (failed open) otherwise. Customer data is never logged.
use std::pin::Pin;
use std::sync::Arc;

use agent_model::kinds::catalog::SideEffect;
use serde_json::Value;
use uuid::Uuid;

use super::engine::{self, EvalInput, LogAt, ToolAsk, UsageBy};
use super::library;
use super::route_hint::QualifierHint;
use super::state;
use crate::design::JobRef;
use crate::job_design::toolset::DesignToolsets;
use crate::job_design::types::{GateDecision, GateFail, GatePoint};
use crate::runtime::assistant::tools::ToolsetIndex;
use crate::runtime::design::RuntimeDesign;
use crate::runtime::domain::OpCall;
use crate::runtime::gates::{
    Decision, GateVerdict, Gates, ReplyGate, ReplyInput, RequestGate, RequestInput, VerdictCause,
};
use crate::runtime::runner::authorise::ToolGate;
use crate::state::AppState;

/// The usage actor of a runtime gate call (the customer's turn; the channel is not known at the hook).
pub const USAGE_ACTOR: &str = "callers:runtime";
/// The refusal of a call the role's toolset does not grant (the assistant's offer leaves such a tool out).
pub const NOT_ENTITLED: &str = "not_entitled";
/// The refusal of a call a P2 question blocked.
pub const GATE_BLOCKED: &str = "gate_blocked";

/// B5's hooks as `AppState.gates` holds them.
pub fn gates() -> Gates {
    Gates {
        request: Arc::new(RequestPoint),
        hinter: Arc::new(QualifierHint),
        reply: Arc::new(ReplyPoint),
    }
}

/// One runtime point to run.
struct Point<'a> {
    tenant: Uuid,
    job: &'a JobRef,
    design: &'a RuntimeDesign,
    point: GatePoint,
    tool: Option<ToolAsk<'a>>,
    state: Value,
    conversation_id: &'a str,
    case_id: Option<&'a str>,
    activity: Option<&'a str>,
}

/// The verdict of a point whose run failed (a database error): handover `Unavailable` with the closed keys of the
/// questions the point would ask, else allow (failed open).
pub fn failed_verdict(
    d: &RuntimeDesign,
    point: GatePoint,
    tool: Option<ToolAsk<'_>>,
) -> GateVerdict {
    let closed: Vec<String> = engine::select(d.job_design.gates.questions(), point, tool)
        .asked
        .into_iter()
        .filter(|q| q.fail == GateFail::Closed)
        .map(|q| q.key.clone())
        .collect();
    GateVerdict {
        decision: if closed.is_empty() {
            Decision::Allow
        } else {
            Decision::Handover
        },
        cause: VerdictCause::Unavailable,
        deciding: closed,
        evaluation_id: None,
        answers: Vec::new(),
    }
}

async fn run(st: &AppState, p: Point<'_>) -> GateVerdict {
    let input = EvalInput {
        tenant: p.tenant,
        job: p.job,
        of: &p.design.of,
        questions: p.design.job_design.gates.questions(),
        point: p.point,
        tool: p.tool,
        state: p.state,
        usage: UsageBy {
            actor: USAGE_ACTOR,
            activity: p.activity,
            conversation_id: Some(p.conversation_id),
            case_id: p.case_id,
        },
        log: Some(LogAt {
            case: &p.design.case,
            conversation_id: p.conversation_id,
            case_id: p.case_id,
        }),
    };
    match engine::evaluate(st, input).await {
        Ok(run) => run.verdict(),
        Err(e) => {
            let (status, code) = e.status_and_code();
            tracing::warn!(
                status = status.as_u16(),
                code,
                point = ?p.point,
                job = %p.job.key,
                "gate_hook_failed"
            );
            failed_verdict(p.design, p.point, p.tool)
        }
    }
}

/// F-16: what the customer reads when `v` hands over or blocks: the first deciding question's effective
/// `customer_reply`, else the care-bus default of the decision. `None` for an allow.
pub fn customer_text(d: &RuntimeDesign, v: &GateVerdict) -> Option<String> {
    let decision = match v.decision {
        Decision::Allow => return None,
        Decision::Block => GateDecision::Block,
        Decision::Handover => GateDecision::Handover,
    };
    let q = v
        .deciding
        .iter()
        .find_map(|k| d.job_design.gates.question(k));
    Some(match q {
        Some(q) => library::customer_text(q, decision).unwrap_or_default(),
        None => match decision {
            GateDecision::Block => library::DEFAULT_BLOCK_REPLY.to_owned(),
            _ => library::DEFAULT_HANDOVER_REPLY.to_owned(),
        },
    })
}

// ---- P1 -------------------------------------------------------------------------------------------------------------

/// P1: the request gate.
pub struct RequestPoint;

impl RequestGate for RequestPoint {
    fn check<'a>(
        &'a self,
        st: &'a AppState,
        input: RequestInput<'a>,
    ) -> Pin<Box<dyn Future<Output = GateVerdict> + Send + 'a>> {
        Box::pin(async move {
            let empty: [&str; 0] = [];
            run(
                st,
                Point {
                    tenant: input.tenant,
                    job: input.job,
                    design: input.design,
                    point: GatePoint::P1,
                    tool: None,
                    state: state::request_state(&empty, input.text),
                    conversation_id: input.conversation_id,
                    case_id: None,
                    activity: None,
                },
            )
            .await
        })
    }
}

// ---- R1 -------------------------------------------------------------------------------------------------------------

/// R1: the reply gate.
pub struct ReplyPoint;

impl ReplyGate for ReplyPoint {
    fn check<'a>(
        &'a self,
        st: &'a AppState,
        input: ReplyInput<'a>,
    ) -> Pin<Box<dyn Future<Output = GateVerdict> + Send + 'a>> {
        Box::pin(async move {
            run(
                st,
                Point {
                    tenant: input.tenant,
                    job: input.job,
                    design: input.design,
                    point: GatePoint::R1,
                    tool: None,
                    state: state::response_state(input.reply, input.operations),
                    conversation_id: input.conversation_id,
                    case_id: Some(input.case_id),
                    activity: None,
                },
            )
            .await
        })
    }
}

/// The activity of the context space a case was qualified for (`agent_case.context_space`, the design's space →
/// activity), for a call that carries none; `None` when the case, the space or its activity is unknown, or the
/// read fails (the gate then refuses a governed role: fail-closed).
async fn legacy_activity(
    st: &AppState,
    tenant: Uuid,
    design: &RuntimeDesign,
    call: &OpCall,
) -> Option<String> {
    let mut tx = crate::conversations::store::begin_job(&st.db, tenant, design.job.id)
        .await
        .ok()?;
    let space: Option<Option<String>> =
        sqlx::query_scalar("SELECT context_space FROM agent_case WHERE case_id = $1")
            .bind(&call.case_id)
            .fetch_optional(&mut *tx)
            .await
            .ok()
            .flatten();
    let _ = tx.commit().await;
    design
        .spaces
        .by_key(space.flatten().as_deref().filter(|k| !k.is_empty())?)
        .map(|sp| sp.activity.clone())
        .filter(|a| !a.is_empty())
}

// ---- P2 -------------------------------------------------------------------------------------------------------------

/// P2: the runner's tool gate (step 5b), per call.
pub struct ToolPoint;

impl ToolGate for ToolPoint {
    fn check<'a>(
        &'a self,
        st: &'a AppState,
        tenant: Uuid,
        design: &'a RuntimeDesign,
        call: &'a OpCall,
        op_key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let own = call.activity.as_deref().filter(|a| !a.is_empty());
            // a case planned before agent#65 carries an empty activity: its context space (the case's `agent_case.context_space`) names the activity the design maps it to
            let legacy = match own {
                None if design.job_design.toolsets.governs_role(&call.agent_role) => {
                    legacy_activity(st, tenant, design, call).await
                }
                _ => None,
            };
            let activity = own.or(legacy.as_deref());
            // refused where the job design governs the role: a known toolset excludes the op, or the call has no
            // (known, or space-resolved) activity; a role without any toolset defers to B4's authorisation (DesignToolsets logs it)
            if !DesignToolsets.allows(design, &call.agent_role, activity, op_key) {
                return Err(NOT_ENTITLED.to_owned());
            }
            // an operation the catalogue does not know is gated as the strongest write (P), as the runner does
            let side_effect = crate::runtime::runner::def_of(design, op_key)
                .map_or(SideEffect::P, |d| d.side_effect);
            let args = Value::Object(call.input.clone());
            let v = run(
                st,
                Point {
                    tenant,
                    job: &design.job,
                    design,
                    point: GatePoint::P2,
                    tool: Some(ToolAsk::Call {
                        op_key,
                        side_effect,
                    }),
                    state: state::tool_state("", op_key, Some(&args)),
                    conversation_id: &call.conversation_id,
                    case_id: Some(&call.case_id),
                    activity,
                },
            )
            .await;
            match v.decision {
                Decision::Allow => Ok(()),
                Decision::Handover => Err(v.handover_reason().unwrap_or(GATE_BLOCKED).to_owned()),
                Decision::Block => Err(GATE_BLOCKED.to_owned()),
            }
        })
    }
}
