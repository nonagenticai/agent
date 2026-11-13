//! Licence re-issue (F-31, B7 T3a, built early): a super_admin replaces a tenant's own licence and/or one member's
//! licence with a new one in the tenant's residency region, without disabling anybody (owner 2026-10-20: Demo Retail's
//! licences were issued before B1b sent `region_id`, so licences holds them with region NULL).
//!
//! The new licence keeps the holder's licences SUBJECT: the tenant licence is `agent-tenant:<id>:a`
//! (crate::identity::tenant_licence_subject), a member's is its Keycloak `sub`. Licences keeps at most one ACTIVE licence
//! per (tenant, app, subject) and its create answers that licence again (`find_active_license`), so the old licence
//! must be revoked BEFORE the new one can be created: there is no mint-first order that yields a new licence for the
//! same subject. The sequence, per holder, reuses B1's crash-safe pieces (D77, review #8):
//!
//! 1. Pre-checks with nothing changed: tenant exists (404), not archived / suspended (409), the member is one of THIS
//!    tenant's members (404 `user_not_found`), the holder has a stored active licence (409 `no_licence`), and licences
//!    answers the residency's region (502 `licences_unavailable` / 409 `region_unmapped`: a re-issue that would land in
//!    region NULL again is refused, not done).
//! 2. ONE transaction under the per-licence advisory lock (crate::identity::lock_licence), held across the licences
//!    call: re-read the holder (another re-issue that got there first moved it on: 409 `concurrent_reissue`), revoke
//!    the old licence, bounded by `LICENCES_REVOKE_TIMEOUT_MS`.
//!    - `Revoked`: the holder's licence id and sealed token are cleared (a member's entitlement goes back to `pending`,
//!      the member stays enabled), the old id is unparked, the revoke is audited `license.revoke` and tombstoned in
//!      `licence_revocation` (B7 T3c, crate::licensing::store: the old id is never stored again) → COMMIT. Nothing dead is held: the old token is gone with the old id.
//!    - `Unavailable` (timeout, 5xx): licences may or may not have revoked it, so the holder is cleared the same way and
//!      the old id is PARKED in `tenants.licence_revoke_pending` (+ `license.revoke_failed`) → COMMIT. The provisioning
//!      tick drains it (crate::tenants::lifecycle::drain_parked) and mints the holder's licence again; the answer says
//!      `pending`. Never a stored token licences may have killed, never an unrecorded live licence. While the old
//!      licence is still active at licences, a mint (tick or `/api/me`) is answered that old id again: the store
//!      refuses it as stale (crate::identity::reissue_parked, keyed on this path's `license.revoke_failed` row with
//!      `via=reissue`), so the re-issue is never silently undone by adopting the old licence.
//!    - `NotRevoked` (licences answered and refused / no-op): ROLLBACK, nothing changed, 409 `licence_revoke_refused`.
//!      Licences answers `{"revoked": false}` only for an unknown id or another tenant's licence; an already-revoked
//!      licence answers `{"revoked": true}` again (licenses `revoke_token_or_license`), so that is `Revoked`.
//!    - A disable, invite revoke, offboard or suspend takes no licence lock and may commit during the licences call.
//!      The conditional `clear` (row count) and the tenant's status, read after it, say whether the holder moved on:
//!      then nothing is minted and the answer is 409 `concurrent_change`. After `Revoked` the old id is still
//!      tombstoned and a member's `revoking` entitlement on it is finished to `revoked` in the same transaction (its
//!      own revoke then has nothing to do); after `Unavailable` with a member gone the transaction rolls back and the
//!      moving flow (its revoke, the tick) owns the id; a tenant suspended / archived meanwhile still has the id parked
//!      and cleared (the tick drains it), and nothing is minted.
//! 3. After COMMIT, the new licence is minted and stored by B1's own path (crate::tenants::lifecycle::ensure_tenant_licence
//!    / crate::members::ensure_member_licence: create in the residency region → mint → conditional store of id + sealed
//!    token in one UPDATE → `license.issue`). A failure there leaves the holder `pending` for the tick (L7), which mints
//!    it: a crash anywhere after step 2's COMMIT is healed by the tick, a crash before it changed nothing.
//!
//! Every request is platform-audited `license.reissue_request` before any change, and its end `license.reissue` (ok, or
//! failed with the code); a completed holder also gets its `licence_reissue` row (R2, `kind = reissue`,
//! `reason = requested`) in the transaction of that audit row. Never a token in any row.
//!
//! **Re-mint** ([`remint`], F-32, B7 T3b, owner ruling R1): the leaked-token path. The tenant's own licence only;
//! `confirm_name` must equal the tenant's name (422 `confirm_name_mismatch`, nothing changed). The same sequence as
//! above (the old licence is revoked FIRST, at once, never left for an overlap) with `kind = remint`,
//! `reason = leaked` and every audit row carrying `kind: remint`. A revoke licences does not confirm (`Unavailable`)
//! still answers 200 with `old_revoke: pending`: the old id is parked for the tick and, until it drains, the tenant's
//! home shows a `licence` attention item (crate::readmodel::home::attention, keyed on the parked id and its
//! `license.revoke_failed` row with `kind = remint`). Both paths end with `verdict::changed`, so the edge and B4's
//! gate read the new licence within one nudge.
use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit::{self, Actor, Event, PlatformEvent};
use crate::error::{ApiError, ApiResult};
use crate::identity::{RevokeOutcome, licences_unavailable, lock_licence};
use crate::licensing::store::{self, NewReissue, Via};
use crate::members::{self, MintOutcome, revoke_code};
use crate::platform_db::begin_platform;
use crate::state::AppState;
use crate::tenancy::{begin_scoped, lock_tenant_row};
use crate::tenants::lifecycle::ensure_tenant_licence;

