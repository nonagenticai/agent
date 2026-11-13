//! B3 T4: provider slots (D69 draft, interim for W5 A1/A2 — plan B3 §5.1, §7 G2, T4). The six rag/ingest slots, the
//! save (validation, live probe against a mock provider, sealed key, `design::track_tenant` draft, no strata call),
//! the `published` hook queueing `connections_put`, the sender against the strata fake (read the whole set, replace
//! the published slots, under the per-tenant advisory lock; 422 fails the row as `slot_test_failed`), drift by
//! fingerprint and its adoption, and the full B2 design contract of `provider_slot`. Also the T4a ruling (an embed
//! change pins existing roots at store level first; roots on the deployment default withhold it), T4b "Use default" (F-08) and T4c "Test" a draft (F-09). Real Postgres 18 (one container
//! for this binary, a fresh database per test); the API runs with the test-only loopback netguard policy so the mock
//! provider on 127.0.0.1 can be probed.
use crate::common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::common::design_contract::{
    self, ContractJob, TEMPLATE, contract_job, cuttable_selection,
};
use crate::common::strata_fake::{self, StrataFake, lock};
use crate::common::{TestEnv, get, insert_tenant, insert_user, json, person, send, super_admin};
use agent_api::audit;
use agent_api::connectors::transport::Transport;
use agent_api::design::DraftRef;
use agent_api::netguard::Policy;
use agent_api::providers::slots;
use agent_api::provision::{Target, drain_once};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get as route_get;
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{Value, json};
use uuid::Uuid;

/// The key the mock provider accepts, and one it refuses (401).
const GOOD: &str = "sk-slot-good-7f3a91";
const WRONG: &str = "sk-slot-wrong-0b2c44";

fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
    vars.insert(
        "PROVIDER_SECRET_KEYS".to_string(),
        format!(r#"{{"s1":"{key}"}}"#),
    );
    vars.insert("PROVIDER_SECRET_KEY_CURRENT".to_string(), "s1".to_string());
}

// ---- the mock provider (the live probe of a save) -------------------------------------------------------------------

/// Every `Authorization` header the mock provider saw.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<Option<String>>>>);

async fn models(State(s): State<Seen>, h: HeaderMap) -> (StatusCode, Json<Value>) {
    let auth = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    s.0.lock().unwrap().push(auth.clone());
    if auth.as_deref() == Some(format!("Bearer {GOOD}").as_str()) {
        (
            StatusCode::OK,
            Json(json!({"data": [{"id": "mistral-embed"}]})),
        )
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid api key"})),
        )
    }
}

/// `http://127.0.0.1:<port>`.
async fn provider() -> (String, Seen) {
    let seen = Seen::default();
    let app = Router::new()
        .route("/ok/models", route_get(models))
        .route("/alt/models", route_get(models))
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

// ---- the world ------------------------------------------------------------------------------------------------------

/// One `provision_outbox` row as the tests read it.
#[derive(Debug, sqlx::FromRow)]
struct Outbox {
    status: String,
    last_error_code: Option<String>,
    last_error: Option<String>,
    payload: Value,
}

/// A `provider_slot` working row.
#[derive(Debug, sqlx::FromRow)]
struct WorkingRow {
    configured: bool,
    kind: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    version: i32,
    secret_id: Option<Uuid>,
    key_kept: bool,
}

/// A `provider_slot_remote` row.
#[derive(Debug, sqlx::FromRow)]
struct Pushed {
    pushed_version: i32,
    pushed_fpr: String,
    pushed_release: Option<String>,
    observed_fpr: Option<String>,
}

struct World {
    e: TestEnv,
    /// The API with the loopback netguard policy.
    app: Router,
    fake: StrataFake,
    /// The mock provider's base (`http://127.0.0.1:<port>`).
    prov: String,
    seen: Seen,
    tenant: Uuid,
    /// integrations_manager, observer (tenant_user).
    im: String,
    obs: String,
}

/// A member of `tenant` holding `role`; its token.
async fn member(e: &TestEnv, tenant: Uuid, role: &str) -> String {
    let sub = format!("u-{role}-{}", Uuid::new_v4().simple());
    insert_user(&e.owner, &sub, tenant, role).await;
    person(&sub, &[])
}

/// A fresh tenant with strata token pairs, the provider keyring and two members.
async fn world() -> World {
    let fake = strata_fake::start().await;
    let (prov, seen) = provider().await;
    let tenant = Uuid::new_v4();
    let e = common::env_with(|v| {
        with_keys(v);
        fake.env(&[tenant], v);
    })
    .await;
    insert_tenant(&e.owner, tenant, &format!("Slots {tenant}")).await;
    let mut st = e.state.clone();
    st.connectors = Transport::new(Policy {
        allow_loopback: true,
        private_targets: vec![],
    });
    let app = agent_api::router(st);
    let im = member(&e, tenant, "integrations_manager").await;
    let obs = member(&e, tenant, "tenant_user").await;
    World {
        e,
        app,
        fake,
        prov,
        seen,
        tenant,
        im,
        obs,
    }
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

impl World {
    async fn call(&self, tok: &str, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        send(&self.app, json(method, uri, Some(tok), body)).await
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.prov)
    }

    /// A save body for the mock provider's `/ok` endpoint.
    fn input(&self, kind: &str, model: &str, secret: Option<&str>, version: i64) -> Value {
        json!({"kind": kind, "base_url": self.url("/ok"), "model": model, "secret": secret,
               "expected_version": version})
    }

    async fn save(&self, slot: &str, body: Value) -> (StatusCode, Value) {
        self.call(
            &self.im,
            "PUT",
            &format!("/api/providers/slots/{slot}"),
            body,
        )
        .await
    }

    async fn slots(&self) -> Vec<Value> {
        let (s, b) = send(&self.app, get("/api/providers/slots", Some(&self.obs))).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        b.as_array().unwrap().clone()
    }

    async fn slot(&self, slot: &str) -> Value {
        self.slots()
            .await
            .into_iter()
            .find(|s| s["slot"] == slot)
            .unwrap()
    }

    async fn count(&self, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql)
            .bind(self.tenant)
            .fetch_one(&self.e.owner)
            .await
            .unwrap()
    }

    /// The latest outbox row of `dedupe_key`.
    async fn outbox(&self, dedupe_key: &str) -> Outbox {
        sqlx::query_as(
            "SELECT status::text AS status, last_error_code, last_error, payload FROM provision_outbox \
              WHERE tenant_id = $1 AND dedupe_key = $2 ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(self.tenant)
        .bind(dedupe_key)
        .fetch_one(&self.e.owner)
        .await
        .unwrap()
    }

    /// The tenant's provider view in a fake service.
    fn set_remote(&self, svc: &strata_fake::Shared, view: Value) {
        lock(svc).tenant_providers.insert(self.tenant, view);
    }

    /// Requests to `/api/tenant/providers` of a fake service: (method, body).
    fn provider_calls(svc: &strata_fake::Shared) -> Vec<(String, Value)> {
        lock(svc)
            .requests
            .iter()
            .filter(|r| r.path == "/api/tenant/providers")
            .map(|r| (r.method.clone(), r.body.clone()))
            .collect()
    }
}

/// A job of `w`'s tenant, created through the API (B2's creation hook ran), with a complete selection so it cuts;
/// and a release manager's token.
async fn with_job(w: &World) -> (ContractJob, String) {
    let mut r = json(
        "POST",
        "/api/jobs",
        Some(&super_admin()),
        json!({"template": format!("{}@v{}", TEMPLATE.0, TEMPLATE.1)}),
    );
    r.headers_mut()
        .insert("x-active-tenant-id", w.tenant.to_string().parse().unwrap());
    let (s, b) = send(&w.e.app, r).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let key = b["key"].as_str().unwrap().to_string();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
        .bind(w.tenant)
        .bind(&key)
        .fetch_one(&w.e.owner)
        .await
        .unwrap();
    let f = contract_job(w.tenant, id, &key);
    design_contract::write(
        &w.e,
        &f,
        "selection",
        &DraftRef::new(),
        Some(cuttable_selection()),
    )
    .await;
    design_contract::publishable(&w.e, &f).await;
    let rm = member(&w.e, w.tenant, "tenant_admin").await;
    (f, rm)
}

/// Approve and apply the job's draft as the release manager; the release id.
async fn publish(w: &World, f: &ContractJob, rm: &str) -> String {
    let job = &f.job.job_key;
    let (s, b) = send(&w.app, get(&format!("/api/jobs/{job}/target"), Some(rm))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let body = json!({"apply": true, "selection_seen_at": b["selection_updated_at"].clone()});
    let (s, b) = w
        .call(rm, "POST", &format!("/api/jobs/{job}/releases"), body)
        .await;
    assert!(
        s == StatusCode::CREATED || s == StatusCode::OK,
        "approve: {s}: {b}"
    );
    assert_eq!(b["applied"], true, "{b}");
    b["release"]["id"].as_str().unwrap().to_string()
}

/// Save `embed` (openai_compatible on the mock, key GOOD) at version 0.
async fn save_embed(w: &World) -> Value {
    let (s, b) = w
        .save(
            "embed",
            w.input("openai_compatible", "mistral-embed", Some(GOOD), 0),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

/// A drift-free tenant whose `embed` was saved, published and pushed: (job, release manager, release id).
async fn pushed_embed(w: &World) -> (ContractJob, String, String) {
    let (f, rm) = with_job(w).await;
    save_embed(w).await;
    let release = publish(w, &f, &rm).await;
    drain_once(&w.e.state).await;
    assert_eq!(
        w.outbox("strata_rag:connections_put").await.status,
        "succeeded"
    );
    (f, rm, release)
}

// ---- list -----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn slots_lists_six_in_slot_order() {
    let w = world().await;
    let list = w.slots().await;
    let order: Vec<&str> = list.iter().map(|s| s["slot"].as_str().unwrap()).collect();
    assert_eq!(
        order,
        ["embed", "rerank", "chat", "ocr", "vision", "transcribe"]
    );
    for s in &list {
        assert_eq!(s["configured"], false, "{s}");
        assert_eq!(s["version"], 0, "{s}");
        assert_eq!(
            s["secret"],
            json!({"set": false, "updated_at": null, "fingerprint": null})
        );
        assert!(!s["used_by"].as_array().unwrap().is_empty(), "{s}");
        assert!(s["drift"].is_null(), "{s}");
    }
    assert_eq!(list[0]["service"], "strata_rag");
    assert_eq!(list[3]["service"], "strata_ingest");
    assert_eq!(list[0]["kinds"], json!(["openai_compatible", "voyage"]));
    assert_eq!(list[1]["kinds"], json!(["cohere", "voyage"]));

    save_embed(&w).await;
    let embed = w.slot("embed").await;
    assert_eq!(embed["configured"], true, "{embed}");
    assert_eq!(embed["version"], 1);
    assert_eq!(embed["kind"], "openai_compatible");
    assert_eq!(embed["model"], "mistral-embed");
    assert_eq!(embed["secret"]["set"], true);
    let fpr = embed["secret"]["fingerprint"].as_str().unwrap();
    assert!(
        fpr.len() == 8 && fpr.chars().all(|c| c.is_ascii_hexdigit()),
        "{fpr}"
    );
    assert_eq!(
        embed["used_by"],
        json!(["knowledge.search", "ingest indexing"])
    );

    // D70: nonagentic's own tenant sees its platform kinds, read from what strata serves it (`default`)
    sqlx::query("UPDATE tenants SET platform_owned = true WHERE id = $1")
        .bind(w.tenant)
        .execute(&w.e.owner)
        .await
        .unwrap();
    let dflt = |dialect: &str, model: &str| json!({"source": "default", "dialect": dialect, "model": model});
    w.set_remote(
        &w.fake.rag,
        json!({"embed": dflt("openai", "BAAI/bge-m3"), "rerank": dflt("cohere", "BAAI/bge-reranker-v2-m3"),
               "chat": dflt("openai", "local-llm")}),
    );
    w.set_remote(
        &w.fake.ingest,
        json!({"ocr": dflt("mistral", "PaddleOCR-VL"), "vision": dflt("openai", "Qwen2-VL-2B-Instruct-4bit"),
               "transcribe": dflt("openai", "whisper-large-v3-turbo")}),
    );
    let list = w.slots().await;
    assert_eq!(list.len(), 6);
    for s in &list {
        let want = if s["slot"] == "chat" {
            "pgx_vllm"
        } else {
            "mlx_api"
        };
        assert_eq!(s["configured"], true, "{s}");
        assert_eq!(s["kind"], want, "{s}");
        assert_eq!(s["kinds"], json!([want]), "{s}");
        assert_eq!(s["secret"]["set"], false, "{s}");
    }
    assert_eq!(list[0]["model"], "BAAI/bge-m3");
    assert_eq!(list[5]["model"], "whisper-large-v3-turbo");
}

// ---- save refusals --------------------------------------------------------------------------------------------------

#[tokio::test]
async fn save_stale_expected_version_409_version_conflict() {
    let w = world().await;
    save_embed(&w).await;
    let (s, b) = w
        .save("embed", w.input("voyage", "voyage-3.5", Some(GOOD), 0))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "VERSION_CONFLICT"),
        "{b}"
    );
    // nothing was written
    let embed = w.slot("embed").await;
    assert_eq!(
        (embed["version"].clone(), embed["kind"].clone()),
        (json!(1), json!("openai_compatible"))
    );
    assert_eq!(
        w.count("SELECT count(*) FROM provider_slot_secret WHERE tenant_id = $1")
            .await,
        1
    );
    // the current version saves
    let (s, b) = w
        .save("embed", w.input("voyage", "voyage-3.5", Some(GOOD), 1))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["version"], 2);
}

