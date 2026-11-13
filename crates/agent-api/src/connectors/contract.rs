//! T3 instance contract tests and sample fetch (B7 T10; outcome rules = the fixture's `contractOutcome`,
//! `web/src/lib/api/areas/connectors.ts`, doc 10 §4.5).
//!
//! Outcome rules, in order: no credential (not platform, not stub) → red `connector_auth_failed`; instance status
//! `auth_failed` → red `connector_auth_failed`; stub → the op's fixture replayed through the stub transport, green
//! ("never counts for readiness"); platform → `not_run` / `not_applicable`; prod X/P → `write_unverified` (no call);
//! prod W without `prod_write_ok` → `needs_ok` (no call); else a real call: probe inputs → render (T5) → transport
//! (T6) → extract. Probe inputs: every `{{canonical.<f>}}` of the op's path and request comes from the instance
//! env id `probe.<f>` (W/X/P ops default `subject`/`summary`/`title` to a tagged text); a missing one is `not_run`
//! `probe_input_missing`, naming the env id. W subjects/summaries are prefixed `[agent-contract-test]`.
//!
//! No DB transaction is held across an upstream call: read (instance, template, credential) in one transaction,
//! call, write (result, status, audit) in another. A 401 sets the instance `auth_failed` + `last_error`; the first
//! green moves `untested → ok`.
//!
//! Not built yet (needs later B7 tasks): the probe chain (a create's output feeding the next read), closing a W
//! test record afterwards (notes say "left open, tagged").
//!
//! Mapping overrides (B7 T12): a test-env instance extracts through its working `field_mapping_override` rows
//! (crate::connectors::overrides::for_env); a prod instance through the template paths (a contract test is not a
//! job's call, so there is no release snapshot to read its overrides from).
use std::collections::BTreeMap;
use std::time::Duration;

use chrono::Utc;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::instances::{self, InstanceRow, NewResult, ResultRow};
use super::mapping::{self, RenderCtx};
use super::model::{ConnectorTemplate, ConnectorTemplateOp, SideEffect, Transport as Wire};
use super::opkey::op_key;
use super::overrides;
use super::transport::{self, AuthSpec, CallOptions, CallTarget, Transport, TransportKind, stub};
use super::validate::placeholders;
use super::wire::{AuthKind, CallSecrets, OpError, RawResponse, RenderedRequest};
use crate::audit::{self, Event};
use crate::connectors::custody;
use crate::error::{ApiError, ApiResult};
use crate::netguard;
use crate::state::AppState;
use crate::tenancy::TenantCtx;

/// The tag every contract-test write carries.
pub const TEST_TAG: &str = "[agent-contract-test]";
/// Largest stored contract response (compact JSON bytes; the column caps its jsonb text at 64 KiB).
const MAX_STORED_RESPONSE: usize = 32 * 1024;
/// Largest stored sample (compact JSON bytes; the column caps its jsonb text at 256 KiB).
const MAX_SAMPLE: usize = 128 * 1024;

/// Why a live call did not produce a response.
#[derive(Debug)]
pub enum CallFailure {
    /// The env id (`probe.<field>`) a required canonical input comes from is not set.
    ProbeMissing(String),
    /// Render, netguard, auth or transport refused.
    Op(OpError),
}

/// What happens to the instance after a test.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Effect {
    None,
    AuthFailed(String),
    InvalidTarget(String),
}

/// One contract-test verdict.
#[derive(Debug, Clone)]
struct Verdict {
    outcome: &'static str,
    code: Option<&'static str>,
    note: String,
    /// `at` is stamped (false for outcomes that made no decision-time call: `write_unverified`, `needs_ok`).
    stamped: bool,
    response: Option<Value>,
    effect: Effect,
}

impl Verdict {
    fn new(outcome: &'static str, code: Option<&'static str>, note: impl Into<String>) -> Self {
        Self {
            outcome,
            code,
            note: note.into(),
            stamped: true,
            response: None,
            effect: Effect::None,
        }
    }
}

fn not_implemented(tpl: &ConnectorTemplate, op: &str) -> ApiError {
    ApiError::Missing(
        "operation_not_implemented",
        format!("{}@{} does not implement {op}.", tpl.key, tpl.version),
    )
}

