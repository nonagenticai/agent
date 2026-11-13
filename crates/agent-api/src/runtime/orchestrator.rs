//! The orchestrator (port of care-bus-agents@96c608c `crates/carebus/src/orchestrator.rs`, B4 T7): one task per
//! scope consumes `request.qualified` from the scope's `bus:customer:{area}` streams and `op.result` from
//! `bus:op:results`, turns a qualified request into a phased plan of operation calls (`op.call` on `bus:op`, run by
//! the runner) and publishes the case's `request.resolved`.
//!
//! - **Plan.** The procedure for the request's context space comes from [`ProcedureSource`] over the scope's
//!   [`RuntimeDesign`] (Live in prod, Draft in test). The plan is stamped with the design it was read at
//!   (`CaseDesign`), stored once in the case hash (first plan wins) and written to the `agent_case` row, so a
//!   publish that lands between qualification and planning is the release the case records
//!   (`qualified_release_id` keeps the qualify-time one).
//! - **Run.** Phases run in order, the steps of one phase in parallel. A `gate` step that answers false ends the
//!   case `not_eligible`. A failed step is retried once when the runner says it may be (same idempotency key), else
//!   the case ends `failed` — and, its reply promising a colleague, queues a J21 item (`tool_failed`) unless the
//!   conversation already has one (N-12); the case row stays `handover`. `unknown_outcome` and a case whose status
//!   turned `handover` (staff, the runner) stop at
//!   the next boundary. A case past its deadline (3,600 s) ends `failed`. When the scope is removed (tenant
//!   suspended, job offboarded), [`cancel_offboarded`] ends its open cases `cancelled_offboard`.
//! - **Checks (T6a, F-20).** A plan carries the procedure's `success_criteria` and `tool_requirements`
//!   ([`ProcedureSource::checks`]). A required operation routed `not_offered` hands the case over (reason
//!   `not_offered`) before its first step; unmet success criteria after the last phase hand it over (reason and
//!   outcome `success_criteria_unmet`). Every item the orchestrator queues notifies the job's worker channels.
//!   A step's pre/postconditions and per-case cap are the runner's: a `precondition_unmet` denial ends a gate step
//!   `not_eligible` and hands any other over; `case_cap_reached` and `postcondition_failed` arrive already handed over.
//! - **Assistant (T7a, F-40).** With an [`Assistant`] attached ([`Orchestrator::with_assistant`]) and its role
//!   switched on for the activity, a request with no procedure is planned as an assistant case and run round by
//!   round (crate::runtime::assistant): one model call, then its tool calls as ordinary `op.call`s (round = plan
//!   phase), the next round once every call answered. Without one (the binary until B5), behaviour is unchanged.
//! - **Identity (B4).** The customer is known when the conversation starts (level 2) and is never asked for a factor
//!   or an id. A step refused `identity_required` (it needs level 3) hands the case over with reason
//!   `step_up_required`. Every call carries the case's
//!   CURRENT level (the case hash `identity`), not the qualified one.
//! - **Declared inputs (A2).** A call answered `input_required` parks the case (`slots::park_input`: the chat hash
//!   field `input`, the question as the reply, code `input_required`); the drain stores the user's next message as
//!   that state key and re-runs the request. `input_unresolved` / `secret_store_unavailable` / `state_unavailable`
//!   hand the case over as `tool_failed`, as does a refused `state_required`.
//! - **Record.** Every status change is an `agent_case` UPDATE plus a `state` case event in one transaction, then
//!   the publish (R6). Logs and events carry ids and codes only (K50).
//!
//! It keeps no state in memory: any replica can handle any message, and a restart resumes from the case hash.
use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use redis::AsyncCommands;
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use super::assistant::turn::{self as assistant_turn, LoopState, Pending};
use super::assistant::{self, Assistant};
use super::bus::{Bus, ConsumeOptions, Envelope, groups};
use super::case::{ACTIVE_SCOPES_KEY, CasePhase, CaseStore, DEFAULT_CASE_DEADLINE};
use super::catalog::OperationCatalog;
use super::conditions::{self, CaseFacts, CondOp, Condition, StepFact};
use super::design::{DesignCache, RuntimeDesign};
use super::domain::{
    AssistantPlan, CaseOutcome, OP_CALL, OP_RESULT, OpCall, OpResult, OpState, OutOfHoursNote,
    Phase, Plan, ProcedureRef, QualifiedRequest, REQUEST_QUALIFIED, REQUEST_RESOLVED,
    ROLE_ASSISTANT, ROLE_PROCEDURE, Resolution, Step, answered_false, call_id, idempotency_key,
    resolve_args,
};
use super::heartbeat::Serving;
use super::reply::KnowledgeUse;
use super::routes::{ResolvedTarget, RouteQuery, RouteResolver};
use super::runner::agent_cases::{self, Queued};
use super::runner::ops::EffectiveRoutes;
use super::runner::{POSTCONDITION_FAILED, PRECONDITION_UNMET, builtin, cap, declared};
use super::scope::Scope;
use super::scopes;
use super::spaces::TemplateProcedure;
use super::trace::{TraceActivity, TraceInput, TraceOp, TraceStep};
use crate::conversations::{handover, ownership, store};
use crate::design::{CaseDesign, JobRef, Registry};
use crate::error::ApiError;
use crate::job_design::toolset::DesignToolsets;
use crate::job_design::{grant_allows, op_label};
use crate::providers::llm::Use;
use crate::usage::LlmUsage;

/// The longest one orchestrator handler (`on_customer` / `on_result`) runs: an assistant round is ONE model call
/// ([`assistant::turn::LLM_TIMEOUT`]; the turn's rounds are separate bus entries, so [`assistant::TURN_BUDGET`] is not
/// a handler bound). Its `reclaim_idle` is this plus slack, so a peer never takes a live round over.
pub const HANDLER_BUDGET: Duration = assistant_turn::LLM_TIMEOUT;
/// The orchestrator's consumer options (what `new` uses): `reclaim_idle` above [`HANDLER_BUDGET`].
pub fn consume_options() -> ConsumeOptions {
    ConsumeOptions::for_handler_budget(HANDLER_BUDGET)
}

/// The runtime role (consumer group, heartbeat role, envelope `from`).
pub const ROLE: &str = "orchestrator";

/// How often a scope task sweeps its overdue cases.
const SWEEP_EVERY: Duration = Duration::from_secs(5);
const SWEEP_BATCH: usize = 50;

/// How often the offboard sweep ([`cancel_offboarded`]) runs in the binary.
pub const OFFBOARD_EVERY: Duration = Duration::from_secs(30);

/// Case statuses still open (`agent_case_open` index; `conversations::store::OPEN_CASE`).
const OPEN: [&str; 4] = store::OPEN_CASE;

/// T6a: the outcome of a case whose procedure's success criteria do not hold after its last phase.
pub const SUCCESS_CRITERIA_UNMET: &str = "success_criteria_unmet";

/// B4: the outcome code and J21 reason of a step that needs identity level 3 (a known customer starts at 2).
pub const STEP_UP_REQUIRED: &str = "step_up_required";

/// T7a: the case hash field of the assistant loop's state ([`LoopState`]).
const ASSISTANT_STATE: &str = "assistant";

/// T7a: the case hash field of round `round`'s tool calls ([`Pending`]).
fn assistant_round_field(round: usize) -> String {
    format!("assistant:round:{round}")
}

/// T7a: the case hash field claiming round `round`'s model call (set once: a redelivery never calls twice).
fn assistant_claim_field(round: usize) -> String {
    format!("assistant:llm:{round}")
}

/// T6a: the procedure-level checks of a plan: a designed procedure's own ([`DesignProcedures`]), else the built-in
/// ones of [`builtin_checks`] / [`knowledge_checks`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcedureChecks {
    pub success_criteria: Vec<Condition>,
    pub tool_requirements: Vec<String>,
}

/// Where the orchestrator gets the plan for a qualified request: the procedure of its context space in the scope's
/// design. [`DesignProcedures`] (the binary, the probe) runs a PUBLISHED tenant version's own steps and falls back to
/// [`StaticProcedures`], which carries the built-in steps of the template procedures the runtime knows.
pub trait ProcedureSource: Send + Sync + 'static {
    /// The procedure version and its phases, or `None` when the job has no procedure for the request (the case is
    /// then handed over `out_of_scope`).
    fn plan(
        &self,
        design: &RuntimeDesign,
        req: &QualifiedRequest,
    ) -> Option<(Option<ProcedureRef>, Vec<Phase>)>;

    /// The procedure's success criteria and required operations (T6a). Default: none (a pre-B5 design).
    fn checks(&self, _design: &RuntimeDesign, _req: &QualifiedRequest) -> ProcedureChecks {
        ProcedureChecks::default()
    }

    /// Why [`Self::plan`] has no plan although a procedure serves the request: `Some(code)` when the source REFUSED
    /// it ([`DesignProcedures`]: steps that do not map, or outside the grant of the request space's activity). The
    /// case then hands over `tool_failed` with that code, never `out_of_scope` and never to the assistant. Default:
    /// never.
    fn refusal(&self, _design: &RuntimeDesign, _req: &QualifiedRequest) -> Option<&'static str> {
        None
    }
}

/// B4's stand-in for B5's procedure steps: the template procedure the context space maps to (by the space's last
/// key segment), run with the built-in steps of [`builtin_phases`]. A job whose pinned template does not declare
/// that procedure has none.
///
/// These builtins are the DESIGNED FALLBACK, not dead code (plan b4-procedure-library-v2, Task 9 and the `choose` rule
/// "template procedure outside the grant => builtin + warn"). Seed v9..v15 carries a library procedure for every
/// builtin space (retail `returns`, `warranty`, `order_status`, `delivery_problem`, `refunds`, `track_parcel`), but
/// only on the retail layer and only for jobs whose design pins are at that head; a job pinned older, a template
/// whose activity grant does not cover the steps, or a design without the retail layer still plans these. Retire
/// them only once every live and draft job pin is proven at the library head (not provable from code).
pub struct StaticProcedures;

/// The template procedure (by name) that serves a context space (its key's last segment).
pub fn procedure_for_space(space: &str) -> Option<&'static str> {
    Some(match space {
        "returns" => "return_request",
        "warranty" => "warranty_claim",
        "order_status" => "order_status",
        "delivery_problem" => "delivery_problem",
        "refunds" => "refund_status",
        // live CS-58: no retail template declares a parcel-tracking procedure (customer_care_agent declares
        // delivery_problem, order_status, refund_status, return_request, warranty_claim), so "where is my parcel"
        // runs as order_status — with its own step and checks ([`builtin_phases_for_space`],
        // [`builtin_checks_for_space`]): the shipment read by the reference the customer gives
        "track_parcel" => "order_status",
        _ => return None,
    })
}

/// The one operation a `track_parcel` case runs: the activity `delivery:track_parcel` grants
/// `delivery.read{by_tracking_ref}` (seed v8's `delivery:where_is_my_order` row), never the no-variant `delivery.read` (a
/// variant grant does not cover it: crate::job_design::toolset `decide`).
pub const TRACK_PARCEL_READ: &str = "delivery.read{by_tracking_ref}";

/// The built-in phases of `procedure` serving context space `space` (its key's last segment): [`builtin_phases`],
/// except `track_parcel` (served by `order_status`): one [`TRACK_PARCEL_READ`] step (`delivery`, a read at identity
/// level 1 — the reference is a possession factor) keyed by the space's only slot `tracking_ref`, bound to the
/// variant's one declared input and nothing else. A system with no `by_tracking_ref` operation (demo_retail@3–5)
/// would serve it with its order-keyed `delivery.read` (level 2): the runner refuses that before the call (step 6b,
/// `not_offered`: the reference is not an order id), and the case hands over. No `order.read`: it needs level 2.
pub fn builtin_phases_for_space(space: &str, procedure: &str) -> Option<Vec<Phase>> {
    if space != "track_parcel" {
        return builtin_phases(procedure);
    }
    Some(vec![Phase {
        steps: vec![Step::op(
            "delivery",
            "delivery.read",
            Some("by_tracking_ref"),
            json!({"tracking_ref": "$tracking_ref"}),
        )],
    }])
}

/// The checks of `procedure` serving context space `space`: [`builtin_checks`], except `track_parcel`, which needs
/// [`TRACK_PARCEL_READ`] routed (else it hands over `not_offered` before any call, as `delivery_problem` does).
pub fn builtin_checks_for_space(space: &str, procedure: &str) -> ProcedureChecks {
    if space != "track_parcel" {
        return builtin_checks(procedure);
    }
    ProcedureChecks {
        success_criteria: Vec::new(),
        tool_requirements: vec![TRACK_PARCEL_READ.to_owned()],
    }
}

/// The built-in phases of a template procedure (B4 until B5 materialises `ProcedureStep[]`). The return flow is
/// `order.read` → `rule.evaluate{RET-01}` (gate) → `case.create{return}` (the RMA).
pub fn builtin_phases(procedure: &str) -> Option<Vec<Phase>> {
    let order = || {
        Step::op(
            "order",
            "order.read",
            None,
            json!({"order_id": "$order_id"}),
        )
    };
    let one = |steps: Vec<Step>| Phase { steps };
    Some(match procedure {
        "return_request" => vec![
            one(vec![order()]),
            one(vec![
                Step::op(
                    "rules",
                    "rule.evaluate",
                    Some("RET-01"),
                    json!({"order_id": "$order_id", "delivered_at": "$order.delivered_at",
                           "return_lines": "$order.lines"}),
                )
                .gated(),
            ]),
            one(vec![Step::op(
                "rma",
                "case.create",
                Some("return"),
                json!({"subject_object": "$order_id", "return_lines": "$order.lines",
                       "return_method": "$return_method", "refund_type": "$refund_type"}),
            )]),
        ],
        "warranty_claim" => vec![
            one(vec![order()]),
            one(vec![
                Step::op(
                    "rules",
                    "rule.evaluate",
                    Some("WAR-01"),
                    json!({"order_id": "$order_id", "offering_id": "$offering_id",
                           "purchase_date": "$order.placed_at"}),
                )
                .gated(),
            ]),
            one(vec![Step::op(
                "claim",
                "case.create",
                Some("warranty_claim"),
                json!({"subject_object": "$order_id", "description": "$defect"}),
            )]),
        ],
        "order_status" => vec![one(vec![order()])],
        // acceptance A4: the problem is reported first (it is logged whatever the shipment read answers), then the
        // shipment is read so [`builtin_checks`] can tell a late parcel (a colleague takes it) from one on its way
        "delivery_problem" => vec![
            one(vec![order()]),
            one(vec![Step::op(
                "report",
                "delivery.report_problem",
                None,
                json!({"order_id": "$order_id", "problem": "$problem"}),
            )]),
            one(vec![Step::op(
                "delivery",
                "delivery.read",
                None,
                json!({"order_id": "$order_id"}),
            )]),
        ],
        "refund_status" => vec![one(vec![Step::op(
            "payments",
            "payment.list",
            None,
            json!({"order_id": "$order_id"}),
        )])],
        _ => return None,
    })
}

/// The canonical `delivery.status` values of a shipment still on its way (the `delivery` object's enum less
/// `problem`, `failed`, `returned` and `delivered`; a carrier's `exception` maps to canonical `problem`). Not
/// `delivered`: this is the criterion of a delivery *complaint*, and a carrier saying "delivered" to a customer who
/// says the parcel never came contradicts them — a colleague must look, the agent must not answer "delivered".
pub const DELIVERY_ON_TRACK: [&str; 4] = ["created", "ready", "in_transit", "pickup_available"];

