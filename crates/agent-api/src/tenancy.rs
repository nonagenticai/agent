//! The tenant and job a request acts in, and the guards built on them (B1 X2; port of care-bus-agents@96c608c
//! `tenancy.rs`).
//!
//! Handlers reach tenant rows only through [`begin_scoped`] / [`TenantCtx::begin`] / [`JobCtx::begin`], which open a
//! transaction and set the tenant (and job) GUCs transaction-locally (`set_config(…, true)`, the bind-parameter
//! form of `SET LOCAL`): RLS then confines every statement to that scope, and nothing survives COMMIT on the pooled
//! connection. Never hold a transaction across an upstream HTTP call: open one before and one after.
//!
//! Resolution (refusals as the UI's `areas/core.ts` `requireTenant`):
//! 1. `X-Active-Tenant-Id` malformed or repeated → 400 `invalid_active_tenant`.
//! 2. super_admin with the header → the tenant must exist (404 `tenant_not_found`), archived → 403
//!    `tenant_archived`, suspended is allowed; the switch is recorded (`agent_switch_active_tenant`, audited
//!    `tenant.switch` in the target tenant when it changed); every role is held (D88), narrowed by the super_admin's
//!    view-as for that tenant (D79); `platform_actor` unless it is the super_admin's home tenant.
//! 3. super_admin without the header → the home tenant, else 409 `active_tenant_required`.
//! 4. anyone else: no (enabled) membership → 403 `not_provisioned`; a header naming another tenant → 403
//!    `forbidden`; archived → 403 `tenant_archived`; suspended → 403 `tenant_suspended`.
//! 5. licence gate 2 (`LICENSE_GATE_2_ENABLED`, crate::licensing::gate::check_request): the stored verdict of the
//!    resolved tenant, for every caller acting in it (a super_admin too); 402 `license_*` / 403 `license_*_mismatch`.
//!    Not on the exempt routes (crate::licensing::gate::exempt: `/api/license/status`, `/api/me*`, `/api/platform/*`).
//!
//! Roles (D88, plan §R.3): a member holds `users.roles`; a super_admin acting in a tenant holds all of them. A live
//! view-as narrows them: `effective = held ∩ view_as`. Every role check ([`TenantCtx::require`], [`Requires`],
//! [`JobRequires`]) reads `effective`; only the last-admin guard, `GET /api/me` and `clearViewAs` read the held set.
//!
//! Service tokens never resolve a [`TenantCtx`].
use std::marker::PhantomData;

use axum::extract::{FromRequestParts, RawPathParams};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::audit::{self, Actor, Event};
pub use crate::auth::ViewAs;
use crate::auth::{Caller, Role, RoleSet, TenantStatus};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub const ACTIVE_TENANT_HEADER: &str = "x-active-tenant-id";

pub async fn begin_scoped(db: &PgPool, tenant: Uuid) -> ApiResult<Transaction<'static, Postgres>> {
    let mut tx = db.begin().await?;
    set_tenant(&mut tx, tenant).await?;
    Ok(tx)
}

