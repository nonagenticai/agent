//! Licence-token reseal (B7 T3): move every stored licence token (the tenant's own, `tenants.licence_token_encrypted`,
//! and its members', `member_entitlement.token_encrypted`) to the keyring's CURRENT kid, so an old kid can leave
//! `LICENSE_TOKEN_ENC_KEYS` once nothing is sealed under it. Only the envelope at rest changes: the licence, its id and
//! the token inside stay the same (a leaked token is re-minted, crate::licensing::reissue::remint, not resealed).
//!
//! - [`reseal_all`] (`resealLicences`): every non-archived tenant, each in its own `begin_scoped` transaction; one
//!   `licence_reseal_run` row (0090) + platform audit `license.reseal` with counts only (no token, no tenant id in
//!   the details); then `verdict::changed` for each tenant that moved (the verdict row carries the kid).
//! - [`reseal_one`] (`resealTenantLicence`): one tenant; 404 `tenant_not_found`, 409 `tenant_archived`, 409
//!   `no_licence` (`licence_id IS NULL`: the B1 tick mints the first one). Platform audit `license.reseal` with the
//!   tenant as subject. A token that does not open is counted in `remaining`, never fatal: re-issue or re-mint it.
//!
//! Every UPDATE is conditional on the value read, so a concurrent store (a mint, a re-issue, another reseal) wins and
//! is not overwritten. Idempotent: a second run finds nothing to move (`resealed = 0`).
use serde::Serialize;
use uuid::Uuid;

use crate::audit::{self, Actor, PlatformEvent};
use crate::crypto::keyring::Keyring;
use crate::error::{ApiError, ApiResult};
use crate::licensing::verdict;
use crate::platform_db::begin_platform;
use crate::state::AppState;
use crate::tenancy::begin_scoped;

/// The `ResealResult` answer (web `types/platform.ts`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Resealed {
    /// The kid every moved token is sealed under now.
    pub kid: String,
    /// Tokens moved to `kid`.
    pub resealed: u32,
    /// Tokens still not under `kid` (they do not open: re-issue / re-mint them).
    pub remaining: u32,
    /// Tenants with at least one token moved.
    pub tenants: Vec<Uuid>,
    /// Of `remaining`, tokens that did not open (logged without any value).
    #[serde(skip)]
    pub unreadable: u32,
}

/// What one tenant's transaction did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct One {
    resealed: u32,
    unreadable: u32,
}

/// Is `stored` sealed under some kid other than `current` (a `v1:` value included)?
fn behind(stored: &str, current: &str) -> bool {
    Keyring::kid_of(stored) != Some(current)
}

fn seal_failed(e: crate::crypto::keyring::KeyringError) -> ApiError {
    ApiError::Internal(anyhow::anyhow!("licence token reseal: {e}"))
}

/// Reseal `tenant`'s tokens (own + members) in one scoped transaction.
async fn reseal_tenant(st: &AppState, tenant: Uuid) -> ApiResult<One> {
    let cipher = &st.identity.cipher;
    let current = cipher.current_kid().to_string();
    let mut one = One::default();
    let mut tx = begin_scoped(&st.db, tenant).await?;
    // The tenants row first, then the entitlements (the order every licence transaction takes).
    let own: Option<Option<String>> =
        sqlx::query_scalar("SELECT licence_token_encrypted FROM tenants WHERE id = $1")
            .bind(tenant)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(Some(old)) = own
        && behind(&old, &current)
    {
        match cipher.open(tenant, &old) {
            Ok(token) => {
                let new = cipher.seal(tenant, &token).map_err(seal_failed)?;
                let n = sqlx::query(
                    "UPDATE tenants SET licence_token_encrypted = $3 \
                      WHERE id = $1 AND licence_token_encrypted = $2",
                )
                .bind(tenant)
                .bind(&old)
                .bind(&new)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                one.resealed += u32::from(n == 1);
            }
            Err(e) => {
                tracing::warn!(%tenant, holder = "tenant", error = %e, "licence_reseal_token_unreadable");
                one.unreadable += 1;
            }
        }
    }
    let members: Vec<(String, String)> = sqlx::query_as(
        "SELECT user_sub, token_encrypted FROM member_entitlement \
          WHERE token_encrypted IS NOT NULL ORDER BY user_sub",
    )
    .fetch_all(&mut *tx)
    .await?;
    for (sub, old) in members {
        if !behind(&old, &current) {
            continue;
        }
        match cipher.open(tenant, &old) {
            Ok(token) => {
                let new = cipher.seal(tenant, &token).map_err(seal_failed)?;
                let n = sqlx::query(
                    "UPDATE member_entitlement SET token_encrypted = $3 \
                      WHERE user_sub = $1 AND token_encrypted = $2",
                )
                .bind(&sub)
                .bind(&old)
                .bind(&new)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                one.resealed += u32::from(n == 1);
            }
            Err(e) => {
                tracing::warn!(%tenant, holder = "member", error = %e, "licence_reseal_token_unreadable");
                one.unreadable += 1;
            }
        }
    }
    tx.commit().await?;
    Ok(one)
}

