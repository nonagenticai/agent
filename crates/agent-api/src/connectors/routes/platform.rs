//! Platform connector templates (B7 T7), super_admin only. Shapes = `web/src/lib/api/types/connectors.ts`; refusals =
//! the plan's Route table and the fixture (`web/src/lib/api/areas/connectors.ts`).
//!
//! Reads:
//! - `GET /api/platform/connector-templates` → `Paged<ConnectorTemplateSummary>` (the template minus `ops`, plus
//!   `implements` and `instances`).
//! - `GET /api/platform/connector-templates/coverage` → `ConnectorTemplateOverview` (crate::connectors::coverage).
//! - `GET /api/platform/connector-templates/{key}/{version}` → `ConnectorTemplateDetail`. 404 `template_not_found`.
//! - `GET …/{key}/{version}/usage` → `ConnectorTemplateUsage`: `previous`, `latest_active`, every tenant instance on
//!   any version of the key. `routed_ops` / `overrides` come from the T13 seam (empty until 0094). 404.
//!
//! Writes (each one transaction as `agent_platform`, crate::platform_db, with one platform audit row):
//! - `POST /api/platform/connector-templates` `{key, version?="1", label, transport, auth_kind, vendor?, system_class?,
//!   description?, wave?, platform_connector?, alt_transport?, auth_detail?, auth_params?, credential_schema?,
//!   rate_limit_model?, vendor_api_pin?, webhooks?, ops?}` → 201 the new DRAFT (no content is compiled in: the platform
//!   offering, `generic_rest` base included, is rows a super_admin creates here, then ops/publish like any draft).
//!   409 `template_exists` (key@version), `draft_exists` (the key has a draft); 422 `invalid_input` (and the
//!   validator's codes when `ops` are given).
//! - `PUT …/{key}/{version}/ops/{op}` (`{op}` = the URL-encoded `object.verb{variant}` of the body) → the template;
//!   drafts only, resets `t1` to `not_run`. 404; 409 `template_immutable`; 422 `invalid_input`,
//!   `idempotency_required`.
//! - `POST …/{key}/{version}/drafts` → 201 the next version (TS `nextVersion`) as a draft copy. 404; 409
//!   `draft_exists`.
//! - `POST …/{key}/{version}/publish` → the template, `active`: the validator (crate::connectors::validate) must pass
//!   (422 `no_operations`, `idempotency_required`, else `invalid_input`, each with `errors`), then the T1 replay
//!   (crate::connectors::t1) must pass (422 `t1_failed` with `failures`; the red `t1` is kept). 404; 409
//!   `template_immutable`.
//! - `POST …/{key}/{version}/t2-runs` → `TierResult` (crate::connectors::t2), stored as `t2`. 404; 409
//!   `not_applicable` (platform connector), `sandbox_not_configured` (no `CONNECTOR_SANDBOXES` entry). No
//!   transaction is held across the sandbox calls.
//! - `POST …/{key}/{version}/retire` → the template, `retired`: it leaves the tenant catalogue and a new connection
//!   on it is 409 `template_retired`; instances that pin it keep working. `…/restore` → `active` again. 404; 409
//!   `template_immutable` (a draft: delete it by not publishing), `no_change`.
//!
//! Cross-tenant pins (`instances`, `pinned_by`, usage) are one read of the `agent_platform` definers
//! `agent_connector_pins()` / `agent_tenant_connector_bases()` (0152, crate::connectors::coverage::tenant_rows).
use std::collections::BTreeMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use crate::audit::{self, Actor, PlatformEvent};
use crate::connectors::coverage::{self, Base, OverviewInput, Pin};
use crate::connectors::instances::{self as db, cmp_version};
use crate::connectors::model::{
    ConnectorTemplate, ConnectorTemplateOp, CredentialField, Idempotency, TemplateStatus, Transport,
};
use crate::connectors::opkey::op_key;
use crate::connectors::validate::{Issue, validate_template};
use crate::connectors::wire::AuthKind;
use crate::connectors::{t1, t2, tenant_templates};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::page::paged;
use crate::platform_db::begin_platform;
use crate::state::AppState;
use crate::tenancy::SuperAdmin;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/platform/connector-templates",
            get(list_connector_templates),
        )
        .route(
            "/api/platform/connector-templates",
            post(create_connector_template),
        )
        .route(
            "/api/platform/connector-templates/coverage",
            get(connector_template_overview),
        )
        .route(
            "/api/platform/connector-templates/{key}/{version}",
            get(connector_template),
        )
        .route(
            "/api/platform/connector-templates/{key}/{version}/usage",
            get(connector_template_usage),
        )
        .route(
            "/api/platform/connector-templates/{key}/{version}/ops/{op}",
            put(save_connector_template_op),
        )
        .route(
            "/api/platform/connector-templates/{key}/{version}/drafts",
            post(draft_connector_template_version),
        )
        .route(
            "/api/platform/connector-templates/{key}/{version}/publish",
            post(publish_connector_template),
        )
        .route(
            "/api/platform/connector-templates/{key}/{version}/t2-runs",
            post(run_template_sandbox),
        )
        .route(
            "/api/platform/connector-templates/{key}/{version}/retire",
            post(retire_connector_template),
        )
        .route(
            "/api/platform/connector-templates/{key}/{version}/restore",
            post(restore_connector_template),
        )
}

