//! Gate 2 on the licence verdict (B7 T15b / T3b, R11): [`VerdictGate`] is the [`LicenceGate`] the drain and the
//! staff probe run. It composes B4's [`TenantStatusGate`] (a tenant that is not `active` is refused first) with the
//! tenant's stored licence verdict ([`super::verdict::read`]):
//!
//! - nonagentic's own tenant (D70, `tenants.platform_owned` or `AGENT_PLATFORM_TENANT_ID`) is always admitted;
//! - a fresh `active` verdict is admitted;
//! - `revoked`, `expired`, `inactive`/`tenant_suspended` and every other definite `inactive` reason (licences'
//!   `license_inactive`, `token_not_found`, `license_not_found`, the `*_mismatch` reasons) are refused, as is a status
//!   this build does not know (fail closed);
//! - what is NOT definite (no verdict row yet, `none` = no licence minted yet, `inactive`/`license_unavailable`
//!   (licences unreachable), `inactive`/`license_unreadable` (the stored token does not open), and an `active`
//!   verdict older than [`MAX_AGE_SECONDS`]) is refused too: the owner ruled (2026-10-22) fail closed — only a fresh
//!   `active` (or platform ownership) admits a turn. Nothing is remembered across verdicts: the writer never keeps an
//!   old `active` across an outage (it overwrites the row with `license_unavailable`), so an outage refuses turns
//!   until licences answers again.
//!
//! The decision itself is the pure [`decide`]; a refusal is logged (`licence_gate_refused`, ids and codes only) and
//! counted on `agent_licence_gate_refused_total{state}`.
//!
//! **At tenant-context resolution** (spec 04 §4b, 02 §7.1; `LICENSE_GATE_2_ENABLED`): [`check_request`] runs once per
//! request in crate::tenancy's `TenantCtx` extractor, after the tenant exists and its status was checked (archived /
//! a member's suspended tenant are 403 first), for every caller acting in a tenant, a super_admin included (judged
//! against THAT tenant's licence). It reads the same stored verdict with the same rule as the per-turn gate
//! ([`decide`]: platform-owned, or a fresh `active`) and never calls licences. A refusal is [`refusal`]'s coded
//! error: 402 `license_required` (no licence or no verdict yet), `license_unavailable` (licences unreachable, a
//! stale `active`, or the verdict could not be read), `license_unreadable`, `license_expired`, `license_inactive`
//! (revoked, inactive, an unknown status); 403 `license_app_mismatch` / `license_tenant_mismatch`,
//! `tenant_suspended`; every refusal carries `"gate": "licence"` in its error object ([`GATE_FIELD`]). Not gated
//! ([`exempt`]): `GET /api/license/status`, `/api/me` (and `/api/me/view-as`), `/api/platform/*`; routes that resolve
//! no `TenantCtx` are never gated either: `/health`, `/ready`, the super_admin tenant routes (`/api/tenants` list,
//! create, import, PATCH, offboard, licence ops: `SuperAdmin`), the signup queue, service-token doors. [`status`]
//! answers `GET /api/license/status` with what this gate would say, as data (never a 402). The same check runs on the
//! job API-key guard (crate::interaction::apikeys `JobCaller`) and on a member's `GET /api/tenants/{id}[/users]`.
use std::pin::Pin;

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use super::verdict::{self, MAX_AGE_SECONDS, Verdict};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::turn::drain::{LicenceGate, TenantStatusGate};

/// The licence status route: exempt from gate 2 and never 402, so a blocked client can learn why.
pub const STATUS_PATH: &str = "/api/license/status";

/// The refusal counter, by [`state_of`] (plus `tenant_not_active` for a refusal of [`TenantStatusGate`]).
pub const REFUSED_TOTAL: &str = "agent_licence_gate_refused_total";

/// How gate 2 reads one verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reading {
    /// A fresh `active` verdict.
    Active,
    /// A definite refusal (revoked, expired, suspended, inactive, unknown status).
    Refused,
    /// Not definite (no row, `none`, licences unavailable, the token unreadable, a stale `active`): refused, fail
    /// closed (owner, 2026-10-22).
    Undecided,
}

