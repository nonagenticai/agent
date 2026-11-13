//! `probe_turn` (B4 T9a): one test-env answer to one message, with no side effects, for B8's draft validation
//! (`runDraftValidation`, the validation cuts).
//!
//! The probe runs what a staff test-chat turn runs, in this process: the licence gate, the `RequestGate` hook,
//! qualify (the job's `qualify` binding, one model call, no draft carried between turns), the plan
//! ([`DesignProcedures`] over the design read), then each phase's steps — a read (`R`) through the same executors the
//! runner uses (its registrations, B7's [`ConnectorExecutor`] for instance targets, B4's built-ins), and a `W`/`X`/`P`
//! operation never: it is reported in [`Probe::would_call`] and the plan goes on without its output — and finally the
//! reply template ([`compose`]).
//!
//! - **Design.** The caller names it: `CaseDesign::Live{release_id}` reads the job's live design, which must be that
//!   release (a job never released: `release_id: None`), `CaseDesign::Draft{draft_rev}` the working rows, which must
//!   be at that revision. A design that is not (or no longer) the one named answers 409 `design_changed`. The env is
//!   always `test`: the demo stub serves, a prod-only route does not.
//! - **No writes.** No `agent_case`, `case_event`, `conversation*`, `operation_call*` or handover row, no audit row
//!   and no Redis key. The only write is one `usage_event` (`source = "validation"`, D56) per model call, in its own
//!   transaction after the probe finished.
//! - **Budget.** [`BUDGET`] (20 s, B8-5) for the whole probe; over it → 503 `unavailable` and nothing is stored (the
//!   usage of a probe that ran out of time is not emitted: no answer came of it).
//!
//! The identity level is the staff test chat's ([`KNOWN_CUSTOMER_IDENTITY`]): a validation stands in for a customer the
//! staff member picked. Message text is data: it is never logged.
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use agent_model::kinds::catalog::SideEffect;
use serde_json::{Map, Value};

use super::drain::{GATE_BLOCKED_TEXT, KNOWN_CUSTOMER_IDENTITY, LicenceGate};
use super::qualify::{self, Draft, LlmUsage, Outcome};
use super::{REFUSAL_TEXT, TURN_FAILED};
use crate::connectors::routes_store;
use crate::conversations::store;
use crate::design::{CaseDesign, DesignOf, JobRef};
use crate::error::{ApiError, ApiResult};
use crate::licensing::gate::VerdictGate;
use crate::providers::llm::{self, Use};
use crate::runtime::Env;
use crate::runtime::catalog::{OpDef, OperationCatalog};
use crate::runtime::design::RuntimeDesign;
use crate::runtime::domain::{
    CaseOutcome, Plan, QualifiedRequest, Resolution, answered_false, resolve_args,
};
use crate::runtime::gates::{Decision, Gates, RequestInput};
use crate::runtime::orchestrator::{
    DesignProcedures, ProcedureSource, STEP_UP_REQUIRED, case_facts, failure_reason, knowledge_use,
};
use crate::runtime::reply::compose;
use crate::runtime::routes::{DefaultRoutes, ResolvedTarget, RouteQuery};
use crate::runtime::runner::ops::{
    self, ConnectorExecutor, Executors, OpExecutor, OpOutcome, OpRequest, OpState, failed, outcome,
};
use crate::runtime::runner::{
    LIVE_TIMEOUT, STUB_TIMEOUT, authorise, builtin, served_refusal, serves_case_create,
};
use crate::state::AppState;
use crate::tenancy::TenantCtx;

/// The whole probe's budget (B8-5).
pub const BUDGET: Duration = Duration::from_secs(20);

/// The id the probe's request carries where a turn carries its case and conversation (nothing is stored under it).
pub const PROBE_ID: &str = "probe";

/// The usage source of every probe model call (0064 `usage_event_source_check`, D56).
pub const SOURCE: &str = "validation";

/// The usage of a probe's model calls.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageTotals {
    pub calls: u32,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub latency_ms: i64,
}