#[derive(Deserialize)]
struct KeyVersion {
    key: String,
    version: String,
}

#[derive(Deserialize)]
struct KeyVersionOp {
    key: String,
    version: String,
    op: String,
}

/// The `t1` / `t2` value of a version that has not been run.
const NOT_RUN: &str = r#"{"status": "not_run", "at": null}"#;

fn to_json<T: serde::Serialize>(t: &T) -> ApiResult<Value> {
    serde_json::to_value(t)
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("connector template to JSON: {e}")))
}

fn invalid(msg: impl Into<String>) -> ApiError {
    ApiError::Unprocessable("invalid_input", msg.into())
}

/// `e` naming `key` and `version` apart (`key@1` would read as an email address to the audit scrub).
fn on(e: PlatformEvent, key: &str, version: &str) -> PlatformEvent {
    e.detail("key", key).detail("version", version)
}

/// Every tenant's platform-scope instances and tenant templates (one read-only transaction).
async fn tenant_rows(st: &AppState) -> ApiResult<(Vec<Pin>, Vec<Base>)> {
    let mut tx = st.db.begin().await?;
    let rows = coverage::tenant_rows(&mut tx).await?;
    tx.commit().await?;
    Ok(rows)
}

fn pinned<'p>(pins: &'p [Pin], key: &str, version: &str) -> impl Iterator<Item = &'p Pin> {
    pins.iter()
        .filter(move |p| p.key == key && p.version == version)
}

async fn require_template(
    conn: &mut PgConnection,
    key: &str,
    version: &str,
) -> ApiResult<ConnectorTemplate> {
    db::platform_template(conn, key, version)
        .await?
        .ok_or_else(|| db::template_not_found(key, version))
}

/// Locks `key@version` for the rest of the transaction; its status, else 404.
async fn lock_status(conn: &mut PgConnection, key: &str, version: &str) -> ApiResult<String> {
    let status: Option<String> = sqlx::query_scalar(
        "SELECT status FROM connector_template WHERE key = $1 AND version = $2 FOR UPDATE",
    )
    .bind(key)
    .bind(version)
    .fetch_optional(&mut *conn)
    .await?;
    status.ok_or_else(|| db::template_not_found(key, version))
}

fn require_draft(key: &str, version: &str, status: &str) -> ApiResult<()> {
    if status != "draft" {
        return Err(ApiError::Conflict(
            "template_immutable",
            format!(
                "{key}@{version} is {status}: a published version never changes. Start a new draft version."
            ),
        ));
    }
    Ok(())
}

// ---- reads ------------------------------------------------------------------------------------------------------