/// The verdict's state label (logs, the refusal counter's `state`): `missing` (no row), `active`, `stale` (an
/// `active` older than [`MAX_AGE_SECONDS`]), `none`, `unavailable`, `unreadable`, `suspended`, `revoked`, `expired`,
/// `inactive`, `unknown`.
pub fn state_of(v: Option<&Verdict>) -> &'static str {
    let Some(v) = v else {
        return "missing";
    };
    match v.status.as_str() {
        "active" if v.age_seconds > MAX_AGE_SECONDS => "stale",
        "active" => "active",
        "none" => "none",
        "revoked" => "revoked",
        "expired" => "expired",
        "inactive" => match v.reason.as_deref() {
            Some("license_unavailable") => "unavailable",
            Some("license_unreadable") => "unreadable",
            Some("tenant_suspended") => "suspended",
            _ => "inactive",
        },
        _ => "unknown",
    }
}

/// How gate 2 reads a [`state_of`] label.
pub fn reading(state: &str) -> Reading {
    match state {
        "active" => Reading::Active,
        "missing" | "none" | "unavailable" | "unreadable" | "stale" => Reading::Undecided,
        _ => Reading::Refused,
    }
}

/// May the turn run? `verdict` is the stored verdict (`None` = never written), `platform_owned` the D70 mark. Only a
/// fresh `active` or platform ownership admits; everything else is refused (fail closed). Pure.
pub fn decide(verdict: Option<&Verdict>, platform_owned: bool) -> bool {
    platform_owned || reading(state_of(verdict)) == Reading::Active
}

/// B7's gate 2: [`TenantStatusGate`], then the licence verdict (fail closed).
pub struct VerdictGate;

impl LicenceGate for VerdictGate {
    fn admits<'a>(
        &'a self,
        st: &'a AppState,
        tenant: Uuid,
    ) -> Pin<Box<dyn Future<Output = ApiResult<bool>> + Send + 'a>> {
        Box::pin(async move {
            if !TenantStatusGate.admits(st, tenant).await? {
                refused(tenant, "tenant_not_active");
                return Ok(false);
            }
            let (owned, v) = lookup(st, tenant).await?;
            if decide(v.as_ref(), owned) {
                return Ok(true);
            }
            refused(tenant, state_of(v.as_ref()));
            Ok(false)
        })
    }
}

pub(crate) fn refused(tenant: Uuid, state: &'static str) {
    tracing::info!(%tenant, state, "licence_gate_refused");
    metrics::counter!(REFUSED_TOTAL, "state" => state).increment(1);
}

/// What gate 2 reads for `tenant`: the D70 mark, and the stored verdict (not read when platform-owned).
pub async fn lookup(st: &AppState, tenant: Uuid) -> ApiResult<(bool, Option<Verdict>)> {
    let mut tx = crate::tenancy::begin_scoped(&st.db, tenant).await?;
    let owned = crate::providers::llm::platform_owned(&mut tx, st, tenant).await?;
    let v = if owned {
        None
    } else {
        verdict::read(&mut tx, tenant).await?
    };
    tx.commit().await?;
    Ok((owned, v))
}

/// Routes that resolve a `TenantCtx` but are not gated (spec 02 §7.1): the status route, `/api/me` and its
/// view-as, and `/api/platform/*`.
pub fn exempt(path: &str) -> bool {
    path == STATUS_PATH
        || path == "/api/me"
        || path.starts_with("/api/me/")
        || path.starts_with("/api/platform/")
}

fn payment_required(code: &'static str, message: &str) -> ApiError {
    ApiError::detailed(StatusCode::PAYMENT_REQUIRED, code, message)
}

fn unavailable() -> ApiError {
    payment_required(
        "license_unavailable",
        "could not verify the tenant licence; access denied",
    )
}

/// The coded refusal of a verdict [`decide`] refused (spec 04 §4a codes). Pure.
pub fn refusal(v: Option<&Verdict>) -> ApiError {
    match state_of(v) {
        "missing" | "none" => payment_required("license_required", "this tenant has no licence"),
        "unavailable" | "stale" => unavailable(),
        "unreadable" => payment_required(
            "license_unreadable",
            "the tenant licence could not be read; access denied",
        ),
        "expired" => payment_required("license_expired", "the tenant licence has expired"),
        "suspended" => ApiError::Forbidden("tenant_suspended", "this tenant is suspended".into()),
        _ => match v.and_then(|v| v.reason.as_deref()) {
            Some("app_mismatch") => ApiError::Forbidden(
                "license_app_mismatch",
                "the licence is not for this application".into(),
            ),
            Some("tenant_mismatch") => ApiError::Forbidden(
                "license_tenant_mismatch",
                "the licence belongs to a different tenant".into(),
            ),
            _ => payment_required("license_inactive", "the tenant licence is not active"),
        },
    }
}

