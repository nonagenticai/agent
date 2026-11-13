//! Members (B1 T8, T12): sessions ([`seen`]) and the member licence (D77 L5/L10): minted after the transaction that
//! wrote the `pending` entitlement ([`ensure_member_licence`]), revoked after the one that set it `revoking`
//! ([`revoke_member_licence`]). The admin routes are crate::routes::users; invite list / resend / revoke / expiry are
//! [`invites`] (B1b, crate::routes::invites).
pub mod accept;
pub mod invites;
pub mod names;
pub mod seen;

use uuid::Uuid;

use crate::audit::{self, Actor, Event};
use crate::error::ApiResult;
use crate::identity::{RevokeOutcome, Stored, lock_licence, store_member_entitlement};
use crate::state::AppState;
use crate::tenancy::{begin_scoped, lock_tenant_row};

/// What [`ensure_member_licence`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MintOutcome {
    /// No `pending` entitlement without a licence (none, already active, revoking, …), or the tenant is not active.
    NotPending,
    /// Minted and stored; the licence id.
    Stored(String),
    /// Another runner stored first; this run's licence was revoked (unless it was the same licence).
    LostRace,
    /// Licences refused or was unreachable: the row stays `pending` with this `error_code`; the tick retries.
    Failed(&'static str),
}

/// Mint `sub`'s member licence on `tenant` if its entitlement is `pending` (D77 L10: called AFTER the transaction
/// that wrote the `pending` row committed; never inside one). Stores it with the conditional UPDATE (L4) and audits
/// `license.issue` in that transaction; on a lost race revokes this run's token.
pub async fn ensure_member_licence(
    st: &AppState,
    tenant: Uuid,
    sub: &str,
    actor: &Actor,
) -> ApiResult<MintOutcome> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT status, license_id FROM member_entitlement WHERE user_sub = $1")
            .bind(sub)
            .fetch_optional(&mut *tx)
            .await?;
    let name: Option<(String, String)> =
        sqlx::query_as("SELECT name, residency FROM tenants WHERE id = $1 AND status = 'active'")
            .bind(tenant)
            .fetch_optional(&mut *tx)
            .await?;
    tx.commit().await?;
    let (Some((status, None)), Some((name, residency))) = (row, name) else {
        return Ok(MintOutcome::NotPending);
    };
    if status != "pending" {
        return Ok(MintOutcome::NotPending);
    }
    let issued = match st
        .identity
        .issue_member_licence(tenant, &name, &residency, sub)
        .await
    {
        Ok(l) => l,
        Err(e) => {
            let code = if e.status.is_none_or(|s| s >= 500 || s == 408 || s == 429) {
                "licences_unavailable"
            } else {
                "licence_refused"
            };
            let mut tx = begin_scoped(&st.db, tenant).await?;
            sqlx::query(
                "UPDATE member_entitlement SET error_code = $2, updated_at = now() \
                  WHERE user_sub = $1 AND status = 'pending'",
            )
            .bind(sub)
            .bind(code)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            metrics::counter!("agent_member_licence_errors_total", "code" => code).increment(1);
            tracing::warn!(%tenant, code, error = %e, "member_licence_mint_failed");
            return Ok(MintOutcome::Failed(code));
        }
    };
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let stored = store_member_entitlement(&mut tx, tenant, sub, &issued).await?;
    if stored == Stored::Stale {
        // Review #8: licences answered an id agent revoked since; the row stays `pending` for the next mint.
        let code = "licence_stale";
        sqlx::query(
            "UPDATE member_entitlement SET error_code = $2, updated_at = now() \
              WHERE user_sub = $1 AND status = 'pending'",
        )
        .bind(sub)
        .bind(code)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        metrics::counter!("agent_member_licence_errors_total", "code" => code).increment(1);
        return Ok(MintOutcome::Failed(code));
    }
    if stored == Stored::Stored {
        audit::record(
            &mut tx,
            actor,
            Event::new("license.issue", "member_entitlement", sub)
                .detail("license_id", issued.license_id.clone()),
        )
        .await?;
        tx.commit().await?;
        return Ok(MintOutcome::Stored(issued.license_id));
    }
    let winner: Option<Option<String>> =
        sqlx::query_scalar("SELECT license_id FROM member_entitlement WHERE user_sub = $1")
            .bind(sub)
            .fetch_optional(&mut *tx)
            .await?;
    tx.commit().await?;
    if winner.flatten().as_deref() != Some(issued.license_id.as_str()) {
        let r = st.identity.revoke_licence(tenant, &issued.license_id).await;
        orphan_if_unrevoked(st, tenant, &issued.license_id, &r).await;
    }
    Ok(MintOutcome::LostRace)
}

