//! B3 T10: knowledge-kind bindings as drafts (plan §5.5, §0 R9/R10/R11; D54, D68): template materialise, the kinds
//! list with leaf store options, `bindKnowledgeKind` (draft through `design::track`, store and pack pins), the design
//! contract of kind `knowledge`, the platform-pack cache and `platformPacks`, the 0054 pin CHECK, and the kind-scoped
//! leaf-set search (PK-7: always leaf_ids, the tenant leaf then the pinned packs' leaves; Draft vs Live; unreadable
//! packs degrade by pin). Real Postgres 18 (one
//! container for this binary, a fresh database per test) and the in-process strata fakes.
use crate::common;

use crate::common::design_contract;
use crate::common::strata_fake::{self, Recorded, StrataFake, lock};
use crate::common::{
    TestEnv, get, insert_tenant, insert_user, json, person, send, sqlstate, super_admin,
};
use agent_api::design::{self, DesignOf, JobRef};
use agent_api::knowledge::kinds::{self, KindSeed, LeafSet};
use agent_api::knowledge::stores::PACK_UNAVAILABLE_TOTAL;
use agent_api::observability;
use agent_api::platform::templates::TemplatePin;
use agent_api::release::compile::{TemplateCompiler, run_pins_adopted};
use agent_api::strata::StrataStoreId;
use agent_api::strata::rag::QueryReq;
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
    /// The job created from `job/customer_care_agent@v4` (its kinds materialised from the template).
    job: JobRef,
    /// job_designer
    designer: String,
    /// a member without roles
    obs: String,
}

async fn member(e: &TestEnv, tenant: Uuid, role: &str) -> String {
    let sub = format!("u-{role}-{}", Uuid::new_v4().simple());
    insert_user(&e.owner, &sub, tenant, role).await;
    person(&sub, &[])
}