async fn list_connector_templates(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let mut conn = st.db.acquire().await?;
    let tpls = db::platform_templates(&mut conn).await?;
    drop(conn);
    let (pins, _) = tenant_rows(&st).await?;
    let items = tpls
        .iter()
        .map(|t| {
            Ok(coverage::summary(
                &to_json(t)?,
                pinned(&pins, &t.key, &t.version).count(),
            ))
        })
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Json(paged(items, None)))
}

async fn connector_template_overview(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let mut conn = st.db.acquire().await?;
    let tpls = db::platform_templates(&mut conn).await?;
    let head = coverage::head_catalogue(&mut conn).await?;
    drop(conn);
    let (pins, bases) = tenant_rows(&st).await?;
    let input = OverviewInput {
        templates: tpls.iter().map(to_json).collect::<ApiResult<Vec<_>>>()?,
        canonical_states: head.canonical_states,
        pins,
        bases,
        catalogue: head.catalogue,
        jobs: head.jobs,
    };
    Ok(Json(coverage::overview(&input)))
}

async fn connector_template(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<KeyVersion>,
) -> ApiResult<Json<Value>> {
    let mut conn = st.db.acquire().await?;
    let t = require_template(&mut conn, &p.key, &p.version).await?;
    let mut versions = db::versions_of(&mut conn, &p.key).await?;
    drop(conn);
    versions.sort_by(|a, b| cmp_version(&b.0, &a.0));
    let versions: Vec<Value> = versions
        .into_iter()
        .map(|(version, status)| json!({"version": version, "status": status}))
        .collect();
    let (pins, _) = tenant_rows(&st).await?;
    let pinned_by: Vec<Value> = pinned(&pins, &p.key, &p.version)
        .map(|i| json!({"tenant": i.tenant, "instance_id": i.instance_id, "env": i.env}))
        .collect();
    // Platform subjects have no tenant member row to name them from: a subject names itself.
    let mut names = Map::new();
    if let Some(sub) = t.published_by.as_ref().filter(|s| !s.is_empty()) {
        names.insert(sub.clone(), Value::String(sub.clone()));
    }
    Ok(Json(json!({
        "template": to_json(&t)?,
        "versions": versions,
        "pinned_by": pinned_by,
        "names": names,
    })))
}

async fn connector_template_usage(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<KeyVersion>,
) -> ApiResult<Json<Value>> {
    let mut conn = st.db.acquire().await?;
    require_template(&mut conn, &p.key, &p.version).await?;
    let mut lineage: Vec<ConnectorTemplate> = db::platform_templates(&mut conn)
        .await?
        .into_iter()
        .filter(|t| t.key == p.key)
        .collect();
    drop(conn);
    lineage.sort_by(|a, b| cmp_version(&b.version, &a.version));
    let at = lineage.iter().position(|t| t.version == p.version);
    let previous = at
        .and_then(|i| lineage.get(i + 1))
        .map(to_json)
        .transpose()?;
    let latest_active = lineage
        .iter()
        .find(|t| t.status == TemplateStatus::Active)
        .map(to_json)
        .transpose()?;
    let (pins, _) = tenant_rows(&st).await?;
    let mut conn = st.db.acquire().await?;
    let mut instances = Vec::new();
    for i in pins.iter().filter(|i| i.key == p.key) {
        let (routed_ops, overrides) =
            coverage::routed_and_overrides(&mut conn, &i.tenant_id, &i.instance_id).await?;
        instances.push(json!({
            "tenant_id": i.tenant_id,
            "tenant": i.tenant,
            "instance_id": i.instance_id,
            "label": i.label,
            "env": i.env,
            "pinned_version": i.version,
            "status": i.status,
            "routed_ops": routed_ops,
            "overrides": overrides,
        }));
    }
    Ok(Json(json!({
        "previous": previous,
        "latest_active": latest_active,
        "instances": instances,
    })))
}

// ---- writes -----------------------------------------------------------------------------------------------------