/// Which operation replaces the licence (`licence_reissue.kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// F-31: an operator re-issue on any verdict (`reason = requested`).
    Reissue,
    /// F-32: the token leaked (`reason = leaked`); confirmed by the tenant's name.
    Remint,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Reissue => "reissue",
            Kind::Remint => "remint",
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Kind::Reissue => "requested",
            Kind::Remint => "leaked",
        }
    }
}

/// Whose licence is re-issued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Holder {
    /// The tenant's own licence (`tenants.licence_id`).
    Tenant,
    /// A member's licence (`member_entitlement.license_id`), by Keycloak `sub`.
    Member(String),
}

impl Holder {
    fn kind(&self) -> &'static str {
        match self {
            Holder::Tenant => "tenant",
            Holder::Member(_) => "member",
        }
    }

    fn sub(&self) -> Option<&str> {
        match self {
            Holder::Tenant => None,
            Holder::Member(s) => Some(s),
        }
    }
}

/// What one holder's re-issue did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reissued {
    pub holder: Holder,
    pub old_licence_id: String,
    /// The licence now stored; `None` while the tick still has to mint it.
    pub new_licence_id: Option<String>,
    /// `revoked` (licences confirmed) or `pending` (parked for the tick).
    pub old_revoke: &'static str,
    /// `reissued` or `pending`.
    pub status: &'static str,
    /// Why it is `pending` (`revoke_pending`, or the mint's code: `licences_unavailable`, `licence_refused`, ...).
    pub code: Option<&'static str>,
}

impl Reissued {
    pub fn to_json(&self) -> Value {
        json!({
            "holder": self.holder.kind(),
            "sub": self.holder.sub(),
            "old_licence_id": self.old_licence_id,
            "new_licence_id": self.new_licence_id,
            "old_revoke": self.old_revoke,
            "status": self.status,
            "code": self.code,
        })
    }
}

/// The whole answer.
#[derive(Clone, Debug)]
pub struct Answer {
    pub tenant: Uuid,
    pub kind: Kind,
    pub residency: String,
    pub results: Vec<Reissued>,
}

impl Answer {
    pub fn to_json(&self) -> Value {
        json!({
            "tenant_id": self.tenant,
            "kind": self.kind.as_str(),
            "residency": self.residency,
            "members_unchanged": !self.results.iter().any(|r| matches!(r.holder, Holder::Member(_))),
            "results": self.results.iter().map(Reissued::to_json).collect::<Vec<_>>(),
        })
    }
}

fn count(holder: &Holder, outcome: &'static str) {
    metrics::counter!("agent_licence_reissue_total", "holder" => holder.kind(), "outcome" => outcome)
        .increment(1);
}

