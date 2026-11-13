//! B7 T10: T3 instance contract tests and sample fetch on a real Postgres 18, against an axum mock on loopback
//! (the API is rebuilt with a `Transport` whose policy has the test-only `allow_loopback`). Outcome rules = the
//! fixture's `contractOutcome`; a prod X/P op, a prod W op without the OK, a missing probe input and a sample of a
//! write op never reach the system.
use crate::common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use agent_api::connectors::transport::Transport;
use agent_api::netguard::Policy;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{get as route_get, post as route_post};
use axum::{Json, Router};
use base64::Engine as _;
use common::platform::{Op, publish_template};
use common::{TestEnv, get, insert_tenant, insert_user, json, person, send};
use serde_json::{Value, json};
use uuid::Uuid;

const TOKEN: &str = "svc-token-for-mock-9911";
/// The fake secret the leaky mock answers (never stored, never served).
const LEAKED: &str = "Xy7-not-real";
const LEAKY_STDOUT: &str = "USERNAME:jdoe\nPASSWORD:Xy7-not-real\nSTATUS:SUCCESS";

#[derive(Clone, Default)]
struct Mock {
    hits: Arc<AtomicUsize>,
    reject: Arc<AtomicBool>,
    /// Answer bodies that carry secrets (a `password` key, a `PASSWORD:` stdout line).
    leak: Arc<AtomicBool>,
    auth: Arc<std::sync::Mutex<Vec<String>>>,
}

fn record(m: &Mock, h: &HeaderMap) {
    m.hits.fetch_add(1, Ordering::SeqCst);
    let a = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    m.auth.lock().unwrap().push(a);
}

async fn offerings(State(m): State<Mock>, h: HeaderMap) -> (StatusCode, Json<Value>) {
    record(&m, &h);
    if m.reject.load(Ordering::SeqCst) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"errors": "Unauthorized"})),
        );
    }
    if m.leak.load(Ordering::SeqCst) {
        return (
            StatusCode::OK,
            Json(json!([
                {"id": "o-1", "sku": "S-1", "title": "Kettle", "price": "19.90", "availability": "in_stock",
                 "password": LEAKED, "stdout": LEAKY_STDOUT}
            ])),
        );
    }
    (
        StatusCode::OK,
        Json(json!([
            {"id": "o-1", "sku": "S-1", "title": "Kettle", "price": "19.90", "availability": "in_stock"},
            {"id": "o-2", "sku": "S-2", "title": "Toaster", "price": "29.90", "availability": "in_stock"}
        ])),
    )
}

async fn problems(State(m): State<Mock>, h: HeaderMap) -> Json<Value> {
    record(&m, &h);
    if m.leak.load(Ordering::SeqCst) {
        return Json(json!({"case_id": "C-1", "status": "open",
                           "auth": {"client_secret": LEAKED}, "stdout": LEAKY_STDOUT}));
    }
    Json(json!({"case_id": "C-1", "status": "open"}))
}

async fn anything(State(m): State<Mock>, h: HeaderMap) -> StatusCode {
    record(&m, &h);
    StatusCode::NOT_FOUND
}

