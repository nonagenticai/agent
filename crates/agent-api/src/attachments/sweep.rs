//! The sweep of attachments nobody will read again (SW-9 lifecycle, 0223 `agent_conversation_attachment_sweep`): an
//! upload still unlinked after one hour and a soft-deleted row older than one hour. A linked attachment stays with its
//! conversation. Every tenant, through the definer function; same shape as the web chat counter sweep.
//!
//! Then the tombstone worker: every deleted attachment row (this sweep, a conversation cascade, a staff
//! delete) left a `conversation_attachment_tombstone` through the 0223 trigger. Each tick deletes the due tombstones'
//! artifacts at artifact_service on the owning tenant's licence (404 = done), drops the tombstone on success and keeps
//! it with `attempts` + `last_error_at` on failure (back-off 30 s * 2^attempts, at most 1 h; the due list and the
//! updates are definer functions, the delete runs under the tenant's own scoped transaction for its licence). Metrics:
//! `agent_attachment_tombstones_total{outcome=deleted|failed}` and the gauge `agent_attachment_tombstones_pending`.
use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

use super::artifacts::ArtifactsClient;
use crate::state::AppState;

/// How often the sweep runs.
pub const SWEEP_EVERY: Duration = Duration::from_secs(600);
/// The most tombstones one tick works.
pub const TOMBSTONE_BATCH: i32 = 50;
/// How often the tombstone worker runs. Short: a deleted conversation's files should not linger.
pub const TOMBSTONE_EVERY: Duration = Duration::from_secs(60);

/// Delete the swept rows: how many.
pub async fn sweep(db: &PgPool) -> Result<i32, sqlx::Error> {
    sqlx::query_scalar("SELECT agent_conversation_attachment_sweep()")
        .fetch_one(db)
        .await
}

/// What one tombstone pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tombstones {
    pub deleted: u32,
    pub failed: u32,
}

/// Work the due tombstones: delete each artifact at artifact_service and settle its tombstone.
pub async fn process_tombstones(st: &AppState) -> Result<Tombstones, sqlx::Error> {
    let due: Vec<(Uuid, Uuid)> =
        sqlx::query_as("SELECT tenant_id, artifact_id FROM agent_attachment_tombstones_due($1)")
            .bind(TOMBSTONE_BATCH)
            .fetch_all(&st.db)
            .await?;
    let client = ArtifactsClient::for_state(st);
    let mut out = Tombstones::default();
    for (tenant, artifact) in due {
        if client.delete_for(tenant, artifact).await.is_ok() {
            sqlx::query("SELECT agent_attachment_tombstone_done($1, $2)")
                .bind(tenant)
                .bind(artifact)
                .execute(&st.db)
                .await?;
            out.deleted += 1;
            metrics::counter!("agent_attachment_tombstones_total", "outcome" => "deleted")
                .increment(1);
        } else {
            sqlx::query("SELECT agent_attachment_tombstone_failed($1, $2)")
                .bind(tenant)
                .bind(artifact)
                .execute(&st.db)
                .await?;
            out.failed += 1;
            metrics::counter!("agent_attachment_tombstones_total", "outcome" => "failed")
                .increment(1);
        }
    }
    let pending: i64 = sqlx::query_scalar("SELECT agent_attachment_tombstones_pending()")
        .fetch_one(&st.db)
        .await?;
    metrics::gauge!("agent_attachment_tombstones_pending").set(pending as f64);
    Ok(out)
}

/// What one upload-intent pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Intents {
    /// Intents settled: their artifacts (if any) deleted, or closed meanwhile by the upload itself.
    pub resolved: u32,
    pub failed: u32,
}

