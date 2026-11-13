//! B7 T7: the platform connector-template routes on a real Postgres 18 — super_admin only; coverage (golden parity
//! with the TS fixture); usage across tenants; draft op saves, new draft versions, publish with the T1 fixture replay;
//! T2 sandbox runs (absent sandbox, platform connector, sandbox without its credential); the 0152 cross-tenant pin
//! definers. Every write leaves one platform audit row.
use crate::common;

use std::collections::HashMap;

use agent_api::connectors::coverage::tenant_rows;
use agent_api::platform_db::begin_platform;
use axum::http::StatusCode;
use base64::Engine as _;
use common::platform::{Op, draft_template, publish_template};
use common::{TestEnv, get, insert_tenant, insert_user, json, person, send, super_admin};
use serde_json::{Value, json};
use uuid::Uuid;

const BASE: &str = "/api/platform/connector-templates";

/// `TENANT_DATA_KEYS` with one kid `k1` (key bytes built at runtime: no key literal is committed).
fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
    vars.insert(
        "TENANT_DATA_KEYS".to_string(),
        format!(r#"{{"k1":"{key}"}}"#),
    );
    vars.insert("TENANT_DATA_KEY_CURRENT".to_string(), "k1".to_string());
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

async fn platform_audit(e: &TestEnv, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM platform_audit WHERE action = $1")
        .bind(action)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn sa_get(e: &TestEnv, uri: &str) -> (StatusCode, Value) {
    send(&e.app, get(uri, Some(&super_admin()))).await
}

async fn sa_send(e: &TestEnv, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, json(method, uri, Some(&super_admin()), body)).await
}

/// The platform templates the tests author through the routes (nothing is seeded): `test_shop@3` (active),
/// `test_sup@1` (active), `test_desk@0` (a draft with a read and a keyed write).
async fn shop(e: &TestEnv) {
    let ops = [
        Op::star("order", "read"),
        Op::star("order", "cancel"),
        Op::star("order", "list"),
    ];
    publish_template(e, "test_shop", "3", &ops).await.unwrap();
}

async fn support(e: &TestEnv) {
    let ops = [
        Op::star("case", "read"),
        Op::new("case", "create", "return"),
    ];
    publish_template(e, "test_sup", "1", &ops).await.unwrap();
}

async fn desk(e: &TestEnv) {
    let ops = [
        Op::star("case", "read"),
        Op::new("case", "create", "return"),
    ];
    draft_template(e, "test_desk", "0", &ops).await.unwrap();
}

/// The op `key` of `key@version` as the detail read answers it.
async fn op_of(e: &TestEnv, key: &str, version: &str, op: (&str, &str, &str)) -> Value {
    let (s, b) = sa_get(e, &format!("{BASE}/{key}/{version}")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["template"]["ops"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["object"] == op.0 && o["verb"] == op.1 && o["variant"] == op.2)
        .cloned()
        .unwrap_or_else(|| panic!("{key}@{version} has no op {op:?}"))
}

#[tokio::test]
async fn sa_only() {
    let e = common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant a").await;
    insert_user(&e.owner, "u-im", t, "integrations_manager").await;
    let im = person("u-im", &[]);
    let op = "case.read";
    for (method, uri) in [
        ("GET", format!("{BASE}/coverage")),
        ("GET", format!("{BASE}/test_desk/0/usage")),
        ("PUT", format!("{BASE}/test_desk/0/ops/{op}")),
        ("POST", format!("{BASE}/test_desk/0/drafts")),
        ("POST", format!("{BASE}/test_desk/0/publish")),
        ("POST", format!("{BASE}/test_desk/0/t2-runs")),
    ] {
        let (s, b) = send(&e.app, json(method, &uri, Some(&im), json!({}))).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{method} {uri}: {b}");
        assert_eq!(code(&b), "super_admin_required", "{method} {uri}");
    }
    for (method, uri) in [
        ("GET", format!("{BASE}/nope/1/usage")),
        ("POST", format!("{BASE}/nope/1/drafts")),
        ("POST", format!("{BASE}/nope/1/publish")),
        ("POST", format!("{BASE}/nope/1/t2-runs")),
    ] {
        let (s, b) = sa_send(&e, method, &uri, json!({})).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{method} {uri}: {b}");
        assert_eq!(code(&b), "template_not_found");
    }
}

#[tokio::test]
async fn save_op_on_active_is_409() {
    let e = common::env().await;
    shop(&e).await;
    let op = op_of(&e, "test_shop", "3", ("order", "read", "*")).await;
    let (s, b) = sa_send(&e, "PUT", &format!("{BASE}/test_shop/3/ops/order.read"), op).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "template_immutable");
    assert_eq!(platform_audit(&e, "connector_template.op_save").await, 0);
}

#[tokio::test]
async fn idempotency_none_on_write_is_422() {
    let e = common::env().await;
    desk(&e).await;
    let mut op = op_of(&e, "test_desk", "0", ("case", "create", "return")).await;
    let uri = format!("{BASE}/test_desk/0/ops/case.create%7Breturn%7D");
    // the body names a different op than the path
    let (s, b) = sa_send(
        &e,
        "PUT",
        &format!("{BASE}/test_desk/0/ops/case.read"),
        op.clone(),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "invalid_input");
    op["idempotency"] = json!({"kind": "none"});
    let (s, b) = sa_send(&e, "PUT", &uri, op.clone()).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "idempotency_required");
    // a keyed write saves (the path's op key is URL-decoded) and resets T1
    op["idempotency"] = json!({"kind": "header", "header": "Idempotency-Key"});
    let (s, b) = sa_send(&e, "PUT", &uri, op).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "draft");
    assert_eq!(b["t1"]["status"], "not_run");
    let saved = b["ops"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["verb"] == "create" && o["variant"] == "return")
        .cloned()
        .unwrap();
    assert_eq!(saved["idempotency"]["kind"], "header");
    assert_eq!(platform_audit(&e, "connector_template.op_save").await, 1);
}

