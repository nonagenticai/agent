//! B7 T14: the connector executor B4's operation runner calls for an `instance` route (spec 12 §3.3 step 9).
//!
//! Input: (tenant, job, op key, env, the resolved route target, canonical inputs). Only `instance` targets are
//! handled; `builtin` / `agent_cases` / `deep_link` / `not_offered` stay B4's and are answered
//! [`OpState::NotHandled`].
//!
//! - **env `prod`** reads the job's current release snapshot (D62, crate::connectors::places::live_connectors):
//!   the snapshot's prod route for the op decides the system (variant, else its `*` route; the target the caller
//!   passed is not trusted over it), the snapshot's mapping overrides apply, and a system pinned to a tenant template
//!   runs the snapshot's version of it (`template_not_active` when the release holds it as a draft, or not at all).
//!   A job never released fails closed with `no_release`. Production never reads a working (draft) row.
//! - **env `test`** runs the working rows: the route target the caller resolved, the `field_mapping_override`
//!   rows of the instance (0094, T12) over the pinned template op's `field_map`, and a tenant template in draft.
//! - Then: instance + credential (custody, one short transaction) → render + transport exactly as the T3 contract
//!   test does ([`contract::call_op`]) → extract (with the overrides) → X ops run their `verify_read` → reduce to
//!   the declared outputs ([`mapping::reduce`]) → cap (strings 2 KiB, outcome 16 KiB) → `untrusted: true`.
//! - Instance `auth_failed` / `invalid_target` → `connector_misconfigured`; a stub instance never serves prod;
//!   per-instance in-process token bucket from `max_rps`, else the template's `rate_limit_model` → `rate_limited`.
//! - The credential never reaches the outcome or a log line: error details come scrubbed from the transport and
//!   are scrubbed again here (the credential's values + [`netguard::scrub_secrets`]); the output is redacted of
//!   the credential's values. No transaction is held across an upstream call.
//!
//! B4 seam: B4 (`runtime::ops`) moves [`OpRequest`] (its `CallCtx`), [`ResolvedTarget`] and [`OpOutcome`] into
//! its module and adds `impl OpExecutor for ConnectorExecutor { async fn execute(&self, ctx: &CallCtx, route:
//! &ResolvedTarget) -> OpOutcome { ConnectorExecutor::execute(self, ctx, route).await } }` plus the one
//! registration line in the runner.
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use sqlx::PgConnection;

use super::contract;
use super::custody;
use super::instances::{self, InstanceRow};
use super::mapping;
use super::model::{ConnectorTemplate, ConnectorTemplateOp, OpRef, SideEffect, Transport as Wire};
use super::opkey::{op_key, parse_op_key, same_op};
use super::places::{self, LiveConnectors};
use super::resolve::{OperationRoute, RouteTarget};
use super::tenant_templates;
use super::transport::{self, AuthSpec, CallTarget, auth};
use super::validate::placeholders;
use super::wire::{AuthKind, CallSecrets, OpError, RawResponse};
use crate::design::JobRef;
use crate::error::ApiError;
use crate::netguard;
use crate::state::AppState;
use crate::tenancy;

/// Longest string an outcome keeps (bytes, cut at a char boundary).
pub const MAX_OUTCOME_STRING_BYTES: usize = 2 * 1024;
/// Largest outcome output (compact JSON bytes).
pub const MAX_OUTCOME_BYTES: usize = 16 * 1024;

/// The environment a call runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Env {
    Test,
    Prod,
}

impl Env {
    pub fn as_str(self) -> &'static str {
        match self {
            Env::Test => "test",
            Env::Prod => "prod",
        }
    }
}

/// One operation call as the runner hands it over (B4's `CallCtx`, subset this executor reads).
/// B4 seam: B4 moves this to `runtime::ops::CallCtx` (adding its own fields: call id, case, initiator, …).
#[derive(Debug, Clone)]
pub struct OpRequest {
    pub tenant_id: Uuid,
    pub job_id: Uuid,
    /// `object.verb{variant}` (variant `*` left off).
    pub operation: String,
    pub env: Env,
    /// The canonical inputs (`{{canonical.*}}`).
    pub inputs: Map<String, Value>,
    /// The operation's declared outputs (B6 catalogue `operations[].outputs`); everything else is dropped.
    pub outputs: Vec<String>,
    /// `{{step.idempotency_key}}` (B4 step 8b; empty for reads).
    pub idempotency_key: String,
    /// The person the call is made for (`op.call.initiator_sub`); `None` = no person (a customer conversation). Never
    /// sent as an MCP task's `on_behalf_of`: a runtime call is the job's own (owner 2026-10-31).
    pub actor: Option<String>,
    /// The case the call belongs to, when one exists: a started MCP task is kept under it.
    pub case_id: Option<String>,
}

/// The route target B4's `RouteResolver` resolved for (job, op, env) — `operation_route.target_kind` and its
/// payload. B4 seam: moves to `runtime::ops::ResolvedTarget`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedTarget {
    Instance { instance_id: String },
    Builtin { detail: Option<String> },
    AgentCases,
    DeepLink { url: String },
    NotOffered,
}

/// How a call ended (`operation_call.state`, plus `not_handled` for targets this executor does not run).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpState {
    Ok,
    Error,
    UnknownOutcome,
    NotHandled,
}

/// What the runner records (`operation_call.state`, `outcome_code`, `latency_ms`, `detail`) and hands to the
/// model. B4 seam: moves to `runtime::ops::OpOutcome`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpOutcome {
    pub state: OpState,
    /// `None` on `ok`; else a canonical code (`connector_misconfigured`, `no_release`, `rate_limited`,
    /// `not_found`, `upstream_unavailable`, `unknown_outcome`, …).
    pub outcome_code: Option<String>,
    /// Operator-facing text: never a credential, an auth header or an upstream body.
    pub detail: Option<String>,
    /// The declared outputs only, capped. Vendor data: untrusted.
    pub output: Map<String, Value>,
    /// Always true for connector output (spec 17 N-04): the runner frames it as data, never as instructions.
    pub untrusted: bool,
    pub instance_id: Option<String>,
    /// `key@version` of the pinned template.
    pub template: Option<String>,
    /// X ops: whether the `verify_read` confirmed the write.
    pub verified: Option<bool>,
    pub latency_ms: u64,
}

