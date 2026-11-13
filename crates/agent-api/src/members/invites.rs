//! Member invites (B1b T2, owner ruling F-01): list, resend with a cooldown, revoke, and the DB-only expiry, over
//! the `member_invites` rows B1 writes (migration 0040: `POST /api/tenant/invites` and a signup approval whose
//! requester already had a portal account). Acceptance stays B1's (crate::members::accept, crate::members::seen).
//!
//! - Expiry is evaluated on read: a `pending` row whose `expires_at` has passed is `expired` on the wire, whatever
//!   its stored state. [`expire_due`] flips such rows to `expired` in the database too (B1's hourly in-process
//!   sweep): a plain UPDATE per tenant, no HTTP, no LLM / GPU call, audited `user.invite_expire` by the system.
//! - Resend: one scoped transaction locks the invite, refuses an accepted / revoked one (409 `invite_not_pending`)
//!   and one sent less than `INVITE_RESEND_COOLDOWN_SECONDS` ago (429 `invite_resend_cooldown` + `retry_after_s`),
//!   re-arms an expired one to `pending` (409 `invite_superseded` when another pending invite holds the address),
//!   bumps `send_count` / `last_sent_at`, moves `expires_at` to now + 7 days and the invitee's `users.invited_at`
//!   (when it has a never-signed-in member row) to now, audits `user.invite_resend`, COMMITs, and only then asks
//!   Keycloak for the email ([`crate::identity::Identity::send_access`]). A failed send restores what the
//!   transaction changed (the cooldown is not consumed), audits `user.invite_email` failed and answers 502
//!   `keycloak_error`.
//! - Revoke: `revoked` (idempotent; an accepted invite is 409 `invite_not_pending`). An invitee member row that
//!   never signed in (`users.invited_at` set) is soft-disabled with it (entitlement `revoking`, the licence revoked
//!   after COMMIT, D77 L5/L10), so the seat goes with the invite. Audited `user.invite_revoke`.
//! - The super_admin variants run the same code in the path tenant and also write the platform row (subject = the
//!   tenant), as B1's super_admin writes do.
//!
//! No email address reaches an audit row: rows carry the invite id, the invitee's subject and counts.
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::audit::{self, Actor, Event, PlatformEvent};
use crate::error::{ApiError, ApiResult};
use crate::identity::{PersonNames, keycloak_error};
use crate::members::{entitle_revoking, revoke_member_licence};
use crate::platform_db::{begin_platform, enter};
use crate::state::AppState;
use crate::tenancy::{begin_scoped, lock_tenant_row};

/// The audit actor of the expiry sweep.
pub const SWEEP_ACTOR: &str = "service:agent-api-invite-sweep";
/// The `invited_by_name` of an invite a signup approval recorded (`invited_by = signup:<id>`).
pub const SIGNUP_INVITER: &str = "Sign-up approval";
/// The wire states (`?state=`).
pub const STATES: [&str; 4] = ["pending", "accepted", "expired", "revoked"];
/// Default and largest page of a list.
pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 200;

/// One `member_invites` row with its read-time fields.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct InviteRow {
    pub id: Uuid,
    pub email: String,
    pub roles: Vec<String>,
    pub state: String,
    pub invited_by: String,
    pub invited_by_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_sent_at: Option<DateTime<Utc>>,
    pub send_count: i32,
    pub accepted_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub signup_id: Option<Uuid>,
    pub invitee_sub: Option<String>,
    /// `pending` with `expires_at` in the past (read-time expiry).
    pub past_due: bool,
    /// `created_at` in microseconds since the epoch: the paging cursor.
    pub ts_us: i64,
}

/// The wire state of a stored state: `pending` past its `expires_at` is `expired`.
const WIRE_STATE: &str =
    "CASE WHEN i.state = 'pending' AND i.expires_at <= now() THEN 'expired' ELSE i.state END";

fn select() -> String {
    format!(
        "SELECT i.id, i.email, i.roles, {WIRE_STATE} AS state, i.invited_by, u.display_name AS invited_by_name, \
                i.created_at, i.expires_at, i.last_sent_at, i.send_count, i.accepted_at, i.revoked_at, \
                i.signup_id, i.invitee_sub, (i.state = 'pending' AND i.expires_at <= now()) AS past_due, \
                (extract(epoch FROM i.created_at) * 1000000)::bigint AS ts_us \
           FROM member_invites i LEFT JOIN users u ON u.sub = i.invited_by"
    )
}