#[tokio::test]
async fn draft_exists_is_409() {
    let e = common::env().await;
    desk(&e).await;
    support(&e).await;
    // test_desk@0 is itself the key's draft
    let (s, b) = sa_send(&e, "POST", &format!("{BASE}/test_desk/0/drafts"), json!({})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "draft_exists");
    // an active version without a draft gets the next version as a draft copy
    let (s, b) = sa_send(&e, "POST", &format!("{BASE}/test_sup/1/drafts"), json!({})).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["version"], "2");
    assert_eq!(b["status"], "draft");
    assert_eq!(b["t1"]["status"], "not_run");
    assert_eq!(b["published_by"], Value::Null);
    let (_, from) = sa_get(&e, &format!("{BASE}/test_sup/1")).await;
    assert_eq!(b["ops"], from["template"]["ops"], "ops copied");
    let (s, b) = sa_send(&e, "POST", &format!("{BASE}/test_sup/1/drafts"), json!({})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "draft_exists");
    assert_eq!(platform_audit(&e, "connector_template.draft").await, 1);
}

#[tokio::test]
async fn publish_replays_fixtures() {
    let e = common::env().await;
    desk(&e).await;
    let mut op = op_of(&e, "test_desk", "0", ("case", "read", "*")).await;
    op["field_map"][0]["path"] = json!("$.data.no_such_field");
    let (s, b) = sa_send(&e, "PUT", &format!("{BASE}/test_desk/0/ops/case.read"), op).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = sa_send(
        &e,
        "POST",
        &format!("{BASE}/test_desk/0/publish"),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "t1_failed");
    let failures = b["error"]["failures"].as_array().expect("failures listed");
    assert!(!failures.is_empty(), "{b}");
    assert!(
        failures.iter().all(|f| f["op"] == "case.read"
            && f["reason"].as_str().unwrap_or("").contains("no_such_field")),
        "{b}"
    );
    let (_, d) = sa_get(&e, &format!("{BASE}/test_desk/0")).await;
    assert_eq!(d["template"]["status"], "draft");
    assert_eq!(d["template"]["t1"]["status"], "red");
    let failed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform_audit WHERE action = 'connector_template.publish' AND status = 'failed'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(failed, 1);
}