/// Scope an open transaction to `tenant` (for the rest of the transaction).
pub async fn set_tenant(conn: &mut sqlx::PgConnection, tenant: Uuid) -> ApiResult<()> {
    sqlx::query("SELECT set_config('app.current_tenant', $1, true)")
        .bind(tenant.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

/// The one lock order (B2 T0d) for a transaction that writes member rows (`users`, `member_entitlement`,
/// `member_invites`) and anything else of the tenant: the `tenants` row FIRST, then the member rows. An offboard
/// takes the row `FOR UPDATE` and then updates every member; a path that locked a member row first and reached the
/// tenants row only later (an `audit_event` insert takes `FOR KEY SHARE` on it through its foreign key) deadlocked
/// against it (40P01). `FOR KEY SHARE` waits only for an offboard: other member writes, a suspend's
/// `UPDATE tenants` and a later `UPDATE tenants` (park / unpark) in the same transaction all coexist with it. The
/// licence paths take it after the licence lock and after the licences call (advisory lock → licences call →
/// tenants → entitlement), so it is never held across an upstream call.
pub async fn lock_tenant_row(conn: &mut sqlx::PgConnection, tenant: Uuid) -> ApiResult<()> {
    sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR KEY SHARE")
        .bind(tenant)
        .execute(conn)
        .await?;
    Ok(())
}

/// A bare (hyphenated), non-nil uuid from a path, else 400 `invalid_tenant_id`.
pub fn tenant_id(raw: &str) -> ApiResult<Uuid> {
    match Uuid::parse_str(raw) {
        Ok(u) if !u.is_nil() && raw.len() == 36 => Ok(u),
        _ => Err(ApiError::BadRequest(
            "invalid_tenant_id",
            "The tenant id must be a bare, non-nil uuid.".into(),
        )),
    }
}

/// The tenant a request acts in.
#[derive(Clone, Debug)]
pub struct TenantCtx {
    pub tenant: Uuid,
    pub status: TenantStatus,
    /// The roles held: `users.roles`, or every role for a super_admin acting in the tenant.
    pub roles: RoleSet,
    /// What role checks read: `roles ∩ view_as.roles`, or `roles` without a view-as.
    pub effective: RoleSet,
    /// A live narrow-only view (D79).
    pub view_as: Option<ViewAs>,
    pub caller: Caller,
    /// A super_admin acting in a tenant that is not their home tenant.
    pub platform_actor: bool,
}

impl TenantCtx {
    /// The tenant GUC set; the job GUC empty (every job of the tenant visible).
    pub async fn begin(&self, db: &PgPool) -> ApiResult<Transaction<'static, Postgres>> {
        begin_scoped(db, self.tenant).await
    }

    /// A context with `effective` derived from `roles` and `view_as`.
    pub fn new(
        tenant: Uuid,
        status: TenantStatus,
        roles: RoleSet,
        view_as: Option<ViewAs>,
        caller: Caller,
        platform_actor: bool,
    ) -> Self {
        let effective = roles.effective(view_as.map(|v| v.roles));
        Self {
            tenant,
            status,
            roles,
            effective,
            view_as,
            caller,
            platform_actor,
        }
    }

    /// 403 `forbidden` with `missing_role` unless the EFFECTIVE set has `r`.
    pub fn require(&self, r: Role) -> ApiResult<()> {
        if self.effective.has(r) {
            Ok(())
        } else {
            Err(missing_role(r))
        }
    }

    /// 403 `forbidden` (`missing_role: support_agent`) unless the EFFECTIVE set may work conversations
    /// ([`crate::auth::CONVERSATION_WORKERS`]: `support_agent` or `support_manager`). Routes taking it are `JobMember`
    /// / `Member` plus this call, listed in `ANY_OF` of tests/roles.rs.
    pub fn require_conversation_worker(&self) -> ApiResult<()> {
        if self.effective.works_conversations() {
            Ok(())
        } else {
            Err(missing_role(Role::SupportAgent))
        }
    }

    /// The effective set holds `tenant_admin`.
    pub fn is_admin(&self) -> bool {
        self.effective.has(Role::TenantAdmin)
    }

    /// The audit actor of this request.
    pub fn actor(&self) -> Actor {
        Actor::of(self)
    }
}

/// A tenant plus the job named by the `{job}` path segment.
#[derive(Clone, Debug)]
pub struct JobCtx {
    pub t: TenantCtx,
    pub job_id: Uuid,
    pub job_key: String,
}

impl JobCtx {
    /// Both GUCs set (`app.current_tenant`, `app.current_job`).
    pub async fn begin(&self, db: &PgPool) -> ApiResult<Transaction<'static, Postgres>> {
        let mut tx = begin_scoped(db, self.t.tenant).await?;
        sqlx::query("SELECT set_config('app.current_job', $1, true)")
            .bind(self.job_id.to_string())
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }
}

fn invalid_active_tenant() -> ApiError {
    ApiError::BadRequest(
        "invalid_active_tenant",
        "X-Active-Tenant-Id must be one bare, non-nil tenant uuid".into(),
    )
}