/// The request checks of a saved op (the fixture's, plus the column rules of 0091).
fn check_op(path_op: &str, op: &ConnectorTemplateOp) -> ApiResult<()> {
    if op.object.trim().is_empty() || op.verb.trim().is_empty() || op.variant.trim().is_empty() {
        return Err(invalid("An operation needs an object and a verb."));
    }
    let key = op_key(&op.op_ref());
    if key != path_op {
        return Err(invalid(format!(
            "The body is {key}, the path names {path_op}."
        )));
    }
    if op.path.trim().is_empty() {
        return Err(invalid("The request path is empty."));
    }
    if op.method.trim().is_empty() {
        return Err(invalid("The request method is empty."));
    }
    if !(100..=30_000).contains(&op.timeout_ms) {
        return Err(invalid("timeout_ms is between 100 and 30000."));
    }
    if op.side_effect.needs_key() && op.idempotency == Idempotency::None {
        return Err(ApiError::Unprocessable(
            "idempotency_required",
            format!(
                "{key} is {}: idempotency \u{201c}none\u{201d} is refused for W/X/P operations.",
                op.side_effect.as_str()
            ),
        ));
    }
    Ok(())
}

const UPSERT_OP: &str = "\
INSERT INTO connector_template_op (key, version, object, verb, variant, side_effect, op_kind, method, path, request,
        field_map, status_map, fault_map, pagination, idempotency, verify_read, timeout_ms, fixtures, input_types)
SELECT $1, $2, o ->> 'object', o ->> 'verb', o ->> 'variant', o ->> 'side_effect', o ->> 'op_kind', o ->> 'method',
       o ->> 'path', o ->> 'request', o -> 'field_map', o -> 'status_map', o -> 'fault_map', o ->> 'pagination',
       o -> 'idempotency', o ->> 'verify_read', (o ->> 'timeout_ms')::integer, o -> 'fixtures',
       coalesce(o -> 'input_types', '{}'::jsonb)
  FROM (SELECT $3::jsonb AS o) AS s
    ON CONFLICT (key, version, object, verb, variant) DO UPDATE
   SET side_effect = EXCLUDED.side_effect, op_kind = EXCLUDED.op_kind, method = EXCLUDED.method,
       path = EXCLUDED.path, request = EXCLUDED.request, field_map = EXCLUDED.field_map,
       status_map = EXCLUDED.status_map, fault_map = EXCLUDED.fault_map, pagination = EXCLUDED.pagination,
       idempotency = EXCLUDED.idempotency, verify_read = EXCLUDED.verify_read, timeout_ms = EXCLUDED.timeout_ms,
       fixtures = EXCLUDED.fixtures, input_types = EXCLUDED.input_types";

async fn save_connector_template_op(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<KeyVersionOp>,
    ApiJson(op): ApiJson<ConnectorTemplateOp>,
) -> ApiResult<Json<Value>> {
    let mut tx = begin_platform(&st.db).await?;
    let status = lock_status(&mut tx, &p.key, &p.version).await?;
    require_draft(&p.key, &p.version, &status)?;
    check_op(&p.op, &op)?;
    sqlx::query(UPSERT_OP)
        .bind(&p.key)
        .bind(&p.version)
        .bind(to_json(&op)?)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE connector_template SET t1 = $3::jsonb WHERE key = $1 AND version = $2")
        .bind(&p.key)
        .bind(&p.version)
        .bind(NOT_RUN)
        .execute(&mut *tx)
        .await?;
    audit::record_platform(
        &mut tx,
        &Actor::person(&c),
        on(
            PlatformEvent::new("connector_template.op_save", &p.key),
            &p.key,
            &p.version,
        )
        .detail("op", p.op.as_str()),
    )
    .await?;
    let out = require_template(&mut tx, &p.key, &p.version).await?;
    tx.commit().await?;
    Ok(Json(to_json(&out)?))
}

fn one() -> String {
    "1".to_owned()
}

fn empty_object() -> Value {
    json!({})
}

fn empty_array() -> Value {
    json!([])
}

