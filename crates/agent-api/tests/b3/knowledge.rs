//! B3 T5: the knowledge-store tree (plan §5.2): create + `store_ensure` (the outbox sender against the strata fake:
//! router/leaf, `duplicate_name` binds the existing store, an unbound parent is created first), "Create missing
//! stores", delete (local children, strata `has_children`), the list with strata counts (G4) or the stale header,
//! documents (25 per page, opaque cursor, G5 language), document delete, the query playground (root resolved
//! agent-side, D91 / §0 R7; rag #23/#24 shapes; platform hits as `platform:<key>@<v>`), the leaf-set search with an
//! unreadable pack (R10/R11), and the model card. Real Postgres 18 (one container for this binary, a fresh database
//! per test) and the in-process strata fakes (`common::strata_fake`).
use crate::common;

use std::sync::Arc;

use crate::common::strata_fake::{self, Recorded, StrataFake, lock};
use crate::common::{TestEnv, get, insert_tenant, insert_user, json, person, send};
use agent_api::knowledge::stores::{self, PACK_UNAVAILABLE_TOTAL, PackLeaves};
use agent_api::observability;
use agent_api::provision::senders::stores::StoreEnsure;
use agent_api::provision::{DrainReport, Senders, drain_once};
use agent_api::strata::rag::{self, QueryReq, QueryResp, QueryTarget};
use agent_api::strata::{self, Service, StrataStoreId};
use agent_api::tenancy::begin_scoped;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

struct W {
    e: TestEnv,
    fake: StrataFake,
    tenant: Uuid,
    /// knowledge_manager
    km: String,
    /// an observer (no role)
    obs: String,
}

/// A member of `tenant` holding `role`; its token.
async fn member(e: &TestEnv, tenant: Uuid, role: &str) -> String {
    let sub = format!("u-{role}-{}", Uuid::new_v4().simple());
    insert_user(&e.owner, &sub, tenant, role).await;
    person(&sub, &[])
}

/// A fresh env and fakes; `tenant` (and `others`) exist, only `pinned` have strata token pairs.
async fn world_with(pinned: bool) -> W {
    let fake = strata_fake::start().await;
    let tenant = Uuid::new_v4();
    // a URL needs at least one token pair (config): an unpinned tenant pins someone else
    let pins: Vec<Uuid> = if pinned {
        vec![tenant]
    } else {
        vec![Uuid::new_v4()]
    };
    let e = common::env_with(|v| fake.env(&pins, v)).await;
    insert_tenant(&e.owner, tenant, &format!("T5 {tenant}")).await;
    let km = member(&e, tenant, "knowledge_manager").await;
    let obs = member(&e, tenant, "tenant_user").await;
    W {
        e,
        fake,
        tenant,
        km,
        obs,
    }
}

async fn world() -> W {
    world_with(true).await
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

async fn send_raw(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let resp = app.clone().oneshot(req).await.expect("router");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, headers, v)
}

fn doc(
    id: Uuid,
    store: Uuid,
    title: Option<&str>,
    source_id: Option<&str>,
    metadata: Value,
) -> Value {
    json!({
        "id": id, "store_id": store, "source": "upload", "title": title, "content_hash": "h",
        "token_count": 40, "chunk_count": 2, "source_id": source_id, "source_run": null,
        "metadata": metadata, "created_at": "2026-10-21T10:00:00Z"
    })
}

impl W {
    async fn call(&self, tok: &str, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        send(&self.e.app, json(method, uri, Some(tok), body)).await
    }

    async fn read(&self, uri: &str) -> (StatusCode, Value) {
        send(&self.e.app, get(uri, Some(&self.obs))).await
    }

    async fn create(&self, key: &str, kind: &str, parent: Option<Uuid>) -> Value {
        let (s, b) = self
            .call(
                &self.km,
                "POST",
                "/api/knowledge/stores",
                json!({"key": key, "label": format!("{key} label"), "parent_id": parent, "kind": kind}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        b
    }

    /// A local store inserted as the owner (no outbox row); its id.
    async fn insert(&self, key: &str, kind: &str, parent: Option<Uuid>, sid: Option<Uuid>) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO knowledge_store (tenant_id, id, key, label, parent_id, kind, strata_store_id, region, updated_by) \
             VALUES ($1, $2, $3, $3, $4, $5, $6, 'eu', 'test')",
        )
        .bind(self.tenant)
        .bind(id)
        .bind(key)
        .bind(parent)
        .bind(kind)
        .bind(sid)
        .execute(&self.e.owner)
        .await
        .unwrap();
        id
    }

    async fn bound(&self, id: Uuid) -> Option<Uuid> {
        sqlx::query_scalar("SELECT strata_store_id FROM knowledge_store WHERE id = $1")
            .bind(id)
            .fetch_one(&self.e.owner)
            .await
            .unwrap()
    }

    /// The tenant's outbox rows: (dedupe_key, op, status, payload), oldest first.
    async fn outbox(&self) -> Vec<(String, String, String, Value)> {
        sqlx::query_as(
            "SELECT dedupe_key, op, status::text, payload FROM provision_outbox \
              WHERE tenant_id = $1 ORDER BY created_at, id",
        )
        .bind(self.tenant)
        .fetch_all(&self.e.owner)
        .await
        .unwrap()
    }

    /// One drainer pass with the `store_ensure` sender.
    async fn drain(&self) -> DrainReport {
        let mut st = self.e.state.clone();
        let mut senders = Senders::default();
        senders.register(Arc::new(StoreEnsure));
        st.senders = Arc::new(senders);
        drain_once(&st).await
    }

    /// Drain until nothing is due.
    async fn drain_all(&self) {
        for _ in 0..10 {
            if self.drain().await.claimed == 0 {
                return;
            }
        }
        panic!("the outbox never drained");
    }

    fn rag(&self, method: &str, path_prefix: &str) -> Vec<Recorded> {
        self.fake
            .rag_requests()
            .into_iter()
            .filter(|r| r.method == method && r.path.starts_with(path_prefix))
            .collect()
    }

    async fn audit_count(&self, action: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE tenant_id = $1 AND action = $2")
            .bind(self.tenant)
            .bind(action)
            .fetch_one(&self.e.owner)
            .await
            .unwrap()
    }
}

fn id_of(v: &Value) -> Uuid {
    v["id"].as_str().unwrap().parse().unwrap()
}

// ---------------------------------------------------------------------------------------------------------------
// Create + store_ensure