#[tokio::test]
async fn save_with_open_drift_409_version_conflict() {
    let w = world().await;
    // another app registered embed in strata: agent never pushed it
    w.set_remote(
        &w.fake.rag,
        json!({"embed": {"source": "tenant", "base_url": "https://other.example/v1", "dialect": "openai",
                         "model": "theirs", "has_api_key": true},
               "rerank": {"source": "unconfigured"}, "chat": {"source": "unconfigured"}}),
    );
    let embed = w.slot("embed").await;
    assert_eq!(embed["drift"]["changed_by"], "another app", "{embed}");
    let (s, b) = w
        .save(
            "embed",
            w.input("openai_compatible", "mistral-embed", Some(GOOD), 0),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "VERSION_CONFLICT"),
        "{b}"
    );
    assert_eq!(
        w.count("SELECT count(*) FROM provider_slot WHERE tenant_id = $1")
            .await,
        0
    );
    assert!(w.seen.0.lock().unwrap().is_empty(), "no probe on a refusal");
    // another slot of the service has not drifted and saves
    let (s, b) = w
        .save("rerank", w.input("cohere", "rerank-v3.5", Some(GOOD), 0))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn save_internal_kind_422_internal_provider_only() {
    let w = world().await;
    for kind in ["mlx_api", "pgx_vllm"] {
        let (s, b) = w
            .save("embed", w.input(kind, "BAAI/bge-m3", Some(GOOD), 0))
            .await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "internal_provider_only"),
            "{kind}: {b}"
        );
    }
    // a kind the slot does not take
    let (s, b) = w
        .save("rerank", w.input("openai_compatible", "m", Some(GOOD), 0))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "unknown_kind"),
        "{b}"
    );
    let (s, b) = w
        .save("embed", w.input("openai_compatible", " ", Some(GOOD), 0))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "model_required"),
        "{b}"
    );
    let (s, b) = w
        .save("nope", w.input("openai_compatible", "m", Some(GOOD), 0))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "slot_not_found"),
        "{b}"
    );
    let mut bad = w.input("openai_compatible", "m", Some(GOOD), 0);
    bad["base_url"] = json!("ftp://x.example");
    let (s, b) = w.save("embed", bad).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_base_url"),
        "{b}"
    );
    assert_eq!(
        w.count("SELECT count(*) FROM provider_slot WHERE tenant_id = $1")
            .await,
        0
    );
}

#[tokio::test]
async fn save_internal_tenant_422_internal_uses_mlx_api() {
    let w = world().await;
    sqlx::query("UPDATE tenants SET platform_owned = true WHERE id = $1")
        .bind(w.tenant)
        .execute(&w.e.owner)
        .await
        .unwrap();
    let (s, b) = w
        .save(
            "embed",
            w.input("openai_compatible", "mistral-embed", Some(GOOD), 0),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "internal_uses_mlx_api"),
        "{b}"
    );
}

