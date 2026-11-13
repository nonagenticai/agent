//! The operation runner (B4 T6, spec 12 §3.3; port of care-bus-agents@96c608c `crates/carebus/src/runner.rs`): one
//! task per scope consumes `op.call` from the scope's `bus:op`, runs the call and publishes `op.result` on
//! `bus:op:results`. It connects to Postgres as `agent_runtime` (B4 0065, 0123).
//!
//! | Step | What |
//! |---|---|
//! | 1 | scope guard (the bus dead-letters another scope's envelope; the call's env must be the scope's) and an RLS transaction (tenant + job) |
//! | 1b | tenant active, else `tenant_suspended` / `tenant_archived` ([`authorise::tenant_active`]) |
//! | 2 | the licence gate ([`licence`]): the tenant's stored verdict is a fresh `active` (or the tenant is platform-owned), else `denied` / `licence_inactive` before any other check; the case hands over (`tool_failed`) and an `operation.denied` audit row is written; runs while `LICENSE_GATE_2_ENABLED` is on |
//! | 3 | A2 ([`declared::required_keys`]): the conversation state holds every key the op requires (its declaration's `requires`, else `subject` for a catalogue level ≥1), else `state_required` (detail: the key names); a staff-only op is `staff_only` |
//! | 3a | T6a: the step's `performed_by` is the calling role, else `not_entitled`; its preconditions hold on the call's case facts, else `precondition_unmet` (detail: the condition ids) |
//! | 3c | A2 ([`declared::Runner::resolve_inputs`]): the op's declared `state` inputs are read from conversation state into the request; a missing key runs its producer chain as `resolve` sub-calls (each a full `handle`), a missing `ask_user` key answers `input_required` (output = the [`declared::Ask`]); a chain that fails answers `input_unresolved` per the declaration's `on_failure` |
//! | 4 | a W/X/P call's `confirm_ref` is a message authored by the conversation's user (role `customer` or `system`), else `confirmation_required`; skipped for a declaration with `confirmation: auto` and for `resolve` sub-calls |
//! | 5b | B5's [`authorise::ToolGate`], per call (B4: allow all; `bin/runner` installs B5's `ToolPoint`: toolset, then P2) |
//! | 6 | [`ops::EffectiveRoutes`]: B7's `operation_route` rows (R10), then the defaults |
//! | 6b, 7 | done in step 5b's `job_design::gates::hooks::ToolPoint` -> `DesignToolsets::allows` (`job_design::toolset::Toolsets::decide`): role enabled on platform and job, the activity known to the toolset, the op in its grant; plus the served-operation check (`not_offered`) in this module |
//! | 8 | route health ([`route_health`]): an instance B7 marks `auth_failed` / `invalid_target` is not called; the step takes the unrouted (`not_offered`) outcome with reason `route_unhealthy`; unknown health routes (fail open) |
//! | 7b | T6a ([`cap`]): an X/P call of a case at its per-case cap (`max_per_case`, default 1) is `denied` / `case_cap_reached` and the case hands over (`tool_failed`) |
//! | 8b | a W/X/P call inserts its `pending` row first (own transaction); a duplicate idempotency key answers the stored outcome |
//! | 9 | the executor ([`ops::Executors`]: registrations, then B7's instance executor; B4's built-ins and `agent_cases` for what it does not handle), a per-instance semaphore and a timeout (stub 3 s, else 300 s) |
//! | 10 | an X/P call that did not answer runs its verify read; no answer → `unknown_outcome`: the case `alert`, a handover with reason `unknown_outcome`, never retried |
//! | 10b | T6a: an `ok` answer's postconditions; any false → the call stays `ok` with outcome `postcondition_failed`, the case `alert`, a handover with reason `tool_failed`, never retried |
//! | 10c | A2 ([`declared::Runner::store_outputs`]): the output fields declared to write state are removed from the answer and stored (secrets sealed); the result keeps only the rest |
//! | 11 | the `operation_call` row, the `operation` case event and the `execute` audit row in one transaction, then `op.result` |
//!
//! **Provenance `context`** (B4 known customer, [`customer_context`]): the orchestrator's read of the identified customer's
//! records at case start. It runs only the read operations of `customer_context::CONTEXT_OPS` (anything else is refused
//! `not_context_op`), skips step 5b (the request's activity lists none of them), is capped at
//! `customer_context::MAX_CONTEXT_CALLS` per case (`context_cap_reached`, counted from `operation_call`), and keeps steps
//! 1b, 3, 8b..11 (the call is a row, a case event and an `execute` audit row with phase `context`).
//! A successful write invalidates the conversation's cached customer context.
//!
//! Every handover item the runner queues (step 7b, 10, 10b and `case.create{handover|complaint}`) tells the job's
//! worker channels after its commit ([`Runner::notify_workers`]); a failed notify is logged, never fails the call.
//!
//! Inputs and outputs never reach a log line, an audit row, a case event or the console (K50: ids and codes).
pub mod agent_cases;
pub mod authorise;
pub mod builtin;
pub mod cap;
pub mod declared;
pub mod licence;
pub mod ops;
pub mod route_health;

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use agent_model::kinds::catalog::{IdentityLevel, SideEffect};
use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, Postgres, Transaction};

use self::agent_cases::Queued;
use self::authorise::{AllowAll, ToolGate};
use self::ops::{
    CallRow, ConnectorExecutor, EffectiveRoutes, Executors, Final, OpExecutor, OpOutcome,
    OpRequest, OpState, Pending, failed, outcome,
};
use self::route_health::Health;
use super::bus::{Bus, ConsumeOptions, Envelope, groups};
use super::case::CaseStore;
use super::catalog::{OpDef, OperationCatalog};
use super::conditions::{self, StepFact};
use super::customer_context;
use super::design::{DesignCache, RuntimeDesign};
use super::domain::{self, OP_CALL, OP_RESULT, OpCall, OpResult};
use super::routes::{ResolvedTarget, RouteQuery, RouteResolver, route_kind};
use super::scope::{Env, Scope};
use crate::audit::{self, Actor, Event, Status};
use crate::conversations::{handover, ownership};
use crate::design::{CaseDesign, JobRef};
use crate::state::AppState;

/// The runtime role (consumer group, heartbeat role, envelope `from`).
pub const ROLE: &str = "runner";
/// Step 9 timeout of a stub instance.
pub const STUB_TIMEOUT: Duration = Duration::from_secs(3);
/// Step 9 timeout of everything else (a live system, strata, a built-in). It wraps the knowledge answer's model call
/// ([`builtin::SYNTHESIS_BUDGET`], 150 s), so it stays above that (owner 2026-11-03: slow local-llm).
pub const LIVE_TIMEOUT: Duration = Duration::from_secs(300);
/// The longest one `op.call` handler runs: the call under [`LIVE_TIMEOUT`], then (a write) its verify read under
/// [`LIVE_TIMEOUT`] again. The runner's `reclaim_idle` is this plus slack, so a peer never takes a live call over.
pub const HANDLER_BUDGET: Duration = Duration::from_secs(2 * LIVE_TIMEOUT.as_secs());
/// The runner's consumer options (what `new` uses): `reclaim_idle` above [`HANDLER_BUDGET`].
pub fn consume_options() -> ConsumeOptions {
    ConsumeOptions::for_handler_budget(HANDLER_BUDGET)
}
/// The longest `operation_call.detail` (0062 CHECK, characters).
const MAX_DETAIL: usize = 2000;
/// T6a: the outcome of an `ok` call whose postconditions do not hold.
pub const POSTCONDITION_FAILED: &str = "postcondition_failed";
/// T6a: the `denied` outcome of a call whose preconditions do not hold.
pub const PRECONDITION_UNMET: &str = "precondition_unmet";

/// `object.verb{variant}` of a call (`*` / empty variant left off).
pub fn op_key_of(call: &OpCall) -> String {
    match call.variant.as_deref().map(str::trim) {
        Some(v) if !v.is_empty() && v != "*" => format!("{}{{{v}}}", call.operation),
        _ => call.operation.clone(),
    }
}

