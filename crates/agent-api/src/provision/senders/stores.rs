//! The `store_ensure` outbox sender (B3 T5; plan §0 R5, §5.2, §7; reuses P4 T4b): create a knowledge store in
//! strata_rag and bind it (`knowledge_store.strata_store_id`).
//!
//! `send` reads the store and its unbound ancestors in a short read transaction (committed before any call), then
//! creates them top-down in strata_rag (agent `group` → strata `router`, agent `key` → strata `name`, `label` →
//! `description`), each under the strata store of its parent. `on_success` binds every store it created, in the
//! drainer's finishing transaction.
//!
//! - A store already bound, or gone (deleted since it was queued), succeeds without a call.
//! - An unbound parent is created first, in the same send. A child does NOT wait for its parent's own row: the
//!   drainer delivers one tenant's rows of one target strictly oldest first (`provision_claim`), so a waiting child
//!   would hold back the very parent row it waits for. The parent's own row then finds it bound.
//! - 409 `duplicate_name` (strata already holds that name under that parent: an earlier send that created it but
//!   failed before its finish) binds the existing store found by listing the tenant's stores.
//! - Any other refusal is classified by the drainer (06 §4.5): 5xx / 503 `unavailable` retried with backoff, a 4xx
//!   fails the row with strata's code.
//!
//! The drainer acts as no person: no `X-Actor-Id`. strata's token is the tenant's static pair, else its own licence (`strata::plane`), never from
//! the payload, which carries only `{store_id, key}`.
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::knowledge::stores::ENSURE_OP;
use crate::provision::{OutboxRow, SendResult, Sender, Target};
use crate::state::AppState;
use crate::strata::rag::{self, NewStore, StoreKind};
use crate::strata::{self, Plane, Service, StrataStoreId};
use crate::tenancy::begin_scoped;

/// The `store_ensure` sender (op [`ENSURE_OP`], target `strata_rag`).
pub struct StoreEnsure;

impl Sender for StoreEnsure {
    fn op(&self) -> &'static str {
        ENSURE_OP
    }

    fn send<'a>(&'a self, st: &'a AppState, row: &'a OutboxRow) -> BoxFuture<'a, SendResult> {
        Box::pin(send(st, row))
    }

    fn on_success<'a>(
        &'a self,
        tx: &'a mut PgConnection,
        _row: &'a OutboxRow,
        result: &'a Value,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(bind(tx, result))
    }
}

/// One store of the chain from the topmost unbound ancestor (or the first bound one) down to the queued store.
#[derive(sqlx::FromRow)]
struct Link {
    id: Uuid,
    key: String,
    label: String,
    kind: String,
    strata_store_id: Option<Uuid>,
}

/// The queued store and its ancestors up to (and including) the first bound one, topmost first. Empty when the store
/// is gone.
async fn chain(st: &AppState, tenant: Uuid, store: Uuid) -> ApiResult<Vec<Link>> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let links: Vec<Link> = sqlx::query_as(
        "WITH RECURSIVE up AS ( \
             SELECT id, key, label, kind, parent_id, strata_store_id, 0 AS depth \
               FROM knowledge_store WHERE id = $1 \
             UNION ALL \
             SELECT s.id, s.key, s.label, s.kind, s.parent_id, s.strata_store_id, up.depth + 1 \
               FROM knowledge_store s JOIN up ON s.id = up.parent_id \
              WHERE up.strata_store_id IS NULL AND up.depth < 64) \
         SELECT id, key, label, kind, strata_store_id FROM up ORDER BY depth DESC",
    )
    .bind(store)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(links)
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

/// The strata store named `name` directly under `parent` (a root when `None`).
async fn existing(
    st: &AppState,
    p: &Plane<'_>,
    parent: Option<StrataStoreId>,
    name: &str,
) -> ApiResult<Option<StrataStoreId>> {
    let stores = rag::stores(&st.http, p).await?;
    Ok(stores
        .into_iter()
        .find(|s| s.parent_id == parent && s.name == name)
        .map(|s| s.id))
}

async fn send(st: &AppState, row: &OutboxRow) -> SendResult {
    if row.target != Target::StrataRag.as_str() {
        return SendResult::Fail("unsupported_target");
    }
    let Some(store) = row
        .payload
        .get("store_id")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
    else {
        return SendResult::Fail("invalid_payload");
    };
    let links = match chain(st, row.tenant_id, store).await {
        Ok(l) => l,
        Err(e) => return SendResult::Transport(format!("reading the store tree failed: {e}")),
    };
    match links.last() {
        None => return SendResult::Done(json!({"bound": [], "gone": true})),
        Some(l) if l.strata_store_id.is_some() => return SendResult::Done(json!({"bound": []})),
        Some(_) => {}
    }
    let p = match strata::plane(st, row.tenant_id, Service::Rag).await {
        Ok(p) => p,
        Err(e) => return refused(&e),
    };
    let mut parent: Option<StrataStoreId> = None;
    let mut bound = Vec::new();
    for l in &links {
        if let Some(sid) = l.strata_store_id {
            parent = Some(StrataStoreId(sid));
            continue;
        }
        let new = NewStore {
            parent_id: parent,
            kind: if l.kind == "group" {
                StoreKind::Router
            } else {
                StoreKind::Leaf
            },
            name: l.key.clone(),
            description: l.label.clone(),
        };
        let sid = match rag::create_store(&st.http, &p, &new).await {
            Ok(v) => v.id,
            Err(e) if e.code() == "duplicate_name" => {
                match existing(st, &p, parent, &l.key).await {
                    Ok(Some(sid)) => sid,
                    Ok(None) => return refused(&e),
                    Err(e) => return refused(&e),
                }
            }
            Err(e) => return refused(&e),
        };
        bound.push(json!({"store_id": l.id, "key": l.key, "strata_store_id": sid}));
        parent = Some(sid);
    }
    SendResult::Http {
        status: 201,
        code: None,
        retry_after: None,
        body: json!({ "bound": bound }),
    }
}

/// Bind every store the send created (a store bound meanwhile, or deleted, is left alone).
async fn bind(tx: &mut PgConnection, result: &Value) -> ApiResult<()> {
    let Some(bound) = result.get("bound").and_then(Value::as_array) else {
        return Ok(());
    };
    for b in bound {
        let id = b
            .get("store_id")
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok());
        let sid = b
            .get("strata_store_id")
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok());
        let (Some(id), Some(sid)) = (id, sid) else {
            continue;
        };
        sqlx::query(
            "UPDATE knowledge_store SET strata_store_id = $2, updated_at = clock_timestamp() \
              WHERE id = $1 AND strata_store_id IS NULL",
        )
        .bind(id)
        .bind(sid)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}
