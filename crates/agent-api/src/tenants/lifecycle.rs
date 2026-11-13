//! Tenant lifecycle (B1 T13; P3 T13/T15, spec 04 §7b, D77): create / import, suspend / resume, offboard.
//!
//! Create, in this order: validate (422) → name taken (409) → resolve the first admin in Keycloak (an email: the
//! existing identity or a new one; a sub: it must exist) → a member elsewhere is 409 `user_exists` (no licences call; the
//! portal identity stays, harmless) → ONE mixed-role transaction: the tenant (no licence yet, L1), its first admin
//! (every role, D88; origin `tenant_create`), the admin's `pending` entitlement (L2), the tenant audit rows, the
//! provisioning hook, the platform audit row → COMMIT. Only then (L10) the tenant licence and the admin's member licence
//! are minted and stored (conditional UPDATE, L4); a licences refusal is platform-audited `license.issue_failed` and
//! leaves the rows for the provisioning tick (L7): the create still answers 201. A failure before COMMIT minted
//! nothing, so there is nothing to give back.
//!
//! Offboard: archive, disable every member, set every member licence `revoking`, cancel pending data-plane rows, store
//! the `OffboardResult` and audit both logs in one transaction; after COMMIT revoke each member licence and the tenant
//! licence, reading the body (a 200 can be a no-op). A licence that did not revoke is parked in
//! `tenants.licence_revoke_pending` and audited `license.revoke_failed`; the offboard still completes (L6). Keycloak is
//! never touched and no licence row is deleted.
use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit::{self, Actor, Event, PlatformEvent};
use crate::auth::RoleSet;
use crate::error::{ApiError, ApiResult};
use crate::identity::{
    PersonNames, RevokeOutcome, Stored, keycloak_error, lock_licence, store_tenant_licence,
};
use crate::licensing::store::{Via, record_revocation};
use crate::members::{self, MintOutcome, RevokeResult, entitle_pending, park, revoke_code};
use crate::platform_db::{enter, leave};
use crate::readmodel::{self, platform, tenant::TenantRow};
use crate::state::AppState;
use crate::tenancy::{begin_scoped, lock_tenant_row};

/// Who becomes the first member (every role, D88).
#[derive(Clone, Debug)]
pub enum FirstAdmin {
    /// An email address (lower-cased): the existing portal identity, else a new one (then invited). The names are what a
    /// new identity is created with (or fill a reused one's empty names), so the invitee only sets a password.
    Email(String, PersonNames),
    /// A Keycloak subject that must exist.
    Sub(String),
}

/// Where the tenant comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Manual,
    /// An existing tenant id kept as it is.
    Import(Uuid),
}

#[derive(Clone, Debug)]
pub struct CreateInput {
    pub name: String,
    pub first_admin: FirstAdmin,
    pub residency: String,
    pub origin: Origin,
}

/// What a create did.
#[derive(Clone, Debug)]
pub struct Created {
    pub tenant: Uuid,
    /// The tenant licence, when the mint after COMMIT succeeded.
    pub licence_id: Option<String>,
    /// The first admin had no portal identity: one was created and invited.
    pub invited: bool,
}

/// A tenant name: one line of 1 to 120 characters, else 422 `invalid_name`.
pub fn tenant_name(raw: &str) -> ApiResult<String> {
    let n = raw.trim();
    if n.is_empty() || n.chars().count() > 120 || n.chars().any(char::is_control) {
        return Err(ApiError::Unprocessable(
            "invalid_name",
            "name must be one line of 1 to 120 characters".into(),
        ));
    }
    Ok(n.to_string())
}

/// D66: `eu` or `us`, required; else 422 `invalid_residency`.
pub fn residency(raw: Option<&str>) -> ApiResult<&'static str> {
    match raw.map(str::trim) {
        Some("eu") => Ok("eu"),
        Some("us") => Ok("us"),
        _ => Err(ApiError::Unprocessable(
            "invalid_residency",
            "residency must be eu or us (chosen once, immutable after)".into(),
        )),
    }
}

fn is_unique_violation(e: &sqlx::Error) -> Option<String> {
    let d = e.as_database_error()?;
    (d.code().as_deref() == Some("23505")).then(|| d.constraint().unwrap_or_default().to_string())
}

