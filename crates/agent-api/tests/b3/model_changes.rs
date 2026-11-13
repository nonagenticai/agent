//! B3 T4a: embeddings (and every knowledge model) per job — store generations, `embedding_pin`, model changes (D83,
//! D92, D93; plan §4 0056, §5.8, §0 R7). A slot save fans a model change out to the jobs pinning the old model; a
//! person's click rebuilds into a NEW strata root (rag `reembed-copy`, `copy_vectors` for rerank/chat, ingest
//! `reingest-copy` for ocr/vision/transcribe) with its own store bindings; a generation is ready only when strata
//! says `done`; accept writes the job's `embedding_pin` draft; each job's Production keeps its old generation until
//! that job publishes; publish retires what nothing uses (`store_delete`). Real Postgres 18 (one container for this
//! binary, a fresh database per test), the strata fakes and a mock provider for the slot probe.
use crate::common;

use std::collections::HashMap;

use crate::common::design_contract::{
    self, ContractJob, TEMPLATE, contract_job, cuttable_selection,
};
use crate::common::strata_fake::{self, Recorded, StrataFake, lock};
use crate::common::{TestEnv, get, insert_tenant, insert_user, json, person, send, super_admin};
use agent_api::audit::Actor;
use agent_api::connectors::transport::Transport;
use agent_api::design::{DesignOf, DraftRef};
use agent_api::knowledge::generations;
use agent_api::knowledge::model_changes::{self, SweepReport};
use agent_api::knowledge::targets::{pin_value, store_ref};
use agent_api::netguard::Policy;
use agent_api::provision::senders::store_delete;
use agent_api::provision::{self, drain_once};
use agent_api::strata::StrataStoreId;
use agent_api::tenancy::begin_scoped;
use axum::http::StatusCode;
use axum::routing::get as route_get;
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{Value, json};
use uuid::Uuid;

/// The key every slot is saved with (the mock provider accepts any).
const KEY: &str = "sk-mc-key-5d1e77";

fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
    vars.insert(
        "PROVIDER_SECRET_KEYS".to_string(),
        format!(r#"{{"m1":"{key}"}}"#),
    );
    vars.insert("PROVIDER_SECRET_KEY_CURRENT".to_string(), "m1".to_string());
}

async fn models() -> Json<Value> {
    Json(json!({"data": [{"id": "any-model"}]}))
}

/// The mock provider a slot save probes: `http://127.0.0.1:<port>/ok`.
async fn provider() -> String {
    let app = Router::new().route("/ok/models", route_get(models));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}/ok")
}

struct World {
    e: TestEnv,
    /// The API with the loopback netguard policy (the slot probe reaches the mock provider).
    app: Router,
    fake: StrataFake,
    prov: String,
    tenant: Uuid,
    /// integrations_manager, knowledge_manager, tenant_admin (releases).
    im: String,
    km: String,
    rm: String,
}

async fn member(e: &TestEnv, tenant: Uuid, role: &str) -> String {
    let sub = format!("u-{role}-{}", Uuid::new_v4().simple());
    insert_user(&e.owner, &sub, tenant, role).await;
    person(&sub, &[])
}

async fn world() -> World {
    let fake = strata_fake::start().await;
    let prov = provider().await;
    let tenant = Uuid::new_v4();
    let e = common::env_with(|v| {
        with_keys(v);
        fake.env(&[tenant], v);
    })
    .await;
    insert_tenant(&e.owner, tenant, &format!("T4a {tenant}")).await;
    let mut st = e.state.clone();
    st.connectors = Transport::new(Policy {
        allow_loopback: true,
        private_targets: vec![],
    });
    // every store a job binds joins its model changes, so these jobs bind none: the publish gate runs without the
    // knowledge contributor (D110 is covered by kh::t8_readiness and b3::readiness)
    let mut r = agent_api::readiness::Readiness::default();
    agent_api::release::readiness_rules::register(&mut r);
    agent_api::connectors::readiness::register(&mut r);
    r.register(std::sync::Arc::new(
        agent_api::knowledge::readiness::ProvidersReadiness,
    ));
    st.readiness = std::sync::Arc::new(r);
    let app = agent_api::router(st);
    let im = member(&e, tenant, "integrations_manager").await;
    let km = member(&e, tenant, "knowledge_manager").await;
    let rm = member(&e, tenant, "tenant_admin").await;
    World {
        e,
        app,
        fake,
        prov,
        tenant,
        im,
        km,
        rm,
    }
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

/// The UI kind a slot is saved with.
fn kind_of(slot: &str) -> &'static str {
    match slot {
        "rerank" => "cohere",
        "ocr" => "mistral_ocr",
        _ => "openai_compatible",
    }
}

/// One generation row as the tests read it.
#[derive(Debug, sqlx::FromRow)]
struct Gen {
    id: Uuid,
    build: String,
    state: String,
    strata_store_id: Option<Uuid>,
    embed: Option<String>,
    provider_ref: Value,
    error: Option<String>,
}

/// One `model_change` row.
#[derive(Debug, sqlx::FromRow)]
struct ChangeRow {
    scope: String,
    slot: String,
    from_model: Option<String>,
    to_model: Option<String>,
    rebuild: String,
}

impl World {
    async fn call(&self, tok: &str, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        send(&self.app, json(method, uri, Some(tok), body)).await
    }

