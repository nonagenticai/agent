//! Tenant systems (B7 T9 + T10): connector instances with write-only credentials, contract tests and samples.
//! Shapes = `web/src/lib/api/types/connectors.ts`; refusals = the plan's Route table and the fixture
//! (`web/src/lib/api/areas/connectors.ts`).
//!
//! - `GET /api/connections/systems` (member) → `Paged<ConnectorInstance>`, prod, test, all, then id.
//! - `GET /api/connections/systems/{id}` (member) → `SystemDetail`. 404 `instance_not_found`, `template_not_found`.
//! - `GET /api/connections/catalogue` (member) → `Paged<SystemCatalogueRow>`: platform, non-platform-connector,
//!   non-retired templates, plus the tenant's own non-retired templates (`scope: "tenant"`, B7 T11). `implements_required` / `required_total` are 0 until job expected operations exist
//!   (B5/B6 `job_activity`); the `X-Job` header is accepted and not read yet.
//! - `POST /api/connections/systems` (integrations_manager) → 201 `ConnectorInstance`. 422 `invalid_input`,
//!   `stub_not_allowed_in_prod`, `invalid_url`, `invalid_target`; 409 `instance_exists`, `template_retired`,
//!   `platform_managed`; 404 `template_not_found`.
//! - `PATCH /api/connections/systems/{id}` (integrations_manager) → `ConnectorInstance` (label, base_url, env_ids,
//!   max_rps). 404; 409 `platform_managed`; 422 `invalid_url`, `invalid_target`, `invalid_input`.
//! - `PUT /api/connections/systems/{id}/credential` (integrations_manager), body = the field → value map →
//!   `{set, updated_at, fingerprint}`. 404; 409 `platform_managed`, `not_applicable` (stub); 422 `invalid_input`.
//! - `POST /api/connections/systems/{id}/repin` `{version}` (integrations_manager) → `ConnectorInstance`; results
//!   reset, R ops and every op that had a result re-run. 404; 409 `platform_managed`, `no_change`,
//!   `template_not_active`.
//! - `POST /api/connections/systems/{id}/contract-tests` `{op, prod_write_ok?}` (integrations_manager) →
//!   `ContractResult` (crate::connectors::contract). 404 `instance_not_found`, `operation_not_implemented`.
//! - `POST /api/connections/systems/{id}/samples` `{op}` (integrations_manager) → `{op, fetched_at, body}`.
//! - `PUT /api/connections/systems/{id}/mappings/{op}/{field}` `{path}` (integrations_manager) → `ConnectorInstance`
//!   with `overrides` = the working rows (B7 T12). 404 `instance_not_found`, `operation_not_implemented`; 422
//!   `unknown_field` (the op does not map that field), `path_no_match` (checked against the stored sample when it is
//!   of this op), `invalid_path` (else: must start with `$`). Draft write (D69): the row change runs inside B2's
//!   `design::track_tenant` (area `connections`, page `T11`, kind `mapping_override`, ref `{instance, op, field}`).
//! - `DELETE /api/connections/systems/{id}/mappings/{op}/{field}` (integrations_manager) → `ConnectorInstance`.
//!   404 `instance_not_found`, `override_not_found`. Draft write (as the save).
//! - `GET /api/connections/systems/{id}/insight` (member) → `SystemInsight` (crate::connectors::insight). 404.
//!
//! A tenant-scope instance (B7 T11) pins the tenant's own template: create resolves the platform template first,
//! else the tenant's; detail, re-pin, contract tests and samples read it through `instances::template_in_scope`.
//!
//! Every write records one audit row. Routes (`used_by`, 0094) are a later B7 task (T13): `used_by` is `[]`, and
//! `op_readiness` is decided from the template ops and the contract results as if each op were routed here.
use std::collections::BTreeMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::audit::{self, Event};
use crate::connectors::contract;
use crate::connectors::custody;
use crate::connectors::insight;
use crate::connectors::instances::{self as db, InstanceRow, cmp_version};
use crate::connectors::model::{ConnectorTemplate, SideEffect, TemplateStatus, Transport};
use crate::connectors::opkey::op_key;
use crate::connectors::overrides;
use crate::connectors::places;
use crate::connectors::tenant_templates;
use crate::design::{self, DraftWrite, Label};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::netguard;
use crate::page::paged;
use crate::state::AppState;
use crate::tenancy::{Member, Requires, TenantCtx, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/connections/systems",
            get(list_systems).post(create_system),
        )
        .route(
            "/api/connections/systems/{id}",
            get(system_detail).patch(update_system),
        )
        .route(
            "/api/connections/systems/{id}/credential",
            put(set_credential),
        )
        .route("/api/connections/systems/{id}/repin", post(repin_system))
        .route(
            "/api/connections/systems/{id}/contract-tests",
            post(run_contract_test),
        )
        .route("/api/connections/systems/{id}/samples", post(fetch_sample))
        .route(
            "/api/connections/systems/{id}/mappings/{op}/{field}",
            put(save_mapping_override).delete(revert_mapping_override),
        )
        .route("/api/connections/systems/{id}/insight", get(system_insight))
        .route("/api/connections/catalogue", get(system_catalogue))
}

