//! The runner's operation seam (B4 T6, §0 R6): [`OpExecutor`] (B7's [`ConnectorExecutor`] is one), the executors a
//! runner holds, the route resolver over B7's `operation_route` rows ([`EffectiveRoutes`], R10) and the
//! `operation_call` rows (0062).
//!
//! The call types are B7's `crate::connectors::runtime::{OpRequest, ResolvedTarget, OpOutcome, OpState}`: one shape
//! from the resolver to the executor and back. `OpState::NotHandled` (a non-instance target) is B4's: built-ins,
//! `agent_cases`, deep links, `not_offered` (crate::runtime::runner).
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::connectors::instances;
use crate::connectors::model::{ConnectorTemplate, InputType};
use crate::connectors::opkey::{parse_op_key, same_op};
use crate::connectors::resolve::{OperationRoute, RouteTarget};
use crate::connectors::routes_store;
pub use crate::connectors::runtime::{ConnectorExecutor, OpOutcome, OpRequest, OpState, Served};
use crate::design::Registry;
use crate::error::ApiResult;
use crate::runtime::routes::{
    DefaultRoutes, ResolvedTarget, RouteQuery, RouteResolver, builtin_family,
};
use crate::runtime::scope::Env;

/// Runs one operation call on a resolved target. Answers [`OpState::NotHandled`] for a target it does not run.
pub trait OpExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        req: &'a OpRequest,
        target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome>;

    /// The operation that would serve `req` on `target`, before the call: B7's connector reads the instance's
    /// template, which serves a variant it lacks with the op's `*` ([`ConnectorExecutor::served`]). The runner
    /// authorises the served operation too (audit of agent#64). `Ok(None)` = served as asked; `Err` = the serving
    /// operation cannot be read, and step 6b refuses the call (fail closed, never authorised at the variant's level).
    fn served<'a>(
        &'a self,
        _req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, Result<Option<Served>, String>> {
        Box::pin(std::future::ready(Ok(None)))
    }
}

/// B7's connector executor (instance targets; everything else `not_handled`).
impl OpExecutor for ConnectorExecutor {
    fn execute<'a>(
        &'a self,
        req: &'a OpRequest,
        target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(ConnectorExecutor::execute(self, req, target))
    }

    fn served<'a>(
        &'a self,
        req: &'a OpRequest,
        target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, Result<Option<Served>, String>> {
        Box::pin(ConnectorExecutor::served(self, req, target))
    }
}

/// The executors of one runner: per-operation registrations (B7's, a test's) first, then the instance executor.
#[derive(Clone, Default)]
pub struct Executors {
    by_op: HashMap<String, Arc<dyn OpExecutor>>,
    instance: Option<Arc<dyn OpExecutor>>,
}

impl Executors {
    /// The instance executor (B7's [`ConnectorExecutor`] in the binary).
    pub fn with_instance(mut self, x: Arc<dyn OpExecutor>) -> Self {
        self.instance = Some(x);
        self
    }

    /// `x` runs operation `op_key` (`object.verb{variant}`) whatever its route says: a registration overrides the
    /// instance route (and B4's built-ins) for that operation.
    pub fn register(mut self, op_key: &str, x: Arc<dyn OpExecutor>) -> Self {
        self.by_op.insert(op_key.to_owned(), x);
        self
    }

    /// The executor registered for `op_key`, if any.
    pub fn registered(&self, op_key: &str) -> Option<&Arc<dyn OpExecutor>> {
        self.by_op.get(op_key)
    }

    pub fn instance(&self) -> Option<&Arc<dyn OpExecutor>> {
        self.instance.as_ref()
    }
}

/// An outcome the runner builds itself (built-ins, agent_cases, denials). Never connector data unless the caller
/// says so.
pub fn outcome(
    state: OpState,
    code: Option<&str>,
    detail: Option<String>,
    output: Map<String, Value>,
) -> OpOutcome {
    OpOutcome {
        state,
        outcome_code: code.map(str::to_owned),
        detail,
        output,
        untrusted: false,
        instance_id: None,
        template: None,
        verified: None,
        latency_ms: 0,
    }
}

/// `error` with `code`.
pub fn failed(code: &str, detail: impl Into<String>) -> OpOutcome {
    outcome(OpState::Error, Some(code), Some(detail.into()), Map::new())
}

/// How many calls of one instance run at once in this process (step 9).
pub const PER_INSTANCE: usize = 4;

