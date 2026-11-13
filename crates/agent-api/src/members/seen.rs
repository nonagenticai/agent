//! Member sessions (B1 T8): which member was seen in which Keycloak session on which surface, for
//! `memberActivity`. Written on tenant resolution (members, not super_admins) and on `GET /api/me`, at most once per
//! (sub, session, surface) per 5 minutes (an in-memory throttle capped at 10 000 entries), in its own scoped
//! transaction. The first seen request also clears `users.invited_at` (the invite was accepted). A failure is logged
//! and counted (`agent_member_seen_errors_total`) and never fails the request. Rows older than 35 days are swept
//! hourly in-process (no CronJob), together with the invite expiry flip (B1b).
use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::Caller;
use crate::error::ApiResult;
use crate::state::AppState;
use crate::tenancy::begin_scoped;

/// One write per key per this long.
pub const THROTTLE: Duration = Duration::from_secs(300);
/// The throttle map never holds more keys than this.
pub const MAX_ENTRIES: usize = 10_000;
/// Sessions not seen for this many days are deleted.
pub const RETENTION_DAYS: i32 = 35;
/// The `X-Agent-Surface` request header.
pub const SURFACE_HEADER: &str = "x-agent-surface";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    Admin,
    Console,
    Chat,
    /// The staff workspace (`/support`): what support routing reads as "online".
    Support,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Surface::Admin => "admin",
            Surface::Console => "console",
            Surface::Chat => "chat",
            Surface::Support => "support",
        }
    }
}

/// `X-Agent-Surface: admin|console|chat|support`; anything else (or nothing) is `admin`.
pub fn surface(headers: &HeaderMap) -> Surface {
    match headers
        .get(SURFACE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("console") => Surface::Console,
        Some("chat") => Surface::Chat,
        Some("support") => Surface::Support,
        _ => Surface::Admin,
    }
}

/// An in-memory "at most once per [`THROTTLE`] per key" gate, per process (`AppState.seen`).
#[derive(Default)]
pub struct Throttle {
    map: Mutex<HashMap<String, Instant>>,
}

impl Throttle {
    /// `true` (and the key is stamped now) when `key` was not stamped within [`THROTTLE`].
    pub fn due(&self, key: String) -> bool {
        let mut m = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        if let Some(t) = m.get(&key)
            && now.duration_since(*t) < THROTTLE
        {
            return false;
        }
        if m.len() >= MAX_ENTRIES {
            m.retain(|_, t| now.duration_since(*t) < THROTTLE);
            if m.len() >= MAX_ENTRIES {
                m.clear();
            }
        }
        m.insert(key, now);
        true
    }
}

async fn write(
    st: &AppState,
    sub: &str,
    tenant: Uuid,
    sid: &str,
    surface: Surface,
) -> ApiResult<()> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    sqlx::query(
        "INSERT INTO member_session (tenant_id, sub, sid, surface, support_seen_at) \
         VALUES ($1, $2, $3, $4, CASE WHEN $4 = 'support' THEN now() END) \
         ON CONFLICT (tenant_id, sub, sid) DO UPDATE SET last_seen_at = now(), surface = EXCLUDED.surface, \
            support_seen_at = CASE WHEN EXCLUDED.surface = 'support' THEN now() \
                                   ELSE member_session.support_seen_at END",
    )
    .bind(tenant)
    .bind(sub)
    .bind(sid)
    .bind(surface.as_str())
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE users SET invited_at = NULL WHERE sub = $1 AND invited_at IS NOT NULL")
        .bind(sub)
        .execute(&mut *tx)
        .await?;
    // The member signed in: their pending invite here is accepted (0040, F-01), also one the hourly sweep already
    // flipped to `expired` (B1b): a seated invitee who signs in late is a member, not an expired invite.
    sqlx::query(
        "UPDATE member_invites SET state = 'accepted', accepted_at = now(), accepted_by = $1, updated_at = now() \
          WHERE invitee_sub = $1 AND state IN ('pending', 'expired')",
    )
    .bind(sub)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Record that `caller` was seen in `tenant` on `surface` (throttled; never fails).
pub async fn record(st: &AppState, caller: &Caller, tenant: Uuid, surface: Surface) {
    let key = format!(
        "seen\u{1f}{}\u{1f}{}\u{1f}{}",
        caller.sub,
        caller.session,
        surface.as_str()
    );
    if !st.seen.due(key) {
        return;
    }
    if let Err(e) = write(st, &caller.sub, tenant, &caller.session, surface).await {
        metrics::counter!("agent_member_seen_errors_total").increment(1);
        tracing::warn!(error = %e, "member_seen_failed");
    }
}

/// Delete sessions not seen for [`RETENTION_DAYS`] (all tenants, through the definer function).
pub async fn sweep(db: &PgPool) -> ApiResult<i64> {
    Ok(sqlx::query_scalar("SELECT agent_member_session_sweep($1)")
        .bind(RETENTION_DAYS)
        .fetch_one(db)
        .await?)
}

/// [`sweep`] and the invite expiry ([`crate::members::invites::expire_due`], B1b F-01; database only) every hour;
/// never stops.
pub fn spawn_sweep(db: PgPool) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_secs(3600));
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            t.tick().await;
            match sweep(&db).await {
                Ok(n) => tracing::info!(deleted = n, "member_session_sweep"),
                Err(e) => tracing::warn!(error = %e, "member_session_sweep_failed"),
            }
            match crate::members::invites::expire_due(&db).await {
                Ok(n) => tracing::info!(expired = n, "member_invite_expiry_sweep"),
                Err(e) => tracing::warn!(error = %e, "member_invite_expiry_sweep_failed"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn throttle_and_surface() {
        let t = Throttle::default();
        assert!(t.due("a".into()));
        assert!(!t.due("a".into()));
        assert!(t.due("b".into()));
        let mut h = HeaderMap::new();
        assert_eq!(surface(&h), Surface::Admin);
        h.insert(SURFACE_HEADER, HeaderValue::from_static("Chat"));
        assert_eq!(surface(&h), Surface::Chat);
        h.insert(SURFACE_HEADER, HeaderValue::from_static("support"));
        assert_eq!(surface(&h), Surface::Support);
        h.insert(SURFACE_HEADER, HeaderValue::from_static("bogus"));
        assert_eq!(surface(&h), Surface::Admin);
    }
}