#[derive(Deserialize)]
struct CreateBody {
    key: String,
    #[serde(default = "one")]
    version: String,
    label: String,
    #[serde(default)]
    vendor: String,
    #[serde(default)]
    system_class: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    wave: String,
    #[serde(default)]
    platform_connector: bool,
    transport: Transport,
    #[serde(default)]
    alt_transport: Option<Transport>,
    auth_kind: AuthKind,
    #[serde(default)]
    auth_detail: String,
    #[serde(default)]
    auth_params: BTreeMap<String, String>,
    /// Absent = the default schema of the auth kind (as a tenant template starts with).
    #[serde(default)]
    credential_schema: Option<Vec<CredentialField>>,
    #[serde(default = "empty_object")]
    rate_limit_model: Value,
    #[serde(default)]
    vendor_api_pin: String,
    #[serde(default = "empty_array")]
    webhooks: Value,
    #[serde(default)]
    ops: Vec<ConnectorTemplateOp>,
}

async fn create_connector_template(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiJson(v): ApiJson<CreateBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if !tenant_templates::valid_key(&v.key) {
        return Err(invalid(
            "A key is 3\u{2013}41 lower-case letters, digits or \u{201c}_\u{201d}, starting with a letter.",
        ));
    }
    if v.version.trim().is_empty() || v.version.chars().count() > 64 {
        return Err(invalid("A version is 1 to 64 characters."));
    }
    if v.label.trim().is_empty() {
        return Err(invalid("A template needs a label."));
    }
    if !v.rate_limit_model.is_object() {
        return Err(invalid("rate_limit_model is an object."));
    }
    if !v.webhooks.is_array() {
        return Err(invalid("webhooks is an array."));
    }
    let schema = v
        .credential_schema
        .unwrap_or_else(|| tenant_templates::default_schema(auth_kind_str(v.auth_kind)));
    tenant_templates::check_schema(&schema).map_err(invalid)?;
    for op in &v.ops {
        check_op(&op_key(&op.op_ref()), op)?;
    }
    for (i, op) in v.ops.iter().enumerate() {
        if v.ops[..i].iter().any(|o| o.op_ref() == op.op_ref()) {
            return Err(invalid(format!(
                "{} is listed twice.",
                op_key(&op.op_ref())
            )));
        }
    }
    let label = v.label.trim().to_owned();
    let candidate = ConnectorTemplate {
        key: v.key.clone(),
        version: v.version.clone(),
        label: label.clone(),
        vendor: v.vendor.clone(),
        system_class: v.system_class.clone(),
        description: v.description.clone(),
        wave: v.wave.clone(),
        platform_connector: v.platform_connector,
        transport: v.transport,
        alt_transport: v.alt_transport,
        auth_kind: v.auth_kind,
        auth_detail: v.auth_detail.clone(),
        auth_params: v.auth_params.clone(),
        credential_schema: schema.clone(),
        rate_limit_model: v.rate_limit_model.clone(),
        vendor_api_pin: v.vendor_api_pin.clone(),
        webhooks: v.webhooks.clone(),
        status: TemplateStatus::Draft,
        t1: Value::Null,
        t2: Value::Null,
        published_by: None,
        published_at: None,
        drift: Value::Null,
        ops: v.ops.clone(),
        // A platform template runs the global `MCP_TASK_TOOLS`; task tools are a tenant template's declaration.
        task_tools: Vec::new(),
    };
    // A draft may be saved unfinished: the validator runs on create only when operations are given (an empty
    // template is refused at publish, `no_operations`; `generic_rest` is exempt).
    if !v.ops.is_empty() {
        let issues = validate_template(&candidate, tenant_templates::op_defs());
        if !issues.is_empty() {
            return Err(validation_refusal(&candidate, &issues)?);
        }
    }
    let mut tx = begin_platform(&st.db).await?;
    let existing = db::versions_of(&mut tx, &v.key).await?;
    if existing.iter().any(|(ver, _)| *ver == v.version) {
        return Err(ApiError::Conflict(
            "template_exists",
            format!("{}@{} already exists.", v.key, v.version),
        ));
    }
    if existing.iter().any(|(_, s)| s == "draft") {
        return Err(ApiError::Conflict(
            "draft_exists",
            format!("{} already has a draft version; edit that one.", v.key),
        ));
    }
    let name = |t: Transport| -> ApiResult<String> {
        Ok(to_json(&t)?.as_str().unwrap_or_default().to_owned())
    };
    let transport = name(v.transport)?;
    let alt_transport = v.alt_transport.map(name).transpose()?;
    let credential_schema = to_json(&schema)?;
    let auth_params = to_json(&v.auth_params)?;
    let inserted = sqlx::query(
        "INSERT INTO connector_template (key, version, label, vendor, system_class, description, wave, \
                platform_connector, transport, alt_transport, auth_kind, auth_detail, auth_params, credential_schema, \
                rate_limit_model, vendor_api_pin, webhooks, status) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, 'draft') \
         ON CONFLICT (key, version) DO NOTHING",
    )
    .bind(&v.key)
    .bind(&v.version)
    .bind(&label)
    .bind(&v.vendor)
    .bind(&v.system_class)
    .bind(&v.description)
    .bind(&v.wave)
    .bind(v.platform_connector)
    .bind(transport)
    .bind(alt_transport)
    .bind(auth_kind_str(v.auth_kind))
    .bind(&v.auth_detail)
    .bind(&auth_params)
    .bind(&credential_schema)
    .bind(&v.rate_limit_model)
    .bind(&v.vendor_api_pin)
    .bind(&v.webhooks)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(ApiError::Conflict(
            "template_exists",
            format!("{}@{} already exists.", v.key, v.version),
        ));
    }
    for op in &v.ops {
        sqlx::query(UPSERT_OP)
            .bind(&v.key)
            .bind(&v.version)
            .bind(to_json(op)?)
            .execute(&mut *tx)
            .await?;
    }
    audit::record_platform(
        &mut tx,
        &Actor::person(&c),
        on(
            PlatformEvent::new("connector_template.create", &v.key),
            &v.key,
            &v.version,
        )
        .detail("ops", v.ops.len()),
    )
    .await?;
    let out = require_template(&mut tx, &v.key, &v.version).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(to_json(&out)?)))
}