fn conflict(code: &'static str, msg: &str) -> ApiError {
    ApiError::Conflict(code, msg.into())
}

/// Platform audit `license.reissue` failed with `code` (never fails the caller: the refusal is already decided).
#[allow(clippy::too_many_arguments)]
async fn audit_failed(
    st: &AppState,
    actor: &Actor,
    tenant: Uuid,
    label: &str,
    kind: Kind,
    holder: &Holder,
    old: Option<&str>,
    code: &'static str,
) {
    let mut e = PlatformEvent::new("license.reissue", label)
        .tenant(tenant)
        .failed(code)
        .detail("kind", kind.as_str())
        .detail("holder", holder.kind());
    if let Some(s) = holder.sub() {
        e = e.detail("sub", s);
    }
    if let Some(o) = old {
        e = e.detail("old_licence_id", o);
    }
    if let Err(err) = audit::record_platform_alone(&st.db, actor, e).await {
        tracing::warn!(%tenant, error = %err, "licence_reissue_audit_failed");
    }
}

/// Re-issue the licences of `holders` (in that order) on `tenant`. See the module doc. The first refusal answers its
/// error; a holder already re-issued before it stays re-issued (each is audited on its own).
pub async fn reissue(
    st: &AppState,
    actor: &Actor,
    tenant: Uuid,
    holders: Vec<Holder>,
) -> ApiResult<Answer> {
    run(st, actor, tenant, holders, Kind::Reissue, None, None).await
}

/// Re-mint the tenant's own licence (F-32, T3b): `confirm_name` must equal the tenant's name, else 422
/// `confirm_name_mismatch` and nothing changes. `note` (the operator's reason, already checked by the route) goes
/// into the platform audit only. See the module doc.
pub async fn remint(
    st: &AppState,
    actor: &Actor,
    tenant: Uuid,
    confirm_name: &str,
    note: Option<&str>,
) -> ApiResult<Answer> {
    run(
        st,
        actor,
        tenant,
        vec![Holder::Tenant],
        Kind::Remint,
        Some(confirm_name),
        note,
    )
    .await
}