#[derive(Deserialize)]
struct IdPath {
    id: String,
}

#[derive(Deserialize)]
struct MappingPath {
    id: String,
    op: String,
    field: String,
}

#[derive(Deserialize)]
struct OverrideBody {
    path: String,
}

#[derive(Deserialize)]
struct NewInstance {
    id: String,
    #[serde(default)]
    label: String,
    template_key: String,
    template_version: String,
    env: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    env_ids: BTreeMap<String, String>,
    #[serde(default)]
    max_rps: Option<f64>,
    #[serde(default)]
    stub: bool,
}

#[derive(Deserialize)]
struct RepinBody {
    version: String,
}

#[derive(Deserialize)]
struct ContractBody {
    op: String,
    #[serde(default)]
    prod_write_ok: bool,
}

#[derive(Deserialize)]
struct SampleBody {
    op: String,
}

fn invalid(msg: impl Into<String>) -> ApiError {
    ApiError::Unprocessable("invalid_input", msg.into())
}

fn platform_managed(msg: &str) -> ApiError {
    ApiError::Conflict("platform_managed", msg.into())
}

/// `max_rps`: empty, or a number in (0, 1000].
fn check_max_rps(v: Option<f64>) -> ApiResult<Option<f64>> {
    match v {
        Some(n) if !n.is_finite() || n <= 0.0 || n > 1000.0 => Err(invalid(
            "max_rps is between 1 and 1000, or empty for the template default.",
        )),
        other => Ok(other),
    }
}

/// A base URL through the server guard (crate::netguard::check_url): the UI's `invalid_url` / `invalid_target`.
/// Answers the stored form and whether the operator's private-target allow-list admitted it.
fn check_base_url(st: &AppState, t: &TenantCtx, raw: &str) -> ApiResult<(String, bool)> {
    let url = netguard::check_url(raw, st.connectors.policy(), t.tenant)
        .map_err(|e| ApiError::Unprocessable(e.code, e.detail))?;
    let private = st
        .connectors
        .policy()
        .private_target(&url, t.tenant)
        .is_some();
    Ok((raw.trim().trim_end_matches('/').to_owned(), private))
}

fn status_str(s: TemplateStatus) -> &'static str {
    match s {
        TemplateStatus::Draft => "draft",
        TemplateStatus::Active => "active",
        TemplateStatus::Retired => "retired",
    }
}

/// The instance, its results and its sample as `ConnectorInstance` JSON (in the open transaction).
async fn instance_value(conn: &mut sqlx::PgConnection, id: &str) -> ApiResult<Value> {
    let inst = db::require(conn, id).await?;
    let tpl = db::pinned_template(conn, &inst).await?;
    let results = db::results(conn, Some(id)).await?;
    let samples = db::samples(conn, Some(id)).await?;
    let ovr = overrides::working(conn, Some(id)).await?;
    Ok(db::instance_json(
        &inst,
        tpl.auth_kind,
        &results,
        &samples,
        &ovr,
    ))
}

/// D67 per-op readiness of one system (TS `opReadinessOf`).
fn op_readiness_of(outcome: &str, routed: bool, stub: bool, platform: bool) -> &'static str {
    if !routed {
        return "unrouted";
    }
    if stub {
        return "stub";
    }
    if platform {
        return "ready";
    }
    match outcome {
        "green" => "ready",
        "red" => "blocked",
        "needs_ok" => "needs_ok",
        "write_unverified" => "unverified",
        _ => "untested",
    }
}