/// The template op `op` (an `object.verb{variant}` key), else 404 `operation_not_implemented`.
fn find_op(tpl: &ConnectorTemplate, op: &str) -> ApiResult<ConnectorTemplateOp> {
    tpl.ops
        .iter()
        .find(|o| op_key(&o.op_ref()) == op)
        .cloned()
        .ok_or_else(|| not_implemented(tpl, op))
}

fn fixtures_of(op: &ConnectorTemplateOp) -> Vec<Value> {
    op.fixtures
        .iter()
        .filter_map(|f| serde_json::to_value(f).ok())
        .collect()
}

/// The JSON kept for later: the known credential strings are redacted by the transport, and every secret the vendor
/// answers (a `password`/`token`/… key, a `PASSWORD:` line) is scrubbed here ([`netguard::scrub_secrets`]) — the
/// stored response is served back as a sample. `None` when it is too large to store.
fn storable(body: &Value, max: usize) -> Option<Value> {
    let body = netguard::scrub_secrets(body.clone());
    let len = serde_json::to_vec(&body)
        .map(|b| b.len())
        .unwrap_or(usize::MAX);
    (len <= max).then_some(body)
}

/// The rules that decide without a network call (`None` = make the call).
fn decide(
    inst: &InstanceRow,
    tpl: &ConnectorTemplate,
    op: &ConnectorTemplateOp,
    prod_write_ok: bool,
) -> Option<Verdict> {
    let side = op.side_effect;
    // `none` and a licence bearer have no stored credential to set: transport mcp + none is the PLATFORM MCP server
    // (its URL is `MCP_URL`), a none-auth REST/GraphQL system sends no Authorization header, and a licence bearer is
    // minted per call.
    if !inst.platform
        && !inst.stub
        && tpl.auth_kind.needs_stored_credential()
        && !inst.has_credential()
    {
        return Some(Verdict::new(
            "red",
            Some("connector_auth_failed"),
            "No credential set on this system.",
        ));
    }
    if inst.status == "auth_failed" {
        let note = inst
            .last_error
            .as_ref()
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("Authentication failed.")
            .to_owned();
        return Some(Verdict::new("red", Some("connector_auth_failed"), note));
    }
    if inst.stub {
        return Some(match stub::replay_sample(&fixtures_of(op)) {
            Ok(resp) if (200..300).contains(&resp.status) => {
                let mut v = Verdict::new(
                    "green",
                    None,
                    "stub fixture \u{2014} never counts for readiness",
                );
                v.response = storable(&resp.body, MAX_STORED_RESPONSE);
                v
            }
            Ok(resp) => Verdict::new(
                "red",
                Some("connector_misconfigured"),
                format!("the stub fixture answered {}", resp.status),
            ),
            Err(e) => Verdict::new("red", Some(e.code), e.detail),
        });
    }
    if inst.platform {
        return Some(Verdict::new(
            "not_run",
            Some("not_applicable"),
            "Platform connectors are tested by the platform, not by a tenant contract test.",
        ));
    }
    let prod_write = inst.env == "prod" && side != SideEffect::R;
    if prod_write && matches!(side, SideEffect::X | SideEffect::P) {
        let mut v = Verdict::new(
            "write_unverified",
            None,
            format!(
                "{} ops run only against a test-env instance; prod relies on confirm-back + verify_read.",
                side.as_str()
            ),
        );
        v.stamped = false;
        return Some(v);
    }
    if prod_write && !prod_write_ok {
        let mut v = Verdict::new("needs_ok", None, "Prod write: needs your explicit OK");
        v.stamped = false;
        return Some(v);
    }
    None
}