impl UsageTotals {
    fn of(calls: &[LlmUsage]) -> UsageTotals {
        UsageTotals {
            calls: u32::try_from(calls.len()).unwrap_or(u32::MAX),
            tokens_in: calls.iter().map(|u| u.tokens_in.max(0)).sum(),
            tokens_out: calls.iter().map(|u| u.tokens_out.max(0)).sum(),
            latency_ms: calls.iter().map(|u| u.latency_ms.max(0)).sum(),
        }
    }
}

/// What the agent would have answered.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Probe {
    /// The reply the customer would read.
    pub reply: String,
    /// The chunk ids the reply cites (a knowledge step's citations).
    pub cited: Vec<String>,
    /// The W/X/P operations the plan reached, in plan order: never sent.
    pub would_call: Vec<String>,
    /// The R operations that ran (answered or failed), in plan order.
    pub ran: Vec<String>,
    /// The turn would have been handed over (a gate, no procedure, a required operation not offered, unmet success
    /// criteria).
    pub handover: bool,
    pub usage: UsageTotals,
}

/// How a probe runs: the gate hooks, the executors and the budget ([`ProbeOpts::new`]: what the runtime runs with).
#[derive(Clone)]
pub struct ProbeOpts {
    pub gates: Gates,
    pub executors: Executors,
    pub budget: Duration,
}

impl ProbeOpts {
    /// The gate hooks on `st` (`AppState.gates`), B7's connector executor for instance targets, [`BUDGET`].
    pub fn new(st: &AppState) -> ProbeOpts {
        let instance: Arc<dyn OpExecutor> = Arc::new(ConnectorExecutor::new(st.clone()));
        ProbeOpts {
            gates: st.gates.clone(),
            executors: Executors::default().with_instance(instance),
            budget: BUDGET,
        }
    }
}

/// One message's test-env answer on `design`, with no side effects (see the module doc).
pub async fn probe_turn(
    st: &AppState,
    acting: &TenantCtx,
    job: &JobRef,
    design: CaseDesign,
    text: &str,
) -> ApiResult<Probe> {
    probe_turn_with(st, acting, job, design, text, &ProbeOpts::new(st)).await
}

/// [`probe_turn`] with `opts` (B5's gate hooks, a test's executors or budget).
pub async fn probe_turn_with(
    st: &AppState,
    acting: &TenantCtx,
    job: &JobRef,
    design: CaseDesign,
    text: &str,
    opts: &ProbeOpts,
) -> ApiResult<Probe> {
    let ran = tokio::time::timeout(opts.budget, run(st, acting, job, &design, text, opts)).await;
    let Ok((answer, calls)) = ran else {
        tracing::warn!(job = %job.key, code = "probe_over_budget", "probe_turn");
        return Err(ApiError::Unavailable(
            "unavailable",
            "The validation turn did not finish in time; try again shortly.".into(),
            Some(5),
        ));
    };
    if !calls.is_empty() {
        let mut tx = store::begin_job(&st.db, acting.tenant, job.id).await?;
        for (label, usage) in &calls {
            crate::usage::emit(
                &mut tx,
                &st.cfg.metering,
                &crate::usage::UsageEvent {
                    job_id: Some(job.id),
                    source: SOURCE,
                    use_: label,
                    activity: None,
                    conversation_id: None,
                    case_id: None,
                    actor: &acting.caller.sub,
                    usage,
                },
            )
            .await?;
        }
        tx.commit().await?;
    }
    let mut probe = answer?;
    let usages: Vec<LlmUsage> = calls.into_iter().map(|(_, u)| u).collect();
    probe.usage = UsageTotals::of(&usages);
    Ok(probe)
}

/// A probe that answers `reply` and ran nothing.
fn answered(reply: impl Into<String>, handover: bool) -> Probe {
    Probe {
        reply: reply.into(),
        handover,
        ..Probe::default()
    }
}

/// The routes a probe of `design` reads: the release snapshot for a Live design, else the working rows.
fn routes_of(design: &CaseDesign) -> routes_store::DesignOf {
    match design {
        CaseDesign::Live { .. } => routes_store::DesignOf::Live,
        CaseDesign::Draft { .. } => routes_store::DesignOf::Draft,
    }
}

