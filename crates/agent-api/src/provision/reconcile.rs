//! The hourly provisioning reconciler (P4 T8, spec 06 §4.6; interim for W5 A2 versioned connections, G2): per active
//! customer tenant and strata target, read what strata holds (`GET /api/tenant/providers`,
//! crate::providers::slots::fetch_remote), record it as the slot observation (`provider_slot_remote`, the same write
//! the slot list does) and set `provision_state.drift` to the slots that moved outside this app, or NULL when every
//! slot matches what agent pushed. It NEVER writes to strata: a drift is shown (Integrations panel, slot views) and
//! adopted or overwritten only by a person (06 §4.6, "never overwritten").
//!
//! - A slot version here is the B3 fingerprint ([`slots::remote_fpr`]): `ours` = what agent last pushed, `remote` =
//!   what strata reports now; `local_version` = `pushed_version`, `remote_version` = `pushed_version + 1` (synthetic
//!   until A2 carries real versions, the same rule as the slot view's drift). `detected_at` = when strata's
//!   fingerprint last changed.
//! - Skipped: suspended and archived tenants (`agent_list_tenants()` status), platform-owned tenants (their slots
//!   are the platform's, D70), and any target strata did not answer for (no URL, no token pair, unreachable): its
//!   drift is left as it was.
//! - Not done here (retired by later decisions): the licence re-ensure of the P4 plan (D70 retired the bundle
//!   minter; D77 mints and revokes tenant/member licences inline with the signup tick's licence reconciler as the
//!   backstop, so no `licence_ensure` op or sender exists) and the A1 manifest-cache refresh (no A1 client exists).
//! - Runs only in `agent-api` (spawned from its `main`, never from `agent-edge` or the agents, 06 §4.1): hourly after
//!   a startup jitter of at most 10 minutes, at most 2 tenants per second.
use std::time::Duration;

use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::ApiResult;
use crate::platform_db::begin_platform;
use crate::providers::kinds::Slot;
use crate::providers::{llm, slots};
use crate::state::AppState;
use crate::strata::Service;
use crate::tenancy::begin_scoped;

/// One pass every hour.
pub const EVERY: Duration = Duration::from_secs(3600);
/// The first pass waits a random part of this (replicas do not hit strata together).
pub const MAX_JITTER: Duration = Duration::from_secs(600);
/// The gap between two tenants (at most 2 tenants per second).
pub const PACE: Duration = Duration::from_millis(500);

/// What one [`run_once`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Active customer tenants reconciled.
    pub tenants: usize,
    /// Targets whose drift is set after this pass.
    pub drift_found: usize,
    /// Targets whose drift was cleared by this pass.
    pub drift_cleared: usize,
    /// Targets skipped because strata did not answer (drift left as it was).
    pub unreachable: usize,
    /// Tenants whose local write failed (logged; retried next hour).
    pub failed: usize,
}

/// The `provision_state.drift` entries of `service`'s slots (web `ProvisionDrift[]`), from the recorded observations.
async fn drift_of(c: &mut PgConnection, tenant: Uuid, service: Service) -> ApiResult<Vec<Value>> {
    let mut out = Vec::new();
    for slot in Slot::of(service) {
        let Some(r) = slots::remote(&mut *c, tenant, slot).await? else {
            continue;
        };
        if r.drifted() {
            out.push(json!({
                "slot": slot.as_str(),
                "local_version": r.pushed_version,
                "remote_version": r.pushed_version + 1,
                "detected_at": r.observed_at,
                "ours": r.pushed_fpr,
                "remote": r.observed_fpr,
            }));
        }
    }
    Ok(out)
}

