//! B3 T3: LLM providers + uses (D70, D69, D89; plan B3 §5.1, T3). The provider records (live), the use bindings (a
//! D69 draft through `design::track_tenant`, kind `llm_use`), the provider test (a mock OpenAI-compatible `/models`,
//! Laya `/v1/models`, 401 for a wrong key), the D70 internal tenant (a virtual `pgx vLLM (internal)` provider from
//! config), delete guarded by the live snapshots (`providers::llm::live_users`), and the full B2 design contract for
//! `llm_use`. Real Postgres 18 (one container for this binary, a fresh database per test); the API is rebuilt with
//! the test-only loopback netguard policy so the mock provider on 127.0.0.1 is reachable.
use crate::common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::common::design_contract::{self, ContractJob, cuttable_selection, new_job};
use crate::common::{TestEnv, get, insert_tenant, insert_user, json, person, send};
use agent_api::connectors::transport::Transport;
use agent_api::design::{DesignOf, DraftRef, JobRef};
use agent_api::netguard::Policy;
use agent_api::providers::llm::{self, INTERNAL_PROVIDER_ID, Use};
use agent_api::tenancy::begin_scoped;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get as route_get;
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{Value, json};
use uuid::Uuid;

/// The key the mock provider accepts, and one it refuses (401).
const GOOD: &str = "sk-good-1";
const WRONG: &str = "sk-wrong-2";
/// The platform service key the mock internal LLM accepts.
const SERVICE_KEY: &str = "internal-service-key-1";

fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
    vars.insert(
        "PROVIDER_SECRET_KEYS".to_string(),
        format!(r#"{{"p1":"{key}"}}"#),
    );
    vars.insert("PROVIDER_SECRET_KEY_CURRENT".to_string(), "p1".to_string());
}

// ---- the mock provider ----------------------------------------------------------------------------------------------

/// Every `Authorization` header the mock saw, in order.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<Option<String>>>>);

impl Seen {
    fn all(&self) -> Vec<Option<String>> {
        self.0.lock().unwrap().clone()
    }
}

struct Fake {
    /// `http://127.0.0.1:<port>`.
    base: String,
    seen: Seen,
}

fn record(s: &Seen, h: &HeaderMap) -> Option<String> {
    let a = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    s.0.lock().unwrap().push(a.clone());
    a
}

fn models_or_401(auth: Option<String>, want: &str, models: Value) -> (StatusCode, Json<Value>) {
    if auth.as_deref() == Some(format!("Bearer {want}").as_str()) {
        (StatusCode::OK, Json(json!({ "data": models })))
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid api key"})),
        )
    }
}

async fn openai_models(State(s): State<Seen>, h: HeaderMap) -> (StatusCode, Json<Value>) {
    let a = record(&s, &h);
    models_or_401(a, GOOD, json!([{"id": "gpt-4.1-mini"}, {"id": "gpt-4.1"}]))
}

async fn laya_models(State(s): State<Seen>, h: HeaderMap) -> (StatusCode, Json<Value>) {
    let a = record(&s, &h);
    models_or_401(a, GOOD, json!([{"id": "jev-latest"}]))
}

async fn internal_models(State(s): State<Seen>, h: HeaderMap) -> (StatusCode, Json<Value>) {
    let a = record(&s, &h);
    models_or_401(a, SERVICE_KEY, json!([{"id": "local-llm"}]))
}

async fn fake() -> Fake {
    let seen = Seen::default();
    let app = Router::new()
        .route("/ok/models", route_get(openai_models))
        .route("/ok/v1/models", route_get(laya_models))
        .route("/int/models", route_get(internal_models))
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Fake {
        base: format!("http://{addr}"),
        seen,
    }
}

// ---- the world ------------------------------------------------------------------------------------------------------

struct World {
    e: TestEnv,
    /// The API with the loopback netguard policy.
    app: Router,
    tenant: Uuid,
    /// integrations_manager, observer.
    im: String,
    obs: String,
    fake: Fake,
}

fn loopback_app(e: &TestEnv) -> Router {
    let mut st = e.state.clone();
    st.connectors = Transport::new(Policy {
        allow_loopback: true,
        private_targets: vec![],
    });
    agent_api::router(st)
}

/// A member of `tenant` holding `role` (`role_keys` syntax); its token.
async fn member(e: &TestEnv, tenant: Uuid, role: &str) -> String {
    let sub = format!("u-{}-{}", role.replace(',', "-"), Uuid::new_v4().simple());
    insert_user(&e.owner, &sub, tenant, role).await;
    person(&sub, &[])
}

async fn world_in(e: TestEnv, tenant: Uuid, fake: Fake) -> World {
    let im = member(&e, tenant, "integrations_manager").await;
    let obs = member(&e, tenant, "tenant_user").await;
    let app = loopback_app(&e);
    World {
        e,
        app,
        tenant,
        im,
        obs,
        fake,
    }
}

/// A fresh tenant (no job) with the provider keyring configured.
async fn world() -> World {
    let fake = fake().await;
    let e = common::env_with(with_keys).await;
    let tenant = Uuid::new_v4();
    insert_tenant(&e.owner, tenant, &format!("Providers {tenant}")).await;
    world_in(e, tenant, fake).await
}

/// A tenant with one job created through the API (B2's creation hook ran).
async fn world_with_job() -> (World, ContractJob) {
    let fake = fake().await;
    let e = common::env_with(with_keys).await;
    let f = new_job(&e).await;
    let w = world_in(e, f.tenant, fake).await;
    (w, f)
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

impl World {
    async fn call(&self, tok: &str, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        send(&self.app, json(method, uri, Some(tok), body)).await
    }

    async fn view(&self) -> Value {
        let (s, b) = send(&self.app, get("/api/providers", Some(&self.obs))).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        b
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.fake.base)
    }

    /// Create a provider as the integrations manager; its id.
    async fn create(&self, name: &str, kind: &str, models: &[&str], secret: Option<&str>) -> Uuid {
        let mut body =
            json!({"name": name, "kind": kind, "base_url": self.url("/ok"), "models": models});
        if let Some(s) = secret {
            body["secret"] = json!(s);
        }
        let (s, b) = self.call(&self.im, "POST", "/api/providers", body).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        b["id"].as_str().unwrap().parse().unwrap()
    }

    async fn bind(
        &self,
        use_name: &str,
        provider: Option<Uuid>,
        model: Option<&str>,
    ) -> (StatusCode, Value) {
        self.call(
            &self.im,
            "PUT",
            &format!("/api/providers/uses/{use_name}"),
            json!({"provider_id": provider, "model": model}),
        )
        .await
    }

    async fn count(&self, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql)
            .bind(self.tenant)
            .fetch_one(&self.e.owner)
            .await
            .unwrap()
    }

    async fn mark_platform_owned(&self) {
        sqlx::query("UPDATE tenants SET platform_owned = true WHERE id = $1")
            .bind(self.tenant)
            .execute(&self.e.owner)
            .await
            .unwrap();
    }
}

