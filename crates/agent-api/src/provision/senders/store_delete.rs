//! The `store_delete` outbox sender (B3 T4a; plan §0 R5, §5.8): delete a retired generation's strata root (and its
//! mirrored subtree) in strata_rag. Queued only by crate::knowledge::generations::retire_unpinned when a job publishes
//! (event-driven, never scheduled) and by a rebuild that failed after creating its root.
//!
//! - `DELETE /api/stores/{id}?recursive=true`; a store strata no longer has (404 `not_found`) succeeds without more.
//! - Any other refusal is classified by the drainer (06 §4.5): 5xx / 503 `unavailable` retried with backoff, a 4xx
//!   (e.g. 409 `pack_published`) fails the row with strata's code.
//!
//! The payload carries only `{strata_store_id}`; strata's token is the tenant's static pair, else its own licence (`strata::plane`).
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::Actor;
use crate::error::{ApiError, ApiResult};
use crate::provision::{self, NewRow, OutboxRow, SendResult, Sender, Target};
use crate::state::AppState;
use crate::strata::rag;
use crate::strata::{self, Service, StrataStoreId};

/// The outbox op (§0 R5).
pub const OP: &str = "store_delete";

/// `strata_rag:store_delete:<strata store id>`.
pub fn dedupe_key(sid: StrataStoreId) -> String {
    format!("{}:{OP}:{sid}", Target::StrataRag.as_str())
}

/// Queue the deletion of strata root `sid` in the caller's scoped transaction.
pub async fn enqueue(
    tx: &mut PgConnection,
    tenant: Uuid,
    sid: StrataStoreId,
    actor: &Actor,
) -> ApiResult<Uuid> {
    provision::enqueue(
        tx,
        tenant,
        NewRow {
            target: Target::StrataRag,
            op: OP,
            dedupe_key: dedupe_key(sid),
            payload: json!({"strata_store_id": sid}),
        },
        actor,
    )
    .await
}

/// The `store_delete` sender (op [`OP`], target `strata_rag`).
pub struct StoreDelete;

impl Sender for StoreDelete {
    fn op(&self) -> &'static str {
        OP
    }

    fn send<'a>(&'a self, st: &'a AppState, row: &'a OutboxRow) -> BoxFuture<'a, SendResult> {
        Box::pin(send(st, row))
    }
}

/// An agent error as the drainer's answer: its status and code (classified by `outbox::classify`).
fn refused(e: &ApiError) -> SendResult {
    let (status, code) = e.status_and_code();
    SendResult::Http {
        status: status.as_u16(),
        code: Some(code.to_string()),
        retry_after: None,
        body: json!({"error": {"code": code, "message": e.to_string()}}),
    }
}

async fn send(st: &AppState, row: &OutboxRow) -> SendResult {
    if row.target != Target::StrataRag.as_str() {
        return SendResult::Fail("unsupported_target");
    }
    let Some(sid) = row
        .payload
        .get("strata_store_id")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .map(StrataStoreId)
    else {
        return SendResult::Fail("invalid_payload");
    };
    let p = match strata::plane(st, row.tenant_id, Service::Rag).await {
        Ok(p) => p,
        Err(e) => return refused(&e),
    };
    match rag::delete_store(&st.http, &p, sid, true).await {
        Ok(()) => SendResult::Done(json!({"deleted": sid})),
        Err(e) if e.code() == "not_found" => {
            SendResult::Done(json!({"deleted": sid, "gone": true}))
        }
        Err(e) => refused(&e),
    }
}