/// `X-Active-Tenant-Id`: absent, or exactly one bare (hyphenated), non-nil uuid; anything else (malformed,
/// repeated) is 400 `invalid_active_tenant`.
pub fn active_tenant_from(headers: &HeaderMap) -> ApiResult<Option<Uuid>> {
    let mut values = headers.get_all(ACTIVE_TENANT_HEADER).iter();
    let Some(v) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(invalid_active_tenant());
    }
    let raw = v.to_str().map_err(|_| invalid_active_tenant())?;
    match Uuid::parse_str(raw) {
        Ok(u) if !u.is_nil() && raw.len() == 36 => Ok(Some(u)),
        _ => Err(invalid_active_tenant()),
    }
}

fn tenant_archived() -> ApiError {
    ApiError::Forbidden("tenant_archived", "this tenant is archived".into())
}

fn tenant_suspended() -> ApiError {
    ApiError::Forbidden("tenant_suspended", "this tenant is suspended".into())
}

fn not_provisioned() -> ApiError {
    ApiError::Forbidden(
        "not_provisioned",
        "this account is not provisioned in any tenant".into(),
    )
}

fn super_admin_required() -> ApiError {
    ApiError::Forbidden(
        "super_admin_required",
        "this needs the super_admin role".into(),
    )
}

/// The status of `tenant`, read under its own GUC (`tenants` is under RLS); `None` when it does not exist.
pub async fn tenant_status(db: &PgPool, tenant: Uuid) -> ApiResult<Option<TenantStatus>> {
    let mut tx = begin_scoped(db, tenant).await?;
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    status
        .map(|s| {
            TenantStatus::parse(&s).ok_or_else(|| {
                ApiError::Internal(anyhow::anyhow!("tenants.status has an unknown value"))
            })
        })
        .transpose()
}