#[tokio::test]
async fn create_store_validates_and_enqueues_store_ensure() {
    let fake = strata_fake::start().await;
    let tenant = Uuid::new_v4();
    let e = common::env_with(|v| fake.env(&[tenant], v)).await;
    // a US tenant: the store lands in its residency (D59/D66)
    sqlx::query(
        "INSERT INTO tenants (id, name, residency, updated_by) VALUES ($1, $2, 'us', 'test')",
    )
    .bind(tenant)
    .bind(format!("T5 us {tenant}"))
    .execute(&e.owner)
    .await
    .unwrap();
    let km = member(&e, tenant, "knowledge_manager").await;
    let obs = member(&e, tenant, "tenant_user").await;
    let w = W {
        e,
        fake,
        tenant,
        km,
        obs,
    };
    let faq = w.create("faq", "group", None).await;
    assert_eq!(faq["key"], "faq");
    assert_eq!(faq["kind"], "group");
    assert_eq!(faq["region"], "us");
    assert_eq!(faq["ensuring"], true);
    assert_eq!(faq["strata_store_id"], Value::Null);
    assert_eq!(
        (faq["documents"].as_i64(), faq["tokens"].as_i64()),
        (Some(0), Some(0))
    );
    let rows = w.outbox().await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (dedupe, op, status, payload) = &rows[0];
    assert_eq!(
        (dedupe.as_str(), op.as_str(), status.as_str()),
        ("strata_rag:store_ensure:faq", "store_ensure", "pending")
    );
    assert_eq!(payload["store_id"], faq["id"]);
    assert_eq!(w.audit_count("knowledge_store.create").await, 1);
    // refusals
    for (body, status, want) in [
        (
            json!({"key": "Bad Key", "label": "x", "parent_id": null, "kind": "leaf"}),
            422,
            "invalid_key",
        ),
        (
            json!({"key": "1abc", "label": "x", "parent_id": null, "kind": "leaf"}),
            422,
            "invalid_key",
        ),
        (
            json!({"key": "ok.key", "label": "   ", "parent_id": null, "kind": "leaf"}),
            422,
            "invalid_store",
        ),
        (
            json!({"key": "ok.key", "label": "x", "parent_id": null, "kind": "folder"}),
            422,
            "invalid_store",
        ),
        (
            json!({"key": "faq", "label": "x", "parent_id": null, "kind": "leaf"}),
            409,
            "store_exists",
        ),
        (
            json!({"key": "ok.key", "label": "x", "parent_id": Uuid::new_v4(), "kind": "leaf"}),
            422,
            "parent_not_group",
        ),
    ] {
        let (s, b) = w
            .call(&w.km, "POST", "/api/knowledge/stores", body.clone())
            .await;
        assert_eq!((s.as_u16(), code(&b)), (status, want), "{body}: {b}");
    }
    let policy = w.create("policy", "leaf", Some(id_of(&faq))).await;
    let (s, b) = w
        .call(
            &w.km,
            "POST",
            "/api/knowledge/stores",
            json!({"key": "policy.sub", "label": "x", "parent_id": policy["id"], "kind": "leaf"}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "parent_not_group"),
        "{b}"
    );
    // an observer cannot create
    let (s, b) = w
        .call(
            &w.obs,
            "POST",
            "/api/knowledge/stores",
            json!({"key": "obs.store", "label": "x", "parent_id": null, "kind": "leaf"}),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{b}");
    // the list shows both, still being created
    let (s, list) = w.read("/api/knowledge/stores").await;
    assert_eq!(s, StatusCode::OK, "{list}");
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{list}");
    assert!(items.iter().all(|i| i["ensuring"] == true), "{list}");
    assert_eq!(list["next"], Value::Null);
    // nothing was sent to strata yet
    assert!(w.rag("POST", "/api/stores").is_empty());
}

#[tokio::test]
async fn store_ensure_creates_router_or_leaf_and_binds() {
    let w = world().await;
    let faq = w.create("faq", "group", None).await;
    let policy = w.create("policy", "leaf", Some(id_of(&faq))).await;
    w.drain_all().await;
    let faq_sid = w.bound(id_of(&faq)).await.expect("faq bound");
    let policy_sid = w.bound(id_of(&policy)).await.expect("policy bound");
    {
        let g = lock(&w.fake.rag);
        let f = g
            .stores
            .iter()
            .find(|s| s.id == faq_sid)
            .expect("faq in strata");
        assert_eq!(
            (f.kind.as_str(), f.name.as_str(), f.parent_id),
            ("router", "faq", None)
        );
        assert_eq!(f.description, "faq label");
        let p = g
            .stores
            .iter()
            .find(|s| s.id == policy_sid)
            .expect("policy in strata");
        assert_eq!(
            (p.kind.as_str(), p.name.as_str(), p.parent_id),
            ("leaf", "policy", Some(faq_sid))
        );
        assert!(g.stores.iter().all(|s| s.tenant == w.tenant));
    }
    let posts = w.rag("POST", "/api/stores");
    assert_eq!(posts.len(), 2, "{posts:?}");
    for r in &posts {
        assert_eq!(
            r.tenant_header.as_deref(),
            Some(w.tenant.to_string().as_str())
        );
        assert_eq!(r.actor, None, "the drainer acts as no person");
    }
    let rows = w.outbox().await;
    assert!(rows.iter().all(|r| r.2 == "succeeded"), "{rows:?}");
    let (_, list) = w.read("/api/knowledge/stores").await;
    for i in list["items"].as_array().unwrap() {
        assert_eq!(i["ensuring"], false, "{i}");
        assert!(i["strata_store_id"].is_string(), "{i}");
    }
}

#[tokio::test]
async fn store_ensure_duplicate_name_binds_existing() {
    let w = world().await;
    // an earlier send created it in strata but never finished
    let existing = lock(&w.fake.rag).add_store(w.tenant, None, "router", "faq");
    let faq = w.create("faq", "group", None).await;
    w.drain_all().await;
    assert_eq!(w.bound(id_of(&faq)).await, Some(existing));
    assert_eq!(
        lock(&w.fake.rag)
            .stores
            .iter()
            .filter(|s| s.name == "faq")
            .count(),
        1
    );
    assert_eq!(w.rag("POST", "/api/stores").len(), 1, "the refused create");
    assert!(
        w.rag("GET", "/api/stores")
            .iter()
            .any(|r| r.path == "/api/stores"),
        "listed to find the existing store"
    );
    assert!(w.outbox().await.iter().all(|r| r.2 == "succeeded"));
}

/// Plan name `store_ensure_child_waits_for_parent`, changed by ruling: the drainer delivers a tenant's strata_rag rows
/// strictly oldest first, so a child that waited would hold back the parent row it waits for. The child's send
/// creates the unbound parent first; the parent's own row then finds it bound and makes no call.
#[tokio::test]
async fn store_ensure_child_binds_unbound_parents_first() {
    let w = world().await;
    // a parent with no outbox row of its own (e.g. its ensure died earlier)
    let faq = w.insert("faq", "group", None, None).await;
    let policy = w.create("policy", "leaf", Some(faq)).await;
    w.drain_all().await;
    let faq_sid = w
        .bound(faq)
        .await
        .expect("parent bound by the child's send");
    let policy_sid = w.bound(id_of(&policy)).await.expect("child bound");
    let posts = w.rag("POST", "/api/stores");
    assert_eq!(posts.len(), 2, "{posts:?}");
    assert_eq!(posts[0].body["name"], "faq", "parent first");
    assert_eq!(posts[1].body["name"], "policy");
    assert_eq!(posts[1].body["parent_id"], json!(faq_sid));
    assert_eq!(
        lock(&w.fake.rag)
            .stores
            .iter()
            .find(|s| s.id == policy_sid)
            .and_then(|s| s.parent_id),
        Some(faq_sid)
    );
    // the parent's own ensure now finds it bound: no call
    let (s, b) = w
        .call(&w.km, "POST", "/api/knowledge/stores/ensure", json!({}))
        .await;
    assert_eq!(
        (s, b["enqueued"].as_i64()),
        (StatusCode::OK, Some(0)),
        "{b}"
    );
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    stores::enqueue_ensure(
        &mut tx,
        w.tenant,
        faq,
        "faq",
        &agent_api::audit::Actor::system("test"),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    w.drain_all().await;
    assert_eq!(w.rag("POST", "/api/stores").len(), 2, "no further create");
    assert!(w.outbox().await.iter().all(|r| r.2 == "succeeded"));
}

// ---------------------------------------------------------------------------------------------------------------
// Create missing stores

#[tokio::test]
async fn ensure_missing_enqueues_one_per_unbound_parents_first() {
    let w = world().await;
    let root = w.insert("b-root", "group", None, None).await;
    w.insert("a-leaf", "leaf", Some(root), None).await;
    w.insert("c-bound", "leaf", None, Some(Uuid::new_v4()))
        .await;
    let (s, b) = w
        .call(&w.obs, "POST", "/api/knowledge/stores/ensure", json!({}))
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{b}");
    let (s, b) = w
        .call(&w.km, "POST", "/api/knowledge/stores/ensure", json!({}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b, json!({"enqueued": 2, "stores": ["b-root", "a-leaf"]}));
    let keys: Vec<String> = w.outbox().await.into_iter().map(|r| r.0).collect();
    assert_eq!(
        keys,
        [
            "strata_rag:store_ensure:b-root",
            "strata_rag:store_ensure:a-leaf"
        ]
    );
    w.drain_all().await;
    assert!(w.bound(root).await.is_some());
    // nothing left to create
    let (s, b) = w
        .call(&w.km, "POST", "/api/knowledge/stores/ensure", json!({}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b, json!({"enqueued": 0, "stores": []}));
}

#[tokio::test]
async fn ensure_while_ensuring_409_provisioning_in_flight() {
    let w = world().await;
    w.create("faq", "group", None).await;
    w.insert("other", "leaf", None, None).await;
    let (s, b) = w
        .call(&w.km, "POST", "/api/knowledge/stores/ensure", json!({}))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "provisioning_in_flight"),
        "{b}"
    );
    assert_eq!(w.outbox().await.len(), 1, "nothing queued");
}

// ---------------------------------------------------------------------------------------------------------------
// Delete

#[tokio::test]
async fn delete_store_with_children_409_store_not_empty() {
    let w = world().await;
    let faq = w.insert("faq", "group", None, Some(Uuid::new_v4())).await;
    w.insert("policy", "leaf", Some(faq), None).await;
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &format!("/api/knowledge/stores/{faq}"),
            Value::Null,
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "store_not_empty"),
        "{b}"
    );
    assert!(
        w.rag("DELETE", "/api/stores").is_empty(),
        "refused before strata"
    );
    // a store being created
    let fresh = w.create("fresh", "leaf", None).await;
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &format!("/api/knowledge/stores/{}", id_of(&fresh)),
            Value::Null,
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "provisioning_in_flight"),
        "{b}"
    );
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &format!("/api/knowledge/stores/{}", Uuid::new_v4()),
            Value::Null,
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "store_not_found"),
        "{b}"
    );
    let (s, b) = w
        .call(
            &w.obs,
            "DELETE",
            &format!("/api/knowledge/stores/{faq}"),
            Value::Null,
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{b}");
}