/// The catalogue entry of `op_key`: the exact key, else its base operation (`rule.evaluate{RET-01}` →
/// `rule.evaluate`: a rule id is a variant the catalogue does not list).
pub(crate) fn def_of(design: &RuntimeDesign, op_key: &str) -> Option<OpDef> {
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

/// Owner 2026-11-01: whether the conversation's identity may run an operation at catalogue level `need`. A2's state
/// requirement only asks for a `subject`; this adds the strength the tenant's IAM procedure declared, so an
/// `identified` (level 1) person never runs a level-2 op while a `verified` (level 2) one does. Only an IAM identity
/// is level-gated: A2's state rule governs every other source (assertion/staff, a channel, a bare subject), and a
/// machine caller keeps the behaviour it had. A2's step-up (level 3, which no conversation level reaches) keeps its
/// confirmation-driven behaviour. The identity chain's own steps are exempt (`establishing`); a serving call that
/// starts while the person is still unknown is checked again after step 3c.
fn level_admits(level_now: u8, need: IdentityLevel, identity_source: Option<&str>) -> bool {
    let Some(required) = authorise::required_level(need) else {
        return true; // staff-only: refused by its own step
    };
    if required > 2 || level_now >= required {
        return true;
    }
    identity_source != Some(crate::conversations::state::SOURCE_IAM)
}

/// The variant of an operation key (`case.create{handover}` → `handover`; none → `""`).
fn variant_of(op_key: &str) -> &str {
    op_key
        .split_once('{')
        .and_then(|(_, v)| v.strip_suffix('}'))
        .unwrap_or("")
}

/// The `op.result` topic of an `op.call` topic (`<area>.<space>.op.call` → `<area>.<space>.op.result`).
pub fn result_topic(call_topic: &str) -> String {
    match call_topic.strip_suffix(OP_CALL) {
        Some(prefix) => format!("{prefix}{OP_RESULT}"),
        None => OP_RESULT.to_owned(),
    }
}

/// Codes after which a call that cannot have changed anything (R, W with its key, or never sent) may run again.
fn transient(code: Option<&str>) -> bool {
    matches!(
        code,
        Some("timeout" | "upstream_unavailable" | "rate_limited")
    )
}

/// The `op.result` state of a final `operation_call.state`.
fn result_state(state: &str) -> domain::OpState {
    match state {
        "ok" => domain::OpState::Ok,
        "unknown_outcome" => domain::OpState::UnknownOutcome,
        "denied" => domain::OpState::Denied,
        _ => domain::OpState::Error,
    }
}

fn capped(detail: Option<String>) -> Option<String> {
    detail.map(|d| d.chars().take(MAX_DETAIL).collect())
}

/// The design a call runs on: the call's (the plan's, D63), else the design read now. Test carries `draft_rev`,
/// prod a release (or none: the baseline).
fn call_design(call: &OpCall, env: Env, now: &CaseDesign) -> CaseDesign {
    match (env, now) {
        (Env::Test, CaseDesign::Draft { draft_rev }) => CaseDesign::Draft {
            draft_rev: call.draft_rev.unwrap_or(*draft_rev),
        },
        (Env::Test, _) => CaseDesign::Draft {
            draft_rev: call.draft_rev.unwrap_or(0),
        },
        (Env::Prod, CaseDesign::Live { release_id }) => CaseDesign::Live {
            release_id: call.release_id.clone().or_else(|| release_id.clone()),
        },
        (Env::Prod, _) => CaseDesign::Live {
            release_id: call.release_id.clone(),
        },
    }
}

/// `(identity_level, verified_party, conversation identity_source)` of a call's case, all derived from the case's
/// conversation state: level 2 and the `subject` for a known customer, else 0 and none.
type CaseIdentity = (i16, Option<String>, Option<String>);

/// What one call came to before it is recorded.
struct Ran {
    out: OpOutcome,
    /// The call was refused before it ran (state `denied`).
    denied: bool,
    retryable: bool,
    /// Why the runner answered without calling the system (audit `reason`), if it did.
    reason: Option<&'static str>,
}

/// One scope's runner.
pub struct Runner {
    bus: Bus,
    scope: Scope,
    /// `db` is the `agent_runtime` pool; `design` the registry; the executors and built-ins use the rest.
    st: AppState,
    designs: Arc<DesignCache>,
    routes: Arc<dyn RouteResolver>,
    executors: Executors,
    gate: Arc<dyn ToolGate>,
    /// Step 2's remembered decision for this scope's tenant.
    licence: licence::LicenceCache,
    consumer: String,
    consume: ConsumeOptions,
    job: OnceLock<JobRef>,
    stub_timeout: Duration,
    live_timeout: Duration,
    /// Step 8: cached reads of the routed instances' B7 status ([`route_health`]).
    health: route_health::RouteHealth,
}

impl Runner {
    /// On a scoped bus (refuses the root bus), with B7's [`ConnectorExecutor`] for instance targets and the R10
    /// route resolver.
    pub fn new(bus: Bus, st: AppState, designs: Arc<DesignCache>) -> Result<Self> {
        let scope = bus
            .scope()
            .cloned()
            .context("the runner runs on a scoped bus")?;
        let instance: Arc<dyn OpExecutor> = Arc::new(ConnectorExecutor::new(st.clone()));
        Ok(Self {
            // B8 T15: shadow mode turns a `not_offered` operation into a handover while it is on
            routes: Arc::new(crate::discovery::shadow::ShadowRoutes::new(Arc::new(
                EffectiveRoutes::new(Arc::clone(&st.design)),
            ))),
            executors: Executors::default().with_instance(instance),
            gate: Arc::new(AllowAll),
            licence: licence::LicenceCache::default(),
            bus,
            scope,
            st,
            designs,
            consumer: super::consumer_name(ROLE),
            consume: consume_options(),
            job: OnceLock::new(),
            stub_timeout: STUB_TIMEOUT,
            live_timeout: LIVE_TIMEOUT,
            health: route_health::RouteHealth::default(),
        })
    }

    /// How long a read of an instance's health is reused (step 8; default [`route_health::MAX_STALENESS`]).
    pub fn with_route_health_ttl(mut self, ttl: Duration) -> Self {
        self.health = route_health::RouteHealth::new(ttl);
        self
    }

    /// Replace the executors (B7 registers its own; tests register fakes).
    pub fn with_executors(mut self, x: Executors) -> Self {
        self.executors = x;
        self
    }

    /// How long step 2 remembers the tenant's licence decision (default [`licence::CACHE_TTL`]; tests shorten it).
    pub fn with_licence_ttl(mut self, ttl: Duration) -> Self {
        self.licence = licence::LicenceCache::new(ttl);
        self
    }

    /// Step 2's cache with its own time source (tests advance it by hand).
    pub fn with_licence_cache(mut self, cache: licence::LicenceCache) -> Self {
        self.licence = cache;
        self
    }

    /// B5's tool gate (step 5b).
    pub fn with_gate(mut self, g: Arc<dyn ToolGate>) -> Self {
        self.gate = g;
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

    /// The step 9 timeouts (stub, everything else).
    pub fn with_timeouts(mut self, stub: Duration, live: Duration) -> Self {
        self.stub_timeout = stub;
        self.live_timeout = live;
        self
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// Consume the scope's `bus:op` until the consumer fails.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        tracing::info!(scope = %self.scope, "runner serving");
        let me: &Self = &self;
        let stream = me.bus.streams.op();
        me.bus
            .consume(
                &stream,
                groups::RUNNER,
                &me.consumer,
                me.consume.clone(),
                move |env| me.on_call(env),
            )
            .await
    }

    /// One `op.call` envelope: run it and publish its `op.result`. An error leaves the entry pending (redelivery;
    /// a write's `pending` row then answers `unknown_outcome`, never a second run).
    pub async fn on_call(&self, env: Envelope) -> Result<()> {
        if env.event() != Some(OP_CALL) {
            return Ok(());
        }
        let mut call: OpCall = env.payload_as()?;
        // `resolve` is the runner's own producer sub-call (step 3c, in process): never accepted from the bus, where it
        // would skip write confirmation and the tool gate
        if call.provenance.as_deref() == Some(domain::PROVENANCE_RESOLVE) {
            tracing::warn!(case_id = %call.case_id, "op_call_resolve_provenance_dropped");
            call.provenance = None;
        }
        let res = self.handle(&call).await?;
        let out = Envelope::new(&result_topic(&env.topic), ROLE, &call.case_id, &res)?
            .with_procedure(env.procedure.as_deref());
        self.bus
            .publish(&self.bus.streams.op_results(), &out)
            .await?;
        Ok(())
    }

    /// A transaction with both GUCs set for the scope (`app.current_tenant`, `app.current_job`), and the job.
    async fn begin(&self) -> Result<(Transaction<'static, Postgres>, JobRef)> {
        let mut tx = crate::tenancy::begin_scoped(&self.st.db, self.scope.tenant).await?;
        let j = match self.job.get() {
            Some(j) => j.clone(),
            None => {
                let id: uuid::Uuid = sqlx::query_scalar(
                    "SELECT id FROM jobs WHERE tenant_id = app_current_tenant() AND key = $1",
                )
                .bind(self.scope.job.clone())
                .fetch_optional(&mut *tx)
                .await?
                .with_context(|| format!("scope {}: no such job", self.scope))?;
                let j = JobRef {
                    id,
                    key: self.scope.job.clone(),
                };
                let _ = self.job.set(j.clone());
                j
            }
        };
        sqlx::query("SELECT set_config('app.current_job', $1, true)")
            .bind(j.id.to_string())
            .execute(&mut *tx)
            .await?;
        Ok((tx, j))
    }

    /// The whole pipeline for one call; the `op.result` to publish.
    pub async fn handle(&self, call: &OpCall) -> Result<OpResult> {
        let op_key = op_key_of(call);
        // ---- step 1: the RLS transaction, the design, the case --------------------------------------------------
        let (mut tx, job) = self.begin().await?;
        let design = self
            .designs
            .get_or_load(&mut tx, &self.st.design, &self.scope, &job)
            .await?;
        let def: Option<OpDef> = def_of(&design, &op_key);
        // an operation the catalogue does not know is gated as the strongest write (P), never as a read
        let effect = def.as_ref().map_or(SideEffect::P, |d| d.side_effect);
        let writes = effect != SideEffect::R;
        // the case's born level and party, read on every call
        let case: Option<CaseIdentity> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {level}::int2, \
                    CASE WHEN {known} THEN {subject} END, \
                    {source} \
               FROM agent_case ac \
              WHERE ac.tenant_id = app_current_tenant() AND ac.job_id = $2 AND ac.case_id = $1",
            level = crate::conversations::state::level_sql("ac"),
            known = crate::conversations::state::known_sql("ac"),
            subject = crate::conversations::state::sub("ac", crate::conversations::state::SUBJECT),
            source = crate::conversations::state::sub(
                "ac",
                crate::conversations::state::IDENTITY_SOURCE
            ),
        )))
        .bind(call.case_id.clone())
        .bind(job.id)
        .fetch_optional(&mut *tx)
        .await?;
        let case_exists = case.is_some();
        let resolve = call.provenance.as_deref() == Some(domain::PROVENANCE_RESOLVE);
        let (case_party, identity_source) = match case {
            Some((_, party, source)) => (party.filter(|v| !v.trim().is_empty()), source),
            None => (None, None),
        };
        // A case's level is derived from its conversation's state (0231/0248); read the conversation itself, so a
        // call that arrives before any case row exists (the identity chain's own step) still sees the person it
        // established.
        let level_now: u8 = {
            let level: Option<i16> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT {level}::int2 FROM conversation c \
                  WHERE c.tenant_id = app_current_tenant() AND c.job_id = $2 AND c.conversation_id = $1",
                level = crate::conversations::state::level_sql("c")
            )))
            .bind(&call.conversation_id)
            .bind(job.id)
            .fetch_optional(&mut *tx)
            .await?;
            u8::try_from(level.unwrap_or(0).clamp(0, 3)).unwrap_or(0)
        };
        // never lower than the call's own level
        let fresh = OpCall {
            identity_level: call.identity_level.max(level_now),
            ..call.clone()
        };
        let call = &fresh;
        // B4 known customer: the orchestrator's customer-context read (provenance `context`, read allowlist)
        let context = call.provenance.as_deref() == Some(domain::PROVENANCE_CONTEXT);
        // a staff test chat's typed email is looked up in the job's system (`party.list{email}`) to find the external
        // party it stands for; the lookup is allowed once, for that email only
        let staff_lookup = context
            && customer_context::staff_test_lookup(
                identity_source.as_deref(),
                &op_key,
                &call.input,
                case_party.as_deref(),
            );
        let cd = call_design(call, self.scope.env, &design.case);
        let (release_id, draft_rev) = crate::conversations::store::stamp(&cd);
        let mut row = CallRow {
            call_id: call.call_id.clone(),
            case_id: case_exists.then(|| call.case_id.clone()),
            operation: op_key.clone(),
            route: "not_offered",
            env: self.scope.env.as_str(),
            initiator: call.initiator.clone(),
            attempt: i32::try_from(call.attempt.max(1)).unwrap_or(i32::MAX),
            idempotency_key: writes.then(|| {
                call.idempotency_key
                    .clone()
                    .filter(|k| !k.trim().is_empty())
                    .unwrap_or_else(|| call.call_id.clone())
            }),
            confirm_ref: call.confirm_ref.clone(),
            release_id,
            draft_rev,
            step: json!({"phase": if context { json!(domain::PROVENANCE_CONTEXT) } else if resolve { json!(domain::PROVENANCE_RESOLVE) } else { json!(call.phase) },
                         "step": call.step, "agent_role": call.agent_role,
                         "procedure": call.procedure, "activity": call.activity,
                         "refs": ops::refs_of(&call.input)}),
        };

        // ---- steps 1 (env), 1b, 3, 3a (T6a), 4, 5b: refusals ---------------------------------------------------
        // an operation the catalogue does not know needs a verified case at least, like the P write it is gated as; it
        // is never offered (step 5b's toolset grants catalogue ops only), and a contract op is floored at its contract
        // level when it IS in the catalogue (`catalog::effective_level`)
        let need = def
            .as_ref()
            .map_or(IdentityLevel::Verified, |d| d.identity_level);
        // step 3 (A2): the state keys the op requires
        let staff_only = authorise::required_level(need).is_none();
        // A3 (owner 2026-11-01): the steps of the identity procedure that establishes the subject are the chain that
        // makes the person — a resolve sub-call on one is never asked for the subject it is about to write.
        let establishing = resolve && design.identity_steps.contains(&op_key);
        let required = declared::required_keys(
            design.policy(&op_key),
            authorise::required_level(need).is_some_and(|n| n >= 1) && !establishing,
        );
        // the conversation's state keys (presence only), read once: step 3 here, step 6b for the serving operation
        let state_keys: std::collections::BTreeSet<String> =
            crate::conversations::state::load(&mut tx, &call.conversation_id)
                .await?
                .into_keys()
                .collect();
        // A serving call that starts before the person exists can run the job's identity chain in step 3c; only that
        // call needs its identity level read again after the chain (an IAM identity it may just have established).
        let may_establish = !resolve
            && !state_keys.contains(crate::conversations::state::SUBJECT)
            && !design.identity.is_empty();
        // a required key a producer of the job can make is resolved in step 3c, not refused here (A3: `requires`
        // triggers the chain)
        let missing_state = declared::missing_keys(&required, &|k| {
            state_keys.contains(k) || (!resolve && design.producers.producer_of(k).is_some())
        });
        let auto_confirmed = resolve
            || design
                .policy(&op_key)
                .is_some_and(|p| p.confirmation == crate::runtime::declared::Confirmation::Auto);
        let refusal: Option<(String, Option<String>)> = if call.env != self.scope.env {
            Some(("scope_mismatch".into(), None))
        } else if resolve && matches!(effect, SideEffect::X | SideEffect::P) {
            // a producer runs unconfirmed: an irreversible or payment operation is never one (A2)
            Some((declared::PRODUCER_NOT_ALLOWED.into(), None))
        } else if context
            && ((!customer_context::is_context_op(&op_key) && !staff_lookup) || writes)
        {
            // a context call runs the allowlisted reads only, whatever the catalogue says of anything else
            Some((customer_context::NOT_CONTEXT_OP.into(), None))
        } else if context
            && row.case_id.is_some()
            && context_cap_reached(&mut tx, &call.case_id, &call.call_id).await?
        {
            Some((customer_context::CONTEXT_CAP.into(), None))
        } else if let Err(code) = authorise::tenant_active(&mut tx).await? {
            Some((code.into(), None))
        } else if let Err(state) =
            licence::admits(&self.licence, &self.st, &mut tx, self.scope.tenant).await?
        {
            // step 2: the licence gate (the state label is the detail: a code, never case data)
            Some((licence::CODE.into(), Some(format!("licence {state}"))))
        } else if staff_only {
            Some(("staff_only".into(), None))
        } else if !missing_state.is_empty() {
            Some((
                declared::STATE_REQUIRED.into(),
                Some(missing_state.join(", ")),
            ))
        } else if !establishing && !level_admits(level_now, need, identity_source.as_deref()) {
            // A2's state requirement says a level-1+ op needs a `subject`; for a tenant IAM identity this also says
            // the person must stand at the level the catalogue declares. The identity procedure's own steps
            // (`establishing`) keep the behaviour they had, and a serving call that starts while the person is still
            // unknown is checked again after step 3c.
            Some(("identity_required".into(), None))
        } else if let Err(code) = authorise::performed_by(call) {
            Some((code.into(), None))
        } else if let Err(ids) = authorise::preconditions(call) {
            Some((PRECONDITION_UNMET.into(), Some(ids)))
        } else if writes
            && !auto_confirmed
            && let Err(code) = authorise::confirmed(&mut tx, call).await?
        {
            Some((code.into(), None))
        } else if context || resolve {
            // (a `resolve` producer call skips 5b too: the tenant's declared chain authorises it)
            // step 5b is skipped: the activity of the request legitimately lists none of the context reads; identity,
            // ownership (3b), the cap and the audit trail still bind the call
            None
        } else {
            // step 5b, per call: B5's tool gate (toolset, then P2) for this very call
            self.gate
                .check(&self.st, self.scope.tenant, &design, call, &op_key)
                .await
                .err()
                .map(|code| (code, None))
        };
        if let Some((code, detail)) = refusal {
            let ran = Ran {
                out: outcome(OpState::Error, Some(code.as_str()), detail, Map::new()),
                denied: true,
                retryable: false,
                reason: None,
            };
            return self.record(tx, &job, call, &row, None, ran, &cd).await;
        }

        // ---- step 6: the route ----------------------------------------------------------------------------------
        let q = RouteQuery {
            tenant: self.scope.tenant,
            job_id: job.id,
            job_key: job.key.clone(),
            env: self.scope.env,
            op: op_key.clone(),
        };
        let target = self.routes.resolve(&mut tx, &q).await?;
        row.route = route_kind(&target);
        let stub = match &target {
            ResolvedTarget::Instance { instance_id } => ops::is_stub(&mut tx, instance_id).await?,
            _ => false,
        };
        // F4 (B7 R10): a connector case.create names the customer — only the runtime sets customer_email: the call's
        // value is dropped (with or without a case row: fail closed), then the email is the conversation state's
        // `email`, else the case party's email, else the template's configured service user answers
        // (ops::with_customer_email)
        let declared = design
            .catalog
            .outputs(def.as_ref().map_or(op_key.as_str(), |d| d.key.as_str()));
        let mut req = OpRequest {
            tenant_id: self.scope.tenant,
            job_id: job.id,
            operation: op_key.clone(),
            env: self.scope.env.into(),
            inputs: call.input.clone(),
            // B7 keeps only the declared outputs (the catalogue row the call resolved to)
            outputs: declared.clone(),
            idempotency_key: row.idempotency_key.clone().unwrap_or_default(),
            actor: call.initiator_sub.clone(),
            case_id: row.case_id.clone(),
        };
        if let Some(d) = design.output_decls(&op_key) {
            declared::request_outputs(&mut req.outputs, d);
        }
        // the decision follows the operation that SERVES the call as well as the one asked for (audit of agent#92); the
        // party email is read first so the transaction is not held across the template read in `served`
        let instance = matches!(target, ResolvedTarget::Instance { .. });
        let party_email = if instance && row.case_id.is_some() {
            let have = crate::conversations::state::load(&mut tx, &call.conversation_id).await?;
            match have
                .get(crate::conversations::state::EMAIL)
                .and_then(crate::conversations::state::StateValue::as_value)
                .map(str::to_owned)
            {
                Some(email) => Some(email),
                None => ops::party_email(&mut tx, &call.case_id).await?,
            }
        } else {
            None
        };
        tx.commit().await?;
        // ---- step 3c (A2): the declared state inputs, the producer chain, the ask-user pause ------------------------
        match self
            .resolve_inputs(&design, call, &op_key, &mut req)
            .await?
        {
            declared::Resolved::Ready => {}
            declared::Resolved::Ask(ask) => {
                let (tx, _) = self.begin().await?;
                let mut out = outcome(
                    OpState::Error,
                    Some(declared::INPUT_REQUIRED),
                    Some(ask.key.clone()),
                    ask.to_output(),
                );
                out.latency_ms = 0;
                let ran = Ran {
                    out,
                    denied: false,
                    retryable: false,
                    reason: None,
                };
                return self.record(tx, &job, call, &row, None, ran, &cd).await;
            }
            declared::Resolved::Failed(detail) => {
                let refuse = design
                    .policy(&op_key)
                    .is_some_and(|p| p.on_failure == crate::runtime::declared::OnFailure::Refuse);
                let (tx, _) = self.begin().await?;
                let ran = Ran {
                    out: outcome(
                        OpState::Error,
                        Some(declared::INPUT_UNRESOLVED),
                        Some(detail),
                        Map::new(),
                    ),
                    denied: refuse,
                    retryable: false,
                    reason: None,
                };
                return self.record(tx, &job, call, &row, None, ran, &cd).await;
            }
        }
        // ---- step 3c follow-up (owner 2026-11-01): the identity procedure may just have established the person.
        // An IAM identity that is weaker than the catalogue's level is refused here before the system is called (the
        // same rule as the top of the call, read again over the state the chain wrote).
        if may_establish && authorise::required_level(need).is_some_and(|n| n <= 2) {
            let (level_after, source_after) = {
                let (mut tx, _) = self.begin().await?;
                let row: Option<(i16, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "SELECT {level}::int2, {source} FROM conversation c \
                      WHERE c.tenant_id = app_current_tenant() AND c.job_id = $2 AND c.conversation_id = $1",
                    level = crate::conversations::state::level_sql("c"),
                    source = crate::conversations::state::sub(
                        "c",
                        crate::conversations::state::IDENTITY_SOURCE
                    )
                )))
                .bind(&call.conversation_id)
                .bind(job.id)
                .fetch_optional(&mut *tx)
                .await?;
                tx.commit().await?;
                match row {
                    Some((level, source)) => (u8::try_from(level.clamp(0, 3)).unwrap_or(0), source),
                    None => (0, None),
                }
            };
            if !level_admits(level_after, need, source_after.as_deref()) {
                let (tx, _) = self.begin().await?;
                let ran = Ran {
                    out: outcome(OpState::Error, Some("identity_required"), None, Map::new()),
                    denied: true,
                    retryable: false,
                    reason: None,
                };
                return self.record(tx, &job, call, &row, None, ran, &cd).await;
            }
        }
        let governed = instance && serves_case_create(&self.executors, &req, &target).await;
        if governed {
            req.inputs = ops::with_customer_email(std::mem::take(&mut req.inputs), party_email);
        }

        // ---- step 6a: a `photo_ref` input names an attachment the PLATFORM holds, so the platform checks it (not the
        // tenant's system): a live image the conversation's own user sent, else `not_found` before the executor runs
        if let Some(raw) = req
            .inputs
            .get(crate::runtime::resolve::PHOTO_SLOT)
            .filter(|v| !v.is_null())
        {
            let own = match raw
                .as_str()
                .and_then(|v| uuid::Uuid::parse_str(v.trim()).ok())
            {
                Some(id) => {
                    let (mut tx, _) = self.begin().await?;
                    let own = crate::attachments::store::is_customer_image(
                        &mut tx,
                        &call.conversation_id,
                        id,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("photo_ref check: {e}"))?;
                    tx.commit().await?;
                    own
                }
                None => false,
            };
            if !own {
                let (tx, _) = self.begin().await?;
                let ran = Ran {
                    out: outcome(OpState::Error, Some("not_found"), None, Map::new()),
                    denied: false,
                    retryable: false,
                    reason: None,
                };
                return self.record(tx, &job, call, &row, None, ran, &cd).await;
            }
        }

        // ---- step 6b (audit of agent#64): the operation that actually serves the call -------------------------------
        if let Some((code, detail)) =
            served_refusal(&self.executors, &req, &target, &design, &state_keys).await
        {
            let (tx, _) = self.begin().await?;
            let ran = Ran {
                out: outcome(OpState::Error, Some(code), Some(detail), Map::new()),
                denied: true,
                retryable: false,
                reason: None,
            };
            return self.record(tx, &job, call, &row, None, ran, &cd).await;
        }

        // ---- step 8: route health. An instance the health model marks positively unhealthy is not called: the call
        // answers `not_offered` (the unrouted-operation outcome), with the reason `route_unhealthy` on the call row,
        // the audit row and the counter. Unknown or unreadable health routes (route_health module docs).
        if let ResolvedTarget::Instance { instance_id } = &target
            && let Health::Unhealthy(status) = self.instance_health(instance_id).await
        {
            let (tx, _) = self.begin().await?;
            let mut out = self.blocked(&op_key, status);
            out.instance_id = Some(instance_id.clone());
            let ran = Ran {
                out,
                denied: false,
                retryable: false,
                reason: Some(route_health::REASON),
            };
            return self.record(tx, &job, call, &row, None, ran, &cd).await;
        }

        // ---- steps 7b (T6a) + 8b: the per-case cap, then a write's pending row (one transaction) -------------------
        let mut pending: Option<String> = None;
        if writes {
            let (mut tx, _) = self.begin().await?;
            if matches!(effect, SideEffect::X | SideEffect::P) && row.case_id.is_some() {
                // the owner guard before `cap::reached` locks the case row (SW T5): the record below may queue a
                // handover item on this transaction
                ownership::guard(&mut tx, &call.conversation_id).await?;
                let own = cap::CallKey {
                    call_id: &row.call_id,
                    idempotency_key: row.idempotency_key.as_deref(),
                };
                let limit = cap::limit(call.max_per_case);
                if cap::reached(&mut tx, &call.case_id, &op_key, &own, limit).await? {
                    let ran = Ran {
                        out: outcome(
                            OpState::Error,
                            Some(cap::CODE),
                            Some(cap::trigger(&op_key)),
                            Map::new(),
                        ),
                        denied: true,
                        retryable: false,
                        reason: None,
                    };
                    return self.record(tx, &job, call, &row, None, ran, &cd).await;
                }
            }
            let p = ops::insert_pending(&mut tx, job.id, &row).await?;
            tx.commit().await?;
            match p {
                Pending::Run { call_id } | Pending::Retry { call_id } => pending = Some(call_id),
                Pending::Stored {
                    state,
                    code,
                    output,
                    route,
                    instance_id,
                    ..
                } => {
                    tracing::info!(call_id = %call.call_id, op = %op_key, state = %state,
                        "duplicate idempotency key: the stored outcome answers");
                    return Ok(OpResult {
                        call_id: call.call_id.clone(),
                        case_id: call.case_id.clone(),
                        phase: call.phase,
                        step: call.step.clone(),
                        attempt: call.attempt,
                        operation: call.operation.clone(),
                        state: result_state(&state),
                        outcome_code: code,
                        output: if state == "ok" { output } else { None },
                        retryable: false,
                        route_kind: Some(route),
                        latency_ms: Some(0),
                        instance_id,
                    });
                }
                Pending::InFlight { call_id } => {
                    // the earlier attempt may have written: never run it again (step 10)
                    let (tx, _) = self.begin().await?;
                    let ran = Ran {
                        out: outcome(
                            OpState::UnknownOutcome,
                            Some("unknown_outcome"),
                            Some("An earlier attempt of this write did not finish.".into()),
                            Map::new(),
                        ),
                        denied: false,
                        retryable: false,
                        reason: None,
                    };
                    let id = call_id.unwrap_or_else(|| call.call_id.clone());
                    return self.record(tx, &job, call, &row, Some(id), ran, &cd).await;
                }
            }
        }

        // ---- steps 9 + 10: execute (and verify) -----------------------------------------------------------------
        let started = Instant::now();
        let mut out = self
            .execute(&req, &target, stub, &job, &design, call, &cd)
            .await;
        if matches!(effect, SideEffect::X | SideEffect::P)
            && out.state == OpState::Error
            && matches!(
                out.outcome_code.as_deref(),
                Some("timeout" | "upstream_unavailable")
            )
        {
            out = self.verify(&req, def.as_ref(), &job, &design).await;
        }
        // ---- step 10b (T6a): postconditions of an ok answer (the call stays ok; never retried) ---------------------
        if out.state == OpState::Ok && !call.postconditions.is_empty() {
            let mut facts = call.facts.clone();
            facts.identity_level = call.identity_level;
            facts.steps.insert(
                call.step.clone(),
                StepFact::answered(&Value::Object(out.output.clone())),
            );
            let failed = conditions::failed(&call.postconditions, &facts);
            if !failed.is_empty() {
                out.outcome_code = Some(POSTCONDITION_FAILED.to_owned());
                out.detail = Some(failed.join(", "));
            }
        }
        out.latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        // step 10c (A2): state-mapped output fields leave the answer here, before the row, the event or the result
        self.store_outputs(&design, call, &job, &op_key, &mut out)
            .await;
        let retryable = out.state == OpState::Error
            && transient(out.outcome_code.as_deref())
            && (!matches!(effect, SideEffect::X | SideEffect::P)
                || out.outcome_code.as_deref() == Some("rate_limited"));
        let ran = Ran {
            out,
            denied: false,
            retryable,
            reason: None,
        };
        let (tx, _) = self.begin().await?;
        self.record(tx, &job, call, &row, pending, ran, &cd).await
    }

    /// Step 8: the health of routed instance `id`, from the cache or one primary-key read. Never a network probe;
    /// an unreadable health is [`Health::Unknown`] (fail open).
    async fn instance_health(&self, id: &str) -> Health {
        let tenant = self.scope.tenant;
        if let Some(h) = self.health.cached(tenant, id, Instant::now()) {
            return h;
        }
        match self.begin().await {
            Ok((mut tx, _)) => self.health.read(&mut tx, tenant, id).await,
            Err(e) => {
                tracing::warn!(
                    instance = id,
                    "route health: no transaction, routing anyway: {e:#}"
                );
                Health::Unknown
            }
        }
    }

    /// The `not_offered` answer of a call whose routed instance is positively unhealthy (`status`), counted.
    fn blocked(&self, operation: &str, status: &'static str) -> OpOutcome {
        metrics::counter!(route_health::METRIC, "status" => status).increment(1);
        tracing::warn!(
            op = operation,
            status,
            "route_unhealthy: the call was not made"
        );
        failed("not_offered", route_health::detail(operation, status))
    }

    /// Step 9 with the timeout (stub or live).
    #[allow(clippy::too_many_arguments)]
    async fn execute(
        &self,
        req: &OpRequest,
        target: &ResolvedTarget,
        stub: bool,
        job: &JobRef,
        design: &RuntimeDesign,
        call: &OpCall,
        cd: &CaseDesign,
    ) -> OpOutcome {
        let limit = if stub {
            self.stub_timeout
        } else {
            self.live_timeout
        };
        tokio::time::timeout(limit, self.dispatch(req, target, job, design, call, cd))
            .await
            .unwrap_or_else(|_| {
                failed(
                    "timeout",
                    format!("{} did not answer in time.", req.operation),
                )
            })
    }

    /// A registered executor, else the instance executor (with the per-instance semaphore). `not_handled` from the
    /// executor is an error here: the target was an instance.
    async fn run_executor(&self, req: &OpRequest, target: &ResolvedTarget) -> Option<OpOutcome> {
        // step 8 for every path that reaches an instance, including the runner's own system pre-reads
        if let ResolvedTarget::Instance { instance_id } = target
            && let Health::Unhealthy(status) = self.instance_health(instance_id).await
        {
            return Some(self.blocked(&req.operation, status));
        }
        if let Some(x) = self.executors.registered(&req.operation) {
            return Some(x.execute(req, target).await);
        }
        let ResolvedTarget::Instance { instance_id } = target else {
            return None;
        };
        let Some(x) = self.executors.instance() else {
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

    /// Step 9: the executor of the target; B4 answers what B7 does not handle.
    async fn dispatch(
        &self,
        req: &OpRequest,
        target: &ResolvedTarget,
        job: &JobRef,
        design: &RuntimeDesign,
        call: &OpCall,
        cd: &CaseDesign,
    ) -> OpOutcome {
        if let Some(o) = self.run_executor(req, target).await {
            return o;
        }
        match target {
            // run_executor answered every instance target
            ResolvedTarget::Instance { .. } => {
                failed("connector_misconfigured", "No executor ran the call.")
            }
            ResolvedTarget::Builtin { detail } => match detail.as_deref() {
                Some("knowledge") => self.knowledge(req, job, design, call).await,
                Some("rule") => builtin::rule(design, variant_of(&req.operation), &req.inputs),
                _ => failed(
                    "not_offered",
                    format!("{} is not a built-in operation.", req.operation),
                ),
            },
            ResolvedTarget::AgentCases => {
                let variant = variant_of(&req.operation);
                if !matches!(variant, "handover" | "complaint") {
                    return failed(
                        "not_offered",
                        format!("agent_cases does not run {}.", req.operation),
                    );
                }
                self.agent_case(job, call, variant, &req.inputs, cd, design)
                    .await
            }
            ResolvedTarget::DeepLink { url } => {
                let mut out = Map::new();
                out.insert("deep_link".into(), Value::String(url.clone()));
                outcome(OpState::Ok, None, None, out)
            }
            ResolvedTarget::NotOffered => failed(
                "not_offered",
                format!("{} is not offered on this job.", req.operation),
            ),
        }
    }

    /// `knowledge.*` (KH T7): the conversation's channel and, when the step names no question, the customer's words
    /// (the inbound message the request was qualified from, the step's `message_id`), read in one short
    /// transaction; then [`builtin::knowledge`]. Its synthesis call is metered (`usage_event`, label
    /// [`builtin::USAGE_LABEL`]) in its own transaction.
    async fn knowledge(
        &self,
        req: &OpRequest,
        job: &JobRef,
        design: &RuntimeDesign,
        call: &OpCall,
    ) -> OpOutcome {
        let message = req
            .inputs
            .get("message_id")
            .and_then(Value::as_str)
            .and_then(|m| uuid::Uuid::parse_str(m).ok());
        let read = async {
            let (mut tx, _) = self.begin().await?;
            let conv =
                crate::conversations::store::conversation(&mut tx, &call.conversation_id).await?;
            let text: Option<String> = match message {
                Some(id) => {
                    sqlx::query_scalar(
                        "SELECT text FROM conversation_message \
                          WHERE message_id = $1 AND conversation_id = $2 AND role IN ('customer', 'caller')",
                    )
                    .bind(id)
                    .bind(&call.conversation_id)
                    .fetch_optional(&mut *tx)
                    .await?
                }
                None => None,
            };
            tx.commit().await?;
            anyhow::Ok((conv, text))
        }
        .await;
        // an unreadable message is no question: the step answers not grounded and the case hands over
        let (conv, text) = match read {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(case_id = %call.case_id, error = %e, "knowledge_context_unreadable");
                (None, None)
            }
        };
        let asked = builtin::Asked {
            operation: &req.operation,
            channel: conv.as_ref().map(|c| c.channel.as_str()),
            question: text.as_deref(),
        };
        let run = builtin::knowledge(
            &self.st,
            self.scope.tenant,
            job,
            design,
            &asked,
            &req.inputs,
        )
        .await;
        if let Some(usage) = &run.usage {
            let (source, actor) = crate::runtime::assistant::usage_party(conv.as_ref());
            let emitted = async {
                let (mut tx, _) = self.begin().await?;
                crate::usage::emit(
                    &mut tx,
                    &self.st.cfg.metering,
                    &crate::usage::UsageEvent {
                        job_id: Some(job.id),
                        source,
                        use_: builtin::USAGE_LABEL,
                        activity: call.activity.as_deref(),
                        conversation_id: Some(&call.conversation_id),
                        case_id: Some(&call.case_id),
                        actor: &actor,
                        usage,
                    },
                )
                .await?;
                tx.commit().await?;
                anyhow::Ok(())
            }
            .await;
            if let Err(e) = emitted {
                // the answer stands; the ledger row is the money path's to replay
                tracing::error!(case_id = %call.case_id, error = %e, "knowledge_answer_usage_failed");
            }
        }
        run.outcome
    }

    /// `case.create{handover|complaint}` in its own transaction.
    async fn agent_case(
        &self,
        job: &JobRef,
        call: &OpCall,
        variant: &str,
        input: &Map<String, Value>,
        cd: &CaseDesign,
        design: &RuntimeDesign,
    ) -> OpOutcome {
        // what the case collected (B4 T6a): its slots, the slots the system resolved and the verified identity; a
        // failed read leaves it empty (the item is still queued)
        let cases = CaseStore::new(self.bus.clone());
        let collected = async {
            let slots = cases
                .request(&call.case_id)
                .await?
                .map(|r| r.slots)
                .unwrap_or_default();
            let resolved = cases.resolved_slots(&call.case_id).await?;
            let level = cases.identity(&call.case_id).await?.map_or(0, |(l, _)| l);
            anyhow::Ok(agent_cases::collected_of(&slots, &resolved, level, None))
        }
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(case_id = %call.case_id, "handover collected read failed: {e:#}");
            Vec::new()
        });
        // the item's `proposal_hint` (B4 T6a): the case's activity (hidden in the design?) and its knowledge results;
        // a failed read leaves the hint out
        let hint = async {
            let req = cases.request(&call.case_id).await?;
            let activity = req
                .as_ref()
                .and_then(|r| r.activity.clone())
                .or_else(|| call.activity.clone())
                .or_else(|| {
                    req.as_ref()
                        .and_then(|r| design.spaces.by_key(&r.context_space))
                        .map(|s| s.activity.clone())
                })
                .filter(|a| !a.trim().is_empty());
            let activity_hidden = match &activity {
                Some(a) => {
                    let (mut tx, _) = self.begin().await?;
                    let rows = crate::design::read::values_of_kind(
                        &mut tx,
                        &self.st.design,
                        job,
                        &design.of,
                        "overlay",
                    )
                    .await?;
                    tx.commit().await?;
                    agent_cases::activity_hidden_in(&rows, a)
                }
                None => false,
            };
            let knowledge_empty =
                agent_cases::knowledge_empty_in(&cases.results(&call.case_id).await?);
            anyhow::Ok(agent_cases::proposal_hint_of(
                &job.key,
                &agent_cases::HintFacts {
                    activity,
                    activity_hidden,
                    knowledge_empty,
                },
            ))
        }
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(case_id = %call.case_id, "handover proposal_hint read failed: {e:#}");
            None
        });
        let run = async {
            let (mut tx, _) = self.begin().await?;
            let o = agent_cases::create_collected(
                &mut tx,
                job,
                &call.case_id,
                &call.conversation_id,
                variant,
                input,
                cd,
                &collected,
                hint,
            )
            .await?;
            tx.commit().await?;
            anyhow::Ok(o)
        };
        match run.await {
            Ok(o) => {
                // `handover_id` is only in the output of an item this call created (`Queued::Created`)
                if o.state == OpState::Ok
                    && let Some(item) = o.output.get("handover_id").and_then(Value::as_str)
                {
                    self.notify_workers(job, item).await;
                }
                o
            }
            Err(e) => {
                let code = e
                    .downcast_ref::<crate::error::ApiError>()
                    .map_or("internal", |a| a.code());
                tracing::warn!(case_id = %call.case_id, call_id = %call.call_id, code, "agent_cases failed");
                failed(code, "The case could not be queued.")
            }
        }
    }

    /// After commit: tell the job's enabled worker bindings (helpdesk ticket, staff channels) about item `item` this
    /// runner queued, as every other handover path does ([`handover::notify_workers`], on the runner's own
    /// `agent_runtime` state; no `EDGE_INTERNAL_KEY` = nothing sent). Never fails the call: a failure is logged and
    /// the item stays queued.
    async fn notify_workers(&self, job: &JobRef, item: &str) {
        handover::notify_workers(&self.st, self.scope.tenant, job.id, item).await;
    }

    /// The route of `op_key` in a short transaction.
    async fn target_of(&self, job: &JobRef, op_key: &str) -> Result<ResolvedTarget> {
        let (mut tx, _) = self.begin().await?;
        let q = RouteQuery {
            tenant: self.scope.tenant,
            job_id: job.id,
            job_key: job.key.clone(),
            env: self.scope.env,
            op: op_key.to_owned(),
        };
        let t = self.routes.resolve(&mut tx, &q).await?;
        tx.commit().await?;
        Ok(t)
    }

    /// Step 10: the X/P call's verify read (the catalogue's `verify_read`). It answering confirms the write (`ok`,
    /// `verified`); no verify read or no answer is `unknown_outcome`.
    async fn verify(
        &self,
        req: &OpRequest,
        def: Option<&OpDef>,
        job: &JobRef,
        design: &RuntimeDesign,
    ) -> OpOutcome {
        let unknown = |detail: &str| {
            outcome(
                OpState::UnknownOutcome,
                Some("unknown_outcome"),
                Some(detail.to_owned()),
                Map::new(),
            )
        };
        let Some(vkey) = def.and_then(|d| d.verify_read.clone()) else {
            return unknown("The write did not answer and declares no verify read.");
        };
        let target = match self.target_of(job, &vkey).await {
            Ok(t) => t,
            Err(_) => return unknown("The write did not answer; its verify read has no route."),
        };
        let vreq = OpRequest {
            operation: vkey.clone(),
            outputs: design.catalog.outputs(&vkey),
            idempotency_key: String::new(),
            ..req.clone()
        };
        let read = tokio::time::timeout(self.live_timeout, self.run_executor(&vreq, &target)).await;
        match read {
            Ok(Some(o)) if o.state == OpState::Ok => {
                let mut ok = o;
                ok.verified = Some(true);
                ok
            }
            _ => unknown("The write did not answer and its verify read failed."),
        }
    }

    /// Step 11: the row, the case event (+ the step 10 alert and handover, a verified credential's level) and the
    /// audit row in one transaction; then the `op.result`.
    #[allow(clippy::too_many_arguments)]
    async fn record(
        &self,
        mut tx: Transaction<'static, Postgres>,
        job: &JobRef,
        call: &OpCall,
        row: &CallRow,
        pending: Option<String>,
        ran: Ran,
        cd: &CaseDesign,
    ) -> Result<OpResult> {
        // SW T5: the conversation's control lock and owner row first, before the case row (`case_effects` updates
        // it, then may queue a handover item): the ownership commands' lock order, so a Take creating an item for
        // this case cannot deadlock with this transaction. Every caller's transaction holds no row lock yet, except
        // the cap check's, which took this guard before its case-row lock.
        if row.case_id.is_some() {
            ownership::guard(&mut tx, &call.conversation_id).await?;
        }
        let reason = ran.reason;
        let out = ran.out;
        let state = ops::state_text(out.state, ran.denied);
        let ok = state == "ok";
        let f = Final {
            state,
            code: out.outcome_code.as_deref(),
            latency_ms: i64::try_from(out.latency_ms).unwrap_or(i64::MAX),
            detail: capped(out.detail.clone()),
            object_change: (ok && row.idempotency_key.is_some())
                .then(|| Value::Object(out.output.clone())),
            instance_id: out.instance_id.as_deref(),
            template: out.template.as_deref(),
        };
        let fresh = match &pending {
            Some(id) => ops::finish_pending(&mut tx, job.id, id, &f).await?,
            None => ops::insert_finished(&mut tx, job.id, row, &f).await?,
        };
        let mut created = None;
        if fresh {
            if row.case_id.is_some() {
                created = case_effects(&mut tx, job, call, row, &f, &out, cd).await?;
            }
            let status = match state {
                "ok" => Status::Success,
                "denied" => Status::Denied,
                "unknown_outcome" => Status::Failure,
                _ => Status::Error,
            };
            let event = Event::new("execute", "operation_call", row.call_id.clone())
                .job(&job.key)
                .status(status)
                .detail("operation", row.operation.clone())
                .detail("route", row.route)
                .detail("env", row.env)
                .detail("state", state)
                .detail("code", f.code.map(str::to_owned))
                .detail("case_id", row.case_id.clone())
                .detail("reason", reason.map(str::to_owned));
            // B4 known customer: the customer-context reads are audited with phase `context`
            let event = if call.provenance.as_deref() == Some(domain::PROVENANCE_CONTEXT) {
                event.detail("phase", domain::PROVENANCE_CONTEXT)
            } else {
                event
            };
            audit::record(&mut tx, &Actor::agent(&call.agent_role), event).await?;
        }
        tx.commit().await?;
        if let Some(item) = &created {
            self.notify_workers(job, item).await;
        }
        // B4 known customer: a write changed what the customer's records say, so the conversation's cached customer
        // context is stale (the next case reads it again)
        if ok && row.idempotency_key.is_some() && row.case_id.is_some() {
            customer_context::invalidate(
                &self.bus,
                self.scope.tenant,
                &job.key,
                &call.conversation_id,
            )
            .await;
        }
        if state != "ok" {
            tracing::info!(call_id = %row.call_id, op = %row.operation, state, code = f.code.unwrap_or(""),
                "operation call did not succeed");
        }
        Ok(OpResult {
            call_id: call.call_id.clone(),
            case_id: call.case_id.clone(),
            phase: call.phase,
            step: call.step.clone(),
            attempt: call.attempt,
            operation: call.operation.clone(),
            state: result_state(state),
            outcome_code: out.outcome_code.clone(),
            // the pause an `input_required` answer carries is the one non-ok output (a key and a question)
            output: (ok || out.outcome_code.as_deref() == Some(declared::INPUT_REQUIRED))
                .then(|| Value::Object(out.output.clone())),
            retryable: ran.retryable,
            route_kind: Some(row.route.to_owned()),
            latency_ms: Some(out.latency_ms),
            instance_id: out.instance_id.clone(),
        })
    }
}