#[tokio::test]
async fn save_new_endpoint_without_key_422_secret_required() {
    let w = world().await;
    // no key stored, none typed
    let (s, b) = w
        .save(
            "embed",
            w.input("openai_compatible", "mistral-embed", None, 0),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "secret_required"),
        "{b}"
    );
    save_embed(&w).await;
    // a new endpoint never reuses the stored key
    let mut moved = w.input("openai_compatible", "mistral-embed", None, 1);
    moved["base_url"] = json!(w.url("/alt"));
    let (s, b) = w.save("embed", moved).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "secret_required"),
        "{b}"
    );
    // nor does a new kind
    let (s, b) = w
        .save("embed", w.input("voyage", "voyage-3.5", None, 1))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "secret_required"),
        "{b}"
    );
    // same kind and endpoint: the stored key is kept (and probed with)
    let (s, b) = w
        .save(
            "embed",
            w.input("openai_compatible", "mistral-embed-2", None, 1),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["model"], "mistral-embed-2");
    assert_eq!(b["secret"]["set"], true);
    let last = w.seen.0.lock().unwrap().last().cloned().flatten();
    assert_eq!(last.as_deref(), Some(format!("Bearer {GOOD}").as_str()));
    let key: Value = sqlx::query_scalar(
        "SELECT details->'key' FROM audit_event WHERE tenant_id = $1 AND action = 'provider_slot.save' \
          ORDER BY id DESC LIMIT 1",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(key, "kept");
}

#[tokio::test]
async fn save_probe_401_422_slot_test_failed() {
    let w = world().await;
    let (s, b) = w
        .save(
            "embed",
            w.input("openai_compatible", "mistral-embed", Some(WRONG), 0),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "slot_test_failed"),
        "{b}"
    );
    let msg = b["error"]["message"].as_str().unwrap();
    assert!(msg.contains("127.0.0.1"), "names the host: {msg}");
    assert!(!b.to_string().contains(WRONG), "never the key: {b}");
    assert_eq!(
        w.seen.0.lock().unwrap().as_slice(),
        [Some(format!("Bearer {WRONG}"))],
        "probed once with the typed key"
    );
    // nothing stored
    assert_eq!(
        w.count("SELECT count(*) FROM provider_slot WHERE tenant_id = $1")
            .await,
        0
    );
    assert_eq!(
        w.count("SELECT count(*) FROM provider_slot_secret WHERE tenant_id = $1")
            .await,
        0
    );
}