/// Record `tenant` as the super_admin's active tenant; a change is audited once, in the target tenant.
pub async fn record_switch(
    db: &PgPool,
    caller: &Caller,
    tenant: Uuid,
    platform: bool,
) -> ApiResult<()> {
    let mut tx = begin_scoped(db, tenant).await?;
    let prev: Option<Uuid> = sqlx::query_scalar("SELECT agent_switch_active_tenant($1, $2)")
        .bind(&caller.sub)
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await?;
    if prev != Some(tenant) {
        audit::record(
            &mut tx,
            &Actor::member(&caller.sub, platform),
            Event::new("tenant.switch", "tenant", tenant.to_string())
                .detail("from", prev.map(|p| p.to_string())),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// A super_admin entering `tenant` by header: it must exist and not be archived.
async fn sa_enter(st: &AppState, caller: &Caller, tenant: Uuid) -> ApiResult<TenantCtx> {
    let status = tenant_status(&st.db, tenant)
        .await?
        .ok_or_else(|| ApiError::Missing("tenant_not_found", format!("no tenant {tenant}")))?;
    if status == TenantStatus::Archived {
        return Err(tenant_archived());
    }
    let platform = caller.home.as_ref().is_none_or(|h| h.tenant != tenant);
    record_switch(&st.db, caller, tenant, platform).await?;
    let view = sa_view_as(&st.db, &caller.sub, tenant).await?;
    Ok(TenantCtx::new(
        tenant,
        status,
        RoleSet::ALL,
        view,
        caller.clone(),
        platform,
    ))
}

/// The super_admin's live view-as in `tenant` (`agent_sa_view_as`; an expired row reads as none).
pub async fn sa_view_as(db: &PgPool, sub: &str, tenant: Uuid) -> ApiResult<Option<ViewAs>> {
    let row: Option<(Vec<String>, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT roles, expires_at FROM agent_sa_view_as($1, $2)")
            .bind(sub)
            .bind(tenant)
            .fetch_optional(db)
            .await?;
    row.map(|(roles, expires_at)| {
        Ok(ViewAs {
            roles: RoleSet::from_db(&roles)?,
            expires_at,
        })
    })
    .transpose()
}

async fn resolve(headers: &HeaderMap, st: &AppState, caller: Caller) -> ApiResult<TenantCtx> {
    let header = active_tenant_from(headers)?;
    if caller.super_admin {
        if let Some(t) = header {
            return sa_enter(st, &caller, t).await;
        }
        let Some(h) = caller.home.clone() else {
            return Err(ApiError::Conflict(
                "active_tenant_required",
                "name the tenant to act in with the X-Active-Tenant-Id header".into(),
            ));
        };
        if h.status == TenantStatus::Archived {
            return Err(tenant_archived());
        }
        let view = sa_view_as(&st.db, &caller.sub, h.tenant).await?;
        return Ok(TenantCtx::new(
            h.tenant,
            h.status,
            RoleSet::ALL,
            view,
            caller,
            false,
        ));
    }
    let h = caller.home.clone().ok_or_else(not_provisioned)?;
    if header.is_some_and(|t| t != h.tenant) {
        return Err(ApiError::Forbidden(
            "forbidden",
            "only a member of that tenant may act in it".into(),
        ));
    }
    match h.status {
        TenantStatus::Archived => return Err(tenant_archived()),
        TenantStatus::Suspended => return Err(tenant_suspended()),
        TenantStatus::Active => {}
    }
    crate::members::seen::record(
        st,
        &caller,
        h.tenant,
        crate::members::seen::surface(headers),
    )
    .await;
    Ok(TenantCtx::new(
        h.tenant, h.status, h.roles, h.view_as, caller, false,
    ))
}

/// Resolved once per request (cached in the request extensions), so a switch is audited once even when a
/// handler takes several guards.
impl FromRequestParts<AppState> for TenantCtx {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        if let Some(t) = parts.extensions.get::<TenantCtx>() {
            return Ok(t.clone());
        }
        let caller = Caller::from_request_parts(parts, st).await?;
        let t = resolve(&parts.headers, st, caller).await?;
        tracing::Span::current().record("tenant", tracing::field::display(t.tenant));
        // 5. licence gate 2 (C1), after the status checks of `resolve`; cached with the context, so once per request
        if !crate::licensing::gate::exempt(parts.uri.path()) {
            crate::licensing::gate::check_request(st, t.tenant).await?;
        }
        parts.extensions.insert(t.clone());
        Ok(t)
    }
}

/// The `{job}` path segment, looked up inside the tenant (RLS); 404 `job_not_found` when the tenant has no such job.
async fn job_ctx(parts: &mut Parts, st: &AppState, t: TenantCtx) -> ApiResult<JobCtx> {
    if let Some(j) = parts.extensions.get::<JobCtx>() {
        return Ok(j.clone());
    }
    let params = RawPathParams::from_request_parts(parts, st)
        .await
        .map_err(|_| ApiError::BadRequest("invalid_path", "The path is not valid.".into()))?;
    let key = params
        .iter()
        .find(|(k, _)| *k == "job")
        .map(|(_, v)| v.to_string())
        .ok_or_else(|| {
            ApiError::Internal(anyhow::anyhow!("job guard on a route without {{job}}"))
        })?;
    let mut tx = t.begin(&st.db).await?;
    let job_id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM jobs WHERE key = $1")
        .bind(&key)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    let job_id =
        job_id.ok_or_else(|| ApiError::Missing("job_not_found", format!("no job {key:?}")))?;
    let j = JobCtx {
        t,
        job_id,
        job_key: key,
    };
    parts.extensions.insert(j.clone());
    Ok(j)
}

/// 403 `forbidden` naming the role the effective set lacks (`{"error": {"code": "forbidden", "missing_role": key}}`).
pub fn missing_role(r: Role) -> ApiError {
    ApiError::Forbidden("forbidden", format!("this needs the {} role", r.key()))
        .with("missing_role", r.key())
}

/// The role markers of [`Requires`] / [`JobRequires`]: one zero-sized type per [`Role`].
pub mod role {
    use crate::auth::Role;

    pub trait RoleMarker: Send + Sync + 'static {
        const ROLE: Role;
    }

    macro_rules! markers {
        ($($name:ident),* $(,)?) => {$(
            #[derive(Clone, Copy, Debug)]
            pub struct $name;
            impl RoleMarker for $name {
                const ROLE: Role = Role::$name;
            }
        )*};
    }

    markers!(
        TenantAdmin,
        JobDesigner,
        ReleaseManager,
        KnowledgeManager,
        IntegrationsManager,
        SupportManager,
        SupportAgent,
        Operator,
    );
}

use role::RoleMarker;

/// Any member of the tenant (an observer included), or a super_admin in it.
pub struct Member(pub TenantCtx);
/// A tenant write: the EFFECTIVE role set holds `R::ROLE` (a super_admin in the tenant holds every role unless a
/// view-as narrows it) → else 403 `forbidden` with `missing_role`.
pub struct Requires<R: RoleMarker>(pub TenantCtx, pub PhantomData<R>);
/// [`Requires`] in the `{job}` of the path (the role is checked before the job is looked up).
pub struct JobRequires<R: RoleMarker>(pub JobCtx, pub PhantomData<R>);
/// The members/provisioning guard (§R.1 `tenant_admin`).
pub type TenantAdmin = Requires<role::TenantAdmin>;
/// A super_admin acting in a tenant → else 403 `super_admin_required`.
pub struct SaInTenant(pub TenantCtx);
/// `Member` in the `{job}` of the path (404 `job_not_found`).
pub struct JobMember(pub JobCtx);
/// super_admin on a platform route; no tenant is resolved → else 403 `super_admin_required`.
pub struct SuperAdmin(pub Caller);

impl FromRequestParts<AppState> for Member {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        Ok(Member(TenantCtx::from_request_parts(parts, st).await?))
    }
}

impl<R: RoleMarker> FromRequestParts<AppState> for Requires<R> {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let t = TenantCtx::from_request_parts(parts, st).await?;
        t.require(R::ROLE)?;
        Ok(Requires(t, PhantomData))
    }
}