/// The `auth_kind` column literal of `k` (the serde name).
fn auth_kind_str(k: AuthKind) -> &'static str {
    match k {
        AuthKind::None => "none",
        AuthKind::ApiKeyHeader => "api_key_header",
        AuthKind::Basic => "basic",
        AuthKind::BearerStatic => "bearer_static",
        AuthKind::Oauth2ClientCredentials => "oauth2_client_credentials",
        AuthKind::BundleToken => "bundle_token",
        AuthKind::LicenceBearer => "licence_bearer",
        AuthKind::Mtls => "mtls",
    }
}

async fn draft_connector_template_version(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<KeyVersion>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let mut tx = begin_platform(&st.db).await?;
    lock_status(&mut tx, &p.key, &p.version).await?;
    let all = db::versions_of(&mut tx, &p.key).await?;
    if all.iter().any(|(_, s)| s == "draft") {
        return Err(ApiError::Conflict(
            "draft_exists",
            format!("{} already has a draft version; edit that one.", p.key),
        ));
    }
    let existing: Vec<String> = all.into_iter().map(|(v, _)| v).collect();
    let version = tenant_templates::next_version(&existing, &p.version);
    sqlx::query(
        "INSERT INTO connector_template (key, version, label, vendor, system_class, description, wave, \
                platform_connector, transport, alt_transport, auth_kind, auth_detail, auth_params, credential_schema, \
                rate_limit_model, vendor_api_pin, webhooks, status) \
         SELECT key, $3, label, vendor, system_class, description, wave, platform_connector, transport, \
                alt_transport, auth_kind, auth_detail, auth_params, credential_schema, rate_limit_model, \
                vendor_api_pin, webhooks, 'draft' \
           FROM connector_template WHERE key = $1 AND version = $2",
    )
    .bind(&p.key)
    .bind(&p.version)
    .bind(&version)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO connector_template_op (key, version, object, verb, variant, side_effect, op_kind, method, path, \
                request, field_map, status_map, fault_map, pagination, idempotency, verify_read, timeout_ms, fixtures, \
                input_types) \
         SELECT key, $3, object, verb, variant, side_effect, op_kind, method, path, request, field_map, status_map, \
                fault_map, pagination, idempotency, verify_read, timeout_ms, fixtures, input_types \
           FROM connector_template_op WHERE key = $1 AND version = $2",
    )
    .bind(&p.key)
    .bind(&p.version)
    .bind(&version)
    .execute(&mut *tx)
    .await?;
    audit::record_platform(
        &mut tx,
        &Actor::person(&c),
        on(
            PlatformEvent::new("connector_template.draft", &p.key),
            &p.key,
            &version,
        )
        .detail("from", p.version.as_str()),
    )
    .await?;
    let out = require_template(&mut tx, &p.key, &version).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(to_json(&out)?)))
}