fn name_taken() -> ApiError {
    ApiError::Conflict("tenant_name_taken", "a tenant with that name exists".into())
}

fn user_exists() -> ApiError {
    ApiError::Conflict(
        "user_exists",
        "the first admin is already a member of a tenant".into(),
    )
}

/// A licences failure as a code: `licences_unavailable` (retryable) or `licence_refused`.
fn licences_code(status: Option<u16>) -> &'static str {
    if status.is_none_or(|s| s >= 500 || s == 408 || s == 429) {
        "licences_unavailable"
    } else {
        "licence_refused"
    }
}

/// Create (or import) a tenant with its first admin. See the module doc for the order.
pub async fn create_tenant(st: &AppState, actor: &Actor, input: CreateInput) -> ApiResult<Created> {
    let name = tenant_name(&input.name)?;
    let residency = residency(Some(&input.residency))?;
    let (action, id, description) = match input.origin {
        Origin::Manual => ("tenant.create", Uuid::now_v7(), ""),
        Origin::Import(id) => ("tenant.import", id, "Imported with its existing tenant id"),
    };
    let key = name.to_lowercase();
    if platform::rollup(&st.db)
        .await?
        .iter()
        .any(|r| r.t.name.trim().to_lowercase() == key)
    {
        return Err(name_taken());
    }
    let (sub, existed, email, display) = match &input.first_admin {
        FirstAdmin::Email(e, names) => {
            let local = e.split('@').next().unwrap_or_default().to_string();
            let (sub, existed) = st
                .identity
                .resolve_or_create(e, names, None)
                .await
                .map_err(|x| keycloak_error(&x))?;
            (
                sub,
                existed,
                Some(e.clone()),
                names.joined().unwrap_or(local),
            )
        }
        FirstAdmin::Sub(s) => {
            let u = st
                .identity
                .get_user(s)
                .await
                .map_err(|x| keycloak_error(&x))?
                .ok_or_else(|| {
                    ApiError::Unprocessable(
                        "invalid_admin",
                        "first_admin names no portal account".into(),
                    )
                })?;
            let display = u.name.clone().unwrap_or_else(|| s.clone());
            (u.sub, true, u.email.map(|e| e.to_lowercase()), display)
        }
    };
    let home: Option<Uuid> = sqlx::query_scalar("SELECT tenant_id FROM agent_auth_user($1)")
        .bind(&sub)
        .fetch_optional(&st.db)
        .await?;
    if home.is_some() {
        return Err(user_exists());
    }

    let mut tx = begin_scoped(&st.db, id).await?;
    sqlx::query(
        "INSERT INTO tenants (id, name, residency, description, updated_by) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(&name)
    .bind(residency)
    .bind(description)
    .bind(&actor.id)
    .execute(&mut *tx)
    .await
    .map_err(|e| match is_unique_violation(&e) {
        Some(c) if c.contains("name") => name_taken(),
        Some(_) => ApiError::Conflict("tenant_exists", "that tenant id exists".into()),
        None => e.into(),
    })?;
    sqlx::query(
        "INSERT INTO users (sub, roles, display_name, email, origin, invited_at, added_by, updated_by) \
         VALUES ($1, agent_role_keys(), $2, $3, 'tenant_create', CASE WHEN $4 THEN now() END, $5, $5)",
    )
    .bind(&sub)
    .bind(&display)
    .bind(email.as_deref())
    .bind(!existed)
    .bind(&actor.id)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        if is_unique_violation(&e).is_some() {
            user_exists()
        } else {
            e.into()
        }
    })?;
    entitle_pending(&mut tx, id, &sub).await?;
    audit::record(
        &mut tx,
        actor,
        Event::new(action, "tenant", id.to_string()).detail("residency", residency),
    )
    .await?;
    audit::record(
        &mut tx,
        actor,
        Event::new("user.add", "user", sub.clone()).detail("roles", RoleSet::ALL.keys()),
    )
    .await?;
    crate::provision::on_tenant_created(&mut tx, id, actor).await?;
    enter(&mut tx).await?;
    audit::record_platform(
        &mut tx,
        actor,
        PlatformEvent::new(action, name.clone())
            .tenant(id)
            .detail("residency", residency),
    )
    .await?;
    leave(&mut tx).await?;
    tx.commit().await?;

    // After COMMIT (D77 L10): the tenant licence, then the first admin's member licence.
    let licence_id = match ensure_tenant_licence(st, id, actor).await {
        Ok(MintOutcome::Stored(l)) => Some(l),
        Ok(MintOutcome::Failed(code)) => {
            issue_failed(st, actor, id, &name, "tenant", code).await;
            None
        }
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(tenant = %id, error = %e, "tenant_licence_after_create_failed");
            None
        }
    };
    match members::ensure_member_licence(st, id, &sub, actor).await {
        Ok(MintOutcome::Failed(code)) => issue_failed(st, actor, id, &name, "member", code).await,
        Ok(_) => {}
        Err(e) => tracing::warn!(tenant = %id, error = %e, "member_licence_after_create_failed"),
    }
    let sent = st.identity.send_access(&sub, existed).await;
    members::access_email_outcome(st, id, actor, &sub, "first_admin", &sent).await;
    Ok(Created {
        tenant: id,
        licence_id,
        invited: !existed,
    })
}

