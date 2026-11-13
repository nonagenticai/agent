//! Leased claims for the B8 background work (migration 0190): a validation grid run or a discovery import is run by
//! the one process that holds its lease, so agent-api can run more than one replica.
//!
//! - [`owner`] is this process's id (one per process start). A restarted pod gets a new one, so the lease its
//!   predecessor held lapses by itself ([`TTL_SECS`]) and is then taken by a sweep.
//! - The holder claims before it works, renews after every chunk / batch and releases when it ends.
//! - [`spawn_sweep`] re-runs both boot resumes every [`SWEEP_EVERY`]: they list only rows without a live lease, and
//!   still never create work.
use std::sync::OnceLock;
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::state::AppState;

/// How long a claim or renewal holds a row.
pub const TTL_SECS: i32 = 600;

/// Period of the resume sweep.
pub const SWEEP_EVERY: Duration = Duration::from_secs(120);

/// This process's lease owner id: `<HOSTNAME or "api">:<uuid>`, fixed for the process lifetime.
pub fn owner() -> &'static str {
    static OWNER: OnceLock<String> = OnceLock::new();
    OWNER.get_or_init(|| {
        let host = std::env::var("HOSTNAME")
            .ok()
            .filter(|h| !h.trim().is_empty())
            .unwrap_or_else(|| "api".to_string());
        format!("{host}:{}", uuid::Uuid::new_v4())
    })
}

/// Every [`SWEEP_EVERY`]: resume the grid runs and discovery imports no live replica holds (the boot resume already
/// ran, so the first tick waits a full period). Logs only when something was taken.
pub fn spawn_sweep(st: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let start = tokio::time::Instant::now() + SWEEP_EVERY;
        let mut tick = tokio::time::interval_at(start, SWEEP_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match crate::validation::grid::resume(&st).await {
                Ok(n) if n > 0 => tracing::info!(runs = n, "validation_grid_sweep"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = ?e, "validation_grid_sweep_failed"),
            }
            match crate::discovery::worker::resume(
                &st.db,
                crate::discovery::worker::classifier(&st),
            )
            .await
            {
                // debug: a parked import (the tenant has no `gates` provider yet) is re-taken by every sweep
                Ok(h) if !h.is_empty() => {
                    tracing::debug!(imports = h.len(), "discovery_import_sweep");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = ?e, "discovery_import_sweep_failed"),
            }
        }
    })
}
