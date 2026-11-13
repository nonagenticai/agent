//! `licensingOverview` (B7 T3, P20 `/platform/licensing`): per tenant its licence, gate-2 verdict and newest
//! re-issue / re-mint; the keyring's kids with how many stored tokens each still seals; the last reseal run. Never a
//! token: tokens are read only to learn their kid (`v2:<kid>:`), inside a transaction scoped to their tenant.
//!
//! Shapes are `LicensingOverview` of `web/src/lib/api/types/platform.ts`, with these differences:
//! - `keyring[].added_at` is `null`: agent-api does not know when a kid was added (the plan's
//!   `LICENSE_TOKEN_KID_ADDED` variable does not exist). Kids are the configured key set plus any kid a stored token
//!   names that the set lacks (`configured: false`: those tokens do not open).
//! - `keyring[].sealed` counts tenant AND member tokens of every tenant (archived included): a kid is safe to drop only
//!   when no stored token of any holder names it.
//! - `bundle` and `metering_emit_enabled` are omitted (D70 superseded the bundle minter; agent-api has no metering
//!   emit switch).
//! - Added: `tenants[].last_reissue` (R2, crate::licensing::store::LastReissue + `old_still_valid`: a re-mint whose old
//!   licence revoke is still parked in `tenants.licence_revoke_pending`) and `verdicts_configured`
//!   (`LICENSE_INTROSPECT_URL` set).
use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::crypto::keyring::Keyring;
use crate::error::ApiResult;
use crate::licensing::store;
use crate::licensing::verdict::{self, Verdict};
use crate::platform_db::begin_platform;
use crate::readmodel::tenant::tenant_licence;
use crate::state::AppState;
use crate::tenancy::begin_scoped;

/// What a tenant stores, read in its scoped transaction (kids only, never a token).
#[derive(Clone, Debug, Default)]
struct Held {
    /// The kid of the tenant's own token (`None`: no token).
    own_kid: Option<String>,
    /// Every stored token's kid (own + members).
    kids: Vec<String>,
    /// `tenants.licence_revoke_pending`.
    parked: Vec<String>,
}

async fn held(st: &AppState, tenant: Uuid) -> ApiResult<Held> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let own: Option<(Option<String>, Vec<String>)> = sqlx::query_as(
        "SELECT licence_token_encrypted, licence_revoke_pending FROM tenants WHERE id = $1",
    )
    .bind(tenant)
    .fetch_optional(&mut *tx)
    .await?;
    let members: Vec<String> = sqlx::query_scalar(
        "SELECT token_encrypted FROM member_entitlement WHERE token_encrypted IS NOT NULL",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let (own, parked) = own.unwrap_or_default();
    let mut h = Held {
        own_kid: own.as_deref().and_then(Keyring::kid_of).map(str::to_string),
        parked,
        ..Held::default()
    };
    for t in own.iter().chain(members.iter()) {
        if let Some(k) = Keyring::kid_of(t) {
            h.kids.push(k.to_string());
        }
    }
    Ok(h)
}

/// The platform `LicenceVerdict` of a stored verdict row; `None` for `none` (no licence id yet).
pub fn verdict_json(v: &Verdict) -> Option<Value> {
    (v.status != "none").then(|| {
        json!({
            "tenant_id": v.tenant_id,
            "status": v.status,
            "checked_at": v.checked_at,
            "reason": v.reason,
            "age_seconds": v.age_seconds,
            "fails_closed": v.fails_closed(),
        })
    })
}

/// `TenantLicence.introspect_status` from a verdict status (`expired` and the rest read as `inactive`).
fn introspect_status(status: &str) -> &'static str {
    match status {
        "active" => "active",
        "revoked" => "revoked",
        _ => "inactive",
    }
}

