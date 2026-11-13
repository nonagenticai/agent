//! Provisioning Retry and Cancel (B1 T15; P4 T7, spec 06 §4.6): the tenant routes (`/api/tenant/provisioning/{id}/…`,
//! tenant shape) and the super_admin's platform routes (`/api/tenants/{tenant_id}/provisioning/{row}/…`, platform shape).
//!
//! Retry: `pending|failed|dead` → `pending`, attempts 0, due now; anything else is 409 `not_retryable`. Cancel:
//! `pending|failed|dead` → `cancelled`; anything else is 409 `not_cancellable`. Each is audited in the row's tenant
//! (`provision.retried|cancelled`). A platform route finds the row's tenant (`agent_provision_row_tenant`, as
//! `agent_platform`); a path tenant that is not the row's is 404, an archived tenant 409 `tenant_archived`.
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{self, Actor, Event};
use crate::error::{ApiError, ApiPath, ApiResult};
use crate::platform_db::begin_platform;
use crate::provision::{OutboxRow, ROW_COLUMNS};
use crate::state::AppState;
use crate::tenancy::{Requires, SuperAdmin, TenantAdmin, tenant_id};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/tenant/provisioning/{id}/retry", post(tenant_retry))
        .route("/api/tenant/provisioning/{id}/cancel", post(tenant_cancel))
        .route(
            "/api/tenants/{tenant_id}/provisioning/{row}/retry",
            post(platform_retry),
        )
        .route(
            "/api/tenants/{tenant_id}/provisioning/{row}/cancel",
            post(platform_cancel),
        )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Act {
    Retry,
    Cancel,
}

/// Retry or cancel row `id` of the scoped tenant; `missing` is the 404 when the tenant has no such row.
async fn act(
    conn: &mut PgConnection,
    id: Uuid,
    what: Act,
    actor: &Actor,
    missing: fn() -> ApiError,
) -> ApiResult<OutboxRow> {
    let row: Option<OutboxRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ROW_COLUMNS} FROM provision_outbox WHERE id = $1 FOR UPDATE"
    )))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    let row = row.ok_or_else(missing)?;
    if !matches!(row.status.as_str(), "pending" | "failed" | "dead") {
        return Err(match what {
            Act::Retry => ApiError::Conflict(
                "not_retryable",
                format!("a {} row cannot be retried", row.status),
            ),
            Act::Cancel => ApiError::Conflict(
                "not_cancellable",
                format!("a {} row cannot be cancelled", row.status),
            ),
        });
    }
    let (sql, action) = match what {
        Act::Retry => (
            "UPDATE provision_outbox SET status = 'pending', attempts = 0, next_attempt_at = now(), \
                    lease_until = NULL, lease_token = NULL, completed_at = NULL, updated_at = now() \
              WHERE id = $1",
            "provision.retried",
        ),
        Act::Cancel => (
            "UPDATE provision_outbox SET status = 'cancelled', lease_until = NULL, lease_token = NULL, \
                    completed_at = now(), updated_at = now() \
              WHERE id = $1",
            "provision.cancelled",
        ),
    };
    sqlx::query(sql).bind(id).execute(&mut *conn).await?;
    audit::record(
        &mut *conn,
        actor,
        Event::new(action, "provision_outbox", id.to_string())
            .detail("op", row.op.clone())
            .detail("target", row.target.clone())
            .detail("from", row.status.clone()),
    )
    .await?;
    let now: OutboxRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ROW_COLUMNS} FROM provision_outbox WHERE id = $1"
    )))
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(now)
}

#[derive(Deserialize)]
struct RowPath {
    id: String,
}

fn provision_not_found() -> ApiError {
    ApiError::Missing("provision_not_found", "no such provisioning row".into())
}

fn row_id(raw: &str, missing: fn() -> ApiError) -> ApiResult<Uuid> {
    Uuid::parse_str(raw).map_err(|_| missing())
}

async fn tenant_act(
    st: &AppState,
    t: &crate::tenancy::TenantCtx,
    raw: &str,
    what: Act,
) -> ApiResult<Json<Value>> {
    let id = row_id(raw, provision_not_found)?;
    let mut tx = t.begin(&st.db).await?;
    let row = act(&mut tx, id, what, &t.actor(), provision_not_found).await?;
    tx.commit().await?;
    Ok(Json(row.tenant_json()))
}

async fn tenant_retry(
    Requires(t, _): TenantAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<RowPath>,
) -> ApiResult<Json<Value>> {
    tenant_act(&st, &t, &p.id, Act::Retry).await
}

async fn tenant_cancel(
    Requires(t, _): TenantAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<RowPath>,
) -> ApiResult<Json<Value>> {
    tenant_act(&st, &t, &p.id, Act::Cancel).await
}

#[derive(Deserialize)]
struct PlatformPath {
    tenant_id: String,
    row: String,
}

fn not_found() -> ApiError {
    ApiError::Missing(
        "not_found",
        "no such provisioning row for this tenant".into(),
    )
}

async fn platform_act(
    st: &AppState,
    caller: &crate::auth::Caller,
    p: &PlatformPath,
    what: Act,
) -> ApiResult<Json<Value>> {
    let tenant = tenant_id(&p.tenant_id)?;
    let id = row_id(&p.row, not_found)?;
    let mut tx = begin_platform(&st.db).await?;
    let owner: Option<Uuid> = sqlx::query_scalar("SELECT agent_provision_row_tenant($1)")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    if owner != Some(tenant) {
        return Err(not_found());
    }
    let (mut tx, t) = crate::routes::tenants::scoped(st, tenant).await?;
    if t.status == "archived" {
        return Err(ApiError::Conflict(
            "tenant_archived",
            "this tenant is archived".into(),
        ));
    }
    let row = act(&mut tx, id, what, &Actor::person(caller), not_found).await?;
    let state: Option<(Option<chrono::DateTime<chrono::Utc>>, Option<Value>)> =
        sqlx::query_as("SELECT last_success_at, drift FROM provision_state WHERE target = $1")
            .bind(&row.target)
            .fetch_optional(&mut *tx)
            .await?;
    tx.commit().await?;
    let (last, drift) = state.unwrap_or((None, None));
    Ok(Json(row.platform_json(last, drift.as_ref())))
}

async fn platform_retry(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<PlatformPath>,
) -> ApiResult<Json<Value>> {
    platform_act(&st, &c, &p, Act::Retry).await
}

async fn platform_cancel(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<PlatformPath>,
) -> ApiResult<Json<Value>> {
    platform_act(&st, &c, &p, Act::Cancel).await
}