/// The 422 of a template the validator refuses: `no_operations`, else `idempotency_required`, else
/// `invalid_input`; every finding in `errors`.
fn validation_refusal(t: &ConnectorTemplate, issues: &[Issue]) -> ApiResult<ApiError> {
    let has = |code: &str| issues.iter().any(|i| i.code == code);
    let err = if has("no_operations") {
        ApiError::Unprocessable(
            "no_operations",
            "A template needs at least one operation before it is published.".into(),
        )
    } else if has("idempotency_required") {
        let bad: Vec<String> = t
            .ops
            .iter()
            .filter(|o| o.side_effect.needs_key() && o.idempotency == Idempotency::None)
            .map(|o| op_key(&o.op_ref()))
            .collect();
        ApiError::Unprocessable(
            "idempotency_required",
            format!(
                "Idempotency \u{201c}none\u{201d} is refused for W/X/P: {}. Declare a header or a dup_search first.",
                bad.join(", ")
            ),
        )
    } else {
        invalid(format!(
            "The template does not validate: {} finding(s).",
            issues.len()
        ))
    };
    Ok(err.with("errors", to_json(&issues)?))
}

async fn publish_connector_template(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<KeyVersion>,
) -> ApiResult<Json<Value>> {
    let mut tx = begin_platform(&st.db).await?;
    let status = lock_status(&mut tx, &p.key, &p.version).await?;
    require_draft(&p.key, &p.version, &status)?;
    let t = require_template(&mut tx, &p.key, &p.version).await?;
    let issues = validate_template(&t, tenant_templates::op_defs());
    if !issues.is_empty() {
        return Err(validation_refusal(&t, &issues)?);
    }
    let report = t1::replay(&t);
    let actor = Actor::person(&c);
    if !report.passed() {
        let note = format!(
            "{} of {} fixtures failed",
            report.failures.len(),
            report.replayed
        );
        let t1 = json!({"status": "red", "at": Utc::now(), "note": note});
        sqlx::query("UPDATE connector_template SET t1 = $3 WHERE key = $1 AND version = $2")
            .bind(&p.key)
            .bind(&p.version)
            .bind(&t1)
            .execute(&mut *tx)
            .await?;
        audit::record_platform(
            &mut tx,
            &actor,
            on(
                PlatformEvent::new("connector_template.publish", &p.key),
                &p.key,
                &p.version,
            )
            .failed("t1_failed")
            .detail("failures", report.failures.len()),
        )
        .await?;
        tx.commit().await?;
        return Err(ApiError::Unprocessable(
            "t1_failed",
            format!("T1 replay failed: {note}. Fix the mapping or the recorded fixtures."),
        )
        .with("failures", to_json(&report.failures)?));
    }
    let t1 = json!({
        "status": "green",
        "at": Utc::now(),
        "note": format!("{} fixtures replayed", report.replayed),
    });
    sqlx::query(
        "UPDATE connector_template SET status = 'active', published_by = $3, published_at = now(), t1 = $4 \
          WHERE key = $1 AND version = $2",
    )
    .bind(&p.key)
    .bind(&p.version)
    .bind(&c.sub)
    .bind(&t1)
    .execute(&mut *tx)
    .await?;
    audit::record_platform(
        &mut tx,
        &actor,
        on(
            PlatformEvent::new("connector_template.publish", &p.key),
            &p.key,
            &p.version,
        )
        .detail("fixtures", report.replayed),
    )
    .await?;
    let out = require_template(&mut tx, &p.key, &p.version).await?;
    tx.commit().await?;
    Ok(Json(to_json(&out)?))
}