    /// Save `slot` with `model` at its current version.
    async fn save_slot(&self, slot: &str, model: &str) {
        let (s, list) = send(&self.app, get("/api/providers/slots", Some(&self.im))).await;
        assert_eq!(s, StatusCode::OK, "{list}");
        let version = list
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["slot"] == slot)
            .map_or(0, |x| x["version"].as_i64().unwrap_or(0));
        let body = json!({"kind": kind_of(slot), "base_url": self.prov, "model": model, "secret": KEY,
                          "expected_version": version});
        let (s, b) = self
            .call(
                &self.im,
                "PUT",
                &format!("/api/providers/slots/{slot}"),
                body,
            )
            .await;
        assert_eq!(s, StatusCode::OK, "save {slot}: {b}");
    }

    /// Drain the outbox until nothing is due (store_ensure binds the stores).
    async fn drain(&self) {
        for _ in 0..10 {
            if drain_once(&self.e.state).await.claimed == 0 {
                return;
            }
        }
        panic!("the outbox never drained");
    }

    /// A store created through the API and bound in strata (drained); its id.
    async fn store(&self, key: &str, kind: &str, parent: Option<Uuid>) -> Uuid {
        let (s, b) = self
            .call(
                &self.km,
                "POST",
                "/api/knowledge/stores",
                json!({"key": key, "label": format!("{key} label"), "parent_id": parent, "kind": kind}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        self.drain().await;
        Uuid::parse_str(b["id"].as_str().unwrap()).unwrap()
    }

    async fn bound(&self, store: Uuid) -> Uuid {
        let sid: Option<Uuid> =
            sqlx::query_scalar("SELECT strata_store_id FROM knowledge_store WHERE id = $1")
                .bind(store)
                .fetch_one(&self.e.owner)
                .await
                .unwrap();
        sid.expect("the store is bound")
    }

    /// A job created through the platform route with a selection that cuts. A tenant holds one job per key, so each
    /// job of a world gets its own key (`desk_<n>`) from the same template.
    async fn job(&self) -> ContractJob {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE tenant_id = $1")
            .bind(self.tenant)
            .fetch_one(&self.e.owner)
            .await
            .unwrap();
        let key = format!("desk_{n}");
        let (s, b) = send(
            &self.e.app,
            json(
                "POST",
                &format!("/api/tenants/{}/jobs", self.tenant),
                Some(&super_admin()),
                json!({"template": format!("{}@v{}", TEMPLATE.0, TEMPLATE.1), "key": key,
                       "label": format!("Desk {n}")}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        let id: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
            .bind(self.tenant)
            .bind(&key)
            .fetch_one(&self.e.owner)
            .await
            .unwrap();
        let f = contract_job(self.tenant, id, &key);
        design_contract::write(
            &self.e,
            &f,
            "selection",
            &DraftRef::new(),
            Some(cuttable_selection()),
        )
        .await;
        design_contract::route_required_prod_ops(&self.e, &f).await;
        f
    }

    /// Approve and apply the job's draft.
    async fn publish(&self, f: &ContractJob) {
        let job = &f.job.job_key;
        let (s, b) = send(
            &self.app,
            get(&format!("/api/jobs/{job}/target"), Some(&self.rm)),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let body = json!({"apply": true, "selection_seen_at": b["selection_updated_at"].clone()});
        let (s, b) = self
            .call(&self.rm, "POST", &format!("/api/jobs/{job}/releases"), body)
            .await;
        assert!(
            s == StatusCode::CREATED || s == StatusCode::OK,
            "publish: {s}: {b}"
        );
        assert_eq!(b["applied"], true, "{b}");
    }

    /// The newest model change of `slot`.
    async fn change(&self, slot: &str) -> Uuid {
        sqlx::query_scalar(
            "SELECT id FROM model_change WHERE tenant_id = $1 AND slot = $2 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(self.tenant)
        .bind(slot)
        .fetch_one(&self.e.owner)
        .await
        .unwrap()
    }

    async fn job_state(&self, change: Uuid, f: &ContractJob) -> Option<String> {
        sqlx::query_scalar(
            "SELECT state FROM model_change_job WHERE model_change_id = $1 AND job_id = $2",
        )
        .bind(change)
        .bind(f.job.job_id)
        .fetch_optional(&self.e.owner)
        .await
        .unwrap()
    }

    async fn reembed(&self, change: Uuid, f: &ContractJob) -> (StatusCode, Value) {
        let uri = format!(
            "/api/knowledge/model-changes/{change}/jobs/{}/reembed",
            f.job.job_key
        );
        self.call(&self.km, "POST", &uri, json!({})).await
    }

    async fn accept(&self, change: Uuid, f: &ContractJob, ack: bool) -> (StatusCode, Value) {
        let q = if ack { "?acknowledge_missing=true" } else { "" };
        let uri = format!(
            "/api/knowledge/model-changes/{change}/jobs/{}/accept{q}",
            f.job.job_key
        );
        self.call(&self.km, "POST", &uri, json!({})).await
    }

    /// `modelChange`, and the part of job `f` in it.
    async fn view(&self, change: Uuid, f: &ContractJob) -> Value {
        let (s, b) = send(
            &self.app,
            get(
                &format!("/api/knowledge/model-changes/{change}"),
                Some(&self.km),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        b["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["job"] == f.job.job_key.as_str())
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn rag(&self, method: &str, path_prefix: &str) -> Vec<Recorded> {
        self.fake
            .rag_requests()
            .into_iter()
            .filter(|r| r.method == method && r.path.starts_with(path_prefix))
            .collect()
    }

    fn ingest(&self, method: &str, path_prefix: &str) -> Vec<Recorded> {
        self.fake
            .ingest_requests()
            .into_iter()
            .filter(|r| r.method == method && r.path.starts_with(path_prefix))
            .collect()
    }

    /// Every rag re-embed copy job → `state` (`done` also reports `chunks`).
    fn finish_rag(&self, state: &str, error: Option<&str>) {
        let mut g = lock(&self.fake.rag);
        for (_, j) in g.reembed_jobs.iter_mut() {
            j["state"] = json!(state);
            j["chunks_done"] = json!(7);
            j["chunks_total"] = json!(7);
            if let Some(e) = error {
                j["error"] = json!(e);
            }
        }
    }

    /// The strata root a fake store of this tenant named `name` (the newest).
    fn fake_store(&self, name_prefix: &str, parent: Option<Uuid>) -> Uuid {
        lock(&self.fake.rag)
            .stores
            .iter()
            .rev()
            .find(|s| {
                s.tenant == self.tenant && s.parent_id == parent && s.name.starts_with(name_prefix)
            })
            .map(|s| s.id)
            .expect("a fake store")
    }

    /// The embed slot's view.
    async fn embed_slot(&self) -> Value {
        let (s, b) = send(&self.app, get("/api/providers/slots", Some(&self.im))).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        b.as_array()
            .unwrap()
            .iter()
            .find(|x| x["slot"] == "embed")
            .cloned()
            .unwrap()
    }

    /// The job's readiness board of its draft, as text (its gaps carry their keys; `release-readiness` items do not).
    async fn readiness(&self, f: &ContractJob) -> String {
        let uri = format!("/api/jobs/{}/readiness/board?of=draft", f.job.job_key);
        let (s, b) = send(&self.app, get(&uri, Some(&self.rm))).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        b.to_string()
    }

    /// A ready rebuilt generation of root `store` running `embed` (made from `source`); its id.
    async fn ready_generation(&self, store: Uuid, source: Uuid, embed: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO store_generation (tenant_id, id, store_id, strata_store_id, embed, build, state, source_id, \
                                           created_by) \
             VALUES ($1, $2, $3, $4, $5, 'reembed', 'ready', $6, 'test')",
        )
        .bind(self.tenant)
        .bind(id)
        .bind(store)
        .bind(Uuid::new_v4())
        .bind(embed)
        .bind(source)
        .execute(&self.e.owner)
        .await
        .unwrap();
        id
    }

    async fn generations(&self, store: Uuid) -> Vec<Gen> {
        sqlx::query_as(
            "SELECT id, build, state, strata_store_id, embed, provider_ref, error FROM store_generation \
              WHERE store_id = $1 ORDER BY created_at, id",
        )
        .bind(store)
        .fetch_all(&self.e.owner)
        .await
        .unwrap()
    }

    /// The strata store `f` uses for agent store `store` in its design `of`.
    async fn strata_for(
        &self,
        f: &ContractJob,
        store: Uuid,
        of: DesignOf,
    ) -> Option<StrataStoreId> {
        let mut tx = begin_scoped(&self.e.state.db, self.tenant).await.unwrap();
        let v = generations::strata_for(&mut tx, &f.job_ref(), store, &of)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        v
    }

    async fn draft_rev(&self, f: &ContractJob) -> i64 {
        sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
            .bind(f.job.job_id)
            .fetch_one(&self.e.owner)
            .await
            .unwrap()
    }

    async fn count(&self, sql: &'static str, id: Uuid) -> i64 {
        sqlx::query_scalar(sql)
            .bind(id)
            .fetch_one(&self.e.owner)
            .await
            .unwrap()
    }
}

/// embed `e-a` saved, two jobs, root leaf `kb` bound, then embed `e-b` saved: (world, jobs, store, change).
async fn embed_change() -> (World, ContractJob, ContractJob, Uuid, Uuid) {
    let w = world().await;
    w.save_slot("embed", "e-a").await;
    let a = w.job().await;
    let b = w.job().await;
    let kb = w.store("kb", "leaf", None).await;
    w.save_slot("embed", "e-b").await;
    let ch = w.change("embed").await;
    (w, a, b, kb, ch)
}

// ---- generations and fan-out ----------------------------------------------------------------------------------------

#[tokio::test]
async fn store_create_makes_generation_one_and_pins_every_using_job() {
    let w = world().await;
    // the jobs exist before the save, so e-a is a change of their drafts (a job created later takes it into its
    // baseline, and its publish pushes nothing)
    let a = w.job().await;
    let b = w.job().await;
    w.save_slot("embed", "e-a").await;
    let kb = w.store("kb", "leaf", None).await;
    let gens = w.generations(kb).await;
    assert_eq!(gens.len(), 1, "{gens:?}");
    assert_eq!(gens[0].build, "initial");
    assert_eq!(gens[0].state, "ready");
    // review I3: e-a is only a draft, strata embeds with its deployment default
    assert_eq!(gens[0].embed, None, "the model strata runs, not the draft");
    assert_eq!(gens[0].provider_ref["embed"]["source"], "default");
    assert_eq!(
        gens[0].strata_store_id, None,
        "generation 1 is the store's own binding"
    );
    let sid = StrataStoreId(w.bound(kb).await);
    for f in [&a, &b] {
        for of in [DesignOf::Draft, DesignOf::Live] {
            assert_eq!(w.strata_for(f, kb, of.clone()).await, Some(sid), "{of:?}");
        }
    }
    // once a job published e-a and strata runs it, a new root records it
    w.publish(&a).await;
    w.drain().await;
    let kb2 = w.store("kb2", "leaf", None).await;
    let g2 = w.generations(kb2).await;
    assert_eq!(g2[0].embed.as_deref(), Some("e-a"), "{g2:?}");
    assert_eq!(g2[0].provider_ref["embed"]["source"], "tenant");
    // a newer model saved but not published changes nothing strata runs
    w.save_slot("embed", "e-b").await;
    let kb3 = w.store("kb3", "leaf", None).await;
    assert_eq!(w.generations(kb3).await[0].embed.as_deref(), Some("e-a"));
    // a child store is not a root: no generation of its own
    let g = w.store("grp", "group", None).await;
    let leaf = w.store("faq", "leaf", Some(g)).await;
    assert!(w.generations(leaf).await.is_empty());
    assert_eq!(w.generations(g).await.len(), 1);
}

#[tokio::test]
async fn save_embed_creates_model_change_and_fans_out_to_pinning_jobs() {
    let w = world().await;
    w.save_slot("embed", "e-a").await;
    let a = w.job().await;
    let b = w.job().await;
    let kb = w.store("kb", "leaf", None).await;
    let initial = w.generations(kb).await[0].id;
    // job b already runs another model: a ready generation of `e-x`, pinned in its draft
    let gx = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO store_generation (tenant_id, id, store_id, strata_store_id, embed, build, state, source_id, \
                                       created_by) \
         VALUES ($1, $2, $3, $4, 'e-x', 'reembed', 'ready', $5, 'test')",
    )
    .bind(w.tenant)
    .bind(gx)
    .bind(kb)
    .bind(Uuid::new_v4())
    .bind(initial)
    .execute(&w.e.owner)
    .await
    .unwrap();
    design_contract::write(
        &w.e,
        &b,
        "embedding_pin",
        &store_ref("kb"),
        Some(pin_value("kb", gx, &json!({"embed": "e-x"}))),
    )
    .await;

    // job c already runs the new model: a ready generation of `e-b`, pinned in its draft
    let c = w.job().await;
    let gb = w.ready_generation(kb, initial, "e-b").await;
    design_contract::write(
        &w.e,
        &c,
        "embedding_pin",
        &store_ref("kb"),
        Some(pin_value("kb", gb, &json!({"embed": "e-b"}))),
    )
    .await;

    w.save_slot("embed", "e-b").await;
    let ch = w.change("embed").await;
    let row: ChangeRow = sqlx::query_as(
        "SELECT scope, slot, from_model, to_model, rebuild FROM model_change WHERE id = $1",
    )
    .bind(ch)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(
        (row.scope.as_str(), row.slot.as_str(), row.rebuild.as_str()),
        ("tenant", "embed", "reembed")
    );
    assert_eq!(row.from_model.as_deref(), Some("e-a"));
    assert_eq!(row.to_model.as_deref(), Some("e-b"));
    assert_eq!(w.job_state(ch, &a).await.as_deref(), Some("pending"));
    // review I1: every job whose pinned generation runs another model than e-b, whatever the working row said
    assert_eq!(
        w.job_state(ch, &b).await.as_deref(),
        Some("pending"),
        "job b runs e-x"
    );
    assert_eq!(
        w.view(ch, &b).await["stores"][0]["from_generation"]["id"],
        json!(gx)
    );
    assert_eq!(w.job_state(ch, &c).await, None, "job c already runs e-b");
    let part = w.view(ch, &a).await;
    assert_eq!(part["stores"][0]["store"], "kb");
    assert_eq!(part["stores"][0]["from_generation"]["id"], json!(initial));
    assert_eq!(part["stores"][0]["to_generation"], Value::Null);
}

#[tokio::test]
async fn save_embed_pushes_nothing_to_tenant_providers() {
    let (w, a, _b, _kb, ch) = embed_change().await;
    assert_eq!(w.job_state(ch, &a).await.as_deref(), Some("pending"));
    assert!(
        w.rag("PUT", "/api/tenant/providers").is_empty(),
        "a save writes nothing to strata's tenant row"
    );
    assert!(w.rag("PUT", "/api/stores/").is_empty());
    assert!(w.rag("POST", "/api/stores/").is_empty(), "no rebuild");
    let pushes = w
        .count(
            "SELECT count(*) FROM provision_outbox WHERE tenant_id = $1 AND op = 'connections_put'",
            w.tenant,
        )
        .await;
    assert_eq!(pushes, 0);
}

#[tokio::test]
async fn slot_save_moves_no_embedding_pin() {
    let (w, a, b, kb, _ch) = embed_change().await;
    let pins = w
        .count(
            "SELECT count(*) FROM job_embedding_pin WHERE tenant_id = $1",
            w.tenant,
        )
        .await;
    assert_eq!(pins, 0);
    let ledger = w
        .count(
            "SELECT count(*) FROM draft_ledger WHERE tenant_id = $1 AND kind = 'embedding_pin'",
            w.tenant,
        )
        .await;
    assert_eq!(ledger, 0, "a slot save writes no job's model pin");
    let sid = StrataStoreId(w.bound(kb).await);
    for f in [&a, &b] {
        assert_eq!(w.strata_for(f, kb, DesignOf::Draft).await, Some(sid));
    }
}

#[tokio::test]
async fn no_timer_starts_a_reembed() {
    let (w, a, _b, _kb, ch) = embed_change().await;
    for _ in 0..3 {
        drain_once(&w.e.state).await;
    }
    assert!(
        w.rag("POST", "/api/stores").len() <= 1,
        "only store_ensure created a store"
    );
    assert!(
        w.fake
            .rag_requests()
            .iter()
            .all(|r| !r.path.ends_with("/reembed-copy")),
        "nothing but a person's click starts a rebuild"
    );
    assert_eq!(w.job_state(ch, &a).await.as_deref(), Some("pending"));
}

#[tokio::test]
async fn save_chat_creates_copy_change_and_fans_out() {
    let w = world().await;
    w.save_slot("chat", "c-a").await;
    let a = w.job().await;
    w.store("kb", "leaf", None).await;
    w.save_slot("chat", "c-b").await;
    let ch = w.change("chat").await;
    let rebuild: String = sqlx::query_scalar("SELECT rebuild FROM model_change WHERE id = $1")
        .bind(ch)
        .fetch_one(&w.e.owner)
        .await
        .unwrap();
    assert_eq!(rebuild, "copy");
    assert_eq!(w.job_state(ch, &a).await.as_deref(), Some("pending"));
}

#[tokio::test]
async fn save_ocr_creates_reingest_change() {
    let w = world().await;
    w.save_slot("ocr", "o-a").await;
    let a = w.job().await;
    w.store("kb", "leaf", None).await;
    w.save_slot("ocr", "o-b").await;
    let ch = w.change("ocr").await;
    let rebuild: String = sqlx::query_scalar("SELECT rebuild FROM model_change WHERE id = $1")
        .bind(ch)
        .fetch_one(&w.e.owner)
        .await
        .unwrap();
    assert_eq!(rebuild, "reingest");
    assert_eq!(w.job_state(ch, &a).await.as_deref(), Some("pending"));
}

// review I1: the fan-out reads what the jobs' generations run, and a newer save supersedes the open change
#[tokio::test]
async fn second_save_before_publish_supersedes_and_reaches_jobs_on_the_old_model() {
    let (w, a, b, kb, ch1) = embed_change().await;
    let initial = w.generations(kb).await[0].id;
    w.save_slot("embed", "e-c").await;
    let ch2 = w.change("embed").await;
    assert_ne!(ch1, ch2);
    for f in [&a, &b] {
        assert_eq!(w.job_state(ch1, f).await.as_deref(), Some("dismissed"));
        assert_eq!(
            w.job_state(ch2, f).await.as_deref(),
            Some("pending"),
            "the jobs still run the published model, not e-b"
        );
    }
    let row: ChangeRow = sqlx::query_as(
        "SELECT scope, slot, from_model, to_model, rebuild FROM model_change WHERE id = $1",
    )
    .bind(ch2)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(
        (row.from_model.as_deref(), row.to_model.as_deref()),
        (Some("e-b"), Some("e-c"))
    );
    assert_eq!(
        w.view(ch2, &a).await["stores"][0]["from_generation"]["id"],
        json!(initial)
    );
    // the superseded change can be neither rebuilt nor accepted
    let (s, v) = w.reembed(ch1, &a).await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::CONFLICT, "change_superseded"),
        "{v}"
    );
    let (s, v) = w.accept(ch1, &a, false).await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::CONFLICT, "change_superseded"),
        "{v}"
    );
    // the newer one builds e-c
    let (s, v) = w.reembed(ch2, &a).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let new = w.fake_store("kb.gen-", None);
    let bind = w.rag("PUT", &format!("/api/stores/{new}/providers"));
    assert_eq!(bind[0].body["embed"]["model"], "e-c");
}

// review I3: a root or a job that appears after a change joins it (on the next read or sweep)
#[tokio::test]
async fn a_root_and_a_job_created_after_a_change_join_it() {
    let (w, a, _b, _kb, ch) = embed_change().await;
    w.store("kb2", "leaf", None).await;
    let c = w.job().await;
    let keys = |part: &Value| {
        let mut k: Vec<String> = part["stores"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["store"].as_str().map(str::to_string))
            .collect();
        k.sort();
        k
    };
    let part = w.view(ch, &a).await;
    assert_eq!(keys(&part), ["kb", "kb2"], "{part}");
    assert_eq!(part["state"], "pending");
    assert_eq!(w.job_state(ch, &c).await.as_deref(), Some("pending"));
    assert_eq!(keys(&w.view(ch, &c).await), ["kb", "kb2"]);
    // idempotent
    assert_eq!(keys(&w.view(ch, &a).await), ["kb", "kb2"]);
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    let n = model_changes::catch_up(&mut tx, w.tenant, "test")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(n, 0, "nothing new to fan out");
}

// review I6: a rebuild that claimed its generation and died before strata answered does not strand the job
#[tokio::test]
async fn sweeper_fails_a_stranded_rebuild_and_queues_its_root_for_deletion() {
    let (w, a, _b, kb, ch) = embed_change().await;
    let initial = w.generations(kb).await[0].id;
    let gid = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO store_generation (tenant_id, id, store_id, embed, build, state, source_id, created_by, \
                                       created_at) \
         VALUES ($1, $2, $3, 'e-b', 'reembed', 'building', $4, 'test', now() - interval '1 hour')",
    )
    .bind(w.tenant)
    .bind(gid)
    .bind(kb)
    .bind(initial)
    .execute(&w.e.owner)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE model_change_job SET state = 'reembedding', stores = $3 WHERE model_change_id = $1 AND job_id = $2",
    )
    .bind(ch)
    .bind(a.job.job_id)
    .bind(json!([{"store_id": kb, "from_generation": initial, "to_generation": gid}]))
    .execute(&w.e.owner)
    .await
    .unwrap();
    // the strata root that rebuild had already made
    let short: String = gid.simple().to_string().chars().take(8).collect();
    let orphan = lock(&w.fake.rag).add_store(w.tenant, None, "leaf", &format!("kb.gen-{short}"));
    let (s, v) = w.reembed(ch, &a).await;
    assert_eq!(
        (s, code(&v)),
        (StatusCode::CONFLICT, "already_reembedding"),
        "stranded: {v}"
    );

    let mut r = SweepReport::default();
    model_changes::sweep_tenant(&w.e.state, w.tenant, &mut r)
        .await
        .unwrap();
    assert_eq!((r.stranded, r.orphans), (1, 1), "{r:?}");
    let g = w
        .generations(kb)
        .await
        .into_iter()
        .find(|g| g.id == gid)
        .unwrap();
    assert_eq!(g.state, "failed");
    let queued = w
        .count(
            "SELECT count(*) FROM provision_outbox WHERE op = 'store_delete' AND status = 'pending' \
               AND payload->>'strata_store_id' = $1::text",
            orphan,
        )
        .await;
    assert_eq!(queued, 1, "its strata root is deleted");
    assert_eq!(w.job_state(ch, &a).await.as_deref(), Some("pending"));
    assert_eq!(
        w.view(ch, &a).await["stores"][0]["to_generation"],
        Value::Null
    );
    // the person starts it again; a second sweep finds nothing
    let (s, v) = w.reembed(ch, &a).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let mut r = SweepReport::default();
    model_changes::sweep_tenant(&w.e.state, w.tenant, &mut r)
        .await
        .unwrap();
    assert!(!r.changed(), "{r:?}");
    // the drainer's sweep runs over every active tenant without failing
    let all = model_changes::sweep(&w.e.state).await.unwrap();
    assert_eq!(all.errors, 0, "{all:?}");
}

// ---- rebuilds -------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn reembed_creates_strata_store_with_store_binding_and_starts_copy() {
    let (w, a, _b, kb, ch) = embed_change().await;
    let old = w.bound(kb).await;
    let (s, v) = w.reembed(ch, &a).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["state"], "reembedding");
    let created = w.rag("POST", "/api/stores");
    let root = created
        .iter()
        .find(|r| {
            r.path == "/api/stores"
                && r.body["name"]
                    .as_str()
                    .is_some_and(|n| n.starts_with("kb.gen-"))
        })
        .expect("a new strata root");
    assert_eq!(root.body["parent_id"], Value::Null, "a NEW root (R7)");
    assert_eq!(root.body["kind"], "leaf");
    let new = w.fake_store("kb.gen-", None);
    let bind = w.rag("PUT", &format!("/api/stores/{new}/providers"));
    assert_eq!(bind.len(), 1);
    assert_eq!(bind[0].body["embed"]["model"], "e-b");
    assert_eq!(bind[0].body["embed"]["api_key"], KEY);
    assert_eq!(bind[0].body["embed"]["dims"], 1024);
    let copy = w.rag("POST", &format!("/api/stores/{old}/reembed-copy"));
    assert_eq!(copy.len(), 1);
    assert_eq!(copy[0].body["target_store"], json!(new));
    assert_eq!(copy[0].body["copy_vectors"], false);
    assert!(w.rag("PUT", "/api/tenant/providers").is_empty());
    let gens = w.generations(kb).await;
    assert_eq!(gens.len(), 2);
    assert_eq!(gens[1].build, "reembed");
    assert_eq!(gens[1].state, "building");
    assert_eq!(gens[1].strata_store_id, Some(new));
    assert_eq!(gens[1].embed.as_deref(), Some("e-b"));
    // a second click while it runs
    let (s, v) = w.reembed(ch, &a).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(code(&v), "already_reembedding");
}