async fn run(
    st: &AppState,
    actor: &Actor,
    tenant: Uuid,
    holders: Vec<Holder>,
    kind: Kind,
    confirm_name: Option<&str>,
    note: Option<&str>,
) -> ApiResult<Answer> {
    // 1. Pre-checks, nothing changed.
    let (mut tx, row) = crate::routes::tenants::scoped(st, tenant).await?;
    let label = row.name.clone();
    if let Some(c) = confirm_name
        && c.trim() != row.name.trim()
    {
        tx.rollback().await?;
        return Err(ApiError::Unprocessable(
            "confirm_name_mismatch",
            "type the tenant's exact name to re-mint its licence".into(),
        ));
    }
    let mut olds = Vec::with_capacity(holders.len());
    let mut refusal: Option<(Holder, Option<String>, ApiError)> = None;
    for h in &holders {
        let old = match h {
            Holder::Tenant => row.licence_id.clone(),
            Holder::Member(sub) => {
                let member: Option<String> =
                    sqlx::query_scalar("SELECT sub FROM users WHERE sub = $1")
                        .bind(sub)
                        .fetch_optional(&mut *tx)
                        .await?;
                if member.is_none() {
                    refusal = Some((
                        h.clone(),
                        None,
                        ApiError::Missing("user_not_found", "no such member in this tenant".into()),
                    ));
                    break;
                }
                let lic: Option<Option<String>> = sqlx::query_scalar(
                    "SELECT license_id FROM member_entitlement WHERE user_sub = $1 AND status = 'active'",
                )
                .bind(sub)
                .fetch_optional(&mut *tx)
                .await?;
                lic.flatten()
            }
        };
        let gate = match row.status.as_str() {
            "archived" => Some(conflict("tenant_archived", "this tenant is archived")),
            "suspended" => Some(conflict(
                "tenant_suspended",
                "resume the tenant first: a suspended tenant's licences are not re-issued",
            )),
            _ => None,
        };
        if let Some(e) = gate {
            refusal = Some((h.clone(), old, e));
            break;
        }
        let Some(old) = old else {
            refusal = Some((
                h.clone(),
                None,
                conflict(
                    "no_licence",
                    "no stored active licence to re-issue; the provisioning tick mints the first one",
                ),
            ));
            break;
        };
        olds.push((h.clone(), old));
    }
    tx.commit().await?;
    if let Some((h, old, e)) = refusal {
        count(&h, "refused");
        audit_failed(
            st,
            actor,
            tenant,
            &label,
            kind,
            &h,
            old.as_deref(),
            e.code(),
        )
        .await;
        return Err(e);
    }
    let first = olds
        .first()
        .map(|(h, _)| h.clone())
        .unwrap_or(Holder::Tenant);
    match st.identity.licences.region_id(&row.residency).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            count(&first, "refused");
            audit_failed(
                st,
                actor,
                tenant,
                &label,
                kind,
                &first,
                None,
                "region_unmapped",
            )
            .await;
            return Err(conflict(
                "region_unmapped",
                "licences has no active region for this tenant's residency; nothing was changed",
            ));
        }
        Err(e) => {
            count(&first, "unavailable");
            audit_failed(
                st,
                actor,
                tenant,
                &label,
                kind,
                &first,
                None,
                "licences_unavailable",
            )
            .await;
            return Err(licences_unavailable(&e));
        }
    }

    let mut results = Vec::with_capacity(olds.len());
    for (h, old) in olds {
        let mut e = PlatformEvent::new("license.reissue_request", label.clone())
            .tenant(tenant)
            .detail("kind", kind.as_str())
            .detail("holder", h.kind())
            .detail("old_licence_id", old.clone())
            .detail("residency", row.residency.clone());
        if let Some(s) = h.sub() {
            e = e.detail("sub", s);
        }
        if let Some(n) = note {
            e = e.detail("note", n);
        }
        audit::record_platform_alone(&st.db, actor, e).await?;
        match swap(st, actor, tenant, kind, &h, &old).await {
            Ok(r) => {
                let mut e = PlatformEvent::new("license.reissue", label.clone())
                    .tenant(tenant)
                    .detail("kind", kind.as_str())
                    .detail("holder", h.kind())
                    .detail("old_licence_id", old.clone())
                    .detail("old_revoke", r.old_revoke)
                    .detail("status", r.status);
                if let Some(s) = h.sub() {
                    e = e.detail("sub", s);
                }
                if let Some(n) = &r.new_licence_id {
                    e = e.detail("new_licence_id", n.clone());
                }
                if let Some(c) = r.code {
                    e = e.detail("pending_code", c);
                }
                // R2: the platform audit and the `licence_reissue` row (the licensing pages' "last re-issue") in one
                // platform transaction. The re-issue itself is done: a failure here is logged, never answered.
                let row = NewReissue {
                    tenant,
                    actor: &actor.id,
                    holder: h.kind(),
                    member_sub: h.sub(),
                    kind: kind.as_str(),
                    reason: kind.reason(),
                    old_licence_id: Some(old.as_str()),
                    new_licence_id: r.new_licence_id.as_deref(),
                    kid: r
                        .new_licence_id
                        .as_ref()
                        .map(|_| st.identity.cipher.current_kid()),
                    old_revoke: r.old_revoke,
                    status: r.status,
                };
                let written = async {
                    let mut ptx = begin_platform(&st.db).await?;
                    audit::record_platform(&mut ptx, actor, e).await?;
                    store::record_reissue(&mut ptx, &row).await?;
                    ptx.commit().await?;
                    Ok::<_, ApiError>(())
                }
                .await;
                if let Err(err) = written {
                    tracing::warn!(%tenant, error = %err, "licence_reissue_audit_failed");
                }
                count(&h, r.status);
                tracing::info!(
                    %tenant,
                    kind = kind.as_str(),
                    holder = h.kind(),
                    status = r.status,
                    old_revoke = r.old_revoke,
                    "licence_reissued"
                );
                results.push(r);
            }
            Err(err) => {
                let outcome = match err.code() {
                    "concurrent_reissue" | "concurrent_change" => "conflict",
                    _ => "refused",
                };
                count(&h, outcome);
                audit_failed(st, actor, tenant, &label, kind, &h, Some(&old), err.code()).await;
                return Err(err);
            }
        }
    }
    // The verdict reads the new licence now (and every replica is nudged); a failure is logged, never answered.
    if let Err(e) = crate::licensing::verdict::changed(st, tenant).await {
        tracing::warn!(%tenant, error = %e, "licence_reissue_verdict_failed");
    }
    Ok(Answer {
        tenant,
        kind,
        residency: row.residency,
        results,
    })
}