/// The operation that serves `req` on `target` ([`OpExecutor::served`]): `None` when it is served as asked (no variant,
/// not an instance target, a registered executor) or there is no instance executor (step 6b refuses that).
pub async fn served_of(
    executors: &Executors,
    req: &OpRequest,
    target: &ResolvedTarget,
) -> Result<Option<ops::Served>, String> {
    if !req.operation.contains('{')
        || !matches!(target, ResolvedTarget::Instance { .. })
        || executors.registered(&req.operation).is_some()
    {
        return Ok(None);
    }
    match executors.instance() {
        Some(x) => x.served(req, target).await,
        None => Ok(None),
    }
}

/// Is the call a connector `case.create` — the operation asked for OR the one that serves it (a variant the template
/// lacks is served by the op's `*`, [`served_of`])? Fail closed: a serving operation that cannot be read counts as a
/// `case.create` (step 6b then refuses the call anyway). Shared with the staff probe.
pub async fn serves_case_create(
    executors: &Executors,
    req: &OpRequest,
    target: &ResolvedTarget,
) -> bool {
    if ops::is_case_create(&req.operation) {
        return true;
    }
    match served_of(executors, req, target).await {
        Ok(Some(s)) => ops::is_case_create(&s.op_key),
        Ok(None) => false,
        Err(_) => true,
    }
}

