//! Tenant read models (B1 T10, X7): the `Tenant`, `TenantUser`, licence and offboard answers of the UI
//! (`web/src/lib/api/types/{core,platform,tenant}.ts`), read inside a transaction scoped to the tenant.
//!
//! The licence (X7 `tenant::licence`, D77 L8) is derived from the tenant licence (`tenants.licence_id`, L1), the
//! caller's own `member_entitlement` row (L2) and the tenant status. B7 adds the keyring `kid` and the gate-2 verdict
//! cache; until then `kid` is null (`v1:` tokens carry none), `checked_at` is now and nothing is platform-owned. The
//! sealed token never leaves this module.
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::ApiResult;
use crate::identity::tenant_licence_subject;

/// The `tenants` columns every tenant answer reads.
pub const TENANT_COLUMNS: &str = "id, name, status, residency, brand, description, industry, created_at, \
     suspended_at, archived_at, licence_id, support_auto_assign, attachment_quota_bytes";

/// One `tenants` row (no token).
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct TenantRow {
    pub id: Uuid,
    pub name: String,
    pub status: String,
    pub residency: String,
    pub brand: Option<String>,
    pub description: String,
    pub industry: String,
    pub created_at: DateTime<Utc>,
    pub suspended_at: Option<DateTime<Utc>>,
    pub archived_at: Option<DateTime<Utc>>,
    pub licence_id: Option<String>,
    /// SW-7: the platform flag in front of support auto-assignment (0207). `default`: the registry rollup
    /// (`agent_platform_tenant_rollup`, flattened into `RollupRow`) does not select it and never shows it.
    #[sqlx(default)]
    pub support_auto_assign: bool,
    /// The tenant's own attachment storage quota in bytes (0224); `None` = the platform default
    /// (`ATTACHMENT_QUOTA_BYTES`). Only a super_admin sets it. `default` for the same reason as the flag above.
    #[sqlx(default)]
    pub attachment_quota_bytes: Option<i64>,
}

/// The tenant `id` (the connection must be scoped to it); `None` when it does not exist.
pub async fn read(conn: &mut PgConnection, id: Uuid) -> ApiResult<Option<TenantRow>> {
    Ok(sqlx::query_as::<_, TenantRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {TENANT_COLUMNS} FROM tenants WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(conn)
    .await?)
}

/// `LicenceSummary.status`: `none` without a tenant licence, else by tenant status.
pub fn licence_status(t: &TenantRow) -> &'static str {
    match (t.licence_id.is_some(), t.status.as_str()) {
        (false, _) => "none",
        (true, "archived") => "revoked",
        (true, "suspended") => "inactive",
        (true, _) => "active",
    }
}

/// `LicenceSummary` (core.ts).
pub fn summary(t: &TenantRow, plan: &str) -> Value {
    json!({"status": licence_status(t), "plan": plan, "kid": null, "platform_owned": false})
}

/// `Tenant` (core.ts).
pub fn tenant_json(t: &TenantRow, plan: &str) -> Value {
    json!({
        "id": t.id,
        "name": t.name,
        "status": t.status,
        "brand": t.brand,
        "description": t.description,
        "industry": t.industry,
        "created_at": t.created_at,
        "suspended_at": t.suspended_at,
        "archived_at": t.archived_at,
        "licence": summary(t, plan),
        "residency": t.residency,
    })
}

/// The caller's own entitlement: (status, license_id, error_code).
type Entitlement = (String, Option<String>, Option<String>);

/// Why the licence is not (or not fully) usable, as a code; `None` when it is.
fn reason(t: &TenantRow, member: Option<&Entitlement>) -> Option<&'static str> {
    if t.licence_id.is_none() {
        return Some("tenant_licence_pending");
    }
    match t.status.as_str() {
        "archived" => return Some("tenant_archived"),
        "suspended" => return Some("tenant_suspended"),
        _ => {}
    }
    match member.map(|(s, _, _)| s.as_str()) {
        Some("pending") => Some("member_licence_pending"),
        Some("revoking" | "revoked") => Some("member_licence_revoked"),
        Some("error") => Some("member_licence_error"),
        _ => None,
    }
}

/// The tenant's licence for `sub` (the caller; `None` on platform routes): `(LicenceSummary, tenant
/// LicenceVerdict, Option<TenantLicence>)` (X7). The connection must be scoped to the tenant.
pub async fn licence(
    conn: &mut PgConnection,
    t: &TenantRow,
    plan: &str,
    sub: Option<&str>,
) -> ApiResult<(Value, Value, Value)> {
    let member: Option<Entitlement> =
        match sub {
            Some(s) => sqlx::query_as(
                "SELECT status, license_id, error_code FROM member_entitlement WHERE user_sub = $1",
            )
            .bind(s)
            .fetch_optional(&mut *conn)
            .await?,
            None => None,
        };
    let verdict = json!({
        "status": licence_status(t),
        "app": "agent",
        "plan": plan,
        "kid": null,
        "licence_id": t.licence_id,
        "holder_sub": t.licence_id.as_ref().map(|_| tenant_licence_subject(t.id)),
        "checked_at": Utc::now(),
        "reason": reason(t, member.as_ref()),
        "platform_owned": false,
    });
    Ok((summary(t, plan), verdict, tenant_licence(t, plan)))
}