fn use_of<'a>(view: &'a Value, use_name: &str) -> &'a Value {
    view["uses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["use"] == use_name)
        .unwrap_or_else(|| panic!("no use {use_name} in {view}"))
}

/// Approve and apply the job's draft (`POST …/releases {apply: true}`) as a release manager; the release id.
async fn publish(w: &World, f: &ContractJob, tok: &str) -> String {
    let job = &f.job.job_key;
    let (s, b) = send(&w.app, get(&format!("/api/jobs/{job}/target"), Some(tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let body = json!({"apply": true, "selection_seen_at": b["selection_updated_at"].clone()});
    let (s, b) = w
        .call(tok, "POST", &format!("/api/jobs/{job}/releases"), body)
        .await;
    assert!(
        s == StatusCode::CREATED || s == StatusCode::OK,
        "approve: {s}: {b}"
    );
    assert_eq!(b["created"], true, "{b}");
    assert_eq!(b["applied"], true, "{b}");
    b["release"]["id"].as_str().unwrap().to_string()
}

/// The job gets a complete selection (so it cuts) and a release manager; its token.
async fn cuttable(w: &World, f: &ContractJob) -> String {
    design_contract::write(
        &w.e,
        f,
        "selection",
        &DraftRef::new(),
        Some(cuttable_selection()),
    )
    .await;
    design_contract::publishable(&w.e, f).await;
    member(&w.e, f.tenant, "tenant_admin").await
}

fn qualify_ref() -> DraftRef {
    let mut r = DraftRef::new();
    r.insert("use".into(), json!("qualify"));
    r
}

// ---- list -----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn list_shows_uses_and_warning() {
    let w = world().await;
    let v = w.view().await;
    assert_eq!(v["hosting"], "external", "{v}");
    assert_eq!(v["providers"], json!([]));
    let uses: Vec<&str> = v["uses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["use"].as_str().unwrap())
        .collect();
    assert_eq!(
        uses,
        ["qualify", "gates", "generator", "asr", "tts"],
        "D89 adds generator, T4d (D101/D102) the speech uses"
    );
    assert!(use_of(&v, "qualify")["provider_id"].is_null());
    // an unbound qualify is the normal path (the inference-router under the tenant's own licence): no warning
    assert!(v["warning"].is_null(), "{v}");

    // bound to a provider without a key: key_required
    let p = w.create("Mistral", "openai", &["gpt-4.1-mini"], None).await;
    let (s, b) = w.bind("qualify", Some(p), Some("gpt-4.1-mini")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let v = w.view().await;
    assert_eq!(v["warning"]["code"], "key_required", "{v}");
    assert_eq!(
        v["warning"]["message"],
        "Mistral: key required. Until a key is stored, turns cannot be answered, every customer message gets \"the assistant is unavailable\".",
        "{v}"
    );
    assert_eq!(use_of(&v, "qualify")["provider_id"], json!(p.to_string()));
    assert_eq!(use_of(&v, "qualify")["model"], "gpt-4.1-mini");
    assert_eq!(v["providers"][0]["token_mode"], "tenant_key");
    assert_eq!(v["providers"][0]["secret"]["set"], false);

    // with a key: no warning
    let (s, b) = w
        .call(
            &w.im,
            "PUT",
            &format!("/api/providers/{p}"),
            json!({"name": "Mistral", "base_url": w.url("/ok"), "models": ["gpt-4.1-mini"], "secret": GOOD}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(w.view().await["warning"].is_null());
}

#[tokio::test]
async fn hosting_internal_for_platform_owned_tenant() {
    let w = world().await;
    w.mark_platform_owned().await;
    let v = w.view().await;
    assert_eq!(v["hosting"], "internal", "{v}");
    let ps = v["providers"].as_array().unwrap();
    assert_eq!(ps.len(), 1, "{v}");
    assert_eq!(ps[0]["id"], json!(INTERNAL_PROVIDER_ID.to_string()));
    assert_eq!(ps[0]["name"], "pgx vLLM (internal)");
    assert_eq!(ps[0]["token_mode"], "internal");
    assert_eq!(ps[0]["region"], "EU (platform)");
    assert_eq!(ps[0]["models"], json!(["local-llm"]));
    assert_eq!(
        use_of(&v, "qualify")["provider_id"],
        json!(INTERNAL_PROVIDER_ID.to_string())
    );
    assert!(
        use_of(&v, "gates")["provider_id"].is_null(),
        "no internal Laya"
    );
    assert!(v["warning"].is_null());
    assert_eq!(
        w.count("SELECT count(*) FROM llm_provider WHERE tenant_id = $1")
            .await,
        0,
        "the internal provider is not a row"
    );
}

#[tokio::test]
async fn boot_marks_platform_owned_tenant() {
    let url = common::fresh_database().await;
    let e = common::env_for(url.clone()).await;
    let (platform, other) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, platform, &format!("Platform {platform}")).await;
    insert_tenant(&e.owner, other, &format!("Other {other}")).await;
    sqlx::query("UPDATE tenants SET platform_owned = true WHERE id = $1")
        .bind(other)
        .execute(&e.owner)
        .await
        .unwrap();
    let mut vars = common::base_env(&url);
    vars.insert("AGENT_PLATFORM_TENANT_ID".into(), platform.to_string());
    let cfg = agent_api::config::Config::from_lookup(|k| vars.get(k).cloned()).unwrap();
    agent_api::state::build_for_tests(cfg, common::TEST_SECRET)
        .await
        .unwrap();
    let owned: Vec<(Uuid, bool)> = sqlx::query_as(
        "SELECT id, platform_owned FROM tenants WHERE id IN ($1, $2) ORDER BY platform_owned DESC",
    )
    .bind(platform)
    .bind(other)
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        owned,
        [(platform, true), (other, false)],
        "exactly the configured tenant is platform-owned"
    );
    // I2: the variable removed, the next boot clears the flag (no tenant keeps the platform key)
    let vars = common::base_env(&url);
    assert!(!vars.contains_key("AGENT_PLATFORM_TENANT_ID"));
    let cfg = agent_api::config::Config::from_lookup(|k| vars.get(k).cloned()).unwrap();
    agent_api::state::build_for_tests(cfg, common::TEST_SECRET)
        .await
        .unwrap();
    let flagged: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM tenants WHERE id IN ($1, $2) AND platform_owned ORDER BY id",
    )
    .bind(platform)
    .bind(other)
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        flagged,
        Vec::<Uuid>::new(),
        "no tenant stays platform-owned"
    );
}

/// I2: the runtime role (`agent_app`, table-wide UPDATE on `tenants`) cannot flag itself platform-owned.
#[tokio::test]
async fn runtime_role_cannot_set_platform_owned() {
    let w = world().await;
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    let err = sqlx::query("UPDATE tenants SET platform_owned = true WHERE id = $1")
        .bind(w.tenant)
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("platform_owned_immutable"),
        "{err}"
    );
    drop(tx);
    // another column of its own row still updates
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    let n = sqlx::query("UPDATE tenants SET platform_owned = platform_owned WHERE id = $1")
        .bind(w.tenant)
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(
        n, 1,
        "the row is visible to it: the refusal above is the guard"
    );
}

// ---- create ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn create_refuses_internal_tenant() {
    let w = world().await;
    w.mark_platform_owned().await;
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            "/api/providers",
            json!({"name": "Mine", "base_url": w.url("/ok"), "models": ["m"]}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "internal_uses_mlx_api"),
        "{b}"
    );
}