#[tokio::test]
async fn copy_rebuild_calls_reembed_copy_with_copy_vectors() {
    let w = world().await;
    // job a exists before the saves, so its publish pushes e-a / r-a (see the store_create test)
    let a = w.job().await;
    w.save_slot("embed", "e-a").await;
    w.save_slot("rerank", "r-a").await;
    // strata runs e-a / r-a (published) when kb is made (review I3: the initial generation is what strata runs)
    w.publish(&a).await;
    w.drain().await;
    let kb = w.store("kb", "leaf", None).await;
    let old = w.bound(kb).await;
    w.save_slot("rerank", "r-b").await;
    let ch = w.change("rerank").await;
    let (s, v) = w.reembed(ch, &a).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let copy = w.rag("POST", &format!("/api/stores/{old}/reembed-copy"));
    assert_eq!(copy.len(), 1);
    assert_eq!(copy[0].body["copy_vectors"], true);
    let new = w.fake_store("kb.gen-", None);
    let bind = w.rag("PUT", &format!("/api/stores/{new}/providers"));
    assert_eq!(bind[0].body["rerank"]["model"], "r-b");
    assert_eq!(bind[0].body["rerank"]["dialect"], "cohere");
    assert_eq!(
        bind[0].body["embed"]["model"], "e-a",
        "the same embed model"
    );
}