/// `TenantLicence` (platform.ts) of the tenant licence, `null` while none is stored. `subject_id` is the holder
/// (the tenant licence's synthetic subject); `issued_at` is the tenant's creation until B7 records the issue time.
pub fn tenant_licence(t: &TenantRow, plan: &str) -> Value {
    match &t.licence_id {
        None => Value::Null,
        Some(id) => json!({
            "licence_id": id,
            "subject_id": tenant_licence_subject(t.id),
            "holder_sub": tenant_licence_subject(t.id),
            "app": "agent",
            "plan": plan,
            "kid": null,
            "introspect_status": match licence_status(t) {
                "revoked" => "revoked",
                "inactive" => "inactive",
                _ => "active",
            },
            "issued_at": t.created_at,
        }),
    }
}

/// The platform `LicenceVerdict` (platform.ts) of the tenant licence; `null` while none is stored.
pub fn platform_verdict(t: &TenantRow) -> Value {
    if t.licence_id.is_none() {
        return Value::Null;
    }
    let status = licence_status(t);
    json!({
        "tenant_id": t.id,
        "status": status,
        "checked_at": Utc::now(),
        "reason": reason(t, None),
        "age_seconds": 0,
        "fails_closed": status != "active",
    })
}

/// The `users` columns a `TenantUser` reads.
pub const USER_COLUMNS: &str =
    "sub, tenant_id, display_name, email, roles, title, disabled_at, invited_at, created_at";

/// One `users` row.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct UserRow {
    pub sub: String,
    pub tenant_id: Uuid,
    pub display_name: Option<String>,
    pub email: Option<String>,
    /// The role keys held, in §R.1 order (every write stores `RoleSet::keys()`); empty = observer.
    pub roles: Vec<String>,
    pub title: Option<String>,
    pub disabled_at: Option<DateTime<Utc>>,
    pub invited_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// `TenantUser` (core.ts). A missing name reads as the sub, a missing address as empty.
pub fn user_json(u: &UserRow) -> Value {
    json!({
        "sub": u.sub,
        "tenant_id": u.tenant_id,
        "display_name": u.display_name.clone().unwrap_or_else(|| u.sub.clone()),
        "email": u.email.clone().unwrap_or_default(),
        "roles": u.roles,
        "title": u.title,
        "disabled_at": u.disabled_at,
        "invited_at": u.invited_at,
        "created_at": u.created_at,
    })
}

/// Every member of the scoped tenant (disabled and invited included), by name.
pub async fn users(conn: &mut PgConnection) -> ApiResult<Vec<UserRow>> {
    Ok(sqlx::query_as::<_, UserRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {USER_COLUMNS} FROM users ORDER BY lower(coalesce(display_name, sub)), sub"
    )))
    .fetch_all(conn)
    .await?)
}

/// One member of the scoped tenant.
pub async fn user(conn: &mut PgConnection, sub: &str) -> ApiResult<Option<UserRow>> {
    Ok(sqlx::query_as::<_, UserRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {USER_COLUMNS} FROM users WHERE sub = $1"
    )))
    .bind(sub)
    .fetch_optional(conn)
    .await?)
}

/// The stored `OffboardResult` (platform.ts) of the scoped tenant, `None` before an offboard. `licence_revoked` is read
/// now: true once no revoke is parked (`tenants.licence_revoke_pending`) and no member licence is still `revoking`.
pub async fn offboard(conn: &mut PgConnection, tenant: Uuid) -> ApiResult<Option<Value>> {
    type Stored = (DateTime<Utc>, String, i32, Value, Value);
    let row: Option<Stored> = sqlx::query_as(
        "SELECT at, \"by\", users_disabled, outbox, data_retained FROM tenant_offboard WHERE tenant_id = $1",
    )
    .bind(tenant)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((at, by, users_disabled, outbox, data_retained)) = row else {
        return Ok(None);
    };
    let revoked: bool = sqlx::query_scalar(
        "SELECT cardinality(t.licence_revoke_pending) = 0 \
                AND NOT EXISTS (SELECT 1 FROM member_entitlement e WHERE e.status = 'revoking') \
           FROM tenants t WHERE t.id = $1",
    )
    .bind(tenant)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Some(json!({
        "tenant_id": tenant,
        "at": at,
        "by": by,
        "licence_revoked": revoked,
        "archived": true,
        "users_disabled": users_disabled,
        "outbox": outbox.get("rows").cloned().unwrap_or_else(|| json!([])),
        "data_retained": data_retained.get("items").cloned().unwrap_or_else(|| json!([])),
        "complete": false,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(status: &str, licence: Option<&str>) -> TenantRow {
        TenantRow {
            id: Uuid::nil(),
            name: "T".into(),
            status: status.into(),
            residency: "eu".into(),
            brand: None,
            description: String::new(),
            industry: "unassigned".into(),
            created_at: Utc::now(),
            suspended_at: None,
            archived_at: None,
            licence_id: licence.map(str::to_string),
            support_auto_assign: false,
            attachment_quota_bytes: None,
        }
    }

    #[test]
    fn licence_status_by_tenant_status() {
        assert_eq!(licence_status(&row("active", None)), "none");
        assert_eq!(licence_status(&row("active", Some("l"))), "active");
        assert_eq!(licence_status(&row("suspended", Some("l"))), "inactive");
        assert_eq!(licence_status(&row("archived", Some("l"))), "revoked");
        assert!(tenant_licence(&row("active", None), "standard").is_null());
        assert!(platform_verdict(&row("active", None)).is_null());
        let v = tenant_licence(&row("active", Some("lic-1")), "standard");
        assert_eq!(v["licence_id"], "lic-1");
        assert!(v.get("token").is_none() && v.get("token_encrypted").is_none());
    }
}