// ---- save writes ----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn save_writes_working_row_secret_and_draft() {
    let w = world().await;
    let (f, _rm) = with_job(&w).await;
    let rev0: i64 = sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
        .bind(f.job.job_id)
        .fetch_one(&w.e.owner)
        .await
        .unwrap();
    let view = save_embed(&w).await;
    assert_eq!(view["version"], 1);
    let (secret_id, sealed, fpr): (Uuid, String, String) = sqlx::query_as(
        "SELECT id, key_sealed, fingerprint FROM provider_slot_secret WHERE tenant_id = $1",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert!(sealed.starts_with("v2:") && !sealed.contains(GOOD));
    assert_eq!(view["secret"]["fingerprint"], fpr);
    let row: WorkingRow = sqlx::query_as(
        "SELECT configured, kind, base_url, model, version, secret_id, key_kept FROM provider_slot \
          WHERE tenant_id = $1 AND slot = 'embed'",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert!(row.configured && !row.key_kept, "{row:?}");
    assert_eq!(row.kind.as_deref(), Some("openai_compatible"));
    assert_eq!(row.base_url, Some(w.url("/ok")));
    assert_eq!(row.model.as_deref(), Some("mistral-embed"));
    assert_eq!((row.version, row.secret_id), (1, Some(secret_id)));
    // the draft: a ledger row in the job, draft_rev bumped by the engine
    let (kind, after): (String, Value) = sqlx::query_as(
        "SELECT kind, after FROM draft_ledger WHERE job_id = $1 AND key = 'slot:embed'",
    )
    .bind(f.job.job_id)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(kind, "provider_slot");
    assert_eq!(
        after,
        json!({"v": {"configured": true, "kind": "openai_compatible", "base_url": w.url("/ok"),
                     "model": "mistral-embed", "secret": {"set": true, "fingerprint": fpr},
                     "secret_id": secret_id.to_string()}})
    );
    let rev1: i64 = sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
        .bind(f.job.job_id)
        .fetch_one(&w.e.owner)
        .await
        .unwrap();
    assert!(rev1 > rev0);
    // D69: nothing is live on save — no strata call, no outbox row
    assert!(w.fake.rag_requests().is_empty() && w.fake.ingest_requests().is_empty());
    assert_eq!(
        w.count(
            "SELECT count(*) FROM provision_outbox WHERE tenant_id = $1 AND op = 'connections_put'"
        )
        .await,
        0
    );
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE tenant_id = $1 AND action = 'provider_slot.save'",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(details["key"], "set");
    assert_eq!(details["draft"], true);
}

#[tokio::test]
async fn save_never_leaks_key() {
    let w = world().await;
    let (f, rm) = with_job(&w).await;
    let view = save_embed(&w).await;
    assert!(!view.to_string().contains(GOOD), "{view}");
    publish(&w, &f, &rm).await;
    let list = Value::Array(w.slots().await);
    assert!(!list.to_string().contains(GOOD));
    for (what, sql) in [
        (
            "audit",
            "SELECT coalesce(string_agg(details::text, ' '), '') FROM audit_event WHERE tenant_id = $1",
        ),
        (
            "ledger",
            "SELECT coalesce(string_agg(coalesce(after::text, '') || label, ' '), '') FROM draft_ledger \
              WHERE tenant_id = $1",
        ),
        (
            "outbox",
            "SELECT coalesce(string_agg(payload::text, ' '), '') FROM provision_outbox WHERE tenant_id = $1",
        ),
        (
            "snapshot",
            "SELECT coalesce(string_agg(snap::text, ' '), '') FROM release_snapshot WHERE tenant_id = $1",
        ),
    ] {
        let text: String = sqlx::query_scalar(sql)
            .bind(w.tenant)
            .fetch_one(&w.e.owner)
            .await
            .unwrap();
        assert!(!text.contains(GOOD), "{what} holds the key: {text}");
    }
    let payload = w.outbox("strata_rag:connections_put").await.payload;
    assert!(audit::scrub::check(&payload).is_ok(), "{payload}");
    // strata echoing the key back in a refusal: scrubbed before it is stored
    lock(&w.fake.rag).script(
        "PUT",
        "/api/tenant/providers",
        422,
        json!({"error": {"code": "probe_failed", "message": format!("embed: provider refused key {GOOD}")}}),
    );
    drain_once(&w.e.state).await;
    let row = w.outbox("strata_rag:connections_put").await;
    assert_eq!(row.status, "failed");
    assert!(!row.last_error.unwrap_or_default().contains(GOOD));
}

// ---- publish → connections_put ---------------------------------------------------------------------------------------

#[tokio::test]
async fn published_enqueues_connections_put() {
    let w = world().await;
    let (f, rm) = with_job(&w).await;
    save_embed(&w).await;
    let (s, b) = w
        .save(
            "ocr",
            w.input("mistral_ocr", "mistral-ocr-latest", Some(GOOD), 0),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        w.count("SELECT count(*) FROM provision_outbox WHERE tenant_id = $1")
            .await,
        0,
        "nothing is queued before a publish"
    );
    let release = publish(&w, &f, &rm).await;
    let embed_key: Uuid = sqlx::query_scalar(
        "SELECT secret_id FROM provider_slot WHERE tenant_id = $1 AND slot = 'embed'",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    let rag = w.outbox("strata_rag:connections_put").await;
    assert_eq!(rag.status, "pending");
    let payload = rag.payload;
    assert_eq!(
        payload,
        json!({"slots": [{"slot": "embed", "configured": true, "kind": "openai_compatible",
                          "base_url": w.url("/ok"), "model": "mistral-embed",
                          "key_ref": embed_key.to_string(), "version": 1, "release": release}]})
    );
    assert!(audit::scrub::check(&payload).is_ok());
    let ing = w.outbox("strata_ingest:connections_put").await;
    assert_eq!(ing.status, "pending");
    assert_eq!(ing.payload["slots"][0]["slot"], "ocr");
    assert_eq!(ing.payload["slots"].as_array().unwrap().len(), 1);
    assert_eq!(
        w.count(
            "SELECT count(*) FROM provision_outbox WHERE tenant_id = $1 AND op = 'connections_put'"
        )
        .await,
        2,
        "one row per service"
    );
    let actor: String = sqlx::query_scalar(
        "SELECT actor_id FROM audit_event WHERE tenant_id = $1 AND action = 'provision.enqueued' LIMIT 1",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(actor, slots::PUBLISHER);
    // the publish itself called nothing
    assert!(World::provider_calls(&w.fake.rag).is_empty());
    assert!(World::provider_calls(&w.fake.ingest).is_empty());
}

// ---- the sender -----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn sender_reads_full_set_and_replaces_one() {
    let w = world().await;
    let (f, rm) = with_job(&w).await;
    // strata already holds a rerank registration (with a key) and serves chat from its default
    w.set_remote(
        &w.fake.rag,
        json!({
            "embed": {"source": "unconfigured"},
            "rerank": {"source": "tenant", "base_url": "https://rerank.example/v2", "dialect": "cohere",
                       "model": "rerank-v3.5", "model_id": null, "dims": null, "regions": ["eu"], "zdr": true,
                       "timeout_ms": 10000, "concurrency": 8, "has_api_key": true, "updated_at": "t0"},
            "chat": {"source": "default", "dialect": "openai", "model": "local-llm"},
        }),
    );
    save_embed(&w).await;
    publish(&w, &f, &rm).await;

    // the read-modify-write waits for the tenant's service lock
    let name = slots::push_lock(w.tenant, Target::StrataRag);
    let mut held = w.e.owner.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
        .bind(&name)
        .execute(&mut *held)
        .await
        .unwrap();
    let st = w.e.state.clone();
    let drain = tokio::spawn(async move { drain_once(&st).await });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        World::provider_calls(&w.fake.rag).is_empty(),
        "no strata call while another push holds the lock"
    );
    sqlx::query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
        .bind(&name)
        .execute(&mut *held)
        .await
        .unwrap();
    drain.await.unwrap();

    let calls = World::provider_calls(&w.fake.rag);
    let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(
        methods,
        ["GET", "PUT"],
        "read the whole set, then replace it"
    );
    let body = &calls[1].1;
    assert_eq!(
        body["embed"],
        json!({"base_url": w.url("/ok"), "dialect": "openai", "model": "mistral-embed", "regions": ["eu"],
               "zdr": true, "dims": 1024, "api_key": GOOD}),
        "the published slot, with its unsealed key"
    );
    assert_eq!(
        body["rerank"],
        json!({"base_url": "https://rerank.example/v2", "dialect": "cohere", "model": "rerank-v3.5",
               "regions": ["eu"], "zdr": true, "timeout_ms": 10000, "concurrency": 8}),
        "another registration kept as strata reports it, without api_key"
    );
    assert_eq!(body["chat"], Value::Null, "a default is not registered");
    assert!(World::provider_calls(&w.fake.ingest).is_empty());
    assert_eq!(
        w.outbox("strata_rag:connections_put").await.status,
        "succeeded"
    );
    // strata kept rerank's key (same base_url)
    assert_eq!(
        lock(&w.fake.rag).tenant_providers[&w.tenant]["rerank"]["has_api_key"],
        true
    );
}

#[tokio::test]
async fn sender_records_pushed_fpr() {
    let w = world().await;
    let (_f, _rm, release) = pushed_embed(&w).await;
    let held = lock(&w.fake.rag).tenant_providers[&w.tenant]["embed"].clone();
    let want = slots::remote_fpr(Some(&held));
    assert_ne!(want, slots::NONE_FPR);
    let p: Pushed = sqlx::query_as(
        "SELECT pushed_version, pushed_fpr, pushed_release, observed_fpr FROM provider_slot_remote \
          WHERE tenant_id = $1 AND slot = 'embed'",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(p.pushed_version, 1, "{p:?}");
    assert_eq!(p.pushed_fpr, want);
    assert_eq!(p.pushed_release.as_deref(), Some(release.as_str()));
    assert_eq!(p.observed_fpr.as_deref(), Some(want.as_str()));
    // what strata reports is what was pushed: no drift
    assert!(w.slot("embed").await["drift"].is_null());
}

#[tokio::test]
async fn sender_422_from_strata_fails_row_with_code() {
    let w = world().await;
    let (f, rm) = with_job(&w).await;
    save_embed(&w).await;
    publish(&w, &f, &rm).await;
    lock(&w.fake.rag).script(
        "PUT",
        "/api/tenant/providers",
        422,
        json!({"error": {"code": "probe_failed",
                         "message": format!("embed: probe failed: 401 from the provider for key {GOOD}")}}),
    );
    drain_once(&w.e.state).await;
    let row = w.outbox("strata_rag:connections_put").await;
    assert_eq!(row.status, "failed", "the B1 chip shows failed");
    assert_eq!(row.last_error_code.as_deref(), Some("slot_test_failed"));
    let last_error = row.last_error.unwrap();
    assert!(
        last_error.contains("probe_failed"),
        "strata's code is kept: {last_error}"
    );
    assert!(!last_error.contains(GOOD), "{last_error}");
    let failed: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE tenant_id = $1 AND action = 'provision.failed'",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(failed["code"], "slot_test_failed");
    // nothing recorded as pushed
    assert_eq!(
        w.count("SELECT count(*) FROM provider_slot_remote WHERE tenant_id = $1 AND pushed_at IS NOT NULL")
            .await,
        0
    );
}

// ---- drift ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn remote_change_shows_drift() {
    let w = world().await;
    pushed_embed(&w).await;
    // another app changes the model behind agent's back
    {
        let mut g = lock(&w.fake.rag);
        let view = g.tenant_providers.get_mut(&w.tenant).unwrap();
        view["embed"]["model"] = json!("mistral-embed-2");
    }
    let list = w.slots().await;
    let d = &list[0]["drift"];
    assert_eq!(d["remote_version"], 2, "{d}");
    assert_eq!(d["changed_by"], "another app");
    assert!(d["at"].is_string(), "{d}");
    assert_eq!(
        d["remote"],
        json!({"kind": "openai_compatible", "base_url": w.url("/ok"), "model": "mistral-embed-2"})
    );
    assert!(list[1..].iter().all(|s| s["drift"].is_null()));
    // the observation's time stays put while the remote does not change again
    let again = w.slot("embed").await;
    assert_eq!(again["drift"]["at"], d["at"]);
    let (s, b) = w
        .save(
            "embed",
            w.input("openai_compatible", "mistral-embed", Some(GOOD), 1),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "VERSION_CONFLICT"),
        "{b}"
    );
}

#[tokio::test]
async fn adopt_drift_drafts_remote_values_and_clears() {
    let w = world().await;
    let (f, rm, _) = pushed_embed(&w).await;
    {
        let mut g = lock(&w.fake.rag);
        let view = g.tenant_providers.get_mut(&w.tenant).unwrap();
        view["embed"]["model"] = json!("mistral-embed-2");
    }
    assert!(!w.slot("embed").await["drift"].is_null());
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            "/api/providers/slots/embed/adopt-remote",
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["model"], "mistral-embed-2");
    assert_eq!(b["kind"], "openai_compatible");
    assert_eq!(b["version"], 2);
    assert_eq!(
        b["secret"],
        json!({"set": true, "updated_at": null, "fingerprint": null}),
        "the key strata holds"
    );
    assert!(b["drift"].is_null(), "{b}");
    let (sid, kept): (Option<Uuid>, bool) = sqlx::query_as(
        "SELECT secret_id, key_kept FROM provider_slot WHERE tenant_id = $1 AND slot = 'embed'",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!((sid, kept), (None, true), "key kept by strata");
    let after: Value = sqlx::query_scalar(
        "SELECT after FROM draft_ledger WHERE job_id = $1 AND key = 'slot:embed'",
    )
    .bind(f.job.job_id)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(after["v"]["secret_id"], Value::Null, "{after}");
    assert_eq!(after["v"]["model"], "mistral-embed-2");
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE tenant_id = $1 AND action = 'provider_slot.adopt_remote'",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(n, 1);
    // published: the next push sends the adopted registration and no api_key for it
    publish(&w, &f, &rm).await;
    drain_once(&w.e.state).await;
    let calls = World::provider_calls(&w.fake.rag);
    let (method, body) = calls.last().unwrap();
    assert_eq!(method, "PUT");
    assert_eq!(body["embed"]["model"], "mistral-embed-2");
    assert!(body["embed"].get("api_key").is_none(), "{body}");
    assert!(w.slot("embed").await["drift"].is_null());
}