/// The per-(tenant, instance) semaphores of this process.
type InstanceSems = HashMap<(Uuid, String), Arc<Semaphore>>;

/// A permit of the per-(tenant, instance) semaphore.
pub async fn instance_permit(tenant: Uuid, instance: &str) -> Option<OwnedSemaphorePermit> {
    static SEMS: OnceLock<Mutex<InstanceSems>> = OnceLock::new();
    let sem = {
        let mut m = SEMS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            m.entry((tenant, instance.to_owned()))
                .or_insert_with(|| Arc::new(Semaphore::new(PER_INSTANCE))),
        )
    };
    sem.acquire_owned().await.ok()
}

/// The route of `op_key` among `routes`: its own variant, else its `*` row.
pub fn route_for<'a>(routes: &'a [OperationRoute], op_key: &str) -> Option<&'a OperationRoute> {
    let want = parse_op_key(op_key);
    if want.object.is_empty() {
        return None;
    }
    let mut star = want.clone();
    star.variant = "*".to_owned();
    routes
        .iter()
        .find(|r| same_op(&r.op_ref(), &want))
        .or_else(|| routes.iter().find(|r| same_op(&r.op_ref(), &star)))
}

/// B7's stored route target as the runner's: a `builtin` row names its family from the operation.
pub fn target_of(t: &RouteTarget, op_key: &str) -> ResolvedTarget {
    // a built-in operation is served in process whatever its stored row says: set_route refuses an Instance
    // route for one, and a row written before that guard must not reach a connector either
    if let Some(family) = builtin_family(op_key)
        && matches!(t, RouteTarget::Instance { .. })
    {
        return ResolvedTarget::Builtin {
            detail: Some(family.to_owned()),
        };
    }
    match t {
        RouteTarget::Instance { instance_id, .. } => ResolvedTarget::Instance {
            instance_id: instance_id.clone(),
        },
        RouteTarget::Builtin => ResolvedTarget::Builtin {
            detail: builtin_family(op_key).map(str::to_owned),
        },
        RouteTarget::AgentCases => ResolvedTarget::AgentCases,
        RouteTarget::DeepLink { url } => ResolvedTarget::DeepLink { url: url.clone() },
        RouteTarget::NotOffered => ResolvedTarget::NotOffered,
    }
}

/// R10: the job's effective routes for the scope env (`connectors::routes_store::effective`: the working rows in
/// test, the release snapshot in prod) first, then [`DefaultRoutes`].
pub struct EffectiveRoutes {
    reg: Arc<Registry>,
    defaults: DefaultRoutes,
}

impl EffectiveRoutes {
    pub fn new(reg: Arc<Registry>) -> Self {
        Self {
            reg,
            defaults: DefaultRoutes::new(),
        }
    }

    async fn resolve_in(&self, c: &mut PgConnection, q: &RouteQuery) -> ApiResult<ResolvedTarget> {
        let of = match q.env {
            Env::Prod => routes_store::DesignOf::Live,
            Env::Test => routes_store::DesignOf::Draft,
        };
        let routes =
            routes_store::effective(&mut *c, &self.reg, q.job_id, q.env.as_str(), of).await?;
        if let Some(r) = route_for(&routes, &q.op) {
            return Ok(target_of(&r.target, &q.op));
        }
        Ok(self.defaults.default_target(q))
    }
}

impl RouteResolver for EffectiveRoutes {
    fn resolve<'a>(
        &'a self,
        c: &'a mut PgConnection,
        q: &'a RouteQuery,
    ) -> BoxFuture<'a, ApiResult<ResolvedTarget>> {
        Box::pin(self.resolve_in(c, q))
    }
}