/// Platform audit `license.issue_failed` (never fails the caller).
async fn issue_failed(
    st: &AppState,
    actor: &Actor,
    tenant: Uuid,
    name: &str,
    holder: &'static str,
    code: &'static str,
) {
    let e = PlatformEvent::new("license.issue_failed", name)
        .tenant(tenant)
        .failed(code)
        .detail("holder", holder);
    if let Err(err) = audit::record_platform_alone(&st.db, actor, e).await {
        tracing::warn!(%tenant, error = %err, "licence_issue_failed_audit_failed");
    }
}

/// Mint and store the tenant licence if the tenant is `active` and has none (D77 L1/L4/L7): called after COMMIT,
/// never inside a transaction. A lost race revokes this run's token.
pub async fn ensure_tenant_licence(
    st: &AppState,
    tenant: Uuid,
    actor: &Actor,
) -> ApiResult<MintOutcome> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let row: Option<(String, Option<String>, String, String)> =
        sqlx::query_as("SELECT name, licence_id, status, residency FROM tenants WHERE id = $1")
            .bind(tenant)
            .fetch_optional(&mut *tx)
            .await?;
    tx.commit().await?;
    let Some((name, None, status, residency)) = row else {
        return Ok(MintOutcome::NotPending);
    };
    if status != "active" {
        return Ok(MintOutcome::NotPending);
    }
    let issued = match st
        .identity
        .issue_tenant_licence(tenant, &name, &residency)
        .await
    {
        Ok(l) => l,
        Err(e) => {
            let code = licences_code(e.status);
            metrics::counter!("agent_tenant_licence_errors_total", "code" => code).increment(1);
            tracing::warn!(%tenant, code, error = %e, "tenant_licence_mint_failed");
            return Ok(MintOutcome::Failed(code));
        }
    };
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let stored = store_tenant_licence(&mut tx, tenant, &issued).await?;
    if stored == Stored::Stale {
        // Review #8: licences answered an id agent revoked since; the tick mints again.
        tx.commit().await?;
        let code = "licence_stale";
        metrics::counter!("agent_tenant_licence_errors_total", "code" => code).increment(1);
        return Ok(MintOutcome::Failed(code));
    }
    if stored == Stored::Stored {
        audit::record(
            &mut tx,
            actor,
            Event::new("license.issue", "tenant", tenant.to_string())
                .detail("license_id", issued.license_id.clone()),
        )
        .await?;
        tx.commit().await?;
        return Ok(MintOutcome::Stored(issued.license_id));
    }
    let winner: Option<String> = sqlx::query_scalar("SELECT licence_id FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    if winner.as_deref() != Some(issued.license_id.as_str()) {
        let r = st.identity.revoke_licence(tenant, &issued.license_id).await;
        members::orphan_if_unrevoked(st, tenant, &issued.license_id, &r).await;
    }
    Ok(MintOutcome::LostRace)
}