/// The `licence_revocation.actor` of a lost race's give-back (no person asked for it).
pub const LOST_RACE_ACTOR: &str = "agent-api";

/// A losing minter's licence whose revoke did not take (CP `license_provision_orphan_unrevoked_total` +
/// `sweepTenantLicences`): counted in `agent_license_orphan_unrevoked_total` and parked in
/// `tenants.licence_revoke_pending`, so the licence reconciler (every provisioning tick, D77 L7) retries the revoke
/// with its backoff until it lands. A revoked licence is nothing to do. Never fails the caller.
pub async fn orphan_if_unrevoked(st: &AppState, tenant: Uuid, license_id: &str, r: &RevokeOutcome) {
    if *r == RevokeOutcome::Revoked {
        // B7 T3c: a confirmed give-back is tombstoned (`via = lost_race`), so the id is never stored again.
        let res = async {
            let mut tx = begin_scoped(&st.db, tenant).await?;
            crate::licensing::store::record_revocation(
                &mut tx,
                tenant,
                license_id,
                crate::licensing::store::Via::LostRace,
                LOST_RACE_ACTOR,
                None,
            )
            .await?;
            tx.commit().await?;
            Ok::<_, crate::error::ApiError>(())
        }
        .await;
        if let Err(e) = res {
            tracing::warn!(%tenant, error = %e, "licence_lost_race_tombstone_failed");
        }
        return;
    }
    metrics::counter!("agent_license_orphan_unrevoked_total").increment(1);
    tracing::warn!(%tenant, outcome = ?r, "licence_loser_not_revoked_parked");
    let res = async {
        let mut tx = begin_scoped(&st.db, tenant).await?;
        park(&mut tx, tenant, license_id).await?;
        tx.commit().await?;
        Ok::<_, crate::error::ApiError>(())
    }
    .await;
    if let Err(e) = res {
        tracing::error!(
            %tenant,
            error = %e,
            remediation = "an unrevoked licence could not be parked; revoke it in licences by hand",
            "licence_orphan_park_failed"
        );
    }
}

/// What [`revoke_member_licence`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevokeResult {
    /// No `revoking` entitlement with a licence for that member.
    NothingToRevoke,
    /// Licences revoked it; the row is `revoked`.
    Revoked(String),
    /// Licences did not revoke it (a no-op 200, a refusal, an outage): the row stays `revoking` for the tick; the code
    /// (audited `license.revoke_failed`) and the licence id.
    Failed(&'static str, String),
}

/// The audit code of a revoke that did not happen.
pub fn revoke_code(r: &RevokeOutcome) -> &'static str {
    match r {
        RevokeOutcome::Revoked => "revoked",
        RevokeOutcome::NotRevoked(code) => code,
        RevokeOutcome::Unavailable(_) => "licences_unavailable",
    }
}

