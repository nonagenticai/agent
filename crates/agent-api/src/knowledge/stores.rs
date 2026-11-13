//! The tenant's knowledge-store tree (B3 T5; plan §4 0052, §5.2; spec 16 §2 `knowledge_store`): agent-local rows,
//! each bound to one strata_rag store by the `store_ensure` outbox op (crate::provision::senders::stores) or not yet
//! (`strata_store_id` NULL). `ensuring` = an open (`pending` / `in_flight`) `store_ensure` row for the store's key.
//!
//! Also the kind-scoped search over a leaf set (§0 R10/R11, PK-7, [`query_leaf_set`]): the tenant leaf plus the
//! pinned platform packs' leaves (`leaf_ids`), degraded by pin (never failed) when a pinned pack is unreadable.
use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::Actor;
use crate::error::{ApiError, ApiResult};
use crate::provision::{self, NewRow, Target};
use crate::strata::rag::{self, QueryReq, QueryResp, QueryTarget};
use crate::strata::{Plane, StrataStoreId};

/// The outbox op that creates a store in strata_rag and binds it (§0 R5).
pub const ENSURE_OP: &str = "store_ensure";

/// Incremented once per query that found a pinned pack unreadable (R11); the alert reads it.
pub const PACK_UNAVAILABLE_TOTAL: &str = "agent_knowledge_pack_unavailable_total";

/// The longest store label.
pub const MAX_LABEL_CHARS: usize = 200;

/// `strata_rag:store_ensure:<key>`: the outbox dedupe key of one store (06 §4.5).
pub fn dedupe_key(key: &str) -> String {
    format!("{}:{ENSURE_OP}:{key}", Target::StrataRag.as_str())
}

/// `^[a-z][a-z0-9_.-]{1,62}$` (the 0052 CHECK and the fixture's `invalid_key`).
pub fn valid_key(key: &str) -> bool {
    let b = key.as_bytes();
    (2..=63).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b[1..].iter().all(|&c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'.' | b'-')
        })
}

/// One `knowledge_store` row with its derived `ensuring`.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct StoreRow {
    pub id: Uuid,
    pub key: String,
    pub label: String,
    pub parent_id: Option<Uuid>,
    pub kind: String,
    pub areas: Vec<String>,
    pub strata_store_id: Option<Uuid>,
    pub region: String,
    pub updated_at: DateTime<Utc>,
    pub ensuring: bool,
}

impl StoreRow {
    pub fn is_leaf(&self) -> bool {
        self.kind == "leaf"
    }

    pub fn strata(&self) -> Option<StrataStoreId> {
        self.strata_store_id.map(StrataStoreId)
    }

    /// `KnowledgeStore` (web `types/tenant.ts`) with the given counts.
    pub fn json(&self, c: Counts) -> Value {
        json!({
            "id": self.id,
            "key": self.key,
            "label": self.label,
            "parent_id": self.parent_id,
            "kind": self.kind,
            "areas": self.areas,
            "strata_store_id": self.strata_store_id,
            "ensuring": self.ensuring,
            "documents": c.documents,
            "chunks": c.chunks,
            "tokens": c.tokens,
            "updated_at": self.updated_at,
            "region": self.region,
        })
    }
}

/// A store's document, chunk and token counts (strata_rag; 0 when unknown).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub documents: i64,
    pub chunks: i64,
    pub tokens: i64,
}

const SELECT: &str = "SELECT s.id, s.key, s.label, s.parent_id, s.kind, s.areas, s.strata_store_id, s.region, \
            s.updated_at, EXISTS (SELECT 1 FROM provision_outbox o \
                                   WHERE o.op = 'store_ensure' AND o.status IN ('pending', 'in_flight') \
                                     AND o.dedupe_key = 'strata_rag:store_ensure:' || s.key) AS ensuring \
       FROM knowledge_store s";

/// Every store of the scoped tenant, oldest first.
pub async fn all(c: &mut PgConnection) -> ApiResult<Vec<StoreRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} ORDER BY s.created_at, s.key"
    )))
    .fetch_all(&mut *c)
    .await?)
}

