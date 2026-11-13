//! The Keycloak and licences operations tenant admin and the signup approval share (B1 T4; D77). Extracted
//! from crate::signup::provisioning so create-tenant, invite, add-by-sub, rehome and `/api/me` call the same code
//! the approval journal does.
//!
//! Licences follow commonpractice `license-provisioning.ts` (D77): the entitlement row (or the tenant's
//! `licence_id IS NULL`) is written INSIDE the caller's transaction, the licence is minted AFTER it commits,
//! and the token is stored with a conditional UPDATE ([`store_member_entitlement`], [`store_tenant_licence`]).
//! Zero rows updated means another runner stored first ([`Stored::LostRace`]): the caller revokes its own
//! token. There is no pre-commit mint and no give-back-on-rollback, which could orphan a licence.
//!
//! Every Keycloak failure is an error, never "no such user": only a 404 on a read is `None`.
use sqlx::PgConnection;
use uuid::Uuid;

use crate::config::IdentityConfig;
use crate::crypto::licence::LicenceCipher;
use crate::error::{ApiError, ApiResult};
use crate::keycloak_admin::{
    ACCESS_ACTIONS, ActionsEmail, INVITE_ACTIONS, KeycloakAdmin, PortalUser,
};
use crate::licenses_admin::{Created, LicensesAdmin, registry_name};
use crate::service_token::UpstreamError;

/// Access-email code: the identity already existed without a password; the set-password email was sent.
pub const EXISTING_SET_PASSWORD: &str = "existing_identity_set_password_email";
/// Access-email code: the identity already signs in; an access email (no set-password) was sent.
pub const EXISTING_ACCESS: &str = "existing_identity_access_email";

/// The clients and settings of [`IdentityConfig`], built once at boot (`AppState.identity`).
pub struct Identity {
    pub cfg: IdentityConfig,
    pub keycloak: KeycloakAdmin,
    pub licences: LicensesAdmin,
    /// Seals licence tokens under the `LicenceToken` keyring (B7 T1, crate::crypto::licence) and opens them.
    pub cipher: LicenceCipher,
}

/// A minted licence, its token sealed for the tenant (`v2:<kid>:`, tenant id ‖ kid as associated data). Ids are TEXT,
/// as licences returns them (0030 / 0037).
#[derive(Clone, Debug)]
pub struct IssuedLicence {
    pub license_id: String,
    pub subject_id: String,
    pub token_sealed: String,
}

/// What a revoke achieved. Only [`RevokeOutcome::Revoked`] means the licence is gone: licences answers 200
/// `{"revoked": false}` for a no-op (commonpractice la.ts:1122) — an unknown licence id or another tenant's
/// (licenses `revoke_token_or_license`, "License not found"). An already-revoked licence answers `{"revoked": true}`
/// again (its `revoke_license` is idempotent), so a repeat revoke is `Revoked`, never `NotRevoked`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevokeOutcome {
    Revoked,
    /// Licences answered and did not revoke: `licence_not_revoked` (200 no-op) or `licence_revoke_refused` (4xx).
    NotRevoked(&'static str),
    /// Transport, 5xx, 408 or 429: try again later.
    Unavailable(String),
}

/// The result of a post-mint conditional store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stored {
    Stored,
    /// Another runner stored a licence first; the caller revokes the one it minted.
    LostRace,
    /// Licences answered an id that agent has revoked since (review #8: `find_active_license` handed back a still-active
    /// parked id, and the reconciler revoked it before this store). Nothing was stored; the row stays as it was and
    /// the caller mints again later (licences then issues a fresh licence).
    Stale,
}

/// Which access email [`Identity::send_access`] sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// A new identity: set a password and verify the address.
    SetPassword,
    /// An existing identity that cannot sign in yet: the set-password email ([`EXISTING_SET_PASSWORD`]).
    ExistingSetPassword,
    /// An existing identity that signs in: an access email only ([`EXISTING_ACCESS`]).
    ExistingAccess,
}

