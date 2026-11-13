//! Tenant reads (B1 T10): the registry, the portfolio, one tenant and its users, jobs, licence, provisioning rows,
//! stored offboard result and footprint. After care-bus-agents@96c608c `routes/tenancy.rs` (plan 14 reads).
//!
//! - `GET /api/tenants[?include_archived=true][&view=portfolio]` — super_admin: `Paged<PlatformTenantRow>` (a superset
//!   of the picker's `TenantSummary`), or with `view=portfolio` a `TenantPortfolio[]`. Archived tenants are listed only
//!   with `include_archived=true`. One read across tenants (`agent_platform_tenant_rollup()`, as `agent_platform`).
//! - `GET /api/tenants/{id}` and `/users` — a super_admin, or a member of that tenant (`Tenant`, `Paged<TenantUser>`
//!   with disabled and invited members). A member passes licence gate 2 (crate::licensing::gate::check_request, as
//!   at `TenantCtx`); a super_admin's platform tenant detail is not gated.
//! - `GET /api/tenants/{id}/jobs|licence|provisioning|offboard|footprint` — super_admin (the platform tenant detail);
//!   `/offboard` is the stored result, or `null` for a tenant never offboarded (B1b T9).
//! - `PATCH /api/tenants/{id}` `{"status"?: "active"|"suspended", "name"?}` — super_admin: suspend / resume, and
//!   (DR, owner 2026-10-23) rename: 422 `invalid_name`, 409 `tenant_name_taken` (case-insensitive, archived
//!   tenants included), 404 `tenant_not_found`; audited `tenant.rename` in both logs. SW-7 (owner 2026-10-27): also
//!   `"support_auto_assign"?: bool`, the platform flag in front of support round-robin (super_admin only, OFF by
//!   default); audited `tenant.support_auto_assign`. The tenant answers carry `support_auto_assign`. SW-9 (owner
//!   2026-10-28): `"attachment_quota_bytes"?: int | null`, the tenant's attachment storage (1 B to 1 TiB; null = the
//!   platform default `ATTACHMENT_QUOTA_BYTES`; 422 `invalid_quota`); audited `tenant.attachment_quota`. The
//!   single-tenant answers carry `attachment_quota_bytes` and `attachment_quota_default_bytes`.
//! - `POST /api/tenants/{id}/offboard {confirm_name}` — super_admin, or the pinned secure-apps service account
//!   (`TENANT_OFFBOARD_SA_*`, crate::tenants::offboarder); the only route that service account opens.
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::audit::{self, Actor, PlatformEvent};
use crate::auth::{Caller, TenantStatus};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::identity::{PersonNames, person_name};
use crate::page::{flag, paged};
use crate::provision::{OutboxRow, ROW_COLUMNS};
use crate::readmodel::{self, platform, tenant::TenantRow};
use crate::state::AppState;
use crate::tenancy::{SuperAdmin, begin_scoped, tenant_id};
use crate::tenants::lifecycle::{self, CreateInput, FirstAdmin, Origin};
use crate::tenants::offboarder::Offboarder;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/tenants", get(list_tenants).post(create))
        .route("/api/tenants/import", post(import))
        .route(
            "/api/tenants/{tenant_id}",
            get(get_tenant).patch(patch_tenant),
        )
        .route(
            "/api/tenants/{tenant_id}/offboard",
            get(get_offboard).post(offboard),
        )
        .route("/api/tenants/{tenant_id}/users", get(list_users))
        .route(
            "/api/tenants/{tenant_id}/jobs",
            get(list_jobs).post(crate::routes::jobs::platform_create),
        )
        .route("/api/tenants/{tenant_id}/licence", get(get_licence))
        .route(
            "/api/tenants/{tenant_id}/provisioning",
            get(list_provisioning),
        )
        .route("/api/tenants/{tenant_id}/footprint", get(get_footprint))
}

/// The licence plan code (`LICENSE_PLAN_CODE`).
pub fn plan(st: &AppState) -> &str {
    &st.identity.cfg.licenses_admin.plan_code
}