/// The success criteria of a built-in procedure (B4's stand-in for B5's procedure checks, like [`builtin_phases`]).
/// `delivery_problem` resolves only when the shipment `delivery.read` answered is still on its way: a late
/// (`problem`), failed, returned or `delivered` parcel — or a read without a status — hands the case over
/// (`success_criteria_unmet`, acceptance A4).
pub fn builtin_checks(procedure: &str) -> ProcedureChecks {
    match procedure {
        "delivery_problem" => ProcedureChecks {
            success_criteria: vec![Condition {
                id: "delivery_on_track".to_owned(),
                fact: "steps.delivery.out.status".to_owned(),
                op: CondOp::In,
                value: Some(json!(DELIVERY_ON_TRACK)),
            }],
            // a scope that does not offer them hands over before the first call (T6a), never fails mid-flow
            tool_requirements: vec![
                "delivery.report_problem".to_owned(),
                "delivery.read".to_owned(),
            ],
        },
        _ => ProcedureChecks::default(),
    }
}

/// The step a support turn's knowledge answer is stored under: the `facts.faq` the reply reads (D109, ruling R6).
pub const KNOWLEDGE_STEP: &str = "faq";

/// The success criterion of a knowledge-answered turn: an answer that is not grounded (no passage, or one citing
/// none) escalates (spec 12 A9).
pub const ANSWER_GROUNDED: &str = "answer_grounded";

/// D109 / D115 (4): the knowledge scope of a support turn in context space `space`: each of the space's
/// `knowledge_kinds` that is a kind of the job (the compile's effective kinds) at the support stage — else at every
/// stage (`*`), else at the first stage the job has it (the seed's areas name `delivery` / `orientation` kinds) — and, when a selected department declares the space, one of that department's own or inherited
/// kinds. Empty = the space routes no knowledge step.
pub fn knowledge_scope(design: &RuntimeDesign, space: &str) -> Vec<(String, String)> {
    use agent_model::kinds::stage_knowledge::ANY_STAGE;
    let Some(s) = design.spaces.by_key(space) else {
        return Vec::new();
    };
    let dept = design.space_departments.get(space);
    let mut out: Vec<(String, String)> = Vec::new();
    for kind in s.knowledge_kinds.iter().map(|k| k.trim()) {
        let fits = |stage: &str| {
            design.knowledge.kinds.iter().find(|k| {
                k.kind == kind && k.stage == stage && dept.is_none_or(|d| k.of_department(d))
            })
        };
        // the area names the kind: the support stage first, then every stage (`*`), then the kind's first stage
        let Some(k) = fits(builtin::SUPPORT_STAGE)
            .or_else(|| fits(ANY_STAGE))
            .or_else(|| {
                design
                    .knowledge
                    .kinds
                    .iter()
                    .find(|k| k.kind == kind && dept.is_none_or(|d| k.of_department(d)))
            })
        else {
            continue;
        };
        let pair = (k.stage.clone(), k.kind.clone());
        if !out.contains(&pair) {
            out.push(pair);
        }
    }
    out
}

/// The plan of a knowledge-answered support turn: one phase, one `knowledge.answer` step ([`KNOWLEDGE_STEP`]) over
/// `scope`, answering the inbound message `message_id` the request was qualified from.
pub fn knowledge_phases(scope: &[(String, String)], message_id: Option<&str>) -> Vec<Phase> {
    let scope: Vec<Value> = scope
        .iter()
        .map(|(stage, kind)| json!({"stage": stage, "kind": kind}))
        .collect();
    let mut args = json!({"scope": scope});
    if let Some(m) = message_id {
        args["message_id"] = json!(m);
    }
    vec![Phase {
        steps: vec![Step::op(KNOWLEDGE_STEP, builtin::ANSWER_OP, None, args)],
    }]
}

/// What the runtime supplies to the `knowledge.answer` / `knowledge.search` steps of a DESIGNED procedure, as
/// [`knowledge_phases`] does for the static plan: the triggering customer message (`message_id`, which the runner
/// reads the question from; the customer is never asked for it, so an authored value is replaced) and, unless the
/// author named one `kind`, the request space's knowledge `scope` (authored args are strings, so a scope list cannot
/// be authored). An author's `query` / `kind` / `stage` / `top_k` are kept.
pub fn bind_knowledge_steps(phases: &mut [Phase], design: &RuntimeDesign, req: &QualifiedRequest) {
    let scope: Vec<Value> = knowledge_scope(design, &req.context_space)
        .iter()
        .map(|(stage, kind)| json!({"stage": stage, "kind": kind}))
        .collect();
    for step in phases.iter_mut().flat_map(|p| p.steps.iter_mut()) {
        if step.operation != builtin::ANSWER_OP && step.operation != builtin::SEARCH_OP {
            continue;
        }
        match &req.message_id {
            Some(m) => step.args.insert("message_id".into(), json!(m)),
            None => step.args.remove("message_id"),
        };
        let names_kind = step
            .args
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|k| !k.trim().is_empty());
        if !names_kind {
            if scope.is_empty() {
                step.args.remove("scope");
            } else {
                step.args
                    .insert("scope".into(), Value::Array(scope.clone()));
            }
        }
    }
}

/// The checks of a knowledge-answered turn: the answer must be grounded ([`ANSWER_GROUNDED`]: an ungrounded one
/// hands over `success_criteria_unmet`), and `knowledge.answer` must be offered.
pub fn knowledge_checks() -> ProcedureChecks {
    ProcedureChecks {
        success_criteria: vec![Condition {
            id: ANSWER_GROUNDED.to_owned(),
            fact: format!("steps.{KNOWLEDGE_STEP}.out.grounded"),
            op: CondOp::Eq,
            value: Some(json!(true)),
        }],
        tool_requirements: vec![builtin::ANSWER_OP.to_owned()],
    }
}

/// The built-in version of `name` the job's template declares: the newest version's
/// [`TemplateProcedure::builtin_version`]. A PUBLISHED tenant version shadowing the name yields the base version it
/// replaced, never its own: [`StaticProcedures`] runs the BUILT-IN steps, so it records the built-in version. A
/// tenant name no base declares has none.
fn builtin_declared(design: &RuntimeDesign, name: &str) -> Option<ProcedureRef> {
    let p = design
        .procedures
        .iter()
        .filter(|p| p.name == name)
        .max_by_key(|p| p.version)?;
    p.builtin_version.map(|version| ProcedureRef {
        name: p.name.clone(),
        version,
    })
}

impl ProcedureSource for StaticProcedures {
    fn checks(&self, design: &RuntimeDesign, req: &QualifiedRequest) -> ProcedureChecks {
        let space = req.space();
        match procedure_for_space(space) {
            Some(name) if builtin_declared(design, name).is_some() => {
                builtin_checks_for_space(space, name)
            }
            _ if !knowledge_scope(design, &req.context_space).is_empty() => knowledge_checks(),
            Some(name) => builtin_checks_for_space(space, name),
            None => ProcedureChecks::default(),
        }
    }

    /// The procedure the space maps to, when the template declares it; else, by default, the knowledge step over
    /// the space's knowledge kinds (D109); else none.
    fn plan(
        &self,
        design: &RuntimeDesign,
        req: &QualifiedRequest,
    ) -> Option<(Option<ProcedureRef>, Vec<Phase>)> {
        if let Some(name) = procedure_for_space(req.space())
            && let Some(builtin) = builtin_declared(design, name)
        {
            return Some((Some(builtin), builtin_phases_for_space(req.space(), name)?));
        }
        let scope = knowledge_scope(design, &req.context_space);
        (!scope.is_empty()).then(|| (None, knowledge_phases(&scope, req.message_id.as_deref())))
    }
}

/// B4 over B5's procedures: the plan of a PUBLISHED tenant version's own steps (`TemplateProcedure::step_defs`, the
/// design read Live in prod and Draft in test — CANARY and DRAFT versions never reach it), else [`StaticProcedures`]
/// unchanged (built-in steps, then the knowledge fallback).
///
/// 1. A designed procedure runs ONLY for the space it was published under: steps, and a stored `context_space`
///    equal to the request's space key (the full D61 key). Several names → the first by name, its newest version,
///    and a warning naming the space and the names.
/// 2. Else [`StaticProcedures`]. A tenant version named like a builtin but published under another space (or under
///    none) is NOT run for this request: publish checked its steps against the grant of ITS space only, so here the
///    BUILT-IN steps run and the built-in version is recorded ([`builtin_declared`]), never the tenant version.
///
/// Defence in depth: before a designed procedure is chosen, every step's operation (and variant) must be inside
/// the grant of the request space's activity ([`DesignToolsets::activity_grant`], the grant publish checks). An
/// activity that is empty or unknown to the design, or a step outside the grant, refuses the procedure; so does a
/// step that does not map ([`Step::from_design`]). A refused procedure plans nothing and the case hands over
/// ([`ProcedureSource::refusal`]) rather than run other steps under its name.
///
/// The checks are the designed procedure's own `success_criteria` / `tool_requirements` when it has any, else
/// [`StaticProcedures`]'. A designed procedure's steps reach the runner as any built-in step does: the identity
/// level, the served-operation check (6b) and the per-case cap (7b) apply unchanged.
pub struct DesignProcedures;

/// The state code of a case whose designed procedure has a step outside the request space's grant.
pub const PROCEDURE_NOT_ENTITLED: &str = "not_entitled";
/// The state code of a case whose designed procedure's steps do not map.
pub const PROCEDURE_INVALID: &str = "not_offered";

/// What [`DesignProcedures`] runs for a request.
enum Chosen<'d> {
    /// A designed procedure and its phases.
    Designed(&'d TemplateProcedure, Vec<Phase>),
    /// A designed procedure serves the request but may not run (its state code): no plan, a handover.
    Refused(&'static str),
    /// No designed procedure serves the request.
    Static,
}

/// The phases of a designed procedure: its steps grouped by `phase` ascending, by `step_number` within one.
pub fn designed_phases(p: &TemplateProcedure) -> Result<Vec<Phase>, String> {
    let mut by_phase: std::collections::BTreeMap<u32, Vec<Step>> =
        std::collections::BTreeMap::new();
    for d in &p.step_defs {
        by_phase
            .entry(d.phase)
            .or_default()
            .push(Step::from_design(d)?);
    }
    Ok(by_phase
        .into_values()
        .map(|mut steps| {
            steps.sort_by_key(|s| s.step_number);
            Phase { steps }
        })
        .collect())
}

/// The designed procedure serving `req`, if any (rule 1 of [`DesignProcedures`]: its stored space is the request's).
fn designed_for<'d>(
    design: &'d RuntimeDesign,
    req: &QualifiedRequest,
) -> Option<&'d TemplateProcedure> {
    let mut by_space: Vec<&TemplateProcedure> = design
        .procedures
        .iter()
        .filter(|p| {
            !p.step_defs.is_empty()
                && p.context_space.as_deref() == Some(req.context_space.as_str())
        })
        .collect();
    // a tenant's own published procedure shadows the template's for its space; within each, first by name
    by_space.sort_by(|a, b| {
        b.tenant
            .cmp(&a.tenant)
            .then(a.name.cmp(&b.name))
            .then(b.version.cmp(&a.version))
    });
    let first = *by_space.first()?;
    let mut names: Vec<&str> = by_space.iter().map(|p| p.name.as_str()).collect();
    names.dedup();
    if names.len() > 1 {
        tracing::warn!(
            context_space = %req.context_space,
            procedures = ?names,
            chosen = %first.name,
            "several designed procedures serve one context space; the first by name runs"
        );
    }
    Some(first)
}

/// The step operations (`object.verb{variant}`) of `p` outside the grant of the request space's activity, or
/// `Err` naming the activity when it is empty or unknown to the design (nothing is granted).
fn outside_grant(
    design: &RuntimeDesign,
    req: &QualifiedRequest,
    p: &TemplateProcedure,
) -> Result<Vec<String>, String> {
    let activity = design
        .spaces
        .by_key(&req.context_space)
        .map(|s| s.activity.as_str())
        .unwrap_or_default();
    let grant = DesignToolsets.activity_grant(design, activity);
    // A known activity can have no connected operations and still use pure knowledge/rule builtins.
    // Missing or unknown activities remain refused; the runner separately enforces role and tenant narrowings.
    if grant.is_empty() && !DesignToolsets.governed(design, "procedure", Some(activity)) {
        return Err(activity.to_owned());
    }
    let mut outside: Vec<String> = Vec::new();
    for s in &p.step_defs {
        let label = op_label(&s.operation, s.variant.as_deref());
        if !outside.contains(&label) && !grant_allows(&grant, &label) {
            outside.push(label);
        }
    }
    Ok(outside)
}

fn choose<'d>(design: &'d RuntimeDesign, req: &QualifiedRequest) -> Chosen<'d> {
    let Some(p) = designed_for(design, req) else {
        return Chosen::Static;
    };
    match outside_grant(design, req, p) {
        Ok(outside) if outside.is_empty() => {}
        // a TEMPLATE procedure the job's grant does not cover is skipped (the builtin runs); a tenant's own
        // published procedure is the author's choice and stays refused
        Ok(outside) if !p.tenant => {
            tracing::warn!(
                procedure = %p.name, version = p.version, context_space = %req.context_space, ops = ?outside,
                "template procedure skipped: steps outside the grant of the request space's activity; the builtin runs"
            );
            return Chosen::Static;
        }
        Ok(outside) => {
            tracing::warn!(
                procedure = %p.name, version = p.version, context_space = %req.context_space, ops = ?outside,
                "designed procedure refused: steps outside the grant of the request space's activity"
            );
            return Chosen::Refused(PROCEDURE_NOT_ENTITLED);
        }
        Err(activity) if !p.tenant => {
            tracing::warn!(
                procedure = %p.name, version = p.version, context_space = %req.context_space, activity = %activity,
                "template procedure skipped: the request space's activity grants nothing; the builtin runs"
            );
            return Chosen::Static;
        }
        Err(activity) => {
            let ops: Vec<String> = p
                .step_defs
                .iter()
                .map(|s| op_label(&s.operation, s.variant.as_deref()))
                .collect();
            tracing::warn!(
                procedure = %p.name, version = p.version, context_space = %req.context_space,
                activity = %activity, ops = ?ops,
                "designed procedure refused: the request space's activity grants nothing"
            );
            return Chosen::Refused(PROCEDURE_NOT_ENTITLED);
        }
    }
    match designed_phases(p) {
        Ok(phases) => Chosen::Designed(p, phases),
        Err(e) => {
            tracing::warn!(
                procedure = %p.name, version = p.version, context_space = %req.context_space, error = %e,
                "designed procedure refused: its steps do not map"
            );
            Chosen::Refused(PROCEDURE_INVALID)
        }
    }
}

/// Authored knowledge answers keep the same grounding requirement as the built-in plan, addressed by their
/// actual step names. Other authored plans retain their existing inherited checks.
fn designed_checks(p: &TemplateProcedure, fallback: ProcedureChecks) -> ProcedureChecks {
    if !p.success_criteria.is_empty() || !p.tool_requirements.is_empty() {
        return ProcedureChecks {
            success_criteria: p.success_criteria.clone(),
            tool_requirements: p.tool_requirements.clone(),
        };
    }
    let answers: Vec<_> = p
        .step_defs
        .iter()
        .filter(|s| s.operation == builtin::ANSWER_OP)
        .collect();
    if answers.is_empty() {
        return fallback;
    }
    ProcedureChecks {
        success_criteria: answers
            .iter()
            .enumerate()
            .map(|(i, s)| Condition {
                id: if i == 0 {
                    ANSWER_GROUNDED.to_owned()
                } else {
                    format!("{ANSWER_GROUNDED}_{}", s.name)
                },
                fact: format!("steps.{}.out.grounded", s.name),
                op: CondOp::Eq,
                value: Some(json!(true)),
            })
            .collect(),
        tool_requirements: vec![builtin::ANSWER_OP.to_owned()],
    }
}

