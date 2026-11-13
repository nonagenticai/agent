//! The metering outbox drainer (port of care-bus-agents@96c608c `metering.rs` `drain_once` / `spawn_drainer`).
//!
//! One loop inside agent-api serves every tenant: [`drain_once`] claims due rows across tenants through
//! `agent_claim_metering` (0129: `FOR UPDATE SKIP LOCKED` plus a 120 s lease, committed before any send), XADDs each
//! to the billing cost stream, then settles it under the row's own tenant GUC (sent, or backed off 15 s doubling to
//! 30 min). No transaction is held across Redis. Two drainers never send the same row; a crash between XADD and the
//! settle re-sends it after the lease, which billing dedups on `event_key` (at least once; billing-service
//! `cost_flusher.py` claims each `event_key` into `processed_cost_event_keys` with `ON CONFLICT DO NOTHING`).
//!
//! The lease is the only thing that keeps a second drainer off a claimed row, so a drainer never starts a send it
//! cannot settle inside the lease: every XADD is bounded by [`XADD_TIMEOUT`], and once [`SEND_BUDGET`] (half the
//! [`LEASE`]) has passed since the claim the rest of the batch is left unsent (`deferred`) for the next claim after
//! the lease — never sent late. The settle is idempotent (`status = 'pending'` guard) and its rowcount is checked: a
//! settle that touches no row means another drainer settled it first (the lease was lost), which is logged and
//! counted (`agent_metering_settle_lost_total`), never assumed away.
//!
//! [`spawn`] starts nothing while `METERING_EMIT_ENABLED` is off (K52: off).
use std::time::Duration;

use anyhow::Context;
use redis::aio::ConnectionManager;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ApiResult;
use crate::state::AppState;
use crate::tenancy::set_tenant;

/// Rows claimed per [`drain_once`]. Small enough that a healthy batch finishes far inside [`SEND_BUDGET`]; a slow one
/// is cut by the budget, not by the lease.
pub const BATCH: i32 = 100;
/// `agent_claim_metering`'s lease (0129: `next_attempt_at = now() + 120 s`).
pub const LEASE: Duration = Duration::from_secs(120);
/// No send starts later than this after the claim began: half the lease, leaving the rest for the in-flight XADD
/// ([`XADD_TIMEOUT`]), its settle, and clock skew between this process and Postgres.
pub const SEND_BUDGET: Duration = Duration::from_secs(60);
/// One XADD may take at most this long; a timeout backs the row off like any send error (billing dedups the event
/// should the XADD have landed after all).
pub const XADD_TIMEOUT: Duration = Duration::from_secs(5);
const BACKOFF_BASE_SECS: f64 = 15.0;
const BACKOFF_MAX_SECS: f64 = 30.0 * 60.0;
/// MAXLEN for the costs stream.
const STREAM_MAXLEN: usize = 100_000;

/// 15 s doubling per failed attempt, capped at 30 min.
pub fn backoff_secs(attempts: i32) -> f64 {
    let exp = attempts.saturating_sub(1).clamp(0, 30);
    (BACKOFF_BASE_SECS * 2f64.powi(exp)).min(BACKOFF_MAX_SECS)
}

/// What one [`drain_once`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Drained {
    pub claimed: usize,
    pub sent: usize,
    pub failed: usize,
    /// Claimed but not sent: the [`SEND_BUDGET`] ran out. Due again after the lease; never sent late.
    pub deferred: usize,
    /// Settles that touched no row (another drainer settled it first: the lease was lost).
    pub settle_lost: usize,
}

/// The time bounds of one [`drain_once_with`]. [`Limits::default`] is production's.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub batch: i32,
    pub send_budget: Duration,
    pub xadd_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            batch: BATCH,
            send_budget: SEND_BUDGET,
            xadd_timeout: XADD_TIMEOUT,
        }
    }
}

#[derive(sqlx::FromRow)]
struct Claimed {
    id: i64,
    tenant_id: Uuid,
    fields: Value,
    attempts: i32,
}

/// XADD one event. Every field value must be a string (as `usage::emit` writes them); anything else is refused, never
/// sent half-empty.
async fn xadd(conn: &mut ConnectionManager, stream: &str, fields: &Value) -> anyhow::Result<()> {
    let obj = fields
        .as_object()
        .context("outbox fields are not an object")?;
    let mut cmd = redis::cmd("XADD");
    cmd.arg(stream)
        .arg("MAXLEN")
        .arg("~")
        .arg(STREAM_MAXLEN)
        .arg("*");
    for (k, v) in obj {
        let v = v
            .as_str()
            .with_context(|| format!("outbox field {k} is not a string"))?;
        cmd.arg(k.as_str()).arg(v);
    }
    let _id: String = cmd.query_async(conn).await?;
    Ok(())
}

/// A short, secret-free reason for `last_error` (a Redis error kind, never a URL).
fn reason(e: &anyhow::Error) -> String {
    let r = match e.downcast_ref::<redis::RedisError>() {
        Some(re) => format!("redis_error: {:?}", re.kind()),
        None => format!("send_refused: {e}"),
    };
    r.chars().take(200).collect()
}

