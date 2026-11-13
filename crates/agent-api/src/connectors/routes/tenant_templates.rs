//! Tenant-private connector templates (B7 T11, D31/D69): a tenant admin configures a standard REST connector
//! (base `generic_rest@2`) themselves, validates it against the operation catalogue, contract-tests it (tenant T1),
//! activates it, and then pins a system to it (`POST /api/connections/systems` with the tenant template's key).
//! Shapes = `web/src/lib/api/types/connectors.ts` (`TenantTemplatePage`, `TenantConnectorTemplate`,
//! `TenantTemplateUsage`, `ValidationError[]`, `OpDefLite[]`); refusals = the plan's Route table and the fixture
//! (`web/src/lib/api/areas/connectors.ts`).
//!
//! - `GET /api/connections/templates` (member) → `TenantTemplatePage`: active, draft, retired; key; newest first.
//! - `GET /api/connections/templates/usage` (member) → `TenantTemplateUsage`. `routes` is `[]` until job routes exist
//!   (0094, B7 T13).
//! - `GET /api/operations?fields=io` (member) → `OpDefLite[]` (crate::connectors::tenant_templates::op_defs). Any
//!   other `fields` → 422 `invalid_input`.
//! - `POST /api/connections/templates` (integrations_manager) → 201 draft `@1`. 422 `invalid_input`; 409
//!   `template_exists` (a platform key, or a key the tenant already has).
//! - `PUT /api/connections/templates/{key}/{version}` (integrations_manager) `{label, description,
//!   credential_schema, ops, auth_params?}` → the template; validation and contract reset. 404 `template_not_found`;
//!   409 `template_immutable` (not a draft); 422 `invalid_input`.
//! - `POST …/{key}/{version}/validate` (integrations_manager) → `ValidationError[]` (the TS rules plus the server
//!   checks `secret_in_url`, `host_not_from_instance`), stored on the version.
//! - `POST …/{key}/{version}/contract-tests` (integrations_manager) → `{at, status, passed, total}` (tenant T1, see
//!   crate::connectors::tenant_templates::t1_op). 409 `validation_required`, `validation_failed`.
//! - `POST …/{key}/{version}/status` `{status}` (integrations_manager) → the template. Active: a draft with a clean
//!   validation and a green contract (409 `template_immutable`, `validation_required`, `contract_tests_required`);
//!   retired: 409 `no_change`, `template_in_use` (a system pins it).
//! - `POST …/{key}/{version}/drafts` (integrations_manager) → 201 the next version as a draft copy. 409
//!   `draft_exists`.
//!
//! D69: create, save, status and drafts are design writes — each runs its row change inside B2's
//! `design::track_tenant` (area `connections`, page `T12`, kind `tenant_template`, ref `{key, version}`): one ledger
//! row per job of the tenant and every job's `draft_rev` bumped, in the same transaction; its audit row carries
//! `draft: "tenant_template"`. Validate and contract-tests write result columns only (no ledger row). Every write
//! records one audit row.
use std::collections::BTreeMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::Utc;
use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::audit::{self, Event};
use crate::connectors::instances as inst_db;
use crate::connectors::model::{CredentialField, TemplateStatus, TenantTemplateOp};
use crate::connectors::places::{self, NewTemplate};
use crate::connectors::tenant_templates::{self as db, TemplateRow};
use crate::connectors::validate::validate_tenant_template_server;
use crate::design::{self, DraftWrite, Label};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::page::paged;
use crate::state::AppState;
use crate::tenancy::{Member, Requires, TenantCtx, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/connections/templates",
            get(list_templates).post(create_template),
        )
        .route("/api/connections/templates/usage", get(template_usage))
        .route(
            "/api/connections/templates/{key}/{version}",
            put(save_template),
        )
        .route(
            "/api/connections/templates/{key}/{version}/validate",
            post(validate_template),
        )
        .route(
            "/api/connections/templates/{key}/{version}/contract-tests",
            post(run_contract_tests),
        )
        .route(
            "/api/connections/templates/{key}/{version}/status",
            post(set_status),
        )
        .route(
            "/api/connections/templates/{key}/{version}/drafts",
            post(draft_version),
        )
        .route("/api/operations", get(op_defs))
}