/// The canonical inputs of `op` from the instance env ids (`probe.<field>`); `Err` names the missing env id.
pub fn probe_inputs(
    op: &ConnectorTemplateOp,
    env_ids: &Map<String, Value>,
) -> Result<Map<String, Value>, String> {
    let write = op.side_effect != SideEffect::R;
    let mut out = Map::new();
    for text in [op.path.as_str(), op.request.as_str()] {
        for p in placeholders(text) {
            // `{{a?}}`: an optional input needs no probe value, the render drops what it fills.
            if p.optional {
                continue;
            }
            // `{{a ?? b}}` (R10): a missing `a` is not an error, the render falls back to `b`.
            let refs: Vec<(&str, &str)> = p.refs().collect();
            for (i, &(ns, name)) in refs.iter().enumerate() {
                let has_fallback = i + 1 < refs.len();
                if ns != "canonical" || out.contains_key(name) {
                    break;
                }
                let key = format!("probe.{name}");
                let given = env_ids
                    .get(&key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|v| !v.is_empty());
                let tagged = write && matches!(name, "subject" | "summary" | "title");
                let value = match (given, tagged) {
                    (Some(v), true) => format!("{TEST_TAG} {v}"),
                    (Some(v), false) => v.to_owned(),
                    (None, true) => format!("{TEST_TAG} contract test from Agent"),
                    (None, false) if has_fallback => continue,
                    (None, false) => return Err(key),
                };
                out.insert(name.to_owned(), Value::String(value));
                break;
            }
        }
    }
    Ok(out)
}

fn transport_kind(tpl: &ConnectorTemplate) -> Result<TransportKind, OpError> {
    match tpl.transport {
        Wire::Rest => Ok(TransportKind::Rest),
        Wire::Graphql => Ok(TransportKind::Graphql),
        Wire::Stub => Ok(TransportKind::Stub),
        Wire::Mcp => Ok(TransportKind::Mcp),
        other => Err(transport::misconfigured(format!(
            "the {other:?} transport is not callable from agent-api yet"
        ))),
    }
}

/// One live call of `op` on `inst` (no transaction may be open around it).
pub async fn live_call(
    connectors: &Transport,
    inst: &InstanceRow,
    tpl: &ConnectorTemplate,
    op: &ConnectorTemplateOp,
    secret: Option<&Map<String, Value>>,
) -> Result<RawResponse, CallFailure> {
    let canonical = probe_inputs(op, &inst.env_map()).map_err(CallFailure::ProbeMissing)?;
    let idempotency_key = format!("agent-contract-test-{}", Uuid::new_v4());
    // A contract test is a probe, not production traffic: only a prod-only system is called as prod.
    let env = if inst.env == "prod" { "prod" } else { "test" };
    call_op(
        connectors,
        inst,
        tpl,
        op,
        &canonical,
        secret,
        &idempotency_key,
        env,
    )
    .await
    .map_err(CallFailure::Op)
}

/// One call of `op` on `inst` with the given canonical inputs: render (T5) → transport (T6). Shared by the T3
/// contract test ([`live_call`]) and the runtime executor (crate::connectors::runtime). No transaction may be open
/// around it. Every `Err` detail is already scrubbed of the credential's values by the transport. `env` is the env
/// of the call (`test` | `prod`), rendered as `{{system.env}}` in the template's static headers.
#[allow(clippy::too_many_arguments)]
pub async fn call_op(
    connectors: &Transport,
    inst: &InstanceRow,
    tpl: &ConnectorTemplate,
    op: &ConnectorTemplateOp,
    canonical: &Map<String, Value>,
    secret: Option<&Map<String, Value>>,
    idempotency_key: &str,
    env: &'static str,
) -> Result<RawResponse, OpError> {
    let env_ids = inst.env_map();
    let secret = secret.cloned().unwrap_or_default();
    // A stub instance of an MCP template replays its fixtures like any other stub.
    let kind = if inst.stub && tpl.transport == Wire::Mcp {
        TransportKind::Stub
    } else {
        transport_kind(tpl)?
    };
    let req = {
        let ctx = RenderCtx {
            canonical,
            config: &env_ids,
            env: &env_ids,
            secret: &secret,
            idempotency_key,
        };
        mapping::render(op, &ctx)?
    };
    call_rendered(connectors, inst, tpl, op, kind, req, env_ids, secret, env).await
}