/// Settle one sent row under its tenant's GUC. Idempotent: only a still-pending row changes. Returns whether a row
/// changed — `false` means it was already settled (by another drainer after this one's lease ran out).
pub async fn settle_sent(db: &PgPool, tenant: Uuid, id: i64) -> ApiResult<bool> {
    let mut tx = db.begin().await?;
    set_tenant(&mut tx, tenant).await?;
    let n = sqlx::query(
        "UPDATE metering_outbox SET status = 'sent', sent_at = now(), last_error = NULL \
          WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(n == 1)
}

fn note_settle_lost(out: &mut Drained, tenant: Uuid, id: i64, what: &'static str) {
    out.settle_lost += 1;
    metrics::counter!("agent_metering_settle_lost_total", "settle" => what).increment(1);
    tracing::warn!(
        tenant = %tenant, outbox_id = id, settle = what,
        "metering_settle_lost (row already settled: the lease ran out and another drainer took it)"
    );
}

/// [`drain_once_with`] under production's [`Limits`].
pub async fn drain_once(
    db: &PgPool,
    conn: &mut ConnectionManager,
    stream: &str,
) -> ApiResult<Drained> {
    drain_once_with(db, conn, stream, Limits::default()).await
}

/// Claim up to `limits.batch` due rows of every tenant, send each to `stream`, settle each. A database error is
/// returned; a send error (or timeout) backs that row off and the rest go on. Once `limits.send_budget` has passed
/// since the claim began, the remaining rows are left to the lease (`deferred`), never sent.
pub async fn drain_once_with(
    db: &PgPool,
    conn: &mut ConnectionManager,
    stream: &str,
    limits: Limits,
) -> ApiResult<Drained> {
    // measured from before the claim: the lease starts no earlier than this, so the budget is conservative
    let started = tokio::time::Instant::now();
    let claimed: Vec<Claimed> =
        sqlx::query_as("SELECT id, tenant_id, fields, attempts FROM agent_claim_metering($1)")
            .bind(limits.batch)
            .fetch_all(db)
            .await?;
    let mut out = Drained {
        claimed: claimed.len(),
        ..Drained::default()
    };
    for row in claimed {
        if started.elapsed() >= limits.send_budget {
            out.deferred = out.claimed - out.sent - out.failed;
            tracing::warn!(
                deferred = out.deferred,
                budget_ms = limits.send_budget.as_millis() as u64,
                "metering_send_budget_exhausted (the rest of the batch waits for the lease)"
            );
            break;
        }
        let sent = match tokio::time::timeout(limits.xadd_timeout, xadd(conn, stream, &row.fields))
            .await
        {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!("xadd timed out")),
        };
        match sent {
            Ok(()) => {
                if !settle_sent(db, row.tenant_id, row.id).await? {
                    note_settle_lost(&mut out, row.tenant_id, row.id, "sent");
                }
                out.sent += 1;
            }
            Err(e) => {
                let attempts = row.attempts.saturating_add(1);
                let why = reason(&e);
                let mut tx = db.begin().await?;
                set_tenant(&mut tx, row.tenant_id).await?;
                tracing::warn!(
                    tenant = %row.tenant_id, outbox_id = row.id, attempts, reason = %why,
                    "metering_send_failed"
                );
                let n = sqlx::query(
                    "UPDATE metering_outbox \
                        SET attempts = $2, last_error = $3, next_attempt_at = now() + make_interval(secs => $4) \
                      WHERE id = $1 AND status = 'pending'",
                )
                .bind(row.id)
                .bind(attempts)
                .bind(why)
                .bind(backoff_secs(attempts))
                .execute(&mut *tx)
                .await?
                .rows_affected();
                tx.commit().await?;
                if n != 1 {
                    note_settle_lost(&mut out, row.tenant_id, row.id, "backoff");
                }
                out.failed += 1;
            }
        }
    }
    if out.settle_lost > 0 {
        tracing::warn!(
            ?out,
            "metering_settles_short (fewer rows settled than were sent or backed off)"
        );
    }
    metrics::counter!("agent_metering_sent_total").increment(out.sent as u64);
    metrics::counter!("agent_metering_send_failed_total").increment(out.failed as u64);
    metrics::counter!("agent_metering_deferred_total").increment(out.deferred as u64);
    Ok(out)
}

/// The drainer, every `METERING_OUTBOX_INTERVAL_MS`, sending to `METERING_REDIS_URL`. `None` while
/// `METERING_EMIT_ENABLED` is off: nothing is spawned and no connection is opened.
pub fn spawn(st: AppState) -> Option<tokio::task::JoinHandle<()>> {
    let m = st.cfg.metering.clone();
    if !m.emit_enabled {
        tracing::info!("metering_drainer_disabled (METERING_EMIT_ENABLED is not true)");
        return None;
    }
    Some(tokio::spawn(async move {
        let mut conn = loop {
            let c = match redis::Client::open(m.redis_url.as_str()) {
                Ok(client) => ConnectionManager::new(client).await,
                Err(e) => Err(e),
            };
            match c {
                Ok(c) => break c,
                Err(e) => {
                    tracing::error!(kind = ?e.kind(), "metering_redis_unreachable; retrying in 30 s");
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        };
        let mut tick = tokio::time::interval(m.outbox_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            // a full batch: drain again at once
            loop {
                match drain_once(&st.db, &mut conn, &m.costs_stream).await {
                    Ok(d) => {
                        if d.sent > 0 || d.failed > 0 {
                            tracing::info!(sent = d.sent, failed = d.failed, "metering_drained");
                        }
                        if d.claimed < usize::try_from(BATCH).unwrap_or(100)
                            || d.failed > 0
                            || d.deferred > 0
                        {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "metering_drain_failed");
                        break;
                    }
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_15_s_and_caps_at_30_min() {
        assert_eq!(backoff_secs(1), 15.0);
        assert_eq!(backoff_secs(2), 30.0);
        assert_eq!(backoff_secs(3), 60.0);
        assert_eq!(backoff_secs(20), 1800.0);
        assert_eq!(backoff_secs(i32::MAX), 1800.0);
    }
}