/// Step 6b (audit of agent#64): a variant the routed system lacks is served by the operation's `*` (B7: the route
/// `route_for`, the template `find_op`), which may need a higher identity level or inputs the variant does not
/// carry — `delivery.read{by_tracking_ref}` (level 1, `tracking_ref`) served by an order-keyed `delivery.read`
/// (level 2, `order_id`). Refused before any upstream call: `not_offered` when the served operation's inputs are
/// missing (the system cannot answer the variant as asked), else the served operation's identity refusal
/// (`identity_required`, the customer is asked to verify) — never the served operation's data at the variant's
/// level. Registered executors run the key as asked; a call with no variant is served as asked. The runner and the
/// staff probe (crate::turn::probe) both call it before their instance executor.
pub async fn served_refusal(
    executors: &Executors,
    req: &OpRequest,
    target: &ResolvedTarget,
    design: &RuntimeDesign,
    state_keys: &std::collections::BTreeSet<String>,
) -> Option<(&'static str, String)> {
    if !req.operation.contains('{')
        || !matches!(target, ResolvedTarget::Instance { .. })
        || executors.registered(&req.operation).is_some()
    {
        return None;
    }
    // fail closed: a variant on an instance whose serving operation cannot be read is never run at the variant's level
    let Some(executor) = executors.instance() else {
        return Some((
            "not_offered",
            format!("{} has no instance executor to serve it.", req.operation),
        ));
    };
    let served = match executor.served(req, target).await {
        Ok(Some(s)) => s,
        Ok(None) => return None,
        Err(why) => {
            tracing::warn!(operation = %req.operation, %why, "served_unreadable");
            return Some(("not_offered", format!("{why}.")));
        }
    };
    if served.op_key == req.operation {
        return None;
    }
    let given = |k: &str| match req.inputs.get(k) {
        None | Some(Value::Null) => false,
        Some(Value::String(v)) => !v.trim().is_empty(),
        Some(_) => true,
    };
    if let Some(missing) = served
        .needs
        .iter()
        .find(|any| !any.iter().any(|k| given(k)))
    {
        return Some((
            "not_offered",
            format!(
                "{} is served by {}, which needs {} the call does not carry.",
                req.operation,
                served.op_key,
                missing.join(" or ")
            ),
        ));
    }
    // fail closed: a serving operation the job's catalogue lacks has no level to check, so it is not offered
    let Some(served_def) = def_of(design, &served.op_key) else {
        return Some((
            "not_offered",
            format!(
                "{} is served by {}, which the catalogue does not offer.",
                req.operation, served.op_key
            ),
        ));
    };
    // the serving operation's own required state (A2 step 3 rule: its policy's `requires`, else `subject` at a
    // catalogue level ≥ 1), never the variant's
    let required = declared::required_keys(
        design.policy(&served.op_key),
        authorise::required_level(served_def.identity_level).is_none_or(|n| n >= 1),
    );
    let missing = declared::missing_keys(&required, &|k| state_keys.contains(k));
    (!missing.is_empty()).then(|| {
        (
            declared::STATE_REQUIRED,
            format!(
                "{} is served by {}, which requires state the conversation lacks: {}.",
                req.operation,
                served.op_key,
                missing.join(", ")
            ),
        )
    })
}

