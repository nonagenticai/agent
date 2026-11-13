//! `GET /api/me`: who the verified caller is and which tenant they act in (any person token with `aud` = the app;
//! also unprovisioned). The same resolution as crate::tenancy, but it ANSWERS instead of refusing: a super_admin
//! with `X-Active-Tenant-Id` gets that tenant (every role, the switch recorded unless it is archived; 404
//! `tenant_not_found` when it does not exist); archived and suspended tenants are reported in `tenant.status`.
//! `roles` are the roles held, `effective_roles` what role checks read (narrowed by a live view-as), `view_as` the
//! live view (D88/D79, plan §R.4).
//!
//! For a member of an active tenant it is also the invite-accept moment (D77 L5; Keycloak has no accept callback):
//! the session is recorded (crate::members::seen) and a `pending` member entitlement is minted after the read
//! committed, at most once per sub per 5 minutes. A mint failure is logged and counted, the row stays `pending`
//! for the provisioning tick, and this still answers 200.
//!
//! View-as (D79): `PUT /api/me/view-as {roles, ttl_seconds?}` narrows the caller's roles in the active tenant for
//! 60 s..8 h (default 1 h) and `DELETE /api/me/view-as` ends it (always allowed, idempotent). Both answer `Me`. The
//! view is a server-side row (`member_view_as`, or `super_admin_view_as` per tenant for a super_admin), so it
//! survives a reload and reaches SSE requests that cannot carry headers. Audited `user.view_as.start` /
//! `user.view_as.end` (`reason` cleared | replaced); an expiry writes nothing (the start row carries `expires_at`).

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit::{self, Actor, Event};
use crate::auth::{Caller, RoleSet, TenantStatus, ViewAs};
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::members::{self, seen};
use crate::state::AppState;
use crate::tenancy::{
    Member, TenantCtx, active_tenant_from, begin_scoped, record_switch, sa_view_as, tenant_status,
};

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/me", get(me)).route(
        "/api/me/view-as",
        axum::routing::put(set_view_as).delete(clear_view_as),
    )
}

/// (name, status, display_name of `sub`) of a tenant, read under its own tenant GUC; `None` when it does not
/// exist.
async fn tenant_row(
    st: &AppState,
    tenant: Uuid,
    sub: &str,
) -> ApiResult<Option<(String, String, Option<String>)>> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let row = sqlx::query_as(
        "SELECT t.name, t.status, (SELECT u.display_name FROM users u WHERE u.sub = $2) \
           FROM tenants t WHERE t.id = $1",
    )
    .bind(tenant)
    .bind(sub)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row)
}

/// The tenant this request acts in, the roles held there and the live view-as; `None` when there is none.
async fn active(
    st: &AppState,
    caller: &Caller,
    header: Option<Uuid>,
    record: bool,
) -> ApiResult<Option<(Uuid, RoleSet, Option<ViewAs>)>> {
    if caller.super_admin {
        let t = match header {
            Some(t) => {
                let status = tenant_status(&st.db, t).await?.ok_or_else(|| {
                    ApiError::Missing("tenant_not_found", format!("no tenant {t}"))
                })?;
                if record && status != TenantStatus::Archived {
                    let platform = caller.home.as_ref().is_none_or(|h| h.tenant != t);
                    record_switch(&st.db, caller, t, platform).await?;
                }
                t
            }
            None => match caller.home.as_ref() {
                Some(h) => h.tenant,
                None => return Ok(None),
            },
        };
        let view = sa_view_as(&st.db, &caller.sub, t).await?;
        return Ok(Some((t, RoleSet::ALL, view)));
    }
    let Some(h) = caller.home.as_ref() else {
        return Ok(None);
    };
    // Read the view fresh: a view-as write in this request changed it after the caller was resolved.
    let mut tx = begin_scoped(&st.db, h.tenant).await?;
    let row: Option<(Vec<String>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT roles, expires_at FROM member_view_as WHERE sub = $1 AND expires_at > now()",
    )
    .bind(&caller.sub)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let view = row
        .map(|(roles, expires_at)| {
            Ok::<_, ApiError>(ViewAs {
                roles: RoleSet::from_db(&roles)?,
                expires_at,
            })
        })
        .transpose()?;
    Ok(Some((h.tenant, h.roles, view)))
}