/// Gate 2 for a caller acting in `tenant` (no-op while `LICENSE_GATE_2_ENABLED` is off). Fail closed: a verdict that
/// cannot be read denies as `license_unavailable`, never a 500.
pub async fn check_request(st: &AppState, tenant: Uuid) -> ApiResult<()> {
    if !st.cfg.license_gate_2_enabled {
        return Ok(());
    }
    evaluate(st, tenant).await
}

/// The mark on every refusal of [`check_request`]: `{"error": {"code", "message", "gate": "licence"}}`, so a client
/// tells the gate's 403 `tenant_suspended` (a licence verdict) from the tenant-status check's (no `gate`).
pub const GATE_FIELD: &str = "gate";
/// The value of [`GATE_FIELD`] on a gate-2 refusal.
pub const GATE_MARK: &str = "licence";

async fn evaluate(st: &AppState, tenant: Uuid) -> ApiResult<()> {
    let (owned, v) = match lookup(st, tenant).await {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!(%tenant, error = %e, "licence verdict lookup failed; denying");
            return Err(unavailable().with(GATE_FIELD, GATE_MARK));
        }
    };
    if decide(v.as_ref(), owned) {
        return Ok(());
    }
    let e = refusal(v.as_ref());
    tracing::info!(%tenant, state = state_of(v.as_ref()), code = e.code(), "licence_gate_2_refused");
    Err(e.with(GATE_FIELD, GATE_MARK))
}