#[tokio::test]
async fn create_ssrf() {
    let w = world().await;
    for (url, want) in [
        ("https://10.0.0.1", "invalid_target"),
        ("https://100.64.1.2/v1", "invalid_target"),
        ("https://vllm.internal/v1", "invalid_target"),
        ("not a url", "invalid_base_url"),
        ("https://user:pw@llm.example.com/v1", "invalid_base_url"),
    ] {
        let (s, b) = w
            .call(
                &w.im,
                "POST",
                "/api/providers",
                json!({"name": format!("p {url}"), "base_url": url, "models": ["m"]}),
            )
            .await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, want),
            "{url}: {b}"
        );
    }
    assert_eq!(
        w.count("SELECT count(*) FROM llm_provider WHERE tenant_id = $1")
            .await,
        0
    );
}

#[tokio::test]
async fn create_validates_name_models_kind() {
    let w = world().await;
    for (body, want) in [
        (
            json!({"name": "  ", "base_url": w.url("/ok"), "models": ["m"]}),
            "invalid_provider",
        ),
        (
            json!({"name": "x", "base_url": w.url("/ok"), "models": [" "]}),
            "model_required",
        ),
        (
            json!({"name": "x", "kind": "anthropic", "base_url": w.url("/ok"), "models": ["m"]}),
            "invalid_kind",
        ),
    ] {
        let (s, b) = w.call(&w.im, "POST", "/api/providers", body).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, want),
            "{b}"
        );
    }
}

#[tokio::test]
async fn create_duplicate_name_409() {
    let w = world().await;
    w.create("Mistral", "openai", &["m"], None).await;
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            "/api/providers",
            json!({"name": " Mistral ", "base_url": w.url("/ok"), "models": ["m"]}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "provider_exists"),
        "{b}"
    );
}

#[tokio::test]
async fn create_never_returns_key() {
    let w = world().await;
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            "/api/providers",
            json!({"name": "Keyed", "base_url": format!("{}/", w.url("/ok")), "models": ["gpt-4.1"], "secret": GOOD}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert!(!b.to_string().contains(GOOD), "{b}");
    assert_eq!(b["base_url"], w.url("/ok"), "trailing slash dropped");
    assert_eq!(b["token_mode"], "tenant_key");
    assert_eq!(b["secret"]["set"], true);
    let fpr = b["secret"]["fingerprint"].as_str().unwrap();
    assert_eq!(fpr.len(), 8, "{b}");
    assert!(b.get("secret_sealed").is_none());
    let id: Uuid = b["id"].as_str().unwrap().parse().unwrap();
    let sealed: String = sqlx::query_scalar("SELECT secret_sealed FROM llm_provider WHERE id = $1")
        .bind(id)
        .fetch_one(&w.e.owner)
        .await
        .unwrap();
    assert!(sealed.starts_with("v2:p1:") && !sealed.contains(GOOD));
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE tenant_id = $1 AND action = 'provider.create'",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(details["key"], "set", "{details}");
    assert!(!details.to_string().contains(GOOD));
    // the list never carries it either
    assert!(!w.view().await.to_string().contains(GOOD));
}

#[tokio::test]
async fn create_without_keyring_is_503() {
    let fake = fake().await;
    let e = common::env().await;
    let tenant = Uuid::new_v4();
    insert_tenant(&e.owner, tenant, &format!("No keyring {tenant}")).await;
    let w = world_in(e, tenant, fake).await;
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            "/api/providers",
            json!({"name": "K", "base_url": w.url("/ok"), "models": ["m"], "secret": GOOD}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "provider_secret_key_unconfigured"
        ),
        "{b}"
    );
}

