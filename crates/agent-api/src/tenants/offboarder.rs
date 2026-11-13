//! Who may call `POST /api/tenants/{id}/offboard` (2026-10-25): a person super_admin (the web app), OR the ONE
//! secure-apps service account pinned by `TENANT_OFFBOARD_SA_{ISSUER,AZP,ROLE}` (crate::config::OffboardServiceConfig).
//! The service door exists so automation can remove an early-access tenant without a human password; it opens this
//! route and no other (no other handler takes [`Offboarder`]).
//!
//! A token whose VERIFIED issuer is the pinned one is judged as the service door, and only as it: `azp` must be the
//! pinned client, `aud` must contain it, the pinned REALM role must be held, and `super_admin` anywhere in the token is
//! refused (crate::signup::require_service, the signup doors' guard) — 403 `tenant_offboard_forbidden` otherwise.
//! Boot pins that issuer to a service-only realm, so a person token never takes this branch. Every other token
//! (any issuer, door unset) goes through the unchanged person path: [`SuperAdmin`], so an untrusted issuer is 401, a
//! secure-apps token with the door closed is 401 (service-only realm), a non-super_admin person is 403.
use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use crate::audit::{Actor, ActorType};
use crate::auth::{Caller, bearer};
use crate::error::ApiError;
use crate::state::AppState;
use crate::tenancy::SuperAdmin;

/// The offboard route's caller.
#[derive(Clone, Debug)]
pub enum Offboarder {
    /// A person holding the super_admin client role.
    Person(Caller),
    /// The pinned service account, by its client id (`azp`).
    Service { azp: String },
}

impl Offboarder {
    /// The audit actor: the person's `sub` (human, platform), or `system:<azp>` (system, platform).
    pub fn actor(&self) -> Actor {
        match self {
            Offboarder::Person(c) => Actor::person(c),
            Offboarder::Service { azp } => Actor {
                id: format!("system:{azp}"),
                kind: ActorType::System,
                platform: true,
                view_as: None,
            },
        }
    }
}

impl FromRequestParts<AppState> for Offboarder {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        if let Some(door) = st.cfg.tenant_offboard_service.as_ref() {
            match st.auth.verify_service(bearer(parts)?).await {
                Ok(c) if c.iss == door.issuer => {
                    let azp = crate::signup::require_service(
                        &c,
                        &door.issuer,
                        &st.cfg.super_admin_role,
                        std::slice::from_ref(&door.azp),
                        &door.role,
                        "tenant_offboard_forbidden",
                        "tenant offboard service door",
                    )?;
                    return Ok(Offboarder::Service { azp });
                }
                // Keycloak's JWKS is down: say so, as the person path would.
                Err(e @ ApiError::Unavailable(..)) => return Err(e),
                // Another issuer, or not a valid service token: the person path decides (and answers 401/403).
                _ => {}
            }
        }
        let SuperAdmin(c) = SuperAdmin::from_request_parts(parts, st).await?;
        Ok(Offboarder::Person(c))
    }
}