async fn run_template_sandbox(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<KeyVersion>,
) -> ApiResult<Json<Value>> {
    let mut conn = st.db.acquire().await?;
    let t = require_template(&mut conn, &p.key, &p.version).await?;
    drop(conn);
    if t.platform_connector {
        return Err(ApiError::Conflict(
            "not_applicable",
            "Platform connectors are tested in agent CI, not in a vendor sandbox.".into(),
        ));
    }
    let Some(sb) = st.cfg.connector_sandboxes.get(&t.key) else {
        return Err(ApiError::Conflict(
            "sandbox_not_configured",
            format!(
                "No vendor sandbox is configured for {} (CONNECTOR_SANDBOXES).",
                t.key
            ),
        ));
    };
    // read at run time, so a rotated sandbox credential needs no restart; never logged or stored
    let credential = std::env::var(&sb.env).ok();
    let result = t2::run(&st.connectors, &t, sb, credential).await;
    let mut tx = begin_platform(&st.db).await?;
    sqlx::query("UPDATE connector_template SET t2 = $3 WHERE key = $1 AND version = $2")
        .bind(&p.key)
        .bind(&p.version)
        .bind(&result)
        .execute(&mut *tx)
        .await?;
    audit::record_platform(
        &mut tx,
        &Actor::person(&c),
        on(
            PlatformEvent::new("connector_template.t2_run", &p.key),
            &p.key,
            &p.version,
        )
        .detail("status", result["status"].clone()),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(result))
}

/// Retired ↔ active of a published version (the 0228 guard allows exactly these two moves).
async fn set_template_retired(
    c: &crate::auth::Caller,
    st: &AppState,
    p: KeyVersion,
    retire: bool,
) -> ApiResult<Json<Value>> {
    let mut tx = begin_platform(&st.db).await?;
    let status = lock_status(&mut tx, &p.key, &p.version).await?;
    let (from, to) = if retire {
        ("active", "retired")
    } else {
        ("retired", "active")
    };
    if status == to {
        return Err(ApiError::Conflict(
            "no_change",
            format!("{}@{} is already {to}.", p.key, p.version),
        ));
    }
    if status != from {
        return Err(ApiError::Conflict(
            "template_immutable",
            format!(
                "{}@{} is {status}: only a published version can be retired or restored.",
                p.key, p.version
            ),
        ));
    }
    sqlx::query("UPDATE connector_template SET status = $3 WHERE key = $1 AND version = $2")
        .bind(&p.key)
        .bind(&p.version)
        .bind(to)
        .execute(&mut *tx)
        .await?;
    audit::record_platform(
        &mut tx,
        &Actor::person(c),
        on(
            PlatformEvent::new(
                if retire {
                    "connector_template.retire"
                } else {
                    "connector_template.restore"
                },
                &p.key,
            ),
            &p.key,
            &p.version,
        ),
    )
    .await?;
    let out = require_template(&mut tx, &p.key, &p.version).await?;
    tx.commit().await?;
    Ok(Json(to_json(&out)?))
}

async fn retire_connector_template(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<KeyVersion>,
) -> ApiResult<Json<Value>> {
    set_template_retired(&c, &st, p, true).await
}

async fn restore_connector_template(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<KeyVersion>,
) -> ApiResult<Json<Value>> {
    set_template_retired(&c, &st, p, false).await
}