/// Resolve the upload intents older than 15 minutes (0223): an upload whose process died before its row was inserted.
/// Each is claimed (so a late upload cannot keep a row naming the bytes), its artifacts are found by the tag
/// `att:<attachment id>` at artifact_service and deleted (none found = done), and the intent is dropped. A failure
/// keeps the intent with `attempts` + `last_error_at` (same back-off as the tombstones).
pub async fn process_intents(st: &AppState) -> Result<Intents, sqlx::Error> {
    let due: Vec<(Uuid, Uuid)> =
        sqlx::query_as("SELECT tenant_id, attachment_id FROM agent_attachment_intents_due($1)")
            .bind(TOMBSTONE_BATCH)
            .fetch_all(&st.db)
            .await?;
    let client = ArtifactsClient::for_state(st);
    let mut out = Intents::default();
    for (tenant, att) in due {
        let claimed: bool = sqlx::query_scalar("SELECT agent_attachment_intent_claim($1, $2)")
            .bind(tenant)
            .bind(att)
            .fetch_one(&st.db)
            .await?;
        if !claimed {
            // the upload finished and closed its intent between the list and the claim
            continue;
        }
        if resolve_intent(&client, tenant, att).await {
            sqlx::query("SELECT agent_attachment_intent_done($1, $2)")
                .bind(tenant)
                .bind(att)
                .execute(&st.db)
                .await?;
            out.resolved += 1;
            metrics::counter!("agent_attachment_intents_total", "outcome" => "resolved")
                .increment(1);
        } else {
            sqlx::query("SELECT agent_attachment_intent_failed($1, $2)")
                .bind(tenant)
                .bind(att)
                .execute(&st.db)
                .await?;
            out.failed += 1;
            metrics::counter!("agent_attachment_intents_total", "outcome" => "failed").increment(1);
        }
    }
    let pending: i64 = sqlx::query_scalar("SELECT agent_attachment_intents_pending()")
        .fetch_one(&st.db)
        .await?;
    metrics::gauge!("agent_attachment_intents_pending").set(pending as f64);
    Ok(out)
}

/// Delete every artifact tagged for attachment `att` of `tenant`; `false` when anything failed (no licence included).
async fn resolve_intent(client: &ArtifactsClient, tenant: Uuid, att: Uuid) -> bool {
    let lic = match client.licence_of(tenant).await {
        Ok(Some(l)) => l,
        Ok(None) => {
            tracing::warn!(%tenant, attachment = %att, "attachment_intent_without_licence");
            return false;
        }
        Err(_) => return false,
    };
    let Ok(found) = client.find_by_attachment(&lic, att).await else {
        return false;
    };
    for artifact in found {
        if client.delete(&lic, artifact).await.is_err() {
            return false;
        }
    }
    true
}

/// The sweep, then the tombstones it (and every other deletion) left.
pub async fn tick(st: &AppState) -> Result<(i32, Tombstones), sqlx::Error> {
    let swept = sweep(&st.db).await?;
    let t = process_tombstones(st).await?;
    Ok((swept, t))
}

/// The 10-minute sweep; never stops.
pub fn spawn_sweep(st: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut t = tokio::time::interval(SWEEP_EVERY);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            t.tick().await;
            match sweep(&st.db).await {
                Ok(n) => tracing::info!(deleted = n, "conversation_attachment_sweep"),
                Err(e) => tracing::warn!(error = %e, "conversation_attachment_sweep_failed"),
            }
        }
    })
}

/// The tombstone worker (every minute); never stops.
pub fn spawn_tombstones(st: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut t = tokio::time::interval(TOMBSTONE_EVERY);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            t.tick().await;
            match process_tombstones(&st).await {
                Ok(o) if o.deleted + o.failed > 0 => {
                    tracing::info!(
                        deleted = o.deleted,
                        failed = o.failed,
                        "attachment_tombstones"
                    )
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "attachment_tombstones_failed"),
            }
            match process_intents(&st).await {
                Ok(o) if o.resolved + o.failed > 0 => {
                    tracing::info!(
                        resolved = o.resolved,
                        failed = o.failed,
                        "attachment_upload_intents"
                    )
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "attachment_upload_intents_failed"),
            }
        }
    })
}