/// `op` rendered on `inst` without calling it (the connector executor starts long-running MCP tools itself).
pub fn render_op(
    inst: &InstanceRow,
    op: &ConnectorTemplateOp,
    canonical: &Map<String, Value>,
    secret: Option<&Map<String, Value>>,
    idempotency_key: &str,
) -> Result<RenderedRequest, OpError> {
    let env_ids = inst.env_map();
    let secret = secret.cloned().unwrap_or_default();
    mapping::render(
        op,
        &RenderCtx {
            canonical,
            config: &env_ids,
            env: &env_ids,
            secret: &secret,
            idempotency_key,
        },
    )
}

#[allow(clippy::too_many_arguments)]
async fn call_rendered(
    connectors: &Transport,
    inst: &InstanceRow,
    tpl: &ConnectorTemplate,
    op: &ConnectorTemplateOp,
    kind: TransportKind,
    req: RenderedRequest,
    env_ids: Map<String, Value>,
    secret: Map<String, Value>,
    env: &'static str,
) -> Result<RawResponse, OpError> {
    let target = CallTarget {
        tenant: inst.tenant_id,
        instance_id: inst.id.clone(),
        env,
        credential_version: inst.cred_version.unwrap_or(0),
        base_url: inst.base_url.clone(),
        stub: inst.stub,
    };
    let auth = AuthSpec {
        kind: tpl.auth_kind,
        params: tpl.auth_params.clone(),
    };
    let secrets = CallSecrets {
        config: env_ids,
        secret,
    };
    let timeout = Duration::from_millis(op.timeout_ms.clamp(100, 30_000));
    if kind == TransportKind::Mcp && tpl.auth_kind != AuthKind::None {
        // An external MCP server at the instance's base_url, authenticated like REST (crate::mcp_client::external).
        return connectors
            .external_mcp()
            .call(
                connectors,
                &target,
                &auth,
                &secrets,
                &op_key(&op.op_ref()),
                &req,
                timeout,
            )
            .await;
    }
    if kind == TransportKind::Mcp {
        let mcp = connectors.mcp().ok_or_else(|| {
            transport::misconfigured("MCP_URL is not set: MCP systems cannot be called")
        })?;
        return mcp
            .call(target.tenant, &op_key(&op.op_ref()), &req, timeout)
            .await;
    }
    let fixtures = fixtures_of(op);
    let opts = CallOptions {
        kind,
        timeout,
        stub_fixtures: &fixtures,
        side_effect: op.side_effect,
    };
    transport::execute(connectors, &target, &auth, &secrets, req, opts).await
}

fn effect_of(e: &OpError) -> Effect {
    match e.code {
        "connector_auth_failed" => Effect::AuthFailed(e.detail.clone()),
        "invalid_target" | "invalid_url" => Effect::InvalidTarget(e.detail.clone()),
        _ => Effect::None,
    }
}

const AUTH_REFUSED: &str = "The system answered 401: the credential was refused.";

/// The verdict of a real call.
async fn called(
    st: &AppState,
    t: &TenantCtx,
    inst: &InstanceRow,
    tpl: &ConnectorTemplate,
    op: &ConnectorTemplateOp,
    secret: Option<&Map<String, Value>>,
) -> Verdict {
    match live_call(&st.connectors, inst, tpl, op, secret).await {
        Err(CallFailure::ProbeMissing(key)) => Verdict::new(
            "not_run",
            Some("probe_input_missing"),
            format!(
                "Set the env id \u{201c}{key}\u{201d} on this system to a test identifier, then run again."
            ),
        ),
        Err(CallFailure::Op(e)) => {
            let mut v = Verdict::new("red", Some(e.code), e.detail.clone());
            v.effect = effect_of(&e);
            v
        }
        Ok(resp) if resp.status == 401 => {
            let mut v = Verdict::new("red", Some("connector_auth_failed"), AUTH_REFUSED);
            v.effect = Effect::AuthFailed(AUTH_REFUSED.to_owned());
            v
        }
        Ok(resp) => match mapping::extract(op, &BTreeMap::new(), &resp) {
            Ok(_) => {
                let note = match op.side_effect {
                    SideEffect::R => "read probe with your test identifiers".to_owned(),
                    _ if inst.env == "prod" => format!(
                        "tagged agent-contract-test, left open; prod write OK by {}",
                        t.caller.actor
                    ),
                    _ => "tagged agent-contract-test in the test env, left open".to_owned(),
                };
                let mut v = Verdict::new("green", None, note);
                v.response = storable(&resp.body, MAX_STORED_RESPONSE);
                v
            }
            Err(e) => Verdict::new("red", Some(e.code), e.detail),
        },
    }
}