impl Access {
    /// The audit / log label of the email sent.
    pub fn as_str(self) -> &'static str {
        match self {
            Access::SetPassword => "set_password",
            Access::ExistingSetPassword => "existing_set_password",
            Access::ExistingAccess => "existing_access",
        }
    }

    /// The journal code of an existing-identity email; `None` for a new identity's set-password email.
    pub fn code(self) -> Option<&'static str> {
        match self {
            Access::SetPassword => None,
            Access::ExistingSetPassword => Some(EXISTING_SET_PASSWORD),
            Access::ExistingAccess => Some(EXISTING_ACCESS),
        }
    }
}

/// A failed [`Identity::send_access`]: which call failed (`keycloak_unavailable` for the login-state read,
/// `invite_email_failed` for the email) and why.
#[derive(Debug)]
pub struct AccessError {
    pub code: &'static str,
    pub error: UpstreamError,
}

/// The first / last name a new (or reused, name-less) portal identity gets. The portal realm's user profile requires
/// both: an identity missing either is stopped at Keycloak's VERIFY_PROFILE form before the set-password one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PersonNames {
    pub first: Option<String>,
    pub last: Option<String>,
}

/// Longest first or last name accepted (commonpractice: 80).
pub const PERSON_NAME_MAX: usize = 80;

impl PersonNames {
    /// commonpractice `inviteNames`: explicit names win (either one given = only the given ones are used); otherwise
    /// `name` split on whitespace (first word, then the rest), which leaves a one-word name without a last name.
    pub fn resolve(first: Option<String>, last: Option<String>, name: &str) -> Self {
        if first.is_some() || last.is_some() {
            return Self { first, last };
        }
        let (f, l) = crate::signup::provisioning::split_name(name);
        Self {
            first: f.map(str::to_string),
            last: l,
        }
    }