#[tokio::test]
async fn generation_store_gets_bindings_for_all_six() {
    let w = world().await;
    // job a exists before the saves, so its publish pushes all six (see the store_create test)
    let a = w.job().await;
    for (slot, model) in [
        ("embed", "e-a"),
        ("rerank", "r-a"),
        ("chat", "c-a"),
        ("ocr", "o-a"),
        ("vision", "v-a"),
        ("transcribe", "t-a"),
    ] {
        w.save_slot(slot, model).await;
    }
    w.publish(&a).await;
    w.drain().await;
    let kb = w.store("kb", "leaf", None).await;
    w.save_slot("embed", "e-b").await;
    let ch = w.change("embed").await;
    let (s, v) = w.reembed(ch, &a).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let new = w.fake_store("kb.gen-", None);
    let rag = w.rag("PUT", &format!("/api/stores/{new}/providers"));
    assert_eq!(rag.len(), 1);
    for (cap, model) in [("embed", "e-b"), ("rerank", "r-a"), ("chat", "c-a")] {
        assert_eq!(rag[0].body[cap]["model"], model, "{cap}");
        assert_eq!(rag[0].body[cap]["api_key"], KEY, "{cap}");
    }
    let ing = w.ingest("PUT", &format!("/api/stores/{new}/providers"));
    assert_eq!(ing.len(), 1);
    for (cap, model) in [("ocr", "o-a"), ("vision", "v-a"), ("transcribe", "t-a")] {
        assert_eq!(ing[0].body[cap]["model"], model, "{cap}");
        assert!(ing[0].body[cap].get("dims").is_none(), "{cap}");
    }
    let gens = w.generations(kb).await;
    for cap in ["embed", "rerank", "chat", "ocr", "vision", "transcribe"] {
        assert_eq!(gens[1].provider_ref[cap]["source"], "tenant", "{cap}");
    }
}