#[derive(Deserialize)]
struct ListQuery {
    include_archived: Option<String>,
    view: Option<String>,
}

async fn list_tenants(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Value>> {
    let portfolio = match q.view.as_deref().map(str::trim) {
        None | Some("") => false,
        Some("portfolio") => true,
        Some(_) => {
            return Err(ApiError::Unprocessable(
                "invalid_view",
                "view must be portfolio or absent".into(),
            ));
        }
    };
    let archived = flag(q.include_archived.as_deref(), "include_archived")?;
    let rows: Vec<platform::RollupRow> = platform::rollup(&st.db)
        .await?
        .into_iter()
        .filter(|r| archived || r.t.status != "archived")
        .collect();
    if portfolio {
        // the request's one bus PING, the fact `GET /api/jobs/{job}/status` reports as `on_bus`
        let bus_up = readmodel::agent_status::bus_up(&st).await;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(platform::portfolio(&st.db, r, bus_up).await?);
        }
        return Ok(Json(Value::Array(out)));
    }
    let items = rows.iter().map(|r| r.registry_json(plan(&st))).collect();
    Ok(Json(paged(items, None)))
}

#[derive(Deserialize)]
struct TenantPath {
    tenant_id: String,
}

fn tenant_not_found(t: Uuid) -> ApiError {
    ApiError::Missing("tenant_not_found", format!("no tenant {t}"))
}

/// [`read_access`], then licence gate 2 for a member (`LICENSE_GATE_2_ENABLED`; its codes): a super_admin reads the
/// platform tenant detail ungated, like the other super_admin tenant routes.
async fn gated_read_access(st: &AppState, caller: &Caller, tenant: Uuid) -> ApiResult<()> {
    read_access(caller, tenant)?;
    if !caller.super_admin {
        crate::licensing::gate::check_request(st, tenant).await?;
    }
    Ok(())
}

/// A super_admin, or an (enabled) member of `tenant` whose tenant is active: else 403 (`not_provisioned`,
/// `forbidden`, `tenant_suspended`, `tenant_archived`).
pub fn read_access(caller: &Caller, tenant: Uuid) -> ApiResult<()> {
    if caller.super_admin {
        return Ok(());
    }
    let home = caller.home.as_ref().ok_or_else(|| {
        ApiError::Forbidden(
            "not_provisioned",
            "this account is not provisioned in any tenant".into(),
        )
    })?;
    if home.tenant != tenant {
        return Err(ApiError::Forbidden(
            "forbidden",
            "only a member of that tenant may read it".into(),
        ));
    }
    match home.status {
        TenantStatus::Active => Ok(()),
        TenantStatus::Suspended => Err(ApiError::Forbidden(
            "tenant_suspended",
            "this tenant is suspended".into(),
        )),
        TenantStatus::Archived => Err(ApiError::Forbidden(
            "tenant_archived",
            "this tenant is archived".into(),
        )),
    }
}

/// A transaction scoped to `tenant` and its row, else 404 `tenant_not_found`.
pub async fn scoped(
    st: &AppState,
    tenant: Uuid,
) -> ApiResult<(Transaction<'static, Postgres>, TenantRow)> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let row = readmodel::tenant::read(&mut tx, tenant)
        .await?
        .ok_or_else(|| tenant_not_found(tenant))?;
    Ok((tx, row))
}

async fn get_tenant(
    caller: Caller,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TenantPath>,
) -> ApiResult<Json<Value>> {
    let tenant = tenant_id(&p.tenant_id)?;
    gated_read_access(&st, &caller, tenant).await?;
    let (mut tx, row) = scoped(&st, tenant).await?;
    let sub = (!caller.super_admin).then_some(caller.sub.as_str());
    let (summary, _, _) = readmodel::tenant::licence(&mut tx, &row, plan(&st), sub).await?;
    tx.commit().await?;
    let mut v = tenant_with_flag(&row, &st);
    v["licence"] = summary;
    Ok(Json(v))
}