#[tokio::test]
async fn adopt_without_drift_409_no_drift() {
    let w = world().await;
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            "/api/providers/slots/embed/adopt-remote",
            Value::Null,
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "no_drift"), "{b}");
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            "/api/providers/slots/nope/adopt-remote",
            Value::Null,
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "slot_not_found"),
        "{b}"
    );
}

#[tokio::test]
async fn slot_writes_need_integrations_manager() {
    let w = world().await;
    let km = member(&w.e, w.tenant, "knowledge_manager").await;
    for tok in [&w.obs, &km] {
        let (s, b) = w
            .call(
                tok,
                "PUT",
                "/api/providers/slots/embed",
                w.input("openai_compatible", "m", Some(GOOD), 0),
            )
            .await;
        assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{b}");
        let (s, b) = w
            .call(
                tok,
                "POST",
                "/api/providers/slots/embed/adopt-remote",
                Value::Null,
            )
            .await;
        assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{b}");
    }
    assert!(w.seen.0.lock().unwrap().is_empty());
}

// ---- the design kind ------------------------------------------------------------------------------------------------

fn slot_ref(slot: &str) -> DraftRef {
    let mut r = DraftRef::new();
    r.insert("slot".into(), json!(slot));
    r
}

#[tokio::test]
async fn provider_slot_full_contract() {
    let e = common::env_with(with_keys).await;
    design_contract::run_all(
        &e,
        "provider_slot",
        &[
            (
                slot_ref("embed"),
                json!({"configured": true, "kind": "openai_compatible", "base_url": "https://api.mistral.ai/v1",
                       "model": "mistral-embed", "secret": {"set": false, "fingerprint": null},
                       "secret_id": null}),
            ),
            (
                slot_ref("ocr"),
                json!({"configured": true, "kind": "mistral_ocr", "base_url": "https://api.mistral.ai/v1",
                       "model": "mistral-ocr-latest", "secret": {"set": true, "fingerprint": null},
                       "secret_id": null}),
            ),
            // T4b (F-08): a slot reset to the deployment default
            (
                slot_ref("rerank"),
                json!({"configured": true, "source": "default", "kind": null, "base_url": null, "model": null,
                       "secret": {"set": false, "fingerprint": null}, "secret_id": null}),
            ),
        ],
        Some("not configured"),
    )
    .await;
}

// ---- T4a ruling: an embed change never retargets an existing root -----------------------------------------------

/// Every request the rag fake received from index `from` on: (method, path, body).
fn rag_since(w: &World, from: usize) -> Vec<(String, String, Value)> {
    lock(&w.fake.rag).requests[from..]
        .iter()
        .map(|r| (r.method.clone(), r.path.clone(), r.body.clone()))
        .collect()
}