#[tokio::test]
async fn publish_without_operations_is_422() {
    let e = common::env().await;
    sqlx::query(
        "INSERT INTO connector_template (key, version, label, vendor, system_class, wave, transport, auth_kind, \
                status) VALUES ('empty_vendor', '0', 'Empty', 'Acme', 'itsm', 'next', 'rest', 'none', 'draft')",
    )
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = sa_send(
        &e,
        "POST",
        &format!("{BASE}/empty_vendor/0/publish"),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "no_operations");
    assert!(b["error"]["errors"].is_array(), "{b}");
}

#[tokio::test]
async fn publish_makes_version_immutable() {
    let e = common::env().await;
    desk(&e).await;
    let (s, b) = sa_send(
        &e,
        "POST",
        &format!("{BASE}/test_desk/0/publish"),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "active");
    assert_eq!(b["t1"]["status"], "green");
    assert_eq!(b["published_by"], "u-super");
    assert!(b["published_at"].is_string(), "{b}");
    assert_eq!(platform_audit(&e, "connector_template.publish").await, 1);

    let op = op_of(&e, "test_desk", "0", ("case", "read", "*")).await;
    let (s, b) = sa_send(&e, "PUT", &format!("{BASE}/test_desk/0/ops/case.read"), op).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "template_immutable");
    let (s, b) = sa_send(
        &e,
        "POST",
        &format!("{BASE}/test_desk/0/publish"),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "template_immutable");
    // the trigger holds for the owner too: a non-result column of an active version never changes
    let err = sqlx::query(
        "UPDATE connector_template SET label = 'x' WHERE key = 'test_desk' AND version = '0'",
    )
    .execute(&e.owner)
    .await
    .unwrap_err();
    assert!(err.to_string().contains("template_immutable"), "{err}");
    // and the next version starts as a draft
    let (s, b) = sa_send(&e, "POST", &format!("{BASE}/test_desk/0/drafts"), json!({})).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["version"], "1");
}

