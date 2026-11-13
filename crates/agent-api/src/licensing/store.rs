//! Licence history tables (B7 T3c + R2, migration 0096).
//!
//! - `licence_revocation` (tenant, RLS): the revoked-licence tombstone. One row per `(tenant_id, license_id)`, written
//!   by [`record_revocation`] in the transaction of every revoke licences confirmed (member revoke, tenant revoke,
//!   drain of a parked id, re-issue, a lost race's give-back). [`is_revoked`] is the guard that a revoked id is never
//!   stored again (crate::identity::licence_revoked). `agent_app` may SELECT and INSERT, never UPDATE or DELETE.
//! - `licence_reissue` (platform): one row per re-issue / re-mint of a holder's licence ([`record_reissue`], from
//!   crate::licensing::reissue); [`last_reissue`] / [`last_reissues`] answer the newest one per tenant for the
//!   licensing pages. Reached as `agent_platform` only (crate::platform_db). Never a token in either table.
use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::ApiResult;

/// Which path confirmed a revoke (`licence_revocation.via`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// A member's licence (disable, invite revoke, offboard): crate::members::revoke_member_licence.
    Member,
    /// The tenant's own licence (offboard): crate::tenants::lifecycle::revoke_tenant_licence.
    Tenant,
    /// A parked id drained by the provisioning tick: crate::tenants::lifecycle::drain_parked.
    Drain,
    /// The old licence of a re-issue / re-mint: crate::licensing::reissue.
    Reissue,
    /// A losing minter's licence given back: crate::members::orphan_if_unrevoked.
    LostRace,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Via::Member => "member",
            Via::Tenant => "tenant",
            Via::Drain => "drain",
            Via::Reissue => "reissue",
            Via::LostRace => "lost_race",
        }
    }
}

/// Tombstone `license_id` on the scoped `tenant` (`conn` = a `begin_scoped(tenant)` transaction, the one that records
/// the revoke). A second revoke of the same id is a no-op (`ON CONFLICT DO NOTHING`); answers whether a row was
/// written. `audit_id` = the `license.revoke` audit row of the same transaction, when there is one.
pub async fn record_revocation(
    conn: &mut PgConnection,
    tenant: Uuid,
    license_id: &str,
    via: Via,
    actor: &str,
    audit_id: Option<i64>,
) -> ApiResult<bool> {
    let n = sqlx::query(
        "INSERT INTO licence_revocation (tenant_id, license_id, actor, via, audit_id) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (tenant_id, license_id) DO NOTHING",
    )
    .bind(tenant)
    .bind(license_id)
    .bind(actor)
    .bind(via.as_str())
    .bind(audit_id)
    .execute(conn)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// Is `license_id` tombstoned on the scoped `tenant` (an index lookup)?
pub async fn is_revoked(
    conn: &mut PgConnection,
    tenant: Uuid,
    license_id: &str,
) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM licence_revocation WHERE tenant_id = $1 AND license_id = $2)",
    )
    .bind(tenant)
    .bind(license_id)
    .fetch_one(conn)
    .await?)
}

/// A `licence_reissue` row to write ([`record_reissue`]).
#[derive(Clone, Debug)]
pub struct NewReissue<'a> {
    pub tenant: Uuid,
    pub actor: &'a str,
    /// `tenant` or `member`.
    pub holder: &'a str,
    /// The member's `sub` (`holder = member` only).
    pub member_sub: Option<&'a str>,
    /// `reissue` or `remint`.
    pub kind: &'a str,
    /// `expired`, `revoked`, `inactive`, `unreadable`, `leaked` or `requested` (an operator re-issue on any verdict).
    pub reason: &'a str,
    pub old_licence_id: Option<&'a str>,
    /// `None` while the provisioning tick still has to mint it.
    pub new_licence_id: Option<&'a str>,
    /// The keyring kid the new token is sealed under (`None` when nothing new is stored yet).
    pub kid: Option<&'a str>,
    /// `revoked`, `pending` (parked for the tick) or `none`.
    pub old_revoke: &'a str,
    /// `reissued` or `pending`.
    pub status: &'a str,
}