#[tokio::test]
async fn reembed_reuses_ready_generation_of_another_job() {
    let (w, a, b, kb, ch) = embed_change().await;
    let (s, _) = w.reembed(ch, &a).await;
    assert_eq!(s, StatusCode::OK);
    w.finish_rag("done", None);
    assert_eq!(w.view(ch, &a).await["state"], "ready");
    let roots = w.rag("POST", "/api/stores").len();
    let (s, v) = w.reembed(ch, &b).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v["state"], "ready",
        "the target generation is already built"
    );
    assert_eq!(
        w.rag("POST", "/api/stores").len(),
        roots,
        "no new strata root"
    );
    let gens = w.generations(kb).await;
    assert_eq!(gens.len(), 2);
    assert_eq!(v["stores"][0]["to_generation"]["id"], json!(gens[1].id));
}

#[tokio::test]
async fn rebuild_not_ready_until_strata_done_after_delta_pass() {
    let (w, a, _b, _kb, ch) = embed_change().await;
    w.reembed(ch, &a).await;
    // the first pass finished, a delta pass is running (D92): still building
    {
        let mut g = lock(&w.fake.rag);
        for (_, j) in g.reembed_jobs.iter_mut() {
            j["chunks_done"] = json!(5);
            j["chunks_total"] = json!(5);
        }
    }
    let part = w.view(ch, &a).await;
    assert_eq!(part["state"], "reembedding");
    assert_eq!(part["stores"][0]["to_generation"]["state"], "building");
    let (s, v) = w.accept(ch, &a, false).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(code(&v), "generation_not_ready");
    w.finish_rag("done", None);
    let part = w.view(ch, &a).await;
    assert_eq!(part["state"], "ready");
    assert_eq!(part["stores"][0]["to_generation"]["state"], "ready");
    assert_eq!(part["stores"][0]["chunks"], 7);
    let (s, v) = w.accept(ch, &a, false).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

#[tokio::test]
async fn reembed_copy_not_converged_failed_marks_generation_failed() {
    let (w, a, _b, kb, ch) = embed_change().await;
    w.reembed(ch, &a).await;
    w.finish_rag(
        "failed",
        Some("conflict: the source kept changing; the copy did not converge"),
    );
    let part = w.view(ch, &a).await;
    assert_eq!(part["state"], "pending", "{part}");
    assert_eq!(part["stores"][0]["to_generation"], Value::Null);
    let gens = w.generations(kb).await;
    assert_eq!(gens[1].state, "failed");
    assert!(gens[1].error.as_deref().unwrap_or("").contains("converge"));
    // the person starts it again: a new generation
    let (s, v) = w.reembed(ch, &a).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(w.generations(kb).await.len(), 3);
    assert_eq!(
        w.fake
            .rag_requests()
            .iter()
            .filter(|r| r.method == "POST" && r.path.ends_with("/reembed-copy"))
            .count(),
        2
    );
}

#[tokio::test]
async fn reingest_missing_originals_409_with_documents() {
    let w = world().await;
    w.save_slot("ocr", "o-a").await;
    let a = w.job().await;
    let kb = w.store("kb", "leaf", None).await;
    let old = w.bound(kb).await;
    w.save_slot("ocr", "o-b").await;
    let ch = w.change("ocr").await;
    let (s, v) = w.reembed(ch, &a).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let new = w.fake_store("kb.gen-", None);
    let start = w.ingest("POST", &format!("/api/stores/{old}/reingest-copy"));
    assert_eq!(start.len(), 1);
    assert_eq!(start[0].body["target_store"], json!(new));
    let bind = w.ingest("PUT", &format!("/api/stores/{new}/providers"));
    assert_eq!(bind[0].body["ocr"]["model"], "o-b");
    let doc = Uuid::new_v4();
    {
        let mut g = lock(&w.fake.ingest);
        for (_, j) in g.reingest_jobs.iter_mut() {
            j["state"] = json!("done");
            j["documents_done"] = json!(3);
            j["documents_total"] = json!(4);
            j["documents_missing"] = json!(1);
            j["missing_originals"] = json!([{"document_id": doc, "reason": "originals_not_retained",
                                              "title": "old.pdf", "source_id": "upload:1"}]);
        }
    }
    let part = w.view(ch, &a).await;
    assert_eq!(part["state"], "ready");
    assert_eq!(
        part["stores"][0]["missing_originals"][0]["document_id"],
        json!(doc)
    );
    let (s, v) = w.accept(ch, &a, false).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(code(&v), "originals_not_retained");
    assert_eq!(v["error"]["documents"][0]["document_id"], json!(doc));
    let (s, v) = w.accept(ch, &a, true).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["state"], "accepted");
}