    /// `first last` (or whichever is set), for a display name.
    pub fn joined(&self) -> Option<String> {
        let parts: Vec<&str> = [self.first.as_deref(), self.last.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}

/// An optional first or last name from a request: trimmed; absent or blank = `None`; else one line of at most
/// [`PERSON_NAME_MAX`] characters, or 422 `invalid_person_name` naming `field`.
pub fn person_name(field: &str, raw: Option<&str>) -> ApiResult<Option<String>> {
    let Some(n) = raw.map(str::trim).filter(|n| !n.is_empty()) else {
        return Ok(None);
    };
    if n.chars().count() > PERSON_NAME_MAX || n.chars().any(char::is_control) {
        return Err(ApiError::Unprocessable(
            "invalid_person_name",
            format!("{field} must be one line of at most {PERSON_NAME_MAX} characters"),
        ));
    }
    Ok(Some(n.to_string()))
}

/// The synthetic licences subject that holds a tenant's own licence (commonpractice `cp-tenant:<id>:a`): never
/// a person, so a member leaving never takes the tenant licence with them.
pub fn tenant_licence_subject(tenant: Uuid) -> String {
    format!("agent-tenant:{tenant}:a")
}

/// A Keycloak failure as a route error: 502 `keycloak_error`. Never "no such user".
pub fn keycloak_error(e: &UpstreamError) -> ApiError {
    tracing::warn!(error = %e, status = ?e.status, "keycloak_admin_failed");
    ApiError::BadGateway(
        "keycloak_error",
        "the identity service did not answer as expected; nothing was changed".into(),
    )
}

/// A licences failure as a route error: 502 `licences_unavailable`.
pub fn licences_unavailable(e: &UpstreamError) -> ApiError {
    tracing::warn!(error = %e, status = ?e.status, "licences_admin_failed");
    ApiError::BadGateway(
        "licences_unavailable",
        "the licence service did not answer as expected".into(),
    )
}

impl Identity {
    pub fn new(cfg: IdentityConfig, http: reqwest::Client) -> Self {
        Self {
            keycloak: KeycloakAdmin::new(&cfg.keycloak_admin, http.clone()),
            licences: LicensesAdmin::new(&cfg.licenses_admin, http),
            cipher: LicenceCipher::new(&cfg.licence_keys),
            cfg,
        }
    }

    /// The portal identity for `email`: `(sub, true)` when it already existed (its EMPTY first/last name filled from
    /// `names`, non-fatal), else a new one created WITHOUT an email (`(sub, false)`) carrying `names` and its `locale`
    /// written (non-fatal). Both names present = the portal realm's user profile is complete, so the invitee's first
    /// sign-in asks only for a password (commonpractice `findOrCreateRealmIdentity` + `backfillMissingNames`).
    pub async fn resolve_or_create(
        &self,
        email: &str,
        names: &PersonNames,
        locale: Option<&str>,
    ) -> Result<(String, bool), UpstreamError> {
        let (first, last) = (names.first.as_deref(), names.last.as_deref());
        if let Some(sub) = self.keycloak.find_by_email(email).await? {
            self.backfill_names(&sub, names).await;
            return Ok((sub, true));
        }
        match self.keycloak.create_user(email, first, last, &[]).await? {
            Some(sub) => {
                if let Some(l) = locale
                    && let Err(e) = self
                        .keycloak
                        .update_attributes(&sub, &[("locale", Some(l))])
                        .await
                {
                    tracing::warn!(error = %e, "identity_locale_attribute_failed");
                }
                Ok((sub, false))
            }
            // 409: created concurrently (or by another app) since the lookup.
            None => match self.keycloak.find_by_email(email).await? {
                Some(sub) => {
                    self.backfill_names(&sub, names).await;
                    Ok((sub, true))
                }
                None => Err(UpstreamError::new(
                    Some(409),
                    "keycloak create user answered 409 but no user has that address",
                )),
            },
        }
    }

    /// Fill a reused identity's EMPTY names (never overwrite one). Only with BOTH names given: a lone first name is
    /// at best a split e-mail local part, which must not be written onto an account another app owns, and could not
    /// complete the profile anyway. Non-fatal, like commonpractice: the identity can sign in without them
    /// (Keycloak's profile form then asks), but it is said loudly.
    async fn backfill_names(&self, sub: &str, names: &PersonNames) {
        if names.first.is_none() || names.last.is_none() {
            return;
        }
        if let Err(e) = self
            .keycloak
            .backfill_names(sub, names.first.as_deref(), names.last.as_deref())
            .await
        {
            tracing::warn!(error = %e, status = ?e.status, "identity_name_backfill_failed");
        }
    }

    /// The identity `sub`; `None` only when Keycloak says 404.
    pub async fn get_user(&self, sub: &str) -> Result<Option<PortalUser>, UpstreamError> {
        self.keycloak.get_user(sub).await
    }

    /// Does `sub` hold `role` on the `agent` client (`AGENT_KC_CLIENT_UUID`; no client lookup, `view-users` only)?
    pub async fn has_agent_role(&self, sub: &str, role: &str) -> Result<bool, UpstreamError> {
        self.keycloak
            .has_client_role(sub, &self.cfg.keycloak_admin.agent_client_uuid, role)
            .await
    }

    /// The one email that lets `sub` into the app (commonpractice `deliver`): a new identity gets the
    /// set-password email; an existing one that cannot sign in yet gets it too; one that signs in gets an
    /// access email only. The link lands in the app (`SIGNUP_FRONTEND_CLIENT_ID`, `AGENT_PUBLIC_URL/`).
    pub async fn send_access(&self, sub: &str, existed: bool) -> Result<Access, AccessError> {
        let (actions, access): (Vec<&str>, Access) = if existed {
            let state = self
                .keycloak
                .login_state(sub)
                .await
                .map_err(|error| AccessError {
                    code: "keycloak_unavailable",
                    error,
                })?;
            if state.needs_setup {
                let actions = if state.email_verified {
                    vec!["UPDATE_PASSWORD"]
                } else {
                    INVITE_ACTIONS.to_vec()
                };
                (actions, Access::ExistingSetPassword)
            } else {
                (ACCESS_ACTIONS.to_vec(), Access::ExistingAccess)
            }
        } else {
            (INVITE_ACTIONS.to_vec(), Access::SetPassword)
        };
        let redirect = format!("{}/", self.cfg.public_url);
        let to = ActionsEmail {
            actions: &actions,
            client_id: &self.cfg.frontend_client_id,
            redirect_uri: &redirect,
            lifespan_seconds: self.cfg.invite_email_lifespan_seconds,
        };
        self.keycloak
            .execute_actions_email(sub, &to)
            .await
            .map_err(|error| AccessError {
                code: "invite_email_failed",
                error,
            })?;
        Ok(access)
    }

    /// Create (or, licences being idempotent per tenant/app/subject, find) `sub`'s licence on `tenant`, in the
    /// region of the tenant's `residency` (B1b T10).
    pub async fn create_licence(
        &self,
        tenant: Uuid,
        sub: &str,
        residency: &str,
    ) -> Result<Created, UpstreamError> {
        self.licences.create_licence(tenant, sub, residency).await
    }

    /// Mint `license_id`'s token and seal it for `tenant`.
    pub async fn mint_sealed(
        &self,
        tenant: Uuid,
        license_id: &str,
    ) -> Result<String, UpstreamError> {
        let token = self.licences.mint(tenant, license_id).await?;
        self.cipher
            .seal(tenant, &token)
            .map_err(|e| UpstreamError::new(None, format!("seal the licence token: {e}")))
    }

    /// Register `tenant` (as `name`) in the licences registry, create `holder`'s licence, mint and seal it.
    async fn issue(
        &self,
        tenant: Uuid,
        name: &str,
        residency: &str,
        holder: &str,
    ) -> Result<IssuedLicence, UpstreamError> {
        self.licences
            .register_tenant(tenant, &registry_name(name, tenant))
            .await?;
        let c = self.create_licence(tenant, holder, residency).await?;
        let token_sealed = self.mint_sealed(tenant, &c.license_id).await?;
        Ok(IssuedLicence {
            license_id: c.license_id,
            subject_id: c.subject_id,
            token_sealed,
        })
    }

    /// The tenant's own licence (holder [`tenant_licence_subject`]). Store it with [`store_tenant_licence`].
    pub async fn issue_tenant_licence(
        &self,
        tenant: Uuid,
        name: &str,
        residency: &str,
    ) -> Result<IssuedLicence, UpstreamError> {
        self.issue(tenant, name, residency, &tenant_licence_subject(tenant))
            .await
    }

    /// `sub`'s member licence on `tenant`. Store it with [`store_member_entitlement`].
    pub async fn issue_member_licence(
        &self,
        tenant: Uuid,
        name: &str,
        residency: &str,
        sub: &str,
    ) -> Result<IssuedLicence, UpstreamError> {
        self.issue(tenant, name, residency, sub).await
    }

    /// Revoke `license_id` on `tenant`, reading the body (a 200 can be a no-op).
    ///
    /// Bounded by `LICENCES_REVOKE_TIMEOUT_MS` (B1b T8): the member / tenant revokes make this call while their
    /// transaction holds the licence lock and a pooled connection (review #8), so a hung licences service must not
    /// hold them for the HTTP client's whole timeout per call and drain the pool. Past the bound the call is dropped
    /// (the request is cancelled) and the outcome is [`RevokeOutcome::Unavailable`]: the row stays `revoking` /
    /// parked, the caller commits (releasing the lock and the connection) and the provisioning tick retries.
    pub async fn revoke_licence(&self, tenant: Uuid, license_id: &str) -> RevokeOutcome {
        let bound = std::time::Duration::from_millis(self.cfg.licences_revoke_timeout_ms);
        let Ok(answer) =
            tokio::time::timeout(bound, self.licences.revoke(tenant, license_id)).await
        else {
            metrics::counter!("agent_licence_revoke_timeouts_total").increment(1);
            tracing::warn!(%tenant, bound_ms = self.cfg.licences_revoke_timeout_ms, "licence_revoke_timed_out");
            return RevokeOutcome::Unavailable(format!(
                "licences revoke did not answer within {} ms",
                self.cfg.licences_revoke_timeout_ms
            ));
        };
        match answer {
            Ok(true) => RevokeOutcome::Revoked,
            Ok(false) => RevokeOutcome::NotRevoked("licence_not_revoked"),
            Err(e) => match e.status {
                Some(s) if (400..500).contains(&s) && s != 408 && s != 429 => {
                    RevokeOutcome::NotRevoked("licence_revoke_refused")
                }
                _ => RevokeOutcome::Unavailable(e.message),
            },
        }
    }
}

/// Serialise every store and every revoke of licence `license_id` on `tenant` (review #8), for the rest of the
/// transaction on `conn`. A revoke holds it across its licences call, so a store either lands before the revoke
/// starts (the revoke then sees the id held and skips it) or after it committed (the store then sees it revoked).
pub async fn lock_licence(
    conn: &mut PgConnection,
    tenant: Uuid,
    license_id: &str,
) -> ApiResult<()> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('agent-licence:' || $1::uuid::text || ':' || $2::text, 0))",
    )
    .bind(tenant)
    .bind(license_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Has agent revoked `license_id` on the scoped `tenant`? Every revoke that licences confirmed writes a
/// `licence_revocation` tombstone (B7 T3c, crate::licensing::store::record_revocation) in the transaction that
/// recorded it; the table is append-only (no UPDATE / DELETE grant) and outlives audit retention, so a licence id,
/// once revoked, is never stored again.
pub async fn licence_revoked(
    conn: &mut PgConnection,
    tenant: Uuid,
    license_id: &str,
) -> ApiResult<bool> {
    crate::licensing::store::is_revoked(conn, tenant, license_id).await
}

/// Is `license_id` parked by a licence re-issue whose revoke did not confirm (F-31, crate::licensing::reissue)? Such
/// an id is being REPLACED: licences still answers it for the holder's subject while it is active, so a mint before
/// the reconciler drains it would hand the old licence back. It must not be stored (and so not unparked by
/// [`claim_parked`]): the drain revokes it, and the next mint gets a new licence. Read under [`lock_licence`].
pub async fn reissue_parked(
    conn: &mut PgConnection,
    tenant: Uuid,
    license_id: &str,
) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM tenants WHERE id = $1 AND $2 = ANY (licence_revoke_pending)) \
            AND EXISTS (SELECT 1 FROM audit_event \
                         WHERE tenant_id = $1 AND action = 'license.revoke_failed' \
                           AND details ->> 'via' = 'reissue' AND details ->> 'license_id' = $2)",
    )
    .bind(tenant)
    .bind(license_id)
    .fetch_one(conn)
    .await?)
}

