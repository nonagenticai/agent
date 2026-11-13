//! Invite acceptance on sign-in (CP `acceptInvite`): a person who is not a member yet and whose access token marks
//! its `email` VERIFIED accepts a pending, unexpired `member_invites` row — one bound to its subject first (a
//! signup's pre-existing identity), else the oldest unbound one for that address (`agent_accept_invite`, migration
//! 0040: claimed once under concurrent sign-ins, the member seated with the
//! invite's role set and a `pending` entitlement, in one transaction). After COMMIT: the audit row
//! `user.invite_accept`, then the licence — for an invite a signup approval recorded, through that signup's own
//! `member_licence` step (its seat / email steps are marked done, so the journal shows how it finished); else
//! [`crate::members::ensure_member_licence`]. An unverified or absent email, or an expired invite, accepts nothing.
use uuid::Uuid;

use crate::audit::{self, Actor, Event};
use crate::auth::RoleSet;
use crate::error::ApiResult;
use crate::platform_db::begin_platform;
use crate::state::AppState;

/// `invite_accepted`: the code the signup journal records for the steps the acceptance completed.
pub const INVITE_ACCEPTED: &str = "invite_accepted";

/// One `agent_accept_invite` row: (invite, tenant, roles, signup, email mismatch).
type Accepted = (Uuid, Uuid, Vec<String>, Option<Uuid>, bool);

/// Accept a pending invite for `email` (the caller checked the token marks it verified) as `sub`. `true` when one
/// was accepted (the caller re-reads the home tenant).
pub async fn accept_invite(st: &AppState, sub: &str, email: &str, name: &str) -> ApiResult<bool> {
    let row: Option<Accepted> = sqlx::query_as(
        "SELECT o_invite, o_tenant, o_roles, o_signup, o_mismatch FROM agent_accept_invite($1, $2, $3)",
    )
    .bind(sub)
    .bind(email)
    .bind(name)
    .fetch_optional(&st.db)
    .await?;
    let Some((invite, tenant, roles, signup, mismatch)) = row else {
        return Ok(false);
    };
    let actor = Actor::member(sub, false);
    if mismatch {
        // CP `invite_email_mismatch`: the invite bound to this account went to another address; it stays pending.
        tracing::warn!(%tenant, %invite, code = "invite_email_mismatch", "invite_accept_refused");
        // Audited once per throttle window, not on every request of that account.
        if !st.seen.due(format!("invite_mismatch\u{1f}{sub}")) {
            return Ok(false);
        }
        audit::record_alone(
            &st.db,
            tenant,
            &actor,
            Event::new("user.invite_accept", "user", sub.to_string())
                .failed("invite_email_mismatch")
                .detail("invite", invite.to_string()),
        )
        .await?;
        return Ok(false);
    }
    let mut e = Event::new("user.invite_accept", "user", sub.to_string())
        .detail("invite", invite.to_string())
        .detail("roles", RoleSet::from_db(&roles)?.keys());
    if let Some(s) = signup {
        e = e.detail("signup", s.to_string());
    }
    audit::record_alone(&st.db, tenant, &actor, e).await?;
    tracing::info!(%tenant, %invite, "invite_accepted");
    match (signup, st.signup.clone()) {
        (Some(id), Some(sg)) => {
            complete_signup(st, id, sub).await?;
            if let Err(e) = crate::signup::provisioning::run(st, &sg, id).await {
                tracing::warn!(signup = %id, error = %e, "invite_accept_signup_run_failed");
            }
        }
        _ => {
            if let Err(e) = crate::members::ensure_member_licence(st, tenant, sub, &actor).await {
                tracing::warn!(%tenant, error = %e, "invite_accept_licence_failed");
            }
        }
    }
    Ok(true)
}

/// The signup that recorded the invite: its skipped seat is now `ok`, its member licence due now (run by the same
/// step the approve uses), its email `ok` (the person is signed in; nothing to send). The identity is the subject
/// that accepted.
async fn complete_signup(st: &AppState, id: Uuid, sub: &str) -> ApiResult<()> {
    let mut tx = begin_platform(&st.db).await?;
    sqlx::query(
        "UPDATE signup_provisioning_steps SET data = data || jsonb_build_object('sub', $2::text), updated_at = now() \
          WHERE signup_id = $1 AND step = 'keycloak_user'",
    )
    .bind(id)
    .bind(sub)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE signup_provisioning_steps \
            SET status = CASE step WHEN 'member_licence' THEN 'pending' ELSE 'ok' END, \
                code = CASE step WHEN 'member_licence' THEN NULL ELSE $2 END, \
                message = CASE step \
                    WHEN 'membership' THEN 'seated when the requester signed in with the verified invited email' \
                    WHEN 'invite_email' THEN 'no email needed: the requester signed in and accepted the invite' END, \
                attempts = 0, last_attempt_at = now(), lease_until = NULL, updated_at = now(), \
                next_attempt_at = CASE step WHEN 'member_licence' THEN now() END \
          WHERE signup_id = $1 AND step IN ('membership', 'member_licence', 'invite_email') AND status = 'skipped'",
    )
    .bind(id)
    .bind(INVITE_ACCEPTED)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