/// Write one `licence_reissue` row; `conn` must be `agent_platform` (`begin_platform` or `enter`). Answers its id.
pub async fn record_reissue(conn: &mut PgConnection, r: &NewReissue<'_>) -> ApiResult<Uuid> {
    Ok(sqlx::query_scalar(
        "INSERT INTO licence_reissue \
                (tenant_id, actor, holder, member_sub, kind, reason, old_licence_id, new_licence_id, kid, \
                 old_revoke, status) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         RETURNING id",
    )
    .bind(r.tenant)
    .bind(r.actor)
    .bind(r.holder)
    .bind(r.member_sub)
    .bind(r.kind)
    .bind(r.reason)
    .bind(r.old_licence_id)
    .bind(r.new_licence_id)
    .bind(r.kid)
    .bind(r.old_revoke)
    .bind(r.status)
    .fetch_one(conn)
    .await?)
}

/// The newest re-issue of a tenant (`TenantLicence.last_reissue`). Serialises without any token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct LastReissue {
    #[serde(skip)]
    pub tenant_id: Uuid,
    pub at: DateTime<Utc>,
    pub kind: String,
    pub reason: String,
    pub actor: String,
    pub holder: String,
    pub member_sub: Option<String>,
    pub old_licence_id: Option<String>,
    pub new_licence_id: Option<String>,
    pub old_revoke: String,
    pub status: String,
}

const LAST_COLUMNS: &str = "tenant_id, at, kind, reason, actor, holder, member_sub, old_licence_id, new_licence_id, \
                            old_revoke, status";

/// The newest `licence_reissue` row of `tenant` (re-issue or re-mint, tenant or member holder), if any; `conn` must
/// be `agent_platform`.
pub async fn last_reissue(conn: &mut PgConnection, tenant: Uuid) -> ApiResult<Option<LastReissue>> {
    Ok(sqlx::query_as::<_, LastReissue>(sqlx::AssertSqlSafe(format!(
        "SELECT {LAST_COLUMNS} FROM licence_reissue WHERE tenant_id = $1 ORDER BY at DESC, id DESC LIMIT 1"
    )))
    .bind(tenant)
    .fetch_optional(conn)
    .await?)
}

/// [`last_reissue`] for several tenants at once (the overview), keyed by tenant; tenants without a row are absent.
pub async fn last_reissues(
    conn: &mut PgConnection,
    tenants: &[Uuid],
) -> ApiResult<HashMap<Uuid, LastReissue>> {
    let rows = sqlx::query_as::<_, LastReissue>(sqlx::AssertSqlSafe(format!(
        "SELECT DISTINCT ON (tenant_id) {LAST_COLUMNS} FROM licence_reissue \
          WHERE tenant_id = ANY ($1) ORDER BY tenant_id, at DESC, id DESC"
    )))
    .bind(tenants.to_vec())
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(|r| (r.tenant_id, r)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn via_strings_match_the_check() {
        let all = [
            Via::Member,
            Via::Tenant,
            Via::Drain,
            Via::Reissue,
            Via::LostRace,
        ];
        let s: Vec<&str> = all.iter().map(|v| v.as_str()).collect();
        assert_eq!(s, ["member", "tenant", "drain", "reissue", "lost_race"]);
    }

    #[test]
    fn last_reissue_serialises_without_tenant_or_token() {
        let r = LastReissue {
            tenant_id: Uuid::nil(),
            at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
            kind: "reissue".into(),
            reason: "requested".into(),
            actor: "sa-1".into(),
            holder: "tenant".into(),
            member_sub: None,
            old_licence_id: Some("lic-old".into()),
            new_licence_id: Some("lic-new".into()),
            old_revoke: "revoked".into(),
            status: "reissued".into(),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("tenant_id").is_none());
        assert_eq!(v["kind"], "reissue");
        assert!(!v.to_string().contains("v2:") && !v.to_string().contains("token"));
    }
}
