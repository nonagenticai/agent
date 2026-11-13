//! B7 T9: the systems API (`/api/connections/…`) and the platform template reads on a real Postgres 18: roles
//! (member reads, integrations_manager writes), the SSRF guard's UI codes, stub and platform-connector refusals,
//! write-only credentials (the value is in no response, no table and no audit row), tenant isolation.
use crate::common;

use std::collections::HashMap;

use axum::http::StatusCode;
use base64::Engine as _;
use common::platform::{Op, publish_template};
use common::{TestEnv, get, insert_tenant, insert_user, json, person, send, super_admin};
use serde_json::{Value, json};
use uuid::Uuid;

const SECRET: &str = "svc-PLAINTEXT-value-4242";

/// `TENANT_DATA_KEYS` with one kid `k1` (key bytes built at runtime: no key literal is committed).
fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
    vars.insert(
        "TENANT_DATA_KEYS".to_string(),
        format!(r#"{{"k1":"{key}"}}"#),
    );
    vars.insert("TENANT_DATA_KEY_CURRENT".to_string(), "k1".to_string());
}

/// A tenant with an integrations manager `u-im-<n>` and an observer `u-obs-<n>`; (env, tenant, im token, obs token).
async fn setup() -> (TestEnv, Uuid, String, String) {
    let e = common::env_with(with_keys).await;
    let (t, im, obs) = tenant(&e, "a").await;
    (e, t, im, obs)
}

/// The platform template every system pins (authored through the routes; idempotent per database).
async fn shop_template(e: &TestEnv, version: &str) {
    let ops = [
        Op::star("order", "read"),
        Op::star("order", "cancel"),
        Op::new("case", "create", "return"),
    ];
    publish_template(e, "test_shop", version, &ops)
        .await
        .unwrap();
}

async fn tenant(e: &TestEnv, n: &str) -> (Uuid, String, String) {
    shop_template(e, "3").await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, &format!("Tenant {n}")).await;
    let im = format!("u-im-{n}");
    let obs = format!("u-obs-{n}");
    insert_user(&e.owner, &im, t, "integrations_manager").await;
    insert_user(&e.owner, &obs, t, "tenant_user").await;
    (t, person(&im, &[]), person(&obs, &[]))
}

fn new_instance(id: &str, env: &str, base_url: &str, stub: bool) -> Value {
    json!({
        "id": id, "label": "Shop", "template_key": "test_shop", "template_version": "3",
        "env": env, "base_url": base_url, "env_ids": {}, "max_rps": null, "stub": stub
    })
}