/// The D69 ledger kind of every write here.
const KIND: &str = "tenant_template";
/// The owning page of a tenant template write (ledger `page`).
const PAGE: &str = "T12";

#[derive(Deserialize)]
struct KeyVersion {
    key: String,
    version: String,
}

#[derive(Deserialize)]
struct FieldsQuery {
    #[serde(default)]
    fields: Option<String>,
}

#[derive(Deserialize)]
struct CreateBody {
    key: String,
    #[serde(default)]
    label: String,
    kind: String,
    /// `rest` (default) | `mcp`.
    #[serde(default)]
    transport: Option<String>,
    auth_kind: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    auth_params: BTreeMap<String, String>,
    /// Operation keys (`object.verb`) that run as durable MCP tasks.
    #[serde(default)]
    task_tools: Vec<String>,
}

#[derive(Deserialize)]
struct SaveBody {
    #[serde(default)]
    label: String,
    #[serde(default)]
    description: String,
    credential_schema: Vec<CredentialField>,
    ops: Vec<TenantTemplateOp>,
    /// Absent = keep the stored ones.
    #[serde(default)]
    auth_params: Option<BTreeMap<String, String>>,
    /// Absent = keep the stored ones.
    #[serde(default)]
    task_tools: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct StatusBody {
    status: String,
}

fn invalid(msg: impl Into<String>) -> ApiError {
    ApiError::Unprocessable("invalid_input", msg.into())
}

fn to_value<T: serde::Serialize>(v: &T) -> ApiResult<Value> {
    serde_json::to_value(v).map_err(|e| ApiError::Internal(anyhow::anyhow!("to JSON: {e}")))
}

/// A design write of `key@version`: `mutate` (the row change) inside B2's `design::track_tenant`, then the audit
/// row (marked `draft`), in the caller's transaction.
#[allow(clippy::too_many_arguments)]
pub(super) async fn design_write<F>(
    conn: &mut PgConnection,
    st: &AppState,
    t: &TenantCtx,
    key: &str,
    version: &str,
    label: String,
    e: Event,
    mutate: F,
) -> ApiResult<()>
where
    F: for<'c> FnOnce(&'c mut PgConnection) -> BoxFuture<'c, ApiResult<()>> + Send,
{
    let w = DraftWrite {
        area: places::AREA_CONNECTIONS,
        page: PAGE,
        kind: KIND,
        r#ref: places::template_ref(key, version),
        label: Label::Now(label),
    };
    design::track_tenant(&mut *conn, &st.design, t, w, mutate).await?;
    audit::record(conn, &t.actor(), e.detail("draft", KIND)).await?;
    Ok(())
}

/// The audit resource of every write here.
const RESOURCE: &str = "tenant_connector_template";

/// `e` naming `key` and `version` apart (`key@1` would read as an email address to the audit scrub).
pub(super) fn on(e: Event, key: &str, version: &str) -> Event {
    e.detail("key", key).detail("version", version)
}

async fn list_templates(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let rows = db::list(&mut tx).await?;
    let mut subs: Vec<String> = rows.iter().map(|r| r.created_by.clone()).collect();
    subs.sort();
    subs.dedup();
    let names = inst_db::names(&mut tx, &subs).await?;
    tx.commit().await?;
    let mut page = paged(rows.iter().map(TemplateRow::json).collect(), None);
    page["names"] = Value::Object(names);
    Ok(Json(page))
}