/// The instance env a result is recorded under (`all` → `prod`: the result column has no `all`).
fn result_env(inst: &InstanceRow) -> String {
    let env = if inst.env == "test" { "test" } else { "prod" };
    env.to_owned()
}

/// `runContractTest`: T3 for one op on this instance's env; answers the `ContractResult`. `audited` = false for the
/// re-runs of a re-pin (the re-pin's own row covers them).
pub async fn run(
    st: &AppState,
    t: &TenantCtx,
    id: &str,
    op: &str,
    prod_write_ok: bool,
    audited: bool,
) -> ApiResult<Value> {
    let mut tx = t.begin(&st.db).await?;
    let inst = instances::require(&mut tx, id).await?;
    let tpl = instances::pinned_template(&mut tx, &inst).await?;
    let mut top = find_op(&tpl, op)?;
    // B7 T12: a contract run maps exactly as the runtime would serve this instance: a `test` or `all` instance
    // through the working overrides (the test env), a `prod` instance through none (no job, no release snapshot).
    let run_env = if inst.env == "prod" { "prod" } else { "test" };
    let ovr = overrides::for_env(&mut tx, id, run_env, op).await?;
    top.field_map = overrides::effective_field_map(&top.field_map, &ovr);
    let decided = decide(&inst, &tpl, &top, prod_write_ok);
    let mut secret = if decided.is_none() && tpl.auth_kind.needs_stored_credential() {
        custody::open_credential(&mut tx, st.tenant_keyring.as_deref(), t.tenant, id).await?
    } else {
        None
    };
    tx.commit().await?;
    // A licence bearer is minted per call, after the transaction is closed (the licence service is remote).
    if decided.is_none() && tpl.auth_kind == AuthKind::LicenceBearer {
        let token = crate::connectors::runtime::mint_licence_token(st, t.tenant)
            .await
            .map_err(|e| ApiError::BadGateway(e.code, e.detail))?;
        secret = Some(Map::from_iter([(
            "licence_token".to_owned(),
            Value::String(token),
        )]));
    }

    let v = match decided {
        Some(v) => v,
        None => called(st, t, &inst, &tpl, &top, secret.as_ref()).await,
    };

    let at = v.stamped.then(Utc::now);
    let row = NewResult {
        op: op.to_owned(),
        env: result_env(&inst),
        side_effect: top.side_effect.as_str(),
        outcome: v.outcome,
        code: v.code,
        note: v.note.clone(),
        at,
        ran_by: t.caller.sub.clone(),
        response: v.response.clone(),
    };
    let mut tx = t.begin(&st.db).await?;
    instances::upsert_result(&mut tx, t.tenant, id, &row).await?;
    match &v.effect {
        Effect::AuthFailed(msg) => {
            instances::set_failed_status(&mut tx, id, "auth_failed", "connector_auth_failed", msg)
                .await?;
        }
        Effect::InvalidTarget(msg) => {
            instances::set_failed_status(&mut tx, id, "invalid_target", "invalid_target", msg)
                .await?;
        }
        Effect::None if v.outcome == "green" => {
            sqlx::query(
                "UPDATE connector_instance SET status = 'ok', updated_at = now() \
                  WHERE id = $1 AND status = 'untested'",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        Effect::None => {}
    }
    if audited {
        audit::record(
            &mut tx,
            &t.actor(),
            Event::new("connector.contract_test.run", "connector_instance", id)
                .detail("op", op)
                .detail("outcome", v.outcome)
                .detail("code", v.code),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(instances::result_json(&ResultRow {
        instance_id: id.to_owned(),
        op: op.to_owned(),
        side_effect: top.side_effect.as_str().to_owned(),
        outcome: v.outcome.to_owned(),
        code: v.code.map(str::to_owned),
        note: v.note,
        at,
    }))
}

/// Where a sample came from (audit detail).
enum Sample {
    Ready(Value, &'static str),
    Live(Option<Map<String, Value>>),
}

async fn mark_auth_failed(st: &AppState, t: &TenantCtx, id: &str, msg: &str) -> ApiResult<()> {
    let mut tx = t.begin(&st.db).await?;
    instances::set_failed_status(&mut tx, id, "auth_failed", "connector_auth_failed", msg).await?;
    tx.commit().await?;
    Ok(())
}

/// The sample of a raw response body: first array item / object / ≤2 000-char text, capped for storage. Secrets are
/// scrubbed first, so the text fallback and a contract response stored before the scrub never carry them either.
fn sample_body(body: &Value) -> Value {
    let body = netguard::scrub_secrets(body.clone());
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    let (sample, _) = netguard::sample_of(&bytes);
    storable(&sample, MAX_SAMPLE).unwrap_or_else(|| {
        Value::String(
            String::from_utf8_lossy(&bytes)
                .chars()
                .take(netguard::SAMPLE_CHARS)
                .collect(),
        )
    })
}

/// `fetchSampleResponse`: R op → live call; stub → its fixture; W/X/P → the last stored contract response, else
/// the template's first fixture, else 409 `sample_unavailable`. Never a live write to get a sample.
pub async fn sample(st: &AppState, t: &TenantCtx, id: &str, op: &str) -> ApiResult<Value> {
    let mut tx = t.begin(&st.db).await?;
    let inst = instances::require(&mut tx, id).await?;
    let tpl = instances::pinned_template(&mut tx, &inst).await?;
    // `none` and a licence bearer have no stored credential to set (platform MCP / a none-auth REST or GraphQL
    // system / a licence minted per call)
    if !inst.platform
        && !inst.stub
        && tpl.auth_kind.needs_stored_credential()
        && !inst.has_credential()
    {
        return Err(ApiError::Conflict(
            "connector_auth_failed",
            "Set the credential before fetching a sample from the system.".into(),
        ));
    }
    if inst.status == "auth_failed" {
        let msg = inst
            .last_error
            .as_ref()
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("Authentication failed.")
            .to_owned();
        return Err(ApiError::BadGateway("connector_auth_failed", msg));
    }
    let top = find_op(&tpl, op)?;
    let plan = if inst.stub {
        let resp = stub::replay_sample(&fixtures_of(&top))
            .map_err(|e| ApiError::Conflict("sample_unavailable", e.detail))?;
        Sample::Ready(sample_body(&resp.body), "stub")
    } else if top.side_effect != SideEffect::R {
        match instances::stored_response(&mut tx, id, op).await? {
            Some(body) => Sample::Ready(sample_body(&body), "contract"),
            None => match top.fixtures.first() {
                Some(f) => Sample::Ready(sample_body(&f.body), "fixture"),
                None => {
                    return Err(ApiError::Conflict(
                        "sample_unavailable",
                        format!(
                            "{op} writes to the system: run its contract test first; a sample is never fetched with a live write."
                        ),
                    ));
                }
            },
        }
    } else if tpl.auth_kind.needs_stored_credential() {
        Sample::Live(
            custody::open_credential(&mut tx, st.tenant_keyring.as_deref(), t.tenant, id).await?,
        )
    } else {
        Sample::Live(None)
    };
    tx.commit().await?;

    let (body, source) = match plan {
        Sample::Ready(body, source) => (body, source),
        Sample::Live(secret) => {
            // A licence bearer has no stored credential: mint it here, with no transaction open.
            let secret = if tpl.auth_kind == AuthKind::LicenceBearer {
                let token = crate::connectors::runtime::mint_licence_token(st, t.tenant)
                    .await
                    .map_err(|e| ApiError::BadGateway(e.code, e.detail))?;
                Some(Map::from_iter([(
                    "licence_token".to_owned(),
                    Value::String(token),
                )]))
            } else {
                secret
            };
            match live_call(&st.connectors, &inst, &tpl, &top, secret.as_ref()).await {
                Err(CallFailure::ProbeMissing(key)) => {
                    return Err(ApiError::Conflict(
                        "sample_unavailable",
                        format!(
                            "Set the env id \u{201c}{key}\u{201d} on this system to a test identifier first."
                        ),
                    ));
                }
                Err(CallFailure::Op(e)) => {
                    return Err(match e.code {
                        "invalid_target" | "invalid_url" => {
                            ApiError::Unprocessable("invalid_target", e.detail)
                        }
                        "connector_auth_failed" => {
                            mark_auth_failed(st, t, id, &e.detail).await?;
                            ApiError::BadGateway("connector_auth_failed", e.detail)
                        }
                        code => ApiError::BadGateway(code, e.detail),
                    });
                }
                Ok(resp) if resp.status == 401 => {
                    mark_auth_failed(st, t, id, AUTH_REFUSED).await?;
                    return Err(ApiError::BadGateway(
                        "connector_auth_failed",
                        AUTH_REFUSED.into(),
                    ));
                }
                Ok(resp) if !(200..300).contains(&resp.status) => {
                    let e = mapping::extract(&top, &BTreeMap::new(), &resp)
                        .err()
                        .unwrap_or_else(|| {
                            OpError::new("upstream_unavailable", format!("http_{}", resp.status))
                        });
                    return Err(ApiError::BadGateway(e.code, e.detail));
                }
                Ok(resp) => (sample_body(&resp.body), "live"),
            }
        }
    };

    let mut tx = t.begin(&st.db).await?;
    let fetched_at = instances::upsert_sample(&mut tx, t.tenant, id, op, &body).await?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new("connector.sample.fetch", "connector_instance", id)
            .detail("op", op)
            .detail("source", source),
    )
    .await?;
    tx.commit().await?;
    Ok(json!({"op": op, "fetched_at": fetched_at, "body": body}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::model::Idempotency;

    fn op(side: SideEffect, path: &str, request: &str) -> ConnectorTemplateOp {
        ConnectorTemplateOp {
            input_types: Default::default(),
            object: "case".into(),
            verb: "create".into(),
            variant: "*".into(),
            side_effect: side,
            op_kind: Default::default(),
            method: "POST".into(),
            path: path.into(),
            request: request.into(),
            field_map: vec![],
            status_map: vec![],
            fault_map: vec![],
            pagination: None,
            idempotency: Idempotency::Nat,
            verify_read: None,
            timeout_ms: 1000,
            fixtures: vec![],
        }
    }

    #[test]
    fn probe_inputs_come_from_env_ids_and_writes_are_tagged() {
        let env: Map<String, Value> =
            serde_json::from_value(json!({"probe.case_id": "C-1"})).unwrap();
        let read = op(SideEffect::R, "/cases/{{canonical.case_id}}", "");
        assert_eq!(probe_inputs(&read, &env).unwrap()["case_id"], "C-1");
        let missing = op(SideEffect::R, "/orders/{{canonical.id}}", "");
        assert_eq!(probe_inputs(&missing, &env).unwrap_err(), "probe.id");
        let write = op(
            SideEffect::W,
            "/cases",
            r#"{"subject":"{{canonical.subject}}"}"#,
        );
        let got = probe_inputs(&write, &env).unwrap();
        assert!(got["subject"].as_str().unwrap().starts_with(TEST_TAG));
    }

    #[test]
    fn stored_responses_and_samples_never_carry_vendor_secrets() {
        let body = json!([{
            "stdout": "USERNAME:jdoe\nPASSWORD:Xy7-not-real\nSTATUS:SUCCESS",
            "password": "Xy7-not-real",
        }]);
        let stored = storable(&body, MAX_STORED_RESPONSE).unwrap();
        let sample = sample_body(&body);
        for v in [&stored, &sample] {
            assert!(!v.to_string().contains("Xy7-not-real"), "{v}");
        }
        assert_eq!(
            sample["stdout"],
            "USERNAME:jdoe\nPASSWORD:[redacted]\nSTATUS:SUCCESS"
        );
    }
}