/// The declared input types (0243) of each of `keys` the job routes to a system on `env`: those of the template op
/// that serves it on the routed instance (its own variant, else its `*` row, as the call is served). A key routed
/// anywhere else, or whose op declares none, is absent: its tool is offered by input names only.
pub async fn input_types(
    c: &mut PgConnection,
    reg: &Registry,
    job: Uuid,
    env: Env,
    keys: &[&str],
) -> ApiResult<BTreeMap<String, BTreeMap<String, InputType>>> {
    let of = match env {
        Env::Prod => routes_store::DesignOf::Live,
        Env::Test => routes_store::DesignOf::Draft,
    };
    let routes = routes_store::effective(&mut *c, reg, job, env.as_str(), of).await?;
    let mut templates: HashMap<String, Option<ConnectorTemplate>> = HashMap::new();
    let mut out = BTreeMap::new();
    for key in keys {
        let Some(r) = route_for(&routes, key) else {
            continue;
        };
        let ResolvedTarget::Instance { instance_id } = target_of(&r.target, key) else {
            continue;
        };
        if !templates.contains_key(&instance_id) {
            let tpl = match instances::one(&mut *c, &instance_id).await? {
                Some(inst) => {
                    instances::template_in_scope(
                        &mut *c,
                        &inst.template_scope,
                        &inst.template_key,
                        &inst.template_version,
                    )
                    .await?
                }
                None => None,
            };
            templates.insert(instance_id.clone(), tpl);
        }
        let types = templates
            .get(&instance_id)
            .and_then(Option::as_ref)
            .and_then(|t| crate::connectors::runtime::find_op(t, key))
            .map(|o| o.input_types.clone())
            .unwrap_or_default();
        if !types.is_empty() {
            out.insert((*key).to_owned(), types);
        }
    }
    Ok(out)
}

/// Is instance `id` a stub (3 s timeout) rather than a live system (20 s)? Unknown → live.
pub async fn is_stub(c: &mut PgConnection, id: &str) -> ApiResult<bool> {
    let stub: Option<bool> =
        sqlx::query_scalar("SELECT stub FROM connector_instance WHERE id = $1")
            .bind(id.to_owned())
            .fetch_optional(&mut *c)
            .await?;
    Ok(stub.unwrap_or(false))
}

// ---- operation_call rows (0062) -------------------------------------------------------------------------------

/// One `operation_call` row as the runner writes it.
#[derive(Clone, Debug)]
pub struct CallRow {
    pub call_id: String,
    /// `None` when the case row does not exist (the FK would refuse it).
    pub case_id: Option<String>,
    pub operation: String,
    /// `operation_call.route` (crate::runtime::routes::route_kind).
    pub route: &'static str,
    pub env: &'static str,
    pub initiator: String,
    pub attempt: i32,
    pub idempotency_key: Option<String>,
    pub confirm_ref: Option<String>,
    pub release_id: Option<String>,
    pub draft_rev: Option<i64>,
    pub step: Value,
}

/// The `operation_call.state` text of a final outcome (`not_handled` never reaches a row: the runner answers it).
pub fn state_text(s: OpState, denied: bool) -> &'static str {
    if denied {
        return "denied";
    }
    match s {
        OpState::Ok => "ok",
        OpState::UnknownOutcome => "unknown_outcome",
        OpState::Error | OpState::NotHandled => "error",
    }
}

/// What a duplicate idempotency key found.
#[derive(Clone, Debug, PartialEq)]
pub enum Pending {
    /// The row is new: run the call, then finish row `call_id`.
    Run { call_id: String },
    /// An earlier `error` call of the key was re-armed (a retry): run, then finish that row.
    Retry { call_id: String },
    /// The key already ended: answer its stored outcome, run nothing.
    Stored {
        state: String,
        code: Option<String>,
        detail: Option<String>,
        output: Option<Value>,
        /// Where the original call ran (`operation_call.route` / `instance_id`), not what this duplicate resolved.
        route: String,
        instance_id: Option<String>,
    },
    /// The key is still `pending` (an earlier attempt crashed mid-call): the write may have landed. Never re-run:
    /// the runner finishes row `call_id` (when it sees it) `unknown_outcome`.
    InFlight { call_id: Option<String> },
}

/// `(call_id, state, outcome_code, detail, object_change, route, instance_id)` of a stored call.
type StoredRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<Value>,
    String,
    Option<String>,
);