/// One metered model call of a probe: its `usage_event.use` label and its usage.
type Metered = (&'static str, LlmUsage);

/// The probe until the reply, plus the usage of every model call it made (emitted by the caller, within budget).
async fn run(
    st: &AppState,
    acting: &TenantCtx,
    job: &JobRef,
    want: &CaseDesign,
    text: &str,
    opts: &ProbeOpts,
) -> (ApiResult<Probe>, Vec<Metered>) {
    let mut calls = Vec::new();
    let out = turn(st, acting, job, want, text, opts, &mut calls).await;
    (out, calls)
}

#[allow(clippy::too_many_arguments)]
async fn turn(
    st: &AppState,
    acting: &TenantCtx,
    job: &JobRef,
    want: &CaseDesign,
    text: &str,
    opts: &ProbeOpts,
    calls: &mut Vec<Metered>,
) -> ApiResult<Probe> {
    let tenant = acting.tenant;
    // gate 2, as a turn: a tenant that is not active gets the neutral text and no model call
    if !VerdictGate.admits(st, tenant).await? {
        return Ok(answered(REFUSAL_TEXT, false));
    }
    let of = match want {
        CaseDesign::Live { .. } => DesignOf::Live,
        CaseDesign::Draft { .. } => DesignOf::Draft,
    };
    let (design, resolved) = {
        let mut tx = store::begin_job(&st.db, tenant, job.id).await?;
        let d = RuntimeDesign::load(&mut tx, &st.design, job, &of).await?;
        let r = llm::resolve(&mut tx, st, tenant, job, Use::Qualify, &of).await?;
        tx.commit().await?;
        (d, r)
    };
    if design.case != *want {
        return Err(ApiError::Conflict(
            "design_changed",
            "The job's design is no longer the one named; read it again and retry.".into(),
        ));
    }
    let verdict = opts
        .gates
        .request
        .check(
            st,
            RequestInput {
                tenant,
                job,
                env: Env::Test,
                conversation_id: PROBE_ID,
                text,
                design: &design,
            },
        )
        .await;
    if verdict.handover_reason().is_some() {
        let notice = crate::channels::connector::outbound_text(
            &crate::channels::connector::Outbound::HandoverNotice { eta_minutes: None },
        )
        .unwrap_or_default();
        return Ok(answered(notice, true));
    }
    if verdict.decision == Decision::Block {
        return Ok(answered(GATE_BLOCKED_TEXT, false));
    }
    let hint = opts.gates.hinter.hint(&verdict, &design);
    let model = match resolved {
        // a person runs the probe: the router attributes the call to them, not to agent's service account
        Some(r) => {
            qualify::model(st, tenant, r.acting_for(Some(acting.caller.sub.as_str()))).await?
        }
        None => None,
    };
    let Some(model) = model else {
        tracing::warn!(code = "qualify_unbound", job = %job.key, "probe_turn");
        return Ok(answered(TURN_FAILED, false));
    };
    let prompt = crate::runtime::prompt::assemble(&(&design).into(), None, None);
    let (extracted, usage) = qualify::extract(
        &model,
        text,
        None,
        &design.spaces,
        &design.settings,
        &prompt,
    )
    .await;
    calls.push(("qualify", usage));
    let extracted = extracted?;
    // `qualify::step` strips system slots (retail ones, every registered record kind's key and any other lookup key; a live case fills them from the customer context). The probe is a staff
    // dry-run with no customer records to resolve them from, so it keeps the ids the staff message typed and plans the
    // whole procedure; a customer's typed id never reaches a live plan.
    let typed: Vec<(String, String)> = extracted
        .slots
        .iter()
        .filter(|(k, v)| qualify::never_asked(k) && !v.trim().is_empty())
        .map(|(k, v)| (k.clone(), v.trim().to_owned()))
        .collect();
    let named_space = extracted.context_space.clone().unwrap_or_default();
    // the sole-space fallback keeps what the extraction found, even though the classifier named no space
    let slots = extracted.slots.clone();
    let (qualified, _) = qualify::step(None, extracted, &design.spaces, PROBE_ID);
    let d = match qualified {
        Outcome::Unclear => {
            // A one-space job has no menu to offer (the drain's fallback, #135): the message belongs to its only
            // space, deterministically, and the staff dry run keeps the ids the tester typed.
            let Some(space) = qualify::sole_space(&design.spaces) else {
                return Ok(answered(qualify::unclear(&design.spaces), false));
            };
            let mut d = Draft {
                context_space: space.key.clone(),
                slots: Default::default(),
                awaiting: None,
                message_id: Some(PROBE_ID.to_owned()),
            };
            qualify::fold(space, &mut d, slots);
            d.slots.extend(typed);
            d
        }
        Outcome::Ask(slot) => return Ok(answered(qualify::question(&slot, &named_space), false)),
        Outcome::Ready(mut d) => {
            d.slots.extend(typed);
            d
        }
    };
    let Some(space) = design
        .spaces
        .by_key(&d.context_space)
        .filter(|s| !s.area.is_empty())
    else {
        return Ok(answered(qualify::unclear(&design.spaces), false));
    };
    let (release_id, draft_rev) = store::stamp(&design.case);
    let req = QualifiedRequest {
        case_id: PROBE_ID.to_owned(),
        conversation_id: PROBE_ID.to_owned(),
        area: space.area.clone(),
        context_space: space.key.clone(),
        request_type: space.space().to_owned(),
        activity: None,
        object: None,
        modality: None,
        slots: d.slots,
        customer_id: None,
        identity_level: KNOWN_CUSTOMER_IDENTITY,
        message_id: None,
        initiator_sub: Some(acting.caller.sub.clone()),
        qualified_release_id: release_id,
        qualified_draft_rev: draft_rev,
        route_hint: hint,
        layers: prompt.layers(),
        prompt_warning: prompt.warning.clone(),
    };
    let Some((procedure, phases)) = DesignProcedures.plan(&design, &req) else {
        // no procedure serves the request: the turn would be handed over (`out_of_scope`)
        return Ok(answered(
            resolution_reply(&req, None, CaseOutcome::Handover),
            true,
        ));
    };
    let checks = DesignProcedures.checks(&design, &req);
    let plan = Plan {
        procedure,
        design: design.case.clone(),
        phases,
        success_criteria: checks.success_criteria,
        tool_requirements: checks.tool_requirements,
        assistant: None,
    };
    let exec = Exec {
        st,
        opts,
        tenant,
        job,
        design: &design,
        routes: routes_of(want),
        question: text,
        answers: Mutex::new(Vec::new()),
    };
    for op in &plan.tool_requirements {
        if exec.target(op).await?.0 == ResolvedTarget::NotOffered {
            return Ok(answered(
                resolution_reply(&req, Some(&plan), CaseOutcome::Handover),
                true,
            ));
        }
    }
    let mut probe = Probe::default();
    let mut results = Map::new();
    let level = req.identity_level;
    let mut end: Option<(CaseOutcome, Option<String>, Option<String>)> = None;
    'phases: for phase in &plan.phases {
        let mut ctx = results.clone();
        ctx.insert("request".into(), request_ctx(&req));
        let mut stop: Option<(CaseOutcome, Option<String>, Option<String>)> = None;
        for step in &phase.steps {
            let op_key = step.op_key();
            // an operation the catalogue does not know is a write (P), never a read (the runner's rule): only a KNOWN
            // read reaches the level check below, so there is no level to default for an absent operation
            let def = match def_of(&design, &op_key) {
                Some(d) if d.side_effect == SideEffect::R => d,
                _ => {
                    probe.would_call.push(op_key);
                    continue;
                }
            };
            let need = def.identity_level;
            if authorise::identity(level, need).is_err() {
                // a known customer is never asked for a factor: a step above their level goes to a colleague
                if stop.is_none() {
                    stop = Some((
                        CaseOutcome::Handover,
                        Some(STEP_UP_REQUIRED.to_owned()),
                        None,
                    ));
                }
                continue;
            }
            if !step.preconditions.is_empty() {
                let mut facts = case_facts(&req, &plan, &results, &Default::default());
                facts.identity_level = level;
                let unmet = crate::runtime::conditions::failed(&step.preconditions, &facts);
                if !unmet.is_empty() {
                    let outcome = if step.gate {
                        CaseOutcome::NotEligible
                    } else {
                        CaseOutcome::Handover
                    };
                    if stop.is_none() {
                        stop = Some((outcome, Some("precondition_unmet".into()), None));
                    }
                    continue;
                }
            }
            let input = resolve_args(&step.args, &ctx);
            let out = match exec.call(&op_key, Some(&def), input, level).await? {
                Ok(o) => o,
                // runner step 6b: refused before the call, as the live case is (it hands over)
                Err(code) => {
                    if stop.is_none() {
                        let code = if code == "identity_required" {
                            STEP_UP_REQUIRED
                        } else {
                            code
                        };
                        stop = Some((CaseOutcome::Handover, Some(code.to_owned()), None));
                    }
                    continue;
                }
            };
            probe.ran.push(op_key.clone());
            if out.state == OpState::Ok {
                results.insert(step.name.clone(), Value::Object(out.output));
            } else {
                let code = out.outcome_code.clone();
                let reason = failure_reason(&step.operation, code.as_deref());
                if stop.is_none() {
                    stop = Some((CaseOutcome::Failed, code, Some(reason)));
                }
            }
        }
        if stop.is_some() {
            end = stop;
            break 'phases;
        }
        for step in phase.steps.iter().filter(|s| s.gate) {
            let out = results.get(&step.name);
            if answered_false(out) {
                let text = |k: &str| {
                    out.and_then(|o| o.get(k))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                };
                // as the runtime: a rule that cannot decide hands over, its reason stays internal
                if let Some(undecided) =
                    text("outcome").filter(|o| matches!(o.as_str(), "placeholder" | "escalate"))
                {
                    let code = text("code").filter(|c| !c.is_empty()).unwrap_or(undecided);
                    end = Some((CaseOutcome::Handover, Some(code), None));
                    break 'phases;
                }
                let code = text("code").unwrap_or_else(|| "not_eligible".into());
                // as the runtime: only the platform's rule's reason reaches the customer
                let reason = text("reason").filter(|_| step.operation == "rule.evaluate");
                end = Some((CaseOutcome::NotEligible, Some(code), reason));
                break 'phases;
            }
        }
    }
    // KH T7: the knowledge step's synthesis calls are metered with the probe's
    calls.extend(
        std::mem::take(&mut *exec.answers.lock().unwrap_or_else(PoisonError::into_inner))
            .into_iter()
            .map(|u| (builtin::USAGE_LABEL, u)),
    );
    let (outcome, code, reason) = match end {
        Some(e) => e,
        None => {
            let unmet = crate::runtime::conditions::failed(
                &plan.success_criteria,
                &case_facts(&req, &plan, &results, &Default::default()),
            );
            if unmet.is_empty() {
                (CaseOutcome::Resolved, None, None)
            } else {
                (CaseOutcome::Handover, None, None)
            }
        }
    };
    let mut ctx = results.clone();
    ctx.insert("request".into(), request_ctx(&req));
    probe.cited = knowledge_use(&plan, &results, &ctx)
        .map(|k| k.citations)
        .unwrap_or_default();
    probe.handover = outcome == CaseOutcome::Handover;
    let res = Resolution {
        case_id: PROBE_ID.to_owned(),
        conversation_id: PROBE_ID.to_owned(),
        context_space: req.context_space.clone(),
        request_type: req.request_type.clone(),
        outcome,
        code,
        reason,
        facts: results,
        procedure: plan.procedure_label(),
        design: Some(plan.design.clone()),
        knowledge: None,
        trace: None,
        out_of_hours: None,
        // a probe never runs a write: the live case would have run the ones it skipped (`would_call`)
        wrote: !probe.would_call.is_empty(),
        round: 0,
    };
    probe.reply = compose(&res);
    Ok(probe)
}