#[tokio::test]
async fn delete_bound_store_calls_rag_delete() {
    let w = world().await;
    let sid = lock(&w.fake.rag).add_store(w.tenant, None, "leaf", "faq");
    let faq = w.insert("faq", "leaf", None, Some(sid)).await;
    let unbound = w.insert("draft", "leaf", None, None).await;
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &format!("/api/knowledge/stores/{faq}"),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    let dels = w.rag("DELETE", "/api/stores");
    assert_eq!(dels.len(), 1, "{dels:?}");
    assert_eq!(dels[0].path, format!("/api/stores/{sid}"));
    assert_eq!(dels[0].query, "", "never recursive");
    assert!(lock(&w.fake.rag).stores.iter().all(|s| s.id != sid));
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM knowledge_store WHERE id = $1")
        .bind(faq)
        .fetch_one(&w.e.owner)
        .await
        .unwrap();
    assert_eq!(left, 0);
    assert_eq!(w.audit_count("knowledge_store.delete").await, 1);
    // an unbound store is deleted locally only
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &format!("/api/knowledge/stores/{unbound}"),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    assert_eq!(w.rag("DELETE", "/api/stores").len(), 1);
}

#[tokio::test]
async fn delete_store_holds_the_row_so_a_concurrent_source_cannot_bind_it() {
    let w = world().await;
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, languages, created_by) \
         VALUES ($1, 'support', 'Support', '[]', ARRAY['nl', 'en'], 'test')",
    )
    .bind(w.tenant)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let sid = lock(&w.fake.rag).add_store(w.tenant, None, "leaf", "faq");
    let faq = w.insert("faq", "leaf", None, Some(sid)).await;
    let pause = lock(&w.fake.rag).pause_at("DELETE", &format!("/api/stores/{sid}"));
    let (app, km) = (w.e.app.clone(), w.km.clone());
    let del = tokio::spawn(async move {
        let uri = format!("/api/knowledge/stores/{faq}");
        send(&app, json("DELETE", &uri, Some(&km), Value::Null)).await
    });
    pause.reached.notified().await;
    // the strata delete is in flight: a source bound to the store now waits for the delete to finish
    let (app, km) = (w.e.app.clone(), w.km.clone());
    let body = json!({
        "name": "Help centre", "type": "website", "config": {"urls": ["https://help.example.nl/returns"]},
        "mapping": [], "target_store_id": faq, "schedule": null, "enabled": true
    });
    let create = tokio::spawn(async move {
        send(
            &app,
            json("POST", "/api/jobs/support/sources", Some(&km), body),
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        !create.is_finished(),
        "the source create waits for the store delete"
    );
    pause.release.notify_one();
    let (s, b) = del.await.unwrap();
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    let (s, b) = create.await.unwrap();
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_target_store"),
        "the store is gone once the source may look: {b}"
    );
    // nothing local binds the deleted strata store
    let (stores, sources): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM knowledge_store WHERE tenant_id = $1), \
                (SELECT count(*) FROM source WHERE tenant_id = $1)",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!((stores, sources), (0, 0));
    assert!(lock(&w.fake.rag).stores.iter().all(|s| s.id != sid));
}

#[tokio::test]
async fn delete_rag_has_children_maps_409_store_not_empty() {
    let w = world().await;
    let sid = {
        let mut g = lock(&w.fake.rag);
        let sid = g.add_store(w.tenant, None, "router", "faq");
        // a child that exists only in strata
        g.add_store(w.tenant, Some(sid), "leaf", "elsewhere");
        sid
    };
    let faq = w.insert("faq", "group", None, Some(sid)).await;
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &format!("/api/knowledge/stores/{faq}"),
            Value::Null,
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "store_not_empty"),
        "{b}"
    );
    assert_eq!(w.bound(faq).await, Some(sid), "kept and still bound");
    assert!(lock(&w.fake.rag).stores.iter().any(|s| s.id == sid));
}