/// SW-7 (owner 2026-10-27): switch the tenant's support auto-assignment flag (`tenants.support_auto_assign`, 0207, OFF
/// by default). The ONLY writer of the flag and of `support_auto_assign_set_by|at`; its one caller is the super_admin
/// `PATCH /api/tenants/{id}`. 409 `tenant_archived` on an archived tenant; the same value is a no-op without audit.
/// Audited `tenant.support_auto_assign` (`enabled`) in both logs, in the update's transaction.
pub async fn set_support_auto_assign(
    st: &AppState,
    actor: &Actor,
    tenant: Uuid,
    enabled: bool,
) -> ApiResult<TenantRow> {
    let (mut tx, row) = crate::routes::tenants::scoped(st, tenant).await?;
    if row.status == "archived" {
        return Err(ApiError::Conflict(
            "tenant_archived",
            "this tenant is archived".into(),
        ));
    }
    if row.support_auto_assign == enabled {
        tx.commit().await?;
        return Ok(row);
    }
    sqlx::query(
        "UPDATE tenants SET support_auto_assign = $2, support_auto_assign_set_by = $3, \
                support_auto_assign_set_at = now(), updated_at = now(), updated_by = $3 WHERE id = $1",
    )
    .bind(tenant)
    .bind(enabled)
    .bind(&actor.id)
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        actor,
        Event::new("tenant.support_auto_assign", "tenant", tenant.to_string())
            .detail("enabled", enabled),
    )
    .await?;
    let now = readmodel::tenant::read(&mut tx, tenant)
        .await?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("tenant {tenant} vanished")))?;
    enter(&mut tx).await?;
    audit::record_platform(
        &mut tx,
        actor,
        PlatformEvent::new("tenant.support_auto_assign", row.name.clone())
            .tenant(tenant)
            .detail("enabled", enabled),
    )
    .await?;
    leave(&mut tx).await?;
    tx.commit().await?;
    Ok(now)
}

/// The largest per-tenant attachment quota a super_admin may set: 1 TiB.
pub const MAX_ATTACHMENT_QUOTA_BYTES: i64 = 1 << 40;

/// SW-9 (owner 2026-10-28): set the tenant's attachment storage quota (`tenants.attachment_quota_bytes`, 0224); `None`
/// returns it to the platform default (`ATTACHMENT_QUOTA_BYTES`). The ONLY writer of the column and of
/// `attachment_quota_set_by|at`; its one caller is the super_admin `PATCH /api/tenants/{id}`. 422 `invalid_quota`
/// outside 1 B to 1 TiB; 409 `tenant_archived`; the same value is a no-op without audit. Audited
/// `tenant.attachment_quota` (`quota_bytes`, null = default) in both logs, in the update's transaction.
pub async fn set_attachment_quota(
    st: &AppState,
    actor: &Actor,
    tenant: Uuid,
    quota_bytes: Option<i64>,
) -> ApiResult<TenantRow> {
    if quota_bytes.is_some_and(|q| !(1..=MAX_ATTACHMENT_QUOTA_BYTES).contains(&q)) {
        return Err(ApiError::Unprocessable(
            "invalid_quota",
            "The attachment quota is between 1 byte and 1 TiB, or null for the platform default."
                .into(),
        ));
    }
    let (mut tx, row) = crate::routes::tenants::scoped(st, tenant).await?;
    if row.status == "archived" {
        return Err(ApiError::Conflict(
            "tenant_archived",
            "this tenant is archived".into(),
        ));
    }
    if row.attachment_quota_bytes == quota_bytes {
        tx.commit().await?;
        return Ok(row);
    }
    sqlx::query(
        "UPDATE tenants SET attachment_quota_bytes = $2, attachment_quota_set_by = $3, \
                attachment_quota_set_at = now(), updated_at = now(), updated_by = $3 WHERE id = $1",
    )
    .bind(tenant)
    .bind(quota_bytes)
    .bind(&actor.id)
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        actor,
        Event::new("tenant.attachment_quota", "tenant", tenant.to_string())
            .detail("quota_bytes", quota_bytes),
    )
    .await?;
    let now = readmodel::tenant::read(&mut tx, tenant)
        .await?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("tenant {tenant} vanished")))?;
    enter(&mut tx).await?;
    audit::record_platform(
        &mut tx,
        actor,
        PlatformEvent::new("tenant.attachment_quota", row.name.clone())
            .tenant(tenant)
            .detail("quota_bytes", quota_bytes),
    )
    .await?;
    leave(&mut tx).await?;
    tx.commit().await?;
    Ok(now)
}