impl FromRequestParts<AppState> for SaInTenant {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let c = Caller::from_request_parts(parts, st).await?;
        if !c.super_admin {
            return Err(super_admin_required());
        }
        let t = TenantCtx::from_request_parts(parts, st).await?;
        // D79: the super_admin's view-as narrows these writes too (a super_admin holds every role unless narrowed,
        // D88), so an observer view-as gets `forbidden {missing_role}` like every other tenant write (A12)
        t.require(Role::TenantAdmin)?;
        Ok(SaInTenant(t))
    }
}

impl FromRequestParts<AppState> for JobMember {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let t = TenantCtx::from_request_parts(parts, st).await?;
        Ok(JobMember(job_ctx(parts, st, t).await?))
    }
}

impl<R: RoleMarker> FromRequestParts<AppState> for JobRequires<R> {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let t = TenantCtx::from_request_parts(parts, st).await?;
        t.require(R::ROLE)?;
        Ok(JobRequires(job_ctx(parts, st, t).await?, PhantomData))
    }
}

impl FromRequestParts<AppState> for SuperAdmin {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let c = Caller::from_request_parts(parts, st).await?;
        if c.super_admin {
            Ok(SuperAdmin(c))
        } else {
            Err(super_admin_required())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(ACTIVE_TENANT_HEADER, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn active_tenant_header_is_one_bare_non_nil_uuid() {
        let t = "0195f48b-d000-7000-8000-000000000099";
        assert_eq!(active_tenant_from(&headers(&[])).unwrap(), None);
        assert_eq!(
            active_tenant_from(&headers(&[t])).unwrap(),
            Some(t.parse().unwrap())
        );
        for bad in [
            vec!["nope"],
            vec!["00000000-0000-0000-0000-000000000000"],
            vec!["0195f48bd00070008000000000000099"],
            vec![t, t],
        ] {
            let e = active_tenant_from(&headers(&bad)).unwrap_err();
            assert_eq!(e.code(), "invalid_active_tenant", "{bad:?}");
        }
    }
}