/// A member of an active home tenant: record the session, and mint a pending entitlement (throttled). Never fails.
async fn member_side_effects(st: &AppState, caller: &Caller, headers: &HeaderMap) {
    let Some(h) = caller.home.as_ref() else {
        return;
    };
    if caller.super_admin || h.status != TenantStatus::Active {
        return;
    }
    seen::record(st, caller, h.tenant, seen::surface(headers)).await;
    if !st.seen.due(format!("mint\u{1f}{}", caller.sub)) {
        return;
    }
    match members::ensure_member_licence(
        st,
        h.tenant,
        &caller.sub,
        &Actor::member(&caller.sub, false),
    )
    .await
    {
        Ok(outcome) => tracing::debug!(outcome = ?outcome, "me_member_licence"),
        Err(e) => {
            metrics::counter!("agent_member_licence_errors_total", "code" => "internal")
                .increment(1);
            tracing::warn!(error = %e, "me_member_licence_failed");
        }
    }
}

/// The `Me` answer. `record`: record a super_admin's tenant switch (GET only).
async fn me_value(
    st: &AppState,
    caller: &Caller,
    header: Option<Uuid>,
    record: bool,
) -> ApiResult<Value> {
    let acting = active(st, caller, header, record).await?;
    let mut display_name = None;
    let mut tenant = Value::Null;
    let (mut roles, mut effective, mut view_as) = (RoleSet::NONE, RoleSet::NONE, Value::Null);
    if let Some((t, held, view)) = acting
        && let Some((name, status, dn)) = tenant_row(st, t, &caller.sub).await?
    {
        display_name = dn;
        tenant = json!({"id": t, "name": name, "status": status});
        roles = held;
        effective = held.effective(view.map(|v| v.roles));
        view_as = view.map_or(
            Value::Null,
            |v| json!({"roles": v.roles.keys(), "expires_at": v.expires_at}),
        );
    }
    let display_name = display_name.unwrap_or_else(|| caller.actor.clone());
    Ok(json!({
        "sub": caller.sub,
        "actor": caller.actor,
        "display_name": display_name,
        "super_admin": caller.super_admin,
        "tenant": tenant,
        "active_tenant": acting.map(|(t, _, _)| t),
        "roles": roles.keys(),
        "effective_roles": effective.keys(),
        "view_as": view_as,
    }))
}

async fn me(
    caller: Caller,
    State(st): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let header = active_tenant_from(&headers)?;
    let v = me_value(&st, &caller, header, true).await?;
    member_side_effects(&st, &caller, &headers).await;
    Ok(Json(v))
}

// ---- view-as (D79) ----------------------------------------------------------------------------------------------

/// Default and bounds of `ttl_seconds`.
pub const VIEW_AS_DEFAULT_TTL: i64 = 3600;
pub const VIEW_AS_MIN_TTL: i64 = 60;
pub const VIEW_AS_MAX_TTL: i64 = 8 * 3600;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewAsBody {
    roles: Vec<String>,
    ttl_seconds: Option<i64>,
}

/// The audit actor of a view-as write: the person, without the view (the start row carries the roles).
fn plain_actor(t: &TenantCtx) -> Actor {
    Actor::member(&t.caller.sub, t.platform_actor)
}

