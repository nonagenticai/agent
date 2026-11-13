//! Licensing ops routes (B7 T3, T3a, T3b) and the licence status route. Every ops route is super_admin only
//! (`SuperAdmin`: else 403 `super_admin_required`); none answers or audits a token.
//!
//! - `GET /api/license/status` (spec 04 §4b; any verified person, unprovisioned included) — what licence gate 2 says
//!   about the caller's tenant (a super_admin's: `X-Active-Tenant-Id`, else their home tenant; everyone else's: their
//!   home tenant, the header ignored), as data: exempt from gate 2 and never 402 (crate::licensing::gate::status). 400
//!   `invalid_active_tenant` for a malformed header from a super_admin.
//! - `GET /api/platform/licensing` (`licensingOverview`) — crate::licensing::overview.
//! - `POST /api/platform/licence-tokens/reseal` (`resealLicences`) — every non-archived tenant's tokens to the current
//!   kid; 200 `ResealResult {kid, resealed, remaining, tenants}`; one `licence_reseal_run` row + platform audit
//!   `license.reseal` (counts only). Idempotent (crate::licensing::reseal).
//! - `POST /api/platform/licence-verdicts/refresh` (`nudgeLicenceVerdicts`) — one refresher pass now (every
//!   non-archived tenant's verdict, crate::licensing::verdict::refresh_all) plus a nudge to every replica, then the
//!   overview (with `refreshed` counts).
//! - `POST /api/tenants/{id}/licence/reseal` (`resealTenantLicence`) — one tenant; 404 `tenant_not_found`, 409
//!   `tenant_archived` / `no_licence`.
//! - `POST /api/tenants/{id}/licence/reissue` `{"tenant_licence"?: bool, "member_sub"?: string}` (F-31) — re-issues
//!   the tenant's own licence and/or one member's licence in the tenant's residency region, keeping each holder's
//!   licences subject and disabling nobody (crate::licensing::reissue). At least one of the two, else 422
//!   `invalid_input`. 200 `{tenant_id, kind, residency, members_unchanged, results: [{holder, sub, old_licence_id,
//!   new_licence_id, old_revoke, status, code}]}`; 404 `tenant_not_found` / `user_not_found`; 409 `tenant_archived`,
//!   `tenant_suspended`, `no_licence`, `concurrent_reissue`, `concurrent_change`, `licence_revoke_refused`, `region_unmapped`; 502
//!   `licences_unavailable` (nothing changed).
//! - `POST /api/tenants/{id}/licence/remint` `{"confirm_name": string, "reason"?: string}` (F-32, R1) — the leaked
//!   token path: the tenant licence is replaced and the old one revoked at once; the same answer and refusals as a
//!   re-issue plus 422 `confirm_name_mismatch` (nothing changed) and 422 `invalid_input` (a `reason` over 200
//!   characters or shaped like a secret / an email address). `old_revoke: pending` = licences did not confirm the
//!   revoke; the tenant gets a `licence` attention item until the tick drains it.
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::audit::{Actor, scrub};
use crate::auth::Caller;
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::licensing::reissue::{self, Holder};
use crate::licensing::{gate, overview, reseal, verdict};
use crate::state::AppState;
use crate::tenancy::{SuperAdmin, active_tenant_from, tenant_id};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(gate::STATUS_PATH, get(licence_status))
        .route("/api/platform/licensing", get(licensing_overview))
        .route("/api/platform/licence-tokens/reseal", post(reseal_all))
        .route(
            "/api/platform/licence-verdicts/refresh",
            post(refresh_verdicts),
        )
        .route(
            "/api/tenants/{tenant_id}/licence/reseal",
            post(reseal_tenant),
        )
        .route(
            "/api/tenants/{tenant_id}/licence/reissue",
            post(reissue_licence),
        )
        .route(
            "/api/tenants/{tenant_id}/licence/remint",
            post(remint_licence),
        )
}

async fn licence_status(
    caller: Caller,
    State(st): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let header = if caller.super_admin {
        active_tenant_from(&headers)?
    } else {
        None
    };
    let tenant = header.or_else(|| caller.home.as_ref().map(|h| h.tenant));
    Ok(Json(gate::status(&st, tenant).await))
}

async fn licensing_overview(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    Ok(Json(overview::overview(&st).await?))
}