/// The case side of a finished call: the `operation` event, `op_calls`, and for `unknown_outcome` the `alert` (column +
/// event) and the handover (reason `unknown_outcome`). Returns the id of a handover item it created (the caller
/// notifies the workers after its commit).
#[allow(clippy::too_many_arguments)]
async fn case_effects(
    c: &mut PgConnection,
    job: &JobRef,
    call: &OpCall,
    row: &CallRow,
    f: &Final<'_>,
    out: &OpOutcome,
    cd: &CaseDesign,
) -> Result<Option<String>> {
    let mut created = None;
    event(c, job, &call.case_id, "operation", ops::event_body(row, f)).await?;
    sqlx::query(
        "UPDATE agent_case SET op_calls = op_calls + 1, updated_at = now() WHERE case_id = $1",
    )
    .bind(call.case_id.clone())
    .execute(&mut *c)
    .await?;
    // F4 (0098): the vendor ticket an instance case.create opened; the first ticket of the case stays
    if f.state == "ok"
        && row.route == "instance"
        && ops::is_case_create(&row.operation)
        && let Some(r) = ops::external_ref(&out.output)
    {
        sqlx::query(
            "UPDATE agent_case SET external_ref = $2, updated_at = now() \
              WHERE case_id = $1 AND external_ref IS NULL",
        )
        .bind(call.case_id.clone())
        .bind(r)
        .execute(&mut *c)
        .await?;
    }
    if f.state == "unknown_outcome" {
        sqlx::query("UPDATE agent_case SET alert = 'unknown_outcome', updated_at = now() WHERE case_id = $1")
            .bind(call.case_id.clone())
            .execute(&mut *c)
            .await?;
        event(
            c,
            job,
            &call.case_id,
            "alert",
            json!({"alert": "unknown_outcome", "call_id": row.call_id, "operation": row.operation}),
        )
        .await?;
        let q = agent_cases::queue(
            c,
            job,
            &call.case_id,
            &call.conversation_id,
            "unknown_outcome",
            &row.operation,
            &row.operation,
            cd,
        )
        .await?;
        match q {
            Queued::Created(item) => created = Some(item),
            Queued::Already => {}
            Queued::NoConversation => {
                tracing::warn!(case_id = %call.case_id, call_id = %row.call_id,
                    "unknown_outcome: the case has no conversation to hand over");
            }
        }
    }
    // T6a: a false postcondition alerts the case; it and the per-case cap hand it over (`tool_failed`)
    if f.state == "ok" && f.code == Some(POSTCONDITION_FAILED) {
        sqlx::query(
            "UPDATE agent_case SET alert = 'postcondition_failed', updated_at = now() WHERE case_id = $1",
        )
        .bind(call.case_id.clone())
        .execute(&mut *c)
        .await?;
        event(
            c,
            job,
            &call.case_id,
            "alert",
            json!({"alert": POSTCONDITION_FAILED, "call_id": row.call_id, "operation": row.operation}),
        )
        .await?;
        let trigger = format!(
            "postcondition failed: {}",
            f.detail.as_deref().unwrap_or_default()
        );
        created =
            created.or(tool_failed(c, job, call, row, POSTCONDITION_FAILED, &trigger, cd).await?);
    }
    if f.state == "denied" && f.code == Some(cap::CODE) {
        // the plan's `operation.denied` audit row, besides the call's `execute` row
        audit::record(
            &mut *c,
            &Actor::agent(&call.agent_role),
            Event::new("operation.denied", "operation_call", row.call_id.clone())
                .job(&job.key)
                .status(Status::Denied)
                .detail("operation", row.operation.clone())
                .detail("code", cap::CODE)
                .detail("case_id", call.case_id.clone()),
        )
        .await?;
        created = created.or(tool_failed(
            c,
            job,
            call,
            row,
            cap::CODE,
            &cap::trigger(&row.operation),
            cd,
        )
        .await?);
    }
    if f.state == "denied" && f.code == Some(licence::CODE) {
        // step 2: the plan's `operation.denied` audit row, then the case hands over without any connector call
        audit::record(
            &mut *c,
            &Actor::agent(&call.agent_role),
            Event::new("operation.denied", "operation_call", row.call_id.clone())
                .job(&job.key)
                .status(Status::Denied)
                .detail("operation", row.operation.clone())
                .detail("code", licence::CODE)
                .detail("case_id", call.case_id.clone()),
        )
        .await?;
        created = created.or(tool_failed(
            c,
            job,
            call,
            row,
            licence::CODE,
            &licence::trigger(&row.operation),
            cd,
        )
        .await?);
    }
    Ok(created)
}