async fn create(e: &TestEnv, tok: &str, body: Value) -> (StatusCode, Value) {
    send(
        &e.app,
        json("POST", "/api/connections/systems", Some(tok), body),
    )
    .await
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

async fn audit_count(e: &TestEnv, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = $1")
        .bind(action)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn tenant_user_reads_integrations_manager_writes() {
    let (e, _t, im, obs) = setup().await;
    let (s, b) = create(&e, &obs, new_instance("shop-test", "test", "", true)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], "integrations_manager");

    let (s, b) = create(&e, &im, new_instance("shop-test", "test", "", true)).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["base_url"], "stub://test_shop");
    assert_eq!(b["status"], "untested");
    assert_eq!(
        b["credential"],
        json!({"set": false, "updated_at": null, "fingerprint": null})
    );
    assert_eq!(audit_count(&e, "connector.instance.create").await, 1);

    let (s, b) = send(&e.app, get("/api/connections/systems", Some(&obs))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"][0]["id"], "shop-test");
    assert_eq!(b["next"], Value::Null);
    let (s, b) = send(
        &e.app,
        get("/api/connections/systems/shop-test", Some(&obs)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["instance"]["id"], "shop-test");
    assert_eq!(b["template"]["status"], "active");
    assert_eq!(b["used_by"], json!([]));
    assert_eq!(b["op_readiness"]["order.read"], "stub");
    let (s, b) = send(&e.app, get("/api/connections/catalogue", Some(&obs))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let row = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == "test_shop")
        .expect("test_shop in the catalogue");
    // the test template is REST-only (no `alt_transport: stub`): it is not stub-capable, whatever an instance of it
    // is created as
    assert_eq!(row["stub_capable"], false);
    assert_eq!(row["scope"], "platform");

    for (method, uri, body) in [
        (
            "PATCH",
            "/api/connections/systems/shop-test",
            json!({"label": "x"}),
        ),
        (
            "PUT",
            "/api/connections/systems/shop-test/credential",
            json!({"service_token": "x"}),
        ),
        (
            "POST",
            "/api/connections/systems/shop-test/repin",
            json!({"version": "4"}),
        ),
        (
            "POST",
            "/api/connections/systems/shop-test/contract-tests",
            json!({"op": "order.read"}),
        ),
        (
            "POST",
            "/api/connections/systems/shop-test/samples",
            json!({"op": "order.read"}),
        ),
    ] {
        let (s, b) = send(&e.app, json(method, uri, Some(&obs), body)).await;
        assert_eq!(
            (s, b["error"]["missing_role"].as_str()),
            (StatusCode::FORBIDDEN, Some("integrations_manager")),
            "{method} {uri}: {b}"
        );
    }
    let (s, b) = send(
        &e.app,
        json(
            "PATCH",
            "/api/connections/systems/shop-test",
            Some(&im),
            json!({"label": "Shop (test)", "max_rps": 5, "env_ids": {"probe.id": "o-1"}}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["label"], "Shop (test)");
    assert_eq!(b["max_rps"], 5.0);
    assert_eq!(b["env_ids"]["probe.id"], "o-1");
    assert_eq!(audit_count(&e, "connector.instance.update").await, 1);
    let (s, b) = send(
        &e.app,
        json(
            "PATCH",
            "/api/connections/systems/shop-test",
            Some(&im),
            json!({"max_rps": 0}),
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input")
    );
}

#[tokio::test]
async fn ssrf_refusals_use_ui_codes() {
    let (e, _t, im, _obs) = setup().await;
    for (url, want) in [
        ("http://shop.example.com", "invalid_url"),
        ("not a url", "invalid_url"),
        ("https://user:pw@shop.example.com", "invalid_url"),
        ("https://shop.example.com/?a=b", "invalid_url"),
        ("https://10.0.0.5", "invalid_target"),
        ("https://100.64.1.2", "invalid_target"),
        ("https://127.0.0.1", "invalid_target"),
        ("https://frappe.frappe.svc.cluster.local", "invalid_target"),
        ("https://registry.internal", "invalid_target"),
    ] {
        let (s, b) = create(&e, &im, new_instance("shop-prod", "prod", url, false)).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, want),
            "{url}: {b}"
        );
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM connector_instance")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0, "a refused target stores nothing");
    let (s, b) = create(
        &e,
        &im,
        new_instance("shop-prod", "prod", "https://shop.example.com/", false),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["base_url"], "https://shop.example.com");
    let (s, b) = create(
        &e,
        &im,
        new_instance("shop-prod", "prod", "https://shop.example.com", false),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "instance_exists"),
        "{b}"
    );
    let (s, b) = send(
        &e.app,
        json(
            "PATCH",
            "/api/connections/systems/shop-prod",
            Some(&im),
            json!({"base_url": "https://192.168.1.10"}),
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_target"),
        "{b}"
    );
    let (s, b) = create(
        &e,
        &im,
        new_instance("Bad Id", "prod", "https://a.example.com", false),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input"),
        "{b}"
    );
}

#[tokio::test]
async fn stub_in_prod_is_422() {
    let (e, _t, im, _obs) = setup().await;
    let (s, b) = create(&e, &im, new_instance("shop-stub", "prod", "", true)).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "stub_not_allowed_in_prod"),
        "{b}"
    );
}

#[tokio::test]
async fn platform_template_cannot_be_instantiated_409() {
    let (e, _t, im, _obs) = setup().await;
    sqlx::query(
        "INSERT INTO connector_template (key, version, label, vendor, system_class, wave, platform_connector, \
                                         transport, auth_kind, status) \
         VALUES ('strata_rag', '1', 'Strata RAG', 'Agent', 'knowledge', 'v1', true, 'platform', 'bundle_token', 'active')",
    )
    .execute(&e.owner)
    .await
    .unwrap();
    let mut body = new_instance("rag", "test", "https://rag.example.com", false);
    body["template_key"] = json!("strata_rag");
    body["template_version"] = json!("1");
    let (s, b) = create(&e, &im, body).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "platform_managed"),
        "{b}"
    );
    let mut body = new_instance("nope", "test", "https://a.example.com", false);
    body["template_version"] = json!("999");
    let (s, b) = create(&e, &im, body).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "template_not_found"),
        "{b}"
    );
    // not in the catalogue either
    let (_, b) = send(&e.app, get("/api/connections/catalogue", Some(&im))).await;
    assert!(
        b["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["key"] != "strata_rag"),
        "{b}"
    );
}