async fn reseal_all(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<reseal::Resealed>> {
    Ok(Json(reseal::reseal_all(&st, &Actor::person(&c)).await?))
}

async fn refresh_verdicts(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let refreshed = verdict::refresh_all(&st).await?;
    if let Err(e) = verdict::nudge(&st.db, None).await {
        tracing::warn!(error = %e, "licence_verdict_nudge_failed");
    }
    let mut v = overview::overview(&st).await?;
    v["refreshed"] = serde_json::to_value(refreshed).unwrap_or(Value::Null);
    Ok(Json(v))
}

async fn reseal_tenant(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TenantPath>,
) -> ApiResult<Json<reseal::Resealed>> {
    let tenant = tenant_id(&p.tenant_id)?;
    Ok(Json(
        reseal::reseal_one(&st, &Actor::person(&c), tenant).await?,
    ))
}

#[derive(Deserialize)]
struct TenantPath {
    tenant_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReissueBody {
    /// Re-issue the tenant's own licence.
    tenant_licence: Option<bool>,
    /// Re-issue this member's licence (a Keycloak `sub` of a member of THIS tenant).
    member_sub: Option<String>,
}

/// The holders `b` names (tenant first), else 422 `invalid_input`.
fn holders(b: ReissueBody) -> ApiResult<Vec<Holder>> {
    let mut out = Vec::new();
    if b.tenant_licence == Some(true) {
        out.push(Holder::Tenant);
    }
    if let Some(raw) = b.member_sub {
        let sub = raw.trim();
        if sub.is_empty()
            || sub.len() > 255
            || sub.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(ApiError::Unprocessable(
                "invalid_input",
                "member_sub must be one Keycloak subject".into(),
            ));
        }
        out.push(Holder::Member(sub.to_string()));
    }
    if out.is_empty() {
        return Err(ApiError::Unprocessable(
            "invalid_input",
            "name what to re-issue: tenant_licence: true and/or member_sub".into(),
        ));
    }
    Ok(out)
}

async fn reissue_licence(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TenantPath>,
    ApiJson(b): ApiJson<ReissueBody>,
) -> ApiResult<Json<Value>> {
    let tenant = tenant_id(&p.tenant_id)?;
    let holders = holders(b)?;
    let answer = reissue::reissue(&st, &Actor::person(&c), tenant, holders).await?;
    Ok(Json(answer.to_json()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemintBody {
    /// The tenant's exact name (the type-to-confirm of the re-mint dialog).
    confirm_name: String,
    /// Why (free text for the platform audit; optional).
    reason: Option<String>,
}

/// The re-mint `reason`, trimmed (`None` when blank), else 422 `invalid_input`: at most 200 characters, and nothing the
/// audit would refuse as secret- or email-shaped (crate::audit::scrub).
fn remint_note(raw: Option<&str>) -> ApiResult<Option<String>> {
    let Some(r) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
        return Ok(None);
    };
    if r.chars().count() > 200 {
        return Err(ApiError::Unprocessable(
            "invalid_input",
            "reason is at most 200 characters".into(),
        ));
    }
    if r.split_whitespace().any(scrub::forbidden_value) || scrub::forbidden_value(r) {
        return Err(ApiError::Unprocessable(
            "invalid_input",
            "reason must not contain a token, a secret or an email address".into(),
        ));
    }
    Ok(Some(r.to_string()))
}

async fn remint_licence(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TenantPath>,
    ApiJson(b): ApiJson<RemintBody>,
) -> ApiResult<Json<Value>> {
    let tenant = tenant_id(&p.tenant_id)?;
    let note = remint_note(b.reason.as_deref())?;
    let answer = reissue::remint(
        &st,
        &Actor::person(&c),
        tenant,
        &b.confirm_name,
        note.as_deref(),
    )
    .await?;
    Ok(Json(answer.to_json()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holders_need_at_least_one_and_keep_tenant_first() {
        let b = |t: Option<bool>, m: Option<&str>| ReissueBody {
            tenant_licence: t,
            member_sub: m.map(str::to_string),
        };
        assert_eq!(holders(b(None, None)).unwrap_err().code(), "invalid_input");
        assert_eq!(
            holders(b(Some(false), None)).unwrap_err().code(),
            "invalid_input"
        );
        assert_eq!(
            holders(b(None, Some(" "))).unwrap_err().code(),
            "invalid_input"
        );
        assert_eq!(
            holders(b(Some(true), Some(" u-1 "))).unwrap(),
            vec![Holder::Tenant, Holder::Member("u-1".into())]
        );
    }

    #[test]
    fn remint_note_is_optional_bounded_and_secret_free() {
        assert_eq!(remint_note(None).unwrap(), None);
        assert_eq!(remint_note(Some("  ")).unwrap(), None);
        assert_eq!(
            remint_note(Some(" pasted in a public channel ")).unwrap(),
            Some("pasted in a public channel".to_string())
        );
        assert_eq!(
            remint_note(Some(&"x".repeat(201))).unwrap_err().code(),
            "invalid_input"
        );
        assert_eq!(
            remint_note(Some("sent to ops@example.com"))
                .unwrap_err()
                .code(),
            "invalid_input"
        );
        assert_eq!(
            remint_note(Some("eyJhbGciOiJIUzI1NiJ9.e30.x"))
                .unwrap_err()
                .code(),
            "invalid_input"
        );
    }
}