/// B4 known customer: has case `case_id` already made [`customer_context::MAX_CONTEXT_CALLS`] context calls that ran
/// (a refused one costs nothing)? `call_id` is the call asking.
async fn context_cap_reached(
    c: &mut PgConnection,
    case_id: &str,
    call_id: &str,
) -> crate::error::ApiResult<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM operation_call \
          WHERE case_id = $1 AND step->>'phase' = 'context' AND call_id <> $2 \
            AND state IN ('ok', 'pending', 'unknown_outcome', 'error')",
    )
    .bind(case_id.to_owned())
    .bind(call_id.to_owned())
    .fetch_one(&mut *c)
    .await?;
    Ok(n >= customer_context::MAX_CONTEXT_CALLS)
}

/// T6a: hand the case over with reason `tool_failed` (`trigger`: condition ids or the capped operation, never case
/// data), its `outcome` set to `code`, plus a `state` event. The id of the item when this call created it.
async fn tool_failed(
    c: &mut PgConnection,
    job: &JobRef,
    call: &OpCall,
    row: &CallRow,
    code: &str,
    trigger: &str,
    cd: &CaseDesign,
) -> Result<Option<String>> {
    let q = agent_cases::queue(
        c,
        job,
        &call.case_id,
        &call.conversation_id,
        "tool_failed",
        trigger,
        &row.operation,
        cd,
    )
    .await?;
    if q == Queued::NoConversation {
        tracing::warn!(case_id = %call.case_id, call_id = %row.call_id, code,
            "the case has no conversation to hand over");
    }
    let created = match q {
        Queued::Created(item) => Some(item),
        _ => None,
    };
    sqlx::query("UPDATE agent_case SET outcome = $2, updated_at = now() WHERE case_id = $1")
        .bind(call.case_id.clone())
        .bind(code.to_owned())
        .execute(&mut *c)
        .await?;
    event(
        c,
        job,
        &call.case_id,
        "state",
        json!({"status": "handover", "code": code, "event": trigger, "call_id": row.call_id}),
    )
    .await?;
    Ok(created)
}

