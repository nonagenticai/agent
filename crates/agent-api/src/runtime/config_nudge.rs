//! Config nudges (B4 §0 R7): after a design change COMMITS, agent-api publishes `<tenant>/<job>/<env>` on the root
//! channel `{prefix}config:changed`, and the runtime processes serving that scope drop their cached design (T4).
//! `prod` follows a release apply / rollback / reapply, `test` a draft write that bumped `draft_rev` (`track`,
//! `track_tenant`, `drafts::restore`).
//!
//! A nudge is only a hint: every runtime cache also refreshes every 30 s, so a lost nudge delays a change and never
//! loses it. Hence [`publish`] never fails its caller: a Redis outage is logged and counted, and the design write
//! that triggered it stands. Publish AFTER commit — a nudge for an uncommitted change is a lie.

use futures_util::StreamExt;
use redis::AsyncCommands;
use uuid::Uuid;

use super::bus::Bus;
use super::scope::{Env, Scope};

/// `root_key` → Pub/Sub channel; the payload is the scope text `<tenant>/<job>/<env>`.
pub const CONFIG_CHANNEL: &str = "config:changed";

/// Publish the nudge for (`tenant`, `job` key, `env`). `redis` is `AppState.redis`: `None` (no `REDIS_URL`) is a
/// no-op. Never fails.
pub async fn publish(redis: Option<&Bus>, tenant: Uuid, job: &str, env: Env) {
    let Some(bus) = redis else {
        return;
    };
    let scope = match Scope::new(tenant, job, env) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%tenant, "config nudge skipped: {e:#}");
            return;
        }
    };
    let channel = bus.root_key(CONFIG_CHANNEL);
    let mut c = bus.conn();
    let sent: redis::RedisResult<i64> = c.publish(&channel, scope.to_string()).await;
    if let Err(e) = sent {
        metrics::counter!("agent_config_nudge_failures_total").increment(1);
        tracing::warn!(scope = %scope, error = %e, "config nudge failed; the 30 s refresh covers it");
    }
}

/// The scope a nudge payload names, or `None` for anything malformed.
pub fn parse(payload: &str) -> Option<Scope> {
    Scope::parse(payload).ok()
}

/// Follow the nudges on a dedicated Pub/Sub connection, calling `on` with each valid scope, until the
/// subscription closes (returns `Ok`) or cannot be made (`Err`). Callers loop with a backoff, as the supervisor
/// does.
pub async fn follow(root: &Bus, mut on: impl FnMut(Scope)) -> anyhow::Result<()> {
    let mut ps = root.client().get_async_pubsub().await?;
    ps.subscribe(root.root_key(CONFIG_CHANNEL)).await?;
    let mut messages = ps.into_on_message();
    while let Some(m) = messages.next().await {
        let Ok(payload) = m.get_payload::<String>() else {
            continue;
        };
        match parse(&payload) {
            Some(s) => on(s),
            None => tracing::warn!("ignoring a malformed config nudge"),
        }
    }
    Ok(())
}