#[tokio::test]
async fn embed_change_pins_existing_roots_before_the_tenant_push() {
    let w = world().await;
    let (f, rm, _) = pushed_embed(&w).await;
    // a root strata made with the pushed model (it inherits the tenant row)
    let old_root = lock(&w.fake.rag).add_store(w.tenant, None, "leaf", "kb-old");
    assert_eq!(
        lock(&w.fake.rag).embed_model_of(old_root),
        "mistral-embed@1024"
    );
    // a new embed model (same endpoint, key kept), published
    let (s, b) = w
        .save(
            "embed",
            w.input("openai_compatible", "mistral-embed-2", None, 1),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    publish(&w, &f, &rm).await;
    let from = lock(&w.fake.rag).requests.len();
    drain_once(&w.e.state).await;
    let row = w.outbox("strata_rag:connections_put").await;
    assert_eq!(row.status, "succeeded", "{:?}", row.last_error);
    let reqs = rag_since(&w, from);
    let pin_path = format!("/api/stores/{old_root}/providers");
    let pin_at = reqs
        .iter()
        .position(|(m, p, _)| m == "PUT" && *p == pin_path)
        .expect("the existing root is pinned at store level");
    let tenant_at = reqs
        .iter()
        .position(|(m, p, _)| m == "PUT" && p == "/api/tenant/providers")
        .expect("the tenant push");
    assert!(pin_at < tenant_at, "the pin lands before the tenant PUT");
    let pin = &reqs[pin_at].2;
    assert_eq!(pin["embed"]["model"], "mistral-embed", "its CURRENT model");
    assert_eq!(pin["embed"]["base_url"], w.url("/ok"));
    assert_eq!(pin["embed"]["dims"], 1024);
    assert_eq!(
        pin["embed"]["api_key"], GOOD,
        "the key of the registration strata held (to strata only)"
    );
    assert_eq!(
        reqs.iter()
            .filter(|(m, p, _)| m == "PUT" && p.starts_with("/api/stores/"))
            .count(),
        1
    );
    assert_eq!(
        reqs[tenant_at].2["embed"]["model"], "mistral-embed-2",
        "the tenant row takes the new default"
    );
    {
        let g = lock(&w.fake.rag);
        assert_eq!(g.embed_model_of(old_root), "mistral-embed@1024");
        assert_eq!(
            g.tenant_providers[&w.tenant]["embed"]["model"],
            "mistral-embed-2"
        );
    }
    // a root created afterwards takes the new tenant default
    let new_root = lock(&w.fake.rag).add_store(w.tenant, None, "leaf", "kb-new");
    assert_eq!(
        lock(&w.fake.rag).embed_model_of(new_root),
        "mistral-embed-2@1024"
    );
    // the key never lands in what agent keeps of the push
    let kept: String = sqlx::query_scalar::<_, String>(
        "SELECT coalesce(result::text, '') || coalesce(last_error, '') || payload::text FROM provision_outbox \
          WHERE tenant_id = $1 AND dedupe_key = 'strata_rag:connections_put'",
    )
    .bind(w.tenant)
    .fetch_all(&w.e.owner)
    .await
    .unwrap()
    .concat();
    assert!(!kept.contains(GOOD));
    // idempotent: a later push of the same model pins nothing again
    let (s, b) = w
        .save(
            "rerank",
            json!({"kind": "cohere", "base_url": w.url("/ok"), "model": "rerank-v3.5", "secret": GOOD,
                   "expected_version": 0}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    publish(&w, &f, &rm).await;
    let from = lock(&w.fake.rag).requests.len();
    drain_once(&w.e.state).await;
    assert!(
        rag_since(&w, from)
            .iter()
            .all(|(m, p, _)| !(m == "PUT" && p.starts_with("/api/stores/"))),
        "no embed change, no pin"
    );
}

/// A tree strata built while the tenant ran its deployment-default embed (a router root with a leaf: content).
fn default_tree(w: &World) -> (Uuid, Uuid) {
    let mut g = lock(&w.fake.rag);
    let root = g.add_store(w.tenant, None, "router", "kb");
    let leaf = g.add_store(w.tenant, Some(root), "leaf", "faq");
    assert_eq!(g.embed_model_of(leaf), strata_fake::DEFAULT_EMBED);
    (root, leaf)
}

/// The newest `connections_put` result of strata_rag.
async fn rag_result(w: &World) -> Value {
    sqlx::query_scalar::<_, Option<Value>>(
        "SELECT result FROM provision_outbox WHERE tenant_id = $1 AND dedupe_key = 'strata_rag:connections_put' \
          ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap()
    .unwrap_or(Value::Null)
}

// ruling 2026-10-21 (revised): strata cannot pin its deployment default per store; a root on it that still holds
// content withholds the tenant embed (the move itself is tests/b3/model_changes.rs)
#[tokio::test]
async fn save_on_default_embed_with_only_empty_roots_pushes_plainly() {
    let w = world().await;
    let (f, rm) = with_job(&w).await;
    let empty = lock(&w.fake.rag).add_store(w.tenant, None, "leaf", "kb-empty");
    save_embed(&w).await;
    publish(&w, &f, &rm).await;
    drain_once(&w.e.state).await;
    let row = w.outbox("strata_rag:connections_put").await;
    assert_eq!(row.status, "succeeded", "{:?}", row.last_error);
    assert!(rag_result(&w).await.get("withheld").is_none());
    let puts: Vec<Value> = World::provider_calls(&w.fake.rag)
        .into_iter()
        .filter(|(m, _)| m == "PUT")
        .map(|(_, b)| b)
        .collect();
    assert_eq!(puts.len(), 1);
    assert_eq!(
        puts[0]["embed"]["model"], "mistral-embed",
        "the tenant row moved"
    );
    assert!(
        !lock(&w.fake.rag).store_providers.contains_key(&empty),
        "an empty root is not pinned"
    );
}

#[tokio::test]
async fn push_withholds_embed_while_a_default_root_holds_content() {
    let w = world().await;
    let (f, rm) = with_job(&w).await;
    // strata holds a tree on its default that agent did not make (it counts as read)
    let (root, _) = default_tree(&w);
    save_embed(&w).await;
    let (s, b) = w
        .save(
            "rerank",
            json!({"kind": "cohere", "base_url": w.url("/ok"), "model": "rerank-v3.5", "secret": GOOD,
                   "expected_version": 0}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "the save is never refused: {b}");
    publish(&w, &f, &rm).await;
    drain_once(&w.e.state).await;
    let row = w.outbox("strata_rag:connections_put").await;
    assert_eq!(row.status, "succeeded", "{:?}", row.last_error);
    let result = rag_result(&w).await;
    assert_eq!(result["withheld"][0]["slot"], "embed", "{result}");
    assert_eq!(result["withheld"][0]["model"], "mistral-embed");
    assert!(!result.to_string().contains(GOOD));
    let puts: Vec<Value> = World::provider_calls(&w.fake.rag)
        .into_iter()
        .filter(|(m, _)| m == "PUT")
        .map(|(_, b)| b)
        .collect();
    assert_eq!(puts.len(), 1);
    assert_eq!(
        puts[0]["embed"],
        Value::Null,
        "the tenant embed stays on the default"
    );
    assert_eq!(
        puts[0]["rerank"]["model"], "rerank-v3.5",
        "the other slots go out"
    );
    assert!(!lock(&w.fake.rag).store_providers.contains_key(&root));
    assert_eq!(
        w.count(
            "SELECT count(*) FROM provider_slot_remote WHERE tenant_id = $1 AND slot = 'embed' \
              AND pushed_fpr <> 'none'"
        )
        .await,
        0,
        "a withheld entry is not recorded as pushed"
    );
    // review I4: the embed slot shows it, with the roots that hold it back
    let embed = w.slot("embed").await;
    assert_eq!(embed["withheld"]["model"], "mistral-embed", "{embed}");
    assert_eq!(embed["withheld"]["roots"], json!(["kb"]), "{embed}");
    assert!(w.slot("rerank").await["withheld"].is_null());
    // retried by hand: still withheld while the root holds content
    let (s, b) = w
        .call(&w.im, "POST", "/api/providers/slots/embed/push", json!({}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["push_id"].is_string(), "{b}");
    assert_eq!(
        w.outbox("strata_rag:connections_put").await.status,
        "pending"
    );
    drain_once(&w.e.state).await;
    assert_eq!(rag_result(&w).await["withheld"][0]["slot"], "embed");
    // pushed once the root is gone (the retry, or the sweeper's requeue)
    let mut tx = agent_api::tenancy::begin_scoped(&w.e.state.db, w.tenant)
        .await
        .unwrap();
    assert!(
        slots::requeue_withheld(&mut tx, w.tenant)
            .await
            .unwrap()
            .is_some()
    );
    tx.commit().await.unwrap();
    {
        let mut g = lock(&w.fake.rag);
        g.stores.retain(|s| s.tenant != w.tenant);
    }
    drain_once(&w.e.state).await;
    assert!(rag_result(&w).await.get("withheld").is_none());
    let last = World::provider_calls(&w.fake.rag)
        .into_iter()
        .rev()
        .find(|(m, _)| m == "PUT")
        .unwrap()
        .1;
    assert_eq!(last["embed"]["model"], "mistral-embed");
    assert_eq!(
        last["rerank"]["model"], "rerank-v3.5",
        "kept as strata holds it"
    );
    assert!(w.slot("embed").await["withheld"].is_null());
    // nothing left to push
    let (s, b) = w
        .call(&w.im, "POST", "/api/providers/slots/embed/push", json!({}))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "nothing_to_push"),
        "{b}"
    );
}

/// Review I2: a push strata refused is not lost when the next publish of the service is queued.
#[tokio::test]
async fn a_failed_push_is_merged_into_the_next_one() {
    let w = world().await;
    let (f, rm) = with_job(&w).await;
    save_rerank(&w).await;
    publish(&w, &f, &rm).await;
    lock(&w.fake.rag).script(
        "PUT",
        "/api/tenant/providers",
        422,
        json!({"error": {"code": "provider_rejected", "message": "nope"}}),
    );
    drain_once(&w.e.state).await;
    let first = w.outbox("strata_rag:connections_put").await;
    assert_eq!(first.status, "failed", "{:?}", first.last_error);
    save_chat(&w).await;
    publish(&w, &f, &rm).await;
    let row = w.outbox("strata_rag:connections_put").await;
    assert_eq!(row.status, "pending");
    let slots_of: Vec<&str> = row.payload["slots"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["slot"].as_str())
        .collect();
    assert_eq!(
        slots_of,
        ["rerank", "chat"],
        "the failed entry is re-merged"
    );
    assert_eq!(
        w.count(
            "SELECT count(*) FROM provision_outbox WHERE tenant_id = $1 AND status = 'superseded' \
               AND dedupe_key = 'strata_rag:connections_put'"
        )
        .await,
        1
    );
    drain_once(&w.e.state).await;
    assert_eq!(
        w.outbox("strata_rag:connections_put").await.status,
        "succeeded"
    );
    let put = World::provider_calls(&w.fake.rag)
        .into_iter()
        .rev()
        .find(|(m, _)| m == "PUT")
        .unwrap()
        .1;
    assert_eq!(put["rerank"]["model"], "rerank-v3.5");
    assert_eq!(put["chat"]["model"], "chat-1");
}

/// Review I2: a row retried while a newer push of its service is pending is superseded (never a second pending row,
/// never a finish that fails forever).
#[tokio::test]
async fn a_retry_while_a_newer_push_is_pending_is_superseded() {
    let w = world().await;
    let (f, rm) = with_job(&w).await;
    save_rerank(&w).await;
    publish(&w, &f, &rm).await;
    let first: Uuid = sqlx::query_scalar(
        "UPDATE provision_outbox SET status = 'in_flight', lease_token = gen_random_uuid(), \
                lease_until = now() - interval '1 second' \
          WHERE tenant_id = $1 AND dedupe_key = 'strata_rag:connections_put' RETURNING id",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    // published while the first is in flight: a new pending row carrying both entries
    save_chat(&w).await;
    publish(&w, &f, &rm).await;
    let second = w.outbox("strata_rag:connections_put").await;
    assert_eq!(second.status, "pending");
    assert_eq!(second.payload["slots"].as_array().unwrap().len(), 2);
    lock(&w.fake.rag).script("PUT", "/api/tenant/providers", 503, json!({}));
    let r = drain_once(&w.e.state).await;
    assert_eq!((r.claimed, r.errors), (1, 0), "{r:?}");
    let status: String =
        sqlx::query_scalar("SELECT status::text FROM provision_outbox WHERE id = $1")
            .bind(first)
            .fetch_one(&w.e.owner)
            .await
            .unwrap();
    assert_eq!(status, "superseded", "the retry yields to the newer row");
    drain_once(&w.e.state).await;
    assert_eq!(
        w.outbox("strata_rag:connections_put").await.status,
        "succeeded"
    );
    let put = World::provider_calls(&w.fake.rag)
        .into_iter()
        .rev()
        .find(|(m, _)| m == "PUT")
        .unwrap()
        .1;
    assert_eq!(put["rerank"]["model"], "rerank-v3.5");
    assert_eq!(put["chat"]["model"], "chat-1");
}

/// Save `rerank` (cohere on the mock, key GOOD) at version 0.
async fn save_rerank(w: &World) {
    let (s, b) = w
        .save(
            "rerank",
            json!({"kind": "cohere", "base_url": w.url("/ok"), "model": "rerank-v3.5", "secret": GOOD,
                   "expected_version": 0}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// Save `chat` (openai_compatible on the mock, key GOOD) at version 0.
async fn save_chat(w: &World) {
    let (s, b) = w
        .save(
            "chat",
            w.input("openai_compatible", "chat-1", Some(GOOD), 0),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// Roots on a TENANT registration keep the pin-before-push path (the save is not refused); a pin agent cannot make
/// (strata holds a key agent did not push) still fails the row `embed_pin_failed`.
#[tokio::test]
async fn tenant_registration_root_that_cannot_be_pinned_fails_embed_pin_failed() {
    let w = world().await;
    let (f, rm, _) = pushed_embed(&w).await;
    let root = {
        let mut g = lock(&w.fake.rag);
        let root = g.add_store(w.tenant, None, "router", "kb-tenant");
        g.add_store(w.tenant, Some(root), "leaf", "faq");
        root
    };
    sqlx::query(
        "UPDATE provider_slot_remote SET pushed_key_ref = NULL WHERE tenant_id = $1 AND slot = 'embed'",
    )
    .bind(w.tenant)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let (s, b) = w
        .save(
            "embed",
            w.input("openai_compatible", "mistral-embed-2", None, 1),
        )
        .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "a tenant-registration root does not block the save: {b}"
    );
    publish(&w, &f, &rm).await;
    let puts_before = World::provider_calls(&w.fake.rag)
        .iter()
        .filter(|(m, _)| m == "PUT")
        .count();
    drain_once(&w.e.state).await;
    let row = w.outbox("strata_rag:connections_put").await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.last_error_code.as_deref(), Some("embed_pin_failed"));
    assert_eq!(
        World::provider_calls(&w.fake.rag)
            .iter()
            .filter(|(m, _)| m == "PUT")
            .count(),
        puts_before,
        "the tenant row is not touched"
    );
    assert!(!lock(&w.fake.rag).store_providers.contains_key(&root));
}

// ---- T4b: "Use default" (F-08) ----------------------------------------------------------------------------------

impl World {
    async fn reset(&self, tok: &str, slot: &str, version: i64) -> (StatusCode, Value) {
        self.call(
            tok,
            "POST",
            &format!("/api/providers/slots/{slot}/reset"),
            json!({"expected_version": version}),
        )
        .await
    }

    async fn working(&self, slot: &str) -> WorkingRow {
        sqlx::query_as(
            "SELECT configured, kind, base_url, model, version, secret_id, key_kept FROM provider_slot \
              WHERE tenant_id = $1 AND slot = $2",
        )
        .bind(self.tenant)
        .bind(slot)
        .fetch_one(&self.e.owner)
        .await
        .unwrap()
    }

    async fn source(&self, slot: &str) -> String {
        sqlx::query_scalar("SELECT source FROM provider_slot WHERE tenant_id = $1 AND slot = $2")
            .bind(self.tenant)
            .bind(slot)
            .fetch_one(&self.e.owner)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn reset_writes_default_and_bumps_version() {
    let w = world().await;
    save_embed(&w).await;
    let (s, b) = w.reset(&w.im, "embed", 1).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["source"], "default");
    assert_eq!(b["configured"], true, "the default is deployed");
    assert_eq!(b["kind"], Value::Null);
    assert_eq!(b["base_url"], Value::Null);
    assert_eq!(b["model"], "BAAI/bge-m3", "the deployment default's model");
    assert_eq!(b["version"], 2);
    assert_eq!(b["secret"]["set"], false);
    let row = w.working("embed").await;
    assert!(row.configured, "{row:?}");
    assert_eq!(
        (
            row.kind,
            row.base_url,
            row.model,
            row.secret_id,
            row.key_kept,
            row.version
        ),
        (None, None, None, None, false, 2)
    );
    assert_eq!(w.source("embed").await, "default");
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE tenant_id = $1 AND action = 'provider_slot.reset'",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(details["slot"], "embed");
    assert_eq!(details["from_source"], "tenant");
    // the list says it too, and a save brings the tenant's own provider back
    assert_eq!(w.slot("embed").await["source"], "default");
    let (s, b) = w
        .save(
            "embed",
            w.input("openai_compatible", "mistral-embed", Some(GOOD), 2),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["source"], "tenant");
    assert_eq!(w.source("embed").await, "tenant");
}

#[tokio::test]
async fn reset_stale_version_409() {
    let w = world().await;
    save_embed(&w).await;
    let (s, b) = w.reset(&w.im, "embed", 0).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "VERSION_CONFLICT"),
        "{b}"
    );
    assert_eq!(w.source("embed").await, "tenant");
    assert_eq!(w.working("embed").await.version, 1);
    assert_eq!(
        w.count("SELECT count(*) FROM provision_outbox WHERE tenant_id = $1")
            .await,
        0
    );
}

#[tokio::test]
async fn reset_twice_409_already_default() {
    let w = world().await;
    save_embed(&w).await;
    let (s, b) = w.reset(&w.im, "embed", 1).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w.reset(&w.im, "embed", 2).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "already_default"),
        "{b}"
    );
    // a slot never configured already runs the default
    let (s, b) = w.reset(&w.im, "rerank", 0).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "already_default"),
        "{b}"
    );
    let (s, b) = w.reset(&w.im, "nope", 0).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "slot_not_found"),
        "{b}"
    );
}

#[tokio::test]
async fn reset_without_deployment_default_409() {
    let w = world().await;
    let (s, b) = w
        .save(
            "transcribe",
            w.input("openai_compatible", "whisper-1", Some(GOOD), 0),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w.reset(&w.im, "transcribe", 1).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "no_deployment_default"),
        "{b}"
    );
    assert_eq!(w.source("transcribe").await, "tenant");
}