/// `active` ↔ `suspended` (409 `tenant_archived` on an archived tenant; the same status is a no-op without audit).
/// Audited `tenant.suspend|resume` in both logs. Returns the tenant row as it is now.
pub async fn set_status(
    st: &AppState,
    actor: &Actor,
    tenant: Uuid,
    status: &str,
) -> ApiResult<TenantRow> {
    let status = match status.trim() {
        "active" => "active",
        "suspended" => "suspended",
        _ => {
            return Err(ApiError::Unprocessable(
                "invalid_status",
                "status must be active or suspended (offboard archives)".into(),
            ));
        }
    };
    let (mut tx, row) = crate::routes::tenants::scoped(st, tenant).await?;
    if row.status == "archived" {
        return Err(ApiError::Conflict(
            "tenant_archived",
            "this tenant is archived".into(),
        ));
    }
    if row.status == status {
        tx.commit().await?;
        return Ok(row);
    }
    sqlx::query(
        "UPDATE tenants SET status = $2, \
                suspended_at = CASE WHEN $2 = 'suspended' THEN now() END, \
                updated_at = now(), updated_by = $3 \
          WHERE id = $1",
    )
    .bind(tenant)
    .bind(status)
    .bind(&actor.id)
    .execute(&mut *tx)
    .await?;
    let action = if status == "suspended" {
        "tenant.suspend"
    } else {
        "tenant.resume"
    };
    audit::record(
        &mut tx,
        actor,
        Event::new(action, "tenant", tenant.to_string()),
    )
    .await?;
    let now = readmodel::tenant::read(&mut tx, tenant)
        .await?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("tenant {tenant} vanished")))?;
    enter(&mut tx).await?;
    audit::record_platform(
        &mut tx,
        actor,
        PlatformEvent::new(action, row.name.clone()).tenant(tenant),
    )
    .await?;
    leave(&mut tx).await?;
    tx.commit().await?;
    // B4 T1: a suspended tenant's scopes leave the runtime registry (its scope tasks stop); a resume brings them back.
    crate::runtime::scopes::changed(st, tenant).await;
    Ok(now)
}

/// Rename `tenant` (DR, owner ruling 2026-10-23): the name is validated as on create (422 `invalid_name`); another
/// tenant holding it, compared trimmed and case-insensitively as `tenants_name_lower` does (archived tenants
/// included), is 409 `tenant_name_taken`; an unknown tenant is 404 `tenant_not_found`. Any status may be renamed
/// (an archived tenant keeps its name until renamed, so a recreated demo can take the name back). The same name is
/// a no-op without audit. Audited `tenant.rename` (`old_name`, `new_name`) in both logs, in the update's transaction.
pub async fn rename(st: &AppState, actor: &Actor, tenant: Uuid, raw: &str) -> ApiResult<TenantRow> {
    let name = tenant_name(raw)?;
    let (mut tx, _) = crate::routes::tenants::scoped(st, tenant).await?;
    // Concurrent renames (and an offboard) serialise on the row; the re-read below is the locked state.
    sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR UPDATE")
        .bind(tenant)
        .execute(&mut *tx)
        .await?;
    let row = readmodel::tenant::read(&mut tx, tenant)
        .await?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("tenant {tenant} vanished")))?;
    if row.name == name {
        tx.commit().await?;
        return Ok(row);
    }
    // Other tenants are invisible under the tenant scope (RLS): the registry read answers for all of them. The
    // unique index stays the guard against a racing create or rename.
    let key = name.to_lowercase();
    if platform::rollup(&st.db)
        .await?
        .iter()
        .any(|r| r.t.id != tenant && r.t.name.trim().to_lowercase() == key)
    {
        return Err(name_taken());
    }
    sqlx::query("UPDATE tenants SET name = $2, updated_at = now(), updated_by = $3 WHERE id = $1")
        .bind(tenant)
        .bind(&name)
        .bind(&actor.id)
        .execute(&mut *tx)
        .await
        .map_err(|e| match is_unique_violation(&e) {
            Some(c) if c.contains("name") => name_taken(),
            _ => e.into(),
        })?;
    audit::record(
        &mut tx,
        actor,
        Event::new("tenant.rename", "tenant", tenant.to_string())
            .detail("old_name", row.name.clone())
            .detail("new_name", name.clone()),
    )
    .await?;
    let now = readmodel::tenant::read(&mut tx, tenant)
        .await?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("tenant {tenant} vanished")))?;
    enter(&mut tx).await?;
    audit::record_platform(
        &mut tx,
        actor,
        PlatformEvent::new("tenant.rename", name.clone())
            .tenant(tenant)
            .detail("old_name", row.name.clone())
            .detail("new_name", name),
    )
    .await?;
    leave(&mut tx).await?;
    tx.commit().await?;
    Ok(now)
}