impl InviteRow {
    /// `MemberInvite` (web `types/tenant.ts`).
    pub fn json(&self) -> Value {
        let via = if self.signup_id.is_some() {
            "signup"
        } else {
            "invite"
        };
        let name = match (
            &self.invited_by_name,
            self.invited_by.starts_with("signup:"),
        ) {
            (Some(n), _) => Some(n.clone()),
            (None, true) => Some(SIGNUP_INVITER.to_string()),
            (None, false) => None,
        };
        json!({
            "id": self.id,
            "email": self.email,
            "roles": self.roles,
            "state": self.state,
            "invited_by": self.invited_by,
            "invited_by_name": name,
            "created_at": self.created_at,
            "expires_at": self.expires_at,
            "last_sent_at": self.last_sent_at,
            "send_count": self.send_count,
            "accepted_at": self.accepted_at,
            "revoked_at": self.revoked_at,
            "via": via,
        })
    }
}

fn invite_not_found() -> ApiError {
    ApiError::Missing("invite_not_found", "no such invite in this tenant".into())
}

fn not_pending(state: &str) -> ApiError {
    ApiError::Conflict(
        "invite_not_pending",
        format!("the invite is {state}; only a pending or expired invite can be changed"),
    )
}

/// `?state=`: absent / empty → all; else one of [`STATES`], or 422 `invalid_state`.
pub fn state_filter(raw: Option<&str>) -> ApiResult<Option<&str>> {
    match raw.map(str::trim) {
        None | Some("") => Ok(None),
        Some(s) => STATES
            .iter()
            .copied()
            .find(|x| *x == s)
            .map(Some)
            .ok_or_else(|| {
                ApiError::Unprocessable(
                    "invalid_state",
                    "state must be pending, accepted, expired or revoked".into(),
                )
            }),
    }
}

/// `?before=`: a `{us}.{uuid}` cursor this list returned as `next`, else 422 `invalid_cursor`.
pub fn cursor(raw: Option<&str>) -> ApiResult<Option<(i64, Uuid)>> {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let bad = || {
        ApiError::Unprocessable(
            "invalid_cursor",
            "before must be a cursor returned as next by this list".into(),
        )
    };
    let (us, id) = s.split_once('.').ok_or_else(bad)?;
    Ok(Some((
        us.parse().map_err(|_| bad())?,
        Uuid::parse_str(id).map_err(|_| bad())?,
    )))
}

/// The invites of the scoped tenant, newest first: `{items, next}`.
pub async fn list(
    conn: &mut PgConnection,
    state: Option<&str>,
    before: Option<(i64, Uuid)>,
    limit: i64,
) -> ApiResult<Value> {
    let (us, id) = match before {
        Some((us, id)) => (Some(us), Some(id)),
        None => (None, None),
    };
    let mut rows: Vec<InviteRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{} WHERE ($1::text IS NULL OR {WIRE_STATE} = $1) \
            AND ($2::bigint IS NULL \
                 OR (i.created_at, i.id) < (timestamptz 'epoch' + $2::bigint * interval '1 microsecond', $3::uuid)) \
          ORDER BY i.created_at DESC, i.id DESC LIMIT $4",
        select()
    )))
    .bind(state)
    .bind(us)
    .bind(id)
    .bind(limit + 1)
    .fetch_all(&mut *conn)
    .await?;
    let next = if rows.len() as i64 > limit {
        rows.truncate(usize::try_from(limit).unwrap_or(0));
        rows.last().map(|r| format!("{}.{}", r.ts_us, r.id))
    } else {
        None
    };
    Ok(crate::page::paged(
        rows.iter().map(InviteRow::json).collect(),
        next,
    ))
}

/// One invite of the scoped tenant.
pub async fn one(conn: &mut PgConnection, id: Uuid) -> ApiResult<Option<InviteRow>> {
    Ok(
        sqlx::query_as(sqlx::AssertSqlSafe(format!("{} WHERE i.id = $1", select())))
            .bind(id)
            .fetch_optional(conn)
            .await?,
    )
}

/// The `MemberInvite` of `id` in `tenant`, read in its own scoped transaction.
async fn answer(db: &PgPool, tenant: Uuid, id: Uuid) -> ApiResult<Value> {
    let mut tx = begin_scoped(db, tenant).await?;
    let r = one(&mut tx, id).await?.ok_or_else(invite_not_found)?;
    tx.commit().await?;
    Ok(r.json())
}