/// Clear the holder's stored licence `old` (in `conn`, under the licence lock). One row or none.
async fn clear(
    conn: &mut sqlx::PgConnection,
    tenant: Uuid,
    h: &Holder,
    old: &str,
) -> ApiResult<u64> {
    let n = match h {
        Holder::Tenant => sqlx::query(
            "UPDATE tenants SET licence_id = NULL, licence_token_encrypted = NULL, updated_at = now() \
              WHERE id = $1 AND licence_id = $2",
        )
        .bind(tenant)
        .bind(old)
        .execute(&mut *conn)
        .await?
        .rows_affected(),
        Holder::Member(sub) => sqlx::query(
            "UPDATE member_entitlement \
                SET status = 'pending', license_id = NULL, license_subject_id = NULL, token_encrypted = NULL, \
                    error_code = NULL, updated_at = now() \
              WHERE user_sub = $1 AND license_id = $2 AND status = 'active'",
        )
        .bind(sub)
        .bind(old)
        .execute(&mut *conn)
        .await?
        .rows_affected(),
    };
    Ok(n)
}

/// Steps 2 and 3 of the module doc for one holder.
async fn swap(
    st: &AppState,
    actor: &Actor,
    tenant: Uuid,
    kind: Kind,
    h: &Holder,
    old: &str,
) -> ApiResult<Reissued> {
    let (resource, resource_id) = match h {
        Holder::Tenant => ("tenant", tenant.to_string()),
        Holder::Member(sub) => ("member_entitlement", sub.clone()),
    };
    let mut tx = begin_scoped(&st.db, tenant).await?;
    lock_licence(&mut tx, tenant, old).await?;
    // Re-read under the lock: a re-issue (or offboard, disable) that got here first moved the holder on.
    let still: bool = match h {
        Holder::Tenant => sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM tenants WHERE id = $1 AND licence_id = $2 AND status = 'active')",
        )
        .bind(tenant)
        .bind(old)
        .fetch_one(&mut *tx)
        .await?,
        Holder::Member(sub) => sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM member_entitlement e JOIN tenants t ON t.id = e.tenant_id \
                             WHERE e.user_sub = $1 AND e.license_id = $2 AND e.status = 'active' \
                               AND t.status = 'active')",
        )
        .bind(sub)
        .bind(old)
        .fetch_one(&mut *tx)
        .await?,
    };
    if !still {
        tx.rollback().await?;
        return Err(conflict(
            "concurrent_reissue",
            "this licence changed while the re-issue waited; read the tenant again",
        ));
    }
    // An id agent already revoked cannot be stored (crate::identity::store_member_entitlement refuses it), so this is
    // defensive: a tombstoned id is not revoked again.
    let r = if crate::identity::licence_revoked(&mut tx, tenant, old).await? {
        RevokeOutcome::Revoked
    } else {
        st.identity.revoke_licence(tenant, old).await
    };
    // A tenant-admin disable, an invite revoke, an offboard or a suspend takes no licence lock: it may commit while
    // the licences call above is in flight. `clear` is conditional, so its row count says whether the holder is
    // still the one the re-issue checked; the tenant's status is read after it, and both are checked again after the
    // mint (a move committed between this COMMIT and the mint). A holder that moved on is never minted for: the
    // answer is 409 `concurrent_change`, and the flow that moved it finishes.
    // Lock order: the `tenants` row before the entitlement row, as offboard takes them (FOR UPDATE, then its
    // entitlement UPDATE). The audit / park / unpark below all touch the tenants row (audit_event's FK takes KEY
    // SHARE), so without this an entitlement `clear` first could deadlock with an offboard (audit of #17). Taken
    // after the licences call, so it is never held across it.
    lock_tenant_row(&mut tx, tenant).await?;
    match &r {
        RevokeOutcome::Revoked => {
            // For the tenant holder `clear` is the tenants row itself, done first so the unpark sees an offboard's park.
            let n = match h {
                Holder::Tenant => {
                    let n = clear(&mut tx, tenant, h, old).await?;
                    members::unpark(&mut tx, tenant, old).await?;
                    n
                }
                Holder::Member(_) => {
                    members::unpark(&mut tx, tenant, old).await?;
                    clear(&mut tx, tenant, h, old).await?
                }
            };
            if n == 0
                && let Holder::Member(sub) = h
            {
                // Disabled / offboarded meanwhile: its entitlement is `revoking` on this very id, which licences has
                // just revoked. Finish that transition here (the tombstone below makes its own revoke a no-op).
                sqlx::query(
                    "UPDATE member_entitlement SET status = 'revoked', revoked_at = now(), error_code = NULL, \
                            updated_at = now() \
                      WHERE user_sub = $1 AND license_id = $2 AND status = 'revoking'",
                )
                .bind(sub)
                .bind(old)
                .execute(&mut *tx)
                .await?;
            }
            let moved = n == 0 || !tenant_active(&mut tx, tenant).await?;
            let audit_id = audit::record(
                &mut tx,
                actor,
                Event::new("license.revoke", resource, resource_id.clone())
                    .detail("license_id", old.to_string())
                    .detail("via", "reissue")
                    .detail("kind", kind.as_str()),
            )
            .await?;
            store::record_revocation(
                &mut tx,
                tenant,
                old,
                Via::Reissue,
                &actor.id,
                Some(audit_id),
            )
            .await?;
            tx.commit().await?;
            if moved {
                tracing::warn!(%tenant, holder = h.kind(), "licence_reissue_holder_changed");
                return Err(concurrent_change(true));
            }
        }
        RevokeOutcome::Unavailable(_) => {
            let code = revoke_code(&r);
            let n = clear(&mut tx, tenant, h, old).await?;
            if n == 0 {
                // The holder moved on (disabled / offboarded): that flow owns this id now (its entitlement is
                // `revoking` on it, and its own revoke and the tick retry it). Nothing of this re-issue is kept.
                tx.rollback().await?;
                tracing::warn!(%tenant, holder = h.kind(), code, "licence_reissue_holder_changed");
                return Err(concurrent_change(false));
            }
            members::park(&mut tx, tenant, old).await?;
            let active = tenant_active(&mut tx, tenant).await?;
            audit::record(
                &mut tx,
                actor,
                Event::new("license.revoke_failed", resource, resource_id.clone())
                    .failed(code)
                    .detail("license_id", old.to_string())
                    .detail("via", "reissue")
                    .detail("kind", kind.as_str()),
            )
            .await?;
            tx.commit().await?;
            tracing::warn!(%tenant, holder = h.kind(), code, "licence_reissue_revoke_parked");
            if !active {
                // Suspended / archived meanwhile: the old id is parked for the tick and nothing is minted now.
                return Err(concurrent_change(false));
            }
            return Ok(Reissued {
                holder: h.clone(),
                old_licence_id: old.to_string(),
                new_licence_id: None,
                old_revoke: "pending",
                status: "pending",
                code: Some("revoke_pending"),
            });
        }
        RevokeOutcome::NotRevoked(code) => {
            tx.rollback().await?;
            tracing::warn!(%tenant, holder = h.kind(), code, "licence_reissue_revoke_refused");
            return Err(conflict(
                "licence_revoke_refused",
                "licences did not revoke the old licence; nothing was changed",
            ));
        }
    }

    // 3. After COMMIT: B1's own mint + conditional store, in the residency region.
    let minted = match h {
        Holder::Tenant => ensure_tenant_licence(st, tenant, actor).await,
        Holder::Member(sub) => members::ensure_member_licence(st, tenant, sub, actor).await,
    };
    let pending_code = match minted {
        Ok(MintOutcome::Stored(id)) => {
            return Ok(Reissued {
                holder: h.clone(),
                old_licence_id: old.to_string(),
                new_licence_id: Some(id),
                old_revoke: "revoked",
                status: "reissued",
                code: None,
            });
        }
        Ok(MintOutcome::Failed(code)) => Some(code),
        // Someone (the tick) stored first: read what is stored now.
        Ok(MintOutcome::LostRace | MintOutcome::NotPending) => None,
        Err(e) => {
            tracing::warn!(%tenant, holder = h.kind(), error = %e, "licence_reissue_mint_failed");
            Some("internal_error")
        }
    };
    let now = current(st, tenant, h).await?.filter(|id| id != old);
    if now.is_none() && !waiting(st, tenant, h).await? {
        // Disabled / offboarded / suspended after the COMMIT above and before the mint: nothing new is held (a licence
        // minted in that window lost the conditional store and was revoked or parked). A member licence stored just
        // before a suspend stays, as every member's licence does across a suspend.
        tracing::warn!(%tenant, holder = h.kind(), "licence_reissue_holder_changed");
        return Err(concurrent_change(true));
    }
    Ok(Reissued {
        holder: h.clone(),
        old_licence_id: old.to_string(),
        status: if now.is_some() { "reissued" } else { "pending" },
        code: if now.is_some() {
            None
        } else {
            Some(pending_code.unwrap_or("mint_pending"))
        },
        new_licence_id: now,
        old_revoke: "revoked",
    })
}