/// What rag and ingest keep after an offboard (the report is never complete).
fn data_retained() -> Value {
    json!({"items": [
        {"app": "strata_rag", "what": "knowledge stores and their documents stay in strata_rag"},
        {"app": "strata_ingest", "what": "ingest sources and jobs stay in strata_ingest"},
    ]})
}

/// Offboard `tenant` (`confirm_name` must equal its name, else 422 `confirm_name_mismatch`). A repeat answers the
/// stored result and calls nothing. See the module doc.
pub async fn offboard(
    st: &AppState,
    actor: &Actor,
    tenant: Uuid,
    confirm_name: &str,
) -> ApiResult<Value> {
    let (mut tx, row) = crate::routes::tenants::scoped(st, tenant).await?;
    if confirm_name.trim() != row.name.trim() {
        return Err(ApiError::Unprocessable(
            "confirm_name_mismatch",
            "type the tenant's exact name to offboard it".into(),
        ));
    }
    // Review #5: concurrent offboards serialise on the tenant row. The loser waits here, then (a fresh snapshot per
    // statement) reads the winner's stored result below and answers it, as a repeat offboard does — not a 409 from
    // the archived-is-terminal trigger.
    sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR UPDATE")
        .bind(tenant)
        .execute(&mut *tx)
        .await?;
    let row = readmodel::tenant::read(&mut tx, tenant)
        .await?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("tenant {tenant} vanished")))?;
    if let Some(stored) = readmodel::tenant::offboard(&mut tx, tenant).await? {
        tx.commit().await?;
        return Ok(stored);
    }
    sqlx::query(
        "UPDATE tenants SET status = 'archived', archived_at = now(), \
                suspended_at = coalesce(suspended_at, now()), updated_at = now(), updated_by = $2 \
          WHERE id = $1",
    )
    .bind(tenant)
    .bind(&actor.id)
    .execute(&mut *tx)
    .await?;
    let disabled = sqlx::query(
        "UPDATE users SET disabled_at = now(), updated_at = now(), updated_by = $1 WHERE disabled_at IS NULL",
    )
    .bind(&actor.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query(
        "UPDATE member_entitlement \
            SET status = CASE WHEN license_id IS NULL THEN 'revoked' ELSE 'revoking' END, \
                revoked_at = CASE WHEN license_id IS NULL THEN now() END, updated_at = now() \
          WHERE status IN ('pending', 'active', 'error')",
    )
    .execute(&mut *tx)
    .await?;
    // The tenant licence is parked in the same transaction, so the tick owns it even if this request dies after
    // COMMIT; the revoke below unparks it.
    if let Some(license_id) = &row.licence_id {
        park(&mut tx, tenant, license_id).await?;
    }
    let cancelled = crate::provision::on_tenant_offboarded(&mut tx, tenant, actor).await?;
    let outbox: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT target, op, status::text FROM provision_outbox ORDER BY created_at DESC, id DESC LIMIT 50",
    )
    .fetch_all(&mut *tx)
    .await?;
    let outbox: Vec<Value> = outbox
        .into_iter()
        .map(|(target, op, status)| json!({"target": target, "op": op, "status": status}))
        .collect();
    let users_disabled = i32::try_from(disabled).unwrap_or(i32::MAX);
    sqlx::query(
        "INSERT INTO tenant_offboard (tenant_id, \"by\", licence_revoked, users_disabled, outbox, data_retained) \
         VALUES ($1, $2, false, $3, $4, $5)",
    )
    .bind(tenant)
    .bind(&actor.id)
    .bind(users_disabled)
    .bind(json!({"rows": outbox}))
    .bind(data_retained())
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        actor,
        Event::new("tenant.offboard", "tenant", tenant.to_string())
            .detail("users_disabled", users_disabled)
            .detail("outbox_cancelled", cancelled.len()),
    )
    .await?;
    let revoking: Vec<String> =
        sqlx::query_scalar("SELECT user_sub FROM member_entitlement WHERE status = 'revoking'")
            .fetch_all(&mut *tx)
            .await?;
    enter(&mut tx).await?;
    audit::record_platform(
        &mut tx,
        actor,
        PlatformEvent::new("tenant.offboard", row.name.clone())
            .tenant(tenant)
            .detail("users_disabled", users_disabled),
    )
    .await?;
    leave(&mut tx).await?;
    tx.commit().await?;
    // B4 T1: an archived tenant is served no more.
    crate::runtime::scopes::changed(st, tenant).await;

    // After COMMIT (D77 L6): revoke every member licence and the tenant licence, reading each body.
    // A failure here never aborts the offboard: whatever is not revoked stays parked / revoking for the tick.
    for sub in &revoking {
        match members::revoke_member_licence(st, tenant, sub, actor, true).await {
            Ok(RevokeResult::Failed(_, license_id)) => {
                if let Err(e) = park_alone(st, tenant, &license_id).await {
                    tracing::warn!(%tenant, error = %e, "offboard_park_failed");
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(%tenant, error = %e, "offboard_member_revoke_failed"),
        }
    }
    if let Some(license_id) = &row.licence_id
        && let Err(e) = revoke_tenant_licence(st, actor, tenant, license_id, true).await
    {
        tracing::warn!(%tenant, error = %e, "offboard_tenant_revoke_failed");
    }
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let stored = readmodel::tenant::offboard(&mut tx, tenant)
        .await?
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("offboard of {tenant} not stored")))?;
    tx.commit().await?;
    Ok(stored)
}