/// A job of `tenant` created by the platform route from the template `support` authors (the creation hook runs).
async fn new_job(e: &TestEnv, tenant: Uuid, key: &str) -> JobRef {
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/tenants/{tenant}/jobs"),
            Some(&super_admin()),
            json!({"template": crate::b3::support::template(e).await, "key": key, "label": key}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let id: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
        .bind(tenant)
        .bind(key)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    JobRef {
        id,
        key: key.to_string(),
    }
}

async fn world() -> W {
    let fake = strata_fake::start().await;
    let tenant = Uuid::new_v4();
    let e = common::env_with(|v| fake.env(&[tenant], v)).await;
    insert_tenant(&e.owner, tenant, &format!("T10 kinds {tenant}")).await;
    let job = new_job(&e, tenant, "care_a").await;
    let designer = member(&e, tenant, "job_designer").await;
    let obs = member(&e, tenant, "tenant_user").await;
    W {
        e,
        fake,
        tenant,
        job,
        designer,
        obs,
    }
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

impl W {
    fn kinds_uri(&self, job: &JobRef) -> String {
        format!("/api/jobs/{}/knowledge-kinds", job.key)
    }

    async fn bind_on(
        &self,
        job: &JobRef,
        stage: &str,
        kind: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let uri = format!("{}/{stage}/{kind}", self.kinds_uri(job));
        send(&self.e.app, json("PUT", &uri, Some(&self.designer), body)).await
    }

    async fn bind(&self, stage: &str, kind: &str, body: Value) -> (StatusCode, Value) {
        self.bind_on(&self.job, stage, kind, body).await
    }

    /// A kind row inserted as the owner (not through the ledger).
    async fn kind_row(
        &self,
        job: &JobRef,
        stage: &str,
        kind: &str,
        store: Option<&str>,
        packs: &[&str],
    ) {
        let packs: Vec<String> = packs.iter().map(|p| p.to_string()).collect();
        sqlx::query(
            "INSERT INTO job_knowledge_kind (tenant_id, job_id, stage, kind, required, description, origin, \
                                             store_path, packs, packs_source, updated_by) \
             VALUES ($1, $2, $3, $4, true, 'test kind', 'base', $5, $6, 'binding', 'test')",
        )
        .bind(self.tenant)
        .bind(job.id)
        .bind(stage)
        .bind(kind)
        .bind(store)
        .bind(packs)
        .execute(&self.e.owner)
        .await
        .unwrap();
    }

    /// A local store inserted as the owner; its id.
    async fn store(&self, key: &str, kind: &str, parent: Option<Uuid>, sid: Option<Uuid>) -> Uuid {
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

    /// A bound leaf `key` (root) in both the fake and the local tree; its strata id.
    async fn bound_leaf(&self, key: &str) -> Uuid {
        let sid = lock(&self.fake.rag).add_store(self.tenant, None, "leaf", key);
        self.store(key, "leaf", None, Some(sid)).await;
        sid
    }

    /// A published platform pack with one leaf (PK-7: a kind searches pack LEAVES); its leaf id. The fake needs a
    /// platform owner for a pack with a real subtree; the agent has no `AGENT_PLATFORM_TENANT_ID` here.
    fn pack_leaf(&self, key: &str, version: i32) -> Uuid {
        let mut g = lock(&self.fake.rag);
        if g.platform_owner.is_none() {
            g.platform_owner = Some(Uuid::new_v4());
        }
        g.add_owned_pack(key, version, &["leaf"], true).1[0]
    }

    fn queries(&self) -> Vec<Recorded> {
        self.fake
            .rag_requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path == "/api/query")
            .collect()
    }

    async fn packs_of(&self, job: &JobRef, stage: &str, kind: &str) -> (Vec<String>, String) {
        sqlx::query_as(
            "SELECT packs, packs_source FROM job_knowledge_kind WHERE job_id = $1 AND stage = $2 AND kind = $3",
        )
        .bind(job.id)
        .bind(stage)
        .bind(kind)
        .fetch_one(&self.e.owner)
        .await
        .unwrap()
    }

    async fn cache(&self) -> Vec<(String, i32, bool, bool)> {
        sqlx::query_as(
            "SELECT pack_key, version, listed, unreadable_at IS NOT NULL FROM platform_pack \
              WHERE tenant_id = $1 ORDER BY pack_key, version",
        )
        .bind(self.tenant)
        .fetch_all(&self.e.owner)
        .await
        .unwrap()
    }

    async fn leaf_set(&self, job: &JobRef, stage: &str, kind: &str, of: DesignOf) -> LeafSet {
        let mut tx = begin_scoped(&self.e.state.db, self.tenant).await.unwrap();
        let s = kinds::leaf_set(&mut tx, &self.e.state.design, job, stage, kind, &of)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        s
    }

    async fn search(&self, job: &JobRef, stage: &str, kind: &str, of: DesignOf) -> Vec<String> {
        let req = QueryReq {
            query: "returns".into(),
            ..QueryReq::default()
        };
        kinds::search(&self.e.state, self.tenant, job, stage, kind, &of, &req)
            .await
            .expect("search")
            .degraded
    }

    async fn draft_rev(&self, job: &JobRef) -> i64 {
        sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
            .bind(job.id)
            .fetch_one(&self.e.owner)
            .await
            .unwrap()
    }

    /// `/api/knowledge/platform-packs` (refreshes the cache).
    async fn refresh(&self) -> Value {
        let (s, b) = send(
            &self.e.app,
            get("/api/knowledge/platform-packs", Some(&self.obs)),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        b
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Materialise

#[tokio::test]
async fn materialise_from_template_is_idempotent() {
    let w = world().await;
    // the creation hook materialised the template's stage_knowledge_kind rows (retail customer service, tag v2)
    let rows: Vec<(String, String, bool, String)> = sqlx::query_as(
        "SELECT stage, kind, required, origin FROM job_knowledge_kind WHERE job_id = $1 ORDER BY stage, kind",
    )
    .bind(w.job.id)
    .fetch_all(&w.e.owner)
    .await
    .unwrap();
    let find = |s: &str, k: &str| rows.iter().find(|r| r.0 == s && r.1 == k).cloned();
    assert_eq!(
        find("support", "policy.returns"),
        Some((
            "support".into(),
            "policy.returns".into(),
            true,
            "base".into()
        )),
        "{rows:?}"
    );
    assert_eq!(
        find("exit", "policy.privacy").map(|r| r.2),
        Some(false),
        "an optional kind: {rows:?}"
    );
    // a tenant binding and origin survive a re-materialise
    sqlx::query(
        "UPDATE job_knowledge_kind SET store_path = 'faq', origin = 'tenant' \
          WHERE job_id = $1 AND stage = 'support' AND kind = 'policy.returns'",
    )
    .bind(w.job.id)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let pins: Vec<TemplatePin> = serde_json::from_value(
        sqlx::query_scalar::<_, Value>("SELECT pins FROM jobs WHERE id = $1")
            .bind(w.job.id)
            .fetch_one(&w.e.owner)
            .await
            .unwrap(),
    )
    .unwrap();
    let sel: agent_model::compile::Selection = serde_json::from_value(
        json!({"profile": "retail_ecommerce", "industry": "retail", "departments": ["industry_dept/retail/customer_service"]}),
    )
    .unwrap();
    let extra = KindSeed {
        stage: "t10".into(),
        kind: "faq.extra".into(),
        required: true,
        tenant_data: false,
        description: "extra".into(),
        packs: Vec::new(),
    };
    for _ in 0..2 {
        let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
        kinds::materialise_from_template(&mut tx, &w.job, &pins, Some(&sel))
            .await
            .unwrap();
        kinds::materialise(&mut tx, &w.job, std::slice::from_ref(&extra))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    let after: Vec<(String, String, Option<String>, String)> = sqlx::query_as(
        "SELECT stage, kind, store_path, origin FROM job_knowledge_kind WHERE job_id = $1 ORDER BY stage, kind",
    )
    .bind(w.job.id)
    .fetch_all(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(after.len(), rows.len() + 1, "idempotent: {after:?}");
    let ret = after
        .iter()
        .find(|r| r.0 == "support" && r.1 == "policy.returns")
        .unwrap();
    assert_eq!(ret.2.as_deref(), Some("faq"), "store_path kept");
    assert_eq!(ret.3, "tenant", "origin kept");
    // no template, no seeds
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    assert!(
        kinds::template_seeds(&mut tx, &pins, None, None)
            .await
            .unwrap()
            .is_empty()
    );
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn materialise_keeps_binding_packs_and_calls_b6b_hook() {
    let w = world().await;
    let seed = KindSeed {
        stage: "support".into(),
        kind: "policy.returns".into(),
        required: true,
        tenant_data: false,
        description: String::new(),
        packs: Vec::new(),
    };
    assert!(
        kinds::template_pack_pins(&seed).is_empty(),
        "the seed's own packs (none here)"
    );
    // a template row follows the template's pins; a binding row is never overwritten
    sqlx::query(
        "UPDATE job_knowledge_kind SET packs = '{stale@1}', packs_source = 'template' \
          WHERE job_id = $1 AND stage = 'support' AND kind = 'policy.returns'",
    )
    .bind(w.job.id)
    .execute(&w.e.owner)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE job_knowledge_kind SET packs = '{base@1}', packs_source = 'binding' \
          WHERE job_id = $1 AND stage = 'support' AND kind = 'policy.warranty'",
    )
    .bind(w.job.id)
    .execute(&w.e.owner)
    .await
    .unwrap();
    // the registered PinsAdopted hook (as approve runs it)
    let pins: Vec<TemplatePin> = serde_json::from_value(
        sqlx::query_scalar::<_, Value>("SELECT pins FROM jobs WHERE id = $1")
            .bind(w.job.id)
            .fetch_one(&w.e.owner)
            .await
            .unwrap(),
    )
    .unwrap();
    let sel: agent_model::compile::Selection =
        serde_json::from_value(design_contract::cuttable_selection()).unwrap();
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    let compiled = TemplateCompiler::compile(w.e.state.compiler.as_ref(), &mut tx, &sel, &pins)
        .await
        .expect("the seed selection compiles");
    assert!(!w.e.state.pins_adopted.is_empty(), "T10 registers its hook");
    run_pins_adopted(
        &w.e.state.pins_adopted,
        &mut tx,
        &w.job,
        &[],
        &pins,
        &compiled,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        w.packs_of(&w.job, "support", "policy.returns").await,
        (vec![], "template".to_string())
    );
    assert_eq!(
        w.packs_of(&w.job, "support", "policy.warranty").await,
        (vec!["base@1".to_string()], "binding".to_string())
    );
}

/// A stack move drops a kind: its row survives (the tenant's store binding is data), demoted with its stale template
/// pins cleared; a kind the new template still asks for is untouched.
#[tokio::test]
async fn materialise_demotes_a_kind_the_new_template_dropped() {
    let w = world().await;
    // A is required and carries the template's pack pin; B is required too
    let a = KindSeed {
        stage: "t10".into(),
        kind: "faq.alpha".into(),
        required: true,
        tenant_data: false,
        description: "alpha".into(),
        packs: vec!["base@1".into()],
    };
    let b = KindSeed {
        stage: "t10".into(),
        kind: "faq.beta".into(),
        required: true,
        tenant_data: true,
        description: "beta".into(),
        packs: Vec::new(),
    };
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    kinds::materialise(&mut tx, &w.job, &[a, b.clone()])
        .await
        .unwrap();
    let before = kinds::row(&mut tx, w.job.id, "t10", "faq.alpha")
        .await
        .unwrap()
        .expect("A's row");
    assert_eq!(
        (before.required, before.packs, before.packs_source),
        (true, vec!["base@1".to_string()], "template".to_string())
    );
    // the new stack asks for B only
    kinds::demote_absent_kinds(&mut tx, &w.job, std::slice::from_ref(&b))
        .await
        .unwrap();
    let after = kinds::row(&mut tx, w.job.id, "t10", "faq.alpha")
        .await
        .unwrap()
        .expect("A's row survives: a kind is demoted, never deleted");
    assert!(!after.required, "the new template does not ask for it");
    assert!(after.packs.is_empty(), "the old template's pins are gone");
    assert_eq!(after.packs_source, "template");
    let kept = kinds::row(&mut tx, w.job.id, "t10", "faq.beta")
        .await
        .unwrap()
        .expect("B's row");
    assert!(kept.required && kept.tenant_data, "B is untouched");
    tx.commit().await.unwrap();
}

/// The tenant's own pins (`packs_source = 'binding'`) survive the kind being dropped by the new template: only the
/// requirement and the template's pins go, and the row and its binding stay. Runs the real template path (the job's
/// own pins and selection), which is what a stack move calls.
#[tokio::test]
async fn materialise_keeps_a_binding_sourced_pack_of_a_dropped_kind() {
    let w = world().await;
    // a kind this job's template does not ask for, pinned by the tenant
    w.kind_row(&w.job, "t10", "faq.binding", None, &["base@1"])
        .await;
    let pins: Vec<TemplatePin> = serde_json::from_value(
        sqlx::query_scalar::<_, Value>("SELECT pins FROM jobs WHERE id = $1")
            .bind(w.job.id)
            .fetch_one(&w.e.owner)
            .await
            .unwrap(),
    )
    .unwrap();
    let sel: agent_model::compile::Selection = serde_json::from_value(
        json!({"profile": "retail_ecommerce", "industry": "retail", "departments": ["industry_dept/retail/customer_service"]}),
    )
    .unwrap();
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    kinds::materialise_from_template(&mut tx, &w.job, &pins, Some(&sel))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (required, packs, source): (bool, Vec<String>, String) = sqlx::query_as(
        "SELECT required, packs, packs_source FROM job_knowledge_kind \
          WHERE job_id = $1 AND stage = 't10' AND kind = 'faq.binding'",
    )
    .bind(w.job.id)
    .fetch_one(&w.e.owner)
    .await
    .expect("the row survives");
    assert!(!required, "the new template does not ask for it");
    assert_eq!(packs, ["base@1"], "the tenant's own pin is kept");
    assert_eq!(source, "binding");
    // and the new template's kinds are materialised as required
    let (required, kind): (bool, String) = sqlx::query_as(
        "SELECT required, kind FROM job_knowledge_kind \
          WHERE job_id = $1 AND stage = 'support' AND kind = 'policy.returns'",
    )
    .bind(w.job.id)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert!(required, "{kind} is required by the template");
}

/// A job with no template (no selection, no pins) seeds nothing: writing "no template" must not demote the job's
/// existing kinds — an empty seed list is not a template that asks for nothing.
#[tokio::test]
async fn materialise_from_template_demotes_nothing_without_a_template() {
    let w = world().await;
    let seed = KindSeed {
        stage: "t10".into(),
        kind: "faq.kept".into(),
        required: true,
        tenant_data: false,
        description: "kept".into(),
        packs: vec!["base@1".into()],
    };
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    kinds::materialise(&mut tx, &w.job, std::slice::from_ref(&seed))
        .await
        .unwrap();
    kinds::materialise_from_template(&mut tx, &w.job, &[], None)
        .await
        .unwrap();
    let row = kinds::row(&mut tx, w.job.id, "t10", "faq.kept")
        .await
        .unwrap()
        .expect("the row");
    assert!(
        row.required,
        "no template is not a template that dropped it"
    );
    assert_eq!(row.packs, ["base@1"], "its template pin is kept");
    tx.commit().await.unwrap();
}

/// A wildcard seed (`*|<kind>`, D107) covers every stage of its kind: a concrete row it names survives the demotion.
#[tokio::test]
async fn demote_keeps_a_concrete_row_a_wildcard_seed_names() {
    let w = world().await;
    let concrete = KindSeed {
        stage: "support".into(),
        kind: "faq.gamma".into(),
        required: true,
        tenant_data: false,
        description: "gamma".into(),
        packs: vec!["base@1".into()],
    };
    let wildcard = KindSeed {
        stage: "*".into(),
        kind: "faq.gamma".into(),
        required: true,
        tenant_data: false,
        description: "gamma".into(),
        packs: Vec::new(),
    };
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    kinds::materialise(&mut tx, &w.job, std::slice::from_ref(&concrete))
        .await
        .unwrap();
    kinds::demote_absent_kinds(&mut tx, &w.job, std::slice::from_ref(&wildcard))
        .await
        .unwrap();
    let row = kinds::row(&mut tx, w.job.id, "support", "faq.gamma")
        .await
        .unwrap()
        .expect("the concrete row");
    assert!(row.required, "the wildcard seed still names it");
    assert_eq!(row.packs, ["base@1"], "its template pin is kept");
    tx.commit().await.unwrap();
}

// ---------------------------------------------------------------------------------------------------------------
// knowledgeKinds

#[tokio::test]
async fn kinds_list_rows_and_leaf_store_options() {
    let w = world().await;
    let (group_sid, leaf_sid) = {
        let mut g = lock(&w.fake.rag);
        let group = g.add_store(w.tenant, None, "router", "faq-policies");
        let leaf = g.add_store(w.tenant, Some(group), "leaf", "policy.returns");
        for _ in 0..2 {
            g.documents.push((
                w.tenant,
                json!({"id": Uuid::new_v4(), "store_id": leaf, "source": "a.pdf", "title": "a",
                       "content_hash": "h", "token_count": 1, "chunk_count": 1, "source_id": null,
                       "source_run": null, "metadata": {}, "created_at": "2026-10-21T10:00:00Z"}),
            ));
        }
        (group, leaf)
    };
    let group = w
        .store("faq-policies", "group", None, Some(group_sid))
        .await;
    w.store("policy.returns", "leaf", Some(group), Some(leaf_sid))
        .await;
    w.store("loose", "leaf", None, None).await;
    w.kind_row(
        &w.job,
        "t10",
        "faq.x",
        Some("faq-policies/policy.returns"),
        &[],
    )
    .await;

    let (s, h, b) = send_raw(&w.e.app, get(&w.kinds_uri(&w.job), Some(&w.obs))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(h.get("x-agent-stale").is_none(), "{h:?}");
    let row = b["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["stage"] == "t10")
        .cloned()
        .expect("the inserted kind");
    assert_eq!(row["kind"], "faq.x");
    assert_eq!(row["store"], "faq-policies/policy.returns");
    assert_eq!(row["required"], true);
    assert_eq!(row["origin"], "base");
    assert_eq!(row["job"], w.job.key);
    assert_eq!(row["packs"], json!([]));
    assert!(
        b["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["stage"] == "support" && r["kind"] == "policy.returns"),
        "template rows are listed: {b}"
    );
    let stores = b["stores"].as_array().unwrap();
    let paths: Vec<&str> = stores.iter().map(|s| s["path"].as_str().unwrap()).collect();
    assert_eq!(
        paths,
        ["faq-policies/policy.returns", "loose"],
        "leaves only, by path"
    );
    assert_eq!(stores[0]["documents"], 2);
    assert_eq!(stores[0]["label"], "policy.returns");
    assert!(stores[0]["updated_at"].is_string());
    assert_eq!(stores[1]["documents"], 0);
}

#[tokio::test]
async fn knowledge_kinds_refreshes_platform_pack_cache() {
    let w = world().await;
    let base = lock(&w.fake.rag).add_pack("base", 1);
    sqlx::query(
        "INSERT INTO platform_pack (tenant_id, pack_key, version, root_id, listed) VALUES ($1, 'gone', 1, $2, true)",
    )
    .bind(w.tenant)
    .bind(Uuid::new_v4())
    .execute(&w.e.owner)
    .await
    .unwrap();
    w.kind_row(&w.job, "t10", "faq.x", None, &["gone@1"]).await;
    let (s, b) = send(&w.e.app, get(&w.kinds_uri(&w.job), Some(&w.obs))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        w.cache().await,
        [
            ("base".to_string(), 1, true, false),
            ("gone".to_string(), 1, false, false)
        ],
        "a cached pack absent from the listing is listed=false"
    );
    let root: Uuid = sqlx::query_scalar(
        "SELECT root_id FROM platform_pack WHERE tenant_id = $1 AND pack_key = 'base'",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(root, base);
    let row = b["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["stage"] == "t10")
        .cloned()
        .unwrap();
    assert_eq!(
        row["packs"],
        // PK T5 widened the pin view (state, excluded_by, tombstoned, leaves); an unlisted pin has no state and no leaves
        json!([{"pin": "gone@1", "listed": false, "unreadable": false, "newer": null,
            "state": null, "excluded_by": null, "tombstoned": false, "leaves": 0}])
    );
}

#[tokio::test]
async fn rag_down_serves_kinds_from_cache_stale() {
    let w = world().await;
    lock(&w.fake.rag).add_pack("base", 1);
    w.refresh().await;
    w.kind_row(&w.job, "t10", "faq.x", None, &["base@1"]).await;
    lock(&w.fake.rag).script(
        "GET",
        "/api/platform/packs",
        503,
        json!({"error": {"code": "unavailable", "message": "down"}}),
    );
    let (s, h, b) = send_raw(&w.e.app, get(&w.kinds_uri(&w.job), Some(&w.obs))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        h.get("x-agent-stale").and_then(|v| v.to_str().ok()),
        Some("strata_rag")
    );
    let row = b["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["stage"] == "t10")
        .cloned()
        .unwrap();
    assert_eq!(row["packs"][0]["pin"], "base@1");
    assert_eq!(row["packs"][0]["listed"], true, "served from the cache");
    assert_eq!(
        w.cache().await,
        [("base".to_string(), 1, true, false)],
        "the cache is untouched"
    );
}

#[tokio::test]
async fn platform_packs_read_maps_listing_and_404_is_empty() {
    let w = world().await;
    let base = {
        let mut g = lock(&w.fake.rag);
        let base = g.add_pack("base", 1);
        g.add_pack("industry/retail", 2);
        base
    };
    let b = w.refresh().await;
    let items = b.as_array().expect("an array");
    assert_eq!(items.len(), 2, "{b}");
    assert_eq!(items[0]["pin"], "base@1");
    assert_eq!(items[0]["key"], "base");
    assert_eq!(items[0]["version"], 1);
    assert_eq!(items[0]["root_id"], json!(base));
    assert_eq!(items[1]["pin"], "industry/retail@2");
    assert_eq!(w.cache().await.len(), 2);
    // platform knowledge off: rag 404 → [] and every cached pack is no longer listed
    lock(&w.fake.rag).platform_off = true;
    let b = w.refresh().await;
    assert_eq!(b, json!([]));
    assert!(
        w.cache().await.iter().all(|c| !c.2),
        "{:?}",
        w.cache().await
    );
}

// ---------------------------------------------------------------------------------------------------------------
// bindKnowledgeKind

#[tokio::test]
async fn bind_unknown_kind_404_kind_not_found() {
    let w = world().await;
    let (s, b) = w.bind("support", "nope", json!({"store": null})).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "kind_not_found");
    // the role is checked first (403 before 404)
    let uri = format!("{}/support/nope", w.kinds_uri(&w.job));
    let (s, b) = send(
        &w.e.app,
        json("PUT", &uri, Some(&w.obs), json!({"store": null})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
}

#[tokio::test]
async fn bind_group_or_unknown_path_422_unknown_store() {
    let w = world().await;
    let group = w.store("faq-policies", "group", None, None).await;
    w.store("policy.returns", "leaf", Some(group), None).await;
    w.kind_row(&w.job, "t10", "faq.x", None, &[]).await;
    for path in ["faq-policies", "nope/x", "policy.returns"] {
        let (s, b) = w.bind("t10", "faq.x", json!({"store": path})).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{path}: {b}");
        assert_eq!(code(&b), "unknown_store", "{path}");
    }
    let (s, b) = w
        .bind(
            "t10",
            "faq.x",
            json!({"store": "faq-policies/policy.returns"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w.bind("t10", "faq.x", json!({"store": 7})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
}

#[tokio::test]
async fn bind_writes_working_row_and_job_draft() {
    let w = world().await;
    let other = new_job(&w.e, w.tenant, "care_b").await;
    let group = w.store("faq-policies", "group", None, None).await;
    w.store("policy.returns", "leaf", Some(group), None).await;
    let (rev_a, rev_b) = (w.draft_rev(&w.job).await, w.draft_rev(&other).await);
    let (s, b) = w
        .bind(
            "support",
            "policy.returns",
            json!({"store": "faq-policies/policy.returns"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["store"], "faq-policies/policy.returns");
    let store: Option<String> = sqlx::query_scalar(
        "SELECT store_path FROM job_knowledge_kind WHERE job_id = $1 AND stage = 'support' AND kind = 'policy.returns'",
    )
    .bind(w.job.id)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(store.as_deref(), Some("faq-policies/policy.returns"));
    let (kind, r, after, label): (String, Value, Option<Value>, String) = sqlx::query_as(
        "SELECT kind, ref, after, label FROM draft_ledger WHERE job_id = $1 AND key = 'kind:support:policy.returns'",
    )
    .bind(w.job.id)
    .fetch_one(&w.e.owner)
    .await
    .expect("a ledger row");
    assert_eq!(kind, "knowledge");
    assert_eq!(r, json!({"stage": "support", "kind": "policy.returns"}));
    assert_eq!(
        after,
        Some(json!({"v": {"store": "faq-policies/policy.returns", "packs": []}}))
    );
    assert_eq!(
        label,
        "policy.returns (support) bound to faq-policies/policy.returns"
    );
    assert_eq!(
        w.draft_rev(&w.job).await,
        rev_a + 1,
        "this job's draft moved"
    );
    assert_eq!(
        w.draft_rev(&other).await,
        rev_b,
        "only this job's draft_rev"
    );
    // unbind
    let (s, b) = w
        .bind("support", "policy.returns", json!({"store": null}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["store"], Value::Null);
}

#[tokio::test]
async fn bind_packs_validates_against_listing() {
    let w = world().await;
    {
        let mut g = lock(&w.fake.rag);
        g.add_pack("base", 1);
        g.add_pack("industry/retail", 2);
    }
    w.kind_row(&w.job, "t10", "faq.x", None, &[]).await;
    let many: Vec<String> = (0..17).map(|i| format!("p{i}@1")).collect();
    for bad in [
        json!(["base"]),
        json!(["base@0"]),
        json!(["base@1", "base@2"]),
        json!(many),
    ] {
        let (s, b) = w.bind("t10", "faq.x", json!({"packs": bad})).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {b}");
        assert_eq!(code(&b), "invalid_pack", "{bad}");
    }
    // a draft or excluded pack is simply absent from the tenant's listing
    let (s, b) = w.bind("t10", "faq.x", json!({"packs": ["draft@1"]})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "unknown_pack");
    let (s, b) = w.bind("t10", "faq.x", json!({"packs": ["base@1"]})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // platform knowledge off (rag 404): nothing is pinnable
    lock(&w.fake.rag).platform_off = true;
    let (s, b) = w
        .bind(
            "t10",
            "faq.x",
            json!({"packs": ["base@1", "industry/retail@2"]}),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "unknown_pack");
}

#[tokio::test]
async fn bind_packs_writes_value_store_and_packs() {
    let w = world().await;
    {
        let mut g = lock(&w.fake.rag);
        g.add_pack("base", 1);
        g.add_pack("industry/retail", 2);
    }
    w.store("faq", "leaf", None, None).await;
    w.kind_row(&w.job, "t10", "faq.x", None, &[]).await;
    let (s, b) = w
        .bind(
            "t10",
            "faq.x",
            json!({"store": "faq", "packs": ["industry/retail@2", "base@1"]}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let pins: Vec<&str> = b["packs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["pin"].as_str().unwrap())
        .collect();
    assert_eq!(pins, ["base@1", "industry/retail@2"], "sorted by key");
    assert_eq!(b["packs_source"], "binding");
    assert_eq!(
        w.packs_of(&w.job, "t10", "faq.x").await,
        (
            vec!["base@1".to_string(), "industry/retail@2".to_string()],
            "binding".to_string()
        )
    );
    let after: Option<Value> = sqlx::query_scalar(
        "SELECT after FROM draft_ledger WHERE job_id = $1 AND key = 'kind:t10:faq.x'",
    )
    .bind(w.job.id)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(
        after,
        Some(json!({"v": {"store": "faq", "packs": ["base@1", "industry/retail@2"]}}))
    );
    // packs absent = keep; store absent = keep
    let (s, b) = w.bind("t10", "faq.x", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["store"], "faq");
    assert_eq!(b["packs"].as_array().unwrap().len(), 2);
    // packs [] = no packs
    let (s, b) = w.bind("t10", "faq.x", json!({"packs": []})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["packs"], json!([]));
}

#[tokio::test]
async fn repin_is_a_draft_change_never_automatic() {
    let w = world().await;
    {
        let mut g = lock(&w.fake.rag);
        g.add_pack("base", 1);
        g.add_pack("base", 2);
    }
    let (s, b) = w
        .bind("support", "policy.returns", json!({"packs": ["base@1"]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // a newer version is shown, never applied
    let (s, b) = send(&w.e.app, get(&w.kinds_uri(&w.job), Some(&w.obs))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let row = b["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["stage"] == "support" && r["kind"] == "policy.returns")
        .cloned()
        .unwrap();
    assert_eq!(row["packs"][0]["pin"], "base@1");
    assert_eq!(row["packs"][0]["newer"], "base@2");
    assert_eq!(
        w.packs_of(&w.job, "support", "policy.returns").await.0,
        ["base@1"]
    );
    // PK-7: the draft's pins, resolved to leaves by a search (no leaves cached yet: unresolved)
    let draft = w
        .leaf_set(&w.job, "support", "policy.returns", DesignOf::Draft)
        .await;
    assert_eq!(draft.pins(), ["base@1"]);
    // live (the baseline) has no pin until a bind is published
    let live = w
        .leaf_set(&w.job, "support", "policy.returns", DesignOf::Live)
        .await;
    assert!(live.pins().is_empty(), "{live:?}");
    // re-pin = an explicit bind, a draft change
    let rev = w.draft_rev(&w.job).await;
    let (s, b) = w
        .bind("support", "policy.returns", json!({"packs": ["base@2"]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(w.draft_rev(&w.job).await, rev + 1);
    let draft = w
        .leaf_set(&w.job, "support", "policy.returns", DesignOf::Draft)
        .await;
    assert_eq!(draft.pins(), ["base@2"]);
    let live = w
        .leaf_set(&w.job, "support", "policy.returns", DesignOf::Live)
        .await;
    assert!(
        live.pins().is_empty(),
        "live is unchanged until publish: {live:?}"
    );
}

#[tokio::test]
async fn retired_pin_kept_on_resave_but_not_addable() {
    let w = world().await;
    {
        let mut g = lock(&w.fake.rag);
        g.add_pack("old", 1);
        g.add_pack("base", 1);
        for p in g.packs.iter_mut().filter(|p| p["key"] == "old") {
            p["retired_at"] = json!("2026-10-20T00:00:00Z");
        }
    }
    w.kind_row(&w.job, "t10", "faq.x", None, &["old@1"]).await;
    w.kind_row(&w.job, "t10", "faq.y", None, &[]).await;
    let (s, b) = w
        .bind("t10", "faq.x", json!({"packs": ["old@1", "base@1"]}))
        .await;
    assert_eq!(s, StatusCode::OK, "a pin already on the row stays: {b}");
    assert_eq!(
        w.packs_of(&w.job, "t10", "faq.x").await.0,
        ["base@1", "old@1"]
    );
    let (s, b) = w.bind("t10", "faq.y", json!({"packs": ["old@1"]})).await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a retired pack is not addable: {b}"
    );
    assert_eq!(code(&b), "unknown_pack");
}

#[tokio::test]
async fn per_job_exclusion_is_absence() {
    let w = world().await;
    let other = new_job(&w.e, w.tenant, "care_b").await;
    lock(&w.fake.rag).add_pack("base", 1);
    let (s, b) = w
        .bind("support", "policy.returns", json!({"packs": ["base@1"]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let a = w
        .leaf_set(&w.job, "support", "policy.returns", DesignOf::Draft)
        .await;
    assert_eq!(a.pins(), ["base@1"]);
    let b_set = w
        .leaf_set(&other, "support", "policy.returns", DesignOf::Draft)
        .await;
    assert!(b_set.pins().is_empty(), "job B does not pin it: {b_set:?}");
}

// ---------------------------------------------------------------------------------------------------------------
// The design contract of kind `knowledge`

/// Both kinds are optional (seed v1): the contract job's required kinds are bound to its fixture leaf after the
/// baseline (`design_contract::bind_required_knowledge`, D110 KH T8), so a write back to their working value would
/// not be a write back to live; an optional kind's working value is its baseline's.
fn samples() -> Vec<(design::DraftRef, Value)> {
    vec![
        (
            kinds::kind_ref("support", "guide.product_usage"),
            json!({"store": null, "packs": ["base@1"]}),
        ),
        (
            kinds::kind_ref("exit", "policy.privacy"),
            json!({"store": "faq/policy", "packs": ["base@1", "industry/retail@2"]}),
        ),
    ]
}

#[tokio::test]
async fn knowledge_design_contract() {
    let e = common::env().await;
    design_contract::run_all(&e, "knowledge", &samples(), Some("unbound")).await;
}

#[tokio::test]
async fn design_kind_knowledge_snapshot_restore() {
    let e = common::env().await;
    let (r, v) = samples().remove(1);
    design_contract::run_release(&e, "knowledge", &r, v).await;
}

// ---------------------------------------------------------------------------------------------------------------
// 0054 CHECK

#[tokio::test]
async fn check_0054_rejects_bad_pin_array() {
    let w = world().await;
    let many: Vec<String> = (0..17).map(|i| format!("p{i}@1")).collect();
    let insert = |kind: &'static str, packs: Vec<String>| {
        sqlx::query(
            "INSERT INTO job_knowledge_kind (tenant_id, job_id, stage, kind, packs, updated_by) \
             VALUES ($1, $2, 't10', $3, $4, 'test')",
        )
        .bind(w.tenant)
        .bind(w.job.id)
        .bind(kind)
        .bind(packs)
        .execute(&w.e.owner)
    };
    let s = |v: &[&str]| v.iter().map(|p| p.to_string()).collect::<Vec<String>>();
    for (kind, packs) in [
        ("k1", s(&["base"])),
        ("k2", s(&["base@0"])),
        ("k3", s(&["Base@1"])),
        ("k4", s(&["base@1", "base@2"])),
        ("k5", many.clone()),
        ("k6", s(&["a/b/c/d/e/f/g@1"])),
    ] {
        let err = insert(kind, packs.clone()).await.unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{packs:?}: {err}");
    }
    insert("ok", s(&["base@1", "industry/retail@2"]))
        .await
        .expect("valid pins");
    insert("ok16", many[..16].to_vec()).await.expect("16 pins");
}

// ---------------------------------------------------------------------------------------------------------------
// The leaf-set query path (R10/R11)

const NO_KNOBS: [&str; 3] = ["supersedes", "platform_penalty", "platform_min_score"];

#[tokio::test]
async fn leaf_set_query_sends_leaf_ids_tenant_leaf_then_pack_leaves() {
    let w = world().await;
    let leaf = w.bound_leaf("faq").await;
    let (base, retail) = (w.pack_leaf("base", 1), w.pack_leaf("industry/retail", 2));
    w.refresh().await;
    w.kind_row(
        &w.job,
        "t10",
        "faq.x",
        Some("faq"),
        &["base@1", "industry/retail@2"],
    )
    .await;
    let degraded = w.search(&w.job, "t10", "faq.x", DesignOf::Draft).await;
    assert!(degraded.is_empty(), "{degraded:?}");
    let q = w.queries();
    assert_eq!(q.len(), 1);
    let body = &q[0].body;
    assert_eq!(body["leaf_ids"], json!([leaf, base, retail]));
    assert!(
        body.get("root_id").is_none() && body.get("root_ids").is_none(),
        "{body}"
    );
    for k in NO_KNOBS {
        assert!(body.get(k).is_none(), "no precedence knob {k}: {body}");
    }
}

#[tokio::test]
async fn leaf_set_tenant_only_sends_leaf_ids() {
    let w = world().await;
    let leaf = w.bound_leaf("faq").await;
    w.kind_row(&w.job, "t10", "faq.x", Some("faq"), &[]).await;
    w.search(&w.job, "t10", "faq.x", DesignOf::Draft).await;
    let q = w.queries();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].body["leaf_ids"], json!([leaf]));
    assert!(q[0].body.get("root_ids").is_none());
}

#[tokio::test]
async fn leaf_set_packs_only_sends_pack_leaves() {
    let w = world().await;
    let base = w.pack_leaf("base", 1);
    w.refresh().await;
    w.kind_row(&w.job, "t10", "faq.x", None, &["base@1"]).await;
    w.search(&w.job, "t10", "faq.x", DesignOf::Draft).await;
    let q = w.queries();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].body["leaf_ids"], json!([base]));
    assert!(q[0].body.get("root_ids").is_none());
}

#[tokio::test]
async fn leaf_set_empty_makes_no_strata_call() {
    let w = world().await;
    w.kind_row(&w.job, "t10", "faq.x", None, &[]).await;
    let degraded = w.search(&w.job, "t10", "faq.x", DesignOf::Draft).await;
    assert!(degraded.is_empty());
    assert!(w.queries().is_empty(), "no strata call");
    // a pin strata cannot resolve has no leaf: no query either (the draft asks strata for its leaves once), and it
    // is reported, never dropped silently
    w.kind_row(&w.job, "t10", "faq.y", None, &["never@1"]).await;
    let degraded = w.search(&w.job, "t10", "faq.y", DesignOf::Draft).await;
    assert_eq!(degraded, ["never@1"]);
    assert!(w.queries().is_empty());
}

#[tokio::test]
async fn leaf_set_draft_vs_live_pins_per_design_of() {
    let w = world().await;
    let base = w.pack_leaf("base", 1);
    let (s, b) = w
        .bind("support", "policy.returns", json!({"packs": ["base@1"]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    w.search(&w.job, "support", "policy.returns", DesignOf::Draft)
        .await;
    let q = w.queries();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].body["leaf_ids"], json!([base]));
    // live = the baseline (never released): the kind is unbound there, so nothing is searched
    w.search(&w.job, "support", "policy.returns", DesignOf::Live)
        .await;
    assert_eq!(w.queries().len(), 1, "live sends nothing");
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

/// R11 (supersedes the older "409 knowledge_pack_unavailable" wording): a pack root strata cannot read degrades the
/// search, marks the cache, counts, and the next search leaves the pin out up front.
#[tokio::test]
async fn pack_root_404_degrades_marks_cache_and_counts() {
    let w = world().await;
    let leaf = w.bound_leaf("faq").await;
    let tag = Uuid::new_v4().simple().to_string();
    let (gone_key, ok_key) = (format!("t10-gone-{tag}"), format!("t10-ok-{tag}"));
    let (gone, ok) = (w.pack_leaf(&gone_key, 1), w.pack_leaf(&ok_key, 1));
    w.refresh().await;
    let (gone_pin, ok_pin) = (format!("{gone_key}@1"), format!("{ok_key}@1"));
    w.kind_row(
        &w.job,
        "t10",
        "faq.x",
        Some("faq"),
        &[gone_pin.as_str(), ok_pin.as_str()],
    )
    .await;
    lock(&w.fake.rag).unreadable.insert(gone);
    let before = pack_unavailable(&gone_pin);
    let degraded = w.search(&w.job, "t10", "faq.x", DesignOf::Draft).await;
    assert_eq!(
        degraded,
        std::slice::from_ref(&gone_pin),
        "never a silent drop"
    );
    assert_eq!(pack_unavailable(&gone_pin), before + 1.0);
    let marked: (bool, bool) = sqlx::query_as(
        "SELECT listed, unreadable_at IS NOT NULL FROM platform_pack WHERE tenant_id = $1 AND pack_key = $2",
    )
    .bind(w.tenant)
    .bind(&gone_key)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(marked, (false, true), "the cache marks the pin unreadable");
    let q = w.queries();
    assert_eq!(q.len(), 2, "one retry");
    assert_eq!(q[0].body["leaf_ids"], json!([leaf, gone, ok]));
    assert_eq!(q[1].body["leaf_ids"], json!([leaf, ok]));
    // the next search leaves it out up front (no failing first try)
    let degraded = w.search(&w.job, "t10", "faq.x", DesignOf::Draft).await;
    assert_eq!(degraded, std::slice::from_ref(&gone_pin));
    let q = w.queries();
    assert_eq!(q.len(), 3);
    assert_eq!(q[2].body["leaf_ids"], json!([leaf, ok]));
    assert_eq!(
        pack_unavailable(&gone_pin),
        before + 1.0,
        "counted once per discovery"
    );
}

/// R11 (review I1) through the B4 entry point: two newly unreadable packs degrade the search (one retry each), both
/// are marked in the cache, and the next search leaves both out up front.
#[tokio::test]
async fn two_unreadable_packs_degrade_and_are_both_marked() {
    let w = world().await;
    let leaf = w.bound_leaf("faq").await;
    let tag = Uuid::new_v4().simple().to_string();
    let keys = [
        format!("t10-a-{tag}"),
        format!("t10-b-{tag}"),
        format!("t10-ok-{tag}"),
    ];
    let (a, b, ok) = (
        w.pack_leaf(&keys[0], 1),
        w.pack_leaf(&keys[1], 1),
        w.pack_leaf(&keys[2], 1),
    );
    w.refresh().await;
    let pins: Vec<String> = keys.iter().map(|k| format!("{k}@1")).collect();
    w.kind_row(
        &w.job,
        "t10",
        "faq.x",
        Some("faq"),
        &[pins[0].as_str(), pins[1].as_str(), pins[2].as_str()],
    )
    .await;
    {
        let mut g = lock(&w.fake.rag);
        g.unreadable.insert(a);
        g.unreadable.insert(b);
    }
    let degraded = w.search(&w.job, "t10", "faq.x", DesignOf::Draft).await;
    assert_eq!(degraded, &pins[..2], "both degraded, never an error");
    let q = w.queries();
    assert_eq!(q.len(), 3, "one retry per unreadable pack");
    assert_eq!(q[2].body["leaf_ids"], json!([leaf, ok]));
    let marked: Vec<(String, bool, bool)> = sqlx::query_as(
        "SELECT pack_key, listed, unreadable_at IS NOT NULL FROM platform_pack \
          WHERE tenant_id = $1 AND pack_key = ANY($2) ORDER BY pack_key",
    )
    .bind(w.tenant)
    .bind(&keys[..2])
    .fetch_all(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(
        marked,
        [
            (keys[0].clone(), false, true),
            (keys[1].clone(), false, true)
        ],
        "every dropped pin is marked"
    );
    let degraded = w.search(&w.job, "t10", "faq.x", DesignOf::Draft).await;
    assert_eq!(degraded, &pins[..2]);
    let q = w.queries();
    assert_eq!(q.len(), 4, "left out up front: no failing first try");
    assert_eq!(q[3].body["leaf_ids"], json!([leaf, ok]));
}

/// R11 (review I1): a pack-only kind whose only pack is unreadable answers empty and degraded, and is marked.
#[tokio::test]
async fn pack_only_kind_with_its_only_pack_unreadable_degrades() {
    let w = world().await;
    let key = format!("t10-only-{}", Uuid::new_v4().simple());
    let pack_leaf = w.pack_leaf(&key, 1);
    w.refresh().await;
    let pin = format!("{key}@1");
    w.kind_row(&w.job, "t10", "faq.x", None, &[pin.as_str()])
        .await;
    lock(&w.fake.rag).unreadable.insert(pack_leaf);
    let degraded = w.search(&w.job, "t10", "faq.x", DesignOf::Draft).await;
    assert_eq!(degraded, std::slice::from_ref(&pin));
    assert_eq!(w.queries().len(), 1, "no call with nothing left to search");
    let degraded = w.search(&w.job, "t10", "faq.x", DesignOf::Draft).await;
    assert_eq!(degraded, std::slice::from_ref(&pin));
    assert_eq!(w.queries().len(), 1, "marked: no call at all");
}

// ---------------------------------------------------------------------------------------------------------------
// The job's pinned generation (review I2): search, coverage and eval read the same strata store

/// The agent store id of `key`.
async fn store_id(w: &W, key: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM knowledge_store WHERE tenant_id = $1 AND key = $2")
        .bind(w.tenant)
        .bind(key)
        .fetch_one(&w.e.owner)
        .await
        .unwrap()
}

/// Root leaf `faq` bound to `old`, its `initial` generation, a ready `reembed` generation on strata root `new`, and
/// `w.job`'s DRAFT pinned to it (its live design, never released, reads the baseline: no pin = `initial`).
async fn pinned_generation(w: &W) -> (Uuid, Uuid, Uuid) {
    let old = w.bound_leaf("faq").await;
    let store = store_id(w, "faq").await;
    let new = lock(&w.fake.rag).add_store(w.tenant, None, "leaf", "faq-g2");
    let initial = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO store_generation (tenant_id, id, store_id, build, state, created_by) \
         VALUES ($1, $2, $3, 'initial', 'ready', 'test')",
    )
    .bind(w.tenant)
    .bind(initial)
    .bind(store)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let g2 = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO store_generation (tenant_id, id, store_id, strata_store_id, embed, build, state, source_id, \
                                       created_by) \
         VALUES ($1, $2, $3, $4, 'e-new', 'reembed', 'ready', $5, 'test')",
    )
    .bind(w.tenant)
    .bind(g2)
    .bind(store)
    .bind(new)
    .bind(initial)
    .execute(&w.e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO job_embedding_pin (tenant_id, job_id, store_key, generation_id, updated_by) \
         VALUES ($1, $2, 'faq', $3, 'test')",
    )
    .bind(w.tenant)
    .bind(w.job.id)
    .bind(g2)
    .execute(&w.e.owner)
    .await
    .unwrap();
    (store, old, new)
}

#[tokio::test]
async fn leaf_set_uses_pinned_generation() {
    let w = world().await;
    let (store, old, new) = pinned_generation(&w).await;
    let (s, b) = w
        .bind("support", "policy.returns", json!({"store": "faq"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let draft = w
        .leaf_set(&w.job, "support", "policy.returns", DesignOf::Draft)
        .await;
    assert_eq!(
        draft.tenant,
        Some(StrataStoreId(new)),
        "the pinned generation, never the store's own binding {old}"
    );
    assert_eq!(draft.tenant_store, Some(store));
    // live = the baseline (never released): the kind is unbound there
    let live = w
        .leaf_set(&w.job, "support", "policy.returns", DesignOf::Live)
        .await;
    assert_eq!((live.tenant, live.tenant_store), (None, None));
    w.search(&w.job, "support", "policy.returns", DesignOf::Draft)
        .await;
    let q = w.queries();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].body["leaf_ids"], json!([new]));
    // with the draft pin gone the job reads the `initial` generation: the store's own binding
    sqlx::query("DELETE FROM job_embedding_pin WHERE job_id = $1")
        .bind(w.job.id)
        .execute(&w.e.owner)
        .await
        .unwrap();
    let draft = w
        .leaf_set(&w.job, "support", "policy.returns", DesignOf::Draft)
        .await;
    assert_eq!(draft.tenant, Some(StrataStoreId(old)));
}

#[tokio::test]
async fn coverage_and_eval_read_the_pinned_generation() {
    let w = world().await;
    let (_, old, new) = pinned_generation(&w).await;
    w.kind_row(&w.job, "t10", "faq.x", Some("faq"), &[]).await;
    let (s, b) = send(
        &w.e.app,
        get(
            &format!("/api/jobs/{}/knowledge/coverage", w.job.key),
            Some(&w.obs),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let docs: Vec<String> = w
        .fake
        .rag_requests()
        .into_iter()
        .filter(|r| r.method == "GET" && r.path.ends_with("/documents"))
        .map(|r| r.path)
        .collect();
    assert_eq!(
        docs,
        [format!("/api/stores/{new}/documents")],
        "coverage reads the draft generation, never {old}"
    );
    // eval: `job` names the job whose pinned draft generation is evaluated; without it the store's own binding
    let km = member(&w.e, w.tenant, "knowledge_manager").await;
    let questions = json!([{"query": "Returns?", "expected_text": "returns"}]);
    let mut roots = Vec::new();
    for body in [
        json!({"job": w.job.key, "questions": questions}),
        json!({"questions": questions}),
    ] {
        let (s, b) = send(
            &w.e.app,
            json("POST", "/api/knowledge/eval/runs", Some(&km), body),
        )
        .await;
        assert_eq!(s, StatusCode::ACCEPTED, "{b}");
        let sent = w
            .fake
            .rag_requests()
            .into_iter()
            .rfind(|r| r.method == "POST" && r.path == "/api/eval")
            .expect("an eval run");
        roots.push(sent.body["root_id"].clone());
        // the run's root comes back as the agent store, generation roots included
        assert_eq!(b["root_id"], json!(store_id(&w, "faq").await), "{b}");
        // the fake runs one eval at a time
        lock(&w.fake.rag).runs.clear();
    }
    assert_eq!(roots, [json!(new), json!(old)]);
    let (s, b) = send(
        &w.e.app,
        json(
            "POST",
            "/api/knowledge/eval/runs",
            Some(&km),
            json!({"job": "no_such_job", "questions": questions}),
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "job_not_found"),
        "{b}"
    );
}

#[tokio::test]
async fn kinds_leaf_counts_read_the_pinned_generation() {
    let w = world().await;
    let (_, old, new) = pinned_generation(&w).await;
    {
        let mut g = lock(&w.fake.rag);
        // one document in the store's own binding, two in the generation the job's draft is pinned to
        for (sid, n) in [(old, 1), (new, 2)] {
            for _ in 0..n {
                g.documents.push((
                    w.tenant,
                    json!({"id": Uuid::new_v4(), "store_id": sid, "source": "a.pdf", "title": "a",
                           "content_hash": "h", "token_count": 1, "chunk_count": 1, "source_id": null,
                           "source_run": null, "metadata": {}, "created_at": "2026-10-21T10:00:00Z"}),
                ));
            }
        }
    }
    let (s, h, b) = send_raw(&w.e.app, get(&w.kinds_uri(&w.job), Some(&w.obs))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(h.get("x-agent-stale").is_none(), "{h:?}");
    let faq = b["stores"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["path"] == "faq")
        .cloned()
        .expect("the faq leaf");
    assert_eq!(
        faq["documents"], 2,
        "the pinned generation {new}, never {old}: {b}"
    );
    let docs: Vec<String> = w
        .fake
        .rag_requests()
        .into_iter()
        .filter(|r| r.method == "GET" && r.path.ends_with("/documents"))
        .map(|r| r.path)
        .collect();
    assert_eq!(docs, [format!("/api/stores/{new}/documents")]);
}

// ---------------------------------------------------------------------------------------------------------------
// Boot backfill (review I4)

async fn kind_count(w: &W, job: &JobRef) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM job_knowledge_kind WHERE job_id = $1")
        .bind(job.id)
        .fetch_one(&w.e.owner)
        .await
        .unwrap()
}

/// Jobs older than B3 have no kinds: the backfill gives them their template's, leaves jobs that have kinds alone,
/// and a second run does nothing.
#[tokio::test]
async fn backfill_materialises_kinds_for_jobs_without_any() {
    let w = world().await;
    let created = kind_count(&w, &w.job).await;
    assert!(created > 0, "the template has kinds");
    let other = new_job(&w.e, w.tenant, "care_b").await;
    sqlx::query(
        "UPDATE job_knowledge_kind SET store_path = 'faq', origin = 'tenant' \
          WHERE job_id = $1 AND stage = 'support' AND kind = 'policy.returns'",
    )
    .bind(other.id)
    .execute(&w.e.owner)
    .await
    .unwrap();
    // `w.job` as a job created before B3
    sqlx::query("DELETE FROM job_knowledge_kind WHERE job_id = $1")
        .bind(w.job.id)
        .execute(&w.e.owner)
        .await
        .unwrap();
    let done = kinds::backfill(&w.e.state.db).await;
    assert_eq!(
        done,
        kinds::Backfilled {
            jobs: 1,
            kinds: usize::try_from(created).unwrap(),
            failed_tenants: 0
        }
    );
    assert_eq!(kind_count(&w, &w.job).await, created);
    let origins: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT origin FROM job_knowledge_kind WHERE job_id = $1")
            .bind(w.job.id)
            .fetch_all(&w.e.owner)
            .await
            .unwrap();
    assert_eq!(origins, ["base"]);
    let kept: Option<String> = sqlx::query_scalar(
        "SELECT store_path FROM job_knowledge_kind \
          WHERE job_id = $1 AND stage = 'support' AND kind = 'policy.returns'",
    )
    .bind(other.id)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(
        kept.as_deref(),
        Some("faq"),
        "a job with kinds is never touched"
    );
    assert_eq!(
        kinds::backfill(&w.e.state.db).await,
        kinds::Backfilled::default(),
        "idempotent"
    );
}

/// A tenant whose job row is held by a serving replica is logged and skipped (bounded by `lock_timeout`), never
/// failing the boot; the next run heals it.
#[tokio::test]
async fn backfill_skips_a_locked_tenant_and_heals_next_run() {
    let w = world().await;
    let created = kind_count(&w, &w.job).await;
    sqlx::query("DELETE FROM job_knowledge_kind WHERE job_id = $1")
        .bind(w.job.id)
        .execute(&w.e.owner)
        .await
        .unwrap();
    let mut held = w.e.owner.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(w.job.id)
        .execute(&mut *held)
        .await
        .unwrap();
    let done = kinds::backfill(&w.e.state.db).await;
    assert_eq!(done.failed_tenants, 1, "{done:?}");
    assert_eq!(kind_count(&w, &w.job).await, 0);
    held.rollback().await.unwrap();
    let done = kinds::backfill(&w.e.state.db).await;
    assert_eq!((done.jobs, done.failed_tenants), (1, 0), "{done:?}");
    assert_eq!(kind_count(&w, &w.job).await, created);
}