/// 409 `concurrent_change`: the holder was disabled, offboarded or suspended while the re-issue ran; nothing new is
/// minted for it. `revoked`: the old licence was revoked (and the holder cleared) before that was seen.
fn concurrent_change(revoked: bool) -> ApiError {
    conflict(
        "concurrent_change",
        if revoked {
            "the holder was disabled, offboarded or suspended during the re-issue: the old licence is revoked and no \
             new one is held"
        } else {
            "the holder was disabled, offboarded or suspended during the re-issue: no new licence is held"
        },
    )
}

/// Is the scoped `tenant` still `active` (read in `conn`)?
async fn tenant_active(conn: &mut sqlx::PgConnection, tenant: Uuid) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM tenants WHERE id = $1 AND status = 'active')",
    )
    .bind(tenant)
    .fetch_one(conn)
    .await?)
}

/// Is `h` still waiting for its licence: the tenant `active` and (a member) its entitlement `pending` or `active`?
/// False once a disable / offboard / suspend moved it on.
async fn waiting(st: &AppState, tenant: Uuid, h: &Holder) -> ApiResult<bool> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let mut ok = tenant_active(&mut tx, tenant).await?;
    if ok && let Holder::Member(sub) = h {
        ok = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM member_entitlement \
                             WHERE user_sub = $1 AND status IN ('pending', 'active'))",
        )
        .bind(sub)
        .fetch_one(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(ok)
}