/// One store of the scoped tenant; `for_update` locks it.
pub async fn by_id(
    c: &mut PgConnection,
    id: Uuid,
    for_update: bool,
) -> ApiResult<Option<StoreRow>> {
    let lock = if for_update { " FOR UPDATE OF s" } else { "" };
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} WHERE s.id = $1{lock}"
    )))
    .bind(id)
    .fetch_optional(&mut *c)
    .await?)
}

/// 404 `store_not_found` (fixture `storeById`).
pub fn store_not_found() -> ApiError {
    ApiError::Missing(
        "store_not_found",
        "No knowledge store with that id in this tenant.".into(),
    )
}

/// Queue `store_ensure` for one store in the caller's scoped transaction.
pub async fn enqueue_ensure(
    tx: &mut PgConnection,
    tenant: Uuid,
    store_id: Uuid,
    key: &str,
    actor: &Actor,
) -> ApiResult<Uuid> {
    provision::enqueue(
        tx,
        tenant,
        NewRow {
            target: Target::StrataRag,
            op: ENSURE_OP,
            dedupe_key: dedupe_key(key),
            payload: json!({"store_id": store_id, "key": key}),
        },
        actor,
    )
    .await
}

/// Queue `store_ensure` for every unbound store without an open row, parents before children (by depth, then key);
/// the keys queued. "Create missing stores" (after its in-flight check) and the job-creation hook call it.
pub async fn ensure_unbound(
    tx: &mut PgConnection,
    tenant: Uuid,
    actor: &Actor,
) -> ApiResult<Vec<String>> {
    let missing: Vec<(Uuid, String)> = sqlx::query_as(
        "WITH RECURSIVE t AS ( \
             SELECT id, key, strata_store_id, 0 AS depth FROM knowledge_store WHERE parent_id IS NULL \
             UNION ALL \
             SELECT s.id, s.key, s.strata_store_id, t.depth + 1 \
               FROM knowledge_store s JOIN t ON s.parent_id = t.id) \
         SELECT t.id, t.key FROM t \
          WHERE t.strata_store_id IS NULL \
            AND NOT EXISTS (SELECT 1 FROM provision_outbox o \
                             WHERE o.op = 'store_ensure' AND o.status IN ('pending', 'in_flight') \
                               AND o.dedupe_key = 'strata_rag:store_ensure:' || t.key) \
          ORDER BY t.depth, t.key",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut keys = Vec::with_capacity(missing.len());
    for (id, key) in missing {
        enqueue_ensure(tx, tenant, id, &key, actor).await?;
        keys.push(key);
    }
    Ok(keys)
}

/// The keys from the root down to `id` (empty when `id` is not in `stores`).
pub fn path_of(stores: &[StoreRow], id: Uuid) -> Vec<String> {
    let by_id: HashMap<Uuid, &StoreRow> = stores.iter().map(|s| (s.id, s)).collect();
    let mut path = Vec::new();
    let mut cur = by_id.get(&id).copied();
    while let Some(s) = cur {
        if path.len() > stores.len() {
            break;
        }
        path.push(s.key.clone());
        cur = s.parent_id.and_then(|p| by_id.get(&p).copied());
    }
    path.reverse();
    path
}

/// The root of `id`'s tree (`id` itself for a root); `None` when `id` is not in `stores`.
pub fn root_of(stores: &[StoreRow], id: Uuid) -> Option<Uuid> {
    let by_id: HashMap<Uuid, &StoreRow> = stores.iter().map(|s| (s.id, s)).collect();
    let mut cur = by_id.get(&id).copied()?;
    for _ in 0..=stores.len() {
        match cur.parent_id.and_then(|p| by_id.get(&p).copied()) {
            Some(p) => cur = p,
            None => break,
        }
    }
    Some(cur.id)
}

/// The stores of the tree whose root is `root` (the root included); `root` must be a root store.
pub fn subtree(stores: &[StoreRow], root: Uuid) -> Vec<&StoreRow> {
    stores
        .iter()
        .filter(|s| root_of(stores, s.id) == Some(root))
        .collect()
}

/// The footprint's `stores` (web `TenantFootprint.stores`): leaves, bound leaves, unbound store keys. `documents`
/// is 0: the footprint reads local state only (no strata call; counts are on the Knowledge page, G4).
pub async fn footprint(c: &mut PgConnection) -> ApiResult<Value> {
    let stores = all(c).await?;
    let leaves = stores.iter().filter(|s| s.is_leaf()).count();
    let bound = stores
        .iter()
        .filter(|s| s.is_leaf() && s.strata_store_id.is_some())
        .count();
    let unbound: Vec<&str> = stores
        .iter()
        .filter(|s| s.strata_store_id.is_none())
        .map(|s| s.key.as_str())
        .collect();
    Ok(json!({"leaves": leaves, "bound": bound, "unbound": unbound, "documents": 0}))
}

// ---------------------------------------------------------------------------------------------------------------
// Kind-scoped search over a leaf set (§0 R10/R11, PK-7)

/// A pinned platform pack resolved to its leaves (PK-7): the release snapshot (`release_pack_leaf`) for a released
/// design, the tenant's leaf cache (`platform_pack_leaf`) for the draft.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackLeaves {
    /// `"<pack_key>@<version>"`.
    pub pin: String,
    /// The pack's leaf stores (strata ids), searched as themselves (`leaf_ids`, no routing, no pruning).
    pub leaves: Vec<StrataStoreId>,
    /// Left out up front and reported degraded: the cache marks it unreadable (`platform_pack.unreadable_at`) or
    /// excluded (`excluded_by`), or it has no leaf to search.
    pub unreadable: bool,
}

