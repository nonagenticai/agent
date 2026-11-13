//! PK T5: the kind-scoped search over a leaf set (plan PK-7, PK-8, PK-12, PK-13). `knowledge::kinds::search` sends
//! `leaf_ids = [tenant leaf] ++ [pinned packs' leaves]` (never `root_id` / `root_ids`): the release snapshot
//! (`release_pack_leaf`) for a released design, the tenant's leaf cache (`platform_pack_leaf`) for the draft (refreshed
//! from strata once when a pin has no leaves). An unreadable pack degrades by pin (R11). The tenant's `supersedes`
//! (sources + uploads of the tenant leaf) are merged into the request's. The playground searches a kind without a
//! root (D91 relaxed). `citations::answer_source` labels a platform hit. PK-12: packs never enter model changes.
//! Real Postgres 18 (one container for the pk binary, a fresh database per test) and the in-process strata fake (T2),
//! whose platform owner is the `AGENT_PLATFORM_TENANT_ID` tenant.
use crate::common;

use crate::common::strata_fake::{self, Recorded, StrataFake, lock};
use crate::common::{
    TestEnv, get, insert_tenant, insert_user, json as req, person, send, super_admin,
};
use agent_api::design::{DesignOf, JobRef};
use agent_api::knowledge::{citations, kinds, model_changes};
use agent_api::providers::kinds::Slot;
use agent_api::strata::rag::QueryReq;
use agent_api::tenancy::begin_scoped;
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

/// The stage every test kind lives in.
const STAGE: &str = "pk";

struct W {
    e: TestEnv,
    fake: StrataFake,
    tenant: Uuid,
    /// The strata platform owner (also `AGENT_PLATFORM_TENANT_ID`).
    owner: Uuid,
    job: JobRef,
    /// tenant_admin
    tok: String,
    /// the `selection_updated_at` of the saved selection
    seen: Value,
}

/// A complete selection of the seed options (compiles with no violation; tests/release_cut.rs `sel`).
fn sel() -> Value {
    json!({
        "profile": "retail_ecommerce",
        "industry": "retail",
        "departments": ["industry_dept/retail/customer_service"],
        "areas": ["order-support"],
        "dimensions": {"channel": ["chat"]}, "region": "eu",
    })
}