/// Record strata's view of `service` for `tenant` and set or clear the target's drift. `Some(true)` = drift is set,
/// `Some(false)` = it was cleared, `None` = nothing changed.
async fn reconcile_target(
    c: &mut PgConnection,
    tenant: Uuid,
    service: Service,
    view: &Value,
) -> ApiResult<Option<bool>> {
    for slot in Slot::of(service) {
        slots::record_observed(&mut *c, tenant, slot, view.get(slot.as_str())).await?;
    }
    let drift = drift_of(&mut *c, tenant, service).await?;
    let target = service.target().as_str();
    if drift.is_empty() {
        let cleared = sqlx::query(
            "UPDATE provision_state SET drift = NULL, updated_at = now() \
              WHERE tenant_id = $1 AND target = $2 AND drift IS NOT NULL",
        )
        .bind(tenant)
        .bind(target)
        .execute(&mut *c)
        .await?
        .rows_affected();
        return Ok((cleared > 0).then_some(false));
    }
    sqlx::query(
        "INSERT INTO provision_state (tenant_id, target, drift) VALUES ($1, $2, $3) \
         ON CONFLICT (tenant_id, target) DO UPDATE SET drift = EXCLUDED.drift, updated_at = now() \
          WHERE provision_state.drift IS DISTINCT FROM EXCLUDED.drift",
    )
    .bind(tenant)
    .bind(target)
    .bind(Value::Array(drift))
    .execute(&mut *c)
    .await?;
    Ok(Some(true))
}

/// One tenant: both reads first (no transaction open across HTTP), then one scoped transaction for the local writes.
async fn reconcile_tenant(
    st: &AppState,
    tenant: Uuid,
    report: &mut ReconcileReport,
) -> ApiResult<bool> {
    {
        let mut tx = begin_scoped(&st.db, tenant).await?;
        let owned = llm::platform_owned(&mut tx, st, tenant).await?;
        tx.commit().await?;
        if owned {
            return Ok(false);
        }
    }
    let mut views = Vec::new();
    for service in [Service::Rag, Service::Ingest] {
        match slots::fetch_remote(st, tenant, service).await {
            Some(v) => views.push((service, v)),
            None => report.unreachable += 1,
        }
    }
    let mut found = 0;
    let mut cleared = 0;
    if !views.is_empty() {
        let mut tx = begin_scoped(&st.db, tenant).await?;
        for (service, view) in &views {
            match reconcile_target(&mut tx, tenant, *service, view).await? {
                Some(true) => found += 1,
                Some(false) => cleared += 1,
                None => {}
            }
        }
        tx.commit().await?;
    }
    report.drift_found += found;
    report.drift_cleared += cleared;
    Ok(true)
}

/// The active tenants (`agent_list_tenants()` is granted to `agent_platform` alone, 0031).
async fn active_tenants(st: &AppState) -> ApiResult<Vec<Uuid>> {
    let mut tx = begin_platform(&st.db).await?;
    let ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM agent_list_tenants() WHERE status = 'active'")
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(ids)
}

/// One pass over every active customer tenant, [`PACE`] apart. Never writes to strata.
pub async fn run_once(st: &AppState) -> ApiResult<ReconcileReport> {
    let mut report = ReconcileReport::default();
    for (i, tenant) in active_tenants(st).await?.into_iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(PACE).await;
        }
        match reconcile_tenant(st, tenant, &mut report).await {
            Ok(true) => report.tenants += 1,
            Ok(false) => {}
            Err(e) => {
                report.failed += 1;
                tracing::warn!(%tenant, error = %e, "provision_reconcile_tenant_failed");
            }
        }
    }
    Ok(report)
}

/// [`run_once`] every [`EVERY`], the first after a random wait of at most [`MAX_JITTER`]; never stops.
pub fn spawn_reconciler(st: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let jitter_ms = Uuid::new_v4().as_u64_pair().0 % (MAX_JITTER.as_millis() as u64 + 1);
        tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
        let mut t = tokio::time::interval(EVERY);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            t.tick().await;
            match run_once(&st).await {
                Ok(r) => tracing::info!(report = ?r, "provision_reconcile"),
                Err(e) => tracing::warn!(error = %e, "provision_reconcile_failed"),
            }
        }
    })
}