/// A case event (ids and codes only, K50).
async fn event(
    c: &mut PgConnection,
    job: &JobRef,
    case_id: &str,
    kind: &str,
    body: Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO case_event (tenant_id, job_id, case_id, kind, body) \
         VALUES (app_current_tenant(), $1, $2, $3, $4)",
    )
    .bind(job.id)
    .bind(case_id.to_owned())
    .bind(kind.to_owned())
    .bind(body)
    .execute(&mut *c)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_keys_topics_and_variants() {
        let mut call: OpCall = serde_json::from_value(json!({
            "call_id": "opc_00000000-0000-0000-0000-000000000001", "case_id": "CS-1", "conversation_id": "c",
            "phase": 0, "step": "rules", "attempt": 1, "agent_role": "procedure", "operation": "rule.evaluate",
            "variant": "RET-01", "env": "test", "initiator": "procedure@t", "deadline_ms": 0
        }))
        .unwrap();
        assert_eq!(op_key_of(&call), "rule.evaluate{RET-01}");
        call.variant = Some("*".into());
        assert_eq!(op_key_of(&call), "rule.evaluate");
        assert_eq!(variant_of("case.create{handover}"), "handover");
        assert_eq!(variant_of("order.read"), "");
        assert_eq!(
            result_topic("order-support.returns.op.call"),
            "order-support.returns.op.result"
        );
        assert_eq!(result_topic("op.call"), "op.result");
    }

    #[test]
    fn only_calls_that_changed_nothing_are_retryable() {
        assert!(transient(Some("timeout")));
        assert!(transient(Some("rate_limited")));
        assert!(!transient(Some("not_found")));
        assert!(!transient(None));
    }
}

#[cfg(test)]
mod reclaim_tests {
    use super::*;

    #[test]
    fn reclaim_idle_outlasts_the_call_and_its_verify_read() {
        let o = consume_options();
        assert!(
            o.reclaim_idle >= LIVE_TIMEOUT * 2 + super::super::bus::RECLAIM_SLACK,
            "{:?} vs 2 x {LIVE_TIMEOUT:?}: a peer would take a live call over",
            o.reclaim_idle
        );
        assert!(o.reclaim_idle > LIVE_TIMEOUT.max(STUB_TIMEOUT));
    }
}
