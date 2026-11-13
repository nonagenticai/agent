//! The inbox sweeper (B4 T9): every 5 s, per registered scope, drain the conversations whose inbox holds message ids
//! and whose lock is gone (the drain's process died, or its lock expired mid-turn). It only re-drains text a customer
//! already sent: no timer starts a model call of its own (plan Global constraints).
use std::time::Duration;

use anyhow::Result;
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::drain::{self, TurnCtx};
use crate::runtime::{Bus, Scope, supervisor};
use crate::state::AppState;

/// How often a scope is swept.
pub const SWEEP_EVERY: Duration = Duration::from_secs(5);
/// At most this many conversations per scope per sweep.
pub const SWEEP_LIMIT: usize = 50;

/// Start the sweeper (one loop per registered scope, crate::runtime::supervisor); `None` while the runtime is dark.
pub fn spawn(st: AppState) -> Option<JoinHandle<()>> {
    let root = st.redis.clone()?;
    Some(tokio::spawn(async move {
        let r = supervisor::run_per_scope(root, "inbox-sweeper", move |bus, scope| {
            sweep_scope(st.clone(), bus, scope)
        })
        .await;
        if let Err(e) = r {
            tracing::error!(error = %format!("{e:#}"), "the inbox sweeper stopped");
        }
    }))
}

async fn sweep_scope(st: AppState, bus: Bus, scope: Scope) -> Result<()> {
    let mut tick = tokio::time::interval(SWEEP_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        match sweep(&st, &bus, &scope, SWEEP_LIMIT).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(scope = %scope, conversations = n, "drained stranded inboxes"),
            Err(e) => {
                tracing::warn!(scope = %scope, error = %format!("{e:#}"), "inbox sweep failed")
            }
        }
    }
}

/// Drain `scope`'s stranded inboxes (at most `limit`); returns how many conversations were drained.
pub async fn sweep(st: &AppState, bus: &Bus, scope: &Scope, limit: usize) -> Result<usize> {
    let stranded = stranded(bus, limit).await?;
    if stranded.is_empty() {
        return Ok(0);
    }
    let Some(job_id) = job_id_of(st, scope).await? else {
        return Ok(0);
    };
    let mut n = 0;
    for id in stranded {
        let token = Uuid::new_v4().to_string();
        // a sender may have taken the lock since the scan: it drains, this skips
        if super::acquire(bus, &id, &token).await? {
            let ctx = TurnCtx {
                tenant: scope.tenant,
                job_id,
                job_key: scope.job.clone(),
                env: scope.env,
                conversation_id: id,
            };
            drain::run(st.clone(), ctx, token).await;
            n += 1;
        }
    }
    Ok(n)
}

/// The scope's job id (`None`: the job is gone).
async fn job_id_of(st: &AppState, scope: &Scope) -> Result<Option<Uuid>> {
    let mut tx = crate::tenancy::begin_scoped(&st.db, scope.tenant).await?;
    let id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM jobs WHERE key = $1")
        .bind(&scope.job)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(id)
}

/// Conversations of the bus's scope with queued ids and no lock holder, at most `limit`: SCAN `chat:*:inbox` under the
/// scope prefix (glob characters escaped; a prod scope never matches the test scope's `test:` keys).
pub async fn stranded(bus: &Bus, limit: usize) -> Result<Vec<String>> {
    let head = bus.streams.key("chat:");
    let escaped: String = head
        .chars()
        .flat_map(|c| match c {
            '*' | '?' | '[' | ']' | '\\' => vec!['\\', c],
            c => vec![c],
        })
        .collect();
    let pattern = format!("{escaped}*:inbox");
    let mut c = bus.conn();
    let mut cursor: u64 = 0;
    let mut out: Vec<String> = Vec::new();
    loop {
        let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(200)
            .query_async(&mut c)
            .await?;
        for k in keys {
            let Some(id) = k
                .strip_prefix(head.as_str())
                .and_then(|r| r.strip_suffix(":inbox"))
            else {
                continue;
            };
            let held: bool = redis::cmd("EXISTS")
                .arg(super::chat_key(bus, id, ":lock"))
                .query_async(&mut c)
                .await?;
            if !held && out.len() < limit && !out.iter().any(|o| o == id) {
                out.push(id.to_string());
            }
        }
        cursor = next;
        if cursor == 0 || out.len() >= limit {
            return Ok(out);
        }
    }
}