#[tokio::test]
async fn reset_enqueues_keyless_connections_put() {
    let w = world().await;
    pushed_embed(&w).await;
    let (s, b) = w.reset(&w.im, "embed", 1).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // queued at once (no publish), naming no key
    let row = w.outbox("strata_rag:connections_put").await;
    assert_eq!(row.status, "pending");
    assert_eq!(row.payload["slots"][0]["slot"], "embed");
    assert_eq!(row.payload["slots"][0]["configured"], false);
    assert_eq!(row.payload["slots"][0]["key_ref"], Value::Null);
    drain_once(&w.e.state).await;
    assert_eq!(
        w.outbox("strata_rag:connections_put").await.status,
        "succeeded"
    );
    let (method, body) = World::provider_calls(&w.fake.rag).pop().unwrap();
    assert_eq!(method, "PUT");
    assert!(
        body.get("embed").is_none_or(Value::is_null),
        "the capability is left out: {body}"
    );
    assert!(!body.to_string().contains("api_key"), "{body}");
    assert_eq!(
        lock(&w.fake.rag).tenant_providers[&w.tenant]["embed"]["source"],
        "default",
        "strata falls back to its deployment default"
    );
}

#[tokio::test]
async fn reset_fans_out_model_change_to_pinning_jobs() {
    let w = world().await;
    // strata runs mistral-embed when kb is made (review I3: the initial generation is what strata runs)
    let (f, _rm, _) = pushed_embed(&w).await;
    let km = member(&w.e, w.tenant, "knowledge_manager").await;
    let (s, b) = w
        .call(
            &km,
            "POST",
            "/api/knowledge/stores",
            json!({"key": "kb", "label": "kb label", "parent_id": null, "kind": "leaf"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    for _ in 0..5 {
        if drain_once(&w.e.state).await.claimed == 0 {
            break;
        }
    }
    let (s, b) = w.reset(&w.im, "embed", 1).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let ch = Uuid::parse_str(b["model_change_id"].as_str().expect("a model change")).unwrap();
    let (slot, from, to, rebuild): (String, Option<String>, Option<String>, String) =
        sqlx::query_as(
            "SELECT slot, from_model, to_model, rebuild FROM model_change WHERE id = $1",
        )
        .bind(ch)
        .fetch_one(&w.e.owner)
        .await
        .unwrap();
    assert_eq!(
        (slot.as_str(), from.as_deref(), to, rebuild.as_str()),
        ("embed", Some("mistral-embed"), None, "reembed"),
        "to = the deployment default"
    );
    let state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM model_change_job WHERE model_change_id = $1 AND job_id = $2",
    )
    .bind(ch)
    .bind(f.job.job_id)
    .fetch_optional(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(state.as_deref(), Some("pending"));
}

#[tokio::test]
async fn reset_by_knowledge_manager_403() {
    let w = world().await;
    save_embed(&w).await;
    let km = member(&w.e, w.tenant, "knowledge_manager").await;
    for tok in [&km, &w.obs] {
        let (s, b) = w.reset(tok, "embed", 1).await;
        assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{b}");
    }
    assert_eq!(w.source("embed").await, "tenant");
}

#[tokio::test]
async fn reset_never_leaks_old_key() {
    let w = world().await;
    let (f, _rm, _) = pushed_embed(&w).await;
    let (s, b) = w.reset(&w.im, "embed", 1).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(!b.to_string().contains(GOOD));
    drain_once(&w.e.state).await;
    let audit: Vec<Value> =
        sqlx::query_scalar("SELECT details FROM audit_event WHERE tenant_id = $1")
            .bind(w.tenant)
            .fetch_all(&w.e.owner)
            .await
            .unwrap();
    let outbox: Vec<String> = sqlx::query_scalar(
        "SELECT payload::text || coalesce(result::text, '') || coalesce(last_error, '') FROM provision_outbox \
          WHERE tenant_id = $1",
    )
    .bind(w.tenant)
    .fetch_all(&w.e.owner)
    .await
    .unwrap();
    let ledger: Vec<Value> = sqlx::query_scalar(
        "SELECT coalesce(after, 'null'::jsonb) FROM draft_ledger WHERE job_id = $1",
    )
    .bind(f.job.job_id)
    .fetch_all(&w.e.owner)
    .await
    .unwrap();
    for text in [
        serde_json::to_string(&audit).unwrap(),
        outbox.concat(),
        serde_json::to_string(&ledger).unwrap(),
    ] {
        assert!(!text.contains(GOOD), "{text}");
    }
    let (_, body) = World::provider_calls(&w.fake.rag).pop().unwrap();
    assert!(!body.to_string().contains(GOOD), "{body}");
}

// ---- T4c: test a slot draft (F-09) ------------------------------------------------------------------------------

impl World {
    async fn test_draft(&self, slot: &str, body: Value) -> (StatusCode, Value) {
        self.call(
            &self.im,
            "POST",
            &format!("/api/providers/slots/{slot}/test"),
            body,
        )
        .await
    }

    /// The bodies the fake `svc` received on its provider test route.
    fn probes(svc: &strata_fake::Shared) -> Vec<Value> {
        lock(svc)
            .requests
            .iter()
            .filter(|r| r.path == "/api/tenant/providers/test")
            .map(|r| r.body.clone())
            .collect()
    }

    async fn counts(&self) -> Vec<i64> {
        let mut out = Vec::new();
        for sql in [
            "SELECT count(*) FROM provider_slot WHERE tenant_id = $1",
            "SELECT count(*) FROM provider_slot_secret WHERE tenant_id = $1",
            "SELECT count(*) FROM draft_ledger WHERE tenant_id = $1",
            "SELECT count(*) FROM provision_outbox WHERE tenant_id = $1",
            "SELECT count(*) FROM audit_event WHERE tenant_id = $1",
        ] {
            out.push(self.count(sql).await);
        }
        out
    }
}

/// A draft body for the mock provider's `path`.
fn draft(w: &World, kind: &str, path: &str, model: &str, secret: Option<&str>) -> Value {
    json!({"kind": kind, "base_url": w.url(path), "model": model, "secret": secret})
}

#[tokio::test]
async fn test_draft_stores_nothing() {
    let w = world().await;
    let before = w.counts().await;
    let (s, b) = w
        .test_draft(
            "embed",
            draft(&w, "openai_compatible", "/ok", "mistral-embed", Some(GOOD)),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["ok"], true, "{b}");
    assert_eq!(b["dims"], 1024);
    assert!(b["at"].is_string());
    assert_eq!(
        w.counts().await,
        before,
        "no row, key, ledger, outbox or audit"
    );
    let probes = World::probes(&w.fake.rag);
    assert_eq!(
        probes,
        [
            json!({"capability": "embed", "dialect": "openai", "base_url": w.url("/ok"),
                "model": "mistral-embed", "api_key": GOOD})
        ]
    );
    assert!(World::probes(&w.fake.ingest).is_empty());
    // an ingest slot goes to strata_ingest, with that slot's dialect
    let (s, b) = w
        .test_draft(
            "ocr",
            draft(&w, "mistral_ocr", "/ok", "mistral-ocr-latest", Some(GOOD)),
        )
        .await;
    assert_eq!((s, &b["ok"]), (StatusCode::OK, &json!(true)), "{b}");
    assert!(b.get("dims").is_none(), "{b}");
    assert_eq!(World::probes(&w.fake.ingest)[0]["dialect"], "mistral");
    assert_eq!(w.counts().await, before);
}

#[tokio::test]
async fn test_draft_typed_key_wins() {
    let w = world().await;
    save_embed(&w).await;
    let (s, b) = w
        .test_draft(
            "embed",
            draft(&w, "openai_compatible", "/ok", "mistral-embed", Some(WRONG)),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(World::probes(&w.fake.rag)[0]["api_key"], WRONG);
    // no key typed: the stored key, for its own host
    let (s, b) = w
        .test_draft(
            "embed",
            draft(&w, "openai_compatible", "/ok", "mistral-embed-2", None),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["ok"], true);
    assert_eq!(World::probes(&w.fake.rag)[1]["api_key"], GOOD);
}

#[tokio::test]
async fn test_draft_stored_key_other_host_422() {
    let w = world().await;
    save_embed(&w).await;
    let (s, b) = w
        .test_draft(
            "embed",
            draft(&w, "openai_compatible", "/alt", "mistral-embed", None),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "secret_required_for_new_host"
        ),
        "{b}"
    );
    // nothing configured yet: no key to fall back on either
    let (s, b) = w
        .test_draft("rerank", draft(&w, "cohere", "/ok", "rerank-v3.5", None))
        .await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "secret_required_for_new_host"
        ),
        "{b}"
    );
    assert!(
        World::probes(&w.fake.rag).is_empty(),
        "strata was not called"
    );
}

#[tokio::test]
async fn test_draft_probe_failure_is_ok_false_not_5xx() {
    let w = world().await;
    let (s, b) = w
        .test_draft(
            "embed",
            draft(&w, "openai_compatible", "/ok", "mistral-embed", Some(WRONG)),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["ok"], false);
    assert!(b["detail"].as_str().unwrap().contains("auth"), "{b}");
    assert!(b.get("dims").is_none(), "{b}");
    // strata's own refusal passes through with agent's message, never strata's text
    lock(&w.fake.rag).script(
        "POST",
        "/api/tenant/providers/test",
        422,
        json!({"error": {"code": "provider_host_not_allowed", "message": "embed: host 10.0.0.1 is private"}}),
    );
    let (s, b) = w
        .test_draft(
            "embed",
            draft(&w, "openai_compatible", "/ok", "mistral-embed", Some(GOOD)),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "provider_host_not_allowed"
        ),
        "{b}"
    );
    assert!(!b.to_string().contains("10.0.0.1"), "{b}");
}

