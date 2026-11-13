//! Member invites (B1b T2, owner ruling F-01); the logic is crate::members::invites.
//!
//! - `GET /api/tenant/invites?state=pending|accepted|expired|revoked&before=&limit=` — any member:
//!   `Paged<MemberInvite>`, newest first (`expired` computed on read). 422 `invalid_state`. (Registered with B1's
//!   `POST /api/tenant/invites` in crate::routes::users.)
//! - `POST /api/tenant/invites/{invite_id}/resend` — tenant_admin: 202 `MemberInvite`. 404 `invite_not_found`; 409
//!   `invite_not_pending` | `invite_superseded`; 429 `invite_resend_cooldown` (`retry_after_s`); 502 `keycloak_error`.
//! - `POST /api/tenant/invites/{invite_id}/revoke` — tenant_admin: `MemberInvite` (idempotent). 404, 409
//!   `invite_not_pending` (accepted).
//! - `GET /api/tenants/{tenant_id}/invites`, `POST …/invites/{invite_id}/resend|revoke` — super_admin, in the path
//!   tenant: the same answers + 404 `tenant_not_found`, 409 `tenant_archived` on resend; the platform log gets a row
//!   too (subject = the tenant).
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::audit::Actor;
use crate::error::{ApiError, ApiPath, ApiQuery, ApiResult};
use crate::members::invites::{self, By, DEFAULT_LIMIT, MAX_LIMIT};
use crate::page::limit;
use crate::routes::tenants::scoped;
use crate::state::AppState;
use crate::tenancy::{Member, Requires, SuperAdmin, TenantAdmin, tenant_id};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/tenant/invites/{invite_id}/resend",
            post(resend_invite),
        )
        .route(
            "/api/tenant/invites/{invite_id}/revoke",
            post(revoke_invite),
        )
        .route(
            "/api/tenants/{tenant_id}/invites",
            get(platform_list_invites),
        )
        .route(
            "/api/tenants/{tenant_id}/invites/{invite_id}/resend",
            post(platform_resend_invite),
        )
        .route(
            "/api/tenants/{tenant_id}/invites/{invite_id}/revoke",
            post(platform_revoke_invite),
        )
}

#[derive(Deserialize)]
pub struct ListQuery {
    state: Option<String>,
    before: Option<String>,
    limit: Option<String>,
}

#[derive(Deserialize)]
struct InvitePath {
    invite_id: String,
}

#[derive(Deserialize)]
struct TenantInvitePath {
    tenant_id: String,
    invite_id: String,
}

#[derive(Deserialize)]
struct TenantPath {
    tenant_id: String,
}

/// An invite id from the path: a uuid, else 404 `invite_not_found` (an id that cannot exist is not found).
fn invite_id(raw: &str) -> ApiResult<Uuid> {
    Uuid::parse_str(raw)
        .map_err(|_| ApiError::Missing("invite_not_found", "no such invite in this tenant".into()))
}

/// The list in the scoped transaction `tx`.
async fn list_in(tx: &mut sqlx::PgConnection, q: &ListQuery) -> ApiResult<Value> {
    let state = invites::state_filter(q.state.as_deref())?;
    let before = invites::cursor(q.before.as_deref())?;
    let lim = limit(q.limit.as_deref(), DEFAULT_LIMIT, MAX_LIMIT)?;
    invites::list(tx, state, before, lim).await
}

/// `tenantInvites`: registered on `/api/tenant/invites` in crate::routes::users (B1 owns the POST).
pub async fn list_invites(
    Member(t): Member,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let v = list_in(&mut tx, &q).await?;
    tx.commit().await?;
    Ok(Json(v))
}

async fn resend_invite(
    Requires(t, _): TenantAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<InvitePath>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let id = invite_id(&p.invite_id)?;
    let actor = t.actor();
    let v = invites::resend(
        &st,
        t.tenant,
        id,
        By {
            actor: &actor,
            platform: false,
        },
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(v)))
}

async fn revoke_invite(
    Requires(t, _): TenantAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<InvitePath>,
) -> ApiResult<Json<Value>> {
    let id = invite_id(&p.invite_id)?;
    let actor = t.actor();
    let v = invites::revoke(
        &st,
        t.tenant,
        id,
        By {
            actor: &actor,
            platform: false,
        },
    )
    .await?;
    Ok(Json(v))
}

async fn platform_list_invites(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TenantPath>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Value>> {
    let tenant = tenant_id(&p.tenant_id)?;
    let (mut tx, _) = scoped(&st, tenant).await?;
    let v = list_in(&mut tx, &q).await?;
    tx.commit().await?;
    Ok(Json(v))
}

/// The path tenant (404 `tenant_not_found`) and its status.
async fn path_tenant(st: &AppState, raw: &str) -> ApiResult<(Uuid, String)> {
    let tenant = tenant_id(raw)?;
    let (tx, row) = scoped(st, tenant).await?;
    tx.commit().await?;
    Ok((tenant, row.status))
}

async fn platform_resend_invite(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TenantInvitePath>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let (tenant, status) = path_tenant(&st, &p.tenant_id).await?;
    if status == "archived" {
        return Err(ApiError::Conflict(
            "tenant_archived",
            "this tenant is archived".into(),
        ));
    }
    let id = invite_id(&p.invite_id)?;
    let actor = Actor::person(&c);
    let v = invites::resend(
        &st,
        tenant,
        id,
        By {
            actor: &actor,
            platform: true,
        },
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(v)))
}

async fn platform_revoke_invite(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TenantInvitePath>,
) -> ApiResult<Json<Value>> {
    let (tenant, _) = path_tenant(&st, &p.tenant_id).await?;
    let id = invite_id(&p.invite_id)?;
    let actor = Actor::person(&c);
    let v = invites::revoke(
        &st,
        tenant,
        id,
        By {
            actor: &actor,
            platform: true,
        },
    )
    .await?;
    Ok(Json(v))
}