/// `Tenant` plus `support_auto_assign` (SW-7) and the attachment quota (SW-9: `attachment_quota_bytes`, null = the
/// default, which `attachment_quota_default_bytes` names), on the single-tenant answers only.
fn tenant_with_flag(row: &TenantRow, st: &AppState) -> Value {
    let mut v = readmodel::tenant::tenant_json(row, plan(st));
    v["support_auto_assign"] = json!(row.support_auto_assign);
    v["attachment_quota_bytes"] = json!(row.attachment_quota_bytes);
    v["attachment_quota_default_bytes"] = json!(st.cfg.attachment_quota_bytes);
    v
}

async fn list_users(
    caller: Caller,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TenantPath>,
) -> ApiResult<Json<Value>> {
    let tenant = tenant_id(&p.tenant_id)?;
    gated_read_access(&st, &caller, tenant).await?;
    let (mut tx, _) = scoped(&st, tenant).await?;
    let users = readmodel::tenant::users(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(paged(
        users.iter().map(readmodel::tenant::user_json).collect(),
        None,
    )))
}

async fn list_jobs(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TenantPath>,
) -> ApiResult<Json<Value>> {
    let tenant = tenant_id(&p.tenant_id)?;
    let on_bus = readmodel::agent_status::bus_up(&st).await;
    let (mut tx, _) = scoped(&st, tenant).await?;
    let jobs = readmodel::jobs::summaries(&mut tx, on_bus).await?;
    tx.commit().await?;
    Ok(Json(paged(jobs, None)))
}

async fn get_licence(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TenantPath>,
) -> ApiResult<Json<Value>> {
    let tenant = tenant_id(&p.tenant_id)?;
    let (tx, row) = scoped(&st, tenant).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "licence": readmodel::tenant::tenant_licence(&row, plan(&st)),
        "verdict": readmodel::tenant::platform_verdict(&row),
    })))
}

/// The newest 50 outbox rows of the scoped tenant, platform shape.
pub async fn platform_rows(tx: &mut sqlx::PgConnection, limit: i64) -> ApiResult<Vec<Value>> {
    let rows: Vec<OutboxRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ROW_COLUMNS} FROM provision_outbox ORDER BY created_at DESC, id DESC LIMIT $1"
    )))
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    let state: Vec<(String, Option<chrono::DateTime<chrono::Utc>>, Option<Value>)> =
        sqlx::query_as("SELECT target, last_success_at, drift FROM provision_state")
            .fetch_all(&mut *tx)
            .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let s = state.iter().find(|(t, _, _)| *t == r.target);
            r.platform_json(
                s.and_then(|(_, at, _)| *at),
                s.and_then(|(_, _, d)| d.as_ref()),
            )
        })
        .collect())
}

async fn list_provisioning(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TenantPath>,
) -> ApiResult<Json<Value>> {
    let tenant = tenant_id(&p.tenant_id)?;
    let (mut tx, _) = scoped(&st, tenant).await?;
    let rows = platform_rows(&mut tx, 50).await?;
    tx.commit().await?;
    Ok(Json(Value::Array(rows)))
}

async fn get_offboard(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TenantPath>,
) -> ApiResult<Json<Value>> {
    let tenant = tenant_id(&p.tenant_id)?;
    let (mut tx, _) = scoped(&st, tenant).await?;
    let stored = readmodel::tenant::offboard(&mut tx, tenant).await?;
    tx.commit().await?;
    // B1b T9 (owner 2026-10-20): a tenant that was never offboarded answers 200 `null`, not a 404 every tenant
    // detail view logged; an unknown (or, under RLS, invisible) tenant stays 404 `tenant_not_found`.
    Ok(Json(stored.unwrap_or(Value::Null)))
}

async fn get_footprint(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TenantPath>,
) -> ApiResult<Json<Value>> {
    let tenant = tenant_id(&p.tenant_id)?;
    let row = platform::rollup_one(&st.db, tenant)
        .await?
        .ok_or_else(|| tenant_not_found(tenant))?;
    Ok(Json(
        platform::footprint(&st.db, &row, readmodel::agent_status::bus_up(&st).await).await?,
    ))
}

// ---- lifecycle (B1 T13) -----------------------------------------------------------------------------------------