impl ProcedureSource for DesignProcedures {
    fn plan(
        &self,
        design: &RuntimeDesign,
        req: &QualifiedRequest,
    ) -> Option<(Option<ProcedureRef>, Vec<Phase>)> {
        match choose(design, req) {
            Chosen::Designed(p, mut phases) => {
                bind_knowledge_steps(&mut phases, design, req);
                Some((
                    Some(ProcedureRef {
                        name: p.name.clone(),
                        version: p.version,
                    }),
                    phases,
                ))
            }
            Chosen::Refused(_) => None,
            Chosen::Static => StaticProcedures.plan(design, req),
        }
    }

    fn checks(&self, design: &RuntimeDesign, req: &QualifiedRequest) -> ProcedureChecks {
        let fallback = StaticProcedures.checks(design, req);
        match choose(design, req) {
            Chosen::Designed(p, _) => designed_checks(p, fallback),
            _ => fallback,
        }
    }

    fn refusal(&self, design: &RuntimeDesign, req: &QualifiedRequest) -> Option<&'static str> {
        match choose(design, req) {
            Chosen::Refused(code) => Some(code),
            _ => None,
        }
    }
}

/// Customer-safe reason for a failed step: never the raw error (URLs, hosts).
pub fn failure_reason(operation: &str, code: Option<&str>) -> String {
    match code {
        Some("timeout") => format!("{operation} did not answer in time"),
        Some("not_found") => format!("{operation} found nothing"),
        _ => format!("{operation} is unavailable"),
    }
}

mod context;
mod slots;

/// One scope's orchestrator.
pub struct Orchestrator<P: ProcedureSource> {
    bus: Bus,
    scope: Scope,
    /// The runtime pool (`agent_runtime`): every query runs in a tenant + job scoped transaction.
    db: PgPool,
    reg: Arc<Registry>,
    designs: Arc<DesignCache>,
    cases: CaseStore,
    procedures: P,
    /// T6a: resolves the plan's `tool_requirements` (the runner's resolver: B7's routes, then the defaults).
    routes: Arc<dyn RouteResolver>,
    consumer: String,
    serving: Option<Serving>,
    job: OnceLock<JobRef>,
    /// T7a: runs the `assistant` role's cases; `None` = no assistant (a request without procedure hands over).
    assistant: Option<Arc<Assistant>>,
    /// How many attempts a retryable failed step gets in all.
    pub max_attempts: u32,
    pub consume: ConsumeOptions,
}

impl<P: ProcedureSource> Orchestrator<P> {
    /// On a scoped bus (refuses the root bus).
    pub fn new(
        bus: Bus,
        db: PgPool,
        reg: Arc<Registry>,
        designs: Arc<DesignCache>,
        procedures: P,
    ) -> Result<Self> {
        let scope = bus
            .scope()
            .cloned()
            .context("the orchestrator runs on a scoped bus")?;
        Ok(Self {
            cases: CaseStore::new(bus.clone()),
            // B8 T15: shadow mode turns a `not_offered` operation into a handover while it is on
            routes: Arc::new(crate::discovery::shadow::ShadowRoutes::new(Arc::new(
                EffectiveRoutes::new(Arc::clone(&reg)),
            ))),
            bus,
            scope,
            db,
            reg,
            designs,
            procedures,
            consumer: super::consumer_name(ROLE),
            serving: None,
            job: OnceLock::new(),
            assistant: None,
            max_attempts: 2,
            consume: consume_options(),
        })
    }

    /// Fail unresolved cases older than `d` (`CASE_DEADLINE_MS`).
    pub fn with_case_deadline(mut self, d: Duration) -> Self {
        self.cases = self.cases.clone().with_deadline(d);
        self
    }

    pub fn with_consumer(mut self, consumer: impl Into<String>) -> Self {
        self.consumer = consumer.into();
        self
    }

    pub fn with_consume(mut self, opts: ConsumeOptions) -> Self {
        self.consume = opts;
        self
    }

    /// Replace the route resolver the plan's `tool_requirements` are checked with (T6a).
    pub fn with_routes(mut self, routes: Arc<dyn RouteResolver>) -> Self {
        self.routes = routes;
        self
    }

    /// T7a: run the `assistant` role's cases with `a` (its switch decides per activity).
    pub fn with_assistant(mut self, a: Arc<Assistant>) -> Self {
        self.assistant = Some(a);
        self
    }

    /// Report the design each scope serves on the process heartbeat.
    pub fn with_serving(mut self, serving: Serving) -> Self {
        self.serving = Some(serving);
        self
    }

    pub fn cases(&self) -> &CaseStore {
        &self.cases
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// Consume the scope's customer streams (one per area of its context spaces) and `bus:op:results`, and sweep
    /// overdue cases, until a consumer fails or the scope's areas change (the supervisor then restarts the task,
    /// which reads the new areas).
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let areas = self.areas().await?;
        tracing::info!(scope = %self.scope, areas = ?areas, "orchestrator serving");
        let me: &Self = &self;
        let streams = &me.bus.streams;
        let customer: Vec<String> = areas.iter().map(|a| streams.customer(a)).collect();
        let customers = async move {
            if customer.is_empty() {
                std::future::pending::<Result<()>>().await
            } else {
                me.bus
                    .consume_all(
                        customer,
                        groups::ORCHESTRATOR,
                        &me.consumer,
                        me.consume.clone(),
                        move |env| me.on_customer(env),
                    )
                    .await
            }
        };
        let op_results = streams.op_results();
        let results = me.bus.consume(
            &op_results,
            groups::ORCHESTRATOR,
            &me.consumer,
            me.consume.clone(),
            move |env| me.on_result(env),
        );
        tokio::select! {
            r = async { tokio::try_join!(customers, results).map(|_| ()) } => r,
            () = me.sweep_loop() => Ok(()),
            r = me.watch_areas(areas) => r,
        }
    }

    /// The areas of the scope's context spaces (sorted, distinct).
    async fn areas(&self) -> Result<Vec<String>> {
        let (tx, _job, design) = self.design().await?;
        tx.commit().await?;
        Ok(design.spaces.areas())
    }