#[tokio::test]
async fn reingest_upstream_missing_503_change_stays_pending() {
    let w = world().await;
    w.save_slot("ocr", "o-a").await;
    let a = w.job().await;
    let kb = w.store("kb", "leaf", None).await;
    let old = w.bound(kb).await;
    w.save_slot("ocr", "o-b").await;
    let ch = w.change("ocr").await;
    lock(&w.fake.ingest).script(
        "POST",
        &format!("/api/stores/{old}/reingest-copy"),
        404,
        Value::Null,
    );
    let (s, v) = w.reembed(ch, &a).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert_eq!(code(&v), "unavailable");
    assert_eq!(w.job_state(ch, &a).await.as_deref(), Some("pending"));
    let gens = w.generations(kb).await;
    assert_eq!(gens.len(), 1, "the unstarted generation is gone: {gens:?}");
    let new = w.fake_store("kb.gen-", None);
    let queued = w
        .count(
            "SELECT count(*) FROM provision_outbox WHERE op = 'store_delete' AND payload->>'strata_store_id' = $1::text",
            new,
        )
        .await;
    assert_eq!(queued, 1, "the root it created is deleted again");
}

// ---- accept, live vs draft, publish ---------------------------------------------------------------------------------

#[tokio::test]
async fn accept_before_ready_409_generation_not_ready() {
    let (w, a, _b, _kb, ch) = embed_change().await;
    let (s, v) = w.accept(ch, &a, false).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(code(&v), "generation_not_ready");
    assert_eq!(v["error"]["stores"], json!(["kb"]));
    w.reembed(ch, &a).await;
    let (s, v) = w.accept(ch, &a, false).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(code(&v), "generation_not_ready");
}