/// Who acts, and whether the platform log gets a row too (the super_admin route: the tenant's name as the label).
pub struct By<'a> {
    pub actor: &'a Actor,
    /// The super_admin path: also write the platform row, subject = the tenant.
    pub platform: bool,
}

/// The locked row a write works on.
#[derive(sqlx::FromRow)]
struct Locked {
    state: String,
    past_due: bool,
    invitee_sub: Option<String>,
    email: String,
    last_sent_at: Option<DateTime<Utc>>,
    send_count: i32,
    expires_at: DateTime<Utc>,
    /// Seconds since `last_sent_at` (`None` when never sent).
    since_sent_s: Option<f64>,
}

async fn lock(conn: &mut PgConnection, id: Uuid) -> ApiResult<Option<Locked>> {
    Ok(sqlx::query_as(
        "SELECT state, (state = 'pending' AND expires_at <= now()) AS past_due, invitee_sub, email, last_sent_at, \
                send_count, expires_at, extract(epoch FROM now() - last_sent_at)::float8 AS since_sent_s \
           FROM member_invites WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(conn)
    .await?)
}

/// 429 `invite_resend_cooldown` when the last send is younger than `cooldown_s` (`retry_after_s` = the seconds
/// left, at least 1).
fn cooldown(row: &Locked, cooldown_s: u64) -> ApiResult<()> {
    let Some(since) = row.since_sent_s else {
        return Ok(());
    };
    let window = cooldown_s as f64;
    if since >= window {
        return Ok(());
    }
    let left = (window - since.max(0.0)).ceil().max(1.0) as u64;
    Err(ApiError::detailed(
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        "invite_resend_cooldown",
        format!("this invite was sent moments ago; it can be sent again in {left} s"),
    )
    .with("retry_after_s", left))
}

/// The tenant's name (the platform row's label), in the scoped transaction.
async fn tenant_label(conn: &mut PgConnection, tenant: Uuid) -> ApiResult<String> {
    Ok(sqlx::query_scalar("SELECT name FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(conn)
        .await?)
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|c| c == "23505")
}

fn superseded() -> ApiError {
    ApiError::Conflict(
        "invite_superseded",
        "another pending invite already holds this address".into(),
    )
}

/// Resend invite `id` of `tenant` (see the module doc): 202 `MemberInvite`.
pub async fn resend(st: &AppState, tenant: Uuid, id: Uuid, by: By<'_>) -> ApiResult<Value> {
    let cooldown_s = st.identity.cfg.invite_resend_cooldown_seconds;
    // A pre-check outside any lock, so a refused resend never reaches Keycloak; an invite without a subject (none
    // today; defensive) is resolved by its address before the write transaction (never hold one across HTTP).
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let row = lock(&mut tx, id).await?.ok_or_else(invite_not_found)?;
    tx.commit().await?;
    if matches!(row.state.as_str(), "accepted" | "revoked") {
        return Err(not_pending(&row.state));
    }
    cooldown(&row, cooldown_s)?;
    let sub = match row.invitee_sub.clone() {
        Some(s) => s,
        None => {
            let local = row.email.split('@').next().unwrap_or_default();
            st.identity
                .resolve_or_create(&row.email, &PersonNames::resolve(None, None, local), None)
                .await
                .map_err(|e| keycloak_error(&e))?
                .0
        }
    };

    let mut tx = begin_scoped(&st.db, tenant).await?;
    // T0d: the tenants row before the invite and member rows (the audit insert below reaches it).
    lock_tenant_row(&mut tx, tenant).await?;
    let row = lock(&mut tx, id).await?.ok_or_else(invite_not_found)?;
    if matches!(row.state.as_str(), "accepted" | "revoked") {
        return Err(not_pending(&row.state));
    }
    cooldown(&row, cooldown_s)?;
    // The invitee's member row here, if any: one that already signed in (its invite is accepted in all but the
    // stored state) or was disabled is not emailed again.
    let member: Option<(Option<DateTime<Utc>>, bool)> =
        sqlx::query_as("SELECT invited_at, disabled_at IS NOT NULL FROM users WHERE sub = $1")
            .bind(&sub)
            .fetch_optional(&mut *tx)
            .await?;
    let invited_at = match member {
        Some((_, true)) => return Err(not_pending("for a disabled member")),
        Some((None, false)) => return Err(not_pending("accepted")),
        Some((at, false)) => at,
        None => None,
    };
    let send_count: i32 = sqlx::query_scalar(
        "UPDATE member_invites \
            SET state = 'pending', last_sent_at = now(), send_count = send_count + 1, \
                expires_at = now() + interval '7 days', invitee_sub = coalesce(invitee_sub, $2), updated_at = now() \
          WHERE id = $1 RETURNING send_count",
    )
    .bind(id)
    .bind(&sub)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            superseded()
        } else {
            e.into()
        }
    })?;
    if invited_at.is_some() {
        // The never-signed-in member's invite clock (home attention, member activity) follows the re-armed invite.
        sqlx::query(
            "UPDATE users SET invited_at = now() WHERE sub = $1 AND invited_at IS NOT NULL",
        )
        .bind(&sub)
        .execute(&mut *tx)
        .await?;
    }
    audit::record(
        &mut tx,
        by.actor,
        Event::new("user.invite_resend", "user", sub.clone())
            .detail("invite", id.to_string())
            .detail("send_count", send_count)
            .detail("rearmed", row.state == "expired" || row.past_due),
    )
    .await?;
    if by.platform {
        let label = tenant_label(&mut tx, tenant).await?;
        enter(&mut tx).await?;
        audit::record_platform(
            &mut tx,
            by.actor,
            PlatformEvent::new("user.invite_resend", label)
                .tenant(tenant)
                .detail("invite", id.to_string())
                .detail("send_count", send_count),
        )
        .await?;
    }
    tx.commit().await?;

    // After COMMIT: the one access email. A failure gives the cooldown back.
    let sent = st.identity.send_access(&sub, true).await;
    if sent.is_ok() {
        // B1b T11: logged and audited (a failure is audited by the compensation below).
        crate::members::access_email_outcome(st, tenant, by.actor, &sub, "invite_resend", &sent)
            .await;
    }
    if let Err(e) = sent {
        tracing::warn!(code = e.code, error = %e.error, %tenant, invite = %id, "invite_resend_email_failed");
        let mut tx = begin_scoped(&st.db, tenant).await?;
        lock_tenant_row(&mut tx, tenant).await?;
        sqlx::query(
            "UPDATE member_invites \
                SET state = $2, last_sent_at = $3, send_count = $4, expires_at = $5, updated_at = now() \
              WHERE id = $1 AND send_count = $6 AND state = 'pending'",
        )
        .bind(id)
        .bind(&row.state)
        .bind(row.last_sent_at)
        .bind(row.send_count)
        .bind(row.expires_at)
        .bind(send_count)
        .execute(&mut *tx)
        .await?;
        if let Some(at) = invited_at {
            sqlx::query(
                "UPDATE users SET invited_at = $2 WHERE sub = $1 AND invited_at IS NOT NULL",
            )
            .bind(&sub)
            .bind(at)
            .execute(&mut *tx)
            .await?;
        }
        audit::record(
            &mut tx,
            by.actor,
            Event::new("user.invite_email", "user", sub.clone())
                .failed(e.code)
                .detail("invite", id.to_string()),
        )
        .await?;
        if by.platform {
            let label = tenant_label(&mut tx, tenant).await?;
            enter(&mut tx).await?;
            audit::record_platform(
                &mut tx,
                by.actor,
                PlatformEvent::new("user.invite_resend", label)
                    .tenant(tenant)
                    .failed(e.code)
                    .detail("invite", id.to_string()),
            )
            .await?;
        }
        tx.commit().await?;
        return Err(keycloak_error(&e.error));
    }
    answer(&st.db, tenant, id).await
}