async fn park_alone(st: &AppState, tenant: Uuid, license_id: &str) -> ApiResult<()> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    park(&mut tx, tenant, license_id).await?;
    tx.commit().await?;
    Ok(())
}

/// The reconciler's drain of one parked id (D77 L7), under the licence lock (crate::identity::lock_licence) held
/// across the licences call, so no store of the same id can interleave (review #8). Re-read under the tenant scope
/// first:
/// - an id that is no longer parked (another path revoked or claimed it) is done;
/// - an id held by a live holder — an `active` / `pending` member entitlement, or the licence of a tenant that is not
///   archived — is unparked WITHOUT a call: licences re-issued that still-active licence to its holder (a member
///   disabled, the revoke failed, re-enabled before the tick: `find_active_license` answers the parked id), and
///   revoking it would leave an active entitlement on a revoked licence;
/// - an id already revoked (its member entitlement is `revoked`, or a `license.revoke` is audited for it) is unparked
///   without a call (licences would answer a no-op for it);
/// - anything else is revoked as [`revoke_tenant_licence`] does (`audit_failure` passed through).
pub async fn drain_parked(
    st: &AppState,
    actor: &Actor,
    tenant: Uuid,
    license_id: &str,
    audit_failure: bool,
) -> ApiResult<RevokeResult> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    lock_licence(&mut tx, tenant, license_id).await?;
    let (parked, held, done): (bool, bool, bool) = sqlx::query_as(
        "SELECT $2 = ANY (t.licence_revoke_pending), \
                EXISTS (SELECT 1 FROM member_entitlement e \
                         WHERE e.license_id = $2 AND e.status IN ('active', 'pending')) \
                OR (t.licence_id IS NOT DISTINCT FROM $2 AND t.status <> 'archived'), \
                EXISTS (SELECT 1 FROM member_entitlement e WHERE e.license_id = $2 AND e.status = 'revoked') \
           FROM tenants t WHERE t.id = $1",
    )
    .bind(tenant)
    .bind(license_id)
    .fetch_one(&mut *tx)
    .await?;
    if !parked {
        tx.commit().await?;
        return Ok(RevokeResult::NothingToRevoke);
    }
    if held {
        members::unpark(&mut tx, tenant, license_id).await?;
        tx.commit().await?;
        metrics::counter!("agent_licence_parked_held_total").increment(1);
        tracing::warn!(%tenant, "parked_licence_held_unparked_not_revoked");
        return Ok(RevokeResult::NothingToRevoke);
    }
    if done || crate::identity::licence_revoked(&mut tx, tenant, license_id).await? {
        members::unpark(&mut tx, tenant, license_id).await?;
        tx.commit().await?;
        return Ok(RevokeResult::Revoked(license_id.to_string()));
    }
    revoke_locked(st, tx, actor, tenant, license_id, audit_failure, Via::Drain).await
}