/// The whole `licensingOverview` answer.
pub async fn overview(st: &AppState) -> ApiResult<Value> {
    let rows = crate::readmodel::platform::rollup(&st.db).await?;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.t.id).collect();
    let mut ptx = begin_platform(&st.db).await?;
    let verdicts: HashMap<Uuid, Verdict> = verdict::overview(&mut ptx)
        .await?
        .into_iter()
        .map(|v| (v.tenant_id, v))
        .collect();
    let last = store::last_reissues(&mut ptx, &ids).await?;
    let reseal: Option<(DateTime<Utc>, i32, i32, String)> = sqlx::query_as(
        "SELECT at, resealed, remaining, kid FROM licence_reseal_run ORDER BY at DESC, id DESC LIMIT 1",
    )
    .fetch_optional(&mut *ptx)
    .await?;
    ptx.commit().await?;

    let current = st.identity.cipher.current_kid().to_string();
    let plan = crate::routes::tenants::plan(st);
    // Configured kids first (BTreeMap: stable order), every one counted from 0.
    let mut sealed: BTreeMap<String, (bool, u64)> = st
        .identity
        .cfg
        .licence_keys
        .set
        .keys
        .keys()
        .map(|k| (k.clone(), (true, 0)))
        .collect();
    let mut tenants = Vec::with_capacity(rows.len());
    for r in &rows {
        let h = held(st, r.t.id).await?;
        for k in &h.kids {
            sealed.entry(k.clone()).or_insert((false, 0)).1 += 1;
        }
        let v = verdicts.get(&r.t.id);
        let mut licence = tenant_licence(&r.t, plan);
        if let Some(obj) = licence.as_object_mut() {
            obj.insert("kid".into(), json!(h.own_kid));
            if let Some(v) = v.filter(|v| v.status != "none") {
                obj.insert(
                    "introspect_status".into(),
                    json!(introspect_status(&v.status)),
                );
            }
        }
        let last_reissue = last.get(&r.t.id).map(|l| {
            let mut j = serde_json::to_value(l).unwrap_or(Value::Null);
            let still = l.old_revoke == "pending"
                && l.old_licence_id
                    .as_ref()
                    .is_some_and(|o| h.parked.contains(o));
            if let Some(obj) = j.as_object_mut() {
                obj.insert("old_still_valid".into(), json!(still));
            }
            j
        });
        tenants.push(json!({
            "tenant_id": r.t.id,
            "name": r.t.name,
            "status": r.t.status,
            "licence": licence,
            "verdict": v.and_then(verdict_json),
            "last_reissue": last_reissue,
        }));
    }
    let keyring: Vec<Value> = sealed
        .into_iter()
        .map(|(kid, (configured, n))| {
            json!({
                "kid": kid,
                "current": kid == current,
                "added_at": Value::Null,
                "sealed": n,
                "configured": configured,
            })
        })
        .collect();
    Ok(json!({
        "now": Utc::now(),
        "current_kid": current,
        "keyring": keyring,
        "tenants": tenants,
        "last_reseal": reseal.map(|(at, resealed, remaining, kid)| json!({
            "at": at, "resealed": resealed, "remaining": remaining, "kid": kid,
        })),
        "verdicts_configured": verdict::configured(st),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(status: &str, age: i64) -> Verdict {
        Verdict {
            tenant_id: Uuid::nil(),
            status: status.into(),
            reason: None,
            licence_id: Some("lic".into()),
            plan: None,
            kid: Some("k1".into()),
            platform_owned: false,
            checked_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
            age_seconds: age,
        }
    }

    #[test]
    fn verdict_json_is_the_platform_shape_and_none_is_null() {
        assert!(verdict_json(&v("none", 0)).is_none());
        let j = verdict_json(&v("active", 10)).unwrap();
        assert_eq!(j["fails_closed"], false);
        assert_eq!(j["age_seconds"], 10);
        assert!(j.get("kid").is_none() && j.get("licence_id").is_none());
        assert_eq!(
            verdict_json(&v("active", 301)).unwrap()["fails_closed"],
            true
        );
        assert_eq!(
            verdict_json(&v("revoked", 1)).unwrap()["fails_closed"],
            true
        );
    }

    #[test]
    fn introspect_status_reads_expired_as_inactive() {
        assert_eq!(introspect_status("active"), "active");
        assert_eq!(introspect_status("revoked"), "revoked");
        assert_eq!(introspect_status("expired"), "inactive");
        assert_eq!(introspect_status("inactive"), "inactive");
    }
}