    /// Return when the scope's areas differ from `serving` (checked every design refresh).
    async fn watch_areas(&self, serving: Vec<String>) -> Result<()> {
        loop {
            tokio::time::sleep(super::design::REFRESH).await;
            match self.areas().await {
                Ok(now) if now != serving => {
                    tracing::info!(scope = %self.scope, "context-space areas changed; restarting");
                    return Ok(());
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(scope = %self.scope, "reading the areas failed: {e:#}"),
            }
        }
    }

    /// A transaction scoped to the tenant and the scope's job (RLS), and the job.
    async fn begin(&self) -> Result<(Transaction<'static, Postgres>, JobRef)> {
        begin_scope(&self.db, &self.scope, &self.job).await
    }

    /// The scope's design (cached; dropped on a nudge) in a fresh scoped transaction.
    async fn design(&self) -> Result<(Transaction<'static, Postgres>, JobRef, Arc<RuntimeDesign>)> {
        let (mut tx, job) = self.begin().await?;
        let d = self
            .designs
            .get_or_load(&mut tx, &self.reg, &self.scope, &job)
            .await?;
        if let Some(s) = &self.serving {
            let (release, rev) = store::stamp(&d.case);
            s.set(&self.scope, release, rev);
        }
        Ok((tx, job, d))
    }

    // ---- deadline sweep ----------------------------------------------------------------------------------------

    async fn sweep_loop(&self) {
        let mut tick = tokio::time::interval(SWEEP_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            match self.sweep_once().await {
                Ok(0) => {}
                Ok(n) => {
                    tracing::warn!(scope = %self.scope, failed = n, "deadline sweep failed overdue cases")
                }
                Err(e) => tracing::error!(scope = %self.scope, "deadline sweep failed: {e:#}"),
            }
        }
    }

    /// Fail every case past its deadline; returns how many this replica failed. At most one replica wins each case
    /// (a leased claim), and the first resolution is the one every replica publishes.
    pub async fn sweep_once(&self) -> Result<usize> {
        let now = self.cases.now_ms().await?;
        let mut failed = 0;
        for case_id in self.cases.overdue(now, SWEEP_BATCH).await? {
            if !self.cases.claim_overdue(&case_id, now).await? {
                continue;
            }
            match self.sweep_case(&case_id).await {
                Ok(true) => failed += 1,
                Ok(false) => {}
                // retried when the lease runs out
                Err(e) => tracing::error!(%case_id, "sweeping an overdue case failed: {e:#}"),
            }
        }
        Ok(failed)
    }

    async fn sweep_case(&self, case_id: &str) -> Result<bool> {
        if self.cases.is_resolved(case_id).await? {
            self.cases.drop_deadline(case_id).await?;
            return Ok(false);
        }
        let Some(req) = self.cases.request(case_id).await? else {
            tracing::warn!(%case_id, "overdue case has no request (hash expired); dropped from the deadline set");
            self.cases.drop_deadline(case_id).await?;
            return Ok(false);
        };
        // the earliest still-outstanding call names the step that did not answer
        let mut open: Vec<Value> = self
            .cases
            .calls(case_id)
            .await?
            .into_iter()
            .filter(|c| c["status"] == "outstanding")
            .collect();
        open.sort_by_key(|c| {
            (
                c["phase"].as_u64().unwrap_or(u64::MAX),
                c["step"].as_str().unwrap_or_default().to_owned(),
            )
        });
        let first = open
            .first()
            .and_then(|c| c["operation"].as_str())
            .map(str::to_owned);
        let waited = self.cases.deadline();
        let mut ops: Vec<&str> = open
            .iter()
            .filter_map(|c| c["operation"].as_str())
            .collect();
        ops.sort_unstable();
        ops.dedup();
        let detail = format!(
            "deadline exceeded after {} ms; outstanding: {}",
            waited.as_millis(),
            ops.join(", ")
        );
        tracing::warn!(%case_id, "case deadline exceeded");
        self.cases.set_error(case_id, &detail).await?;
        let reason = match first {
            Some(op) => format!("{op} did not answer in time"),
            None => "the request did not finish in time".to_owned(),
        };
        let plan = self.cases.plan(case_id).await?;
        self.finish(
            &req,
            plan.as_ref(),
            CaseOutcome::Failed,
            Some("deadline_exceeded"),
            Some(reason),
        )
        .await?;
        Ok(true)
    }

    // ---- handlers ----------------------------------------------------------------------------------------------

    /// `bus:customer:{area}` carries both directions; only `request.qualified` is the orchestrator's.
    pub async fn on_customer(&self, env: Envelope) -> Result<()> {
        match env.event() {
            Some(REQUEST_QUALIFIED) => self.on_qualified(env).await,
            _ => Ok(()),
        }
    }

    pub async fn on_qualified(&self, env: Envelope) -> Result<()> {
        let req: QualifiedRequest = env.payload_as()?;
        if self.cases.create(&req).await? {
            self.observe_shadow(&req).await;
        } else if self.cases.reopen(&req).await? {
            // A4 `case_per_thread`: a later message of the thread continues its resolved case (a new round)
            tracing::info!(case_id = %req.case_id, "case_continued");
            // the new round is a new turn: its own time and tool-call budget, the earlier messages stay
            if let Some(mut state) = self.assistant_state(&req.case_id).await? {
                state.started_ms = self.cases.now_ms().await?;
                state.used = 0;
                self.save_assistant_state(&req.case_id, &state).await?;
            }
        }
        if self.cases.is_resolved(&req.case_id).await? {
            return Ok(());
        }
        let plan = match self.cases.plan(&req.case_id).await? {
            Some(p) => p,
            None => match self.make_plan(&req).await? {
                Some(p) => p,
                // no procedure: handed over, resolved
                None => return Ok(()),
            },
        };
        // a crash between the plan and its row write is repaired by the redelivery
        if !self.cases.plan_recorded(&req.case_id).await? {
            self.record_plan(&req, &plan).await?;
        }
        self.proceed(&req, &plan).await
    }

    /// The case is planned and open: check its boundary, load the identified customer's context (B4 known customer,
    /// [`context`]; the case waits for it and is proceeded again by the last answer), then run the plan.
    async fn proceed(&self, req: &QualifiedRequest, plan: &Plan) -> Result<()> {
        match self.boundary(req).await? {
            Boundary::Go => {}
            Boundary::Stop(outcome) => {
                return self
                    .finish(req, Some(plan), outcome, Some("handed_over"), None)
                    .await;
            }
            Boundary::Closed => return self.cases.mark_resolved(&req.case_id, "closed").await,
        }
        match self.cases.phase(&req.case_id).await? {
            CasePhase::HandedOver => {
                self.finish(
                    req,
                    Some(plan),
                    CaseOutcome::Handover,
                    Some("handed_over"),
                    None,
                )
                .await
            }
            CasePhase::At(i) if plan.assistant.is_some() => {
                if !self.context_ready(req, plan).await? || !self.slots_ready(req, plan).await? {
                    return Ok(());
                }
                self.assistant_round(req, plan, i).await
            }
            CasePhase::At(i) => {
                // T6a: before the first step, every operation the procedure needs must be offered
                if i == 0
                    && self.cases.calls(&req.case_id).await?.is_empty()
                    && let Some(op) = self.unoffered_requirement(plan).await?
                {
                    let trigger = format!("procedure needs {op}, which is not offered");
                    return self.hand_over(req, plan, "not_offered", &trigger).await;
                }
                if !self.context_ready(req, plan).await? || !self.slots_ready(req, plan).await? {
                    return Ok(());
                }
                self.start_phase(req, plan, i).await
            }
        }
    }

    pub async fn on_result(&self, env: Envelope) -> Result<()> {
        if env.event() != Some(OP_RESULT) {
            return Ok(());
        }
        let r: OpResult = env.payload_as()?;
        if self.cases.is_resolved(&r.case_id).await? {
            return Ok(());
        }
        let Some(req) = self.cases.request(&r.case_id).await? else {
            tracing::warn!(case_id = %r.case_id, call_id = %r.call_id, "op.result for an unknown case; dropped");
            return Ok(());
        };
        // B4 known customer: an answer of the context phase
        if r.step.starts_with(super::customer_context::STEP_PREFIX) {
            return self.context_result(&req, r).await;
        }
        let plan = self
            .cases
            .plan(&r.case_id)
            .await?
            .with_context(|| format!("case {} has no plan", r.case_id))?;
        if plan.assistant.is_some() {
            return self.assistant_result(&req, &plan, r).await;
        }
        let Some(step) = plan.step(r.phase, &r.step).cloned() else {
            tracing::warn!(case_id = %r.case_id, phase = r.phase, step = %r.step, "op.result for a step not in the plan; dropped");
            return Ok(());
        };
        self.cases
            .set_call_status(
                &r.case_id,
                &r.call_id,
                &json!({"call_id": r.call_id, "phase": r.phase, "step": r.step, "operation": r.operation,
                        "attempt": r.attempt, "status": r.state, "outcome_code": r.outcome_code,
                        "instance_id": r.instance_id, "latency_ms": r.latency_ms}),
            )
            .await?;
        if self.cases.phase(&r.case_id).await? == CasePhase::HandedOver {
            return self
                .finish(
                    &req,
                    Some(&plan),
                    CaseOutcome::Handover,
                    Some("handed_over"),
                    None,
                )
                .await;
        }
        let code = r.outcome_code.as_deref();
        match r.state {
            // T6a: the runner alerted the case and queued its handover (`tool_failed`); never retried
            OpState::Ok if code == Some(POSTCONDITION_FAILED) => {
                self.cases
                    .record_result(
                        &r.case_id,
                        r.phase,
                        &r.step,
                        r.output.as_ref().unwrap_or(&Value::Null),
                    )
                    .await?;
                self.finish(&req, Some(&plan), CaseOutcome::Handover, code, None)
                    .await
            }
            OpState::Ok => {
                self.cases
                    .record_result(
                        &r.case_id,
                        r.phase,
                        &r.step,
                        r.output.as_ref().unwrap_or(&Value::Null),
                    )
                    .await?;
                self.maybe_advance(&req, &plan, r.phase).await
            }
            // the runner already alerted the case and queued the handover (T6 step 10); never retried
            OpState::UnknownOutcome => {
                self.finish(
                    &req,
                    Some(&plan),
                    CaseOutcome::Handover,
                    Some("unknown_outcome"),
                    None,
                )
                .await
            }
            // T6a: the runner queued the handover (`tool_failed`) at the per-case cap
            OpState::Denied if code == Some(cap::CODE) => {
                self.finish(&req, Some(&plan), CaseOutcome::Handover, code, None)
                    .await
            }
            // T6a: an unmet precondition ends a gate step not_eligible and hands any other step over
            OpState::Denied if code == Some(PRECONDITION_UNMET) && step.gate => {
                self.finish(&req, Some(&plan), CaseOutcome::NotEligible, code, None)
                    .await
            }
            OpState::Denied if code == Some(PRECONDITION_UNMET) => {
                let trigger = format!("preconditions unmet for {}", step.op_key());
                self.hand_over(&req, &plan, PRECONDITION_UNMET, &trigger)
                    .await
            }
            // audit of agent#64: the routed system cannot answer the step as asked (runner step 6b: its variant is
            // served by an operation needing inputs the step does not carry) — a colleague takes it, not an outage
            OpState::Denied if code == Some("not_offered") => {
                let trigger = format!("{} is not offered by the routed system", step.op_key());
                self.hand_over(&req, &plan, "not_offered", &trigger).await
            }
            // A2: the step needs a value only the user has: park the case on the question, the next message answers it
            OpState::Error if code == Some(declared::INPUT_REQUIRED) => {
                self.park_input(&req, &plan, &r).await
            }
            // A2: a declared input could not be filled (a producer failed, no source, no secret store): a colleague
            OpState::Error if input_failed(code) => {
                let trigger = format!("{} could not get a declared input", step.op_key());
                self.hand_over(&req, &plan, "tool_failed", &trigger).await
            }
            // A2: the conversation lacks state the step requires (no known subject): a colleague, not an outage
            OpState::Denied if code == Some(declared::STATE_REQUIRED) => {
                let trigger = format!("{} requires state the conversation lacks", step.op_key());
                self.hand_over(&req, &plan, "tool_failed", &trigger).await
            }
            // a known customer starts at level 2 and is never asked for a factor: a step that needs level 3
            // (step-up) goes to a colleague, who confirms the one more thing
            OpState::Denied if code == Some("identity_required") => {
                let trigger = format!("{} needs a step-up", step.op_key());
                self.hand_over(&req, &plan, STEP_UP_REQUIRED, &trigger)
                    .await
            }
            OpState::Denied => {
                let outcome = match code {
                    Some("confirmation_required") => CaseOutcome::ConfirmationRequired,
                    _ => CaseOutcome::Failed,
                };
                let reason =
                    (outcome == CaseOutcome::Failed).then(|| failure_reason(&r.operation, code));
                self.finish(&req, Some(&plan), outcome, code.or(Some("denied")), reason)
                    .await
            }
            OpState::Error if r.retryable && r.attempt < self.max_attempts => {
                let ctx = self.cases.context(&req).await?;
                self.dispatch(
                    &req,
                    &plan,
                    r.phase,
                    &step,
                    resolve_args(&step.args, &ctx),
                    r.attempt + 1,
                )
                .await
            }
            OpState::Error => {
                tracing::warn!(case_id = %r.case_id, call_id = %r.call_id, operation = %r.operation,
                    code = code.unwrap_or("error"), "step failed; the case fails");
                self.finish(
                    &req,
                    Some(&plan),
                    CaseOutcome::Failed,
                    code.or(Some("error")),
                    Some(failure_reason(&r.operation, code)),
                )
                .await
            }
        }
    }

    // ---- planning ----------------------------------------------------------------------------------------------

    /// Read the scope's design now (plan time) and store the first plan; `None` when the job has no procedure for
    /// the request (the case was handed over and resolved).
    async fn make_plan(&self, req: &QualifiedRequest) -> Result<Option<Plan>> {
        let (tx, job, design) = self.design().await?;
        let planned = self.procedures.plan(&design, req);
        // a refused designed procedure hands over: never the assistant, never `out_of_scope`
        if planned.is_none()
            && let Some(code) = self.procedures.refusal(&design, req)
        {
            self.refused(tx, &job, req, &design.case, code).await?;
            return Ok(None);
        }
        match planned {
            Some((procedure, phases)) => {
                drop(tx);
                let checks = self.procedures.checks(&design, req);
                let proposed = Plan {
                    procedure,
                    design: design.case.clone(),
                    phases,
                    success_criteria: checks.success_criteria,
                    tool_requirements: checks.tool_requirements,
                    assistant: None,
                };
                Ok(Some(self.cases.set_plan(&req.case_id, &proposed).await?))
            }
            // T7a: no procedure, the assistant role on for the activity: the same design, no phases
            None if self.assistant_on(&design, req) => {
                drop(tx);
                let proposed = Plan {
                    procedure: None,
                    design: design.case.clone(),
                    phases: Vec::new(),
                    success_criteria: Vec::new(),
                    tool_requirements: Vec::new(),
                    assistant: Some(AssistantPlan {
                        role: ROLE_ASSISTANT.to_owned(),
                        activity: req.activity.clone(),
                    }),
                };
                Ok(Some(self.cases.set_plan(&req.case_id, &proposed).await?))
            }
            None => {
                self.no_procedure(tx, &job, req, &design.case).await?;
                Ok(None)
            }
        }
    }

    /// Write the plan onto the case row: the plan-time design (when it differs from the qualify-time stamp), the
    /// procedure version and the space, plus a `state` event, in one transaction.
    async fn record_plan(&self, req: &QualifiedRequest, plan: &Plan) -> Result<()> {
        let (mut tx, job) = self.begin().await?;
        let row: Option<(Option<String>, Option<i64>)> =
            sqlx::query_as("SELECT release_id, draft_rev FROM agent_case WHERE case_id = $1")
                .bind(&req.case_id)
                .fetch_optional(&mut *tx)
                .await?;
        let stored = row.with_context(|| {
            format!(
                "case {} has no agent_case row (the drain writes it before publishing)",
                req.case_id
            )
        })?;
        let (release_id, draft_rev) = store::stamp(&plan.design);
        if stored != (release_id.clone(), draft_rev) {
            sqlx::query("UPDATE agent_case SET release_id = $2, draft_rev = $3 WHERE case_id = $1")
                .bind(&req.case_id)
                .bind(&release_id)
                .bind(draft_rev)
                .execute(&mut *tx)
                .await?;
        }
        let (name, version) = match &plan.procedure {
            Some(p) => (Some(p.name.clone()), i32::try_from(p.version).ok()),
            None => (None, None),
        };
        sqlx::query(
            "UPDATE agent_case SET procedure_name = $2, procedure_version = $3, \
                    context_space = COALESCE(context_space, $4), request_type = COALESCE(request_type, $5), \
                    activity = COALESCE(activity, $6), updated_at = now() \
              WHERE case_id = $1",
        )
        .bind(&req.case_id)
        .bind(name)
        .bind(version)
        .bind(&req.context_space)
        .bind(&req.request_type)
        .bind(&req.activity)
        .execute(&mut *tx)
        .await?;
        state_event(
            &mut tx,
            &job,
            &req.case_id,
            json!({"status": "open", "event": "planned", "procedure": plan.procedure_label(),
                   "phases": plan.phases.len(), "release_id": release_id, "draft_rev": draft_rev}),
        )
        .await?;
        tx.commit().await?;
        self.cases.mark_plan_recorded(&req.case_id).await
    }

    /// The T7a predicate: an assistant is attached and its role is on for the request's activity.
    fn assistant_on(&self, design: &RuntimeDesign, req: &QualifiedRequest) -> bool {
        self.assistant
            .as_ref()
            .is_some_and(|a| a.enabled(design, req.activity.as_deref()))
    }

    /// No procedure for the request: queue a handover (`out_of_scope`) with the case's `state` event, then resolve.
    async fn no_procedure(
        &self,
        tx: Transaction<'static, Postgres>,
        job: &JobRef,
        req: &QualifiedRequest,
        design: &CaseDesign,
    ) -> Result<()> {
        self.out_of_scope(
            tx,
            job,
            req,
            design,
            None,
            "no procedure serves this request",
            "not_offered",
        )
        .await
    }

    /// A designed procedure serves the request but its source refused it at plan time
    /// ([`ProcedureSource::refusal`]): queue a handover `tool_failed` (never `out_of_scope`: the job does have a
    /// procedure for the request, the design is what is wrong) with the case's `state` event (`code`), then resolve.
    async fn refused(
        &self,
        tx: Transaction<'static, Postgres>,
        job: &JobRef,
        req: &QualifiedRequest,
        design: &CaseDesign,
        code: &str,
    ) -> Result<()> {
        self.queue_unplanned(
            tx,
            job,
            req,
            design,
            None,
            "tool_failed",
            "a designed procedure was refused at plan time",
            code,
        )
        .await
    }

    /// Queue a handover `out_of_scope` (`trigger`: a fixed text, never case data) with the case's `state` event
    /// (`code`), commit, then resolve the case `handover`.
    #[allow(clippy::too_many_arguments)]
    async fn out_of_scope(
        &self,
        tx: Transaction<'static, Postgres>,
        job: &JobRef,
        req: &QualifiedRequest,
        design: &CaseDesign,
        plan: Option<&Plan>,
        trigger: &str,
        code: &str,
    ) -> Result<()> {
        self.queue_unplanned(tx, job, req, design, plan, "out_of_scope", trigger, code)
            .await
    }

    /// Queue a handover `reason` (one of [`agent_cases::REASONS`]; `trigger`: a fixed text, never case data) with
    /// the case's `state` event (`code`), commit, then resolve the case `handover`.
    #[allow(clippy::too_many_arguments)]
    async fn queue_unplanned(
        &self,
        mut tx: Transaction<'static, Postgres>,
        job: &JobRef,
        req: &QualifiedRequest,
        design: &CaseDesign,
        plan: Option<&Plan>,
        reason: &str,
        trigger: &str,
        code: &str,
    ) -> Result<()> {
        // out-of-hours aware (B4 T16a): `agent_cases::queue` stamps the item closed at the database's now()
        let q = agent_cases::queue(
            &mut tx,
            job,
            &req.case_id,
            &req.conversation_id,
            reason,
            trigger,
            &req.context_space,
            design,
        )
        .await?;
        if let Queued::Created(_) = q {
            state_event(
                &mut tx,
                job,
                &req.case_id,
                json!({"status": "handover", "code": code}),
            )
            .await?;
        }
        tx.commit().await?;
        if let Queued::Created(item) = &q {
            self.notify_workers(job, item).await;
        }
        self.finish(req, plan, CaseOutcome::Handover, Some(code), None)
            .await
    }

    // ---- phases ------------------------------------------------------------------------------------------------

    /// Safe to call repeatedly: call ids are deterministic and steps with a recorded result are skipped.
    async fn start_phase(&self, req: &QualifiedRequest, plan: &Plan, idx: usize) -> Result<()> {
        let Some(phase) = plan.phases.get(idx) else {
            // T6a: the procedure's success criteria decide whether the finished case resolves
            if !plan.success_criteria.is_empty() {
                let results = self.cases.results(&req.case_id).await?;
                let resolved = self.cases.resolved_slots(&req.case_id).await?;
                let failed = conditions::failed(
                    &plan.success_criteria,
                    &case_facts(req, plan, &results, &resolved),
                );
                if !failed.is_empty() {
                    let trigger = format!("success criteria unmet: {}", failed.join(", "));
                    return self
                        .hand_over(req, plan, SUCCESS_CRITERIA_UNMET, &trigger)
                        .await;
                }
            }
            return self
                .finish(req, Some(plan), CaseOutcome::Resolved, None, None)
                .await;
        };
        self.cases
            .init_pending(&req.case_id, idx, phase.steps.len())
            .await?;
        let ctx = self.cases.context(req).await?;
        for step in &phase.steps {
            if !self.cases.has_result(&req.case_id, &step.name).await? {
                self.dispatch(req, plan, idx, step, resolve_args(&step.args, &ctx), 1)
                    .await?;
            }
        }
        // a redelivery after every result landed has nothing to dispatch
        self.maybe_advance(req, plan, idx).await
    }

    async fn maybe_advance(&self, req: &QualifiedRequest, plan: &Plan, idx: usize) -> Result<()> {
        if self.cases.remaining(&req.case_id, idx).await? > 0 {
            return Ok(());
        }
        match self.boundary(req).await? {
            Boundary::Go => {}
            Boundary::Stop(outcome) => {
                return self
                    .finish(req, Some(plan), outcome, Some("handed_over"), None)
                    .await;
            }
            Boundary::Closed => return self.cases.mark_resolved(&req.case_id, "closed").await,
        }
        let results = self.cases.results(&req.case_id).await?;
        if let Some(phase) = plan.phases.get(idx) {
            for step in phase.steps.iter().filter(|s| s.gate) {
                let out = results.get(&step.name);
                if answered_false(out) {
                    let text = |k: &str| out.and_then(|o| o.get(k)).and_then(Value::as_str);
                    // a rule that cannot decide (`placeholder`: values unset; `escalate`) is not an answer for the
                    // customer: its reason is internal, so a colleague takes the case instead (acceptance A1 defect)
                    if let Some(undecided @ ("placeholder" | "escalate")) = text("outcome") {
                        let code = text("code").filter(|c| !c.is_empty()).unwrap_or(undecided);
                        let trigger = format!("{} undecided ({undecided})", step.op_key());
                        return self.hand_over(req, plan, code, &trigger).await;
                    }
                    let code = text("code").unwrap_or("not_eligible").to_owned();
                    // only the platform's rule speaks to the customer: its reason is the rule's authored text; a
                    // connector's gate text is untrusted, so the reply is the fixed one (its output stays in results)
                    let reason = text("reason")
                        .filter(|_| step.operation == "rule.evaluate")
                        .map(str::to_owned);
                    return self
                        .finish(
                            req,
                            Some(plan),
                            CaseOutcome::NotEligible,
                            Some(&code),
                            reason,
                        )
                        .await;
                }
            }
        }
        self.cases.advance_phase(&req.case_id, idx, idx + 1).await?;
        if self.cases.phase(&req.case_id).await? == CasePhase::At(idx + 1) {
            return Box::pin(self.start_phase(req, plan, idx + 1)).await;
        }
        Ok(())
    }

    /// At a phase boundary: is the case still the agent's? (`agent_case.status` is the record: a staff hand-over
    /// writes only the row.)
    async fn boundary(&self, req: &QualifiedRequest) -> Result<Boundary> {
        if self.cases.phase(&req.case_id).await? == CasePhase::HandedOver {
            return Ok(Boundary::Stop(CaseOutcome::Handover));
        }
        let (mut tx, _job) = self.begin().await?;
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM agent_case WHERE case_id = $1")
                .bind(&req.case_id)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(match status.as_deref() {
            Some("open") | None => Boundary::Go,
            Some("handover") => Boundary::Stop(CaseOutcome::Handover),
            Some(_) => Boundary::Closed,
        })
    }

    async fn dispatch(
        &self,
        req: &QualifiedRequest,
        plan: &Plan,
        phase: usize,
        step: &Step,
        input: Map<String, Value>,
        attempt: u32,
    ) -> Result<()> {
        let id = call_id(&req.case_id, phase, &step.name, attempt);
        let op_key = step.op_key();
        // reads carry no idempotency key (12 §3.7); an operation the catalogue does not know is treated as a write
        let design = self.runtime_design().await?;
        let writes = design.catalog.get(&op_key).is_none_or(|o| o.writes());
        let deadline_ms = match self.cases.due_ms(&req.case_id).await? {
            Some(due) => u64::try_from(due).unwrap_or(0),
            None => {
                let now = u64::try_from(self.cases.now_ms().await?).unwrap_or(0);
                now.saturating_add(u64::try_from(self.cases.deadline().as_millis()).unwrap_or(0))
            }
        };
        let (release_id, draft_rev) = store::stamp(&plan.design);
        // T6a: the case facts travel only with a step that has conditions
        // B4 customer verification: the case's CURRENT level (a verify may have raised it), never the qualified one
        let level = self.current_level(req).await?;
        let facts = if step.has_conditions() {
            let results = self.cases.results(&req.case_id).await?;
            let resolved = self.cases.resolved_slots(&req.case_id).await?;
            let mut f = case_facts(req, plan, &results, &resolved);
            f.identity_level = level;
            f
        } else {
            CaseFacts::default()
        };
        // T7a: an assistant case's calls are the assistant role's
        let role = if plan.assistant.is_some() {
            ROLE_ASSISTANT
        } else {
            ROLE_PROCEDURE
        };
        let call = OpCall {
            call_id: id.clone(),
            case_id: req.case_id.clone(),
            conversation_id: req.conversation_id.clone(),
            phase,
            step: step.name.clone(),
            attempt,
            agent_role: role.to_owned(),
            activity: req.activity.clone(),
            operation: step.operation.clone(),
            variant: step.variant.clone(),
            input,
            idempotency_key: writes.then(|| idempotency_key(&req.case_id, phase, &step.name)),
            env: self.scope.env,
            release_id,
            draft_rev,
            confirm_ref: if writes { req.message_id.clone() } else { None },
            initiator: format!("{role}@{}", self.consumer),
            initiator_sub: req.initiator_sub.clone(),
            identity_level: level,
            procedure: plan.procedure_label(),
            deadline_ms,
            preconditions: step.preconditions.clone(),
            postconditions: step.postconditions.clone(),
            performed_by: step.performed_by.clone(),
            max_per_case: step.max_per_case,
            facts,
            provenance: None,
        };
        self.cases
            .set_call_status(
                &req.case_id,
                &id,
                &json!({"call_id": id, "phase": phase, "step": step.name, "operation": op_key,
                        "attempt": attempt, "status": "outstanding"}),
            )
            .await?;
        let env = Envelope::new(&req.topic(OP_CALL), ROLE, &req.case_id, &call)?
            .with_procedure(plan.procedure_label().as_deref());
        self.bus.publish(&self.bus.streams.op(), &env).await?;
        Ok(())
    }

    /// B8 T15: record the new case as a shadow-mode observation (a no-op unless shadow mode is on for the scope's job
    /// and env). Best effort: a failure is logged and never fails the case.
    async fn observe_shadow(&self, req: &QualifiedRequest) {
        let res: Result<()> = async {
            let (mut tx, job) = self.begin().await?;
            let o = crate::discovery::shadow::Observation {
                tenant: self.scope.tenant,
                job: job.id,
                case_id: &req.case_id,
                env: self.scope.env.as_str(),
                stage: None,
                activity: req.activity.as_deref(),
                labelled: req.activity.is_some(),
                text: None,
            };
            crate::discovery::shadow::observe(&mut tx, &o).await?;
            tx.commit().await?;
            Ok(())
        }
        .await;
        if let Err(e) = res {
            tracing::warn!(case_id = %req.case_id, error = %e, "shadow_observe_failed");
        }
    }

    /// T6a: the first of the plan's `tool_requirements` its route resolves `not_offered`, if any.
    async fn unoffered_requirement(&self, plan: &Plan) -> Result<Option<String>> {
        if plan.tool_requirements.is_empty() {
            return Ok(None);
        }
        let (mut tx, job) = self.begin().await?;
        let mut missing = None;
        for op in &plan.tool_requirements {
            let q = RouteQuery {
                tenant: self.scope.tenant,
                job_id: job.id,
                job_key: job.key.clone(),
                env: self.scope.env,
                op: op.clone(),
            };
            if matches!(
                self.routes.resolve(&mut tx, &q).await?,
                ResolvedTarget::NotOffered
            ) {
                missing = Some(op.clone());
                break;
            }
        }
        tx.commit().await?;
        Ok(missing)
    }

    /// T6a: hand the case over with the reason of `code` ([`hand_over_reason`]; `trigger`: an operation key or
    /// condition ids, never case data) and outcome `code`, with its `state` event, in one transaction; then tell the
    /// job's worker channels ([`Self::notify_workers`]) and resolve it `handover`.
    ///
    /// N-12 (one open J21 item per conversation), as [`Self::queue_failed`]: the conversation's control lock
    /// (`store::control_key`) is the first statement of the transaction, then the open item is looked up. When
    /// another case's item is open, no second item is queued and the case resolves `failed` (code `code`, no
    /// reason — the trigger is internal and `reply::compose` would show it to the customer) instead of `handover`: a `handover` case with no item of its own would sit open where no colleague
    /// sees it. The open item holds the conversation, so the interaction daemon withholds this case's reply
    /// (`open_handover`) and staff answer from the one item. `finish` re-checks under the same lock
    /// (`queue_failed`): if that item closed in between, this case queues its own and the customer hears "a colleague
    /// will follow up" — still one item.
    async fn hand_over(
        &self,
        req: &QualifiedRequest,
        plan: &Plan,
        code: &str,
        trigger: &str,
    ) -> Result<()> {
        let (mut tx, job) = self.begin().await?;
        // the control lock, then the owner row (the ownership commands' order, SW T5)
        ownership::guard(&mut tx, &req.conversation_id).await?;
        if let Some(item) = handover::open_for_conversation(&mut tx, &req.conversation_id).await?
            && item.case_id != req.case_id
        {
            tx.commit().await?;
            tracing::info!(case_id = %req.case_id, item = %item.id, code,
                "hand-over: the conversation already has an open handover item; the case fails");
            return self
                .finish(
                    req,
                    Some(plan),
                    CaseOutcome::Failed,
                    Some(code),
                    // never the trigger: `reply::compose` prints the reason to the customer, and a trigger is
                    // internal (an operation key / condition ids); `queue_failed` falls back to the code
                    None,
                )
                .await;
        }
        let q = agent_cases::queue(
            &mut tx,
            &job,
            &req.case_id,
            &req.conversation_id,
            hand_over_reason(code),
            trigger,
            &req.context_space,
            &plan.design,
        )
        .await?;
        match &q {
            Queued::Created(_) => {
                sqlx::query(
                    "UPDATE agent_case SET outcome = $2, updated_at = now() \
                      WHERE tenant_id = app_current_tenant() AND job_id = $3 AND case_id = $1",
                )
                .bind(&req.case_id)
                .bind(code)
                .bind(job.id)
                .execute(&mut *tx)
                .await?;
                state_event(
                    &mut tx,
                    &job,
                    &req.case_id,
                    json!({"status": "handover", "code": code, "event": trigger}),
                )
                .await?;
            }
            Queued::Already => {}
            Queued::NoConversation => {
                tracing::warn!(case_id = %req.case_id, code, "the case has no conversation to hand over");
            }
        }
        tx.commit().await?;
        if let Queued::Created(item) = &q {
            self.notify_workers(&job, item).await;
        }
        self.finish(req, Some(plan), CaseOutcome::Handover, Some(code), None)
            .await
    }

    /// After commit: tell the job's enabled worker bindings (helpdesk ticket, staff channels) about item `item` this
    /// orchestrator queued, as every other handover path does ([`handover::notify_workers`]). The runtime state it
    /// needs is the [`Assistant`]'s: the binary attaches one whenever `state::optional_runtime` assembles, its role
    /// switch deciding only whether cases run as assistant cases. Without it nothing is sent. Never fails the case:
    /// a failure is logged and the item stays queued.
    async fn notify_workers(&self, job: &JobRef, item: &str) {
        if let Some(a) = &self.assistant {
            handover::notify_workers(a.state(), self.scope.tenant, job.id, item).await;
        }
    }

    /// Queue the J21 item of a `failed` resolution on `tx`: reason `tool_failed` (B4 acceptance: the reply promises
    /// a colleague), trigger the customer-safe reason or the code (never case data, K50), outcome `code`, with its
    /// `state` event — as [`Self::hand_over`]. Nothing when another case's item is open in the conversation (N-12:
    /// one item per conversation; that item holds it) or there is no conversation. Idempotent: a redelivery finds
    /// the case's own item (`Queued::Already`). True when the case has its own open item, with that item's
    /// out-of-hours note when it was queued while the scope was closed ([`agent_cases::out_of_hours_note`]), and the
    /// item's id when this call created it (the caller notifies the workers after its commit).
    ///
    /// The check and the insert run under the conversation's control lock (`store::control_key`, taken first in
    /// `tx` as crate::runtime::interaction does): an empty `FOR UPDATE` locks no row, and the only unique index is
    /// per case, so two failed cases of one conversation resolving at once would otherwise both queue an item.
    async fn queue_failed(
        &self,
        tx: &mut PgConnection,
        job: &JobRef,
        res: &Resolution,
    ) -> Result<(bool, Option<OutOfHoursNote>, Option<String>)> {
        // the control lock, then the owner row (the ownership commands' order, SW T5)
        ownership::guard(&mut *tx, &res.conversation_id).await?;
        if let Some(item) = handover::open_for_conversation(&mut *tx, &res.conversation_id).await?
            && item.case_id != res.case_id
        {
            tracing::info!(case_id = %res.case_id, item = %item.id,
                "failed case: the conversation already has an open handover item");
            return Ok((false, None, None));
        }
        let design = match &res.design {
            Some(d) => d.clone(),
            // failed before planning (the deadline): the scope's design now
            None => self
                .designs
                .get_or_load(&mut *tx, &self.reg, &self.scope, job)
                .await?
                .case
                .clone(),
        };
        let code = res.code.as_deref().unwrap_or("failed");
        let trigger = format!("case failed: {}", res.reason.as_deref().unwrap_or(code));
        let q = agent_cases::queue(
            &mut *tx,
            job,
            &res.case_id,
            &res.conversation_id,
            "tool_failed",
            &trigger,
            &res.context_space,
            &design,
        )
        .await?;
        match &q {
            Queued::Created(_) => {
                sqlx::query(
                    "UPDATE agent_case SET outcome = $2, updated_at = now() WHERE case_id = $1",
                )
                .bind(&res.case_id)
                .bind(code)
                .execute(&mut *tx)
                .await?;
                state_event(
                    &mut *tx,
                    job,
                    &res.case_id,
                    json!({"status": "handover", "code": code, "event": trigger}),
                )
                .await?;
            }
            Queued::Already => {}
            Queued::NoConversation => {
                tracing::warn!(case_id = %res.case_id, code, "the failed case has no conversation to hand over");
                return Ok((false, None, None));
            }
        }
        let note = agent_cases::out_of_hours_note(&mut *tx, job, &res.case_id, &design).await?;
        let created = match q {
            Queued::Created(item) => Some(item),
            _ => None,
        };
        Ok((true, note, created))
    }

    // ---- T7a: the assistant loop ---------------------------------------------------------------------------------

    /// Round `round` of an assistant case: claim it (one model call per round, ever), check the turn budget, add
    /// the previous round's tool answers, call the model, meter the call, then resolve the case (no tool calls) or
    /// send the round's tool calls. Nothing is held across the model call.
    async fn assistant_round(
        &self,
        req: &QualifiedRequest,
        plan: &Plan,
        round: usize,
    ) -> Result<()> {
        let Some(a) = self.assistant.clone() else {
            let (tx, job) = self.begin().await?;
            return self
                .out_of_scope(
                    tx,
                    &job,
                    req,
                    &plan.design,
                    Some(plan),
                    "no assistant runs on this orchestrator",
                    assistant::UNBOUND,
                )
                .await;
        };
        if !self.assistant_claim(&req.case_id, round).await? {
            return Ok(());
        }
        let now = self.cases.now_ms().await?;
        let mut state = match self.assistant_state(&req.case_id).await? {
            Some(s) => s,
            None => LoopState {
                started_ms: now,
                ..LoopState::default()
            },
        };
        // A5: the job's own turn time (role_setting `turn_budget_s`) over the assistant's default
        let budget = {
            let (_tx, _job, d) = self.design().await?;
            let mut b = a.budget();
            b.turn = d.job_design.roles.turn_budget(ROLE_ASSISTANT, b.turn);
            b
        };
        let turn_ms = i64::try_from(budget.turn.as_millis()).unwrap_or(i64::MAX);
        if now.saturating_sub(state.started_ms) > turn_ms {
            let trigger = format!(
                "assistant turn budget ({} ms) exhausted",
                budget.turn.as_millis()
            );
            return self
                .hand_over(req, plan, assistant::BUDGET_EXHAUSTED, &trigger)
                .await;
        }
        if round > 0 {
            let answered = self.assistant_pending(&req.case_id, round - 1).await?;
            let results = self.cases.results(&req.case_id).await?;
            state
                .messages
                .extend(assistant_turn::tool_messages(&answered, &results));
        }
        // the conversation, the customer's words and the model binding in one short transaction
        let (mut tx, job, design) = self.design().await?;
        let conv = store::conversation(&mut tx, &req.conversation_id).await?;
        let text = assistant_turn::customer_text(&mut tx, req).await?;
        let audience: String = sqlx::query_scalar("SELECT audience FROM jobs WHERE id = $1")
            .bind(job.id)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or_default();
        let resolved = crate::providers::llm::resolve(
            &mut tx,
            a.state(),
            self.scope.tenant,
            &job,
            Use::Qualify,
            &design.of,
        )
        .await;
        tx.commit().await?;
        let resolved = match resolved {
            Ok(r) => r,
            Err(ApiError::Conflict(code, _)) => {
                tracing::warn!(case_id = %req.case_id, code, "assistant: the qualify binding cannot serve");
                None
            }
            // no inference-router licence for the tenant yet (fail closed): the assistant is unavailable, a person
            // takes the case
            Err(e) if e.code() == crate::providers::llm::INFERENCE_NOT_PROVISIONED_CODE => {
                tracing::warn!(case_id = %req.case_id, "assistant: llm_inference_not_provisioned");
                return self
                    .hand_over(
                        req,
                        plan,
                        assistant::LLM_UNAVAILABLE,
                        "the tenant's language model access is not provisioned",
                    )
                    .await;
            }
            Err(e) => return Err(e.into()),
        };
        let model = match resolved {
            Some(r) => match assistant_turn::model(a.state(), self.scope.tenant, r).await {
                Ok(m) => m,
                Err(_) => {
                    return self
                        .hand_over(
                            req,
                            plan,
                            assistant::LLM_UNAVAILABLE,
                            "the assistant's model is unreachable",
                        )
                        .await;
                }
            },
            None => None,
        };
        let Some(model) = model else {
            let (tx, job) = self.begin().await?;
            return self
                .out_of_scope(
                    tx,
                    &job,
                    req,
                    &plan.design,
                    Some(plan),
                    "the assistant has no model bound",
                    assistant::UNBOUND,
                )
                .await;
        };
        if round == 0 {
            // a case planned before agent#65 carries an empty activity: its context space names it, as ToolPoint reads it
            let activity = plan
                .assistant
                .as_ref()
                .and_then(|p| p.activity.as_deref())
                .filter(|a| !a.is_empty())
                .or_else(|| {
                    design
                        .spaces
                        .by_key(&req.context_space)
                        .map(|s| s.activity.as_str())
                        .filter(|a| !a.is_empty())
                });
            state.tools = assistant::tools::offered(
                &design.catalog,
                &design,
                a.toolset(),
                ROLE_ASSISTANT,
                activity,
            );
            // what a routed system declares of its inputs (an MCP tool's `inputSchema`): the model reads types,
            // not names only; unreadable → names only, the turn goes on
            let keys: Vec<&str> = state.tools.iter().map(|t| t.op_key.as_str()).collect();
            let (mut tx, jref) = self.begin().await?;
            match super::runner::ops::input_types(
                &mut tx,
                &self.reg,
                jref.id,
                self.scope.env,
                &keys,
            )
            .await
            {
                Ok(mut types) => {
                    tx.commit().await?;
                    for t in &mut state.tools {
                        if let Some(ty) = types.remove(&t.op_key) {
                            t.input_types = ty
                                .into_iter()
                                .filter(|(k, _)| t.inputs.contains(k))
                                .collect();
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(case_id = %req.case_id, error = %e, "assistant: input types unreadable, names only");
                }
            }
        }
        // D108 (KH T5): the one system message from the case's design layers, for the conversation's channel
        let prompt = super::prompt::assemble(
            &design.as_ref().into(),
            conv.as_ref().map(|c| c.channel.as_str()),
            None,
        );
        // the round's markers: kept with the loop state (the test-env trace) and, at round 0, on a `state` event
        state.layers = prompt.layers();
        if round == 0 {
            let (mut tx, job) = self.begin().await?;
            state_event(
                &mut tx,
                &job,
                &req.case_id,
                json!({"note": "assistant prompt", "layers": state.layers, "prompt_warning": prompt.warning}),
            )
            .await?;
            tx.commit().await?;
        }
        // A5: the job's own task prompt, else the audience's default
        let task = design.job_design.roles.system_prompt(&audience);
        let body = assistant_turn::body_with(
            &model.model,
            &prompt,
            &task,
            &assistant_turn::user_message(req, text.as_deref()),
            &state.messages,
            &assistant::tools::specs(&state.tools),
        );
        let (reply, usage) = assistant_turn::chat(&model, &body).await;
        self.assistant_usage(&a, &job, req, conv.as_ref(), &usage)
            .await?;
        let Ok(reply) = reply else {
            return self
                .hand_over(
                    req,
                    plan,
                    assistant::LLM_UNAVAILABLE,
                    "the assistant's model did not answer",
                )
                .await;
        };
        if reply.tool_calls.is_empty() {
            self.save_assistant_state(&req.case_id, &state).await?;
            self.cases
                .record_result(
                    &req.case_id,
                    round,
                    ROLE_ASSISTANT,
                    &json!({"answer": reply.content}),
                )
                .await?;
            return self
                .finish(req, Some(plan), CaseOutcome::Resolved, None, None)
                .await;
        }
        let n = u32::try_from(reply.tool_calls.len()).unwrap_or(u32::MAX);
        // B5 T11a/T12 (F-40): the job's own `max_tool_calls` (role_setting), never above the assistant's budget
        let max_tool_calls = design
            .job_design
            .roles
            .assistant_budget()
            .max_tool_calls
            .min(budget.max_tool_calls);
        if state.used.saturating_add(n) > max_tool_calls {
            let trigger = format!("assistant tool-call budget ({max_tool_calls}) exhausted");
            return self
                .hand_over(req, plan, assistant::BUDGET_EXHAUSTED, &trigger)
                .await;
        }
        state.used += n;
        state.messages.push(reply.message.clone());
        let pending: Vec<Pending> = reply
            .tool_calls
            .iter()
            .enumerate()
            .map(|(i, tc)| Pending {
                step: assistant_turn::step_name(round, i),
                tool_call_id: tc.id.clone(),
            })
            .collect();
        // state, the round's calls and its count first: an answer may land on another replica at once
        self.save_assistant_state(&req.case_id, &state).await?;
        self.save_assistant_pending(&req.case_id, round, &pending)
            .await?;
        self.cases
            .init_pending(&req.case_id, round, pending.len())
            .await?;
        for (tc, p) in reply.tool_calls.iter().zip(&pending) {
            match assistant::tools::by_name(&state.tools, &tc.name) {
                Some(tool) => {
                    let (operation, variant) = assistant::tools::split_op_key(&tool.op_key);
                    let step = Step::op(&p.step, &operation, variant.as_deref(), Value::Null);
                    let input = assistant::tools::input_of(tool, &tc.arguments);
                    self.dispatch(req, plan, round, &step, input, 1).await?;
                }
                // not offered: refused without a call
                None => {
                    self.cases
                        .record_result(
                            &req.case_id,
                            round,
                            &p.step,
                            &assistant::tools::refusal("not_entitled"),
                        )
                        .await?;
                }
            }
        }
        self.assistant_next(req, plan, round).await
    }

    /// An `op.result` of an assistant case: record the answer (or `{"error": code}`) as the call's result; the
    /// runner's handovers (`unknown_outcome`, the per-case cap, a false postcondition) stop the case.
    async fn assistant_result(
        &self,
        req: &QualifiedRequest,
        plan: &Plan,
        r: OpResult,
    ) -> Result<()> {
        let round = r.phase;
        let pending = self.assistant_pending(&r.case_id, round).await?;
        if !pending.iter().any(|p| p.step == r.step) {
            tracing::warn!(case_id = %r.case_id, round, step = %r.step, "op.result for a call the assistant did not make; dropped");
            return Ok(());
        }
        self.cases
            .set_call_status(
                &r.case_id,
                &r.call_id,
                &json!({"call_id": r.call_id, "phase": r.phase, "step": r.step, "operation": r.operation,
                        "attempt": r.attempt, "status": r.state, "outcome_code": r.outcome_code,
                        "instance_id": r.instance_id, "latency_ms": r.latency_ms}),
            )
            .await?;
        if self.cases.phase(&r.case_id).await? == CasePhase::HandedOver {
            return self
                .finish(
                    req,
                    Some(plan),
                    CaseOutcome::Handover,
                    Some("handed_over"),
                    None,
                )
                .await;
        }
        let code = r.outcome_code.as_deref();
        let output = match r.state {
            OpState::UnknownOutcome => {
                return self
                    .finish(
                        req,
                        Some(plan),
                        CaseOutcome::Handover,
                        Some("unknown_outcome"),
                        None,
                    )
                    .await;
            }
            OpState::Denied if code == Some(cap::CODE) => {
                return self
                    .finish(req, Some(plan), CaseOutcome::Handover, code, None)
                    .await;
            }
            OpState::Ok if code == Some(POSTCONDITION_FAILED) => {
                return self
                    .finish(req, Some(plan), CaseOutcome::Handover, code, None)
                    .await;
            }
            OpState::Ok => r.output.clone().unwrap_or(Value::Null),
            // A2: a tool call needs a value only the user has: park the case on the question
            OpState::Error if code == Some(declared::INPUT_REQUIRED) => {
                return self.park_input(req, plan, &r).await;
            }
            OpState::Error if input_failed(code) => {
                let trigger = format!("{} could not get a declared input", r.operation);
                return self.hand_over(req, plan, "tool_failed", &trigger).await;
            }
            OpState::Denied | OpState::Error => assistant::tools::refusal(code.unwrap_or("error")),
        };
        self.cases
            .record_result(&r.case_id, round, &r.step, &output)
            .await?;
        self.assistant_next(req, plan, round).await
    }

    /// When every call of `round` answered (and the case is still the agent's), move to the next round once.
    async fn assistant_next(
        &self,
        req: &QualifiedRequest,
        plan: &Plan,
        round: usize,
    ) -> Result<()> {
        if self.cases.remaining(&req.case_id, round).await? > 0 {
            return Ok(());
        }
        match self.boundary(req).await? {
            Boundary::Go => {}
            Boundary::Stop(outcome) => {
                return self
                    .finish(req, Some(plan), outcome, Some("handed_over"), None)
                    .await;
            }
            Boundary::Closed => return self.cases.mark_resolved(&req.case_id, "closed").await,
        }
        self.cases
            .advance_phase(&req.case_id, round, round + 1)
            .await?;
        if self.cases.phase(&req.case_id).await? == CasePhase::At(round + 1) {
            return Box::pin(self.assistant_round(req, plan, round + 1)).await;
        }
        Ok(())
    }

    /// One `usage_event` row (use label `assistant`) for one model call, in its own job-scoped transaction.
    async fn assistant_usage(
        &self,
        a: &Assistant,
        job: &JobRef,
        req: &QualifiedRequest,
        conv: Option<&store::Conv>,
        usage: &LlmUsage,
    ) -> Result<()> {
        let (source, actor) = assistant::usage_party(conv);
        let (mut tx, _) = self.begin().await?;
        crate::usage::emit(
            &mut tx,
            &a.state().cfg.metering,
            &crate::usage::UsageEvent {
                job_id: Some(job.id),
                source,
                use_: assistant::USAGE_LABEL,
                activity: req.activity.as_deref(),
                conversation_id: Some(&req.conversation_id),
                case_id: Some(&req.case_id),
                actor: &actor,
                usage,
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Claim round `round`'s model call: true for exactly one caller.
    async fn assistant_claim(&self, case_id: &str, round: usize) -> Result<bool> {
        Ok(self
            .bus
            .conn()
            .hset_nx(self.cases.key(case_id), assistant_claim_field(round), 1)
            .await?)
    }

    async fn assistant_state(&self, case_id: &str) -> Result<Option<LoopState>> {
        let raw: Option<String> = self
            .bus
            .conn()
            .hget(self.cases.key(case_id), ASSISTANT_STATE)
            .await?;
        raw.map(|r| serde_json::from_str(&r))
            .transpose()
            .map_err(Into::into)
    }

    async fn save_assistant_state(&self, case_id: &str, state: &LoopState) -> Result<()> {
        let _: () = self
            .bus
            .conn()
            .hset(
                self.cases.key(case_id),
                ASSISTANT_STATE,
                serde_json::to_string(state)?,
            )
            .await?;
        Ok(())
    }

    /// The tool calls of round `round` (empty before the round sent any).
    async fn assistant_pending(&self, case_id: &str, round: usize) -> Result<Vec<Pending>> {
        let raw: Option<String> = self
            .bus
            .conn()
            .hget(self.cases.key(case_id), assistant_round_field(round))
            .await?;
        Ok(match raw {
            Some(r) => serde_json::from_str(&r)?,
            None => Vec::new(),
        })
    }

    async fn save_assistant_pending(
        &self,
        case_id: &str,
        round: usize,
        pending: &[Pending],
    ) -> Result<()> {
        let _: bool = self
            .bus
            .conn()
            .hset_nx(
                self.cases.key(case_id),
                assistant_round_field(round),
                serde_json::to_string(pending)?,
            )
            .await?;
        Ok(())
    }

    /// [`agent_cases::out_of_hours_note`] of the case's open item, in its own read-only transaction (no lock: the
    /// item was committed before `finish`). The design is the plan's, else the scope's now (no plan was made).
    async fn out_of_hours_note(
        &self,
        req: &QualifiedRequest,
        plan: Option<&Plan>,
    ) -> Result<Option<OutOfHoursNote>> {
        let (mut tx, job) = self.begin().await?;
        let design = match plan {
            Some(p) => p.design.clone(),
            None => self
                .designs
                .get_or_load(&mut tx, &self.reg, &self.scope, &job)
                .await?
                .case
                .clone(),
        };
        let note = agent_cases::out_of_hours_note(&mut tx, &job, &req.case_id, &design).await?;
        tx.commit().await?;
        Ok(note)
    }

    /// The reply's `local_days` fact: each RFC3339 instant in `facts` (text with `T` and `Z` or an offset) mapped to
    /// its calendar day on the wall clock of the scope's business-hours timezone (`hours.timezone`, the zone the
    /// out-of-hours note uses), converted by Postgres (`AT TIME ZONE`). `None` when there are no instants, the scope
    /// has no hours (no zone is known: nothing is converted), or the read fails (the reply reads the date as written).
    async fn local_days(&self, plan: Option<&Plan>, facts: &Map<String, Value>) -> Option<Value> {
        fn walk(v: &Value, out: &mut std::collections::BTreeSet<String>) {
            match v {
                Value::String(s) => {
                    let t = s.trim();
                    if t.len() > 10 && chrono::DateTime::parse_from_rfc3339(t).is_ok() {
                        out.insert(t.to_owned());
                    }
                }
                Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
                Value::Object(o) => o.values().for_each(|x| walk(x, out)),
                _ => {}
            }
        }
        let mut found = std::collections::BTreeSet::new();
        facts.values().for_each(|v| walk(v, &mut found));
        if found.is_empty() {
            return None;
        }
        let found: Vec<String> = found.into_iter().take(200).collect();
        let read = async {
            let (mut tx, job) = self.begin().await?;
            let design = match plan {
                Some(p) => p.design.clone(),
                None => self
                    .designs
                    .get_or_load(&mut tx, &self.reg, &self.scope, &job)
                    .await?
                    .case
                    .clone(),
            };
            let (hours, _) = crate::handover::policy::hours_of(
                &mut tx,
                &job,
                &crate::handover::sla::design_of(&design),
            )
            .await?;
            let Some(tz) = hours.map(|h| h.timezone) else {
                return Ok::<_, anyhow::Error>(None);
            };
            let rows: Vec<(String, chrono::NaiveDate)> = sqlx::query_as(
                "SELECT t, ((t::timestamptz) AT TIME ZONE $2::text)::date FROM unnest($1::text[]) AS u(t)",
            )
            .bind(&found)
            .bind(&tz)
            .fetch_all(&mut *tx)
            .await?;
            Ok(Some(rows))
        };
        match read.await {
            Ok(Some(rows)) => Some(Value::Object(
                rows.into_iter()
                    .map(|(t, d)| (t, Value::String(d.format("%Y-%m-%d").to_string())))
                    .collect(),
            )),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(error = %e, "reply: instants left on their UTC day (timezone unreadable)");
                None
            }
        }
    }

    /// The scope's design: the cached one, else read in a fresh scoped transaction (rolled back: a read).
    async fn runtime_design(&self) -> Result<Arc<RuntimeDesign>> {
        if let Some(d) = self.designs.cached(&self.scope) {
            return Ok(d);
        }
        let (tx, _job, d) = self.design().await?;
        drop(tx);
        Ok(d)
    }

    // ---- B4 known customer -----------------------------------------------------------------------------

    /// The case's current identity level: the case hash's (a verify raised it), never below the qualified one.
    async fn current_level(&self, req: &QualifiedRequest) -> Result<u8> {
        Ok(match self.cases.identity(&req.case_id).await? {
            Some((level, _)) => level.max(req.identity_level),
            None => req.identity_level,
        })
    }

    /// End the case: the first resolution wins; its status change and `state` event are written in one
    /// transaction, then `request.resolved` is published, then the case leaves the deadline set. A redelivery
    /// re-publishes the stored resolution (the interaction daemon dedupes by case).
    async fn finish(
        &self,
        req: &QualifiedRequest,
        plan: Option<&Plan>,
        outcome: CaseOutcome,
        code: Option<&str>,
        reason: Option<String>,
    ) -> Result<()> {
        if self.cases.is_resolved(&req.case_id).await? {
            return Ok(());
        }
        let facts = self.cases.results(&req.case_id).await?;
        let calls = self.cases.calls(&req.case_id).await?;
        let ctx = self.cases.context(req).await?;
        let knowledge = plan.and_then(|p| knowledge_use(p, &facts, &ctx));
        let mut trace = (self.scope.env == super::scope::Env::Test)
            .then(|| trace_input(req, plan, &facts, &calls, knowledge.as_ref()));
        if let Some(t) = trace.as_mut()
            && let Some(s) = self.assistant_state(&req.case_id).await?
        {
            t.assistant_layers = s.layers;
        }
        // B4 T16a (F-14, acceptance A15): a case handed over while the scope is closed — by `hand_over`,
        // `out_of_scope`, the runner — tells the customer the next open time (a failed case: below, `queue_failed`)
        let out_of_hours = if outcome == CaseOutcome::Handover {
            self.out_of_hours_note(req, plan).await?
        } else {
            None
        };
        // live CS-53: a resolved reply claims the request is done only when a write step of the case answered ok
        let wrote = match plan {
            Some(p) if outcome == CaseOutcome::Resolved => {
                case_wrote(p, &facts, &self.runtime_design().await?.catalog)
            }
            _ => false,
        };
        // the customer's day for every instant the reply may print (the scope's business-hours timezone)
        let mut facts = facts;
        if let Some(p) = plan {
            bind_knowledge_reply(p, &mut facts);
        }
        if let Some(days) = self.local_days(plan, &facts).await {
            facts.insert("local_days".into(), days);
        }
        let proposed = Resolution {
            case_id: req.case_id.clone(),
            conversation_id: req.conversation_id.clone(),
            context_space: req.context_space.clone(),
            request_type: req.request_type.clone(),
            outcome,
            code: code.map(str::to_owned),
            reason,
            facts,
            procedure: plan.and_then(Plan::procedure_label),
            design: plan.map(|p| p.design.clone()),
            knowledge,
            trace,
            out_of_hours,
            wrote,
            round: self.cases.round(&req.case_id).await?,
        };
        let mut res = self.cases.set_resolution(&req.case_id, &proposed).await?;
        let (mut tx, job) = self.begin().await?;
        // a failed case is a colleague's: its J21 item is queued (and committed) before the resolution is published,
        // so staff see it and the interaction daemon still delivers the case's own reply (`withheld`)
        let (own_item, note, created) = if res.outcome == CaseOutcome::Failed {
            self.queue_failed(&mut tx, &job, &res).await?
        } else {
            (false, None, None)
        };
        // B4 T16a: a failed case's own item queued out of hours — the reply says when a colleague follows up (set
        // on every publish: a redelivery finds the item again, `Queued::Already`)
        if own_item {
            res.out_of_hours = note;
        }
        // the item moved the case to `handover`, where it stays until staff resolve it or hand it back
        let status = if own_item {
            CaseOutcome::Handover
        } else {
            res.outcome
        };
        write_status(
            &mut tx,
            &job,
            &req.case_id,
            status,
            res.code.as_deref(),
            res.procedure.as_deref(),
        )
        .await?;
        tx.commit().await?;
        if let Some(item) = &created {
            self.notify_workers(&job, item).await;
        }
        let env = Envelope::new(&req.topic(REQUEST_RESOLVED), ROLE, &req.case_id, &res)?
            .with_procedure(res.procedure.as_deref());
        self.bus
            .publish(&self.bus.streams.customer(&req.area), &env)
            .await?;
        self.cases
            .mark_resolved(&req.case_id, res.outcome.status())
            .await
    }
}

/// The J21 reason (0063 + 0179) of an [`Orchestrator::hand_over`] with outcome `code`, recorded on the item and as the
/// case's `escalation_reason`: a required or routed operation the job does not offer is `not_offered`, a procedure
/// whose success criteria do not hold after its last phase (every step ok) is `success_criteria_unmet`. Anything
/// else is `tool_failed`, which stays the reason of an operation that did not work (refused, error, timeout, the
/// model unreachable, the turn budget) and of an F-20 `precondition_unmet` denial (B5 plan).
/// A2: the runner could not fill a declared input or store a declared output (`input_unresolved`,
/// `secret_store_unavailable`, `state_unavailable`): the case is handed over as `tool_failed`.
fn input_failed(code: Option<&str>) -> bool {
    matches!(
        code,
        Some(
            declared::INPUT_UNRESOLVED
                | declared::SECRET_STORE_UNAVAILABLE
                | declared::STATE_UNAVAILABLE
        )
    )
}

fn hand_over_reason(code: &str) -> &'static str {
    match code {
        "not_offered" => "not_offered",
        STEP_UP_REQUIRED => STEP_UP_REQUIRED,
        SUCCESS_CRITERIA_UNMET => SUCCESS_CRITERIA_UNMET,
        _ => "tool_failed",
    }
}

/// What a phase boundary finds on the case.
enum Boundary {
    /// Still the agent's: go on.
    Go,
    /// Left the agent's hands: stop with this outcome.
    Stop(CaseOutcome),
    /// Already closed elsewhere: stop silently.
    Closed,
}

/// A transaction with both GUCs set for `scope` (`app.current_tenant`, `app.current_job`) and the job (looked up by
/// key once, then cached in `job`).
async fn begin_scope(
    db: &PgPool,
    scope: &Scope,
    job: &OnceLock<JobRef>,
) -> Result<(Transaction<'static, Postgres>, JobRef)> {
    let mut tx = crate::tenancy::begin_scoped(db, scope.tenant).await?;
    let j = match job.get() {
        Some(j) => j.clone(),
        None => {
            let id: uuid::Uuid = sqlx::query_scalar(
                "SELECT id FROM jobs WHERE tenant_id = app_current_tenant() AND key = $1",
            )
            .bind(&scope.job)
            .fetch_optional(&mut *tx)
            .await?
            .with_context(|| format!("scope {scope}: no such job"))?;
            let j = JobRef {
                id,
                key: scope.job.clone(),
            };
            // a racing first lookup stores the same job
            let _ = job.set(j.clone());
            j
        }
    };
    sqlx::query("SELECT set_config('app.current_job', $1, true)")
        .bind(j.id.to_string())
        .execute(&mut *tx)
        .await?;
    Ok((tx, j))
}

/// A `state` case event (ids and codes only, K50).
async fn state_event(
    conn: &mut PgConnection,
    job: &JobRef,
    case_id: &str,
    body: Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO case_event (tenant_id, job_id, case_id, kind, body) \
         VALUES (app_current_tenant(), $1, $2, 'state', $3)",
    )
    .bind(job.id)
    .bind(case_id)
    .bind(body)
    .execute(conn)
    .await?;
    Ok(())
}

/// Move an open case to `outcome`'s status and append its `state` event; nothing when the case is already closed or
/// already has that status (a redelivery, a staff hand-over). True when the status changed.
async fn write_status(
    conn: &mut PgConnection,
    job: &JobRef,
    case_id: &str,
    outcome: CaseOutcome,
    code: Option<&str>,
    procedure: Option<&str>,
) -> Result<bool> {
    let status = outcome.status();
    let changed: Option<String> = sqlx::query_scalar(
        "UPDATE agent_case SET status = $2, \
                outcome = COALESCE($3, $2), \
                escalation_reason = CASE WHEN $2 = 'handover' THEN COALESCE(escalation_reason, $3) \
                                         ELSE escalation_reason END, \
                resolved_at = CASE WHEN $4 THEN now() ELSE resolved_at END, \
                duration_ms = CASE WHEN $4 THEN (EXTRACT(EPOCH FROM (now() - started_at)) * 1000)::bigint \
                                   ELSE duration_ms END, \
                updated_at = now() \
          WHERE case_id = $1 AND status = ANY($5) AND status <> $2 \
          RETURNING case_id",
    )
    .bind(case_id)
    .bind(status)
    .bind(code)
    .bind(outcome.closes())
    .bind(&OPEN[..])
    .fetch_optional(&mut *conn)
    .await?;
    if changed.is_none() {
        return Ok(false);
    }
    state_event(
        conn,
        job,
        case_id,
        json!({"status": status, "code": code, "procedure": procedure}),
    )
    .await?;
    Ok(true)
}

/// End the open cases of every scope that left the registry (tenant suspended or archived, job offboarded):
/// status `cancelled_offboard` + `state` event, and the case leaves the orchestrator. Nothing is published (the
/// scope's daemons have stopped). Refuses to act on an empty registry (a wiped Redis is not an offboarding).
/// Returns how many cases it cancelled.
pub async fn cancel_offboarded(root: &Bus, db: &PgPool) -> Result<usize> {
    let registered: HashSet<Scope> = scopes::list(root).await?.into_iter().collect();
    if registered.is_empty() {
        tracing::warn!("offboard sweep: the scope registry is empty; nothing cancelled");
        return Ok(0);
    }
    let active_key = root.root_key(ACTIVE_SCOPES_KEY);
    let active: Vec<String> = root.conn().smembers(&active_key).await?;
    let mut cancelled = 0;
    for text in active {
        let Ok(scope) = Scope::parse(&text) else {
            let _: i64 = root.conn().srem(&active_key, &text).await?;
            continue;
        };
        if registered.contains(&scope) {
            continue;
        }
        let cases = CaseStore::new(root.scoped(&scope));
        let job = OnceLock::new();
        for case_id in cases.tracked().await? {
            let (mut tx, j) = match begin_scope(db, &scope, &job).await {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!(scope = %scope, "offboard sweep: {e:#}");
                    break;
                }
            };
            let changed = write_status(
                &mut tx,
                &j,
                &case_id,
                CaseOutcome::CancelledOffboard,
                Some("scope_removed"),
                None,
            )
            .await?;
            tx.commit().await?;
            cases
                .mark_resolved(&case_id, CaseOutcome::CancelledOffboard.status())
                .await?;
            if changed {
                cancelled += 1;
                tracing::info!(scope = %scope, %case_id, "case cancelled: scope removed");
            }
        }
        if cases.tracked().await?.is_empty() {
            let _: i64 = root.conn().srem(&active_key, &text).await?;
        }
    }
    Ok(cancelled)
}

/// [`cancel_offboarded`] every [`OFFBOARD_EVERY`] until the task is aborted; a failed sweep is logged and retried.
pub async fn offboard_loop(root: Bus, db: PgPool) {
    let mut tick = tokio::time::interval(OFFBOARD_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        if let Err(e) = cancel_offboarded(&root, &db).await {
            tracing::warn!("offboard sweep failed: {e:#}");
        }
    }
}

/// T6a: the facts of a case its conditions read ([`conditions::FACT_PATHS`]): the request's non-blank slots, its
/// identity level and activity, every recorded step output, and the customer tier a `party.read` step answered.
/// `resolved` are the system slots resolved from the customer's records ([`CaseStore::resolved_slots`]): server-side
/// truth, they OVERRIDE a request slot of the same name, exactly as in a step's `$request.*` context.
pub fn case_facts(
    req: &QualifiedRequest,
    plan: &Plan,
    results: &Map<String, Value>,
    resolved: &std::collections::BTreeMap<String, String>,
) -> CaseFacts {
    let mut slots: std::collections::BTreeMap<String, Value> = req
        .slots
        .iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    for (k, v) in resolved.iter().filter(|(_, v)| !v.trim().is_empty()) {
        slots.insert(k.clone(), Value::String(v.clone()));
    }
    let steps = results
        .iter()
        .map(|(k, v)| (k.clone(), StepFact::answered(v)))
        .collect();
    let customer_tier = plan
        .phases
        .iter()
        .flat_map(|p| p.steps.iter())
        .filter(|s| s.operation == "party.read")
        .find_map(|s| {
            results
                .get(&s.name)?
                .get("tier")?
                .as_str()
                .map(str::to_owned)
        });
    CaseFacts {
        slots,
        identity_level: req.identity_level,
        customer_tier,
        steps,
        activity: req.activity.clone(),
    }
}

/// A citation as its chunk id (`"c1"` or `{"chunk_id"|"id": "c1"}`).
fn citation_id(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o
            .get("chunk_id")
            .or_else(|| o.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

/// Live CS-53: whether a write step of `plan` (catalogue side effect W/X/P; an operation the catalogue does not know
/// counts as a write, the runner's and [`Orchestrator`]'s dispatch rule) answered `ok` — its output is in `facts`.
/// An assistant case's tool calls are not plan steps: it never counts as written.
pub fn case_wrote(plan: &Plan, facts: &Map<String, Value>, catalog: &dyn OperationCatalog) -> bool {
    plan.phases
        .iter()
        .flat_map(|p| p.steps.iter())
        .filter(|s| facts.contains_key(&s.name))
        .any(|s| catalog.get(&s.op_key()).is_none_or(|o| o.writes()))
}

/// Keep the reply composer’s canonical answer slot aligned with the actual authored knowledge-answer step.
fn bind_knowledge_reply(plan: &Plan, facts: &mut Map<String, Value>) {
    if let Some(answer) = plan
        .phases
        .iter()
        .flat_map(|p| &p.steps)
        .filter(|s| s.operation == builtin::ANSWER_OP)
        .find_map(|s| facts.get(&s.name).cloned())
    {
        facts.insert(KNOWLEDGE_STEP.to_owned(), answer);
    }
}

/// How the case used knowledge: the first `knowledge.*` step with an output, its query, citations and sources.
pub fn knowledge_use(
    plan: &Plan,
    facts: &Map<String, Value>,
    ctx: &Map<String, Value>,
) -> Option<KnowledgeUse> {
    plan.phases
        .iter()
        .flat_map(|p| p.steps.iter())
        .filter(|s| s.operation.starts_with("knowledge."))
        .find_map(|s| {
            let out = facts.get(&s.name)?;
            let args = resolve_args(&s.args, ctx);
            // the step's own query, else the question it answered (KH T7: a support turn's step reads the message)
            let query = ["query", "question"]
                .iter()
                .find_map(|k| args.get(*k).and_then(Value::as_str))
                .or_else(|| out.get("query").and_then(Value::as_str))
                .map(str::to_owned);
            let cited = out.get("citations").and_then(Value::as_array);
            let citations: Vec<String> = cited
                .map(|a| a.iter().filter_map(citation_id).collect())
                .unwrap_or_default();
            // D109: the citations are `AnswerSource`s (layer, pack, provenance); an older step listed `sources`
            let sources = out
                .get("sources")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_else(|| {
                    cited
                        .map(|a| a.iter().filter(|c| c.is_object()).cloned().collect())
                        .unwrap_or_default()
                });
            let ungrounded = out.get("grounded") == Some(&Value::Bool(false));
            Some(KnowledgeUse {
                query,
                dont_know: citations.is_empty() || ungrounded,
                citations,
                sources,
            })
        })
}

/// The trace's citations of `k`: each source as `<layer> <pack> · <title>` (D109: the layer and pack a citation
/// names, crate::knowledge::citations::cite_label), else the cited chunk ids.
fn cite_labels(k: &KnowledgeUse) -> Vec<String> {
    let labels: Vec<String> = k
        .sources
        .iter()
        .filter_map(crate::knowledge::citations::cite_label)
        .collect();
    if labels.is_empty() {
        k.citations.clone()
    } else {
        labels
    }
}

/// What the case did, for the test-env reply trace (T8 fills the design side): the procedure, each dispatched
/// step's state, the calls, the cited chunks and the design places read (`route:<op>:test`, `rule_values:<rule id>`).
pub fn trace_input(
    req: &QualifiedRequest,
    plan: Option<&Plan>,
    facts: &Map<String, Value>,
    calls: &[Value],
    knowledge: Option<&KnowledgeUse>,
) -> TraceInput {
    // the latest call of each step (highest attempt)
    let mut last: std::collections::BTreeMap<String, &Value> = std::collections::BTreeMap::new();
    for c in calls {
        let Some(step) = c["step"].as_str() else {
            continue;
        };
        let attempt = c["attempt"].as_u64().unwrap_or(0);
        let newer = last
            .get(step)
            .is_none_or(|p| p["attempt"].as_u64().unwrap_or(0) <= attempt);
        if newer {
            last.insert(step.to_owned(), c);
        }
    }
    let mut steps = Vec::new();
    let mut read = Vec::new();
    for s in plan
        .iter()
        .flat_map(|p| p.phases.iter())
        .flat_map(|p| p.steps.iter())
    {
        let key = s.op_key();
        let state = if facts.contains_key(&s.name) {
            "done"
        } else {
            match last.get(&s.name).and_then(|c| c["status"].as_str()) {
                None => continue,
                Some("outstanding") => "stopped",
                Some(_) => "failed",
            }
        };
        steps.push(TraceStep {
            label: format!("{} ({key})", s.name),
            state: state.to_owned(),
            detail: last
                .get(&s.name)
                .and_then(|c| c["outcome_code"].as_str())
                .map(str::to_owned),
        });
        // the trace is the Test env's: the route a turn read is that op's TEST route, keyed as the draft ledger keys it
        // (`route:<op>:<env>`, connectors::places), so a saved test case counts as having exercised that change
        read.push(format!("route:{key}:test"));
        if s.operation == "rule.evaluate"
            && let Some(rule) = s.variant.as_deref()
        {
            read.push(format!("rule_values:{rule}"));
        }
    }
    read.sort();
    read.dedup();
    let ops = last
        .values()
        .map(|c| TraceOp {
            operation: c["operation"].as_str().unwrap_or_default().to_owned(),
            state: c["status"].as_str().unwrap_or_default().to_owned(),
            code: c["outcome_code"]
                .as_str()
                .or_else(|| (c["status"] == "ok").then_some("ok"))
                .unwrap_or_default()
                .to_owned(),
            instance_id: c["instance_id"].as_str().unwrap_or_default().to_owned(),
            latency_ms: c["latency_ms"].as_u64().unwrap_or(0),
        })
        .collect();
    TraceInput {
        activity: req.activity.clone().map(|key| TraceActivity {
            key,
            label: String::new(),
        }),
        procedure: plan.and_then(Plan::procedure_label),
        steps,
        ops,
        cites: knowledge.map(cite_labels).unwrap_or_default(),
        read,
        layers: req.layers.clone(),
        assistant_layers: Vec::new(),
        prompt_warning: req.prompt_warning.clone(),
    }
}

/// `CASE_DEADLINE_MS`: unset or empty keeps the default (3,600 s), anything else must be a positive integer.
pub fn case_deadline_from(raw: Option<&str>) -> Result<Duration> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(DEFAULT_CASE_DEADLINE),
        Some(s) => match s.parse::<u64>() {
            Ok(ms) if ms > 0 => Ok(Duration::from_millis(ms)),
            _ => anyhow::bail!("CASE_DEADLINE_MS must be a positive integer (milliseconds)"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reclaim_idle_outlasts_one_model_round() {
        let o = consume_options();
        assert!(
            o.reclaim_idle >= assistant_turn::LLM_TIMEOUT + super::super::bus::RECLAIM_SLACK,
            "{:?}: a peer would take a live assistant round over",
            o.reclaim_idle
        );
    }

    #[test]
    fn pure_builtins_plan_for_a_known_activity_without_connected_grants() {
        use crate::design::DesignOf;
        use crate::job_design::base::{BaseActivity, Bases};
        use crate::job_design::toolset::Toolsets;
        use agent_model::compile::CatalogueItem;
        let mut d = RuntimeDesign::blank(
            JobRef {
                id: uuid::Uuid::nil(),
                key: "test".into(),
            },
            DesignOf::Draft,
        );
        d.spaces = super::super::spaces::SpaceCatalog::new(vec![
            super::super::spaces::ContextSpace::from_item(
                "retail:faq:shipping",
                json!({"label":"Shipping", "activity":"support:shipping", "area":"faq"})
                    .as_object()
                    .unwrap(),
            ),
        ]);
        let b = Bases {
            agent_roles: vec![CatalogueItem {
                key: "procedure".into(),
                layer: "base".into(),
                row_id: "test".into(),
                fields:
                    json!({"label":"Procedure", "enabled":true,"max_side_effect":"R","grants":[]})
                        .as_object()
                        .unwrap()
                        .clone(),
            }],
            activities: vec![BaseActivity {
                key: "support:shipping".into(),
                item: Map::new(),
                ops: Vec::new(),
            }],
            ..Bases::default()
        };
        std::sync::Arc::make_mut(&mut d.job_design).toolsets =
            Toolsets::build(&b, [], Default::default());
        let mut p = TemplateProcedure {name:"answer".into(),version:1,label:"Answer".into(),uses_slots:Vec::new(),steps:1,
            step_defs:vec![serde_json::from_value(json!({"step_number":1,"phase":0,"name":"answer","operation":"knowledge.answer","variant":null,"modality":"observe","side_effect":"R","identity_level":"0","confirm_back":false,"gate":false,"args":{}})).unwrap()],
            success_criteria:Vec::new(),tool_requirements:Vec::new(),context_space:Some("retail:faq:shipping".into()),builtin_version:None,tenant:true};
        let mut req: QualifiedRequest=serde_json::from_value(json!({"case_id":"CS-test","conversation_id":"test","area":"faq","context_space":"retail:faq:shipping","request_type":"shipping","slots":{},"identity_level":2,"message_id":"m-test"})).unwrap();
        assert_eq!(outside_grant(&d, &req, &p), Ok(Vec::new()));
        let checks = designed_checks(&p, knowledge_checks());
        assert_eq!(checks.success_criteria[0].fact, "steps.answer.out.grounded");
        let answer = json!({"answer":"Our policy answer [1].","citations":[{"id":"source-1"}],"grounded":true});
        let mut evaluated = conditions::CaseFacts::default();
        evaluated
            .steps
            .insert("answer".into(), conditions::StepFact::answered(&answer));
        assert!(conditions::failed(&checks.success_criteria, &evaluated).is_empty());
        evaluated.steps.insert(
            "answer".into(),
            conditions::StepFact::answered(&json!({"grounded":false})),
        );
        assert_eq!(
            conditions::failed(&checks.success_criteria, &evaluated),
            vec![ANSWER_GROUNDED.to_owned()]
        );
        let plan = Plan {
            procedure: None,
            design: CaseDesign::Draft { draft_rev: 0 },
            phases: designed_phases(&p).unwrap(),
            success_criteria: checks.success_criteria,
            tool_requirements: checks.tool_requirements,
            assistant: None,
        };
        let mut facts = json!({"answer":answer}).as_object().unwrap().clone();
        bind_knowledge_reply(&plan, &mut facts);
        let resolution:Resolution=serde_json::from_value(json!({"case_id":"CS-test","conversation_id":"test","context_space":"retail:faq:shipping","request_type":"shipping","outcome":"resolved","facts":facts})).unwrap();
        assert_eq!(
            super::super::reply::compose(&resolution),
            "Our policy answer [1]."
        );
        p.step_defs[0].operation = "rule.evaluate".into();
        assert_eq!(outside_grant(&d, &req, &p), Ok(Vec::new()));
        p.step_defs[0].operation = "payment.refund".into();
        assert_eq!(
            outside_grant(&d, &req, &p),
            Ok(vec!["payment.refund".into()])
        );
        req.context_space = "unknown".into();
        assert!(outside_grant(&d, &req, &p).is_err());
    }

    #[test]
    fn the_return_flow_is_order_read_then_the_gated_rule_then_the_rma() {
        let phases = builtin_phases("return_request").unwrap();
        let keys: Vec<Vec<String>> = phases
            .iter()
            .map(|p| p.steps.iter().map(Step::op_key).collect())
            .collect();
        assert_eq!(
            keys,
            [
                vec!["order.read".to_owned()],
                vec!["rule.evaluate{RET-01}".to_owned()],
                vec!["case.create{return}".to_owned()]
            ]
        );
        assert!(phases[1].steps[0].gate);
        assert_eq!(procedure_for_space("returns"), Some("return_request"));
        for space in [
            "returns",
            "warranty",
            "order_status",
            "delivery_problem",
            "refunds",
        ] {
            let p = procedure_for_space(space).unwrap();
            assert!(builtin_phases(p).is_some_and(|ph| !ph.is_empty()), "{p}");
        }
        assert!(procedure_for_space("shipping").is_none());
    }

    /// Live CS-58: "where is my order RET-01?" (space `retail:logistics:track_parcel`, slot `tracking_ref`) handed
    /// over `out_of_scope` with no call. It runs as the declared `order_status`: one shipment read by the variant the
    /// `delivery:track_parcel` activity grants, keyed by the reference the customer gave, and that read must be
    /// routed (else `not_offered` before any call).
    #[test]
    fn track_parcel_runs_as_order_status_with_the_granted_shipment_read() {
        assert_eq!(procedure_for_space("track_parcel"), Some("order_status"));
        let phases = builtin_phases_for_space("track_parcel", "order_status").unwrap();
        let steps: Vec<Vec<(String, String, Value)>> = phases
            .iter()
            .map(|p| {
                p.steps
                    .iter()
                    .map(|s| (s.name.clone(), s.op_key(), Value::Object(s.args.clone())))
                    .collect()
            })
            .collect();
        assert_eq!(
            steps,
            [vec![(
                "delivery".to_owned(),
                TRACK_PARCEL_READ.to_owned(),
                json!({"tracking_ref": "$tracking_ref"})
            )]]
        );
        assert!(phases.iter().flat_map(|p| &p.steps).all(|s| !s.gate));
        let checks = builtin_checks_for_space("track_parcel", "order_status");
        assert_eq!(checks.tool_requirements, [TRACK_PARCEL_READ]);
        assert!(checks.success_criteria.is_empty());
        // every other space keeps its procedure's own phases and checks
        assert_eq!(
            builtin_phases_for_space("order_status", "order_status"),
            builtin_phases("order_status")
        );
        assert_eq!(
            builtin_checks_for_space("delivery_problem", "delivery_problem"),
            builtin_checks("delivery_problem")
        );
        assert_eq!(
            builtin_checks_for_space("order_status", "order_status"),
            ProcedureChecks::default()
        );
    }

    #[test]
    fn the_delivery_problem_flow_reads_the_shipment_and_resolves_only_when_it_is_on_track() {
        let phases = builtin_phases("delivery_problem").unwrap();
        let keys: Vec<Vec<String>> = phases
            .iter()
            .map(|p| p.steps.iter().map(Step::op_key).collect())
            .collect();
        assert_eq!(
            keys,
            [
                vec!["order.read".to_owned()],
                vec!["delivery.report_problem".to_owned()],
                vec!["delivery.read".to_owned()]
            ]
        );
        let names: Vec<&str> = phases
            .iter()
            .flat_map(|p| p.steps.iter().map(|s| s.name.as_str()))
            .collect();
        let checks = builtin_checks("delivery_problem");
        assert_eq!(checks.success_criteria.len(), 1);
        conditions::validate(&checks.success_criteria, &names).unwrap();
        let facts = |status: Value| CaseFacts {
            steps: [(
                "delivery".to_owned(),
                StepFact::answered(&json!({"status": status})),
            )]
            .into_iter()
            .collect(),
            ..CaseFacts::default()
        };
        // `delivered` contradicts the complaint ("tracking says delivered, I never got it"): a colleague's
        for late in [
            json!("problem"),
            json!("failed"),
            json!("returned"),
            json!("delivered"),
            Value::Null,
        ] {
            assert_eq!(
                conditions::failed(&checks.success_criteria, &facts(late.clone())),
                ["delivery_on_track"],
                "{late}"
            );
        }
        for on_track in ["created", "ready", "in_transit", "pickup_available"] {
            assert!(
                conditions::failed(&checks.success_criteria, &facts(json!(on_track))).is_empty(),
                "{on_track}"
            );
        }
        // only the delivery flow carries criteria
        assert_eq!(builtin_checks("return_request"), ProcedureChecks::default());
    }

    #[test]
    fn failure_reasons_never_carry_the_raw_error() {
        assert_eq!(
            failure_reason("order.read", Some("timeout")),
            "order.read did not answer in time"
        );
        assert_eq!(
            failure_reason("order.read", Some("upstream_unavailable")),
            "order.read is unavailable"
        );
        assert_eq!(
            failure_reason("order.read", None),
            "order.read is unavailable"
        );
    }

    #[test]
    fn case_deadline_parses() {
        assert_eq!(case_deadline_from(None).unwrap(), DEFAULT_CASE_DEADLINE);
        assert_eq!(
            case_deadline_from(Some(" ")).unwrap(),
            DEFAULT_CASE_DEADLINE
        );
        assert_eq!(
            case_deadline_from(Some("250")).unwrap(),
            Duration::from_millis(250)
        );
        for bad in ["0", "-1", "soon"] {
            assert!(case_deadline_from(Some(bad)).is_err(), "{bad}");
        }
    }

    /// B4 known customer: a precondition reading `slots.order_id` (the `$order_id` of a step's args) sees the order
    /// resolved from the customer's records, a model-supplied value of the same slot never overrides it, and a fact
    /// like the reply's `item_label` (a case result, not a slot) is unaffected.
    /// Mutations: the resolved slots left out of `case_facts` (the precondition fails closed); a request slot allowed
    /// to win over the resolved one.
    #[test]
    fn case_facts_carry_the_resolved_system_slots_and_they_win() {
        let plan = Plan {
            procedure: None,
            design: CaseDesign::Live { release_id: None },
            phases: Vec::new(),
            success_criteria: Vec::new(),
            tool_requirements: Vec::new(),
            assistant: None,
        };
        let mut req: QualifiedRequest = serde_json::from_value(json!({
            "case_id": "c", "conversation_id": "v", "area": "a", "context_space": "a:refunds",
            "request_type": "refund", "slots": {"reason": "too big", "order_id": "MODEL-GUESS"},
            "identity_level": 2
        }))
        .unwrap();
        let pre = Condition {
            id: "order_is_resolved".into(),
            fact: "slots.order_id".into(),
            op: CondOp::Eq,
            value: Some(json!("A-1")),
        };
        let resolved: std::collections::BTreeMap<String, String> =
            [("order_id".to_owned(), "A-1".to_owned())].into();
        let mut results = Map::new();
        results.insert(
            "resolved".into(),
            json!({"item_label": "Jute rug ordered on 12 October"}),
        );
        // resolved wins over the model's value, and the slot is there at all
        let f = case_facts(&req, &plan, &results, &resolved);
        assert!(conditions::eval(&pre, &f));
        assert_eq!(f.slots["reason"], "too big");
        assert_eq!(
            f.steps["resolved"].out.as_ref().unwrap()["item_label"],
            "Jute rug ordered on 12 October"
        );
        // without the resolved slot the model's guess is all there is: the precondition fails closed
        assert!(!conditions::eval(
            &pre,
            &case_facts(&req, &plan, &results, &Default::default())
        ));
        // the request carries nothing: the resolved slot alone satisfies it
        req.slots.remove("order_id");
        assert!(conditions::eval(
            &pre,
            &case_facts(&req, &plan, &results, &resolved)
        ));
    }
}