#[tokio::test]
async fn accept_writes_embedding_pin_draft_for_this_job_only() {
    let (w, a, b, kb, ch) = embed_change().await;
    w.reembed(ch, &a).await;
    w.finish_rag("done", None);
    let (rev_a, rev_b) = (w.draft_rev(&a).await, w.draft_rev(&b).await);
    let (s, v) = w.accept(ch, &a, false).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["state"], "accepted");
    let gen2 = w.generations(kb).await[1].id;
    let pinned: Option<Uuid> = sqlx::query_scalar(
        "SELECT generation_id FROM job_embedding_pin WHERE job_id = $1 AND store_key = 'kb'",
    )
    .bind(a.job.job_id)
    .fetch_optional(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(pinned, Some(gen2));
    assert!(w.draft_rev(&a).await > rev_a, "the engine bumps the draft");
    let ledger = w
        .count(
            "SELECT count(*) FROM draft_ledger WHERE job_id = $1 AND kind = 'embedding_pin' AND key = 'embedding:kb'",
            a.job.job_id,
        )
        .await;
    assert_eq!(ledger, 1);
    assert_eq!(w.draft_rev(&b).await, rev_b, "job b is untouched");
    let pins_b = w
        .count(
            "SELECT count(*) FROM job_embedding_pin WHERE job_id = $1",
            b.job.job_id,
        )
        .await;
    assert_eq!(pins_b, 0);
    assert_eq!(w.job_state(ch, &b).await.as_deref(), Some("pending"));
    let (s, v) = w.accept(ch, &a, false).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(code(&v), "already_accepted");
}

#[tokio::test]
async fn job_b_live_queries_old_generation_until_b_publishes() {
    let (w, a, b, kb, ch) = embed_change().await;
    let old = StrataStoreId(w.bound(kb).await);
    w.reembed(ch, &a).await;
    w.finish_rag("done", None);
    let (s, v) = w.reembed(ch, &b).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = w.accept(ch, &b, false).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let new = StrataStoreId(w.fake_store("kb.gen-", None));
    assert_eq!(w.strata_for(&b, kb, DesignOf::Draft).await, Some(new));
    assert_eq!(
        w.strata_for(&b, kb, DesignOf::Live).await,
        Some(old),
        "not published yet"
    );
    w.publish(&b).await;
    assert_eq!(w.strata_for(&b, kb, DesignOf::Live).await, Some(new));
    assert_eq!(w.job_state(ch, &b).await.as_deref(), Some("published"));
    // job a never accepted: both of its designs stay on the old generation
    assert_eq!(w.strata_for(&a, kb, DesignOf::Live).await, Some(old));
    assert_eq!(w.strata_for(&a, kb, DesignOf::Draft).await, Some(old));
}

#[tokio::test]
async fn test_env_query_reads_draft_pin() {
    let (w, a, _b, kb, ch) = embed_change().await;
    let old = w.bound(kb).await;
    w.reembed(ch, &a).await;
    w.finish_rag("done", None);
    w.accept(ch, &a, false).await;
    let new = w.fake_store("kb.gen-", None);
    let ask = |job: Option<&str>| {
        let mut body = json!({"query": "opening hours", "mode": "embed", "store_id": kb});
        if let Some(j) = job {
            body["job"] = json!(j);
        }
        body
    };
    let (s, v) = w
        .call(
            &w.km,
            "POST",
            "/api/knowledge/query",
            ask(Some(a.job.job_key.as_str())),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = w
        .call(&w.km, "POST", "/api/knowledge/query", ask(None))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let sent: Vec<Value> = w
        .rag("POST", "/api/query")
        .into_iter()
        .map(|r| r.body["root_id"].clone())
        .collect();
    assert_eq!(
        sent,
        vec![json!(new), json!(old)],
        "the Test env names the draft pin's root"
    );
}

#[tokio::test]
async fn reembed_creates_new_root_and_queries_name_it() {
    let (w, a, _b, kb, ch) = embed_change().await;
    w.reembed(ch, &a).await;
    w.finish_rag("done", None);
    w.accept(ch, &a, false).await;
    w.publish(&a).await;
    w.drain().await;
    let new = w.fake_store("kb.gen-", None);
    let roots = lock(&w.fake.rag)
        .stores
        .iter()
        .filter(|s| s.tenant == w.tenant && s.parent_id.is_none())
        .count();
    assert_eq!(roots, 2, "the generation is its own strata root (R7)");
    assert_eq!(
        w.strata_for(&a, kb, DesignOf::Live).await,
        Some(StrataStoreId(new))
    );
    let (s, v) = w
        .call(
            &w.km,
            "POST",
            "/api/knowledge/query",
            json!({"query": "opening hours", "mode": "embed", "job": a.job.job_key}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "never 409 ambiguous_root: {v}");
    let last = w.rag("POST", "/api/query").pop().unwrap();
    assert_eq!(last.body["root_id"], json!(new));
}

#[tokio::test]
async fn ingest_targets_pinned_generation_per_design_of() {
    let w = world().await;
    w.save_slot("embed", "e-a").await;
    let a = w.job().await;
    let kb = w.store("kb", "group", None).await;
    let faq = w.store("faq", "leaf", Some(kb)).await;
    let faq_old = w.bound(faq).await;
    w.save_slot("embed", "e-b").await;
    let ch = w.change("embed").await;
    let (s, v) = w.reembed(ch, &a).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    w.finish_rag("done", None);
    let (s, v) = w.accept(ch, &a, false).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let new_root = w.fake_store("kb.gen-", None);
    let faq_new = w.fake_store("faq", Some(new_root));
    assert_eq!(
        w.strata_for(&a, faq, DesignOf::Draft).await,
        Some(StrataStoreId(faq_new)),
        "Test ingests go to the draft pin's mirrored leaf"
    );
    assert_eq!(
        w.strata_for(&a, faq, DesignOf::Live).await,
        Some(StrataStoreId(faq_old)),
        "Production keeps the published leaf"
    );
}

#[tokio::test]
async fn publish_marks_published_and_retires_unpinned_generations() {
    let (w, a, _b, kb, ch1) = embed_change().await;
    w.reembed(ch1, &a).await;
    w.finish_rag("done", None);
    w.accept(ch1, &a, false).await;
    w.publish(&a).await;
    assert_eq!(w.job_state(ch1, &a).await.as_deref(), Some("published"));
    let gen2 = w.generations(kb).await[1].id;
    let root2 = w.fake_store("kb.gen-", None);
    // a second change: job a moves on from generation 2
    w.save_slot("embed", "e-c").await;
    let ch2 = w.change("embed").await;
    assert_ne!(ch1, ch2);
    assert_eq!(w.job_state(ch2, &a).await.as_deref(), Some("pending"));
    let (s, v) = w.reembed(ch2, &a).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let copies = w.rag("POST", &format!("/api/stores/{root2}/reembed-copy"));
    assert_eq!(
        copies.len(),
        1,
        "generation 3 is copied from generation 2's root"
    );
    w.finish_rag("done", None);
    let (s, v) = w.accept(ch2, &a, false).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    w.publish(&a).await;
    assert_eq!(w.job_state(ch2, &a).await.as_deref(), Some("published"));
    let gens = w.generations(kb).await;
    let states: Vec<(&str, &str)> = gens
        .iter()
        .map(|g| (g.build.as_str(), g.state.as_str()))
        .collect();
    assert_eq!(
        states,
        [
            ("initial", "ready"),
            ("reembed", "retired"),
            ("reembed", "ready")
        ],
        "job b still uses generation 1; nothing uses generation 2"
    );
    assert_eq!(gens[1].id, gen2);
    let key = format!("strata_rag:store_delete:{root2}");
    let status = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT status::text FROM provision_outbox WHERE dedupe_key = $1 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&key)
        .fetch_one(&w.e.owner)
        .await
        .unwrap()
    };
    assert_eq!(status().await, "pending");
    w.drain().await;
    assert_eq!(status().await, "succeeded");
    let deleted = w.rag("DELETE", &format!("/api/stores/{root2}"));
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].query, "recursive=true");
}

