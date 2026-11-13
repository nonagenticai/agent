//! B8 discovery (plan 2026-10-20-b8-journeys-generator-discovery, J18; migrations 0173 + 0178): tenant ticket/chat
//! exports classified against the job's journey, the scorecard weight, and shadow mode. Built so far (§ Revision
//! 2026-10-22, "start now" bucket): T14a (upload, parse, scrub, dedupe, worker + boot resume), T15a (F-51 weight,
//! F-52 preview / columns / XLSX / delete), the shadow half of T15 (start / stop / `active` / `observe` / purge) and
//! T14b ([`classify`]: Laya classification over the job's effective activities, uncovered rows clustered by one LLM
//! call, both metered as source `discovery`). Signals, proposals and accept / reject (T15, [`proposals`]) read the job's journey (T5) and write through the J4 / J6 / J10 draft targets.
//!
//! Routes: crate::routes::discovery. Boot: [`boot`] (resume classifying imports, purge old observations).
pub mod classify;
pub mod import;
pub mod proposals;
pub mod rows;
pub mod scrub;
pub mod shadow;
pub mod state;
pub mod worker;

use crate::state::AppState;

/// API boot: purge shadow observations older than 30 days and resume every import still `classifying`. Never
/// creates work (no new import; a resumed import only finishes the upload that started it). Errors are logged, not
/// fatal.
pub async fn boot(st: &AppState) {
    let db = &st.db;
    match shadow::purge_all(db).await {
        Ok(n) => tracing::info!(deleted = n, "discovery_observation_purge"),
        Err(e) => tracing::warn!(error = %e, "discovery_observation_purge_failed"),
    }
    match worker::resume(db, worker::classifier(st)).await {
        Ok(h) => tracing::info!(imports = h.len(), "discovery_import_resume"),
        Err(e) => tracing::warn!(error = %e, "discovery_import_resume_failed"),
    }
}