// ---- update ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn update_host_change_keeps_key_is_422_secret_required_for_new_host() {
    let w = world().await;
    let p = w.create("Keyed", "openai", &["m"], Some(GOOD)).await;
    let other_host = w.fake.base.replace("127.0.0.1", "127.0.0.2");
    let uri = format!("/api/providers/{p}");
    let (s, b) = w
        .call(
            &w.im,
            "PUT",
            &uri,
            json!({"name": "Keyed", "base_url": format!("{other_host}/ok"), "models": ["m"]}),
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
    // same host, new path: the key is kept
    let (s, b) = w
        .call(
            &w.im,
            "PUT",
            &uri,
            json!({"name": "Keyed", "base_url": w.url("/ok/"), "models": ["m"], "secret": ""}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["secret"]["set"], true);
    // a new host with the key typed again is fine; null clears it
    let (s, b) = w
        .call(
            &w.im,
            "PUT",
            &uri,
            json!({"name": "Keyed", "base_url": format!("{other_host}/ok"), "models": ["m"], "secret": GOOD}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w
        .call(
            &w.im,
            "PUT",
            &uri,
            json!({"name": "Keyed", "base_url": format!("{other_host}/ok"), "models": ["m"], "secret": null}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["secret"],
        json!({"set": false, "updated_at": null, "fingerprint": null})
    );
    let keys: Vec<Value> = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE tenant_id = $1 AND action = 'provider.update' ORDER BY id",
    )
    .bind(w.tenant)
    .fetch_all(&w.e.owner)
    .await
    .unwrap();
    let keys: Vec<&str> = keys.iter().map(|d| d["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["kept", "replaced", "cleared"]);
}

#[tokio::test]
async fn update_kind_change_with_bound_use_409_provider_in_use() {
    let w = world().await;
    let p = w.create("OpenAI", "openai", &["m"], None).await;
    let (s, b) = w.bind("qualify", Some(p), Some("m")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w
        .call(
            &w.im,
            "PUT",
            &format!("/api/providers/{p}"),
            json!({"name": "OpenAI", "kind": "laya", "base_url": w.url("/ok"), "models": ["m"]}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "provider_in_use"),
        "{b}"
    );
    assert!(b["error"]["message"].as_str().unwrap().contains("qualify"));
    // unbound: the kind may change
    let (s, b) = w.bind("qualify", None, None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w
        .call(
            &w.im,
            "PUT",
            &format!("/api/providers/{p}"),
            json!({"name": "OpenAI", "kind": "laya", "base_url": w.url("/ok"), "models": ["m"]}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["kind"], "laya");
}

/// I3: a kind change is refused while a job's Production still binds the provider for a use the new kind cannot
/// serve, even when the draft no longer does.
#[tokio::test]
async fn update_kind_change_with_live_binding_409() {
    let (w, f) = world_with_job().await;
    let rm = cuttable(&w, &f).await;
    let p = w.create("LiveKind", "openai", &["m"], None).await;
    let (s, b) = w.bind("qualify", Some(p), Some("m")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    publish(&w, &f, &rm).await;
    let (s, b) = w.bind("qualify", None, None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let uri = format!("/api/providers/{p}");
    let body =
        json!({"name": "LiveKind", "kind": "laya", "base_url": w.url("/ok"), "models": ["m"]});
    let (s, b) = w.call(&w.im, "PUT", &uri, body.clone()).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "provider_in_use"),
        "{b}"
    );
    assert_eq!(b["error"]["jobs"], json!([f.job.job_key.clone()]), "{b}");
    // the unbind published: the kind may change
    publish(&w, &f, &rm).await;
    let (s, b) = w.call(&w.im, "PUT", &uri, body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["kind"], "laya");
}

#[tokio::test]
async fn update_dropping_bound_model_records_llm_use_draft() {
    let (w, f) = world_with_job().await;
    let p = w.create("Two", "openai", &["m-a", "m-b"], None).await;
    let (s, b) = w.bind("qualify", Some(p), Some("m-b")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w
        .call(
            &w.im,
            "PUT",
            &format!("/api/providers/{p}"),
            json!({"name": "Two", "base_url": w.url("/ok"), "models": ["m-a"]}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let model: Option<String> = sqlx::query_scalar(
        "SELECT model FROM llm_use_binding WHERE tenant_id = $1 AND \"use\" = 'qualify'",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(model.as_deref(), Some("m-a"), "re-bound to models[0]");
    let (label, after): (String, Value) = sqlx::query_as(
        "SELECT label, after FROM draft_ledger WHERE job_id = $1 AND kind = 'llm_use' AND key = 'llm-use:qualify'",
    )
    .bind(f.job.job_id)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(after["v"]["model"], "m-a", "{after}");
    assert_eq!(after["v"]["provider_id"], json!(p.to_string()));
    assert!(label.contains("model m-b removed"), "{label}");
}

// ---- delete ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn delete_bound_409() {
    let w = world().await;
    let p = w.create("Bound", "laya", &["jev-latest"], None).await;
    let (s, b) = w.bind("gates", Some(p), Some("jev-latest")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let uri = format!("/api/providers/{p}");
    let (s, b) = w.call(&w.im, "DELETE", &uri, Value::Null).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "provider_in_use"),
        "{b}"
    );
    assert!(b["error"]["message"].as_str().unwrap().contains("gates"));
    // unbound in the draft and never live anywhere (no job): deleted
    w.bind("gates", None, None).await;
    let (s, b) = w.call(&w.im, "DELETE", &uri, Value::Null).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    let (s, b) = w.call(&w.im, "DELETE", &uri, Value::Null).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "provider_not_found"),
        "{b}"
    );
}

/// I1: delete is a soft delete. The row stays (a snapshot may name it) without its key; it leaves the list, cannot be
/// bound, updated or tested, and its name is free again.
#[tokio::test]
async fn delete_is_soft_hidden_and_unbindable() {
    let w = world().await;
    let p = w
        .create("Gone", "openai", &["gpt-4.1-mini"], Some(GOOD))
        .await;
    let uri = format!("/api/providers/{p}");
    let (s, b) = w.call(&w.im, "DELETE", &uri, Value::Null).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    let (deleted, sealed): (bool, Option<String>) = sqlx::query_as(
        "SELECT deleted_at IS NOT NULL, secret_sealed FROM llm_provider WHERE tenant_id = $1",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert!(deleted, "the row is kept, marked deleted");
    assert_eq!(sealed, None, "its key is dropped");
    assert_eq!(
        w.view().await["providers"],
        json!([]),
        "hidden from the list"
    );
    let (s, b) = w.bind("qualify", Some(p), Some("gpt-4.1-mini")).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "provider_not_found"),
        "{b}"
    );
    let (s, b) = w
        .call(
            &w.im,
            "PUT",
            &uri,
            json!({"name": "Gone", "base_url": w.url("/ok"), "models": ["m"]}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "provider_not_found"),
        "{b}"
    );
    let (s, b) = w
        .call(&w.im, "POST", &format!("{uri}/test"), Value::Null)
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "provider_not_found"),
        "{b}"
    );
    // the name is free again
    let again = w.create("Gone", "openai", &["m"], None).await;
    assert_ne!(again, p);
}

/// I1: an approved cut that is not applied yet still binds the provider: applying it would make it live, so the
/// delete is refused although neither the draft nor the live design binds it.
#[tokio::test]
async fn delete_bound_in_approved_unapplied_cut_409() {
    let (w, f) = world_with_job().await;
    let rm = cuttable(&w, &f).await;
    let p = w.create("Pending", "openai", &["m"], None).await;
    let (s, b) = w.bind("qualify", Some(p), Some("m")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let job = f.job.job_key.clone();
    let (s, b) = send(&w.app, get(&format!("/api/jobs/{job}/target"), Some(&rm))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let body = json!({"apply": false, "selection_seen_at": b["selection_updated_at"].clone()});
    let (s, b) = w
        .call(&rm, "POST", &format!("/api/jobs/{job}/releases"), body)
        .await;
    assert!(s.is_success(), "approve: {s}: {b}");
    assert_eq!(b["applied"], false, "{b}");
    let cut = b["release"]["id"].as_str().unwrap().to_string();
    let (s, b) = w.bind("qualify", None, None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    {
        let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
        let live = llm::live_users(&mut tx, &w.e.state.design, p)
            .await
            .unwrap();
        assert!(live.is_empty(), "not live anywhere: {live:?}");
        let held = llm::holders(&mut tx, &w.e.state.design, p).await.unwrap();
        assert_eq!(
            held,
            [llm::Holder {
                job: job.clone(),
                use_name: "qualify".into(),
                live: false,
            }]
        );
    }
    let uri = format!("/api/providers/{p}");
    let (s, b) = w.call(&w.im, "DELETE", &uri, Value::Null).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "provider_in_use"),
        "{b}"
    );
    assert_eq!(b["error"]["jobs"], json!([job.clone()]), "{b}");
    // applied, it is live: still refused (now as live)
    let (s, b) = w
        .call(
            &rm,
            "POST",
            &format!("/api/jobs/{job}/releases/{cut}/apply"),
            json!({}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w.call(&w.im, "DELETE", &uri, Value::Null).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "provider_in_use"),
        "{b}"
    );
}

/// I1: a rollback to a cut that binds a since-deleted provider resolves to 409 `provider_deleted` (counted, logged)
/// and the providers view lists it; there is no keyword fallback to run silently.
#[tokio::test]
async fn rollback_after_delete_resolves_provider_deleted() {
    let (w, f) = world_with_job().await;
    let rm = cuttable(&w, &f).await;
    let old = w.create("Old", "openai", &["m"], Some(GOOD)).await;
    let new = w.create("New", "openai", &["m"], Some(GOOD)).await;
    w.bind("qualify", Some(old), Some("m")).await;
    let r1 = publish(&w, &f, &rm).await;
    w.bind("qualify", Some(new), Some("m")).await;
    publish(&w, &f, &rm).await;
    let (s, b) = w
        .call(
            &w.im,
            "DELETE",
            &format!("/api/providers/{old}"),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    assert_eq!(w.view().await["live_problems"], json!([]));
    let job = f.job.job_key.clone();
    let (s, b) = w
        .call(
            &rm,
            "POST",
            &format!("/api/jobs/{job}/releases/{r1}/apply"),
            json!({}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["direction"], "rollback", "{b}");
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    let err = llm::resolve(
        &mut tx,
        &w.e.state,
        w.tenant,
        &f.job_ref(),
        Use::Qualify,
        &DesignOf::Live,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.status_and_code(),
        (StatusCode::CONFLICT, "provider_deleted"),
        "{err}"
    );
    assert!(err.to_string().contains("Old"), "names the provider: {err}");
    drop(tx);
    let v = w.view().await;
    let problems = v["live_problems"].as_array().unwrap();
    assert_eq!(problems.len(), 1, "{v}");
    assert_eq!(
        (
            &problems[0]["job"],
            &problems[0]["use"],
            &problems[0]["code"]
        ),
        (&json!(job), &json!("qualify"), &json!("provider_deleted"))
    );
    // the other uses are unbound, not refused
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    let gates = llm::resolve(
        &mut tx,
        &w.e.state,
        w.tenant,
        &f.job_ref(),
        Use::Gates,
        &DesignOf::Live,
    )
    .await
    .unwrap();
    assert!(gates.is_none());
}

/// I3: a live binding whose provider's kind can no longer serve the use (here: the kind changed under it) is an
/// explicit 409 with the use's refusal code, never a provider of the wrong dialect.
#[tokio::test]
async fn resolve_refuses_live_kind_mismatch() {
    let (w, f) = world_with_job().await;
    let rm = cuttable(&w, &f).await;
    let p = w.create("Drifted", "openai", &["m"], Some(GOOD)).await;
    w.bind("qualify", Some(p), Some("m")).await;
    publish(&w, &f, &rm).await;
    sqlx::query("UPDATE llm_provider SET kind = 'laya' WHERE id = $1")
        .bind(p)
        .execute(&w.e.owner)
        .await
        .unwrap();
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    let err = llm::resolve(
        &mut tx,
        &w.e.state,
        w.tenant,
        &f.job_ref(),
        Use::Qualify,
        &DesignOf::Live,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.status_and_code(),
        (StatusCode::CONFLICT, "provider_kind_mismatch"),
        "{err}"
    );
    drop(tx);
    let v = w.view().await;
    assert_eq!(
        v["live_problems"][0]["code"], "provider_kind_mismatch",
        "{v}"
    );
}

#[tokio::test]
async fn delete_live_in_job_snapshot_409() {
    let (w, f) = world_with_job().await;
    let rm = cuttable(&w, &f).await;
    let p = w.create("Live", "openai", &["m"], None).await;
    let (s, b) = w.bind("qualify", Some(p), Some("m")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    publish(&w, &f, &rm).await;
    // unbound in the draft only: Production of the job still calls it
    let (s, b) = w.bind("qualify", None, None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let job = f.job.job_key.clone();
    {
        let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
        let live = llm::live_users(&mut tx, &w.e.state.design, p)
            .await
            .unwrap();
        assert_eq!(live, std::slice::from_ref(&job));
    }
    let uri = format!("/api/providers/{p}");
    let (s, b) = w.call(&w.im, "DELETE", &uri, Value::Null).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "provider_in_use"),
        "{b}"
    );
    assert!(
        b["error"]["message"].as_str().unwrap().contains(&job),
        "the message names the job: {b}"
    );
    assert_eq!(b["error"]["jobs"], json!([job]));
    // the unbind published: the provider can go
    publish(&w, &f, &rm).await;
    let (s, b) = w.call(&w.im, "DELETE", &uri, Value::Null).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
}

// ---- test -----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn test_saved_uses_stored_key_and_records_last_test() {
    let w = world().await;
    let p = w.create("Good", "openai", &["gpt-4.1"], Some(GOOD)).await;
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            &format!("/api/providers/{p}/test"),
            json!({}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["ok"], true, "{b}");
    let detail = b["detail"].as_str().unwrap();
    assert!(detail.starts_with("HTTP 200 · 2 models in "), "{detail}");
    assert!(detail.contains("stored key"), "{detail}");
    assert!(!b.to_string().contains(GOOD));
    assert!(
        b.get("models").is_none(),
        "testLlmProvider answers {{ok, at, detail}}"
    );
    assert_eq!(w.fake.seen.all(), [Some(format!("Bearer {GOOD}"))]);
    let last: Value = sqlx::query_scalar("SELECT last_test FROM llm_provider WHERE id = $1")
        .bind(p)
        .fetch_one(&w.e.owner)
        .await
        .unwrap();
    assert_eq!(last, b, "the result is the provider's last_test");
    assert_eq!(w.view().await["providers"][0]["last_test"], b);
    // Laya probes /v1/models
    let l = w.create("Laya", "laya", &["jev-latest"], Some(GOOD)).await;
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            &format!("/api/providers/{l}/test"),
            json!({}),
        )
        .await;
    assert_eq!((s, b["ok"].clone()), (StatusCode::OK, json!(true)), "{b}");
    assert!(b["detail"].as_str().unwrap().contains("1 models"), "{b}");
}

#[tokio::test]
async fn test_saved_401_is_ok_false_not_5xx() {
    let w = world().await;
    let p = w.create("Wrong", "openai", &["m"], Some(WRONG)).await;
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            &format!("/api/providers/{p}/test"),
            json!({}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["ok"], false, "{b}");
    assert!(b["detail"].as_str().unwrap().starts_with("HTTP 401"), "{b}");
    assert!(!b.to_string().contains(WRONG));
    // no key stored: key_required, the provider is not called
    let n = w.fake.seen.all().len();
    let k = w.create("Keyless", "openai", &["m"], None).await;
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            &format!("/api/providers/{k}/test"),
            json!({}),
        )
        .await;
    assert_eq!((s, b["ok"].clone()), (StatusCode::OK, json!(false)), "{b}");
    assert!(
        b["detail"].as_str().unwrap().contains("key_required"),
        "{b}"
    );
    assert_eq!(w.fake.seen.all().len(), n);
    // a host that refuses connections: ok false, never a 5xx
    let dead = w.create("Dead", "openai", &["m"], None).await;
    let (s, b) = w
        .call(
            &w.im,
            "PUT",
            &format!("/api/providers/{dead}"),
            json!({"name": "Dead", "base_url": "http://127.0.0.1:9/v1", "models": ["m"], "secret": GOOD}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            &format!("/api/providers/{dead}/test"),
            json!({}),
        )
        .await;
    assert_eq!((s, b["ok"].clone()), (StatusCode::OK, json!(false)), "{b}");
}

#[tokio::test]
async fn test_draft_typed_key_wins() {
    let w = world().await;
    let p = w
        .create("Stored wrong", "openai", &["m"], Some(WRONG))
        .await;
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            "/api/providers/test",
            json!({"kind": "openai", "base_url": w.url("/ok"), "secret": GOOD, "provider_id": p}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["ok"], true, "{b}");
    assert_eq!(b["models"], json!(["gpt-4.1-mini", "gpt-4.1"]));
    assert!(
        b["detail"].as_str().unwrap().contains("the key you typed"),
        "{b}"
    );
    assert!(
        b["detail"]
            .as_str()
            .unwrap()
            .ends_with("Nothing was saved."),
        "{b}"
    );
    assert_eq!(w.fake.seen.all(), [Some(format!("Bearer {GOOD}"))]);
    // absent: the stored key (here wrong) for the same host
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            "/api/providers/test",
            json!({"kind": "openai", "base_url": w.url("/ok"), "provider_id": p}),
        )
        .await;
    assert_eq!((s, b["ok"].clone()), (StatusCode::OK, json!(false)), "{b}");
    assert_eq!(
        w.fake.seen.all().last().cloned().flatten(),
        Some(format!("Bearer {WRONG}"))
    );
    // null: no key at all
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            "/api/providers/test",
            json!({"kind": "openai", "base_url": w.url("/ok"), "provider_id": p, "secret": null}),
        )
        .await;
    assert_eq!((s, b["ok"].clone()), (StatusCode::OK, json!(false)), "{b}");
    assert_eq!(w.fake.seen.all().last().cloned().flatten(), None);
}

#[tokio::test]
async fn test_draft_stored_key_other_host_422() {
    let w = world().await;
    let p = w.create("Stored", "openai", &["m"], Some(GOOD)).await;
    let other = w.fake.base.replace("127.0.0.1", "127.0.0.2");
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            "/api/providers/test",
            json!({"kind": "openai", "base_url": format!("{other}/ok"), "provider_id": p}),
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
    assert!(w.fake.seen.all().is_empty(), "the stored key was not sent");
    for (body, want) in [
        (
            json!({"kind": "gpt", "base_url": w.url("/ok")}),
            "invalid_kind",
        ),
        (
            json!({"kind": "openai", "base_url": "https://10.1.2.3/v1"}),
            "invalid_target",
        ),
        (
            json!({"kind": "openai", "base_url": "ftp://x.example.com"}),
            "invalid_base_url",
        ),
    ] {
        let (s, b) = w.call(&w.im, "POST", "/api/providers/test", body).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, want),
            "{b}"
        );
    }
}

#[tokio::test]
async fn test_draft_stores_nothing() {
    let w = world().await;
    let audit_before = w
        .count("SELECT count(*) FROM audit_event WHERE tenant_id = $1")
        .await;
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            "/api/providers/test",
            json!({"kind": "openai", "base_url": w.url("/ok"), "secret": GOOD}),
        )
        .await;
    assert_eq!((s, b["ok"].clone()), (StatusCode::OK, json!(true)), "{b}");
    assert_eq!(
        w.count("SELECT count(*) FROM llm_provider WHERE tenant_id = $1")
            .await,
        0,
        "no provider row"
    );
    assert_eq!(
        w.count("SELECT count(*) FROM audit_event WHERE tenant_id = $1")
            .await,
        audit_before,
        "no audit row"
    );
    // with a stored provider: its last_test stays null
    let p = w.create("Stored", "openai", &["m"], Some(GOOD)).await;
    let (s, _) = w
        .call(
            &w.im,
            "POST",
            "/api/providers/test",
            json!({"kind": "openai", "base_url": w.url("/ok"), "provider_id": p}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let last: Option<Value> =
        sqlx::query_scalar("SELECT last_test FROM llm_provider WHERE id = $1")
            .bind(p)
            .fetch_one(&w.e.owner)
            .await
            .unwrap();
    assert_eq!(last, None);
}

#[tokio::test]
async fn test_internal_uses_platform_key() {
    let fake = fake().await;
    let tenant = Uuid::new_v4();
    let int_url = format!("{}/int", fake.base);
    let e = common::env_with(|vars| {
        with_keys(vars);
        vars.insert("AGENT_PLATFORM_TENANT_ID".into(), tenant.to_string());
        vars.insert("INTERNAL_LLM_URL".into(), int_url);
        vars.insert("INTERNAL_LLM_KEY".into(), SERVICE_KEY.into());
    })
    .await;
    insert_tenant(&e.owner, tenant, &format!("Platform {tenant}")).await;
    let w = world_in(e, tenant, fake).await;
    let v = w.view().await;
    assert_eq!(v["hosting"], "internal", "{v}");
    assert_eq!(v["providers"][0]["secret"]["set"], true);
    assert!(!v.to_string().contains(SERVICE_KEY));
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            &format!("/api/providers/{INTERNAL_PROVIDER_ID}/test"),
            json!({}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["ok"], true, "{b}");
    assert!(b["detail"].as_str().unwrap().contains("service key"), "{b}");
    assert!(!b.to_string().contains(SERVICE_KEY));
    assert_eq!(w.fake.seen.all(), [Some(format!("Bearer {SERVICE_KEY}"))]);
    // no other id exists for the internal tenant
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            &format!("/api/providers/{}/test", Uuid::new_v4()),
            json!({}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "provider_not_found"),
        "{b}"
    );
    // and the draft test refuses: its providers are the platform's
    let (s, b) = w
        .call(
            &w.im,
            "POST",
            "/api/providers/test",
            json!({"kind": "openai", "base_url": w.url("/ok")}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "internal_uses_mlx_api"),
        "{b}"
    );
    // B4/B5 resolve the internal pgx vLLM for qualify, nothing for gates
    let mut tx = begin_scoped(&w.e.state.db, tenant).await.unwrap();
    let job = JobRef {
        id: Uuid::new_v4(),
        key: "none".into(),
    };
    let q = llm::resolve(
        &mut tx,
        &w.e.state,
        tenant,
        &job,
        Use::Qualify,
        &DesignOf::Live,
    )
    .await
    .unwrap()
    .expect("internal qualify");
    assert_eq!(
        (q.provider_id, q.model.as_str(), q.funding),
        (None, "local-llm", "local_gpu")
    );
    assert_eq!(q.key.as_ref().map(|k| k.expose()), Some(SERVICE_KEY));
    assert!(
        !format!("{q:?}").contains(SERVICE_KEY),
        "Debug redacts the key"
    );
    let g = llm::resolve(
        &mut tx,
        &w.e.state,
        tenant,
        &job,
        Use::Gates,
        &DesignOf::Live,
    )
    .await
    .unwrap();
    assert!(g.is_none());
}

// ---- bind -----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn bind_kind_mismatch_422() {
    let w = world().await;
    let openai = w.create("OpenAI", "openai", &["m"], None).await;
    let laya = w.create("Laya", "laya", &["jev-latest"], None).await;
    let (s, b) = w.bind("gates", Some(openai), Some("m")).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "provider_kind_mismatch"),
        "{b}"
    );
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Only a Laya provider")
    );
    for use_name in ["qualify", "generator"] {
        let (s, b) = w.bind(use_name, Some(laya), Some("jev-latest")).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "provider_kind_mismatch"),
            "{use_name}: {b}"
        );
    }
    let (s, b) = w.bind("summarise", Some(openai), Some("m")).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "use_not_found"),
        "{b}"
    );
    let (s, b) = w.bind("qualify", Some(Uuid::new_v4()), Some("m")).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "provider_not_found"),
        "{b}"
    );
    assert_eq!(
        w.count("SELECT count(*) FROM llm_use_binding WHERE tenant_id = $1")
            .await,
        0
    );
}