/// `GET /api/license/status` for `tenant` (spec 04 §4b, CFG `license::status`): what gate 2 would say, as data.
/// `{"enforced": false, "tenant", "state": "not_enforced", "code": null}` while the flag is off;
/// `{"enforced": true, "tenant": null, "state": "no_workspace", "code": null}` without a tenant;
/// `{"enforced": true, "tenant", "state": "active", "code": null}`, or
/// `{"enforced": true, "tenant", "state": "blocked", "code", "message"}`.
pub async fn status(st: &AppState, tenant: Option<Uuid>) -> Value {
    if !st.cfg.license_gate_2_enabled {
        return json!({"enforced": false, "tenant": tenant, "state": "not_enforced", "code": null});
    }
    let Some(t) = tenant else {
        return json!({"enforced": true, "tenant": null, "state": "no_workspace", "code": null});
    };
    match evaluate(st, t).await {
        Ok(()) => json!({"enforced": true, "tenant": t, "state": "active", "code": null}),
        Err(e) => json!({"enforced": true, "tenant": t, "state": "blocked", "code": e.code(),
                         "message": e.to_string()}),
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    fn v(status: &str, reason: Option<&str>, age: i64) -> Verdict {
        Verdict {
            tenant_id: Uuid::nil(),
            status: status.to_string(),
            reason: reason.map(str::to_string),
            licence_id: Some("lic".to_string()),
            plan: None,
            kid: None,
            platform_owned: false,
            checked_at: Utc::now(),
            age_seconds: age,
        }
    }

    const FRESH: i64 = 5;
    const STALE: i64 = MAX_AGE_SECONDS + 1;

    /// Every verdict shape, its state label and how gate 2 reads it.
    fn cases() -> Vec<(Option<Verdict>, &'static str, Reading)> {
        vec![
            (None, "missing", Reading::Undecided),
            (Some(v("active", None, FRESH)), "active", Reading::Active),
            (
                Some(v("active", None, MAX_AGE_SECONDS)),
                "active",
                Reading::Active,
            ),
            (Some(v("active", None, STALE)), "stale", Reading::Undecided),
            (
                Some(v("none", Some("no_licence"), FRESH)),
                "none",
                Reading::Undecided,
            ),
            (
                Some(v("inactive", Some("license_unavailable"), FRESH)),
                "unavailable",
                Reading::Undecided,
            ),
            (
                Some(v("inactive", Some("license_unreadable"), FRESH)),
                "unreadable",
                Reading::Undecided,
            ),
            (
                Some(v("inactive", Some("tenant_suspended"), FRESH)),
                "suspended",
                Reading::Refused,
            ),
            (
                Some(v("revoked", Some("token_invalid"), FRESH)),
                "revoked",
                Reading::Refused,
            ),
            (
                Some(v("revoked", Some("license_revoked"), STALE)),
                "revoked",
                Reading::Refused,
            ),
            (
                Some(v("expired", Some("license_expired"), FRESH)),
                "expired",
                Reading::Refused,
            ),
            (
                Some(v("inactive", Some("license_inactive"), FRESH)),
                "inactive",
                Reading::Refused,
            ),
            (
                Some(v("inactive", Some("token_not_found"), FRESH)),
                "inactive",
                Reading::Refused,
            ),
            (
                Some(v("inactive", Some("license_not_found"), FRESH)),
                "inactive",
                Reading::Refused,
            ),
            (
                Some(v("inactive", Some("app_mismatch"), FRESH)),
                "inactive",
                Reading::Refused,
            ),
            (
                Some(v("inactive", Some("tenant_mismatch"), FRESH)),
                "inactive",
                Reading::Refused,
            ),
            (
                Some(v("inactive", Some("licence_mismatch"), FRESH)),
                "inactive",
                Reading::Refused,
            ),
            (
                Some(v("inactive", None, FRESH)),
                "inactive",
                Reading::Refused,
            ),
            (Some(v("bogus", None, FRESH)), "unknown", Reading::Refused),
        ]
    }

    #[test]
    fn states_and_readings() {
        for (verdict, state, read) in cases() {
            assert_eq!(state_of(verdict.as_ref()), state, "{verdict:?}");
            assert_eq!(reading(state), read, "{verdict:?}");
        }
    }

    #[test]
    fn decide_is_exhaustive() {
        for (verdict, state, read) in cases() {
            for owned in [false, true] {
                let want = owned || read == Reading::Active;
                assert_eq!(
                    decide(verdict.as_ref(), owned),
                    want,
                    "state={state} owned={owned}"
                );
            }
        }
    }

    #[test]
    fn refusal_codes() {
        let code = |v: Option<Verdict>| refusal(v.as_ref()).status_and_code();
        let p = StatusCode::PAYMENT_REQUIRED;
        let f = StatusCode::FORBIDDEN;
        assert_eq!(code(None), (p, "license_required"));
        assert_eq!(
            code(Some(v("none", Some("no_licence"), FRESH))),
            (p, "license_required")
        );
        assert_eq!(
            code(Some(v("active", None, STALE))),
            (p, "license_unavailable")
        );
        assert_eq!(
            code(Some(v("inactive", Some("license_unavailable"), FRESH))),
            (p, "license_unavailable")
        );
        assert_eq!(
            code(Some(v("inactive", Some("license_unreadable"), FRESH))),
            (p, "license_unreadable")
        );
        assert_eq!(
            code(Some(v("expired", Some("license_expired"), FRESH))),
            (p, "license_expired")
        );
        assert_eq!(
            code(Some(v("revoked", Some("token_invalid"), FRESH))),
            (p, "license_inactive")
        );
        assert_eq!(
            code(Some(v("inactive", Some("token_not_found"), FRESH))),
            (p, "license_inactive")
        );
        assert_eq!(code(Some(v("bogus", None, FRESH))), (p, "license_inactive"));
        assert_eq!(
            code(Some(v("inactive", Some("app_mismatch"), FRESH))),
            (f, "license_app_mismatch")
        );
        assert_eq!(
            code(Some(v("inactive", Some("tenant_mismatch"), FRESH))),
            (f, "license_tenant_mismatch")
        );
        assert_eq!(
            code(Some(v("inactive", Some("tenant_suspended"), FRESH))),
            (f, "tenant_suspended")
        );
    }

    #[test]
    fn exempt_routes() {
        for p in [
            "/api/license/status",
            "/api/me",
            "/api/me/view-as",
            "/api/platform/licensing",
        ] {
            assert!(exempt(p), "{p}");
        }
        for p in [
            "/api/jobs",
            "/api/mex",
            "/api/license/statusx",
            "/api/platformx",
            "/api/tenants/x/invites",
        ] {
            assert!(!exempt(p), "{p}");
        }
    }

    #[test]
    fn named_cases() {
        // fail closed: no definite fresh `active` → refused (platform-owned still admitted)
        assert!(!decide(None, false), "no row yet: refused");
        assert!(decide(None, true), "platform-owned");
        let unavailable = v("inactive", Some("license_unavailable"), FRESH);
        assert!(!decide(Some(&unavailable), false));
        assert!(!decide(Some(&v("revoked", None, FRESH)), false));
        assert!(
            decide(Some(&v("revoked", None, FRESH)), true),
            "platform-owned"
        );
        assert!(!decide(Some(&v("active", None, STALE)), false));
        assert!(decide(Some(&v("active", None, FRESH)), false));
    }
}