/// The reply of a request that ends `outcome` before any step ran.
fn resolution_reply(req: &QualifiedRequest, plan: Option<&Plan>, outcome: CaseOutcome) -> String {
    compose(&Resolution {
        case_id: PROBE_ID.to_owned(),
        conversation_id: PROBE_ID.to_owned(),
        context_space: req.context_space.clone(),
        request_type: req.request_type.clone(),
        outcome,
        code: None,
        reason: None,
        facts: Map::new(),
        procedure: plan.and_then(Plan::procedure_label),
        design: plan.map(|p| p.design.clone()),
        knowledge: None,
        trace: None,
        out_of_hours: None,
        wrote: false,
        round: 0,
    })
}

/// The request's slots (and customer) as a step's `$request.*` context (crate::runtime::case::CaseStore::context).
fn request_ctx(req: &QualifiedRequest) -> Value {
    let mut request: Map<String, Value> = req
        .slots
        .iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    if let Some(c) = &req.customer_id {
        request.insert("customer_id".into(), Value::String(c.clone()));
    }
    Value::Object(request)
}

/// The catalogue entry of `op_key`: the exact key, else its base operation (a rule id is a variant the catalogue
/// does not list).
fn def_of(design: &RuntimeDesign, op_key: &str) -> Option<OpDef> {
    design
        .catalog
        .get(op_key)
        .or_else(|| {
            op_key
                .split_once('{')
                .and_then(|(base, _)| design.catalog.get(base))
        })
        .cloned()
}