async fn template_usage(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let pins = db::pins(&mut tx).await?;
    let platform = inst_db::platform_templates(&mut tx).await?;
    tx.commit().await?;
    let pins: Vec<Value> = pins
        .iter()
        .map(|p| {
            json!({
                "key": p.template_key,
                "version": p.template_version,
                "instance_id": p.id,
                "label": p.label,
                "env": p.env,
                "status": p.status,
            })
        })
        .collect();
    let platform: Vec<Value> = platform
        .iter()
        .filter(|x| !x.platform_connector && x.status != TemplateStatus::Retired)
        .map(|x| json!({"key": x.key, "version": x.version, "label": x.label, "vendor": x.vendor}))
        .collect();
    Ok(Json(json!({
        "pins": pins,
        // job routes (0094, B7 T13) do not exist yet: no route serves a system
        "routes": [],
        "platform": platform,
    })))
}

async fn op_defs(Member(_): Member, ApiQuery(q): ApiQuery<FieldsQuery>) -> ApiResult<Json<Value>> {
    if q.fields.as_deref() != Some("io") {
        return Err(invalid(
            "This catalogue read answers fields=io only (operation inputs and outputs).",
        ));
    }
    Ok(Json(to_value(&db::op_defs())?))
}

async fn create_template(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiJson(v): ApiJson<CreateBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if !db::valid_key(&v.key) {
        return Err(invalid(
            "A key is 3\u{2013}41 lower-case letters, digits or \u{201c}_\u{201d}, starting with a letter.",
        ));
    }
    if !matches!(v.kind.as_str(), "generic" | "custom") {
        return Err(invalid("kind is generic or custom."));
    }
    if !db::AUTH_KINDS.contains(&v.auth_kind.as_str()) {
        return Err(invalid(format!(
            "auth_kind is one of {}.",
            db::AUTH_KINDS.join(", ")
        )));
    }
    let transport = v.transport.as_deref().unwrap_or("rest").to_owned();
    if !matches!(transport.as_str(), "rest" | "mcp") {
        return Err(invalid("transport is rest or mcp."));
    }
    db::check_auth_params(&v.auth_params).map_err(invalid)?;
    db::check_task_tools(&v.task_tools, &[]).map_err(invalid)?;
    let mut tx = t.begin(&st.db).await?;
    if db::platform_key_exists(&mut tx, &v.key).await? {
        return Err(ApiError::Conflict(
            "template_exists",
            format!(
                "\u{201c}{}\u{201d} is a platform template key; pick another.",
                v.key
            ),
        ));
    }
    if !db::versions_of(&mut tx, &v.key).await?.is_empty() {
        return Err(ApiError::Conflict(
            "template_exists",
            format!(
                "You already have a template \u{201c}{}\u{201d}. Start a new version of it instead.",
                v.key
            ),
        ));
    }
    let label = match v.label.trim() {
        "" => v.key.clone(),
        l => l.to_owned(),
    };
    let row = NewTemplate {
        tenant: t.tenant,
        key: v.key.clone(),
        version: "1".to_owned(),
        label,
        kind: v.kind.clone(),
        transport,
        auth_kind: v.auth_kind.clone(),
        auth_params: to_value(&v.auth_params)?,
        credential_schema: to_value(&db::default_schema(&v.auth_kind))?,
        task_tools: v.task_tools.clone(),
        description: v.description.trim().to_owned(),
        created_by: t.caller.sub.clone(),
    };
    let e = on(
        Event::new("connector.tenant_template.create", RESOURCE, &v.key),
        &v.key,
        "1",
    )
    .detail("kind", v.kind.as_str())
    .detail("auth_kind", v.auth_kind.as_str());
    let label = format!("New template {}@1", v.key);
    design_write(&mut tx, &st, &t, &v.key, "1", label, e, move |c| {
        Box::pin(async move { places::insert_template(c, &row).await })
    })
    .await?;
    let out = db::require(&mut tx, &v.key, "1").await?.json();
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

fn require_draft(row: &TemplateRow) -> ApiResult<()> {
    if row.status != "draft" {
        return Err(ApiError::Conflict(
            "template_immutable",
            format!(
                "{}@{} is {}: a published version never changes. Start a new draft version.",
                row.key, row.version, row.status
            ),
        ));
    }
    Ok(())
}

async fn save_template(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<KeyVersion>,
    ApiJson(b): ApiJson<SaveBody>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let row = db::require(&mut tx, &p.key, &p.version).await?;
    require_draft(&row)?;
    db::check_schema(&b.credential_schema).map_err(invalid)?;
    let ops = db::check_ops(b.ops).map_err(invalid)?;
    let auth_params = match &b.auth_params {
        Some(params) => {
            db::check_auth_params(params).map_err(invalid)?;
            to_value(params)?
        }
        None => row.auth_params.clone(),
    };
    let task_tools = match b.task_tools {
        Some(tools) => {
            db::check_task_tools(&tools, &ops).map_err(invalid)?;
            tools
        }
        None => row.task_tools.clone(),
    };
    let label = match b.label.trim() {
        "" => row.label.clone(),
        l => l.to_owned(),
    };
    let e = on(
        Event::new("connector.tenant_template.save", RESOURCE, &row.key),
        &row.key,
        &row.version,
    )
    .detail("ops", ops.len());
    let schema = to_value(&b.credential_schema)?;
    let (key, version, tenant) = (row.key.clone(), row.version.clone(), t.tenant);
    let description = b.description.clone();
    let text = format!("Save {}@{}", row.key, row.version);
    design_write(
        &mut tx,
        &st,
        &t,
        &row.key,
        &row.version,
        text,
        e,
        move |c| {
            Box::pin(async move {
                places::update_template(
                    &mut *c,
                    &key,
                    &version,
                    &label,
                    &description,
                    &schema,
                    &auth_params,
                    &task_tools,
                )
                .await?;
                places::replace_ops(&mut *c, tenant, &key, &version, &ops).await
            })
        },
    )
    .await?;
    let out = db::require(&mut tx, &row.key, &row.version).await?.json();
    tx.commit().await?;
    Ok(Json(out))
}

async fn validate_template(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<KeyVersion>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let row = db::require(&mut tx, &p.key, &p.version).await?;
    let model = row.model()?;
    // the platform catalogue plus the template's own op definitions (0246)
    let issues = validate_tenant_template_server(&model, &db::defs_with_own(&model.ops));
    let errors = to_value(&issues)?;
    let validation = json!({"at": Utc::now(), "errors": errors});
    // a failed validation voids the last contract run (the fixture)
    let contract = if issues.is_empty() {
        row.contract.clone()
    } else {
        None
    };
    db::set_results(
        &mut tx,
        &row.key,
        &row.version,
        Some(&validation),
        contract.as_ref(),
    )
    .await?;
    audit::record(
        &mut tx,
        &t.actor(),
        on(
            Event::new("connector.tenant_template.validate", RESOURCE, &row.key),
            &row.key,
            &row.version,
        )
        .detail("errors", issues.len()),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(errors))
}

async fn run_contract_tests(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<KeyVersion>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let row = db::require(&mut tx, &p.key, &p.version).await?;
    match row.validation_errors() {
        None => {
            return Err(ApiError::Conflict(
                "validation_required",
                "Validate the template first.".into(),
            ));
        }
        Some(n) if n > 0 => {
            return Err(ApiError::Conflict(
                "validation_failed",
                format!("Fix the {n} validation error(s) before running contract tests."),
            ));
        }
        Some(_) => {}
    }
    // T1 renders and maps locally: no upstream call, so one transaction holds the read and the write.
    let recorded = db::recorded_responses(&mut tx, &row.key).await?;
    let model = row.model()?;
    let (passed, total, failures) = db::run_t1(&model, &db::defs_with_own(&model.ops), &recorded);
    let status = if total > 0 && passed == total {
        "green"
    } else {
        "red"
    };
    let contract = json!({"at": Utc::now(), "status": status, "passed": passed, "total": total});
    db::set_results(
        &mut tx,
        &row.key,
        &row.version,
        row.validation.as_ref(),
        Some(&contract),
    )
    .await?;
    audit::record(
        &mut tx,
        &t.actor(),
        on(
            Event::new(
                "connector.tenant_template.contract_test",
                RESOURCE,
                &row.key,
            ),
            &row.key,
            &row.version,
        )
        .detail("status", status)
        .detail("passed", passed)
        .detail("total", total)
        .detail("failures", failures),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(contract))
}

async fn set_status(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<KeyVersion>,
    ApiJson(b): ApiJson<StatusBody>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let row = db::require(&mut tx, &p.key, &p.version).await?;
    match b.status.as_str() {
        "active" => {
            if row.status != "draft" {
                return Err(ApiError::Conflict(
                    "template_immutable",
                    format!("{}@{} is {}.", row.key, row.version, row.status),
                ));
            }
            if row.validation_errors() != Some(0) {
                return Err(ApiError::Conflict(
                    "validation_required",
                    "Validate the template against the operation catalogue first.".into(),
                ));
            }
            if !row.contract_green() {
                return Err(ApiError::Conflict(
                    "contract_tests_required",
                    "Contract tests are mandatory before a template may serve a route.".into(),
                ));
            }
        }
        "retired" => {
            if row.status == "retired" {
                return Err(ApiError::Conflict("no_change", "Already retired.".into()));
            }
            let users = db::pinned_by(&mut tx, &row.key, &row.version).await?;
            if !users.is_empty() {
                return Err(ApiError::Conflict(
                    "template_in_use",
                    format!(
                        "Pinned by {}. Re-pin those systems first.",
                        users.join(", ")
                    ),
                ));
            }
        }
        _ => return Err(invalid("status is active or retired.")),
    }
    let e = on(
        Event::new("connector.tenant_template.status", RESOURCE, &row.key),
        &row.key,
        &row.version,
    )
    .detail("from", row.status.as_str())
    .detail("to", b.status.as_str());
    let (key, version, status) = (row.key.clone(), row.version.clone(), b.status.clone());
    let text = format!("{}@{} {}", row.key, row.version, b.status);
    design_write(
        &mut tx,
        &st,
        &t,
        &row.key,
        &row.version,
        text,
        e,
        move |c| {
            Box::pin(async move { places::set_template_status(c, &key, &version, &status).await })
        },
    )
    .await?;
    let out = db::require(&mut tx, &row.key, &row.version).await?.json();
    tx.commit().await?;
    Ok(Json(out))
}

async fn draft_version(
    Requires(t, _): Requires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<KeyVersion>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let mut tx = t.begin(&st.db).await?;
    let from = db::require(&mut tx, &p.key, &p.version).await?;
    let all = db::versions_of(&mut tx, &from.key).await?;
    if all.iter().any(|(_, s)| s == "draft") {
        return Err(ApiError::Conflict(
            "draft_exists",
            format!("{} already has a draft version; edit that one.", from.key),
        ));
    }
    let existing: Vec<String> = all.into_iter().map(|(v, _)| v).collect();
    let version = db::next_version(&existing, &from.version);
    let row = NewTemplate {
        tenant: t.tenant,
        key: from.key.clone(),
        version: version.clone(),
        label: from.label.clone(),
        kind: from.kind.clone(),
        transport: from.transport.clone(),
        auth_kind: from.auth_kind.clone(),
        auth_params: from.auth_params.clone(),
        credential_schema: from.credential_schema.clone(),
        task_tools: from.task_tools.clone(),
        description: from.description.clone(),
        created_by: t.caller.sub.clone(),
    };
    let ops = from.model()?.ops;
    let e = on(
        Event::new("connector.tenant_template.draft", RESOURCE, &from.key),
        &from.key,
        &version,
    )
    .detail("from", from.version.as_str());
    let text = format!("New draft {}@{version} from {}", from.key, from.version);
    design_write(&mut tx, &st, &t, &from.key, &version, text, e, move |c| {
        Box::pin(async move {
            places::insert_template(&mut *c, &row).await?;
            places::replace_ops(&mut *c, row.tenant, &row.key, &row.version, &ops).await
        })
    })
    .await?;
    let out = db::require(&mut tx, &from.key, &version).await?.json();
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}