async fn set_view_as(
    Member(t): Member,
    State(st): State<AppState>,
    headers: HeaderMap,
    ApiJson(b): ApiJson<ViewAsBody>,
) -> ApiResult<Json<Value>> {
    let roles = RoleSet::from_keys(&b.roles)?;
    let ttl = b.ttl_seconds.unwrap_or(VIEW_AS_DEFAULT_TTL);
    if !(VIEW_AS_MIN_TTL..=VIEW_AS_MAX_TTL).contains(&ttl) {
        return Err(ApiError::Unprocessable(
            "invalid_ttl",
            format!("ttl_seconds must be {VIEW_AS_MIN_TTL}..{VIEW_AS_MAX_TTL}"),
        ));
    }
    // Narrow-only, against the roles HELD (a view never widens, and a second view replaces the first).
    if !roles.is_subset(t.roles) {
        return Err(ApiError::Unprocessable(
            "view_as_not_narrowing",
            "a view can only hold roles you hold".into(),
        ));
    }
    let ttl_i32 = i32::try_from(ttl).unwrap_or(i32::MAX);
    let actor = plain_actor(&t);
    let mut tx = t.begin(&st.db).await?;
    let expires_at: DateTime<Utc> = if t.caller.super_admin {
        let live: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT expires_at FROM agent_sa_view_as($1, $2)")
                .bind(&t.caller.sub)
                .bind(t.tenant)
                .fetch_optional(&mut *tx)
                .await?;
        if live.is_some() {
            audit_end(&mut tx, &actor, &t, "replaced").await?;
        }
        sqlx::query_scalar("SELECT agent_set_sa_view_as($1, $2, $3, $4)")
            .bind(&t.caller.sub)
            .bind(t.tenant)
            .bind(roles.keys())
            .bind(ttl_i32)
            .fetch_one(&mut *tx)
            .await?
    } else {
        let live: Option<bool> = sqlx::query_scalar(
            "SELECT expires_at > now() FROM member_view_as WHERE sub = $1 FOR UPDATE",
        )
        .bind(&t.caller.sub)
        .fetch_optional(&mut *tx)
        .await?;
        if live == Some(true) {
            audit_end(&mut tx, &actor, &t, "replaced").await?;
        }
        sqlx::query_scalar(
            "INSERT INTO member_view_as (sub, roles, started_at, expires_at) \
             VALUES ($1, $2, now(), now() + make_interval(secs => $3)) \
             ON CONFLICT (sub) DO UPDATE \
                SET tenant_id = EXCLUDED.tenant_id, roles = EXCLUDED.roles, \
                    started_at = EXCLUDED.started_at, expires_at = EXCLUDED.expires_at \
             RETURNING expires_at",
        )
        .bind(&t.caller.sub)
        .bind(roles.keys())
        .bind(f64::from(ttl_i32))
        .fetch_one(&mut *tx)
        .await?
    };
    audit::record(
        &mut tx,
        &actor,
        Event::new("user.view_as.start", "user", t.caller.sub.clone())
            .detail("roles", roles.keys())
            .detail("expires_at", expires_at.to_rfc3339()),
    )
    .await?;
    tx.commit().await?;
    let header = active_tenant_from(&headers)?;
    Ok(Json(me_value(&st, &t.caller, header, false).await?))
}

async fn audit_end(
    tx: &mut sqlx::PgConnection,
    actor: &Actor,
    t: &TenantCtx,
    reason: &'static str,
) -> ApiResult<()> {
    audit::record(
        tx,
        actor,
        Event::new("user.view_as.end", "user", t.caller.sub.clone()).detail("reason", reason),
    )
    .await?;
    Ok(())
}

/// Always allowed for a member (it reads no role), idempotent: no live view → 200 and no audit row.
async fn clear_view_as(
    Member(t): Member,
    State(st): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let actor = plain_actor(&t);
    let mut tx = t.begin(&st.db).await?;
    let live: bool = if t.caller.super_admin {
        sqlx::query_scalar("SELECT agent_clear_sa_view_as($1, $2)")
            .bind(&t.caller.sub)
            .bind(t.tenant)
            .fetch_one(&mut *tx)
            .await?
    } else {
        sqlx::query_scalar::<_, bool>(
            "DELETE FROM member_view_as WHERE sub = $1 RETURNING expires_at > now()",
        )
        .bind(&t.caller.sub)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false)
    };
    if live {
        audit_end(&mut tx, &actor, &t, "cleared").await?;
    }
    tx.commit().await?;
    let header = active_tenant_from(&headers)?;
    Ok(Json(me_value(&st, &t.caller, header, false).await?))
}