/// B1's last-admin guard (crate::routes::users) for the soft-disable a revoke does: when the never-signed-in
/// invitee holds `tenant_admin` and no other enabled admin that accepted its invite exists, 409
/// `last_tenant_admin`. The admins are locked `FOR UPDATE`, as B1 does.
async fn last_admin_guard(conn: &mut PgConnection, sub: &str) -> ApiResult<()> {
    let admins: Vec<(String, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT sub, invited_at FROM users \
          WHERE 'tenant_admin' = ANY (roles) AND disabled_at IS NULL ORDER BY sub FOR UPDATE",
    )
    .fetch_all(conn)
    .await?;
    let this = admins
        .iter()
        .any(|(s, invited)| s == sub && invited.is_some());
    let others = admins
        .iter()
        .filter(|(s, invited)| s != sub && invited.is_none())
        .count();
    if this && others == 0 {
        return Err(ApiError::Conflict(
            "last_tenant_admin",
            "a tenant keeps at least one active tenant_admin".into(),
        ));
    }
    Ok(())
}

/// Revoke invite `id` of `tenant` (see the module doc): `MemberInvite`.
pub async fn revoke(st: &AppState, tenant: Uuid, id: Uuid, by: By<'_>) -> ApiResult<Value> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    // T0d: the tenants row before the invite, the admins and the member (the audit insert below reaches it).
    lock_tenant_row(&mut tx, tenant).await?;
    let row = lock(&mut tx, id).await?.ok_or_else(invite_not_found)?;
    match row.state.as_str() {
        "revoked" => {
            tx.commit().await?;
            return answer(&st.db, tenant, id).await;
        }
        "accepted" => return Err(not_pending("accepted")),
        _ => {}
    }
    sqlx::query(
        "UPDATE member_invites SET state = 'revoked', revoked_at = now(), updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    // The seat goes with the invite: a member row that never signed in is soft-disabled (B1's disable path).
    let mut disabled = None;
    if let Some(sub) = row.invitee_sub.as_deref() {
        last_admin_guard(&mut tx, sub).await?;
        let n = sqlx::query(
            "UPDATE users SET disabled_at = now(), updated_at = now(), updated_by = $2 \
              WHERE sub = $1 AND invited_at IS NOT NULL AND disabled_at IS NULL",
        )
        .bind(sub)
        .bind(&by.actor.id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n == 1 {
            entitle_revoking(&mut tx, sub).await?;
            disabled = Some(sub.to_string());
        }
    }
    let resource = row.invitee_sub.clone().unwrap_or_else(|| id.to_string());
    audit::record(
        &mut tx,
        by.actor,
        Event::new("user.invite_revoke", "user", resource)
            .detail("invite", id.to_string())
            .detail("member_disabled", disabled.is_some()),
    )
    .await?;
    if by.platform {
        let label = tenant_label(&mut tx, tenant).await?;
        enter(&mut tx).await?;
        audit::record_platform(
            &mut tx,
            by.actor,
            PlatformEvent::new("user.invite_revoke", label)
                .tenant(tenant)
                .detail("invite", id.to_string())
                .detail("member_disabled", disabled.is_some()),
        )
        .await?;
    }
    tx.commit().await?;
    // After COMMIT (D77 L5/L10): a failure stays `revoking` (audited) for the provisioning tick.
    if let Some(sub) = disabled {
        match revoke_member_licence(st, tenant, &sub, by.actor, true).await {
            Ok(o) => tracing::debug!(outcome = ?o, "invite_revoke_licence"),
            Err(e) => tracing::warn!(error = %e, "invite_revoke_licence_failed"),
        }
    }
    answer(&st.db, tenant, id).await
}

/// Flip every pending invite past its `expires_at` to `expired`, tenant by tenant (B1's hourly in-process sweep):
/// one UPDATE per tenant scope, one `user.invite_expire` row per flipped invite by [`SWEEP_ACTOR`]. Database only:
/// no HTTP, no LLM / GPU call. Returns how many were flipped.
pub async fn expire_due(db: &PgPool) -> ApiResult<u64> {
    let mut tx = begin_platform(db).await?;
    let tenants: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM agent_list_tenants()")
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    let actor = Actor::system(SWEEP_ACTOR);
    let mut flipped = 0u64;
    for tenant in tenants {
        // One tenant failing never stops the others; it is retried next hour.
        match expire_tenant(db, tenant, &actor).await {
            Ok(n) => flipped += n,
            Err(e) => tracing::warn!(%tenant, error = %e, "member_invite_expiry_tenant_failed"),
        }
    }
    Ok(flipped)
}

/// [`expire_due`] for one tenant, in its own scoped transaction.
async fn expire_tenant(db: &PgPool, tenant: Uuid, actor: &Actor) -> ApiResult<u64> {
    let mut tx = begin_scoped(db, tenant).await?;
    let rows: Vec<(Uuid, Option<String>)> = sqlx::query_as(
        "UPDATE member_invites SET state = 'expired', updated_at = now() \
          WHERE state = 'pending' AND expires_at <= now() RETURNING id, invitee_sub",
    )
    .fetch_all(&mut *tx)
    .await?;
    for (id, sub) in &rows {
        audit::record(
            &mut tx,
            actor,
            Event::new(
                "user.invite_expire",
                "user",
                sub.clone().unwrap_or_else(|| id.to_string()),
            )
            .detail("invite", id.to_string()),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(rows.len() as u64)
}