/// Step 8b: insert the `pending` row of a write (its own transaction, committed before the upstream call). A
/// duplicate key answers the stored outcome, re-arms an `error` row (a retry with the same key) or reports a call
/// still in flight.
pub async fn insert_pending(c: &mut PgConnection, job: Uuid, row: &CallRow) -> ApiResult<Pending> {
    let inserted: Option<String> = sqlx::query_scalar(
        "INSERT INTO operation_call (tenant_id, job_id, call_id, case_id, operation, route, env, state, initiator, \
                                     attempt, idempotency_key, confirm_ref, release_id, draft_rev, step) \
         VALUES (app_current_tenant(), $1, $2, $3, $4, $5, $6, 'pending', $7, $8, $9, $10, $11, $12, $13) \
         ON CONFLICT DO NOTHING RETURNING call_id",
    )
    .bind(job)
    .bind(&row.call_id)
    .bind(&row.case_id)
    .bind(&row.operation)
    .bind(row.route)
    .bind(row.env)
    .bind(&row.initiator)
    .bind(row.attempt)
    .bind(&row.idempotency_key)
    .bind(&row.confirm_ref)
    .bind(&row.release_id)
    .bind(row.draft_rev)
    .bind(&row.step)
    .fetch_optional(&mut *c)
    .await?;
    if let Some(call_id) = inserted {
        return Ok(Pending::Run { call_id });
    }
    let existing: Option<StoredRow> = sqlx::query_as(
        "SELECT call_id, state, outcome_code, detail, object_change, route, instance_id FROM operation_call \
              WHERE job_id = $1 AND (idempotency_key = $2 OR call_id = $3) \
              ORDER BY (idempotency_key = $2) DESC LIMIT 1",
    )
    .bind(job)
    .bind(&row.idempotency_key)
    .bind(&row.call_id)
    .fetch_optional(&mut *c)
    .await?;
    let Some((call_id, state, code, detail, output, route, instance_id)) = existing else {
        // the conflicting row is invisible (another job's key cannot collide: the key is per job)
        return Ok(Pending::InFlight { call_id: None });
    };
    let kind = state.clone();
    match kind.as_str() {
        "pending" => Ok(Pending::InFlight {
            call_id: Some(call_id),
        }),
        "error" => {
            // the re-armed row describes the retry, not the failed attempt: its route is the retry's and its
            // instance/template are cleared (finish_pending writes the retry's). 0181 grants UPDATE(route).
            let rearmed: Option<String> = sqlx::query_scalar(
                "UPDATE operation_call SET state = 'pending', finished_at = NULL, outcome_code = NULL, \
                        detail = NULL, latency_ms = NULL, instance_id = NULL, template = NULL, route = $3 \
                  WHERE job_id = $1 AND call_id = $2 AND state = 'error' RETURNING call_id",
            )
            .bind(job)
            .bind(&call_id)
            .bind(row.route)
            .fetch_optional(&mut *c)
            .await?;
            Ok(match rearmed {
                Some(id) => Pending::Retry { call_id: id },
                None => Pending::InFlight {
                    call_id: Some(call_id),
                },
            })
        }
        _ => Ok(Pending::Stored {
            state,
            code,
            detail,
            output,
            route,
            instance_id,
        }),
    }
}

/// The final columns of a call.
pub struct Final<'a> {
    pub state: &'static str,
    pub code: Option<&'a str>,
    pub latency_ms: i64,
    pub detail: Option<String>,
    /// What a write changed (its declared outputs), kept for a duplicate key's stored answer.
    pub object_change: Option<Value>,
    pub instance_id: Option<&'a str>,
    pub template: Option<&'a str>,
}