/// Revoke `sub`'s member licence on `tenant` if its entitlement is `revoking` (D77 L5/L6: called AFTER the transaction
/// that set `revoking` committed), under the licence lock (review #8). The body is read (a 200 can be a no-op): only a
/// real revoke moves the row to `revoked` (+ audit `license.revoke` in that transaction); anything else leaves it
/// `revoking` and, when `audit_failure`, audits `license.revoke_failed` in its own transaction (the reconciler passes
/// `false` and audits a failure only when its backoff state changes, review #2).
pub async fn revoke_member_licence(
    st: &AppState,
    tenant: Uuid,
    sub: &str,
    actor: &Actor,
    audit_failure: bool,
) -> ApiResult<RevokeResult> {
    let lic: Option<Option<String>> = {
        let mut tx = begin_scoped(&st.db, tenant).await?;
        let lic = sqlx::query_scalar(
            "SELECT license_id FROM member_entitlement WHERE user_sub = $1 AND status = 'revoking'",
        )
        .bind(sub)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        lic
    };
    let Some(Some(license_id)) = lic else {
        return Ok(RevokeResult::NothingToRevoke);
    };
    // Review #8: hold the licence lock across the licences call (crate::identity::lock_licence), and re-read under
    // it: a re-entitle may have moved the row on (and a store of the same id waits for this revoke to commit).
    let mut tx = begin_scoped(&st.db, tenant).await?;
    lock_licence(&mut tx, tenant, &license_id).await?;
    let still: Option<String> = sqlx::query_scalar(
        "SELECT license_id FROM member_entitlement \
          WHERE user_sub = $1 AND status = 'revoking' AND license_id = $2",
    )
    .bind(sub)
    .bind(&license_id)
    .fetch_optional(&mut *tx)
    .await?;
    if still.is_none() {
        tx.commit().await?;
        return Ok(RevokeResult::NothingToRevoke);
    }
    // An id agent already revoked (the `license.revoke` tombstone, e.g. a licence re-issue that revoked it while this
    // disable waited on the lock) is done: no licences call, the row finishes its transition.
    let tombstoned = crate::identity::licence_revoked(&mut tx, tenant, &license_id).await?;
    let r = if tombstoned {
        RevokeOutcome::Revoked
    } else {
        st.identity.revoke_licence(tenant, &license_id).await
    };
    if r == RevokeOutcome::Revoked {
        // T0d: the tenants row before the entitlement row; after the licences call, so never held across it.
        lock_tenant_row(&mut tx, tenant).await?;
        sqlx::query(
            "UPDATE member_entitlement SET status = 'revoked', revoked_at = now(), error_code = NULL, \
                    updated_at = now() \
              WHERE user_sub = $1 AND status = 'revoking' AND license_id = $2",
        )
        .bind(sub)
        .bind(&license_id)
        .execute(&mut *tx)
        .await?;
        unpark(&mut tx, tenant, &license_id).await?;
        let audit_id = audit::record(
            &mut tx,
            actor,
            Event::new("license.revoke", "member_entitlement", sub)
                .detail("license_id", license_id.clone())
                .detail("already_revoked", tombstoned),
        )
        .await?;
        crate::licensing::store::record_revocation(
            &mut tx,
            tenant,
            &license_id,
            crate::licensing::store::Via::Member,
            &actor.id,
            Some(audit_id),
        )
        .await?;
        tx.commit().await?;
        return Ok(RevokeResult::Revoked(license_id));
    }
    tx.commit().await?;
    let code = revoke_code(&r);
    tracing::warn!(%tenant, code, outcome = ?r, "member_licence_revoke_failed");
    if audit_failure {
        audit::record_alone(
            &st.db,
            tenant,
            actor,
            Event::new("license.revoke_failed", "member_entitlement", sub)
                .failed(code)
                .detail("license_id", license_id.clone()),
        )
        .await?;
    }
    Ok(RevokeResult::Failed(code, license_id))
}