/// The licence id stored for `h` now.
async fn current(st: &AppState, tenant: Uuid, h: &Holder) -> ApiResult<Option<String>> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let id: Option<Option<String>> = match h {
        Holder::Tenant => {
            sqlx::query_scalar("SELECT licence_id FROM tenants WHERE id = $1")
                .bind(tenant)
                .fetch_optional(&mut *tx)
                .await?
        }
        Holder::Member(sub) => sqlx::query_scalar(
            "SELECT license_id FROM member_entitlement WHERE user_sub = $1 AND status = 'active'",
        )
        .bind(sub)
        .fetch_optional(&mut *tx)
        .await?,
    };
    tx.commit().await?;
    Ok(id.flatten())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_says_whether_members_were_touched() {
        let r = |h: Holder| Reissued {
            holder: h,
            old_licence_id: "old".into(),
            new_licence_id: Some("new".into()),
            old_revoke: "revoked",
            status: "reissued",
            code: None,
        };
        let a = Answer {
            tenant: Uuid::nil(),
            kind: Kind::Reissue,
            residency: "eu".into(),
            results: vec![r(Holder::Tenant)],
        };
        let v = a.to_json();
        assert_eq!(v["members_unchanged"], true);
        assert_eq!(v["results"][0]["holder"], "tenant");
        assert!(v["results"][0]["sub"].is_null());
        let a = Answer {
            results: vec![r(Holder::Member("u-1".into()))],
            ..a
        };
        let v = a.to_json();
        assert_eq!(v["members_unchanged"], false);
        assert_eq!(v["results"][0]["sub"], "u-1");
        assert!(!v.to_string().contains("token"), "no token field: {v}");
    }
}