// ---------------------------------------------------------------------------------------------------------------
// List

#[tokio::test]
async fn list_merges_counts_from_rag_and_document_count_g4() {
    let w = world().await;
    let (faq_sid, policy_sid) = {
        let mut g = lock(&w.fake.rag);
        let faq = g.add_store(w.tenant, None, "router", "faq");
        let policy = g.add_store(w.tenant, Some(faq), "leaf", "policy");
        for _ in 0..3 {
            let d = doc(Uuid::new_v4(), policy, Some("Hours"), None, json!({}));
            g.documents.push((w.tenant, d));
        }
        g.script(
            "GET",
            "/api/stores",
            200,
            json!({"stores": [
                {"id": faq, "parent_id": null, "kind": "router", "name": "faq", "description": "",
                 "token_count": 0, "chunk_count": 0, "children": [policy]},
                {"id": policy, "parent_id": faq, "kind": "leaf", "name": "policy", "description": "",
                 "token_count": 1200, "chunk_count": 7, "children": []}
            ]}),
        );
        (faq, policy)
    };
    let faq = w.insert("faq", "group", None, Some(faq_sid)).await;
    let policy = w
        .insert("policy", "leaf", Some(faq), Some(policy_sid))
        .await;
    let draft = w.insert("draft", "leaf", Some(faq), None).await;
    let (s, h, b) = send_raw(&w.e.app, get("/api/knowledge/stores", Some(&w.obs))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(h.get("x-agent-stale").is_none(), "{h:?}");
    let item = |id: Uuid| {
        b["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == json!(id))
            .cloned()
            .unwrap()
    };
    let p = item(policy);
    assert_eq!(
        (
            p["documents"].as_i64(),
            p["chunks"].as_i64(),
            p["tokens"].as_i64()
        ),
        (Some(3), Some(7), Some(1200)),
        "{p}"
    );
    assert_eq!(item(faq)["documents"], 3, "a group sums its leaves");
    assert_eq!(item(draft)["documents"], 0);
    assert_eq!(item(draft)["strata_store_id"], Value::Null);
    let counted = w
        .rag("GET", &format!("/api/stores/{policy_sid}/documents"))
        .into_iter()
        .next()
        .expect("documents counted per bound leaf");
    assert!(counted.query.contains("limit=500"), "{}", counted.query);
}

#[tokio::test]
async fn list_rag_down_serves_tree_with_stale_header() {
    let w = world().await;
    let sid = lock(&w.fake.rag).add_store(w.tenant, None, "leaf", "faq");
    lock(&w.fake.rag).script(
        "GET",
        "/api/stores",
        503,
        json!({"error": {"code": "internal", "message": "down"}}),
    );
    w.insert("faq", "leaf", None, Some(sid)).await;
    let (s, h, b) = send_raw(&w.e.app, get("/api/knowledge/stores", Some(&w.obs))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        h.get("x-agent-stale").and_then(|v| v.to_str().ok()),
        Some("strata_rag")
    );
    assert_eq!(b["items"][0]["key"], "faq");
    assert_eq!(b["items"][0]["documents"], 0);
    // a tenant without strata credentials: the same
    let u = world_with(false).await;
    u.insert("faq", "leaf", None, Some(Uuid::new_v4())).await;
    let (s, h, b) = send_raw(&u.e.app, get("/api/knowledge/stores", Some(&u.obs))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        h.get("x-agent-stale").and_then(|v| v.to_str().ok()),
        Some("strata_rag")
    );
    // no bound store: nothing to read, not stale
    let n = world().await;
    n.insert("draft", "leaf", None, None).await;
    let (s, h, _) = send_raw(&n.e.app, get("/api/knowledge/stores", Some(&n.obs))).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("x-agent-stale").is_none());
}

// ---------------------------------------------------------------------------------------------------------------
// Documents

#[tokio::test]
async fn documents_page_25_cursor_passthrough() {
    let w = world().await;
    let sid = lock(&w.fake.rag).add_store(w.tenant, None, "leaf", "faq");
    let faq = w.insert("faq", "leaf", None, Some(sid)).await;
    let upload = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO knowledge_upload (tenant_id, id, store_id, language, files, ingest_job_ids, created_by) \
         VALUES ($1, $2, $3, 'nl', '[]', ARRAY['job-1'], 'test')",
    )
    .bind(w.tenant)
    .bind(upload)
    .bind(faq)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let (d1, d2, d3) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let src = format!("upload:{upload}");
    lock(&w.fake.rag).script(
        "GET",
        &format!("/api/stores/{sid}/documents"),
        200,
        json!({"items": [
            doc(d1, sid, Some("Openingstijden"), Some(src.as_str()), json!({})),
            doc(d2, sid, None, Some("src-9"), json!({"language": "de"})),
            doc(d3, sid, Some("Plain"), None, json!({})),
        ], "next": "opaque-c2"}),
    );
    let (s, b) = w
        .read(&format!(
            "/api/knowledge/stores/{faq}/documents?before=opaque-c1"
        ))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let r = w
        .rag("GET", &format!("/api/stores/{sid}/documents"))
        .pop()
        .unwrap();
    assert!(r.query.contains("limit=25"), "{}", r.query);
    assert!(r.query.contains("cursor=opaque-c1"), "{}", r.query);
    assert_eq!(b["next"], "opaque-c2", "passed back as is");
    let items = b["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["store_id"], json!(faq));
    assert_eq!(items[0]["title"], "Openingstijden");
    assert_eq!(items[0]["language"], "nl", "the upload's language (G5)");
    assert_eq!(items[0]["ingest_job_id"], "job-1");
    assert_eq!(
        (items[0]["tokens"].as_i64(), items[0]["chunks"].as_i64()),
        (Some(40), Some(2))
    );
    assert_eq!(items[0]["added_at"], "2026-10-21T10:00:00Z");
    assert_eq!(items[1]["title"], "upload", "no title: the source");
    assert_eq!(items[1]["language"], "de");
    assert_eq!(items[1]["ingest_job_id"], Value::Null);
    assert_eq!(items[2]["language"], "und");
    // a group and an unbound leaf answer an empty page without a call; an unknown store is 404
    let group = w.insert("grp", "group", None, Some(Uuid::new_v4())).await;
    let unbound = w.insert("draft", "leaf", None, None).await;
    let before = w.fake.rag_requests().len();
    for id in [group, unbound] {
        let (s, b) = w
            .read(&format!("/api/knowledge/stores/{id}/documents"))
            .await;
        assert_eq!(
            (s, b.clone()),
            (StatusCode::OK, json!({"items": [], "next": null})),
            "{b}"
        );
    }
    assert_eq!(w.fake.rag_requests().len(), before);
    let (s, b) = w
        .read(&format!(
            "/api/knowledge/stores/{}/documents",
            Uuid::new_v4()
        ))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "store_not_found"),
        "{b}"
    );
}