/// Revoke a licence id held by the tenant itself (its own licence, or a parked one), under the licence lock: `Revoked`
/// → unpark + audit `license.revoke`; anything else → park + (when `audit_failure`) audit `license.revoke_failed`.
/// Answers `Revoked` or `Failed(code, id)`.
pub async fn revoke_tenant_licence(
    st: &AppState,
    actor: &Actor,
    tenant: Uuid,
    license_id: &str,
    audit_failure: bool,
) -> ApiResult<RevokeResult> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    lock_licence(&mut tx, tenant, license_id).await?;
    revoke_locked(
        st,
        tx,
        actor,
        tenant,
        license_id,
        audit_failure,
        Via::Tenant,
    )
    .await
}

/// [`revoke_tenant_licence`] in `tx`, which is scoped to `tenant` and holds the licence lock: the licences call is
/// made inside it, so the lock spans the call.
async fn revoke_locked(
    st: &AppState,
    mut tx: sqlx::Transaction<'static, sqlx::Postgres>,
    actor: &Actor,
    tenant: Uuid,
    license_id: &str,
    audit_failure: bool,
    via: Via,
) -> ApiResult<RevokeResult> {
    // An id agent already revoked (the `license.revoke` tombstone) is done without a licences call: a revoke that
    // another path confirmed while this one waited on the lock must not be retried into a park.
    let tombstoned = crate::identity::licence_revoked(&mut tx, tenant, license_id).await?;
    let r = if tombstoned {
        RevokeOutcome::Revoked
    } else {
        st.identity.revoke_licence(tenant, license_id).await
    };
    if r == RevokeOutcome::Revoked {
        // T0d: the tenants row before the entitlement rows (unpark locks it only when the id is parked); after the
        // licences call, so never held across it.
        lock_tenant_row(&mut tx, tenant).await?;
        members::unpark(&mut tx, tenant, license_id).await?;
        // A member licence parked by an offboard: its entitlement is revoked now too.
        sqlx::query(
            "UPDATE member_entitlement SET status = 'revoked', revoked_at = now(), updated_at = now() \
              WHERE license_id = $1 AND status = 'revoking'",
        )
        .bind(license_id)
        .execute(&mut *tx)
        .await?;
        let audit_id = audit::record(
            &mut tx,
            actor,
            Event::new("license.revoke", "tenant", tenant.to_string())
                .detail("license_id", license_id.to_string())
                .detail("already_revoked", tombstoned),
        )
        .await?;
        record_revocation(&mut tx, tenant, license_id, via, &actor.id, Some(audit_id)).await?;
        tx.commit().await?;
        return Ok(RevokeResult::Revoked(license_id.to_string()));
    }
    let code = revoke_code(&r);
    tracing::warn!(%tenant, code, outcome = ?r, "tenant_licence_revoke_failed");
    park(&mut tx, tenant, license_id).await?;
    if audit_failure {
        audit::record(
            &mut tx,
            actor,
            Event::new("license.revoke_failed", "tenant", tenant.to_string())
                .failed(code)
                .detail("license_id", license_id.to_string()),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(RevokeResult::Failed(code, license_id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_residency() {
        assert_eq!(tenant_name("  Acme ").unwrap(), "Acme");
        assert_eq!(tenant_name(" ").unwrap_err().code(), "invalid_name");
        assert_eq!(
            tenant_name(&"x".repeat(121)).unwrap_err().code(),
            "invalid_name"
        );
        assert_eq!(residency(Some("us")).unwrap(), "us");
        assert_eq!(residency(None).unwrap_err().code(), "invalid_residency");
        assert_eq!(
            residency(Some("EU ")).unwrap_err().code(),
            "invalid_residency"
        );
        assert_eq!(licences_code(None), "licences_unavailable");
        assert_eq!(licences_code(Some(403)), "licence_refused");
    }
}