/// A tenant with one job (`POST /api/jobs` from the seed template) and a complete saved selection; the strata
/// platform owner is `AGENT_PLATFORM_TENANT_ID` (as tests/pk/cut.rs).
async fn world() -> W {
    let fake = strata_fake::start().await;
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    lock(&fake.rag).platform_owner = Some(owner);
    let e = common::env_with(|v| {
        fake.env(&[tenant, owner], v);
        v.insert("AGENT_PLATFORM_TENANT_ID".into(), owner.to_string());
    })
    .await;
    insert_tenant(&e.owner, tenant, &format!("PK query {tenant}")).await;
    let mut r = req(
        "POST",
        "/api/jobs",
        Some(&super_admin()),
        json!({"template": format!("job/customer_care_agent@v{}", 4)}),
    );
    r.headers_mut()
        .insert("x-active-tenant-id", tenant.to_string().parse().unwrap());
    let (s, b) = send(&e.app, r).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let key = b["key"].as_str().expect("JobSummary.key").to_string();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
        .bind(tenant)
        .bind(&key)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let sub = format!("u-pk-query-{}", tenant.simple());
    insert_user(&e.owner, &sub, tenant, "tenant_admin").await;
    let tok = person(&sub, &[]);
    let body = json!({"selection": sel(), "expected_updated_at": Value::Null});
    let (s, b) = send(
        &e.app,
        req("PUT", &format!("/api/jobs/{key}/target"), Some(&tok), body),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let seen = b["selection_updated_at"].clone();
    W {
        e,
        fake,
        tenant,
        owner,
        job: JobRef { id, key },
        tok,
        seen,
    }
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

impl W {
    /// The knowledge kind `(STAGE, kind)` bound to `store` (a leaf path) and pinning `packs`, written as the owner.
    async fn kind(&self, kind: &str, store: Option<&str>, packs: &[&str]) {
        let packs: Vec<String> = packs.iter().map(|p| p.to_string()).collect();
        sqlx::query(
            "INSERT INTO job_knowledge_kind (tenant_id, job_id, stage, kind, required, description, origin, \
                                             store_path, packs, packs_source, updated_by) \
             VALUES ($1, $2, $3, $4, false, 'pk kind', 'tenant', $5, $6, 'binding', 'test') \
             ON CONFLICT (tenant_id, job_id, stage, kind) DO UPDATE \
                SET store_path = EXCLUDED.store_path, packs = EXCLUDED.packs",
        )
        .bind(self.tenant)
        .bind(self.job.id)
        .bind(STAGE)
        .bind(kind)
        .bind(store)
        .bind(packs)
        .execute(&self.e.owner)
        .await
        .unwrap();
    }

    /// A published pack of the platform owner with `leaves`: (root, leaf ids sorted, as the search sends them).
    fn pack(&self, key: &str, version: i32, leaves: &[&str]) -> (Uuid, Vec<Uuid>) {
        let (root, mut ids) = lock(&self.fake.rag).add_owned_pack(key, version, leaves, true);
        ids.sort();
        (root, ids)
    }

    /// A root leaf store of the tenant bound to a fresh strata store: (agent store id, strata store id).
    async fn tenant_leaf(&self, key: &str) -> (Uuid, Uuid) {
        let sid = lock(&self.fake.rag).add_store(self.tenant, None, "leaf", key);
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO knowledge_store (tenant_id, id, key, label, parent_id, kind, strata_store_id, region, \
                                          updated_by) \
             VALUES ($1, $2, $3, $3, NULL, 'leaf', $4, 'eu', 'test')",
        )
        .bind(self.tenant)
        .bind(id)
        .bind(key)
        .bind(sid)
        .execute(&self.e.owner)
        .await
        .unwrap();
        (id, sid)
    }

    fn exclude(&self, prefix: &str) {
        lock(&self.fake.rag).exclusions.push((
            self.tenant,
            json!({"key_prefix": prefix, "reason": "regulation", "created_at": "2026-10-21T09:00:00Z"}),
        ));
    }

    async fn search(&self, kind: &str, of: DesignOf, query: QueryReq) -> (Vec<String>, Value) {
        let a = kinds::search(
            &self.e.state,
            self.tenant,
            &self.job,
            STAGE,
            kind,
            &of,
            &query,
        )
        .await
        .expect("search");
        (a.degraded, serde_json::to_value(&a.resp.results).unwrap())
    }

    fn queries(&self) -> Vec<Recorded> {
        self.fake
            .rag_requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path == "/api/query")
            .collect()
    }

    /// A cut (`apply: false`) that must create a release; its id.
    async fn cut(&self) -> String {
        let (s, b) = send(
            &self.e.app,
            req(
                "POST",
                &format!("/api/jobs/{}/releases", self.job.key),
                Some(&self.tok),
                json!({"apply": false, "selection_seen_at": self.seen}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        b["release"]["id"].as_str().expect("release id").to_string()
    }

    async fn count(&self, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql)
            .bind(self.tenant)
            .fetch_one(&self.e.owner)
            .await
            .unwrap()
    }
}

fn q(text: &str) -> QueryReq {
    QueryReq {
        query: text.into(),
        ..QueryReq::default()
    }
}

/// The ids a query body names under `field`.
fn ids(body: &Value, field: &str) -> Vec<Uuid> {
    body[field]
        .as_array()
        .unwrap_or_else(|| panic!("{field} in {body}"))
        .iter()
        .map(|v| Uuid::parse_str(v.as_str().unwrap()).unwrap())
        .collect()
}

fn assert_leaf_ids_only(body: &Value) {
    assert!(
        body.get("root_id").is_none() && body.get("root_ids").is_none(),
        "a leaf set never names a root: {body}"
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Target shape and the supersedes union

#[tokio::test]
async fn kind_search_sends_leaf_ids_and_the_tenant_supersedes_union() {
    let w = world().await;
    let (store, leaf) = w.tenant_leaf("faq").await;
    let (other, _) = w.tenant_leaf("other").await;
    let (_, base) = w.pack("base", 1, &["a", "b"]);
    let (_, retail) = w.pack("industry/retail", 2, &["c"]);
    w.kind("faq.x", Some("faq"), &["base@1", "industry/retail@2"])
        .await;
    // the tenant leaf's enabled source and upload count; a disabled source and another store's upload do not
    for (name, enabled, keys, target) in [
        ("site", true, vec!["hr/leave", "faq/returns"], store),
        ("off", false, vec!["x/disabled"], store),
        ("elsewhere", true, vec!["x/other-store"], other),
    ] {
        sqlx::query(
            "INSERT INTO source (tenant_id, job_id, id, name, type, config, mapping, target_store_id, enabled, \
                                 language, updated_by, supersedes) \
             VALUES ($1, $2, $3, $4, 'website', '{}', '[]', $5, $6, 'en', 'test', $7)",
        )
        .bind(w.tenant)
        .bind(w.job.id)
        .bind(Uuid::new_v4())
        .bind(name)
        .bind(target)
        .bind(enabled)
        .bind(keys.iter().map(|k| k.to_string()).collect::<Vec<_>>())
        .execute(&w.e.owner)
        .await
        .unwrap();
    }
    for (target, keys) in [
        (store, vec!["faq/returns", "pay/refunds"]),
        (other, vec!["x/upload-elsewhere"]),
    ] {
        sqlx::query(
            "INSERT INTO knowledge_upload (tenant_id, id, store_id, language, created_by, supersedes) \
             VALUES ($1, $2, $3, 'en', 'test', $4)",
        )
        .bind(w.tenant)
        .bind(Uuid::new_v4())
        .bind(target)
        .bind(keys.iter().map(|k| k.to_string()).collect::<Vec<_>>())
        .execute(&w.e.owner)
        .await
        .unwrap();
    }
    let mut query = q("returns");
    query.supersedes = Some(vec!["a/caller".into(), "faq/returns".into()]);
    let (degraded, _) = w.search("faq.x", DesignOf::Draft, query).await;
    assert!(degraded.is_empty(), "{degraded:?}");
    let qs = w.queries();
    assert_eq!(qs.len(), 1);
    let body = &qs[0].body;
    let mut want = vec![leaf];
    want.extend(&base);
    want.extend(&retail);
    assert_eq!(
        ids(body, "leaf_ids"),
        want,
        "tenant leaf, then each pin's leaves"
    );
    assert_leaf_ids_only(body);
    assert_eq!(
        body["supersedes"],
        json!(["a/caller", "faq/returns", "hr/leave", "pay/refunds"]),
        "request ∪ enabled sources ∪ uploads of the tenant leaf, sorted, deduplicated: {body}"
    );
    // the leaves were fetched once and cached; the next search reads the cache (no `/stores` call)
    let stores_calls = |w: &W| {
        w.fake
            .rag_requests()
            .iter()
            .filter(|r| r.method == "GET" && r.path.ends_with("/stores"))
            .count()
    };
    let before = stores_calls(&w);
    w.search("faq.x", DesignOf::Draft, q("returns")).await;
    assert_eq!(stores_calls(&w), before);
    assert_eq!(ids(&w.queries()[1].body, "leaf_ids"), want);
    // no tenant key and no caller key: no supersedes field at all
    w.kind("faq.y", None, &["base@1"]).await;
    w.search("faq.y", DesignOf::Draft, q("returns")).await;
    let last = w.queries().pop().unwrap();
    assert_eq!(ids(&last.body, "leaf_ids"), base);
    assert!(last.body.get("supersedes").is_none(), "{}", last.body);
}

/// The audit finding on `merge_supersedes`: a request key that is not a topic key is refused 422 before any strata
/// call (it would only become strata's 400), and a merged set over strata's 32 keys fails loud (422
/// `supersedes_limit`) instead of silently dropping tenant overrides.
#[tokio::test]
async fn bad_request_supersedes_and_an_overfull_merge_fail_loud() {
    let w = world().await;
    let (store, _) = w.tenant_leaf("faq").await;
    w.pack("base", 1, &["a"]);
    w.kind("faq.x", Some("faq"), &["base@1"]).await;
    let wr = &w;
    let err = |query: QueryReq| async move {
        kinds::search(
            &wr.e.state,
            wr.tenant,
            &wr.job,
            STAGE,
            "faq.x",
            &DesignOf::Draft,
            &query,
        )
        .await
        .err()
        .map(|e| e.code().to_string())
    };
    // a bad request key: 422, and strata is never called
    let calls = w.fake.rag_requests().len();
    let mut query = q("returns");
    query.supersedes = Some(vec!["faq/returns".into(), "Not A Key".into()]);
    assert_eq!(err(query).await.as_deref(), Some("invalid_supersedes"));
    assert_eq!(w.fake.rag_requests().len(), calls, "no strata call");
    // 30 tenant keys on the leaf's upload + 3 new request keys = 33 > 32: refused, no query sent
    let tenant_keys: Vec<String> = (0..30).map(|i| format!("t/k{i:02}")).collect();
    sqlx::query(
        "INSERT INTO knowledge_upload (tenant_id, id, store_id, language, created_by, supersedes) \
         VALUES ($1, $2, $3, 'en', 'test', $4)",
    )
    .bind(w.tenant)
    .bind(Uuid::new_v4())
    .bind(store)
    .bind(&tenant_keys)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let mut query = q("returns");
    query.supersedes = Some(vec!["a/1".into(), "a/2".into(), "a/3".into()]);
    assert_eq!(err(query).await.as_deref(), Some("supersedes_limit"));
    assert!(w.queries().is_empty(), "nothing reached /api/query");
    // 2 request keys (one already a tenant key) + 30 = 31 ≤ 32: sent whole, the tenant's keys all there
    let mut query = q("returns");
    query.supersedes = Some(vec!["a/1".into(), "t/k00".into()]);
    assert_eq!(err(query).await, None);
    let sent = w.queries().pop().unwrap().body["supersedes"].clone();
    let mut want = tenant_keys.clone();
    want.push("a/1".into());
    want.sort();
    assert_eq!(sent, json!(want));
}

// ---------------------------------------------------------------------------------------------------------------
// Release vs Draft leaf sources

#[tokio::test]
async fn release_reads_the_snapshot_and_draft_reads_the_cache() {
    let w = world().await;
    let (root, cut_leaves) = w.pack("base", 1, &["faq"]);
    w.kind("faq.x", None, &["base@1"]).await;
    let release = w.cut().await;
    // the pack grows a leaf after the cut (a fake-only change: published packs never change in strata); the draft's
    // cache is rewritten to name only that leaf, so the two designs read different sources
    let later = lock(&w.fake.rag).add_store(w.owner, Some(root), "leaf", "later");
    sqlx::query("DELETE FROM platform_pack_leaf WHERE tenant_id = $1")
        .bind(w.tenant)
        .execute(&w.e.owner)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO platform_pack_leaf (tenant_id, pack_key, version, leaf_id, path) \
         VALUES ($1, 'base', 1, $2, ARRAY['base@1', 'later'])",
    )
    .bind(w.tenant)
    .bind(later)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let (degraded, _) = w
        .search("faq.x", DesignOf::Release(release.clone()), q("returns"))
        .await;
    assert!(degraded.is_empty(), "{degraded:?}");
    let (degraded, _) = w.search("faq.x", DesignOf::Draft, q("returns")).await;
    assert!(degraded.is_empty(), "{degraded:?}");
    let qs = w.queries();
    assert_eq!(qs.len(), 2);
    assert_eq!(
        ids(&qs[0].body, "leaf_ids"),
        cut_leaves,
        "release: release_pack_leaf"
    );
    assert_eq!(
        ids(&qs[1].body, "leaf_ids"),
        [later],
        "draft: platform_pack_leaf"
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Degrade by pin (R11)

#[tokio::test]
async fn excluding_one_of_two_packs_degrades_that_pin_only() {
    let w = world().await;
    let (_, leaf) = w.tenant_leaf("faq").await;
    let (_, a) = w.pack("industry/retail", 1, &["x", "y"]);
    let (_, b) = w.pack("base", 1, &["z"]);
    w.kind("faq.x", Some("faq"), &["base@1", "industry/retail@1"])
        .await;
    // first search caches both packs' leaves
    let (degraded, _) = w.search("faq.x", DesignOf::Draft, q("returns")).await;
    assert!(degraded.is_empty(), "{degraded:?}");
    let mut all = vec![leaf];
    all.extend(&b);
    all.extend(&a);
    assert_eq!(ids(&w.queries()[0].body, "leaf_ids"), all);
    // the tenant excludes the retail pack in strata: its cached leaves now answer 404
    w.exclude("industry");
    let (degraded, _) = w.search("faq.x", DesignOf::Draft, q("returns")).await;
    assert_eq!(degraded, ["industry/retail@1"], "never an error");
    let qs = w.queries();
    assert_eq!(qs.len(), 3, "one retry");
    assert_eq!(ids(&qs[1].body, "leaf_ids"), all);
    let mut kept = vec![leaf];
    kept.extend(&b);
    assert_eq!(
        ids(&qs[2].body, "leaf_ids"),
        kept,
        "ALL the excluded pin's leaves are dropped; the other pack's are still sent"
    );
    // marked: the next search leaves it out up front
    let (degraded, _) = w.search("faq.x", DesignOf::Draft, q("returns")).await;
    assert_eq!(degraded, ["industry/retail@1"]);
    let qs = w.queries();
    assert_eq!(qs.len(), 4);
    assert_eq!(ids(&qs[3].body, "leaf_ids"), kept);
}

#[tokio::test]
async fn a_pin_excluded_before_its_leaves_are_cached_is_degraded_up_front() {
    let w = world().await;
    let (_, a) = w.pack("industry/retail", 1, &["x"]);
    let (_, b) = w.pack("base", 1, &["z"]);
    w.exclude("industry/retail");
    w.kind("faq.x", None, &["base@1", "industry/retail@1"])
        .await;
    let (degraded, _) = w.search("faq.x", DesignOf::Draft, q("returns")).await;
    assert_eq!(degraded, ["industry/retail@1"]);
    let qs = w.queries();
    assert_eq!(qs.len(), 1, "no failing first try");
    assert_eq!(ids(&qs[0].body, "leaf_ids"), b);
    assert!(!qs[0].body.to_string().contains(&a[0].to_string()));
    // the refresh recorded why (the pin view reads it)
    let (s, v) = send(
        &w.e.app,
        get(
            &format!("/api/jobs/{}/knowledge-kinds", w.job.key),
            Some(&w.tok),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let row = v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["stage"] == STAGE && r["kind"] == "faq.x")
        .cloned()
        .expect("the kind row");
    let pin = |p: &str| {
        row["packs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["pin"] == p)
            .cloned()
            .unwrap()
    };
    let retail = pin("industry/retail@1");
    assert_eq!(retail["excluded_by"], "industry/retail", "{retail}");
    assert_eq!(retail["leaves"], 0);
    assert_eq!(retail["tombstoned"], false);
    let base = pin("base@1");
    assert_eq!(base["excluded_by"], Value::Null, "{base}");
    assert_eq!(base["state"], "published");
    assert_eq!(base["leaves"], 1);
    assert_eq!(base["tombstoned"], false);
}

// ---------------------------------------------------------------------------------------------------------------
// Playground: a kind without a root (D91 relaxed)

#[tokio::test]
async fn playground_kind_search_with_two_tenant_roots_is_not_root_required() {
    let w = world().await;
    let (_, leaf) = w.tenant_leaf("faq").await;
    w.tenant_leaf("hr").await;
    let (_, base) = w.pack("base", 1, &["z"]);
    w.kind("faq.x", Some("faq"), &["base@1"]).await;
    let call = |body: Value| {
        send(
            &w.e.app,
            req("POST", "/api/knowledge/query", Some(&w.tok), body),
        )
    };
    // control: the tree playground with two roots and none named
    let (s, b) = call(json!({"query": "returns", "mode": "hybrid"})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "root_required"),
        "{b}"
    );
    assert!(w.queries().is_empty());
    let (s, b) = call(
        json!({"query": "returns", "mode": "hybrid", "job": w.job.key,
                              "stage": STAGE, "kind": "faq.x", "top_k": 3}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["degraded"], json!([]), "{b}");
    assert_eq!(b["searched"], json!(["faq"]), "{b}");
    let qs = w.queries();
    assert_eq!(qs.len(), 1);
    let mut want = vec![leaf];
    want.extend(&base);
    assert_eq!(ids(&qs[0].body, "leaf_ids"), want);
    assert_leaf_ids_only(&qs[0].body);
    assert_eq!(qs[0].body["top_k"], 3);
    assert!(qs[0].body.get("route_mode").is_none(), "{}", qs[0].body);
    // naming only part of a kind is refused before any call
    let (s, b) =
        call(json!({"query": "returns", "mode": "hybrid", "job": w.job.key, "stage": STAGE})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_query"),
        "{b}"
    );
    assert_eq!(w.queries().len(), 1);
    // an unknown kind is 404
    let (s, b) = call(
        json!({"query": "returns", "mode": "hybrid", "job": w.job.key,
                              "stage": STAGE, "kind": "nope"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "kind_not_found"),
        "{b}"
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Citations (PK-13)

#[tokio::test]
async fn answer_source_labels_a_platform_hit_from_a_kind_search() {
    let w = world().await;
    w.pack("base", 1, &["z"]);
    w.kind("faq.x", None, &["base@1"]).await;
    let (chunk, doc) = (Uuid::new_v4(), Uuid::new_v4());
    let long = "x".repeat(400);
    lock(&w.fake.rag).hits = vec![json!({
        "chunk_id": chunk, "document_id": doc, "store_path": ["base", "z"],
        "layer": "platform", "pack": "base@1", "ord": 0, "text": long,
        "page_start": 2, "page_end": 3, "source": "platform", "title": "Hours",
        "scores": {"dense": 0.7}, "source_id": null, "source_run": null,
        "metadata": {"provenance": {"source_url": "https://e.x/h", "license": "CC-BY-4.0",
                                    "retrieved_at": "2026-10-01T00:00:00Z"}, "other": 1}
    })];
    let a = kinds::search(
        &w.e.state,
        w.tenant,
        &w.job,
        STAGE,
        "faq.x",
        &DesignOf::Draft,
        &q("hours"),
    )
    .await
    .expect("search");
    assert_eq!(a.resp.results.len(), 1);
    let src = citations::answer_source(&a.resp.results[0]);
    assert_eq!(
        src,
        json!({
            "chunk_id": chunk, "document_id": doc, "store_path": ["platform:base@1"], "title": "Hours",
            "page": 2, "excerpt": "x".repeat(280), "layer": "platform", "pack": "base@1",
            "provenance": {"source_url": "https://e.x/h", "license": "CC-BY-4.0",
                           "retrieved_at": "2026-10-01T00:00:00Z"}
        })
    );
}

// ---------------------------------------------------------------------------------------------------------------
// PK-12: packs never enter the tenant's model machinery

#[tokio::test]
async fn pack_only_job_gets_no_embedding_pin_and_model_changes_never_name_a_pack() {
    let w = world().await;
    let (root, leaves) = w.pack("base", 1, &["a", "b"]);
    w.kind("faq.x", None, &["base@1"]).await;
    // the pack's leaves are cached by a search
    let (degraded, _) = w.search("faq.x", DesignOf::Draft, q("returns")).await;
    assert!(degraded.is_empty(), "{degraded:?}");
    assert_eq!(
        w.count("SELECT count(*) FROM platform_pack_leaf WHERE tenant_id = $1")
            .await,
        2
    );
    let pins = "SELECT count(*) FROM job_embedding_pin WHERE tenant_id = $1";
    let gens = "SELECT count(*) FROM store_generation WHERE tenant_id = $1";
    assert_eq!(
        w.count(pins).await,
        0,
        "a pack-only job has no embedding pin"
    );
    let wr = &w;
    let change = |from: Option<&'static str>, to: &'static str| async move {
        let mut tx = begin_scoped(&wr.e.state.db, wr.tenant).await.unwrap();
        let id =
            model_changes::on_slot_saved(&mut tx, wr.tenant, Slot::Embed, from, Some(to), "test")
                .await
                .unwrap()
                .expect("the model moves");
        tx.commit().await.unwrap();
        id
    };
    let first = change(None, "e-b").await;
    assert_eq!(w.count(gens).await, 0, "no generation for a pack");
    let parts = |id: Uuid| async move {
        let v: Vec<Value> = sqlx::query_scalar(
            "SELECT stores FROM model_change_job WHERE model_change_id = $1 AND job_id = $2",
        )
        .bind(id)
        .bind(wr.job.id)
        .fetch_all(&wr.e.owner)
        .await
        .unwrap();
        v
    };
    assert!(
        parts(first).await.is_empty(),
        "nothing of the job to rebuild"
    );
    // control: a tenant root makes the same fan-out reach the job, naming only the tenant store
    let (store, sid) = w.tenant_leaf("kb").await;
    let second = change(Some("e-b"), "e-c").await;
    let p = parts(second).await;
    assert_eq!(p.len(), 1, "the fan-out ran: {p:?}");
    let text = p[0].to_string();
    assert!(text.contains(&store.to_string()), "{text}");
    for id in leaves.iter().chain([&root]) {
        assert!(
            !text.contains(&id.to_string()),
            "a pack id in a model change: {text}"
        );
    }
    assert_eq!(
        w.count(
            "SELECT count(*) FROM store_generation g WHERE g.tenant_id = $1 AND g.store_id <> ALL( \
                   ARRAY(SELECT id FROM knowledge_store WHERE tenant_id = $1))"
        )
        .await,
        0,
        "generations exist for tenant stores only"
    );
    // no strata rebuild names a pack store
    for r in w
        .fake
        .rag_requests()
        .iter()
        .chain(w.fake.ingest_requests().iter())
    {
        if r.path.ends_with("/reembed-copy") || r.path.ends_with("/reingest-copy") {
            let all = format!("{} {}", r.path, r.body);
            assert!(all.contains(&sid.to_string()), "{all}");
            for id in leaves.iter().chain([&root]) {
                assert!(!all.contains(&id.to_string()), "{all}");
            }
        }
    }
}