#[tokio::test]
async fn w2v_kind_rejected_for_qualify_422() {
    let w = world().await;
    let w2v = w.create("Speech", "w2v", &["kokoro"], None).await;
    let laya = w.create("Laya", "laya", &["jev-latest"], None).await;
    for use_name in ["qualify", "gates", "generator"] {
        let (s, b) = w.bind(use_name, Some(w2v), Some("kokoro")).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "kind_not_valid_for_use"),
            "{use_name}: {b}"
        );
    }
    // the speech uses take w2v or OpenAI-compatible, never Laya
    for use_name in ["asr", "tts"] {
        let (s, b) = w.bind(use_name, Some(laya), Some("jev-latest")).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "kind_not_valid_for_use"),
            "{use_name}: {b}"
        );
    }
    assert_eq!(
        w.count("SELECT count(*) FROM llm_use_binding WHERE tenant_id = $1")
            .await,
        0,
        "nothing bound"
    );
    for use_name in ["asr", "tts"] {
        let (s, v) = w.bind(use_name, Some(w2v), Some("kokoro")).await;
        assert_eq!(s, StatusCode::OK, "{use_name}: {v}");
        assert_eq!(use_of(&v, use_name)["provider_id"], json!(w2v.to_string()));
    }
    // a provider bound to a speech use cannot become Laya; it may become OpenAI-compatible
    let body = |kind: &str| json!({"name": "Speech", "kind": kind, "base_url": w.url("/ok"), "models": ["kokoro"]});
    let (s, b) = w
        .call(&w.im, "PUT", &format!("/api/providers/{w2v}"), body("laya"))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "provider_in_use"),
        "{b}"
    );
    let (s, b) = w
        .call(
            &w.im,
            "PUT",
            &format!("/api/providers/{w2v}"),
            body("openai"),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// Bind `use_name` to `p`'s `kokoro` with `voice`.
async fn put_voice(w: &World, use_name: &str, p: Uuid, voice: &str) -> (StatusCode, Value) {
    w.call(
        &w.im,
        "PUT",
        &format!("/api/providers/uses/{use_name}"),
        json!({"provider_id": p, "model": "kokoro", "voice": voice}),
    )
    .await
}

#[tokio::test]
async fn voice_on_tts_only() {
    let (w, f) = world_with_job().await;
    let jr = f.job_ref();
    let p = w.create("Voice", "w2v", &["kokoro"], None).await;
    let (s, b) = put_voice(&w, "asr", p, "af_heart").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "voice_only_for_tts"),
        "{b}"
    );
    let long = "v".repeat(65);
    for bad in ["two words", "x\"y", long.as_str()] {
        let (s, b) = put_voice(&w, "tts", p, bad).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_voice"),
            "{bad}: {b}"
        );
    }
    assert_eq!(
        w.count("SELECT count(*) FROM llm_use_binding WHERE tenant_id = $1")
            .await,
        0
    );
    let rev = |db: sqlx::PgPool, id: Uuid| async move {
        sqlx::query_scalar::<_, i64>("SELECT draft_rev FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap()
    };
    let before = rev(w.e.owner.clone(), jr.id).await;
    let (s, v) = put_voice(&w, "tts", p, " af_heart ").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(use_of(&v, "tts")["voice"], "af_heart", "{v}");
    assert!(use_of(&v, "asr")["voice"].is_null());
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT voice FROM llm_use_binding WHERE tenant_id = $1 AND \"use\" = 'tts'",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(stored.as_deref(), Some("af_heart"));
    let after: Value = sqlx::query_scalar(
        "SELECT after FROM draft_ledger WHERE job_id = $1 AND key = 'llm-use:tts' ORDER BY id DESC LIMIT 1",
    )
    .bind(jr.id)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(
        after,
        json!({"v": {"provider_id": p.to_string(), "model": "kokoro", "provider": "Voice", "voice": "af_heart"}})
    );
    assert_eq!(rev(w.e.owner.clone(), jr.id).await, before + 1);
    // a voice-only change is a design change too
    let (s, v) = put_voice(&w, "tts", p, "bf_emma").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(rev(w.e.owner.clone(), jr.id).await, before + 2);
    // the live design is untouched (D69), the draft carries the voice
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    let live = llm::resolve(
        &mut tx,
        &w.e.state,
        w.tenant,
        &jr,
        Use::Tts,
        &DesignOf::Live,
    )
    .await
    .unwrap();
    assert!(live.is_none());
    let draft = llm::resolve(
        &mut tx,
        &w.e.state,
        w.tenant,
        &jr,
        Use::Tts,
        &DesignOf::Draft,
    )
    .await
    .unwrap()
    .expect("the draft binds tts");
    assert_eq!(draft.voice.as_deref(), Some("bf_emma"));
    assert_eq!(draft.kind, llm::Kind::W2v);
}