#[tokio::test]
async fn credential_get_never_returns_the_value() {
    let (e, _t, im, obs) = setup().await;
    let (s, b) = create(
        &e,
        &im,
        new_instance("shop-prod", "prod", "https://shop.example.com", false),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let mut bodies = vec![b];
    let (s, b) = send(
        &e.app,
        json(
            "PUT",
            "/api/connections/systems/shop-prod/credential",
            Some(&im),
            json!({"service_token": SECRET}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["set"], true);
    let fp = b["fingerprint"].as_str().unwrap().to_string();
    assert_eq!(fp.len(), 8);
    assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(b["updated_at"].is_string());
    bodies.push(b);
    for tok in [&im, &obs] {
        for uri in [
            "/api/connections/systems",
            "/api/connections/systems/shop-prod",
        ] {
            let (s, b) = send(&e.app, get(uri, Some(tok))).await;
            assert_eq!(s, StatusCode::OK, "{b}");
            bodies.push(b);
        }
    }
    let (_, detail) = send(
        &e.app,
        get("/api/connections/systems/shop-prod", Some(&obs)),
    )
    .await;
    assert_eq!(detail["instance"]["credential"]["set"], true);
    assert_eq!(detail["instance"]["credential"]["fingerprint"], fp);
    for b in &bodies {
        assert!(
            !b.to_string().contains(SECRET),
            "a response carries the secret: {b}"
        );
    }
    let cipher: Vec<u8> = sqlx::query_scalar("SELECT ciphertext FROM connector_credential")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert!(
        !cipher.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()),
        "the ciphertext holds the plaintext"
    );
    for table in [
        "SELECT coalesce(string_agg(to_jsonb(c)::text, ''), '') FROM connector_credential c",
        "SELECT coalesce(string_agg(to_jsonb(i)::text, ''), '') FROM connector_instance i",
        "SELECT coalesce(string_agg(to_jsonb(a)::text, ''), '') FROM audit_event a",
    ] {
        let text: String = sqlx::query_scalar(table).fetch_one(&e.owner).await.unwrap();
        assert!(!text.contains(SECRET), "{table}");
    }
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE action = 'connector.credential.set'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let mut keys: Vec<&String> = details.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["credential_version", "fingerprint", "instance"]);
    assert_eq!(details["fingerprint"], fp);
    // a second write bumps the version
    let (s, b) = send(
        &e.app,
        json(
            "PUT",
            "/api/connections/systems/shop-prod/credential",
            Some(&im),
            json!({"service_token": "another-value-77"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_ne!(b["fingerprint"], fp);
    let v: i32 = sqlx::query_scalar("SELECT credential_version FROM connector_credential")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(v, 2);
}

#[tokio::test]
async fn credential_unknown_field_422() {
    let (e, _t, im, _obs) = setup().await;
    let (s, _) = create(
        &e,
        &im,
        new_instance("shop-prod", "prod", "https://shop.example.com", false),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let put = |body: Value| {
        json(
            "PUT",
            "/api/connections/systems/shop-prod/credential",
            Some(&im),
            body,
        )
    };
    let (s, b) = send(&e.app, put(json!({"service_token": "x", "nope": "y"}))).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input"),
        "{b}"
    );
    assert!(b["error"]["message"].as_str().unwrap().contains("nope"));
    let (s, b) = send(&e.app, put(json!({"service_token": "  "}))).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input"),
        "{b}"
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM connector_credential")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
    let (s, _) = create(&e, &im, new_instance("shop-stub", "test", "", true)).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, b) = send(
        &e.app,
        json(
            "PUT",
            "/api/connections/systems/shop-stub/credential",
            Some(&im),
            json!({"service_token": "x"}),
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "not_applicable"),
        "{b}"
    );
    let (s, b) = send(
        &e.app,
        json(
            "PUT",
            "/api/connections/systems/ghost/credential",
            Some(&im),
            json!({"service_token": "x"}),
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "instance_not_found"),
        "{b}"
    );
}

#[tokio::test]
async fn tenant_b_cannot_read_tenant_a_system() {
    let (e, _a, im_a, _obs_a) = setup().await;
    let (_b, im_b, obs_b) = tenant(&e, "b").await;
    let (s, _) = create(
        &e,
        &im_a,
        new_instance("shop-a", "prod", "https://a.example.com", false),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, b) = send(&e.app, get("/api/connections/systems/shop-a", Some(&obs_b))).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "instance_not_found"),
        "{b}"
    );
    let (s, b) = send(&e.app, get("/api/connections/systems", Some(&obs_b))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["items"], json!([]));
    let (s, b) = send(
        &e.app,
        json(
            "PUT",
            "/api/connections/systems/shop-a/credential",
            Some(&im_b),
            json!({"service_token": "x"}),
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "instance_not_found"),
        "{b}"
    );
    // the same id is free in tenant b
    let (s, b) = create(
        &e,
        &im_b,
        new_instance("shop-a", "prod", "https://b.example.com", false),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
}

#[tokio::test]
async fn platform_template_reads_are_super_admin_only() {
    let (e, _t, im, _obs) = setup().await;
    let (s, b) = send(&e.app, get("/api/platform/connector-templates", Some(&im))).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "super_admin_required"),
        "{b}"
    );
    let sa = super_admin();
    shop_template(&e, "4").await;
    let (s, b) = send(&e.app, get("/api/platform/connector-templates", Some(&sa))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let demo = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == "test_shop")
        .expect("test_shop listed");
    assert!(demo.get("ops").is_none());
    assert!(!demo["implements"].as_array().unwrap().is_empty());
    assert_eq!(demo["instances"], 0);
    let (s, b) = send(
        &e.app,
        get("/api/platform/connector-templates/test_shop/3", Some(&sa)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(!b["template"]["ops"].as_array().unwrap().is_empty());
    assert_eq!(b["versions"][0]["version"], "4");
    assert_eq!(b["pinned_by"], json!([]));
    let (s, b) = send(
        &e.app,
        get("/api/platform/connector-templates/test_shop/999", Some(&sa)),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "template_not_found"),
        "{b}"
    );
}