/// The variant of an operation key (`rule.evaluate{RET-01}` → `RET-01`; none → `""`).
fn variant_of(op_key: &str) -> &str {
    op_key
        .split_once('{')
        .and_then(|(_, v)| v.strip_suffix('}'))
        .unwrap_or("")
}

/// The R10 defaults (read from the embedded connector seed once).
static DEFAULTS: LazyLock<DefaultRoutes> = LazyLock::new(DefaultRoutes::new);

/// One probe's read path: the routes of its design in env `test`, then the runner's executors.
struct Exec<'a> {
    st: &'a AppState,
    opts: &'a ProbeOpts,
    tenant: uuid::Uuid,
    job: &'a JobRef,
    design: &'a RuntimeDesign,
    routes: routes_store::DesignOf,
    /// The probe's message: the question a knowledge step answers (KH T7).
    question: &'a str,
    /// The usage of every knowledge synthesis call (metered with the probe's, KH T7).
    answers: Mutex<Vec<LlmUsage>>,
}

impl Exec<'_> {
    /// The target of `op` (B7's routes of the design, then the defaults) and whether it is a stub instance, in a
    /// short read-only transaction.
    async fn target(&self, op: &str) -> ApiResult<(ResolvedTarget, bool)> {
        let mut tx = store::begin_job(&self.st.db, self.tenant, self.job.id).await?;
        let routes = routes_store::effective(
            &mut tx,
            &self.st.design,
            self.job.id,
            Env::Test.as_str(),
            self.routes,
        )
        .await?;
        let target = match ops::route_for(&routes, op) {
            Some(r) => ops::target_of(&r.target, op),
            None => {
                let q = RouteQuery {
                    tenant: self.tenant,
                    job_id: self.job.id,
                    job_key: self.job.key.clone(),
                    env: Env::Test,
                    op: op.to_owned(),
                };
                DEFAULTS.default_target(&q)
            }
        };
        let stub = match &target {
            ResolvedTarget::Instance { instance_id } => ops::is_stub(&mut tx, instance_id).await?,
            _ => false,
        };
        tx.commit().await?;
        Ok((target, stub))
    }

    /// One read with the runner's step 9 timeout (stub 3 s, else 300 s); `Err(code)` = refused before the call by the
    /// runner's step 6b ([`served_refusal`]: the operation that serves a variant needs inputs or a level the call
    /// lacks), so no executor runs.
    async fn call(
        &self,
        op_key: &str,
        def: Option<&OpDef>,
        inputs: Map<String, Value>,
        level: u8,
    ) -> ApiResult<Result<OpOutcome, &'static str>> {
        let (target, stub) = self.target(op_key).await?;
        // as the runner (F4): a connector case.create never sends a customer_email the call names; a probe has no
        // case party, so the template's service user answers
        let mut req = OpRequest {
            tenant_id: self.tenant,
            job_id: self.job.id,
            operation: op_key.to_owned(),
            env: Env::Test.into(),
            inputs,
            outputs: self
                .design
                .catalog
                .outputs(def.map_or(op_key, |d| d.key.as_str())),
            idempotency_key: String::new(),
            actor: None,
            case_id: None,
        };
        // also when the operation that SERVES the call is a case.create (audit of agent#92)
        if matches!(target, ResolvedTarget::Instance { .. })
            && serves_case_create(&self.opts.executors, &req, &target).await
        {
            req.inputs = ops::with_customer_email(std::mem::take(&mut req.inputs), None);
        }
        // the probe's identified customer holds the channel identity keys in state; an anonymous probe holds none
        let state_keys: std::collections::BTreeSet<String> = if level >= 1 {
            [
                crate::conversations::state::SUBJECT,
                crate::conversations::state::EMAIL,
            ]
            .map(str::to_owned)
            .into()
        } else {
            std::collections::BTreeSet::new()
        };
        if let Some((code, _)) = served_refusal(
            &self.opts.executors,
            &req,
            &target,
            self.design,
            &state_keys,
        )
        .await
        {
            return Ok(Err(code));
        }
        let limit = if stub { STUB_TIMEOUT } else { LIVE_TIMEOUT };
        Ok(Ok(tokio::time::timeout(
            limit,
            self.dispatch(&req, &target),
        )
        .await
        .unwrap_or_else(|_| {
            failed("timeout", format!("{op_key} did not answer in time."))
        })))
    }

    /// A registered executor, else the instance executor (`not_handled` from it is an error: the target was an
    /// instance); `None` for a target no executor runs.
    async fn run_executor(&self, req: &OpRequest, target: &ResolvedTarget) -> Option<OpOutcome> {
        if let Some(x) = self.opts.executors.registered(&req.operation) {
            return Some(x.execute(req, target).await);
        }
        let ResolvedTarget::Instance { instance_id } = target else {
            return None;
        };
        let Some(x) = self.opts.executors.instance() else {
            return Some(failed(
                "connector_misconfigured",
                "No connector executor is registered.",
            ));
        };
        let _permit = ops::instance_permit(req.tenant_id, instance_id).await;
        let o = x.execute(req, target).await;
        Some(if o.state == OpState::NotHandled {
            failed(
                "connector_misconfigured",
                "The routed system did not run the call.",
            )
        } else {
            o
        })
    }

    /// The runner's step 9 for a read: the executor of the target, else B4's built-ins.
    async fn dispatch(&self, req: &OpRequest, target: &ResolvedTarget) -> OpOutcome {
        if let Some(o) = self.run_executor(req, target).await {
            return o;
        }
        match target {
            ResolvedTarget::Builtin { detail } => match detail.as_deref() {
                Some("knowledge") => {
                    let asked = builtin::Asked {
                        operation: &req.operation,
                        channel: None,
                        question: Some(self.question),
                    };
                    let run = builtin::knowledge(
                        self.st,
                        self.tenant,
                        self.job,
                        self.design,
                        &asked,
                        &req.inputs,
                    )
                    .await;
                    if let Some(u) = run.usage {
                        self.answers
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(u);
                    }
                    run.outcome
                }
                Some("rule") => builtin::rule(self.design, variant_of(&req.operation), &req.inputs),
                _ => failed(
                    "not_offered",
                    format!("{} is not a built-in operation.", req.operation),
                ),
            },
            ResolvedTarget::DeepLink { url } => {
                let mut out = Map::new();
                out.insert("deep_link".into(), Value::String(url.clone()));
                outcome(OpState::Ok, None, None, out)
            }
            // agent_cases only runs writes (`case.create{…}`), which a probe never sends
            ResolvedTarget::Instance { .. }
            | ResolvedTarget::AgentCases
            | ResolvedTarget::NotOffered => failed(
                "not_offered",
                format!("{} is not offered on this job.", req.operation),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_totals_sum_every_call() {
        let u = |i, o, ms| LlmUsage {
            tokens_in: i,
            tokens_out: o,
            latency_ms: ms,
            ..LlmUsage::default()
        };
        assert_eq!(UsageTotals::of(&[]), UsageTotals::default());
        assert_eq!(
            UsageTotals::of(&[u(9, 5, 100), u(3, -1, 20)]),
            UsageTotals {
                calls: 2,
                tokens_in: 12,
                tokens_out: 5,
                latency_ms: 120
            }
        );
    }

    #[test]
    fn variant_is_the_braced_part() {
        assert_eq!(variant_of("rule.evaluate{RET-01}"), "RET-01");
        assert_eq!(variant_of("order.read"), "");
    }
}