impl PackLeaves {
    /// Searched: not marked, and at least one leaf.
    pub fn live(&self) -> bool {
        !self.unreadable && !self.leaves.is_empty()
    }
}

/// What a leaf-set search answered: strata's response and the pins it ran without (R11).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LeafSetAnswer {
    pub resp: QueryResp,
    /// `"<pack_key>@<version>"` of every pinned pack left out, in pin order then discovery order.
    pub degraded: Vec<String>,
    /// KH T7: `"<pack_key>@<version>"` of every pinned pack whose leaves the answer searched (pin order): what a
    /// platform hit's `pack` must name to be cited (D109).
    pub searched: Vec<String>,
    /// DRAFT only: pinned packs whose leaves the cache does not have and could not be refreshed from strata (cold
    /// cache). Unlike `degraded` they do not fail a test-chat turn: the tenant's own stores are still searched, the
    /// run logs and names them (`unsearched_packs`).
    pub cold: Vec<String>,
}

/// The query target of a leaf set (PK-7): always `leaf_ids = [tenant leaf] ++ [each live pack's leaves]` (no
/// routing, no pruning; `root_id` / `root_ids` route and prune, so a pinned pack is never named by its root). A leaf
/// named twice is sent once. Nothing → an empty `leaf_ids` (the caller makes no strata call).
pub fn leaf_set_target(tenant_leaf: Option<StrataStoreId>, packs: &[&PackLeaves]) -> QueryTarget {
    let mut ids: Vec<StrataStoreId> = Vec::new();
    for id in tenant_leaf
        .into_iter()
        .chain(packs.iter().flat_map(|p| p.leaves.iter().copied()))
    {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    QueryTarget::Leaves(ids)
}

/// One leaf-set search ([`query_leaf_set`]): the pins it ran without, on EVERY path (the caller marks them in its
/// `platform_pack` cache even when the search then failed for another reason), and strata's answer or the error.
#[derive(Debug)]
pub struct LeafSetRun {
    /// `"<pack_key>@<version>"` of every pinned pack left out: the ones already marked unreadable or without leaves
    /// (pin order), then the ones this search found unreadable (discovery order).
    pub degraded: Vec<String>,
    pub result: ApiResult<QueryResp>,
}

impl LeafSetRun {
    /// The answer with its degraded pins, or the search's error.
    pub fn into_answer(self) -> ApiResult<LeafSetAnswer> {
        let resp = self.result?;
        Ok(LeafSetAnswer {
            resp,
            degraded: self.degraded,
            searched: Vec::new(),
            cold: Vec::new(),
        })
    }
}

/// R11 (B3 ruling, degrade by pin): search a kind's leaf set; an unreadable pinned pack degrades the answer, it
/// never fails it. strata fails the whole request on one unreadable store (404 `not found: store <id>`,
/// `rag::unreadable_store`) and names one store per answer, so the pin owning that leaf loses ALL its leaves, is
/// counted (`agent_knowledge_pack_unavailable_total{pack}`), logged, and the search is retried without it: once per
/// unreadable pack, so at most `pins + 1` calls. Pins already marked (or without leaves) are left out up front. When
/// every pack is gone and there is no tenant leaf, the answer is empty and strata is not called again. Only an
/// unreadable tenant leaf (or any other error) fails the search; [`LeafSetRun::degraded`] still names the pins
/// dropped before it, so the caller records them in its `platform_pack` cache on the error path too.
pub async fn query_leaf_set(
    http: &reqwest::Client,
    p: &Plane<'_>,
    tenant_leaf: Option<StrataStoreId>,
    packs: &[PackLeaves],
    req: &QueryReq,
) -> LeafSetRun {
    let mut degraded: Vec<String> = packs
        .iter()
        .filter(|k| !k.live())
        .map(|k| k.pin.clone())
        .collect();
    let mut live: Vec<&PackLeaves> = packs.iter().filter(|k| k.live()).collect();
    // every failed try removes one live pack, so the loop ends after at most `live.len() + 1` calls
    loop {
        if tenant_leaf.is_none() && live.is_empty() {
            return LeafSetRun {
                degraded,
                result: Ok(QueryResp::default()),
            };
        }
        let target = leaf_set_target(tenant_leaf, &live);
        let e = match rag::query(http, p, &target, req).await {
            Ok(resp) => {
                return LeafSetRun {
                    degraded,
                    result: Ok(resp),
                };
            }
            Err(e) => e,
        };
        let Some(i) = rag::unreadable_store(&e)
            .filter(|bad| Some(*bad) != tenant_leaf)
            .and_then(|bad| live.iter().position(|k| k.leaves.contains(&bad)))
        else {
            return LeafSetRun {
                degraded,
                result: Err(e),
            };
        };
        let k = live.remove(i);
        metrics::counter!(PACK_UNAVAILABLE_TOTAL, "pack" => k.pin.clone()).increment(1);
        tracing::warn!(pack = %k.pin, "pinned knowledge pack is unreadable; searching without it");
        degraded.push(k.pin.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_follow_the_check() {
        for ok in ["ab", "policy.gift_cards", "faq-2", "a1"] {
            assert!(valid_key(ok), "{ok}");
        }
        let long = "a".repeat(64);
        for bad in ["a", "1ab", "Ab", "a b", "a/b", "", long.as_str()] {
            assert!(!valid_key(bad), "{bad}");
        }
        assert!(valid_key(&"a".repeat(63)));
    }

    #[test]
    fn leaf_set_target_shapes() {
        let leaf = StrataStoreId(Uuid::from_u128(1));
        let (a, b) = (
            StrataStoreId(Uuid::from_u128(2)),
            StrataStoreId(Uuid::from_u128(3)),
        );
        let pack = PackLeaves {
            pin: "base@1".into(),
            leaves: vec![a, b],
            unreadable: false,
        };
        let other = PackLeaves {
            pin: "industry/retail@2".into(),
            leaves: vec![b, StrataStoreId(Uuid::from_u128(4))],
            unreadable: false,
        };
        assert_eq!(
            leaf_set_target(Some(leaf), &[]),
            QueryTarget::Leaves(vec![leaf])
        );
        assert_eq!(
            leaf_set_target(Some(leaf), &[&pack]),
            QueryTarget::Leaves(vec![leaf, a, b])
        );
        assert_eq!(
            leaf_set_target(None, &[&pack, &other]),
            QueryTarget::Leaves(vec![a, b, StrataStoreId(Uuid::from_u128(4))]),
            "a leaf named twice is sent once"
        );
        assert_eq!(leaf_set_target(None, &[]), QueryTarget::Leaves(vec![]));
        let empty = PackLeaves {
            pin: "base@1".into(),
            leaves: vec![],
            unreadable: false,
        };
        assert!(pack.live() && !empty.live());
    }
}