impl OpOutcome {
    fn bare(state: OpState, code: Option<&str>, detail: Option<String>) -> Self {
        Self {
            state,
            outcome_code: code.map(str::to_owned),
            detail,
            output: Map::new(),
            untrusted: true,
            instance_id: None,
            template: None,
            verified: None,
            latency_ms: 0,
        }
    }

    fn not_handled() -> Self {
        Self::bare(OpState::NotHandled, Some("not_handled"), None)
    }

    fn error(code: &str, detail: impl Into<String>) -> Self {
        Self::bare(OpState::Error, Some(code), Some(detail.into()))
    }
}

/// A token bucket: `rate` tokens per second, burst `capacity` (= ⌈rate⌉, at least 1).
#[derive(Debug, Clone)]
struct TokenBucket {
    rate: f64,
    capacity: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    fn new(rate: f64, now: Instant) -> Self {
        let capacity = rate.ceil().max(1.0);
        Self {
            rate,
            capacity,
            tokens: capacity,
            last: now,
        }
    }

    fn try_take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Calls per second for an instance: its `max_rps` when set (> 0), else a `rate_limit_model` of kind `rps`/`rpm`
/// whose `value` text names `<n> rps` / `<n> rpm` (e.g. `GET 5 rps`); `None` = no limit known.
fn rate_of(max_rps: Option<f64>, model: &Value) -> Option<f64> {
    if let Some(r) = max_rps.filter(|r| r.is_finite() && *r > 0.0) {
        return Some(r);
    }
    let kind = model.get("kind").and_then(Value::as_str)?;
    if !matches!(kind, "rps" | "rpm") {
        return None;
    }
    let text = model.get("value").and_then(Value::as_str)?;
    let words: Vec<&str> = text.split_whitespace().collect();
    words.windows(2).find_map(|w| {
        let n: f64 = w[0]
            .parse()
            .ok()
            .filter(|n: &f64| n.is_finite() && *n > 0.0)?;
        let unit = w[1].to_ascii_lowercase();
        if unit.starts_with("rps") {
            Some(n)
        } else if unit.starts_with("rpm") {
            Some(n / 60.0)
        } else {
            None
        }
    })
}

/// One working `field_mapping_override` row (0094): `path` replaces the template path of `field` on `op`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideRow {
    pub op: String,
    pub field: String,
    pub path: String,
}

/// The overrides that apply to template op `top` (keyed `op_key`): only fields the op's `field_map` maps, only
/// non-empty paths. The result is the `overrides` argument of [`mapping::extract`] (override beats template path).
pub fn override_map(
    top: &ConnectorTemplateOp,
    key: &str,
    rows: &[OverrideRow],
) -> BTreeMap<String, String> {
    rows.iter()
        .filter(|r| r.op == key && !r.path.trim().is_empty())
        .filter(|r| top.field_map.iter().any(|e| e.field == r.field))
        .map(|r| (r.field.clone(), r.path.trim().to_owned()))
        .collect()
}

/// The template op that serves a call ([`ConnectorExecutor::served`]): its key (`object.verb` when a variant the
/// template lacks is served by the op's `*`, [`find_op`]) and the canonical inputs its request needs — each entry
/// names the inputs any one of which satisfies one placeholder (`{{canonical.a ?? canonical.b}}`); a placeholder
/// with a non-canonical fallback (`?? env.x`) or an optional one (`{{canonical.x?}}`) needs nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    pub op_key: String,
    pub needs: Vec<Vec<String>>,
}

/// The canonical inputs `op`'s path (with its query) and request render from ([`Served::needs`]).
pub fn canonical_needs(op: &ConnectorTemplateOp) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    for text in [op.path.as_str(), op.request.as_str()] {
        for p in placeholders(text) {
            // An optional placeholder (`{{canonical.x?}}`) never holds a call back: it renders without its input.
            if p.optional || p.refs().any(|(ns, _)| ns != "canonical") {
                continue;
            }
            let any: Vec<String> = p.refs().map(|(_, name)| name.to_owned()).collect();
            if !out.contains(&any) {
                out.push(any);
            }
        }
    }
    out
}

/// The template op for `key`: the exact variant, else the op's `*` (never the other way round).
pub(crate) fn find_op<'a>(
    tpl: &'a ConnectorTemplate,
    key: &str,
) -> Option<&'a ConnectorTemplateOp> {
    let want = parse_op_key(key);
    if want.object.is_empty() {
        return None;
    }
    let star = OpRef {
        object: want.object.clone(),
        verb: want.verb.clone(),
        variant: "*".to_owned(),
    };
    tpl.ops
        .iter()
        .find(|o| same_op(&o.op_ref(), &want))
        .or_else(|| tpl.ops.iter().find(|o| same_op(&o.op_ref(), &star)))
}

/// May `inst` serve a call in `env`? A stub serves test only; the instance env must be `env` or `all`.
fn instance_serves(inst_env: &str, stub: bool, env: Env) -> Result<(), String> {
    if stub && env == Env::Prod {
        return Err("A stub system never serves production.".to_owned());
    }
    if inst_env != env.as_str() && inst_env != "all" {
        return Err(format!(
            "This system is a {inst_env} system; it cannot serve {}.",
            env.as_str()
        ));
    }
    Ok(())
}