async fn list_systems(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let rows = db::all(&mut tx).await?;
    let results = db::results(&mut tx, None).await?;
    let samples = db::samples(&mut tx, None).await?;
    let ovr = overrides::working(&mut tx, None).await?;
    // the list is small; each instance's pinned template carries the auth kind the payload reports
    let mut items = Vec::with_capacity(rows.len());
    for i in &rows {
        let tpl = db::pinned_template(&mut tx, i).await?;
        items.push(db::instance_json(
            i,
            tpl.auth_kind,
            &results,
            &samples,
            &ovr,
        ));
    }
    tx.commit().await?;
    Ok(Json(paged(items, None)))
}

async fn system_detail(
    Member(t): Member,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<IdPath>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let inst = db::require(&mut tx, &p.id).await?;
    let tpl = db::pinned_template(&mut tx, &inst).await?;
    let versions = if inst.template_scope == "tenant" {
        tenant_templates::versions_of(&mut tx, &inst.template_key).await?
    } else {
        db::versions_of(&mut tx, &inst.template_key).await?
    };
    let newest = versions
        .into_iter()
        .filter(|(_, status)| status == "active")
        .map(|(v, _)| v)
        .max_by(|a, b| cmp_version(a, b));
    let results = db::results(&mut tx, Some(&inst.id)).await?;
    let samples = db::samples(&mut tx, Some(&inst.id)).await?;
    let ovr = overrides::working(&mut tx, Some(&inst.id)).await?;
    let mut subs = vec![inst.created_by.clone()];
    subs.extend(ovr.iter().map(|o| o.updated_by.clone()));
    subs.sort();
    subs.dedup();
    let names = db::names(&mut tx, &subs).await?;
    tx.commit().await?;
    let newer_version =
        newest.filter(|v| cmp_version(v, &inst.template_version) == std::cmp::Ordering::Greater);
    let ops: Vec<Value> = tpl
        .ops
        .iter()
        .map(|o| {
            let field_map: Vec<Value> = o
                .field_map
                .iter()
                .map(|f| json!({"path": f.path, "field": f.field}))
                .collect();
            json!({
                "op": op_key(&o.op_ref()),
                "side_effect": o.side_effect.as_str(),
                "verify_read": o.verify_read,
                "field_map": field_map,
            })
        })
        .collect();
    let mut op_readiness = Map::new();
    for o in &tpl.ops {
        let key = op_key(&o.op_ref());
        let outcome = results
            .iter()
            .find(|r| r.op == key)
            .map_or("not_run", |r| r.outcome.as_str());
        let r = op_readiness_of(outcome, true, inst.stub, inst.platform);
        op_readiness.insert(key, Value::from(r));
    }
    Ok(Json(json!({
        "instance": db::instance_json(&inst, tpl.auth_kind, &results, &samples, &ovr),
        "template": {
            "label": tpl.label,
            "vendor": tpl.vendor,
            "status": status_str(tpl.status),
            "transport": tpl.transport,
            "auth_kind": tpl.auth_kind,
            "auth_detail": tpl.auth_detail,
            "credential_schema": tpl.credential_schema,
            "ops": ops,
        },
        "newer_version": newer_version,
        "used_by": [],
        "op_readiness": op_readiness,
        "names": names,
    })))
}

/// One catalogue row of a platform template or (`scope = "tenant"`) of the tenant's own template.
fn catalogue_row(t: &ConnectorTemplate, scope: &str) -> Value {
    let keys: Vec<String> = t.ops.iter().map(|o| op_key(&o.op_ref())).collect();
    json!({
        "key": t.key,
        "version": t.version,
        "label": t.label,
        "vendor": t.vendor,
        "scope": scope,
        "status": status_str(t.status),
        "transport": t.transport,
        "auth_kind": t.auth_kind,
        "credential_schema": t.credential_schema,
        "implements_required": 0,
        "required_total": 0,
        "implements_total": keys.len(),
        "stub_capable": t.transport == Transport::Stub || t.alt_transport == Some(Transport::Stub),
        "implements": keys,
    })
}