/// Before a store (under [`lock_licence`]): [`Stored::Stale`] when the id was revoked since licences answered it, or
/// is parked by a re-issue that is replacing it ([`reissue_parked`]).
async fn stale(conn: &mut PgConnection, tenant: Uuid, license_id: &str) -> ApiResult<bool> {
    lock_licence(conn, tenant, license_id).await?;
    if licence_revoked(conn, tenant, license_id).await? {
        tracing::warn!(%tenant, "licence_store_refused_revoked_id");
        return Ok(true);
    }
    if reissue_parked(conn, tenant, license_id).await? {
        metrics::counter!("agent_licence_reissue_stale_total").increment(1);
        tracing::warn!(%tenant, "licence_store_refused_reissue_parked_id");
        return Ok(true);
    }
    Ok(false)
}

/// Unpark `license_id` if it is parked (true when it was), BEFORE the conditional store: the `tenants` row is locked
/// ahead of the entitlement row, the order the membership and offboard transactions take, so a store cannot deadlock
/// against them. A store that then loses parks it again (net nothing).
async fn claim_parked(conn: &mut PgConnection, tenant: Uuid, license_id: &str) -> ApiResult<bool> {
    let n = sqlx::query(
        "UPDATE tenants SET licence_revoke_pending = array_remove(licence_revoke_pending, $2) \
          WHERE id = $1 AND $2 = ANY (licence_revoke_pending)",
    )
    .bind(tenant)
    .bind(license_id)
    .execute(conn)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// Store a minted member licence on `sub`'s entitlement, only while it is still `pending` without a licence
/// (D77 L4). `conn` is scoped to `tenant` (`begin_scoped`). Zero rows = [`Stored::LostRace`]; the winner's row
/// is left as it is. Review #8: an id agent revoked since is [`Stored::Stale`]; a stored id that is parked in
/// `tenants.licence_revoke_pending` (licences re-issued a still-active licence of this member that a failed revoke had
/// parked) is unparked in the same transaction: it is this member's live licence now, not one to revoke.
pub async fn store_member_entitlement(
    conn: &mut PgConnection,
    tenant: Uuid,
    sub: &str,
    l: &IssuedLicence,
) -> ApiResult<Stored> {
    if stale(conn, tenant, &l.license_id).await? {
        return Ok(Stored::Stale);
    }
    // T0d: the tenants row before the entitlement row (advisory lock → tenants → entitlement), also when nothing is
    // parked (claim_parked's UPDATE then locks nothing): the caller's `license.issue` audit reaches it after the
    // store, which deadlocked against an offboard holding the row and updating the entitlement.
    crate::tenancy::lock_tenant_row(conn, tenant).await?;
    let claimed = claim_parked(conn, tenant, &l.license_id).await?;
    let n = sqlx::query(
        "UPDATE member_entitlement \
            SET license_id = $3, license_subject_id = $4, token_encrypted = $5, status = 'active', \
                error_code = NULL, updated_at = now() \
          WHERE tenant_id = $1 AND user_sub = $2 AND status = 'pending' AND license_id IS NULL",
    )
    .bind(tenant)
    .bind(sub)
    .bind(&l.license_id)
    .bind(&l.subject_id)
    .bind(&l.token_sealed)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if n != 1 {
        if claimed {
            crate::members::park(conn, tenant, &l.license_id).await?;
        }
        return Ok(Stored::LostRace);
    }
    Ok(Stored::Stored)
}

/// Store a minted tenant licence on `tenants`, only while `licence_id IS NULL` and the tenant is `active` (D77 L1/L4;
/// a licence minted while an offboard committed must not land on the archived tenant). `conn` is scoped to
/// `tenant`. Zero rows = [`Stored::LostRace`]. Review #8, as [`store_member_entitlement`]: a revoked id is
/// [`Stored::Stale`], a stored parked id is unparked.
pub async fn store_tenant_licence(
    conn: &mut PgConnection,
    tenant: Uuid,
    l: &IssuedLicence,
) -> ApiResult<Stored> {
    if stale(conn, tenant, &l.license_id).await? {
        return Ok(Stored::Stale);
    }
    let claimed = claim_parked(conn, tenant, &l.license_id).await?;
    let n = sqlx::query(
        "UPDATE tenants SET licence_id = $2, licence_token_encrypted = $3 \
          WHERE id = $1 AND licence_id IS NULL AND status = 'active'",
    )
    .bind(tenant)
    .bind(&l.license_id)
    .bind(&l.token_sealed)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if n != 1 {
        if claimed {
            crate::members::park(conn, tenant, &l.license_id).await?;
        }
        return Ok(Stored::LostRace);
    }
    Ok(Stored::Stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_codes_name_only_existing_identities() {
        assert_eq!(Access::SetPassword.code(), None);
        assert_eq!(
            Access::ExistingSetPassword.code(),
            Some(EXISTING_SET_PASSWORD)
        );
        assert_eq!(Access::ExistingAccess.code(), Some(EXISTING_ACCESS));
        assert!(tenant_licence_subject(Uuid::nil()).starts_with("agent-tenant:"));
    }
}