/// Step 11: finish the `pending` row `call_id`, with the instance and template the executor answered (0154);
/// `false` when it was not pending (already finished). Both are assigned, never COALESCEd: the row describes the
/// attempt that finished it, so a re-armed retry that answered no instance (a registered/builtin executor, a
/// load() failure) must not keep the failed attempt's.
pub async fn finish_pending(
    c: &mut PgConnection,
    job: Uuid,
    call_id: &str,
    f: &Final<'_>,
) -> ApiResult<bool> {
    let n = sqlx::query(
        "UPDATE operation_call SET state = $3, outcome_code = $4, finished_at = now(), latency_ms = $5, \
                detail = $6, object_change = $7, instance_id = $8, template = $9 \
          WHERE job_id = $1 AND call_id = $2 AND state = 'pending'",
    )
    .bind(job)
    .bind(call_id.to_owned())
    .bind(f.state)
    .bind(f.code.map(str::to_owned))
    .bind(f.latency_ms)
    .bind(f.detail.clone())
    .bind(f.object_change.clone())
    .bind(f.instance_id.map(str::to_owned))
    .bind(f.template.map(str::to_owned))
    .execute(&mut *c)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// Step 11 for a call that had no `pending` row (a read, a denial): insert it finished. A redelivered call (same
/// `call_id`) is not written twice. Denials carry no idempotency key, so a later confirmed call of the step can run.
pub async fn insert_finished(
    c: &mut PgConnection,
    job: Uuid,
    row: &CallRow,
    f: &Final<'_>,
) -> ApiResult<bool> {
    let key = if f.state == "denied" {
        None
    } else {
        row.idempotency_key.clone()
    };
    let n = sqlx::query(
        "INSERT INTO operation_call (tenant_id, job_id, call_id, case_id, operation, route, instance_id, template, \
                                     env, state, outcome_code, finished_at, latency_ms, initiator, attempt, \
                                     idempotency_key, confirm_ref, detail, release_id, draft_rev, step, \
                                     object_change) \
         VALUES (app_current_tenant(), $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now(), $11, $12, $13, $14, $15, \
                 $16, $17, $18, $19, $20) \
         ON CONFLICT DO NOTHING",
    )
    .bind(job)
    .bind(&row.call_id)
    .bind(&row.case_id)
    .bind(&row.operation)
    .bind(row.route)
    .bind(f.instance_id.map(str::to_owned))
    .bind(f.template.map(str::to_owned))
    .bind(row.env)
    .bind(f.state)
    .bind(f.code.map(str::to_owned))
    .bind(f.latency_ms)
    .bind(&row.initiator)
    .bind(row.attempt)
    .bind(key)
    .bind(&row.confirm_ref)
    .bind(f.detail.clone())
    .bind(&row.release_id)
    .bind(row.draft_rev)
    .bind(&row.step)
    .bind(f.object_change.clone())
    .execute(&mut *c)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// The record ids a call names (`order_id`, `payment_id`, `delivery_id`), kept on its `operation_call.step` as `refs`
/// (ids only, K50): the staff customer-context route's "focus" reads them. Nothing else of the input is kept.
pub fn refs_of(input: &Map<String, Value>) -> Value {
    let refs: Map<String, Value> = ["order_id", "payment_id", "delivery_id"]
        .iter()
        .filter_map(|k| {
            let v = input.get(*k)?.as_str()?.trim();
            (!v.is_empty() && v.len() <= 100)
                .then(|| ((*k).to_owned(), Value::String(v.to_owned())))
        })
        .collect();
    Value::Object(refs)
}

/// The `operation` case event of a finished call (ids and codes only, K50).
pub fn event_body(row: &CallRow, f: &Final<'_>) -> Value {
    json!({"call_id": row.call_id, "operation": row.operation, "route": row.route, "state": f.state,
           "code": f.code, "latency_ms": f.latency_ms, "attempt": row.attempt})
}

// ---- connector case.create (B4 F4; B7 R10, 0098) -----------------------------------------------------------------

/// The canonical `case.create` input naming the customer (B7 R10). Absent → the template's configured service user.
pub const CUSTOMER_EMAIL: &str = "customer_email";
/// The longest `agent_case.external_ref` (0098 CHECK, characters).
const MAX_EXTERNAL_REF: usize = 200;

/// Is `op_key` a `case.create` (any variant)?
pub fn is_case_create(op_key: &str) -> bool {
    op_key == "case.create" || op_key.starts_with("case.create{")
}

/// The email of the case's party (its `email` identity, verified first, then the most recently seen; the query of
/// `conversations::handover`'s worker notify). `None` when the case has no party or the party no email identity.
pub async fn party_email(c: &mut PgConnection, case_id: &str) -> ApiResult<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT pi.subject FROM agent_case ac \
           JOIN party_identity pi ON pi.tenant_id = ac.tenant_id AND pi.party_id = ac.party_id \
          WHERE ac.case_id = $1 AND pi.channel = 'email' \
          ORDER BY pi.verified DESC, pi.last_seen_at DESC LIMIT 1",
    )
    .bind(case_id.to_owned())
    .fetch_optional(&mut *c)
    .await?)
}

/// `inputs` of a connector `case.create` on a case with `customer_email` set by the runtime only: any value the call
/// names is dropped (on a customer channel the caller is the model, so a prompt could otherwise raise a ticket in
/// someone else's name), then the case party's `email` is set when it has one; absent, the template's configured
/// service user answers (`?? env.*`).
pub fn with_customer_email(
    mut inputs: Map<String, Value>,
    email: Option<String>,
) -> Map<String, Value> {
    inputs.remove(CUSTOMER_EMAIL);
    if let Some(e) = email {
        inputs.insert(CUSTOMER_EMAIL.to_owned(), Value::String(e));
    }
    inputs
}

