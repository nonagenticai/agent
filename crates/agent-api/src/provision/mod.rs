//! The provisioning engine (B1 X5; spec 06 §4.5–4.6, P4 T3/T4a): what agent still has to tell a downstream
//! (licences, strata_rag, strata_ingest) for a tenant, queued in `provision_outbox` in the same transaction as the
//! change that needs it, and delivered by a fenced drainer ([`outbox`]) through the [`Sender`] registered for the
//! row's op. B1 registers no sender: `store_ensure` / `connections_put` are B3's, `licence_ensure` /
//! `licence_revoke` B7's. The tenant and member licences of D77 are NOT outbox work: they are minted inline after
//! COMMIT (crate::identity) with the signup tick as the backstop.
pub mod outbox;
pub mod reconcile;
pub mod routes;
pub mod senders;

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{self, Actor, Event};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub use outbox::{
    DrainReport, OutboxRow, ROW_COLUMNS, SendResult, Verdict, backoff, classify, drain_once,
    drift_summary, spawn_drainer,
};
pub use reconcile::{ReconcileReport, spawn_reconciler};

/// A downstream a row is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    Licences,
    StrataRag,
    StrataIngest,
}

impl Target {
    pub fn as_str(self) -> &'static str {
        match self {
            Target::Licences => "licences",
            Target::StrataRag => "strata_rag",
            Target::StrataIngest => "strata_ingest",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "licences" => Some(Target::Licences),
            "strata_rag" => Some(Target::StrataRag),
            "strata_ingest" => Some(Target::StrataIngest),
            _ => None,
        }
    }
}

/// A row to queue. `dedupe_key` names the desired state it carries (e.g. `strata_rag:connections_put`): a newer
/// enqueue of the same key replaces a still-pending row's payload.
#[derive(Clone, Debug)]
pub struct NewRow {
    pub target: Target,
    pub op: &'static str,
    pub dedupe_key: String,
    pub payload: Value,
}

/// Delivers the rows of one op. `send` runs after the claim committed, outside any transaction (HTTP timeout below
/// the 120 s lease); `on_success` runs in the finishing transaction (scoped to the row's tenant).
pub trait Sender: Send + Sync {
    fn op(&self) -> &'static str;
    fn send<'a>(&'a self, st: &'a AppState, row: &'a OutboxRow) -> BoxFuture<'a, SendResult>;
    fn on_success<'a>(
        &'a self,
        _tx: &'a mut PgConnection,
        _row: &'a OutboxRow,
        _result: &'a Value,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// op → its sender (`AppState.senders`). The drainer claims only these ops.
#[derive(Default)]
pub struct Senders(HashMap<&'static str, Arc<dyn Sender>>);

impl Senders {
    pub fn register(&mut self, s: Arc<dyn Sender>) {
        self.0.insert(s.op(), s);
    }

    pub fn get(&self, op: &str) -> Option<Arc<dyn Sender>> {
        self.0.get(op).cloned()
    }

    /// The registered ops, sorted.
    pub fn ops(&self) -> Vec<String> {
        let mut ops: Vec<String> = self.0.keys().map(|k| k.to_string()).collect();
        ops.sort();
        ops
    }
}

/// Queue `row` for `tenant` in the caller's scoped transaction: a pending row of the same `dedupe_key` takes the new
/// payload; failed/dead rows of that key are superseded; audited `provision.enqueued`. A payload carrying anything
/// secret-, token- or email-shaped is refused (422 `secret_rejected`): keys travel only in synchronous requests.
pub async fn enqueue(
    tx: &mut PgConnection,
    tenant: Uuid,
    row: NewRow,
    actor: &Actor,
) -> ApiResult<Uuid> {
    if let Err(field) = audit::scrub::check(&row.payload) {
        return Err(ApiError::Unprocessable(
            "secret_rejected",
            format!(
                "the provisioning payload field {field:?} looks like a secret; it is never queued"
            ),
        ));
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO provision_outbox (tenant_id, target, op, dedupe_key, payload, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (tenant_id, dedupe_key) WHERE status = 'pending' \
         DO UPDATE SET payload = EXCLUDED.payload, created_by = EXCLUDED.created_by, updated_at = now() \
         RETURNING id",
    )
    .bind(tenant)
    .bind(row.target.as_str())
    .bind(row.op)
    .bind(&row.dedupe_key)
    .bind(&row.payload)
    .bind(&actor.id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE provision_outbox SET status = 'superseded', completed_at = now(), updated_at = now() \
          WHERE tenant_id = $1 AND dedupe_key = $2 AND status IN ('failed', 'dead')",
    )
    .bind(tenant)
    .bind(&row.dedupe_key)
    .execute(&mut *tx)
    .await?;
    audit::record(
        tx,
        actor,
        Event::new("provision.enqueued", "provision_outbox", id.to_string())
            .detail("op", row.op)
            .detail("target", row.target.as_str())
            .detail("dedupe_key", row.dedupe_key),
    )
    .await?;
    Ok(id)
}

/// 06 §4.6 / areas/tenant.ts `requireProvisioned`, in a transaction scoped to the tenant: `target` is usable once it
/// last succeeded (`provision_state.last_success_at`) or when nothing was ever queued for it; while its latest row is
/// pending/in flight → 409 `provision_pending`; when it failed or died → 503 `provision_failed` naming the target.
pub async fn require_provisioned(conn: &mut PgConnection, target: Target) -> ApiResult<()> {
    let ok: Option<bool> = sqlx::query_scalar(
        "SELECT last_success_at IS NOT NULL FROM provision_state WHERE target = $1",
    )
    .bind(target.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    if ok == Some(true) {
        return Ok(());
    }
    let latest: Option<String> = sqlx::query_scalar(
        "SELECT status::text FROM provision_outbox \
          WHERE target = $1 AND status NOT IN ('superseded', 'cancelled') \
          ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(target.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    match latest.as_deref() {
        Some("pending" | "in_flight") => Err(ApiError::Conflict(
            "provision_pending",
            format!(
                "{} is still being provisioned for this tenant",
                target.as_str()
            ),
        )),
        Some("failed" | "dead") => Err(ApiError::detailed(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "provision_failed",
            format!("provisioning {} failed for this tenant", target.as_str()),
        )
        .with("target", target.as_str())),
        _ => Ok(()),
    }
}

/// Tenant creation hook (inside the create transaction). B1 queues nothing; B3/B7 add their rows here.
pub async fn on_tenant_created(
    _tx: &mut PgConnection,
    _tenant: Uuid,
    _actor: &Actor,
) -> ApiResult<Vec<Uuid>> {
    Ok(Vec::new())
}

/// Offboarding hook (inside the archive transaction): pending data-plane writes (`connections_put`,
/// `store_ensure`, `store_delete`) are cancelled and audited `provision.cancelled`. B7 adds its `licence_revoke` rows.
pub async fn on_tenant_offboarded(
    tx: &mut PgConnection,
    tenant: Uuid,
    actor: &Actor,
) -> ApiResult<Vec<Uuid>> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE provision_outbox SET status = 'cancelled', completed_at = now(), updated_at = now() \
          WHERE tenant_id = $1 AND status = 'pending' AND op IN ('connections_put', 'store_ensure', 'store_delete') \
         RETURNING id",
    )
    .bind(tenant)
    .fetch_all(&mut *tx)
    .await?;
    for id in &ids {
        audit::record(
            tx,
            actor,
            Event::new("provision.cancelled", "provision_outbox", id.to_string())
                .detail("reason", "tenant_offboarded"),
        )
        .await?;
    }
    Ok(ids)
}