/// The `Tenant` answer of `tenant`, read now.
async fn tenant_answer(st: &AppState, tenant: Uuid) -> ApiResult<Value> {
    let (tx, row) = scoped(st, tenant).await?;
    tx.commit().await?;
    Ok(readmodel::tenant::tenant_json(&row, plan(st)))
}

/// A lower-cased email address, else 422 `invalid_email`.
fn email(raw: &str) -> ApiResult<String> {
    let e = raw.trim().to_lowercase();
    if e.chars().count() > 254 || !audit::scrub::looks_like_email(&e) {
        return Err(ApiError::Unprocessable(
            "invalid_email",
            "first_admin_email must be one email address".into(),
        ));
    }
    Ok(e)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    name: String,
    first_admin_email: String,
    /// REQUIRED with the email (commonpractice `createTenantSchema`): the portal realm's user profile requires both
    /// names, and an identity created without them is stopped at Keycloak's VERIFY_PROFILE form before the
    /// set-password one.
    first_admin_first_name: Option<String>,
    first_admin_last_name: Option<String>,
    residency: Option<String>,
}

/// Both of the first admin's names, else 422 `first_admin_name_required` (or `invalid_person_name`).
fn first_admin_names(first: Option<&str>, last: Option<&str>) -> ApiResult<PersonNames> {
    let first = person_name("first_admin_first_name", first)?;
    let last = person_name("first_admin_last_name", last)?;
    match (first, last) {
        (Some(f), Some(l)) => Ok(PersonNames {
            first: Some(f),
            last: Some(l),
        }),
        _ => Err(ApiError::Unprocessable(
            "first_admin_name_required",
            "first_admin_first_name and first_admin_last_name are required with a first admin email, so the \
             invitee only sets a password"
                .into(),
        )),
    }
}

async fn create(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<CreateBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let name = lifecycle::tenant_name(&b.name)?;
    let email = email(&b.first_admin_email)?;
    let residency = lifecycle::residency(b.residency.as_deref())?;
    let names = first_admin_names(
        b.first_admin_first_name.as_deref(),
        b.first_admin_last_name.as_deref(),
    )?;
    let created = lifecycle::create_tenant(
        &st,
        &Actor::person(&c),
        CreateInput {
            name,
            first_admin: FirstAdmin::Email(email, names),
            residency: residency.to_string(),
            origin: Origin::Manual,
        },
    )
    .await?;
    let (mut tx, row) = scoped(&st, created.tenant).await?;
    let provisioning = platform_rows(&mut tx, 50).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "tenant": readmodel::tenant::tenant_json(&row, plan(&st)),
            "licence": created.licence_id.map(|id| json!({"id": id, "kid": null})),
            "provisioning": provisioning,
            "invited": created.invited,
        })),
    ))
}

/// The platform-log label of an import refused before any tenant exists (review #7: never the typed name).
pub const REJECTED_IMPORT_LABEL: &str = "(rejected import)";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportBody {
    id: String,
    name: String,
    first_admin: String,
    /// Required when `first_admin` is an email (as on create); ignored for a subject.
    first_admin_first_name: Option<String>,
    first_admin_last_name: Option<String>,
    residency: Option<String>,
}