/// Rewrite the verdicts of `tenants` (their kid changed) and nudge the other replicas; failures are logged only.
async fn verdicts_changed(st: &AppState, tenants: &[Uuid]) {
    for t in tenants {
        if let Err(e) = verdict::changed(st, *t).await {
            tracing::warn!(tenant = %t, error = %e, "licence_reseal_verdict_failed");
        }
    }
}

/// `resealLicences`: every non-archived tenant (see the module doc).
pub async fn reseal_all(st: &AppState, actor: &Actor) -> ApiResult<Resealed> {
    let kid = st.identity.cipher.current_kid().to_string();
    let mut out = Resealed {
        kid: kid.clone(),
        ..Resealed::default()
    };
    for r in crate::readmodel::platform::rollup(&st.db).await? {
        if r.t.status == "archived" {
            continue;
        }
        let one = reseal_tenant(st, r.t.id).await?;
        out.resealed += one.resealed;
        out.unreadable += one.unreadable;
        if one.resealed > 0 {
            out.tenants.push(r.t.id);
        }
    }
    out.remaining = out.unreadable;
    let n = out.tenants.len();
    let mut ptx = begin_platform(&st.db).await?;
    sqlx::query(
        "INSERT INTO licence_reseal_run (actor, kid, resealed, remaining, unreadable, tenants) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(&actor.id)
    .bind(&kid)
    .bind(i32::try_from(out.resealed).unwrap_or(i32::MAX))
    .bind(i32::try_from(out.remaining).unwrap_or(i32::MAX))
    .bind(i32::try_from(out.unreadable).unwrap_or(i32::MAX))
    .bind(&out.tenants)
    .execute(&mut *ptx)
    .await?;
    audit::record_platform(
        &mut ptx,
        actor,
        PlatformEvent::new(
            "license.reseal",
            format!("{n} tenant{}", if n == 1 { "" } else { "s" }),
        )
        .detail("kid", kid.clone())
        .detail("resealed", out.resealed)
        .detail("remaining", out.remaining)
        .detail("unreadable", out.unreadable)
        .detail("tenant_count", n),
    )
    .await?;
    ptx.commit().await?;
    metrics::counter!("agent_licence_resealed_total").increment(u64::from(out.resealed));
    tracing::info!(kid = %kid, resealed = out.resealed, remaining = out.remaining, "licence_reseal_run");
    verdicts_changed(st, &out.tenants).await;
    Ok(out)
}

/// `resealTenantLicence`: one tenant (see the module doc).
pub async fn reseal_one(st: &AppState, actor: &Actor, tenant: Uuid) -> ApiResult<Resealed> {
    let (tx, row) = crate::routes::tenants::scoped(st, tenant).await?;
    tx.commit().await?;
    if row.status == "archived" {
        return Err(ApiError::Conflict(
            "tenant_archived",
            "this tenant is archived".into(),
        ));
    }
    if row.licence_id.is_none() {
        return Err(ApiError::Conflict(
            "no_licence",
            "no stored licence to reseal; the provisioning tick mints the first one".into(),
        ));
    }
    let kid = st.identity.cipher.current_kid().to_string();
    let one = reseal_tenant(st, tenant).await?;
    audit::record_platform_alone(
        &st.db,
        actor,
        PlatformEvent::new("license.reseal", row.name.clone())
            .tenant(tenant)
            .detail("kid", kid.clone())
            .detail("resealed", one.resealed)
            .detail("remaining", one.unreadable)
            .detail("unreadable", one.unreadable),
    )
    .await?;
    metrics::counter!("agent_licence_resealed_total").increment(u64::from(one.resealed));
    let tenants = if one.resealed > 0 {
        vec![tenant]
    } else {
        vec![]
    };
    verdicts_changed(st, &tenants).await;
    Ok(Resealed {
        kid,
        resealed: one.resealed,
        remaining: one.unreadable,
        tenants,
        unreadable: one.unreadable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn behind_is_any_other_kid_or_a_v1_value() {
        assert!(!behind("v2:k2:abc", "k2"));
        assert!(behind("v2:k1:abc", "k2"));
        assert!(behind("v1:abc", "k2"));
        assert!(behind("garbage", "k2"));
    }

    #[test]
    fn the_answer_carries_counts_and_ids_only() {
        let r = Resealed {
            kid: "k2".into(),
            resealed: 2,
            remaining: 1,
            tenants: vec![Uuid::nil()],
            unreadable: 1,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"kid": "k2", "resealed": 2, "remaining": 1, "tenants": [Uuid::nil()]})
        );
    }
}