#[tokio::test]
async fn coverage_reads_the_head_catalogue_and_cross_tenant_pins() {
    let e = common::env_with(with_keys).await;
    shop(&e).await;
    // a draft whose write has no idempotency key cannot be published
    draft_template(&e, "test_woo", "0", &[Op::star("order", "cancel")])
        .await
        .unwrap();
    sqlx::query("UPDATE connector_template_op SET idempotency = '{\"kind\": \"none\"}' WHERE key = 'test_woo'")
        .execute(&e.owner)
        .await
        .unwrap();
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant a").await;
    insert_user(&e.owner, "u-im", t, "integrations_manager").await;
    let im = person("u-im", &[]);
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/connections/systems",
            Some(&im),
            json!({
                "id": "shop-test", "label": "Shop", "template_key": "test_shop", "template_version": "3",
                "env": "test", "base_url": "", "env_ids": {}, "max_rps": null, "stub": true
            }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");

    // the catalogue is the base layer's `operation` rows: the platform authors the write operation the test names
    let base: String = sqlx::query_scalar("SELECT id::text FROM layer WHERE key = 'base'")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/layers/{base}/rows"),
            Some(&super_admin()),
            json!({"kind": "operation", "key": "case.create", "op": "add", "fields": {
                "discriminator": null,
                "attrs": {
                    "modality": "request", "is_derive": false, "side_effect": "W", "idempotency": "key+dup",
                    "confirm_back": true, "verify_read": null, "builtin": false, "op_kind": "standard",
                    "guard_rules": [], "identity_level": "2", "input_fields": ["case_type", "subject"],
                    "output_fields": ["case_id", "status"]
                },
                "description": "Open a case.", "retail": null, "stages": [], "extra_errors": []
            }}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");

    let (s, b) = sa_get(&e, &format!("{BASE}/coverage")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let catalogue = b["catalogue"].as_array().unwrap();
    assert!(
        catalogue
            .iter()
            .any(|c| c["key"] == "case.create" && c["side_effect"] == "W"),
        "B6 operations in the catalogue"
    );
    let demo = b["coverage"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["key"] == "test_shop" && c["version"] == "3")
        .cloned()
        .unwrap();
    assert_eq!(
        demo["pinned_by"],
        json!([{"tenant_id": t.to_string(), "tenant": "Tenant a", "instance_id": "shop-test", "env": "test"}])
    );
    let woo = b["coverage"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["key"] == "test_woo")
        .cloned()
        .unwrap();
    assert!(
        woo["publish_blockers"][0]
            .as_str()
            .unwrap_or("")
            .contains("order.cancel"),
        "{woo}"
    );
    let summary = b["templates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["key"] == "test_shop")
        .cloned()
        .unwrap();
    assert_eq!(summary["instances"], 1);
    assert!(summary.get("ops").is_none());
    assert!(
        !b["jobs"].as_array().unwrap().is_empty(),
        "job archetypes from B6"
    );

    // the list and the detail count the same pin
    let (_, list) = sa_get(&e, BASE).await;
    let row = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["key"] == "test_shop")
        .cloned()
        .unwrap();
    assert_eq!(row["instances"], 1);
    let (_, detail) = sa_get(&e, &format!("{BASE}/test_shop/3")).await;
    assert_eq!(
        detail["pinned_by"],
        json!([{"tenant": "Tenant a", "instance_id": "shop-test", "env": "test"}])
    );
}

#[tokio::test]
async fn sandbox_absent_is_409() {
    let e = common::env().await;
    support(&e).await;
    let (s, b) = sa_send(&e, "POST", &format!("{BASE}/test_sup/1/t2-runs"), json!({})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "sandbox_not_configured");
    assert_eq!(platform_audit(&e, "connector_template.t2_run").await, 0);
}

#[tokio::test]
async fn platform_connector_sandbox_is_409() {
    let e = common::env_with(|v| {
        v.insert(
            "CONNECTOR_SANDBOXES".into(),
            r#"{"strata_rag": {"base_url": "https://sandbox.example.com", "env": "AGENT_T7_SANDBOX_UNSET"}}"#.into(),
        );
    })
    .await;
    sqlx::query(
        "INSERT INTO connector_template (key, version, label, vendor, system_class, wave, platform_connector, \
                transport, auth_kind, status) \
         VALUES ('strata_rag', '1', 'Strata RAG', 'nonagentic', 'knowledge', 'v1', true, 'platform', 'bundle_token', \
                 'active')",
    )
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = sa_send(
        &e,
        "POST",
        &format!("{BASE}/strata_rag/1/t2-runs"),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "not_applicable");
}

#[tokio::test]
async fn sandbox_without_its_credential_records_red() {
    let e = common::env_with(|v| {
        v.insert(
            "CONNECTOR_SANDBOXES".into(),
            r#"{"test_sup": {"base_url": "https://sandbox.example.com", "env": "AGENT_T7_SANDBOX_CREDENTIAL_UNSET_9F2C"}}"#.into(),
        );
    })
    .await;
    support(&e).await;
    let (s, b) = sa_send(&e, "POST", &format!("{BASE}/test_sup/1/t2-runs"), json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "red");
    assert!(
        b["note"]
            .as_str()
            .unwrap_or("")
            .contains("AGENT_T7_SANDBOX_CREDENTIAL_UNSET_9F2C"),
        "{b}"
    );
    let (_, d) = sa_get(&e, &format!("{BASE}/test_sup/1")).await;
    assert_eq!(d["template"]["t2"]["status"], "red");
    assert_eq!(platform_audit(&e, "connector_template.t2_run").await, 1);
}

/// 0152: `agent_connector_pins()` / `agent_tenant_connector_bases()` are `agent_platform`-only and see every
/// tenant's rows; `coverage::tenant_rows` reads both tenants through them in one pass.
#[tokio::test]
async fn connector_pins_definer_reads_across_tenants() {
    let e = common::env_with(with_keys).await;
    shop(&e).await;
    common::platform::generic_rest(&e).await.unwrap();
    let mut tenants = Vec::new();
    for n in ["a", "b"] {
        let t = Uuid::new_v4();
        insert_tenant(&e.owner, t, &format!("Pins {n}")).await;
        let sub = format!("u-pin-{n}");
        insert_user(&e.owner, &sub, t, "integrations_manager").await;
        let (s, b) = send(
            &e.app,
            json(
                "POST",
                "/api/connections/systems",
                Some(&person(&sub, &[])),
                json!({
                    "id": format!("pin-{n}"), "label": "Shop", "template_key": "test_shop",
                    "template_version": "3", "env": "test", "base_url": "", "env_ids": {}, "max_rps": null,
                    "stub": true
                }),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        sqlx::query(
            "INSERT INTO tenant_connector_template (tenant_id, key, version, label, kind, auth_kind, created_by) \
             VALUES ($1, 'own_shop', '1', 'Own shop', 'generic', 'none', 'test')",
        )
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
        tenants.push(t);
    }

    // agent_app may not call either definer
    for f in [
        "SELECT * FROM agent_connector_pins()",
        "SELECT * FROM agent_tenant_connector_bases()",
    ] {
        let err = sqlx::query(f).execute(&e.state.db).await.unwrap_err();
        assert_eq!(
            common::sqlstate(&err).as_deref(),
            Some("42501"),
            "{f}: {err}"
        );
    }

    let mut tx = begin_platform(&e.state.db).await.unwrap();
    let pins: Vec<(Uuid, String, String, String, String)> = sqlx::query_as(
        "SELECT tenant_id, tenant_name, instance_id, template_key, template_version FROM agent_connector_pins() \
          WHERE tenant_id = ANY ($1) ORDER BY tenant_name",
    )
    .bind(tenants.clone())
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let s = |v: &str| v.to_string();
    assert_eq!(
        pins,
        [
            (tenants[0], s("Pins a"), s("pin-a"), s("test_shop"), s("3")),
            (tenants[1], s("Pins b"), s("pin-b"), s("test_shop"), s("3")),
        ]
    );

    // the coverage read (as agent_app, switching itself) sees both tenants' pins and bases, in tenant order
    let mut tx = e.state.db.begin().await.unwrap();
    let (pins, bases) = tenant_rows(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    let ours: Vec<(String, String, String)> = pins
        .iter()
        .filter(|p| tenants.iter().any(|t| t.to_string() == p.tenant_id))
        .map(|p| (p.tenant.clone(), p.instance_id.clone(), p.env.clone()))
        .collect();
    assert_eq!(
        ours,
        [
            (s("Pins a"), s("pin-a"), s("test")),
            (s("Pins b"), s("pin-b"), s("test")),
        ]
    );
    let ours: Vec<(String, String, String)> = bases
        .iter()
        .filter(|b| b.tenant.starts_with("Pins "))
        .map(|b| (b.tenant.clone(), b.key.clone(), b.base.clone()))
        .collect();
    assert_eq!(
        ours,
        [
            (s("Pins a"), s("own_shop"), s("generic_rest@2")),
            (s("Pins b"), s("own_shop"), s("generic_rest@2")),
        ]
    );
}