async fn import(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<ImportBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let actor = Actor::person(&c);
    let id = match Uuid::parse_str(b.id.trim()) {
        Ok(u) if !u.is_nil() && b.id.trim().len() == 36 => u,
        _ => {
            // Review #7: the typed name is unvalidated input (it may be an address or anything else) and no tenant
            // exists, so the label is fixed and the name is never recorded.
            audit::record_platform_alone(
                &st.db,
                &actor,
                PlatformEvent::new("tenant.import", REJECTED_IMPORT_LABEL)
                    .failed("invalid_tenant_id"),
            )
            .await?;
            return Err(ApiError::Unprocessable(
                "invalid_tenant_id",
                "id must be a full, non-nil tenant uuid".into(),
            ));
        }
    };
    let name = lifecycle::tenant_name(&b.name)?;
    let residency = lifecycle::residency(b.residency.as_deref())?;
    let raw = b.first_admin.trim();
    let first_admin = if raw.contains('@') {
        let addr = email(raw).map_err(|_| invalid_admin())?;
        let names = first_admin_names(
            b.first_admin_first_name.as_deref(),
            b.first_admin_last_name.as_deref(),
        )?;
        FirstAdmin::Email(addr, names)
    } else if raw.is_empty() || raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid_admin());
    } else {
        FirstAdmin::Sub(raw.to_string())
    };
    let mut tx = begin_scoped(&st.db, id).await?;
    let exists = readmodel::tenant::read(&mut tx, id).await?;
    tx.commit().await?;
    if let Some(row) = exists {
        return Ok((
            StatusCode::OK,
            Json(
                json!({"tenant": readmodel::tenant::tenant_json(&row, plan(&st)), "imported": false}),
            ),
        ));
    }
    let created = lifecycle::create_tenant(
        &st,
        &actor,
        CreateInput {
            name,
            first_admin,
            residency: residency.to_string(),
            origin: Origin::Import(id),
        },
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"tenant": tenant_answer(&st, created.tenant).await?, "imported": true})),
    ))
}

fn invalid_admin() -> ApiError {
    ApiError::Unprocessable(
        "invalid_admin",
        "first_admin must be an email address or a Keycloak subject".into(),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchBody {
    status: Option<String>,
    name: Option<String>,
    support_auto_assign: Option<bool>,
    /// SW-9 (owner 2026-10-28): the tenant's attachment storage quota in bytes; `null` = the platform default.
    #[serde(default, deserialize_with = "set_or_null")]
    attachment_quota_bytes: Option<Option<i64>>,
}

/// A field that may be sent as a number, as `null`, or not at all (`None`, `Some(None)`, `Some(Some(n))`).
fn set_or_null<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<i64>>, D::Error> {
    Option::<i64>::deserialize(d).map(Some)
}

/// `PATCH /api/tenants/{id}` (super_admin) `{"status"?, "name"?}`, at least one: a rename first
/// ([`lifecycle::rename`], any status), then the status change ([`lifecycle::set_status`]).
async fn patch_tenant(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TenantPath>,
    ApiJson(b): ApiJson<PatchBody>,
) -> ApiResult<Json<Value>> {
    let tenant = tenant_id(&p.tenant_id)?;
    let actor = Actor::person(&c);
    if b.name.is_none()
        && b.status.is_none()
        && b.support_auto_assign.is_none()
        && b.attachment_quota_bytes.is_none()
    {
        return Err(ApiError::Unprocessable(
            "invalid_input",
            "send status, name, support_auto_assign, attachment_quota_bytes or a combination"
                .into(),
        ));
    }
    let mut row = None;
    if let Some(name) = &b.name {
        row = Some(lifecycle::rename(&st, &actor, tenant, name).await?);
    }
    if let Some(status) = &b.status {
        row = Some(lifecycle::set_status(&st, &actor, tenant, status).await?);
    }
    if let Some(enabled) = b.support_auto_assign {
        row = Some(lifecycle::set_support_auto_assign(&st, &actor, tenant, enabled).await?);
    }
    if let Some(quota) = b.attachment_quota_bytes {
        row = Some(lifecycle::set_attachment_quota(&st, &actor, tenant, quota).await?);
    }
    let row = row.ok_or_else(|| ApiError::Internal(anyhow::anyhow!("patch changed nothing")))?;
    Ok(Json(tenant_with_flag(&row, &st)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OffboardBody {
    confirm_name: String,
}

/// A person super_admin, or the pinned secure-apps service account (crate::tenants::offboarder); `confirm_name` is
/// required either way.
async fn offboard(
    caller: Offboarder,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TenantPath>,
    ApiJson(b): ApiJson<OffboardBody>,
) -> ApiResult<Json<Value>> {
    let tenant = tenant_id(&p.tenant_id)?;
    Ok(Json(
        lifecycle::offboard(&st, &caller.actor(), tenant, &b.confirm_name).await?,
    ))
}