/// The mock system: its base URL and its counters.
async fn mock() -> (String, Mock) {
    let m = Mock::default();
    let app = Router::new()
        .route("/offerings", route_get(offerings))
        .route("/shipments/{order}/problems", route_post(problems))
        .fallback(anything)
        .with_state(m.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://127.0.0.1:{port}"), m)
}

fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([5u8; 32]);
    vars.insert(
        "TENANT_DATA_KEYS".to_string(),
        format!(r#"{{"k1":"{key}"}}"#),
    );
    vars.insert("TENANT_DATA_KEY_CURRENT".to_string(), "k1".to_string());
}

struct World {
    e: TestEnv,
    app: Router,
    im: String,
}

/// A tenant with an integrations manager, served by an API whose connector policy admits loopback.
async fn world() -> World {
    let e = common::env_with(with_keys).await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Contract").await;
    insert_user(&e.owner, "u-im", t, "integrations_manager").await;
    let mut st = e.state.clone();
    st.connectors = Transport::new(Policy {
        allow_loopback: true,
        private_targets: vec![],
    });
    let app = agent_api::router(st);
    // the platform templates the tests pin (rows authored through the platform routes, stub fixture `{id, status}`)
    let ok = || json!({"id": "redacted", "status": "ok"});
    publish_template(
        &e,
        "test_shop",
        "3",
        &[
            Op::star("offering", "list")
                .path("/offerings")
                .returns(ok()),
            Op::star("order", "read").returns(ok()),
            Op::star("order", "cancel").returns(ok()),
            Op::star("delivery", "report_problem")
                .path("/shipments/{{canonical.delivery_id}}/problems")
                .returns(ok()),
            Op::star("payment", "refund").returns(ok()),
        ],
    )
    .await
    .unwrap();
    for version in ["1", "2"] {
        publish_template(
            &e,
            "test_shop_v",
            version,
            &[
                Op::star("order", "read").returns(ok()),
                Op::star("order", "modify").returns(ok()),
                Op::star("order", "cancel").returns(ok()),
            ],
        )
        .await
        .unwrap();
    }
    World {
        e,
        app,
        im: person("u-im", &[]),
    }
}

impl World {
    async fn call(&self, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        send(&self.app, json(method, uri, Some(&self.im), body)).await
    }

    /// A test_shop@3 instance (`stub` → `stub://`), with the credential set unless it is a stub.
    async fn system(&self, id: &str, env: &str, base_url: &str, stub: bool, env_ids: Value) {
        let (s, b) = self
            .call(
                "POST",
                "/api/connections/systems",
                json!({"id": id, "label": id, "template_key": "test_shop", "template_version": "3",
                       "env": env, "base_url": base_url, "env_ids": env_ids, "max_rps": null, "stub": stub}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        if !stub {
            let (s, b) = self
                .call(
                    "PUT",
                    &format!("/api/connections/systems/{id}/credential"),
                    json!({"service_token": TOKEN}),
                )
                .await;
            assert_eq!(s, StatusCode::OK, "{b}");
        }
    }

    async fn test(&self, id: &str, body: Value) -> (StatusCode, Value) {
        self.call(
            "POST",
            &format!("/api/connections/systems/{id}/contract-tests"),
            body,
        )
        .await
    }

    async fn repin(&self, version: &str) -> (StatusCode, Value) {
        self.call(
            "POST",
            "/api/connections/systems/shop-versioned/repin",
            json!({"version": version}),
        )
        .await
    }

    async fn detail(&self, id: &str) -> Value {
        let (s, b) = send(
            &self.app,
            get(&format!("/api/connections/systems/{id}"), Some(&self.im)),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        b
    }
}

fn hits(m: &Mock) -> usize {
    m.hits.load(Ordering::SeqCst)
}

#[tokio::test]
async fn contract_test_stub_green() {
    let w = world().await;
    w.system("shop-stub", "test", "", true, json!({})).await;
    let (s, b) = w.test("shop-stub", json!({"op": "order.read"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["outcome"], "green");
    assert_eq!(b["code"], Value::Null);
    assert_eq!(b["side_effect"], "R");
    assert!(b["note"].as_str().unwrap().contains("never counts"), "{b}");
    assert!(b["at"].is_string());
    let d = w.detail("shop-stub").await;
    assert_eq!(d["instance"]["status"], "ok");
    assert_eq!(d["instance"]["contract"][0]["op"], "order.read");
    assert_eq!(d["op_readiness"]["order.read"], "stub");
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action = 'connector.contract_test.run'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(n, 1);
    let (s, b) = w.test("shop-stub", json!({"op": "order.teleport"})).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("operation_not_implemented")),
        "{b}"
    );
    // a stub sample is its fixture
    let (s, b) = w
        .call(
            "POST",
            "/api/connections/systems/shop-stub/samples",
            json!({"op": "order.read"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["op"], "order.read");
    assert_eq!(b["body"]["status"], "ok");
}

#[tokio::test]
async fn no_credential_is_red_without_a_call() {
    let w = world().await;
    let (base, m) = mock().await;
    let (s, b) = w
        .call(
            "POST",
            "/api/connections/systems",
            json!({"id": "shop-bare", "label": "Bare", "template_key": "test_shop", "template_version": "3",
                   "env": "test", "base_url": base, "env_ids": {}, "max_rps": null, "stub": false}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = w.test("shop-bare", json!({"op": "offering.list"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["outcome"].as_str(), b["code"].as_str()),
        (Some("red"), Some("connector_auth_failed"))
    );
    let (s, b) = w
        .call(
            "POST",
            "/api/connections/systems/shop-bare/samples",
            json!({"op": "offering.list"}),
        )
        .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("connector_auth_failed")),
        "{b}"
    );
    assert_eq!(hits(&m), 0);
}

#[tokio::test]
async fn read_probe_green_and_sample() {
    let w = world().await;
    let (base, m) = mock().await;
    w.system("shop-test", "test", &base, false, json!({})).await;
    let (s, b) = w.test("shop-test", json!({"op": "offering.list"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["outcome"], "green", "{b}");
    assert_eq!(hits(&m), 1);
    assert_eq!(
        m.auth.lock().unwrap().last().cloned().unwrap_or_default(),
        format!("Bearer {TOKEN}"),
        "the single secret field is presented as the bearer token"
    );
    let d = w.detail("shop-test").await;
    assert_eq!(d["instance"]["status"], "ok");
    assert_eq!(d["op_readiness"]["offering.list"], "ready");
    let stored: Option<Value> = sqlx::query_scalar(
        "SELECT response FROM connector_contract_result WHERE op = 'offering.list'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert!(stored.is_some(), "the redacted response is kept");
    let (s, b) = w
        .call(
            "POST",
            "/api/connections/systems/shop-test/samples",
            json!({"op": "offering.list"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["body"]["id"], "o-1", "the first array item: {b}");
    assert_eq!(hits(&m), 2);
    let d = w.detail("shop-test").await;
    assert_eq!(d["instance"]["sample"]["op"], "offering.list");
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action = 'connector.sample.fetch'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(n, 1);
    for b in [&d, &b] {
        assert!(!b.to_string().contains(TOKEN));
    }
}

#[tokio::test]
async fn vendor_secrets_are_neither_stored_nor_served() {
    let w = world().await;
    let (base, m) = mock().await;
    m.leak.store(true, Ordering::SeqCst);
    w.system(
        "shop-test",
        "test",
        &base,
        false,
        json!({"probe.delivery_id": "D-1", "probe.problem": "late", "probe.photo_ref": "contract-test"}),
    )
    .await;
    let mut served = Vec::new();
    // a read (sample = a live call) and a write (sample = the stored contract response)
    for op in ["offering.list", "delivery.report_problem"] {
        let (s, b) = w.test("shop-test", json!({"op": op})).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["outcome"], "green", "{op}: {b}");
        served.push(b);
        let (s, b) = w
            .call(
                "POST",
                "/api/connections/systems/shop-test/samples",
                json!({"op": op}),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{op}: {b}");
        assert_eq!(
            b["body"]["stdout"], "USERNAME:jdoe\nPASSWORD:[redacted]\nSTATUS:SUCCESS",
            "the key stays, the value does not: {b}"
        );
        served.push(b);
    }
    assert_eq!(
        hits(&m),
        3,
        "two tests + the read sample; the write sample is the stored response"
    );
    served.push(w.detail("shop-test").await);
    for b in &served {
        assert!(
            !b.to_string().contains(LEAKED),
            "served a vendor secret: {b}"
        );
    }
    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT response::text FROM connector_contract_result WHERE response IS NOT NULL",
    )
    .fetch_all(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(stored.len(), 2, "{stored:?}");
    let samples: Vec<String> = sqlx::query_scalar("SELECT body::text FROM connector_sample")
        .fetch_all(&w.e.owner)
        .await
        .unwrap();
    for text in stored.iter().chain(&samples) {
        assert!(!text.contains(LEAKED), "stored a vendor secret: {text}");
        assert!(text.contains("PASSWORD:[redacted]"), "{text}");
    }
}

#[tokio::test]
async fn missing_probe_is_not_run() {
    let w = world().await;
    let (base, m) = mock().await;
    w.system("shop-test", "test", &base, false, json!({})).await;
    let (s, b) = w.test("shop-test", json!({"op": "order.read"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["outcome"].as_str(), b["code"].as_str()),
        (Some("not_run"), Some("probe_input_missing"))
    );
    assert!(
        b["note"].as_str().unwrap().contains("probe.order_id"),
        "{b}"
    );
    assert_eq!(hits(&m), 0);
}

#[tokio::test]
async fn auth_401_sets_auth_failed() {
    let w = world().await;
    let (base, m) = mock().await;
    m.reject.store(true, Ordering::SeqCst);
    w.system("shop-test", "test", &base, false, json!({})).await;
    let (s, b) = w.test("shop-test", json!({"op": "offering.list"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["outcome"].as_str(), b["code"].as_str()),
        (Some("red"), Some("connector_auth_failed"))
    );
    let d = w.detail("shop-test").await;
    assert_eq!(d["instance"]["status"], "auth_failed");
    assert_eq!(d["instance"]["last_error"]["code"], "connector_auth_failed");
    assert_eq!(d["op_readiness"]["offering.list"], "blocked");
    // the sample refuses while auth failed, without a call
    let before = hits(&m);
    let (s, b) = w
        .call(
            "POST",
            "/api/connections/systems/shop-test/samples",
            json!({"op": "offering.list"}),
        )
        .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::BAD_GATEWAY, Some("connector_auth_failed")),
        "{b}"
    );
    assert_eq!(hits(&m), before);
    // a new credential clears it
    m.reject.store(false, Ordering::SeqCst);
    let (s, b) = w
        .call(
            "PUT",
            "/api/connections/systems/shop-test/credential",
            json!({"service_token": "a-fresh-token-55"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let d = w.detail("shop-test").await;
    assert_eq!(d["instance"]["status"], "untested");
    assert_eq!(d["instance"]["last_error"], Value::Null);
    let (_, b) = w.test("shop-test", json!({"op": "offering.list"})).await;
    assert_eq!(b["outcome"], "green", "{b}");
}

#[tokio::test]
async fn prod_write_needs_explicit_ok() {
    let w = world().await;
    let (base, m) = mock().await;
    w.system(
        "shop-prod",
        "prod",
        &base,
        false,
        json!({"probe.delivery_id": "D-1", "probe.problem": "late", "probe.photo_ref": "contract-test"}),
    )
    .await;
    let (s, b) = w
        .test("shop-prod", json!({"op": "delivery.report_problem"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["outcome"], "needs_ok");
    assert_eq!(b["at"], Value::Null);
    assert_eq!(hits(&m), 0);
    let (s, b) = w
        .test(
            "shop-prod",
            json!({"op": "delivery.report_problem", "prod_write_ok": true}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["outcome"], "green", "{b}");
    assert!(b["note"].as_str().unwrap().contains("agent-contract-test"));
    assert_eq!(hits(&m), 1);
}

#[tokio::test]
async fn prod_x_op_is_write_unverified_without_a_call() {
    let w = world().await;
    let (base, m) = mock().await;
    w.system(
        "shop-prod",
        "prod",
        &base,
        false,
        json!({"probe.order_id": "O-1"}),
    )
    .await;
    for body in [
        json!({"op": "order.cancel"}),
        json!({"op": "order.cancel", "prod_write_ok": true}),
        json!({"op": "payment.refund", "prod_write_ok": true}),
    ] {
        let (s, b) = w.test("shop-prod", body).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["outcome"], "write_unverified", "{b}");
        assert_eq!(b["at"], Value::Null);
    }
    assert_eq!(hits(&m), 0);
    let d = w.detail("shop-prod").await;
    assert_eq!(d["op_readiness"]["order.cancel"], "unverified");
}

#[tokio::test]
async fn sample_of_write_op_never_calls_the_system() {
    let w = world().await;
    let (base, m) = mock().await;
    w.system("shop-prod", "prod", &base, false, json!({})).await;
    for op in ["delivery.report_problem", "order.cancel", "payment.refund"] {
        let (s, b) = w
            .call(
                "POST",
                "/api/connections/systems/shop-prod/samples",
                json!({"op": op}),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{op}: {b}");
        assert_eq!(b["op"], op);
        assert_eq!(
            b["body"]["status"], "ok",
            "the template's first fixture: {b}"
        );
    }
    assert_eq!(hits(&m), 0);
}

#[tokio::test]
async fn results_reset_on_repin() {
    let w = world().await;
    let (s, b) = w
        .call(
            "POST",
            "/api/connections/systems",
            json!({"id": "shop-versioned", "label": "Versioned", "template_key": "test_shop_v",
                   "template_version": "1", "env": "test", "base_url": "", "env_ids": {},
                   "max_rps": null, "stub": true}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (_, b) = w
        .test("shop-versioned", json!({"op": "order.modify"}))
        .await;
    assert_eq!(b["outcome"], "green", "{b}");
    let (s, b) = w.repin("1").await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("no_change"))
    );
    let (s, b) = w.repin("1999").await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("template_not_found"))
    );
    let (s, b) = w.repin("2").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["template_version"], "2");
    let ops: Vec<&str> = b["contract"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["op"].as_str().unwrap())
        .collect();
    // every R op re-ran, plus the W op that had a result
    assert!(
        ops.contains(&"order.read") && ops.contains(&"order.modify"),
        "{ops:?}"
    );
    assert!(!ops.contains(&"order.cancel"), "{ops:?}");
    assert!(
        b["contract"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["outcome"] == "green"),
        "{b}"
    );
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action = 'connector.instance.repin'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(n, 1);
}