#[tokio::test]
async fn delete_document_other_tenant_404_document_not_found() {
    let fake = strata_fake::start().await;
    let (a, b_t) = (Uuid::new_v4(), Uuid::new_v4());
    let e = common::env_with(|v| fake.env(&[a, b_t], v)).await;
    insert_tenant(&e.owner, a, &format!("T5 A {a}")).await;
    insert_tenant(&e.owner, b_t, &format!("T5 B {b_t}")).await;
    let km = member(&e, a, "knowledge_manager").await;
    let obs = member(&e, a, "tenant_user").await;
    let w = W {
        e,
        fake,
        tenant: a,
        km,
        obs,
    };
    let (theirs, mine) = (Uuid::new_v4(), Uuid::new_v4());
    {
        let mut g = lock(&w.fake.rag);
        let bs = g.add_store(b_t, None, "leaf", "faq");
        let as_ = g.add_store(a, None, "leaf", "faq");
        g.documents
            .push((b_t, doc(theirs, bs, Some("B"), None, json!({}))));
        g.documents
            .push((a, doc(mine, as_, Some("A"), None, json!({}))));
    }
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &format!("/api/knowledge/documents/{theirs}"),
            Value::Null,
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "document_not_found"),
        "{b}"
    );
    assert!(
        lock(&w.fake.rag)
            .documents
            .iter()
            .any(|(_, d)| d["id"] == json!(theirs)),
        "the other tenant's document is untouched"
    );
    let (s, b) = w
        .call(
            &w.obs,
            "DELETE",
            &format!("/api/knowledge/documents/{mine}"),
            Value::Null,
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{b}");
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &format!("/api/knowledge/documents/{mine}"),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    assert!(
        lock(&w.fake.rag)
            .documents
            .iter()
            .all(|(_, d)| d["id"] != json!(mine))
    );
    assert_eq!(w.audit_count("knowledge_document.delete").await, 1);
}

// ---------------------------------------------------------------------------------------------------------------
// Query playground

/// A tenant hit of `faq/policy`.
fn tenant_hit() -> Value {
    json!({
        "chunk_id": Uuid::new_v4(), "document_id": Uuid::new_v4(), "store_path": ["faq", "policy"], "ord": 2,
        "text": "We open at 9.", "page_start": 1, "page_end": 1, "source": "upload", "title": "Hours",
        "scores": {"dense": 0.8, "lexical": null, "rerank": 0.9}, "source_id": null, "source_run": null,
        "metadata": {"internal_note": "x"}
    })
}

/// `faq` (group) → `policy` (bound leaf) + `draft` (unbound leaf); the strata ids of faq and policy.
async fn tree(w: &W) -> (Uuid, Uuid, Uuid) {
    let (faq_sid, policy_sid) = {
        let mut g = lock(&w.fake.rag);
        let faq = g.add_store(w.tenant, None, "router", "faq");
        let policy = g.add_store(w.tenant, Some(faq), "leaf", "policy");
        (faq, policy)
    };
    let faq = w.insert("faq", "group", None, Some(faq_sid)).await;
    let policy = w
        .insert("policy", "leaf", Some(faq), Some(policy_sid))
        .await;
    w.insert("draft", "leaf", Some(faq), None).await;
    (faq_sid, policy_sid, policy)
}

fn last_query(w: &W) -> Value {
    w.rag("POST", "/api/query")
        .pop()
        .expect("a query was sent")
        .body
}

#[tokio::test]
async fn query_maps_mode_to_route_mode_and_hits_to_store_paths() {
    let w = world().await;
    let (faq_sid, _, policy) = tree(&w).await;
    lock(&w.fake.rag).hits = vec![tenant_hit()];
    let (s, b) = w
        .call(
            &w.obs,
            "POST",
            "/api/knowledge/query",
            json!({"query": "opening hours", "mode": "laya", "top_k": 5, "min_score": 0.2, "beam": 2}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let sent = last_query(&w);
    assert_eq!(sent["route_mode"], "laya");
    assert_eq!(sent["top_k"], 5);
    assert_eq!(sent["beam"], 2);
    assert!(
        (sent["min_score"].as_f64().unwrap() - 0.2).abs() < 1e-6,
        "{sent}"
    );
    assert_eq!(sent["root_id"], json!(faq_sid));
    assert!(
        sent.get("root_ids").is_none() && sent.get("leaf_ids").is_none(),
        "{sent}"
    );
    assert_eq!(b["query"], "opening hours");
    assert_eq!(b["mode"], "laya");
    assert_eq!(b["searched"], json!(["policy"]));
    assert_eq!(b["skipped_unbound"], json!(["draft"]));
    assert_eq!(b["took_ms"], 11);
    assert_eq!(b["timings_ms"]["total"], 11);
    assert_eq!(b["routing"]["mode_used"], "laya");
    assert_eq!(
        b["routing"]["leaves"],
        json!(["faq"]),
        "strata ids shown as keys"
    );
    let h = &b["hits"][0];
    assert_eq!(h["store_id"], json!(policy));
    assert_eq!(h["store_path"], json!(["faq", "policy"]));
    assert_eq!(h["document"], "Hours");
    assert!((h["score"].as_f64().unwrap() - 0.9).abs() < 1e-6, "{h}");
    assert_eq!(h["scores"]["lexical"], Value::Null);
    assert_eq!(h["layer"], "tenant");
    assert_eq!(h["metadata"], json!({}), "only provenance is passed on");
    // embed mode: no beam
    let (s, b) = w
        .call(
            &w.obs,
            "POST",
            "/api/knowledge/query",
            json!({"query": "opening hours", "mode": "embed", "top_k": 5, "min_score": 0.2, "beam": 2}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let sent = last_query(&w);
    assert_eq!(sent["route_mode"], "embed");
    assert!(sent.get("beam").is_none(), "{sent}");
}

#[tokio::test]
async fn query_empty_422_empty_query() {
    let w = world().await;
    tree(&w).await;
    for q in ["", "   ", "?! ..."] {
        let (s, b) = w
            .call(
                &w.obs,
                "POST",
                "/api/knowledge/query",
                json!({"query": q, "mode": "hybrid", "top_k": 5, "min_score": 0.2}),
            )
            .await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "empty_query"),
            "{q:?}: {b}"
        );
    }
    assert!(w.rag("POST", "/api/query").is_empty());
}

#[tokio::test]
async fn query_resolves_root_agent_side() {
    let w = world().await;
    let (faq_sid, _, policy) = tree(&w).await;
    let q = |store: Option<Uuid>| json!({"query": "hours", "mode": "hybrid", "top_k": 5, "min_score": 0.2, "store_id": store});
    // one root, none named: that root
    let (s, b) = w
        .call(&w.obs, "POST", "/api/knowledge/query", q(None))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(last_query(&w)["root_id"], json!(faq_sid));
    // a second root: none named is refused before strata
    let hr_sid = lock(&w.fake.rag).add_store(w.tenant, None, "router", "hr");
    let hr = w.insert("hr", "group", None, Some(hr_sid)).await;
    let hr_leaf = w.insert("hr.leave", "leaf", Some(hr), None).await;
    let sent = w.rag("POST", "/api/query").len();
    let (s, b) = w
        .call(&w.obs, "POST", "/api/knowledge/query", q(None))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "root_required"),
        "{b}"
    );
    assert_eq!(b["error"]["roots"], json!(["faq", "hr"]), "{b}");
    assert_eq!(w.rag("POST", "/api/query").len(), sent, "no strata call");
    // a named store: its root
    let (s, b) = w
        .call(&w.obs, "POST", "/api/knowledge/query", q(Some(hr_leaf)))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(last_query(&w)["root_id"], json!(hr_sid));
    assert_eq!(b["skipped_unbound"], json!(["hr.leave"]));
    let (s, b) = w
        .call(&w.obs, "POST", "/api/knowledge/query", q(Some(policy)))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(last_query(&w)["root_id"], json!(faq_sid));
    let (s, b) = w
        .call(
            &w.obs,
            "POST",
            "/api/knowledge/query",
            q(Some(Uuid::new_v4())),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "store_not_found"),
        "{b}"
    );
    // every query that reached strata named its root (D91)
    assert!(
        w.rag("POST", "/api/query")
            .iter()
            .all(|r| r.body["root_id"].is_string())
    );
}