#[tokio::test]
async fn bind_unknown_model_422() {
    let w = world().await;
    let p = w.create("OpenAI", "openai", &["m"], None).await;
    for model in [Some("other"), None] {
        let (s, b) = w.bind("qualify", Some(p), model).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "unknown_model"),
            "{b}"
        );
    }
}

#[tokio::test]
async fn bind_writes_working_row_and_draft_only() {
    let (w, f) = world_with_job().await;
    let jr = f.job_ref();
    let rev = |db: sqlx::PgPool, id: Uuid| async move {
        sqlx::query_scalar::<_, i64>("SELECT draft_rev FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap()
    };
    let before = rev(w.e.owner.clone(), jr.id).await;
    let p = w.create("Gen", "openai", &["gpt-4.1"], Some(GOOD)).await;
    let (s, v) = w.bind("generator", Some(p), Some("gpt-4.1")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        use_of(&v, "generator")["provider_id"],
        json!(p.to_string()),
        "answers LlmProviders"
    );
    let (by, model): (String, String) = sqlx::query_as(
        "SELECT updated_by, model FROM llm_use_binding WHERE tenant_id = $1 AND \"use\" = 'generator'",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert!(by.starts_with("u-integrations_manager-"), "{by}");
    assert_eq!(model, "gpt-4.1");
    // the ledger of the tenant's job records it; the engine bumped draft_rev
    let after: Value = sqlx::query_scalar(
        "SELECT after FROM draft_ledger WHERE job_id = $1 AND kind = 'llm_use' AND key = 'llm-use:generator'",
    )
    .bind(jr.id)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(
        after,
        json!({"v": {"provider_id": p.to_string(), "model": "gpt-4.1", "provider": "Gen"}})
    );
    assert_eq!(rev(w.e.owner.clone(), jr.id).await, before + 1);
    let drafts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE tenant_id = $1 AND action = 'tenant.draft_change'",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(drafts, 1);
    // live is unchanged (D69: never live on save); the draft resolves, with its key redacted in Debug
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    let live = llm::resolve(
        &mut tx,
        &w.e.state,
        w.tenant,
        &jr,
        Use::Generator,
        &DesignOf::Live,
    )
    .await
    .unwrap();
    assert!(live.is_none(), "live unchanged");
    let draft = llm::resolve(
        &mut tx,
        &w.e.state,
        w.tenant,
        &jr,
        Use::Generator,
        &DesignOf::Draft,
    )
    .await
    .unwrap()
    .expect("the draft binds generator");
    assert_eq!(
        (draft.provider_id, draft.model.as_str(), draft.funding),
        (Some(p), "gpt-4.1", "tenant_byo")
    );
    assert_eq!(draft.key.as_ref().map(|k| k.expose()), Some(GOOD));
    assert!(!format!("{draft:?}").contains(GOOD));
    drop(tx);
    // binding the same value again changes nothing
    let (s, _) = w.bind("generator", Some(p), Some("gpt-4.1")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(rev(w.e.owner.clone(), jr.id).await, before + 1);
}

#[tokio::test]
async fn bind_by_tenant_user_403() {
    let w = world().await;
    let p = w.create("P", "openai", &["m"], None).await;
    let (s, b) = w
        .call(
            &w.obs,
            "PUT",
            "/api/providers/uses/qualify",
            json!({"provider_id": p, "model": "m"}),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{b}");
    for (method, uri, body) in [
        (
            "POST",
            "/api/providers".to_string(),
            json!({"name": "x", "base_url": w.url("/ok"), "models": ["m"]}),
        ),
        ("DELETE", format!("/api/providers/{p}"), Value::Null),
        ("POST", format!("/api/providers/{p}/test"), json!({})),
        (
            "POST",
            "/api/providers/test".to_string(),
            json!({"kind": "openai", "base_url": w.url("/ok")}),
        ),
    ] {
        let (s, b) = w.call(&w.obs, method, &uri, body).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::FORBIDDEN, "forbidden"),
            "{method} {uri}: {b}"
        );
    }
    assert!(w.fake.seen.all().is_empty());
}

// ---- the design kind ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn design_kind_llm_use_snapshot_restore() {
    let (w, f) = world_with_job().await;
    let rm = cuttable(&w, &f).await;
    let p = w.create("Snap", "openai", &["m-1", "m-2"], None).await;
    w.bind("qualify", Some(p), Some("m-1")).await;
    let release = publish(&w, &f, &rm).await;
    let snap: Value = sqlx::query_scalar(
        "SELECT snap FROM release_snapshot WHERE job_id = $1 AND release_id = $2 AND key = 'llm-use:qualify'",
    )
    .bind(f.job.job_id)
    .bind(&release)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(
        snap,
        json!({"v": {"provider_id": p.to_string(), "model": "m-1", "provider": "Snap"}})
    );
    // a later draft, discarded: the working row is the live value again
    let (s, b) = w.bind("qualify", Some(p), Some("m-2")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let designer = member(&w.e, f.tenant, "job_designer").await;
    let (s, b) = w
        .call(
            &designer,
            "DELETE",
            &format!("/api/jobs/{}/draft", f.job.job_key),
            Value::Null,
        )
        .await;
    assert!(s.is_success(), "discard: {s}: {b}");
    let v = w.view().await;
    assert_eq!(use_of(&v, "qualify")["model"], "m-1", "{v}");
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    let live = llm::resolve(
        &mut tx,
        &w.e.state,
        w.tenant,
        &f.job_ref(),
        Use::Qualify,
        &DesignOf::Live,
    )
    .await
    .unwrap()
    .expect("live qualify");
    assert_eq!(live.model, "m-1");
}

#[tokio::test]
async fn llm_use_full_contract() {
    let e = common::env_with(with_keys).await;
    let pid = Uuid::new_v4().to_string();
    design_contract::run_all(
        &e,
        "llm_use",
        &[
            (
                qualify_ref(),
                json!({"provider_id": pid, "model": "gpt-4.1", "provider": pid}),
            ),
            (
                {
                    let mut r = DraftRef::new();
                    r.insert("use".into(), json!("gates"));
                    r
                },
                json!({"provider_id": pid, "model": null, "provider": pid}),
            ),
            (
                {
                    let mut r = DraftRef::new();
                    r.insert("use".into(), json!("tts"));
                    r
                },
                json!({"provider_id": pid, "model": "kokoro", "provider": pid, "voice": "af_heart"}),
            ),
        ],
        Some("not bound"),
    )
    .await;
}