async fn system_catalogue(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let platform = db::platform_templates(&mut tx).await?;
    let own = tenant_templates::list(&mut tx).await?;
    tx.commit().await?;
    let mut tpls: Vec<(ConnectorTemplate, &str)> = platform
        .into_iter()
        .filter(|x| !x.platform_connector && x.status != TemplateStatus::Retired)
        .map(|x| (x, "platform"))
        .collect();
    // B7 T11: the tenant's own non-retired templates (TS vendor "<kind> · your template")
    for row in own.iter().filter(|r| r.status != "retired") {
        tpls.push((tenant_templates::as_connector_template(row)?, "tenant"));
    }
    // Every row implements 0 required ops until job expectations exist: active first, else the stored order.
    tpls.sort_by_key(|(x, _)| x.status != TemplateStatus::Active);
    let items = tpls
        .iter()
        .map(|(x, scope)| catalogue_row(x, scope))
        .collect();
    Ok(Json(paged(items, None)))
}

async fn create_system(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiJson(v): ApiJson<NewInstance>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if !db::valid_id(&v.id) {
        return Err(invalid(
            "An id is 3\u{2013}41 lower-case letters, digits, \u{201c}-\u{201d} or \u{201c}_\u{201d}, starting with a letter.",
        ));
    }
    if !matches!(v.env.as_str(), "prod" | "test") {
        return Err(invalid("env is prod or test."));
    }
    let max_rps = check_max_rps(v.max_rps)?;
    let mut tx = t.begin(&st.db).await?;
    if db::one(&mut tx, &v.id).await?.is_some() {
        return Err(ApiError::Conflict(
            "instance_exists",
            format!("A system with id \u{201c}{}\u{201d} already exists.", v.id),
        ));
    }
    // the platform template, else the tenant's own (B7 T11; the fixture's `plat ?? ten`)
    let (scope, tpl) = match db::platform_template(&mut tx, &v.template_key, &v.template_version)
        .await?
    {
        Some(p) => ("platform", p),
        None => {
            let own =
                db::template_in_scope(&mut tx, "tenant", &v.template_key, &v.template_version)
                    .await?
                    .ok_or_else(|| db::template_not_found(&v.template_key, &v.template_version))?;
            ("tenant", own)
        }
    };
    if tpl.status == TemplateStatus::Retired {
        return Err(ApiError::Conflict(
            "template_retired",
            format!("{}@{} is retired.", tpl.key, tpl.version),
        ));
    }
    if tpl.platform_connector {
        return Err(platform_managed(
            "Platform connectors are provisioned by Agent.",
        ));
    }
    if v.stub && v.env == "prod" {
        return Err(ApiError::Unprocessable(
            "stub_not_allowed_in_prod",
            "Stub instances are allowed on the test env only and never count for readiness.".into(),
        ));
    }
    let (base_url, private) = if v.stub {
        (format!("stub://{}", tpl.key), false)
    } else {
        check_base_url(&st, &t, &v.base_url)?
    };
    let label = match v.label.trim() {
        "" => v.id.clone(),
        l => l.to_owned(),
    };
    let status = if tpl.status == TemplateStatus::Draft {
        "draft_template"
    } else {
        "untested"
    };
    let env_ids = serde_json::to_value(&v.env_ids)
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("env_ids: {e}")))?;
    let inserted: Option<String> = sqlx::query_scalar(
        "INSERT INTO connector_instance \
                (tenant_id, id, label, template_scope, template_key, template_version, env, base_url, env_ids, \
                 max_rps, stub, platform, status, created_by) \
         VALUES ($1, $2, $3, $13, $4, $5, $6, $7, $8, $9::float8::numeric, $10, false, $11, $12) \
         ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(t.tenant)
    .bind(&v.id)
    .bind(&label)
    .bind(&tpl.key)
    .bind(&tpl.version)
    .bind(&v.env)
    .bind(&base_url)
    .bind(env_ids)
    .bind(max_rps)
    .bind(v.stub)
    .bind(status)
    .bind(&t.caller.sub)
    .bind(scope)
    .fetch_optional(&mut *tx)
    .await?;
    if inserted.is_none() {
        return Err(ApiError::Conflict(
            "instance_exists",
            format!("A system with id \u{201c}{}\u{201d} already exists.", v.id),
        ));
    }
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new("connector.instance.create", "connector_instance", &v.id)
            // key and version apart: `key@1.2` would read as an email address to the audit scrub
            .detail("template", tpl.key.as_str())
            .detail("version", tpl.version.as_str())
            .detail("scope", scope)
            .detail("env", v.env.as_str())
            .detail("stub", v.stub)
            .detail("private_target", private),
    )
    .await?;
    let out = instance_value(&mut tx, &v.id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

async fn update_system(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<IdPath>,
    ApiJson(patch): ApiJson<Map<String, Value>>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let inst = db::require(&mut tx, &p.id).await?;
    if inst.platform {
        return Err(platform_managed(
            "Platform connectors are provisioned by Agent.",
        ));
    }
    let mut label = inst.label.clone();
    let mut base_url = inst.base_url.clone();
    let mut env_ids = inst.env_ids.clone();
    let mut max_rps = inst.max_rps;
    let mut status = inst.status.clone();
    let mut private = false;
    let mut fields: Vec<String> = Vec::new();
    for (k, val) in &patch {
        match k.as_str() {
            "label" => {
                let s = val
                    .as_str()
                    .ok_or_else(|| invalid("label is text."))?
                    .trim();
                if !s.is_empty() {
                    label = s.to_owned();
                }
            }
            "base_url" => {
                let raw = val.as_str().ok_or_else(|| invalid("base_url is text."))?;
                if !inst.stub {
                    let (url, admitted) = check_base_url(&st, &t, raw)?;
                    base_url = url;
                    private = admitted;
                    if status == "invalid_target" {
                        status = "untested".to_owned();
                    }
                }
            }
            "env_ids" => {
                let ok = val
                    .as_object()
                    .is_some_and(|m| m.values().all(Value::is_string));
                if !ok {
                    return Err(invalid("env_ids is an object of text values."));
                }
                env_ids = val.clone();
            }
            "max_rps" => {
                let n = match val {
                    Value::Null => None,
                    Value::Number(n) => n.as_f64(),
                    _ => return Err(invalid("max_rps is a number or null.")),
                };
                max_rps = check_max_rps(n)?;
            }
            other => {
                return Err(invalid(format!(
                    "{other} cannot be changed here (label, base_url, env_ids, max_rps)."
                )));
            }
        }
        fields.push(k.clone());
    }
    sqlx::query(
        "UPDATE connector_instance \
            SET label = $2, base_url = $3, env_ids = $4, max_rps = $5::float8::numeric, status = $6, \
                updated_at = now() \
          WHERE id = $1",
    )
    .bind(&inst.id)
    .bind(&label)
    .bind(&base_url)
    .bind(&env_ids)
    .bind(max_rps)
    .bind(&status)
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new("connector.instance.update", "connector_instance", &inst.id)
            .detail("fields", fields)
            .detail("private_target", private),
    )
    .await?;
    let out = instance_value(&mut tx, &inst.id).await?;
    tx.commit().await?;
    Ok(Json(out))
}