fn truncate_bytes(s: &mut String, max: usize) -> bool {
    if s.len() <= max {
        return false;
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    s.truncate(i);
    true
}

fn cap_strings(v: &mut Value, truncated: &mut bool) {
    match v {
        Value::String(s) => *truncated |= truncate_bytes(s, MAX_OUTCOME_STRING_BYTES),
        Value::Array(items) => {
            for x in items.iter_mut() {
                cap_strings(x, truncated);
            }
        }
        Value::Object(o) => {
            for x in o.values_mut() {
                cap_strings(x, truncated);
            }
        }
        _ => {}
    }
}

fn json_len(v: &impl Serialize) -> usize {
    serde_json::to_vec(v).map(|b| b.len()).unwrap_or(usize::MAX)
}

/// Caps a reduced output: every string at [`MAX_OUTCOME_STRING_BYTES`], then the whole object at
/// [`MAX_OUTCOME_BYTES`] by dropping the largest field until it fits. `truncated: true` when anything was cut.
pub fn cap_outcome(mut out: Map<String, Value>) -> Map<String, Value> {
    let mut truncated = out.get("truncated") == Some(&Value::Bool(true));
    for v in out.values_mut() {
        cap_strings(v, &mut truncated);
    }
    loop {
        let mut probe = out.clone();
        if truncated {
            probe.insert("truncated".to_owned(), Value::Bool(true));
        }
        if json_len(&probe) <= MAX_OUTCOME_BYTES {
            return probe;
        }
        let largest = out
            .iter()
            .filter(|(k, _)| k.as_str() != "truncated")
            .max_by_key(|(_, v)| json_len(*v))
            .map(|(k, _)| k.clone());
        match largest {
            Some(k) => {
                out.remove(&k);
                truncated = true;
            }
            None => return Map::new(),
        }
    }
}

/// What one call needs, read in one short tenant-scoped transaction.
struct Loaded {
    inst: InstanceRow,
    tpl: ConnectorTemplate,
    top: ConnectorTemplateOp,
    overrides: Vec<OverrideRow>,
    secret: Option<Map<String, Value>>,
}

/// The tenant's conveyor licence bearer (S06b): minted on demand and cached by `BundleMinter` until shortly
/// before it expires. A failure is `licence_unavailable` — no call and no tool listing proceeds without it.
///
/// Shared with the import path (`routes::mcp_import`), which builds its own secrets to list an external system's
/// tools: a licence-bearer system whose import carried no token would answer 401 and import nothing.
pub(crate) async fn mint_licence_token(st: &AppState, tenant: Uuid) -> Result<String, OpError> {
    st.bundle_minter
        .mint(tenant, crate::tenants::app_licence::App::Conveyor)
        .await
        .map(|minted| minted.token.expose().to_owned())
        .map_err(|e| {
            tracing::warn!(error = ?e, "the conveyor licence cannot be minted");
            OpError::new(
                "licence_unavailable",
                "The conveyor licence cannot be minted right now.",
            )
        })
}

fn from_api(e: &ApiError) -> OpOutcome {
    let detail = match e {
        ApiError::Internal(_) => "internal error".to_owned(),
        other => scrub_detail(&other.to_string(), &[]),
    };
    OpOutcome::error(e.code(), detail)
}

/// A detail with the credential's values and every `key: value` secret line removed.
fn scrub_detail(detail: &str, secrets: &[String]) -> String {
    let mut d = detail.to_owned();
    for s in secrets {
        d = netguard::redact_text(&d, s);
    }
    match netguard::scrub_secrets(Value::String(d)) {
        Value::String(s) => s,
        other => other.to_string(),
    }
}

fn op_error(e: &OpError, secrets: &[String]) -> OpOutcome {
    OpOutcome::error(e.code, scrub_detail(&e.detail, secrets))
}

/// The published prod route of `operation` (`object.verb{variant}`): its own route, else its `*` route.
fn live_target(routes: &[OperationRoute], operation: &str) -> Option<RouteTarget> {
    let op = parse_op_key(operation);
    if op.object.is_empty() {
        return None;
    }
    let star = OpRef {
        object: op.object.clone(),
        verb: op.verb.clone(),
        variant: "*".to_owned(),
    };
    let on = |want: &OpRef| {
        routes
            .iter()
            .find(|r| r.env == "prod" && same_op(&r.op_ref(), want))
            .map(|r| r.target.clone())
    };
    on(&op).or_else(|| on(&star))
}

/// The tenant template version `inst` pins, as the release holds it: `template_not_active` when the release holds it
/// as a draft or not at all.
#[allow(clippy::result_large_err)]
fn live_template(
    live: &LiveConnectors,
    tenant: Uuid,
    inst: &InstanceRow,
) -> Result<ConnectorTemplate, OpOutcome> {
    let not_active = || {
        OpOutcome::error(
            "template_not_active",
            format!(
                "{}@{} is not published in this job's release.",
                inst.template_key, inst.template_version
            ),
        )
    };
    let v = live
        .templates
        .get(&(inst.template_key.clone(), inst.template_version.clone()))
        .ok_or_else(not_active)?;
    if v.get("status").and_then(Value::as_str) == Some("draft") {
        return Err(not_active());
    }
    let row = places::template_row_of(tenant, v).map_err(|e| from_api(&e))?;
    tenant_templates::as_connector_template(&row).map_err(|e| from_api(&e))
}

/// The connector executor (`connectors::runtime::ConnectorExecutor: OpExecutor`, used by B4's runner).
#[derive(Clone)]
pub struct ConnectorExecutor {
    st: AppState,
    buckets: Arc<Mutex<HashMap<(Uuid, String), TokenBucket>>>,
}

impl ConnectorExecutor {
    pub fn new(st: AppState) -> Self {
        Self {
            st,
            buckets: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Takes one token of the instance's bucket (`true` = go). No known limit → always go.
    fn admit(&self, tenant: Uuid, instance: &str, rate: Option<f64>) -> bool {
        let Some(rate) = rate else {
            return true;
        };
        let now = Instant::now();
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        let bucket = buckets
            .entry((tenant, instance.to_owned()))
            .or_insert_with(|| TokenBucket::new(rate, now));
        if (bucket.rate - rate).abs() > f64::EPSILON {
            *bucket = TokenBucket::new(rate, now);
        }
        bucket.try_take(now)
    }

    /// The template op that would serve `req` on `route` and the inputs it needs, read as [`Self::execute`] reads
    /// it (same instance, release and template; no credential, no upstream call). `Ok(None)` for a non-instance
    /// route (served as asked); `Err` when it cannot be read, which the runner's step 6b refuses (fail closed).
    pub async fn served(
        &self,
        req: &OpRequest,
        route: &ResolvedTarget,
    ) -> Result<Option<Served>, String> {
        let ResolvedTarget::Instance { instance_id } = route else {
            return Ok(None);
        };
        let l = self.load(req, instance_id, false).await.map_err(|o| {
            format!(
                "the operation serving {} on {instance_id} cannot be read ({})",
                req.operation,
                o.outcome_code.as_deref().unwrap_or("unknown")
            )
        })?;
        Ok(Some(Served {
            op_key: op_key(&l.top.op_ref()),
            needs: canonical_needs(&l.top),
        }))
    }

    /// Runs one operation call. B4 seam: the body of `impl OpExecutor for ConnectorExecutor`.
    pub async fn execute(&self, req: &OpRequest, route: &ResolvedTarget) -> OpOutcome {
        let started = Instant::now();
        let mut out = self.run(req, route).await;
        out.latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        out
    }

    async fn run(&self, req: &OpRequest, route: &ResolvedTarget) -> OpOutcome {
        let ResolvedTarget::Instance { instance_id } = route else {
            return OpOutcome::not_handled();
        };
        let loaded = match self.load(req, instance_id, true).await {
            Ok(l) => l,
            Err(mut o) => {
                if o.instance_id.is_none() && o.state != OpState::NotHandled {
                    o.instance_id = Some(instance_id.clone());
                }
                return o;
            }
        };
        let mut out = self.call(req, &loaded).await;
        out.instance_id = Some(loaded.inst.id.clone());
        out.template = Some(format!("{}@{}", loaded.tpl.key, loaded.tpl.version));
        if out.state != OpState::Ok {
            tracing::debug!(
                instance = %loaded.inst.id,
                op = %req.operation,
                code = ?out.outcome_code,
                "connector call did not succeed"
            );
        }
        out
    }

    /// The tenant's conveyor licence bearer (S06b), for this call's secret map.
    #[allow(clippy::result_large_err)]
    async fn licence_token(&self, tenant: Uuid) -> Result<String, OpOutcome> {
        mint_licence_token(&self.st, tenant)
            .await
            .map_err(|e| op_error(&e, &[]))
    }

    /// Instance, template, op, overrides and credential, in one tenant-scoped transaction (closed before the
    /// upstream call). Prod reads the job's current release snapshot first (no release → `no_release`).
    // `Err` is the call's final outcome (an early answer, not an error type); it is built once per call.
    #[allow(clippy::result_large_err)]
    async fn load(
        &self,
        req: &OpRequest,
        instance_id: &str,
        with_secret: bool,
    ) -> Result<Loaded, OpOutcome> {
        let api = |e: ApiError| from_api(&e);
        let mut tx = tenancy::begin_scoped(&self.st.db, req.tenant_id)
            .await
            .map_err(api)?;
        let (id, live) = if req.env == Env::Prod {
            let live = self.live(&mut tx, req).await?;
            let id = match live_target(&live.routes, &req.operation) {
                Some(RouteTarget::Instance { instance_id, .. }) => instance_id,
                // the published route is B4's (builtin, agent_cases, deep link, not offered)
                Some(_) => return Err(OpOutcome::not_handled()),
                None => {
                    return Err(OpOutcome::error(
                        "no_route",
                        format!(
                            "{} has no production route in the job's published release.",
                            req.operation
                        ),
                    ));
                }
            };
            (id, Some(live))
        } else {
            (instance_id.to_owned(), None)
        };
        match self
            .load_in(&mut tx, req, &id, live.as_ref(), with_secret)
            .await
        {
            Ok(mut l) => {
                tx.commit()
                    .await
                    .map_err(|e| from_api(&ApiError::from(e)))?;
                // A licence-bearer template has no stored credential to read: the executor mints the tenant's
                // licence for this call, so `credential_headers` has a token and nothing goes stale. Minted after
                // the transaction closes: the licence service is remote, and no request's pool connection is held
                // across it.
                // Only a credential-bearing load mints: `served()` reads the same instance with
                // `with_secret = false` for route planning, and a planning struct must not carry a token.
                if with_secret && l.tpl.auth_kind == AuthKind::LicenceBearer {
                    let token = self.licence_token(req.tenant_id).await?;
                    l.secret
                        .get_or_insert_with(Map::new)
                        .insert("licence_token".to_owned(), Value::String(token));
                }
                Ok(l)
            }
            Err(mut o) => {
                o.instance_id = Some(id);
                Err(o)
            }
        }
    }

    /// The connector design of the job's current release (D62); `no_release` when it has none.
    #[allow(clippy::result_large_err)]
    async fn live(
        &self,
        c: &mut PgConnection,
        req: &OpRequest,
    ) -> Result<LiveConnectors, OpOutcome> {
        let api = |e: ApiError| from_api(&e);
        let key: Option<String> = sqlx::query_scalar("SELECT key FROM jobs WHERE id = $1")
            .bind(req.job_id)
            .fetch_optional(&mut *c)
            .await
            .map_err(|e| from_api(&ApiError::from(e)))?;
        let no_release = || {
            OpOutcome::error(
                "no_release",
                "This job has no published release to run in production.",
            )
        };
        let Some(key) = key else {
            return Err(no_release());
        };
        let jr = JobRef {
            id: req.job_id,
            key,
        };
        places::live_connectors(c, &self.st.design, &jr)
            .await
            .map_err(api)?
            .ok_or_else(no_release)
    }

    /// [`Self::load`] for the system `id` (`live` = Production's release design, `None` = the test env's working
    /// rows); the credential is opened only `with_secret`.
    #[allow(clippy::result_large_err)]
    async fn load_in(
        &self,
        tx: &mut PgConnection,
        req: &OpRequest,
        id: &str,
        live: Option<&LiveConnectors>,
        with_secret: bool,
    ) -> Result<Loaded, OpOutcome> {
        let api = |e: ApiError| from_api(&e);
        let inst = instances::one(&mut *tx, id)
            .await
            .map_err(api)?
            .ok_or_else(|| {
                OpOutcome::error(
                    "connector_misconfigured",
                    format!("The routed system \u{201c}{id}\u{201d} does not exist."),
                )
            })?;
        instance_serves(&inst.env, inst.stub, req.env)
            .map_err(|d| OpOutcome::error("connector_misconfigured", d))?;
        if matches!(inst.status.as_str(), "auth_failed" | "invalid_target") {
            let note = inst
                .last_error
                .as_ref()
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .map_or_else(
                    || format!("The system is marked {}.", inst.status),
                    |m| scrub_detail(m, &[]),
                );
            return Err(OpOutcome::error("connector_misconfigured", note));
        }
        let tpl = match live {
            Some(l) if inst.template_scope == "tenant" => live_template(l, req.tenant_id, &inst)?,
            _ => instances::pinned_template(&mut *tx, &inst)
                .await
                .map_err(|e| OpOutcome::error("connector_misconfigured", e.to_string()))?,
        };
        let top = find_op(&tpl, &req.operation).cloned().ok_or_else(|| {
            OpOutcome::error(
                "connector_misconfigured",
                format!(
                    "{}@{} does not implement {}.",
                    tpl.key, tpl.version, req.operation
                ),
            )
        })?;
        let overrides = match live {
            Some(l) => l
                .overrides
                .iter()
                .filter(|o| o.instance == inst.id)
                .map(|o| OverrideRow {
                    op: o.op.clone(),
                    field: o.field.clone(),
                    path: o.path.clone(),
                })
                .collect(),
            None => {
                let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
                    "SELECT op, field, path FROM field_mapping_override \
                      WHERE tenant_id = $1 AND instance_id = $2",
                )
                .bind(req.tenant_id)
                .bind(&inst.id)
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| from_api(&ApiError::from(e)))?;
                rows.into_iter()
                    .filter_map(|(op, field, path)| {
                        path.map(|path| OverrideRow { op, field, path })
                    })
                    .collect()
            }
        };
        // A template that declares `auth_kind: none` has nothing to authenticate with, so a stored credential is
        // not a precondition: transport `mcp` + kind `none` is the PLATFORM MCP server (its URL is `MCP_URL`, its
        // identity the platform's own; crate::mcp_client::client), and a none-auth REST/GraphQL system sends no
        // Authorization header. Requiring a row here refused every such system with `connector_misconfigured`
        // before the call was made (found live 2026-11-05: the `fr_mcp` platform-MCP system could not start a
        // project). An external MCP server is the `auth_kind != none` branch and still needs its credential; a
        // licence bearer (`needs_stored_credential() = false`) is minted per call below and stores none.
        let secret = if !with_secret
            || inst.stub
            || inst.platform
            || !tpl.auth_kind.needs_stored_credential()
        {
            None
        } else if !inst.has_credential() {
            return Err(OpOutcome::error(
                "connector_misconfigured",
                "No credential is set on this system.",
            ));
        } else {
            custody::open_credential(
                &mut *tx,
                self.st.tenant_keyring.as_deref(),
                req.tenant_id,
                &inst.id,
            )
            .await
            .map_err(api)?
        };
        Ok(Loaded {
            inst,
            tpl,
            top,
            overrides,
            secret,
        })
    }

    /// A long-running MCP tool is started as a durable MCP task instead of being called: the call answers at once with
    /// the task's handle (`mcp_task_started`), the runner's poller follows the task and relays its questions and result
    /// to the conversation (crate::runtime::task_relay); staff answer and cancel it in the job's Tasks inbox too.
    ///
    /// Two servers can run a task: the PLATFORM MCP server (transport `mcp`, auth `none`: its task tools are the
    /// global `MCP_TASK_TOOLS`) and an EXTERNAL one (any other auth kind) whose pinned template declares the op in
    /// `task_tools`. The row records which (`origin`), so every later get/answer/cancel reaches that server
    /// (crate::connectors::tasks). A server that finishes it at once (or does not offer tasks) answers like a plain
    /// call. `None` = not such a tool: call it as usual.
    async fn mcp_task(&self, req: &OpRequest, l: &Loaded) -> Option<OpOutcome> {
        use crate::mcp_client::external::ExtTaskStart;
        use crate::mcp_client::store::{NewTask, record_started};
        use crate::mcp_client::tasks::{StaffActor, TaskHandle, TaskStart};

        if l.tpl.transport != Wire::Mcp || l.inst.stub {
            return None;
        }
        let key = op_key(&l.top.op_ref());
        // What a server that answers at once returns maps exactly like a plain call.
        let extracted = |resp: Result<RawResponse, OpError>| {
            let ovr = override_map(&l.top, &key, &l.overrides);
            match resp.and_then(|r| mapping::extract(&l.top, &ovr, &r)) {
                Ok(written) => {
                    let mut out = OpOutcome::bare(OpState::Ok, None, None);
                    out.output = cap_outcome(mapping::reduce(written, &req.outputs));
                    out
                }
                Err(e) => op_error(&e, &[]),
            }
        };
        // (origin, instance, tool, handle), whichever server started the task
        let (origin, instance, tool, handle): (&str, Option<&str>, String, TaskHandle);
        if l.tpl.auth_kind == AuthKind::None {
            // the platform MCP server: its task tools are the global `MCP_TASK_TOOLS`
            let mcp = self.st.connectors.mcp()?;
            let name = l.top.path.trim().trim_start_matches('/');
            if !mcp.is_task_tool(name) {
                return None;
            }
            let args = match contract::render_op(
                &l.inst,
                &l.top,
                &req.inputs,
                l.secret.as_ref(),
                &req.idempotency_key,
            )
            .and_then(|r| transport::mcp::arguments(r.body.as_ref()))
            {
                Ok(a) => a,
                Err(e) => return Some(op_error(&e, &[])),
            };
            // A runtime call is chosen by the model, never by a person, so its task always starts as the job's own
            // (owner 2026-10-31: address `agent:<tenant>`, the job in the actor meta), even in a staff member's chat:
            // `on_behalf_of` hands fr that person's full rights and comes only from their own request (the Tasks inbox).
            let actor = StaffActor::for_job(req.tenant_id, req.job_id);
            match mcp.client.start_task(&actor, name, args).await {
                Ok(TaskStart::Task(h)) => {
                    origin = "platform";
                    instance = None;
                    tool = name.to_owned();
                    handle = h;
                }
                Ok(TaskStart::Completed(v)) => {
                    return Some(extracted(transport::mcp::answer(Ok(v))));
                }
                Err(e) => return Some(extracted(transport::mcp::answer(Err(e)))),
            }
        } else {
            // an external MCP server: the pinned template says which of its ops run as tasks (op keys; the stored
            // spelling is canonicalised in `as_connector_template`, and by `op_key` here in case a caller did not)
            if !l
                .tpl
                .task_tools
                .iter()
                .any(|k| op_key(&parse_op_key(k)) == key)
            {
                return None;
            }
            let rendered = match contract::render_op(
                &l.inst,
                &l.top,
                &req.inputs,
                l.secret.as_ref(),
                &req.idempotency_key,
            ) {
                Ok(r) => r,
                Err(e) => return Some(op_error(&e, &[])),
            };
            let target = CallTarget {
                tenant: req.tenant_id,
                instance_id: l.inst.id.clone(),
                env: req.env.as_str(),
                credential_version: l.inst.cred_version.unwrap_or(0),
                base_url: l.inst.base_url.clone(),
                stub: l.inst.stub,
            };
            let auth = AuthSpec {
                kind: l.tpl.auth_kind,
                params: l.tpl.auth_params.clone(),
            };
            let secrets = CallSecrets {
                config: l.inst.env_map(),
                secret: l.secret.clone().unwrap_or_default(),
            };
            let timeout = Duration::from_millis(l.top.timeout_ms.clamp(100, 30_000));
            // The task's `_meta` actor is the job's own, as for the platform server above (owner 2026-10-31).
            let actor = StaffActor::for_job(req.tenant_id, req.job_id);
            match self
                .st
                .connectors
                .external_mcp()
                .start_task(
                    &self.st.connectors,
                    &target,
                    &auth,
                    &secrets,
                    &key,
                    &rendered,
                    timeout,
                    Some(crate::mcp_client::tasks::actor_meta(&actor)),
                )
                .await
            {
                Ok(ExtTaskStart::Task(h)) => {
                    origin = "connector";
                    instance = Some(l.inst.id.as_str());
                    tool = key.clone();
                    handle = h;
                }
                Ok(ExtTaskStart::Completed(r)) => return Some(extracted(Ok(r))),
                Err(e) => return Some(extracted(Err(e))),
            }
        }
        // A runtime call is the job's own whichever server runs the task (owner 2026-10-31).
        let created_by = StaffActor::for_job(req.tenant_id, req.job_id).staff_user;
        let task = NewTask {
            tenant: req.tenant_id,
            job: req.job_id,
            operation: &req.operation,
            idempotency_key: &req.idempotency_key,
            case_id: req.case_id.as_deref(),
            created_by: &created_by,
            origin,
            instance,
        };
        if let Err(e) = record_started(&self.st.db, &task, &tool, &handle).await {
            tracing::error!(task_id = %handle.task_id, error = %e, "mcp: started task not recorded");
            return Some(OpOutcome::bare(
                OpState::UnknownOutcome,
                Some("unknown_outcome"),
                Some(format!(
                    "{tool} started a long-running task that could not be recorded."
                )),
            ));
        }
        let mut out = OpOutcome::bare(
            OpState::Ok,
            Some("mcp_task_started"),
            Some(task_started_detail(&tool)),
        );
        out.output = task_started_output(&tool, &handle.task_id, &handle.status);
        Some(out)
    }

    /// One call of `top` (render → transport → extract with the overrides). `Err` is the failed outcome.
    #[allow(clippy::result_large_err)]
    async fn call_one(
        &self,
        l: &Loaded,
        top: &ConnectorTemplateOp,
        canonical: &Map<String, Value>,
        idempotency_key: &str,
        secrets: &[String],
        env: Env,
    ) -> Result<Map<String, Value>, OpOutcome> {
        let resp = contract::call_op(
            &self.st.connectors,
            &l.inst,
            &l.tpl,
            top,
            canonical,
            l.secret.as_ref(),
            idempotency_key,
            env.as_str(),
        )
        .await
        .map_err(|e| op_error(&e, secrets))?;
        let ovr = override_map(top, &op_key(&top.op_ref()), &l.overrides);
        mapping::extract(top, &ovr, &resp).map_err(|e| op_error(&e, secrets))
    }

    async fn call(&self, req: &OpRequest, l: &Loaded) -> OpOutcome {
        let secrets = match &l.secret {
            Some(s) => auth::secret_strings(&CallSecrets {
                config: Map::new(),
                secret: s.clone(),
            }),
            None => Vec::new(),
        };
        let rate = rate_of(l.inst.max_rps, &l.tpl.rate_limit_model);
        if !self.admit(req.tenant_id, &l.inst.id, rate) {
            return OpOutcome::error(
                "rate_limited",
                "This system's call rate limit is reached; try again shortly.",
            );
        }
        if let Some(out) = self.mcp_task(req, l).await {
            return out;
        }
        let written = match self
            .call_one(
                l,
                &l.top,
                &req.inputs,
                &req.idempotency_key,
                &secrets,
                req.env,
            )
            .await
        {
            Ok(o) => o,
            Err(o) => return o,
        };
        let mut verified = None;
        // B4 step 10: an X or P call runs its verify read.
        if matches!(l.top.side_effect, SideEffect::X | SideEffect::P) {
            let vop = l
                .top
                .verify_read
                .as_deref()
                .and_then(|k| find_op(&l.tpl, k));
            let Some(vop) = vop else {
                return OpOutcome::bare(
                    OpState::UnknownOutcome,
                    Some("unknown_outcome"),
                    Some(
                        "The write was sent but its template declares no callable verify_read."
                            .into(),
                    ),
                );
            };
            let mut canonical = req.inputs.clone();
            for (k, v) in &written {
                canonical.insert(k.clone(), v.clone());
            }
            if let Err(o) = self
                .call_one(l, vop, &canonical, "", &secrets, req.env)
                .await
            {
                let detail = format!(
                    "The write was sent but its verify read failed ({}).",
                    o.outcome_code.as_deref().unwrap_or("error")
                );
                return OpOutcome::bare(
                    OpState::UnknownOutcome,
                    Some("unknown_outcome"),
                    Some(detail),
                );
            }
            verified = Some(true);
        }
        let reduced = mapping::reduce(written, &req.outputs);
        let redacted = transport::redact_body(Value::Object(reduced), &secrets);
        let output = match redacted {
            Value::Object(m) => cap_outcome(m),
            _ => Map::new(),
        };
        let mut out = OpOutcome::bare(OpState::Ok, None, None);
        out.output = output;
        out.verified = verified;
        out
    }
}

/// The detail a started MCP task reports to the model. Starting a task only QUEUES it: the work has not run when the
/// call returns, so the detail says so and forbids claiming completion. Found live 2026-11-07: a queued `fr` project
/// start read as completion and the assistant told the customer "I've set up the project for you" while only a queued
/// task existed.
fn task_started_detail(tool: &str) -> String {
    format!(
        "{tool} is long-running: the request is QUEUED — nothing is created or complete yet. Tell the user it was submitted and the result follows in this conversation; do not claim the task or project exists, is set up, is created, or is done until the result arrives."
    )
}

/// The MODEL-VISIBLE result of a started task. `OpResult.output` is exactly this map (runtime/runner/mod.rs:1401),
/// and the assistant hands it to the model as the tool message (`runtime/orchestrator.rs`: `OpState::Ok =>
/// r.output.clone()` → `assistant::loop::tool_messages`). `detail` alone never reaches the model, which is why the
/// live 2026-11-07 run read `status: "working"` as completion. `mcp_task_state` states the queued fact; the note
/// carries the same directive as the operator-facing detail.
fn task_started_output(tool: &str, task_id: &str, status: &str) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("mcp_task_id".into(), Value::String(task_id.to_owned()));
    out.insert("mcp_task_status".into(), Value::String(status.to_owned()));
    out.insert("mcp_task_state".into(), Value::String("queued".into()));
    out.insert(
        "mcp_task_note".into(),
        Value::String(task_started_detail(tool)),
    );
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::connectors::model::{FieldMapEntry, Idempotency};

    fn op(fields: &[(&str, &str)]) -> ConnectorTemplateOp {
        ConnectorTemplateOp {
            input_types: Default::default(),
            object: "order".into(),
            verb: "read".into(),
            variant: "*".into(),
            side_effect: SideEffect::R,
            op_kind: Default::default(),
            method: "GET".into(),
            path: "/orders/{{canonical.order_id}}".into(),
            request: String::new(),
            field_map: fields
                .iter()
                .map(|(f, p)| FieldMapEntry {
                    path: (*p).into(),
                    field: (*f).into(),
                    transform: None,
                })
                .collect(),
            status_map: vec![],
            fault_map: vec![],
            pagination: None,
            idempotency: Idempotency::Nat,
            verify_read: None,
            timeout_ms: 1000,
            fixtures: vec![],
        }
    }

    fn row(op: &str, field: &str, path: &str) -> OverrideRow {
        OverrideRow {
            op: op.into(),
            field: field.into(),
            path: path.into(),
        }
    }

    #[test]
    fn override_merge_keeps_only_mapped_fields_of_this_op() {
        let top = op(&[("status", "$.status"), ("total", "$.total")]);
        let rows = [
            row("order.read", "status", "$.state"),
            row("order.read", "unknown", "$.x"),
            row("order.list", "total", "$.sum"),
            row("order.read", "total", "  "),
        ];
        let got = override_map(&top, "order.read", &rows);
        assert_eq!(got.len(), 1);
        assert_eq!(got["status"], "$.state");
    }

    #[test]
    fn strings_are_capped_at_2_kib_on_a_char_boundary() {
        let out: Map<String, Value> =
            serde_json::from_value(json!({"a": "é".repeat(2000), "b": "short"})).unwrap();
        let capped = cap_outcome(out);
        let a = capped["a"].as_str().unwrap();
        assert!(a.len() <= MAX_OUTCOME_STRING_BYTES);
        assert!(a.chars().all(|c| c == 'é'));
        assert_eq!(capped["b"], "short");
        assert_eq!(capped["truncated"], true);
    }

    #[test]
    fn outcome_is_capped_at_16_kib_by_dropping_the_largest_field() {
        let mut out = Map::new();
        for i in 0..20 {
            out.insert(format!("f{i:02}"), Value::String("x".repeat(1500)));
        }
        out.insert("id".into(), json!("C-1"));
        let capped = cap_outcome(out);
        assert!(serde_json::to_vec(&capped).unwrap().len() <= MAX_OUTCOME_BYTES);
        assert_eq!(capped["id"], "C-1");
        assert_eq!(capped["truncated"], true);
    }

    #[test]
    fn small_outcome_is_untouched() {
        let out: Map<String, Value> =
            serde_json::from_value(json!({"status": "open", "n": 3})).unwrap();
        assert_eq!(cap_outcome(out.clone()), out);
    }

    #[test]
    fn token_bucket_bursts_then_refills() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(2.0, t0);
        assert!(b.try_take(t0));
        assert!(b.try_take(t0));
        assert!(!b.try_take(t0));
        assert!(b.try_take(t0 + Duration::from_millis(500)));
        assert!(!b.try_take(t0 + Duration::from_millis(500)));
        // a sub-1 rps limit still allows one call, then waits
        let mut slow = TokenBucket::new(0.5, t0);
        assert!(slow.try_take(t0));
        assert!(!slow.try_take(t0 + Duration::from_secs(1)));
        assert!(slow.try_take(t0 + Duration::from_secs(2)));
    }

    #[test]
    fn rate_comes_from_max_rps_else_the_model() {
        let model = json!({"kind": "rps", "value": "GET 5 rps", "honour_retry_after": true});
        assert_eq!(rate_of(Some(2.0), &model), Some(2.0));
        assert_eq!(rate_of(None, &model), Some(5.0));
        assert_eq!(rate_of(Some(0.0), &model), Some(5.0));
        let rpm = json!({"kind": "rpm", "value": "120 rpm"});
        assert_eq!(rate_of(None, &rpm), Some(2.0));
        let vague = json!({"kind": "rpm", "value": "6,000 requests / 5 min per user"});
        assert_eq!(rate_of(None, &vague), None);
        assert_eq!(rate_of(None, &json!({"kind": "none"})), None);
    }

    #[test]
    fn per_instance_buckets_are_independent() {
        let mut buckets: HashMap<(Uuid, String), TokenBucket> = HashMap::new();
        let t = Uuid::new_v4();
        let now = Instant::now();
        let a = buckets
            .entry((t, "a".into()))
            .or_insert_with(|| TokenBucket::new(1.0, now));
        assert!(a.try_take(now));
        assert!(!a.try_take(now));
        let b = buckets
            .entry((t, "b".into()))
            .or_insert_with(|| TokenBucket::new(1.0, now));
        assert!(b.try_take(now));
    }

    #[test]
    fn stub_never_serves_prod_and_env_must_match() {
        assert!(instance_serves("test", true, Env::Prod).is_err());
        assert!(instance_serves("test", true, Env::Test).is_ok());
        assert!(instance_serves("prod", false, Env::Test).is_err());
        assert!(instance_serves("test", false, Env::Prod).is_err());
        assert!(instance_serves("all", false, Env::Prod).is_ok());
    }

    #[test]
    fn variant_falls_back_to_star_never_the_reverse() {
        let mut star = op(&[]);
        star.object = "case".into();
        star.verb = "create".into();
        let mut ret = star.clone();
        ret.variant = "return".into();
        let base: ConnectorTemplate = serde_json::from_value(json!({
            "key": "k", "version": "1", "transport": "rest", "auth_kind": "none",
            "status": "active", "ops": []
        }))
        .unwrap();
        let tpl_star_only = ConnectorTemplate {
            ops: vec![star.clone()],
            ..base.clone()
        };
        assert_eq!(
            find_op(&tpl_star_only, "case.create{complaint}").map(|o| o.variant.as_str()),
            Some("*")
        );
        let tpl_variant_only = ConnectorTemplate {
            ops: vec![ret],
            ..base
        };
        assert!(find_op(&tpl_variant_only, "case.create").is_none());
        assert_eq!(
            find_op(&tpl_variant_only, "case.create{return}").map(|o| o.variant.as_str()),
            Some("return")
        );
        assert!(find_op(&tpl_star_only, "not a key").is_none());
    }

    #[test]
    fn error_details_are_scrubbed_of_the_credential() {
        let e = OpError::new(
            "upstream_unavailable",
            "got tok-SECRET-123 back\nPASSWORD:hunter2",
        );
        let o = op_error(&e, &["tok-SECRET-123".to_owned()]);
        let d = o.detail.unwrap();
        assert!(!d.contains("tok-SECRET-123"), "{d}");
        assert!(!d.contains("hunter2"), "{d}");
        assert_eq!(o.outcome_code.as_deref(), Some("upstream_unavailable"));
        assert!(o.untrusted);
    }

    #[test]
    fn non_instance_targets_are_not_handled_shape() {
        let o = OpOutcome::not_handled();
        assert_eq!(o.state, OpState::NotHandled);
        assert_eq!(o.outcome_code.as_deref(), Some("not_handled"));
    }

    #[test]
    fn a_started_task_detail_says_queued_and_forbids_claiming_completion() {
        let d = task_started_detail("fr_start_project");
        assert!(d.contains("QUEUED"), "{d}");
        assert!(d.contains("nothing is created"), "{d}");
        assert!(d.contains("do not claim"), "{d}");
        assert!(d.contains("fr_start_project"), "{d}");
    }

    /// The model reads `OpResult.output`, not `detail`: the queued fact must be IN the tool result.
    #[test]
    fn the_model_visible_task_result_says_queued() {
        let out = task_started_output("fr_start_project", "task-1", "working");
        assert_eq!(out["mcp_task_id"], Value::String("task-1".into()));
        assert_eq!(out["mcp_task_status"], Value::String("working".into()));
        assert_eq!(out["mcp_task_state"], Value::String("queued".into()));
        let note = out["mcp_task_note"].as_str().unwrap_or_default();
        assert!(note.contains("QUEUED"), "{note}");
        assert!(note.contains("nothing is created"), "{note}");
        assert!(note.contains("do not claim"), "{note}");
    }
}