#[tokio::test]
async fn query_maps_mode_used_null_and_layer() {
    let w = world().await;
    tree(&w).await;
    lock(&w.fake.rag).script(
        "POST",
        "/api/query",
        200,
        json!({
            "results": [tenant_hit(), {
                "chunk_id": Uuid::new_v4(), "document_id": Uuid::new_v4(), "store_path": ["base", "hours"],
                "layer": "platform", "pack": "base@1", "ord": 0, "text": "Typical hours.", "page_start": null,
                "page_end": null, "source": "platform", "title": null,
                "scores": {"dense": 0.5, "lexical": 0.3, "rerank": null}, "metadata": {}
            }],
            "routing": {"mode_used": null, "leaves": [], "laya_consulted": false},
            "timings_ms": {"embed": 1, "route": 0, "search": 2, "rerank": 0, "total": 4, "leaves": 3, "path": "exact"},
            "timings_by_model": [{"embed_model": "m@1", "leaves": 1}],
            "providers": {}
        }),
    );
    let (s, b) = w
        .call(
            &w.obs,
            "POST",
            "/api/knowledge/query",
            json!({"query": "hours", "mode": "hybrid", "top_k": 5, "min_score": 0.2}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["routing"]["mode_used"], Value::Null);
    assert_eq!(b["took_ms"], 4);
    assert_eq!(b["hits"][0]["layer"], "tenant");
    assert_eq!(b["hits"][0]["pack"], Value::Null);
    assert_eq!(b["hits"][1]["layer"], "platform");
    assert_eq!(b["hits"][1]["pack"], "base@1");
    assert!(
        (b["hits"][1]["score"].as_f64().unwrap() - 0.5).abs() < 1e-6,
        "no rerank: the dense score"
    );
    assert_eq!(b["hits"][1]["document"], "platform");
}

/// Until T4a the root's strata store is its own binding; T4a re-points it to the job's pinned generation. Either way
/// the request carries the bound strata id, never the agent id, and follows a re-binding.
#[tokio::test]
async fn query_sends_pinned_generation_strata_id_as_root_id() {
    let w = world().await;
    let (faq_sid, _, _) = tree(&w).await;
    let body = json!({"query": "hours", "mode": "hybrid", "top_k": 5, "min_score": 0.2});
    let (s, b) = w
        .call(&w.obs, "POST", "/api/knowledge/query", body.clone())
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(last_query(&w)["root_id"], json!(faq_sid));
    let generation = lock(&w.fake.rag).add_store(w.tenant, None, "router", "faq-g2");
    sqlx::query(
        "UPDATE knowledge_store SET strata_store_id = $1 WHERE tenant_id = $2 AND key = 'faq'",
    )
    .bind(generation)
    .bind(w.tenant)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let (s, b) = w.call(&w.obs, "POST", "/api/knowledge/query", body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(last_query(&w)["root_id"], json!(generation));
}

#[tokio::test]
async fn query_without_token_409_provision_pending() {
    let w = world_with(false).await;
    w.insert("faq", "leaf", None, Some(Uuid::new_v4())).await;
    let (s, b) = w
        .call(
            &w.obs,
            "POST",
            "/api/knowledge/query",
            json!({"query": "hours", "mode": "hybrid", "top_k": 5, "min_score": 0.2}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "provision_pending"),
        "{b}"
    );
    assert!(
        w.fake.rag_requests().is_empty(),
        "no call without a token pair"
    );
}

// ---------------------------------------------------------------------------------------------------------------
// R10 client half

#[tokio::test]
async fn query_target_serialises_exactly_one_field() {
    let w = world().await;
    let (leaf, pack) = {
        let mut g = lock(&w.fake.rag);
        let leaf = g.add_store(w.tenant, None, "leaf", "faq");
        (leaf, g.add_pack("base", 1))
    };
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    let p = strata::gate(&w.e.state, &mut tx, w.tenant, Service::Rag)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let req = QueryReq {
        query: "hours".into(),
        ..QueryReq::default()
    };
    let targets = [
        (QueryTarget::Root(StrataStoreId(leaf)), "root_id"),
        (
            QueryTarget::Roots(vec![StrataStoreId(leaf), StrataStoreId(pack)]),
            "root_ids",
        ),
        (QueryTarget::Leaves(vec![StrataStoreId(leaf)]), "leaf_ids"),
    ];
    for (target, field) in &targets {
        rag::query(&w.e.state.http, &p, target, &req).await.unwrap();
        let sent = last_query(&w);
        let named: Vec<&str> = ["root_id", "root_ids", "leaf_ids"]
            .into_iter()
            .filter(|k| sent.get(*k).is_some())
            .collect();
        assert_eq!(named, [*field], "{sent}");
        for never in ["supersedes", "platform_penalty", "platform_min_score"] {
            assert!(sent.get(never).is_none(), "{never} is never sent: {sent}");
        }
    }
    // a leaf set (PK-7): always leaf_ids, the tenant leaf then each pack's leaves (never a pack root)
    let pr = PackLeaves {
        pin: "base@1".into(),
        leaves: vec![StrataStoreId(pack)],
        unreadable: false,
    };
    assert_eq!(
        stores::leaf_set_target(Some(StrataStoreId(leaf)), &[&pr]),
        QueryTarget::Leaves(vec![StrataStoreId(leaf), StrataStoreId(pack)])
    );
    assert_eq!(
        stores::leaf_set_target(Some(StrataStoreId(leaf)), &[]),
        QueryTarget::Leaves(vec![StrataStoreId(leaf)])
    );
}

#[tokio::test]
async fn platform_hit_maps_store_path_to_pack_pin() {
    let w = world().await;
    tree(&w).await;
    lock(&w.fake.rag).hits = vec![
        json!({
            "chunk_id": Uuid::new_v4(), "document_id": Uuid::new_v4(), "store_path": ["base", "hours"],
            "layer": "platform", "pack": "base@1", "ord": 3, "text": "Typical hours are 9-5.",
            "page_start": 1, "page_end": 2, "source": "platform", "title": "Hours (platform)",
            "scores": {"dense": 0.7, "lexical": 0.2, "rerank": 0.6}, "source_id": null, "source_run": null,
            "metadata": {"provenance": {"license": "cc-by-4.0", "source": "kb"}, "other": 1}
        }),
        tenant_hit(),
    ];
    let (s, b) = w
        .call(
            &w.obs,
            "POST",
            "/api/knowledge/query",
            json!({"query": "hours", "mode": "hybrid", "top_k": 5, "min_score": 0.2}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let p = &b["hits"][0];
    assert_eq!(p["store_id"], "platform:base@1");
    assert_eq!(p["store_path"], json!(["platform:base@1"]));
    assert_eq!(p["layer"], "platform");
    assert_eq!(p["pack"], "base@1");
    assert_eq!(
        p["metadata"],
        json!({"provenance": {"license": "cc-by-4.0", "source": "kb"}})
    );
    assert_eq!(b["hits"][1]["store_path"], json!(["faq", "policy"]));
}

fn pack_unavailable(pin: &str) -> f64 {
    let text = observability::metrics_handle().render();
    text.lines()
        .find(|l| {
            l.starts_with(&format!("{PACK_UNAVAILABLE_TOTAL}{{"))
                && l.contains(&format!("pack=\"{pin}\""))
        })
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

/// R11: an unreadable pinned pack degrades the search (counted, retried once without it); a pin the cache already
/// marks unreadable is left out up front; only an unreadable tenant leaf is an error. A second unreadable pack is
/// one more retry, never an error (review I1).
#[tokio::test]
async fn unreadable_pack_degrades_and_retries_once_without_it() {
    let w = world().await;
    let tag = Uuid::new_v4().simple().to_string();
    let (gone_pin, ok_pin, marked_pin) = (
        format!("t5-gone-{tag}@1"),
        format!("t5-ok-{tag}@2"),
        format!("t5-marked-{tag}@1"),
    );
    let (leaf, gone, ok, marked) = {
        let mut g = lock(&w.fake.rag);
        let leaf = g.add_store(w.tenant, None, "leaf", "faq");
        let gone = g.add_pack(&format!("t5-gone-{tag}"), 1);
        let ok = g.add_pack(&format!("t5-ok-{tag}"), 2);
        let marked = g.add_pack(&format!("t5-marked-{tag}"), 1);
        g.unreadable.insert(gone);
        (leaf, gone, ok, marked)
    };
    let before = pack_unavailable(&gone_pin);
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    let p = strata::gate(&w.e.state, &mut tx, w.tenant, Service::Rag)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let packs = vec![
        PackLeaves {
            pin: marked_pin.clone(),
            leaves: vec![StrataStoreId(marked)],
            unreadable: true,
        },
        pack_leaves(&gone_pin, gone),
        pack_leaves(&ok_pin, ok),
    ];
    let req = QueryReq {
        query: "returns".into(),
        ..QueryReq::default()
    };
    let r = stores::query_leaf_set(&w.e.state.http, &p, Some(StrataStoreId(leaf)), &packs, &req)
        .await
        .into_answer()
        .expect("degraded, not failed");
    assert_eq!(r.degraded, [marked_pin.clone(), gone_pin.clone()]);
    assert_eq!(pack_unavailable(&gone_pin), before + 1.0);
    let sent: Vec<Value> = w
        .rag("POST", "/api/query")
        .into_iter()
        .map(|r| r.body)
        .collect();
    assert_eq!(sent.len(), 2, "one retry");
    assert_eq!(
        sent[0]["leaf_ids"],
        json!([leaf, gone, ok]),
        "the marked pin is left out up front"
    );
    assert_eq!(sent[1]["leaf_ids"], json!([leaf, ok]));
    // the tenant leaf itself unreadable: an error, not a degrade (no pack was dropped before it)
    lock(&w.fake.rag).unreadable.insert(leaf);
    let run = stores::query_leaf_set(
        &w.e.state.http,
        &p,
        Some(StrataStoreId(leaf)),
        &packs[2..],
        &req,
    )
    .await;
    assert!(run.degraded.is_empty(), "{:?}", run.degraded);
    let err = run.into_answer().unwrap_err();
    assert_eq!(err.status_and_code(), (StatusCode::NOT_FOUND, "not_found"));
    lock(&w.fake.rag).unreadable.remove(&leaf);
}

/// The query plane of `w`'s tenant (the data-plane gate).
async fn rag_plane(w: &W) -> strata::Plane<'_> {
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    let p = strata::gate(&w.e.state, &mut tx, w.tenant, Service::Rag)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    p
}

/// A pin with one leaf (the fake answers any pack root id as a readable store, so a root id stands in for a leaf).
fn pack_leaves(pin: &str, leaf: Uuid) -> PackLeaves {
    PackLeaves {
        pin: pin.to_string(),
        leaves: vec![StrataStoreId(leaf)],
        unreadable: false,
    }
}

/// R11 (review I1): strata names one unreadable root per answer, so two unreadable pins cost two retries (one per
/// pack, bounded by the pin count) and the search still answers from the tenant leaf, both pins degraded and counted.
#[tokio::test]
async fn two_unreadable_packs_degrade_with_one_retry_each() {
    let w = world().await;
    let tag = Uuid::new_v4().simple().to_string();
    let (a_pin, b_pin, ok_pin) = (
        format!("t5-a-{tag}@1"),
        format!("t5-b-{tag}@1"),
        format!("t5-ok-{tag}@1"),
    );
    let (leaf, a, b, ok) = {
        let mut g = lock(&w.fake.rag);
        let leaf = g.add_store(w.tenant, None, "leaf", "faq");
        let a = g.add_pack(&format!("t5-a-{tag}"), 1);
        let b = g.add_pack(&format!("t5-b-{tag}"), 1);
        let ok = g.add_pack(&format!("t5-ok-{tag}"), 1);
        g.unreadable.insert(a);
        g.unreadable.insert(b);
        (leaf, a, b, ok)
    };
    let (before_a, before_b) = (pack_unavailable(&a_pin), pack_unavailable(&b_pin));
    let p = rag_plane(&w).await;
    let packs = vec![
        pack_leaves(&a_pin, a),
        pack_leaves(&b_pin, b),
        pack_leaves(&ok_pin, ok),
    ];
    let req = QueryReq {
        query: "returns".into(),
        ..QueryReq::default()
    };
    let r = stores::query_leaf_set(&w.e.state.http, &p, Some(StrataStoreId(leaf)), &packs, &req)
        .await
        .into_answer()
        .expect("two unreadable packs degrade, never fail");
    assert_eq!(r.degraded, [a_pin.clone(), b_pin.clone()]);
    assert_eq!(pack_unavailable(&a_pin), before_a + 1.0);
    assert_eq!(pack_unavailable(&b_pin), before_b + 1.0);
    let sent: Vec<Value> = w
        .rag("POST", "/api/query")
        .into_iter()
        .map(|r| r.body)
        .collect();
    assert_eq!(sent.len(), 3, "one retry per unreadable pack: {sent:?}");
    assert_eq!(sent[0]["leaf_ids"], json!([leaf, a, b, ok]));
    assert_eq!(sent[1]["leaf_ids"], json!([leaf, b, ok]));
    assert_eq!(sent[2]["leaf_ids"], json!([leaf, ok]));
    // every pack unreadable: the tenant leaf alone answers (`leaf_ids`), all three degraded
    lock(&w.fake.rag).unreadable.insert(ok);
    let r = stores::query_leaf_set(&w.e.state.http, &p, Some(StrataStoreId(leaf)), &packs, &req)
        .await
        .into_answer()
        .expect("the tenant leaf still answers");
    assert_eq!(r.degraded, [a_pin, b_pin, ok_pin]);
    let sent = w.rag("POST", "/api/query");
    assert_eq!(sent.len(), 3 + 4, "bounded by the pin count");
    assert_eq!(sent[6].body["leaf_ids"], json!([leaf]));
}

/// R11 (review I1): a pack-only kind whose only pack is unreadable answers empty and degraded; strata is not called
/// again with nothing to search.
#[tokio::test]
async fn pack_only_set_with_its_only_pack_unreadable_answers_empty() {
    let w = world().await;
    let tag = Uuid::new_v4().simple().to_string();
    let gone_pin = format!("t5-only-{tag}@1");
    let gone = {
        let mut g = lock(&w.fake.rag);
        let gone = g.add_pack(&format!("t5-only-{tag}"), 1);
        g.unreadable.insert(gone);
        gone
    };
    let p = rag_plane(&w).await;
    let req = QueryReq {
        query: "returns".into(),
        ..QueryReq::default()
    };
    let r = stores::query_leaf_set(
        &w.e.state.http,
        &p,
        None,
        &[pack_leaves(&gone_pin, gone)],
        &req,
    )
    .await
    .into_answer()
    .expect("degraded, not failed");
    assert_eq!(r.degraded, [gone_pin]);
    assert_eq!(r.resp, QueryResp::default(), "nothing left to search");
    assert_eq!(
        w.rag("POST", "/api/query").len(),
        1,
        "no call without a target"
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Model card

#[tokio::test]
async fn models_card_reads_working_embed_rerank_with_dims() {
    let w = world().await;
    for (slot, configured, kind, model, base, version) in [
        (
            "embed",
            true,
            Some("openai_compatible"),
            Some("text-embedding-3-small"),
            Some("https://api.example.com/v1"),
            3,
        ),
        ("rerank", false, None, None, None, 1),
    ] {
        sqlx::query(
            "INSERT INTO provider_slot (tenant_id, slot, service, configured, kind, base_url, model, version, updated_by) \
             VALUES ($1, $2, 'strata_rag', $3, $4, $5, $6, $7, 'test')",
        )
        .bind(w.tenant)
        .bind(slot)
        .bind(configured)
        .bind(kind)
        .bind(base)
        .bind(model)
        .bind(version)
        .execute(&w.e.owner)
        .await
        .unwrap();
    }
    lock(&w.fake.rag).tenant_providers.insert(
        w.tenant,
        json!({"embed": {"source": "tenant", "model": "text-embedding-3-small", "dims": 1536},
               "rerank": {"source": "unconfigured"}, "chat": {"source": "unconfigured"}}),
    );
    let (s, b) = w.read("/api/knowledge/models").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let em = &b["embedding"];
    assert_eq!(
        (
            em["slot"].as_str(),
            em["configured"].as_bool(),
            em["kind"].as_str(),
            em["model"].as_str()
        ),
        (
            Some("embed"),
            Some(true),
            Some("openai_compatible"),
            Some("text-embedding-3-small")
        ),
        "{em}"
    );
    assert_eq!(em["base_url"], "https://api.example.com/v1");
    assert_eq!(
        (em["version"].as_i64(), em["internal"].as_bool()),
        (Some(3), Some(false))
    );
    assert_eq!(em["dimensions"], 1536);
    let rr = &b["reranker"];
    assert_eq!(
        (rr["configured"].as_bool(), rr["model"].clone()),
        (Some(false), Value::Null),
        "{rr}"
    );
    assert_eq!(
        (rr["version"].as_i64(), rr["candidates"].as_i64()),
        (Some(1), Some(8))
    );
    assert_eq!(
        b["chunking"],
        json!({"target_tokens": 384, "overlap_tokens": 48})
    );
    assert_eq!(b["query_defaults"]["mode"], "hybrid");
    assert_eq!(
        (
            b["query_defaults"]["top_k"].as_i64(),
            b["query_defaults"]["beam"].as_i64()
        ),
        (Some(8), Some(3))
    );
    assert_eq!(rr["platform"], false, "nothing serves it: {rr}");
    // a slot the tenant binds nothing to, while strata serves it the platform's model (`source: default`): the card
    // names that model, not "Not bound"; the tenant's own embed binding still wins
    lock(&w.fake.rag).tenant_providers.insert(
        w.tenant,
        json!({"embed": {"source": "tenant", "model": "text-embedding-3-small", "dims": 1536},
               "rerank": {"source": "default", "model": "BAAI/bge-reranker-v2-m3"}, "chat": {"source": "default"}}),
    );
    let (s, b) = w.read("/api/knowledge/models").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let rr = &b["reranker"];
    assert_eq!(
        (
            rr["configured"].as_bool(),
            rr["model"].as_str(),
            rr["platform"].as_bool(),
            rr["internal"].as_bool(),
            rr["version"].as_i64()
        ),
        (
            Some(true),
            Some("BAAI/bge-reranker-v2-m3"),
            Some(true),
            Some(false),
            Some(1)
        ),
        "{rr}"
    );
    assert_eq!(
        (
            b["embedding"]["model"].as_str(),
            b["embedding"]["platform"].as_bool()
        ),
        (Some("text-embedding-3-small"), Some(false))
    );
    // the internal tenant (D70): mlx_api, strata's own models
    sqlx::query("UPDATE tenants SET platform_owned = true WHERE id = $1")
        .bind(w.tenant)
        .execute(&w.e.owner)
        .await
        .unwrap();
    lock(&w.fake.rag).tenant_providers.insert(
        w.tenant,
        json!({"embed": {"source": "default", "model": "BAAI/bge-m3", "dims": 1024},
               "rerank": {"source": "default", "model": "bge-reranker-v2-m3"}, "chat": {"source": "default"}}),
    );
    let (s, b) = w.read("/api/knowledge/models").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for (k, model) in [
        ("embedding", "BAAI/bge-m3"),
        ("reranker", "bge-reranker-v2-m3"),
    ] {
        let m = &b[k];
        assert_eq!(
            (
                m["internal"].as_bool(),
                m["kind"].as_str(),
                m["configured"].as_bool(),
                m["model"].as_str()
            ),
            (Some(true), Some("mlx_api"), Some(true), Some(model)),
            "{m}"
        );
    }
    assert_eq!(b["embedding"]["dimensions"], 1024);
}