/// The vendor ticket id of a `case.create` answer (its `case_id` output) as `agent_case.external_ref` stores it:
/// trimmed, non-blank, at most 200 characters (the 0098 CHECK); anything else is not stored.
pub fn external_ref(output: &Map<String, Value>) -> Option<String> {
    let id = match output.get("case_id")? {
        Value::String(s) => s.trim().to_owned(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    (!id.is_empty() && id.chars().count() <= MAX_EXTERNAL_REF).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(variant: &str, env: &str, target: RouteTarget) -> OperationRoute {
        OperationRoute {
            object: "rule".into(),
            verb: "evaluate".into(),
            variant: variant.into(),
            env: env.into(),
            target,
            updated_by: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn a_stored_instance_route_never_sends_a_builtin_to_a_connector() {
        let inst = RouteTarget::Instance {
            instance_id: "frappe-1".into(),
            detail: None,
        };
        assert!(matches!(
            target_of(&inst, "knowledge.search"),
            ResolvedTarget::Builtin { detail: Some(ref f) } if f == "knowledge"
        ));
        assert!(matches!(
            target_of(&inst, "rule.evaluate{RET-01}"),
            ResolvedTarget::Builtin { .. }
        ));
        assert!(matches!(
            target_of(&inst, "case.create"),
            ResolvedTarget::Instance { ref instance_id } if instance_id == "frappe-1"
        ));
    }

    #[test]
    fn a_variant_route_wins_over_the_star_route() {
        let rows = vec![
            route("*", "test", RouteTarget::Builtin),
            route("RET-01", "test", RouteTarget::NotOffered),
        ];
        let r = route_for(&rows, "rule.evaluate{RET-01}").unwrap();
        assert_eq!(r.target, RouteTarget::NotOffered);
        let r = route_for(&rows, "rule.evaluate{WAR-01}").unwrap();
        assert_eq!(
            target_of(&r.target, "rule.evaluate{WAR-01}"),
            ResolvedTarget::Builtin {
                detail: Some("rule".into())
            }
        );
        assert!(route_for(&rows, "order.read").is_none());
        assert!(route_for(&rows, "not a key").is_none());
    }

    #[test]
    fn denied_and_not_handled_states() {
        assert_eq!(state_text(OpState::Ok, true), "denied");
        assert_eq!(state_text(OpState::NotHandled, false), "error");
        assert_eq!(
            state_text(OpState::UnknownOutcome, false),
            "unknown_outcome"
        );
    }

    #[test]
    fn case_create_keys() {
        assert!(is_case_create("case.create"));
        assert!(is_case_create("case.create{return}"));
        assert!(!is_case_create("case.created"));
        assert!(!is_case_create("case.read"));
    }

    #[test]
    fn the_party_email_wins_over_the_call() {
        let filled = with_customer_email(Map::new(), Some("ada@example.test".into()));
        assert_eq!(filled[CUSTOMER_EMAIL], "ada@example.test");
        assert!(
            with_customer_email(Map::new(), None)
                .get(CUSTOMER_EMAIL)
                .is_none()
        );
        // a value the call names never overrides the party's email (prompt-injection safety) …
        let mut given = Map::new();
        given.insert(CUSTOMER_EMAIL.into(), json!("attacker@evil.test"));
        let replaced = with_customer_email(given.clone(), Some("ada@example.test".into()));
        assert_eq!(replaced[CUSTOMER_EMAIL], "ada@example.test");
        // … and is dropped when there is none: the template's service user answers instead
        let dropped = with_customer_email(given, None);
        assert!(dropped.get(CUSTOMER_EMAIL).is_none(), "{dropped:?}");
        let mut null = Map::new();
        null.insert(CUSTOMER_EMAIL.into(), Value::Null);
        let filled = with_customer_email(null, Some("ada@example.test".into()));
        assert_eq!(filled[CUSTOMER_EMAIL], "ada@example.test");
    }

    #[test]
    fn external_ref_of_a_case_create_answer() {
        let o = |v: Value| v.as_object().cloned().unwrap();
        assert_eq!(
            external_ref(&o(json!({"case_id": " HD-4711 "}))).as_deref(),
            Some("HD-4711")
        );
        assert_eq!(
            external_ref(&o(json!({"case_id": 42}))).as_deref(),
            Some("42")
        );
        assert_eq!(external_ref(&o(json!({"case_id": "  "}))), None);
        assert_eq!(external_ref(&o(json!({"status": "open"}))), None);
        assert_eq!(external_ref(&o(json!({"case_id": "x".repeat(201)}))), None);
    }
}