#[tokio::test]
async fn test_draft_upstream_route_missing_503_slot_test_unavailable() {
    let w = world().await;
    lock(&w.fake.rag).script("POST", "/api/tenant/providers/test", 404, Value::Null);
    let (s, b) = w
        .test_draft(
            "embed",
            draft(&w, "openai_compatible", "/ok", "mistral-embed", Some(GOOD)),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::SERVICE_UNAVAILABLE, "slot_test_unavailable"),
        "{b}"
    );
    assert_eq!(World::probes(&w.fake.rag).len(), 1);
}

#[tokio::test]
async fn test_draft_never_sends_key_to_logs_or_response() {
    let w = world().await;
    // a strata that echoes the key in its detail: agent redacts it
    lock(&w.fake.rag).script(
        "POST",
        "/api/tenant/providers/test",
        200,
        json!({"ok": false, "capability": "embed", "model": "m", "latency_ms": 1,
               "detail": format!("embed: 401 for key {GOOD}"), "error_class": "auth"}),
    );
    let before = w.counts().await;
    let (s, b) = w
        .test_draft(
            "embed",
            draft(&w, "openai_compatible", "/ok", "mistral-embed", Some(GOOD)),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["ok"], false);
    assert!(!b.to_string().contains(GOOD), "{b}");
    assert_eq!(
        w.counts().await,
        before,
        "no audit or outbox row carries it"
    );
}

#[tokio::test]
async fn test_draft_internal_kind_422_internal_provider_only() {
    let w = world().await;
    for (kind, want) in [
        ("mlx_api", "internal_provider_only"),
        ("pgx_vllm", "internal_provider_only"),
        ("weird", "unknown_kind"),
    ] {
        let (s, b) = w
            .test_draft("embed", draft(&w, kind, "/ok", "m", Some(GOOD)))
            .await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, want),
            "{kind}: {b}"
        );
    }
    let (s, b) = w
        .test_draft(
            "nope",
            draft(&w, "openai_compatible", "/ok", "m", Some(GOOD)),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "slot_not_found"),
        "{b}"
    );
    assert!(World::probes(&w.fake.rag).is_empty());
}