async fn set_credential(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<IdPath>,
    ApiJson(values): ApiJson<BTreeMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let inst = db::require(&mut tx, &p.id).await?;
    if inst.platform {
        return Err(platform_managed(
            "Platform connectors use Agent-minted tokens; there is nothing to set.",
        ));
    }
    if inst.stub {
        return Err(ApiError::Conflict(
            "not_applicable",
            "Stub instances need no credential.".into(),
        ));
    }
    let tpl = db::pinned_template(&mut tx, &inst).await?;
    let schema = &tpl.credential_schema;
    let unknown: Vec<&str> = values
        .keys()
        .filter(|k| !schema.iter().any(|f| &f.name == *k))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(invalid(format!(
            "Not in this template's credential schema: {}.",
            unknown.join(", ")
        )));
    }
    let missing: Vec<&str> = schema
        .iter()
        .filter(|f| f.secret && !f.name.contains("webhook"))
        .filter(|f| values.get(&f.name).is_none_or(|v| v.trim().is_empty()))
        .map(|f| {
            if f.label.is_empty() {
                f.name.as_str()
            } else {
                f.label.as_str()
            }
        })
        .collect();
    if !missing.is_empty() {
        return Err(invalid(format!(
            "Required secret field(s) missing: {}.",
            missing.join(", ")
        )));
    }
    let mut secret = Map::new();
    let mut env_ids = inst.env_map();
    for f in schema {
        let Some(v) = values.get(&f.name) else {
            continue;
        };
        if f.secret {
            if !v.trim().is_empty() {
                secret.insert(f.name.clone(), Value::String(v.clone()));
            }
        } else if !v.trim().is_empty() {
            env_ids.insert(f.name.clone(), Value::String(v.trim().to_owned()));
        }
    }
    let sealed = custody::seal_credential(
        &mut tx,
        st.tenant_keyring.as_deref(),
        t.tenant,
        &inst.id,
        &secret,
        &t.caller.sub,
    )
    .await?;
    sqlx::query(
        "UPDATE connector_instance \
            SET env_ids = $2, \
                status = CASE WHEN status = 'auth_failed' THEN 'untested' ELSE status END, \
                last_error = CASE WHEN status = 'auth_failed' THEN NULL ELSE last_error END, \
                updated_at = now() \
          WHERE id = $1",
    )
    .bind(&inst.id)
    .bind(Value::Object(env_ids))
    .execute(&mut *tx)
    .await?;
    let updated_at: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT updated_at FROM connector_credential WHERE instance_id = $1")
            .bind(&inst.id)
            .fetch_one(&mut *tx)
            .await?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new("connector.credential.set", "connector_instance", &inst.id)
            .detail("instance", inst.id.as_str())
            .detail("credential_version", sealed.credential_version)
            .detail("fingerprint", sealed.fingerprint.as_str()),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "set": true,
        "updated_at": updated_at,
        "fingerprint": sealed.fingerprint,
    })))
}

