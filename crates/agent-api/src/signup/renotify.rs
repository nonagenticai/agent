//! Re-post the Telegram new-signup notice (with its Approve/Reject buttons) for ONE pending signup — commonpractice
//! `scripts/renotify-signup.ts` (#478). The notice is posted once, at intake; if the message loses its buttons or is
//! deleted, this puts a fresh, tappable one in the topic. It writes NOTHING to the database: the row stays pending,
//! and the decision door is idempotent, so two live notices cannot decide one signup twice.
//!
//! Run in the agent-api pod (it needs the pod's configuration): `agent-api renotify-signup <signupId> [--apply]`
//! (`scripts/renotify-signup.sh` wraps it). Dry run by default.
use uuid::Uuid;

use crate::error::ApiResult;
use crate::platform_db::begin_platform;
use crate::signup::{store, telegram};
use crate::state::AppState;

/// What a renotify found or did.
#[derive(Debug, PartialEq, Eq)]
pub enum Renotice {
    /// Nothing to post: the reason (not found, not pending, an invite never notified, signup or Telegram off).
    Refused(String),
    /// Dry run: what would be posted (ids only, no personal data).
    DryRun(String),
    /// Posted (`true`) or Telegram did not accept it (`false`).
    Posted(bool),
}

/// Load the PENDING signup `id` and, with `apply`, post its new-signup notice and wait for the send.
pub async fn renotify(st: &AppState, id: Uuid, apply: bool) -> ApiResult<Renotice> {
    let Some(sg) = st.signup.clone() else {
        return Ok(Renotice::Refused(
            "early-access signup is off in this environment".into(),
        ));
    };
    let mut tx = begin_platform(&st.db).await?;
    let a = store::get(&mut tx, id).await?;
    let pending = store::pending_count(&mut tx).await?;
    tx.commit().await?;
    let Some(a) = a else {
        return Ok(Renotice::Refused(format!("signup {id} not found")));
    };
    if a.status != "pending" {
        return Ok(Renotice::Refused(format!(
            "signup {id} is {}, not pending; nothing to decide",
            a.status
        )));
    }
    if a.trust.is_none() {
        return Ok(Renotice::Refused(format!(
            "signup {id} has no trust tier (a super_admin invite); it was never notified"
        )));
    }
    let what = format!(
        "signup {id} · source {} · locale {} · {pending} pending",
        a.source,
        a.locale.as_deref().unwrap_or("-")
    );
    if !apply {
        return Ok(Renotice::DryRun(what));
    }
    if sg.cfg.telegram.is_none() {
        return Ok(Renotice::Refused(
            "Telegram signup notices are not configured here; nothing posted".into(),
        ));
    }
    let notice = telegram::NewSignup {
        id: a.id,
        name: &a.name,
        email: &a.email,
        company: a.company.as_deref(),
        message: a.message.as_deref(),
        source: &a.source,
        locale: a.locale.as_deref(),
        pending,
    };
    let sent = telegram::send_now(
        &sg.cfg,
        &sg.http,
        telegram::new_signup_payload(&sg.cfg, &notice),
        "renotify",
    )
    .await;
    Ok(Renotice::Posted(sent))
}