// ruling 2026-10-21 (revised): strata cannot pin its deployment default per store, so a tree on the default is moved
// by the model change the save fans out, and the tenant embed waits until no job reads the old root
#[tokio::test]
async fn default_tree_embed_change_is_withheld_until_the_job_publishes_its_new_root() {
    let w = world().await;
    let a = w.job().await;
    let kb = w.store("kb", "group", None).await;
    w.store("faq", "leaf", Some(kb)).await;
    let old_root = w.bound(kb).await;
    // the save is allowed and fans out a model change for kb (job a reads its initial generation, the default)
    w.save_slot("embed", "e-b").await;
    let ch = w.change("embed").await;
    assert_eq!(w.job_state(ch, &a).await.as_deref(), Some("pending"));
    let part = w.view(ch, &a).await;
    assert_eq!(part["stores"][0]["store"], "kb", "{part}");
    // publishing the slot alone: the tenant embed is withheld (kb holds content on the default and is still read)
    w.publish(&a).await;
    w.drain().await;
    let withheld: Option<Value> = sqlx::query_scalar(
        "SELECT result -> 'withheld' FROM provision_outbox \
          WHERE tenant_id = $1 AND dedupe_key = 'strata_rag:connections_put' AND status = 'succeeded' \
          ORDER BY completed_at DESC LIMIT 1",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(withheld.unwrap_or(Value::Null)[0]["model"], "e-b");
    let puts = w.rag("PUT", "/api/tenant/providers");
    assert!(!puts.is_empty());
    assert!(
        puts.iter().all(|r| r.body["embed"].is_null()),
        "the tenant embed stays on the default"
    );
    assert!(
        w.rag("PUT", &format!("/api/stores/{old_root}/providers"))
            .is_empty(),
        "a default cannot be pinned"
    );
    // review I4: the embed slot and the job's readiness say the model is withheld, and by what
    let embed = w.embed_slot().await;
    assert_eq!(embed["withheld"]["model"], "e-b", "{embed}");
    assert_eq!(embed["withheld"]["roots"], json!(["kb"]), "{embed}");
    assert!(
        w.readiness(&a).await.contains("embed_push_withheld"),
        "a readiness warning"
    );
    // job a moves kb: re-embed into a new root bound to e-b at store level, accept, publish
    let (s, v) = w.reembed(ch, &a).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    w.finish_rag("done", None);
    let (s, v) = w.accept(ch, &a, false).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        w.count(
            "SELECT count(*) FROM provision_outbox WHERE tenant_id = $1 AND status = 'pending' \
               AND dedupe_key = 'strata_rag:connections_put'",
            w.tenant,
        )
        .await,
        1,
        "accepting queued the withheld embed again"
    );
    w.publish(&a).await;
    assert_eq!(w.job_state(ch, &a).await.as_deref(), Some("published"));
    w.drain().await;
    let last = w.rag("PUT", "/api/tenant/providers").pop().unwrap();
    assert_eq!(
        last.body["embed"]["model"], "e-b",
        "nothing reads the default root any more: the tenant embed goes out"
    );
    assert_eq!(
        lock(&w.fake.rag).tenant_providers[&w.tenant]["embed"]["model"],
        "e-b"
    );
    let remote: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM provider_slot_remote WHERE tenant_id = $1 AND slot = 'embed' AND pushed_fpr <> 'none'",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(remote, 1, "recorded as pushed only now");
    assert!(w.embed_slot().await["withheld"].is_null());
    assert!(!w.readiness(&a).await.contains("embed_push_withheld"));
}

// ---- the design contract, roles, refusals, offboarding ----------------------------------------------------------

#[tokio::test]
async fn embedding_pin_full_contract() {
    let e = common::env().await;
    let models = |embed: &str| json!({"embed": embed, "rerank": null, "chat": null, "ocr": null, "vision": null, "transcribe": null});
    let samples = [
        (
            store_ref("faq"),
            json!({"store": "faq", "generation": Uuid::from_u128(1).to_string(), "models": models("e-1")}),
        ),
        (
            store_ref("policy.gift_cards"),
            json!({"store": "policy.gift_cards", "generation": Uuid::from_u128(2).to_string(),
                   "models": models("e-2")}),
        ),
    ];
    design_contract::run_all(&e, "embedding_pin", &samples, Some("first generation")).await;
}

#[tokio::test]
async fn model_change_writes_need_knowledge_manager() {
    let (w, a, _b, _kb, ch) = embed_change().await;
    for action in ["reembed", "accept"] {
        let uri = format!(
            "/api/knowledge/model-changes/{ch}/jobs/{}/{action}",
            a.job.job_key
        );
        let (s, v) = w.call(&w.im, "POST", &uri, json!({})).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{action}: {v}");
        assert_eq!(v["error"]["missing_role"], "knowledge_manager");
    }
    let (s, v) = w
        .call(
            &w.km,
            "POST",
            &format!(
                "/api/knowledge/model-changes/{}/jobs/{}/reembed",
                Uuid::new_v4(),
                a.job.job_key
            ),
            json!({}),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(code(&v), "model_change_not_found");
}

#[tokio::test]
async fn model_changes_list_filters_by_state() {
    let (w, _a, _b, _kb, ch) = embed_change().await;
    let (s, v) = send(
        &w.app,
        get("/api/knowledge/model-changes?state=pending", Some(&w.km)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let ids: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["id"].as_str())
        .collect();
    assert!(ids.contains(&ch.to_string().as_str()), "{v}");
    let (s, v) = send(
        &w.app,
        get("/api/knowledge/model-changes?state=published", Some(&w.km)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v, json!([]));
    let (s, v) = send(
        &w.app,
        get("/api/knowledge/model-changes?state=bogus", Some(&w.km)),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(code(&v), "invalid_state");
}

#[tokio::test]
async fn offboard_cancels_pending_store_delete() {
    let w = world().await;
    let sid = StrataStoreId(Uuid::new_v4());
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    let id = store_delete::enqueue(&mut tx, w.tenant, sid, &Actor::system("test-t4a"))
        .await
        .unwrap();
    let cancelled = provision::on_tenant_offboarded(&mut tx, w.tenant, &Actor::system("test-t4a"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(cancelled.contains(&id), "{cancelled:?}");
}