async fn repin_system(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<IdPath>,
    ApiJson(b): ApiJson<RepinBody>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let inst: InstanceRow = db::require(&mut tx, &p.id).await?;
    if inst.platform {
        return Err(platform_managed(
            "Platform connectors are re-pinned by Agent releases.",
        ));
    }
    if b.version == inst.template_version {
        return Err(ApiError::Conflict(
            "no_change",
            format!("Already pinned to {}@{}.", inst.template_key, b.version),
        ));
    }
    let tpl = db::template_in_scope(
        &mut tx,
        &inst.template_scope,
        &inst.template_key,
        &b.version,
    )
    .await?
    .ok_or_else(|| db::template_not_found(&inst.template_key, &b.version))?;
    if tpl.status != TemplateStatus::Active {
        return Err(ApiError::Conflict(
            "template_not_active",
            format!(
                "{}@{} is {}; pin an active version.",
                tpl.key,
                tpl.version,
                status_str(tpl.status)
            ),
        ));
    }
    let had: Vec<String> = db::results(&mut tx, Some(&inst.id))
        .await?
        .into_iter()
        .map(|r| r.op)
        .collect();
    sqlx::query(
        "UPDATE connector_instance \
            SET template_version = $2, \
                status = CASE WHEN status = 'draft_template' THEN 'untested' ELSE status END, \
                updated_at = now() \
          WHERE id = $1",
    )
    .bind(&inst.id)
    .bind(&tpl.version)
    .execute(&mut *tx)
    .await?;
    // Re-pinning resets the contract results and re-runs them (doc 11 §6.1 pinning rule 4).
    sqlx::query("DELETE FROM connector_contract_result WHERE instance_id = $1")
        .bind(&inst.id)
        .execute(&mut *tx)
        .await?;
    let rerun: Vec<String> = tpl
        .ops
        .iter()
        .map(|o| (op_key(&o.op_ref()), o.side_effect))
        .filter(|(k, side)| *side == SideEffect::R || had.contains(k))
        .map(|(k, _)| k)
        .collect();
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new("connector.instance.repin", "connector_instance", &inst.id)
            .detail("from", inst.template_version.as_str())
            .detail("to", tpl.version.as_str())
            .detail("rerun", rerun.len()),
    )
    .await?;
    tx.commit().await?;
    // Each re-run reads, calls and writes on its own: no transaction is open across an upstream call.
    for op in &rerun {
        contract::run(&st, &t, &inst.id, op, false, false).await?;
    }
    let mut tx = t.begin(&st.db).await?;
    let out = instance_value(&mut tx, &inst.id).await?;
    tx.commit().await?;
    Ok(Json(out))
}

async fn run_contract_test(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<IdPath>,
    ApiJson(b): ApiJson<ContractBody>,
) -> ApiResult<Json<Value>> {
    let r = contract::run(&st, &t, &p.id, b.op.trim(), b.prod_write_ok, true).await?;
    Ok(Json(r))
}

async fn fetch_sample(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<IdPath>,
    ApiJson(b): ApiJson<SampleBody>,
) -> ApiResult<Json<Value>> {
    let s = contract::sample(&st, &t, &p.id, b.op.trim()).await?;
    Ok(Json(s))
}