/// The outcome of one access email (execute-actions-email, crate::identity::Identity::send_access) — B1b T11: an
/// `info` line on success (`warn` on failure) with the tenant, the subject and the action (`via`: `invite`,
/// `invite_resend`, `first_admin`, `signup`), and one tenant audit row `user.invite_email`, success or failure (the
/// code). Never an email address or a token. A failed audit write is logged, never fails the caller (the email's
/// fate is already decided).
pub async fn access_email_outcome(
    st: &AppState,
    tenant: Uuid,
    actor: &Actor,
    sub: &str,
    via: &'static str,
    r: &Result<crate::identity::Access, crate::identity::AccessError>,
) {
    let e = match r {
        Ok(access) => {
            tracing::info!(%tenant, sub, action = via, access = access.as_str(), "access_email_sent");
            Event::new("user.invite_email", "user", sub)
                .detail("via", via)
                .detail("access", access.as_str())
        }
        Err(err) => {
            tracing::warn!(%tenant, sub, action = via, code = err.code, error = %err.error, "access_email_failed");
            Event::new("user.invite_email", "user", sub)
                .failed(err.code)
                .detail("via", via)
        }
    };
    if let Err(err) = audit::record_alone(&st.db, tenant, actor, e).await {
        tracing::warn!(%tenant, error = %err, "access_email_audit_failed");
    }
}

/// Give `sub` a `pending` entitlement on the scoped `tenant` (inside the membership transaction, D77 L10): a new row,
/// or a `revoked` / `error` / `revoking` one reset (licence fields cleared). A licence still being revoked is parked in
/// `tenants.licence_revoke_pending` first, so the provisioning tick still revokes it. An `active` or `pending` row is
/// left as it is.
pub async fn entitle_pending(
    conn: &mut sqlx::PgConnection,
    tenant: Uuid,
    sub: &str,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE tenants t SET licence_revoke_pending = array_append(t.licence_revoke_pending, e.license_id) \
           FROM member_entitlement e \
          WHERE t.id = $1 AND e.user_sub = $2 AND e.status = 'revoking' AND e.license_id IS NOT NULL",
    )
    .bind(tenant)
    .bind(sub)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO member_entitlement (tenant_id, user_sub) VALUES ($1, $2) \
         ON CONFLICT (tenant_id, user_sub) DO UPDATE \
            SET status = 'pending', license_id = NULL, license_subject_id = NULL, token_encrypted = NULL, \
                error_code = NULL, revoked_at = NULL, updated_at = now() \
          WHERE member_entitlement.status IN ('revoked', 'error', 'revoking')",
    )
    .bind(tenant)
    .bind(sub)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Stop `sub`'s entitlement on the scoped tenant (inside the disabling transaction): one with a licence becomes
/// `revoking` (revoked after COMMIT by [`revoke_member_licence`]), one without is `revoked` at once.
pub async fn entitle_revoking(conn: &mut sqlx::PgConnection, sub: &str) -> ApiResult<()> {
    sqlx::query(
        "UPDATE member_entitlement \
            SET status = CASE WHEN license_id IS NULL THEN 'revoked' ELSE 'revoking' END, \
                revoked_at = CASE WHEN license_id IS NULL THEN now() END, updated_at = now() \
          WHERE user_sub = $1 AND status IN ('pending', 'active', 'error')",
    )
    .bind(sub)
    .execute(conn)
    .await?;
    Ok(())
}

/// Park `license_id` in `tenants.licence_revoke_pending` (once): the provisioning tick revokes it (D77 L6/L7).
pub async fn park(conn: &mut sqlx::PgConnection, tenant: Uuid, license_id: &str) -> ApiResult<()> {
    sqlx::query(
        "UPDATE tenants SET licence_revoke_pending = array_append(licence_revoke_pending, $2) \
          WHERE id = $1 AND NOT ($2 = ANY (licence_revoke_pending))",
    )
    .bind(tenant)
    .bind(license_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Drop `license_id` from `tenants.licence_revoke_pending` (it was revoked).
pub async fn unpark(
    conn: &mut sqlx::PgConnection,
    tenant: Uuid,
    license_id: &str,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE tenants SET licence_revoke_pending = array_remove(licence_revoke_pending, $2) \
          WHERE id = $1 AND $2 = ANY (licence_revoke_pending)",
    )
    .bind(tenant)
    .bind(license_id)
    .execute(conn)
    .await?;
    Ok(())
}