/// The B2 draft write of one mapping override (D69, tenant-wide).
fn override_write(instance: &str, op: &str, field: &str, label: String) -> DraftWrite {
    // the ledger keeps labels to 500 characters (0044); paths and keys come from requests
    let label: String = label.chars().take(400).collect();
    DraftWrite {
        area: places::AREA_CONNECTIONS,
        page: "T11",
        kind: "mapping_override",
        r#ref: places::override_ref(instance, op, field),
        label: Label::Now(label),
    }
}

async fn save_mapping_override(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<MappingPath>,
    ApiJson(b): ApiJson<OverrideBody>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let inst = db::require(&mut tx, &p.id).await?;
    let tpl = db::pinned_template(&mut tx, &inst).await?;
    let op = tpl
        .ops
        .iter()
        .find(|o| op_key(&o.op_ref()) == p.op)
        .ok_or_else(|| {
            ApiError::Missing(
                "operation_not_implemented",
                format!("{}@{} does not implement {}.", tpl.key, tpl.version, p.op),
            )
        })?;
    let def = op
        .field_map
        .iter()
        .find(|f| f.field == p.field)
        .ok_or_else(|| {
            ApiError::Unprocessable(
                "unknown_field",
                format!("{} is not mapped by {}.", p.field, p.op),
            )
        })?;
    let samples = db::samples(&mut tx, Some(&inst.id)).await?;
    let sample = samples.first().map(|s| (s.op.as_str(), &s.body));
    let path = overrides::check_path(&b.path, sample, &p.op)?;
    // D69: a field mapping is design, saved to the draft of every job (live per job on publish).
    let w = override_write(
        &inst.id,
        &p.op,
        &p.field,
        format!("{} {} \u{2190} {path}", p.op, p.field),
    );
    let (id, op, field, default_path, by) = (
        inst.id.clone(),
        p.op.clone(),
        p.field.clone(),
        def.path.clone(),
        t.caller.sub.clone(),
    );
    design::track_tenant::<(), _>(&mut tx, &st.design, &t, w, move |c| {
        Box::pin(async move {
            places::write_override(c, &id, &op, &field, &default_path, &path, &by, None).await
        })
    })
    .await?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new(
            "connector.mapping_override.save",
            "connector_instance",
            &inst.id,
        )
        .detail("op", p.op.as_str())
        .detail("field", p.field.as_str())
        .detail("draft", "mapping_override"),
    )
    .await?;
    let out = instance_value(&mut tx, &inst.id).await?;
    tx.commit().await?;
    Ok(Json(out))
}

async fn revert_mapping_override(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<MappingPath>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let inst = db::require(&mut tx, &p.id).await?;
    let w = override_write(
        &inst.id,
        &p.op,
        &p.field,
        format!("{} {}: template default", p.op, p.field),
    );
    let (id, op, field) = (inst.id.clone(), p.op.clone(), p.field.clone());
    design::track_tenant::<(), _>(&mut tx, &st.design, &t, w, move |c| {
        Box::pin(async move {
            if places::delete_override(c, &id, &op, &field).await? {
                Ok(())
            } else {
                Err(ApiError::Missing(
                    "override_not_found",
                    "No override for that field.".into(),
                ))
            }
        })
    })
    .await?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new(
            "connector.mapping_override.revert",
            "connector_instance",
            &inst.id,
        )
        .detail("op", p.op.as_str())
        .detail("field", p.field.as_str())
        .detail("draft", "mapping_override"),
    )
    .await?;
    let out = instance_value(&mut tx, &inst.id).await?;
    tx.commit().await?;
    Ok(Json(out))
}

async fn system_insight(
    Member(t): Member,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<IdPath>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let out = insight::system_insight(&mut tx, &p.id).await?;
    tx.commit().await?;
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_matches_the_ts_rules() {
        assert_eq!(op_readiness_of("green", false, false, false), "unrouted");
        assert_eq!(op_readiness_of("green", true, true, false), "stub");
        assert_eq!(op_readiness_of("red", true, false, true), "ready");
        assert_eq!(op_readiness_of("red", true, false, false), "blocked");
        assert_eq!(
            op_readiness_of("write_unverified", true, false, false),
            "unverified"
        );
        assert_eq!(op_readiness_of("not_run", true, false, false), "untested");
        assert!(check_max_rps(Some(0.0)).is_err());
        assert!(check_max_rps(Some(1001.0)).is_err());
        assert_eq!(check_max_rps(None).unwrap(), None);
    }
}
