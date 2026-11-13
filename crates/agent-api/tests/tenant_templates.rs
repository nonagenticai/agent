//! B7 T11: tenant-private connector templates (`/api/connections/templates…`, `/api/operations?fields=io`) on a real
//! Postgres 18: a tenant admin configures a standard REST connector on `generic_rest@2`, validates it (golden parity
//! with the TS validator on the fixture `frappe_helpdesk@2`), contract-tests it (tenant T1), activates it and pins a
//! system to it, whose T3 contract test calls an axum mock on loopback through the shared mapping/transport path.
//! Also: tenant isolation, roles, the immutable active version, retire-while-pinned, the next-draft copy, and D69:
//! every design write runs inside B2's `design::track_tenant` (one ledger row per job of the tenant, every job's
//! `draft_rev` bumped, audit rows marked `draft`); validate and contract-tests record no ledger row.
use crate::common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_api::connectors::transport::Transport;
use agent_api::netguard::Policy;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::get as route_get;
use axum::{Json, Router};
use base64::Engine as _;
use common::platform::{Op, generic_rest, publish_template};
use common::{TestEnv, get, insert_tenant, insert_user, json, person, send, sqlstate};
use serde_json::{Value, json};
use uuid::Uuid;

const KEY: &str = "acme_desk";
const API_KEY: &str = "tenant-api-key-5150";

fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
    vars.insert(
        "TENANT_DATA_KEYS".to_string(),
        format!(r#"{{"k1":"{key}"}}"#),
    );
    vars.insert("TENANT_DATA_KEY_CURRENT".to_string(), "k1".to_string());
}

/// Hits and the `Authorization` header values the mock saw.
#[derive(Clone, Default)]
struct Mock {
    hits: Arc<AtomicUsize>,
    auth: Arc<std::sync::Mutex<Vec<String>>>,
}

async fn ticket(
    State(m): State<Mock>,
    Path(id): Path<String>,
    h: HeaderMap,
) -> (StatusCode, Json<Value>) {
    m.hits.fetch_add(1, Ordering::SeqCst);
    let a = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    m.auth.lock().unwrap().push(a);
    (
        StatusCode::OK,
        Json(
            json!({"data": {"name": id, "status": "Open", "ticket_type": "Warranty", "agent_group": "Support", "modified": "2026-10-20T09:00:00Z"}}),
        ),
    )
}

/// The mock system: its base URL and its counters.
async fn mock() -> (String, Mock) {
    let m = Mock::default();
    let app = Router::new()
        .route("/tickets/{id}", route_get(ticket))
        .with_state(m.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://127.0.0.1:{port}"), m)
}

/// Two tenants (A, B), each with an integrations manager and an observer, behind an API whose connector policy
/// admits loopback (for the mock).
struct World {
    e: TestEnv,
    app: Router,
    a_im: String,
    a_obs: String,
    b_im: String,
}

async fn world() -> World {
    let e = common::env_with(with_keys).await;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    generic_rest(&e).await.unwrap();
    publish_template(&e, "test_shop", "3", &[Op::star("order", "read")])
        .await
        .unwrap();
    insert_tenant(&e.owner, a, "Tenant A").await;
    insert_tenant(&e.owner, b, "Tenant B").await;
    insert_user(&e.owner, "u-im-a", a, "integrations_manager").await;
    insert_user(&e.owner, "u-obs-a", a, "tenant_user").await;
    insert_user(&e.owner, "u-im-b", b, "integrations_manager").await;
    let mut st = e.state.clone();
    st.connectors = Transport::new(Policy {
        allow_loopback: true,
        private_targets: vec![],
    });
    let app = agent_api::router(st);
    World {
        e,
        app,
        a_im: person("u-im-a", &[]),
        a_obs: person("u-obs-a", &[]),
        b_im: person("u-im-b", &[]),
    }
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

fn tpl_uri(key: &str, version: &str, tail: &str) -> String {
    format!("/api/connections/templates/{key}/{version}{tail}")
}

impl World {
    async fn call(&self, tok: &str, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        send(&self.app, json(method, uri, Some(tok), body)).await
    }

    async fn read(&self, tok: &str, uri: &str) -> (StatusCode, Value) {
        send(&self.app, get(uri, Some(tok))).await
    }

    async fn create(&self, tok: &str, key: &str) -> (StatusCode, Value) {
        self.call(
            tok,
            "POST",
            "/api/connections/templates",
            json!({"key": key, "label": "Acme Desk", "kind": "custom", "auth_kind": "api_key_header",
                   "description": "Our in-house helpdesk"}),
        )
        .await
    }

    async fn save(&self, key: &str, version: &str, ops: Value) -> (StatusCode, Value) {
        self.call(
            &self.a_im,
            "PUT",
            &tpl_uri(key, version, ""),
            json!({"label": "Acme Desk", "description": "Our in-house helpdesk",
                   "credential_schema": [{"name": "api_key", "label": "API key", "secret": true}],
                   "ops": ops}),
        )
        .await
    }

    async fn post(&self, key: &str, version: &str, tail: &str, body: Value) -> (StatusCode, Value) {
        self.call(&self.a_im, "POST", &tpl_uri(key, version, tail), body)
            .await
    }

    /// Create `KEY@1`, save [`good_ops`], validate, contract-test and activate it.
    async fn active_template(&self) {
        let (s, b) = self.create(&self.a_im, KEY).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        let (s, b) = self.save(KEY, "1", good_ops()).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let (s, b) = self.post(KEY, "1", "/validate", json!({})).await;
        assert_eq!((s, b.clone()), (StatusCode::OK, json!([])), "{b}");
        let (s, b) = self.post(KEY, "1", "/contract-tests", json!({})).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["status"], "green", "{b}");
        let (s, b) = self
            .post(KEY, "1", "/status", json!({"status": "active"}))
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["status"], "active");
    }
}

/// A clean read: `case.read` by id, mapped from `$.data`.
fn good_ops() -> Value {
    json!([{
        "object": "case", "verb": "read", "variant": "*", "method": "get",
        "path": "/tickets/{{canonical.case_id}}", "request": "",
        "field_map": [
            {"path": "$.data.ticket_type", "field": "case_type", "transform": "lowercase"},
            {"path": "$.data.status", "field": "status", "transform": "lowercase"},
            {"path": "$.data.agent_group", "field": "queue", "transform": "none"}
        ],
        "status_map": [],
        "idempotency": {"kind": "nat"}
    }])
}

#[tokio::test]
async fn tenant_template_invisible_to_other_tenant() {
    let w = world().await;
    let (s, b) = w.create(&w.a_im, KEY).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["version"], "1");
    assert_eq!(b["status"], "draft");
    assert_eq!(b["base"], "generic_rest@2");
    assert_eq!(b["transport"], "rest");
    assert_eq!(
        b["credential_schema"],
        json!([{"name": "api_key", "label": "API key", "secret": true}])
    );
    assert_eq!(b["validation"], Value::Null);
    assert_eq!(b["contract"], Value::Null);

    let (s, b) = w.read(&w.a_obs, "/api/connections/templates").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"][0]["key"], KEY);
    assert_eq!(b["next"], Value::Null);
    assert_eq!(b["names"]["u-im-a"], "u-im-a");

    let (s, b) = w.read(&w.b_im, "/api/connections/templates").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"], json!([]));
    let (s, b) = w
        .call(&w.b_im, "POST", &tpl_uri(KEY, "1", "/validate"), json!({}))
        .await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "template_not_found"));
    // tenant B's catalogue has no row of tenant A's template; B may use the same key for its own
    let (s, b) = w.read(&w.b_im, "/api/connections/catalogue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(
        b["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["scope"] == "platform"),
        "{b}"
    );
    let (s, b) = w.create(&w.b_im, KEY).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
}

#[tokio::test]
async fn tenant_user_cannot_author() {
    let w = world().await;
    let (s, b) = w.create(&w.a_obs, KEY).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], "integrations_manager");
    let (s, b) = w.create(&w.a_im, KEY).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    for (method, tail, body) in [
        ("PUT", "", json!({"credential_schema": [], "ops": []})),
        ("POST", "/validate", json!({})),
        ("POST", "/contract-tests", json!({})),
        ("POST", "/status", json!({"status": "active"})),
        ("POST", "/drafts", json!({})),
    ] {
        let (s, b) = w
            .call(&w.a_obs, method, &tpl_uri(KEY, "1", tail), body)
            .await;
        assert_eq!(
            (s, b["error"]["missing_role"].as_str()),
            (StatusCode::FORBIDDEN, Some("integrations_manager")),
            "{method} {tail}: {b}"
        );
    }
    let (s, _) = w.read(&w.a_obs, "/api/connections/templates/usage").await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn create_with_platform_key_409() {
    let w = world().await;
    let (s, b) = w.create(&w.a_im, "test_shop").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "template_exists"),
        "{b}"
    );
    for bad in ["AB", "ab", "1abc", "has-dash"] {
        let (s, b) = w.create(&w.a_im, bad).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input"),
            "{bad}: {b}"
        );
    }
    let (s, b) = w.create(&w.a_im, KEY).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = w.create(&w.a_im, KEY).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "template_exists"),
        "{b}"
    );
    let (s, b) = w
        .call(
            &w.a_im,
            "POST",
            "/api/connections/templates",
            json!({"key": "basic_one", "label": "", "kind": "generic", "auth_kind": "basic", "description": ""}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["label"], "basic_one");
    assert_eq!(
        b["credential_schema"],
        json!([
            {"name": "username", "label": "Username", "secret": false},
            {"name": "password", "label": "Password", "secret": true}
        ])
    );
    let (s, b) = w
        .call(
            &w.a_im,
            "POST",
            "/api/connections/templates",
            json!({"key": "bundle_one", "label": "", "kind": "custom", "auth_kind": "bundle_token", "description": ""}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input"),
        "{b}"
    );
}

#[tokio::test]
async fn save_on_active_is_template_immutable() {
    let w = world().await;
    let (s, b) = w.create(&w.a_im, KEY).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    // a save normalises the method and refuses a method outside the list or a doubled op
    let (s, b) = w.save(KEY, "1", good_ops()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["ops"][0]["method"], "GET");
    let mut bad = good_ops();
    bad[0]["method"] = json!("TRACE");
    let (s, b) = w.save(KEY, "1", bad).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input"),
        "{b}"
    );
    let twice = json!([good_ops()[0].clone(), good_ops()[0].clone()]);
    let (s, b) = w.save(KEY, "1", twice).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input"),
        "{b}"
    );
    let (s, b) = w.save("nope_key", "1", good_ops()).await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "template_not_found"));

    // activate, then the version is frozen: API and database
    let (s, _) = w.post(KEY, "1", "/validate", json!({})).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = w.post(KEY, "1", "/contract-tests", json!({})).await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = w
        .post(KEY, "1", "/status", json!({"status": "active"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w.save(KEY, "1", good_ops()).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "template_immutable"),
        "{b}"
    );
    let (s, b) = w
        .post(KEY, "1", "/status", json!({"status": "active"}))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "template_immutable"),
        "{b}"
    );
    let err = sqlx::query("UPDATE tenant_connector_template SET label = 'x' WHERE key = $1")
        .bind(KEY)
        .execute(&w.e.owner)
        .await
        .expect_err("an active version is immutable");
    assert_eq!(sqlstate(&err).as_deref(), Some("P0001"));
    assert!(err.to_string().contains("template_immutable"), "{err}");
    let err = sqlx::query("UPDATE tenant_connector_template_op SET path = '/x' WHERE key = $1")
        .bind(KEY)
        .execute(&w.e.owner)
        .await
        .expect_err("the operations of an active version are immutable");
    assert!(err.to_string().contains("template_immutable"), "{err}");
    // results stay writable (a re-validation of an active version)
    let (s, b) = w.post(KEY, "1", "/validate", json!({})).await;
    assert_eq!((s, b.clone()), (StatusCode::OK, json!([])), "{b}");
}

#[tokio::test]
async fn activate_requires_validation_and_green_contract() {
    let w = world().await;
    let (s, b) = w.create(&w.a_im, KEY).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = w
        .post(KEY, "1", "/status", json!({"status": "active"}))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "validation_required"),
        "{b}"
    );
    let (s, b) = w.post(KEY, "1", "/contract-tests", json!({})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "validation_required"),
        "{b}"
    );
    // no operations yet: a validation error, so contract tests refuse
    let (s, b) = w.post(KEY, "1", "/validate", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b[0]["code"], "no_operations");
    let (s, b) = w.post(KEY, "1", "/contract-tests", json!({})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "validation_failed"),
        "{b}"
    );
    // an absolute URL is a server-side validation error
    let mut abs = good_ops();
    abs[0]["path"] = json!("https://elsewhere.example/tickets/{{canonical.case_id}}");
    let (s, _) = w.save(KEY, "1", abs).await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = w.post(KEY, "1", "/validate", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(
        b.as_array()
            .unwrap()
            .iter()
            .any(|e| e["code"] == "host_not_from_instance"),
        "{b}"
    );
    // clean: validated but not contract-tested
    let (s, _) = w.save(KEY, "1", good_ops()).await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = w.post(KEY, "1", "/validate", json!({})).await;
    assert_eq!((s, b.clone()), (StatusCode::OK, json!([])), "{b}");
    let (s, b) = w
        .post(KEY, "1", "/status", json!({"status": "active"}))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "contract_tests_required"),
        "{b}"
    );
    let (s, b) = w.post(KEY, "1", "/contract-tests", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (&b["status"], &b["passed"], &b["total"]),
        (&json!("green"), &json!(1), &json!(1)),
        "{b}"
    );
    assert!(b["at"].is_string());
    // a save resets both results
    let (s, b) = w.save(KEY, "1", good_ops()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (&b["validation"], &b["contract"]),
        (&Value::Null, &Value::Null)
    );
    let (s, b) = w
        .post(KEY, "1", "/status", json!({"status": "active"}))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "validation_required"),
        "{b}"
    );
    let (s, b) = w
        .post(KEY, "1", "/status", json!({"status": "paused"}))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input"),
        "{b}"
    );
    w.post(KEY, "1", "/validate", json!({})).await;
    w.post(KEY, "1", "/contract-tests", json!({})).await;
    let (s, b) = w
        .post(KEY, "1", "/status", json!({"status": "active"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "active");
}

#[tokio::test]
async fn retire_in_use_409() {
    let w = world().await;
    w.active_template().await;
    let (s, b) = w
        .call(
            &w.a_im,
            "POST",
            "/api/connections/systems",
            json!({"id": "acme-test", "label": "Acme (test)", "template_key": KEY, "template_version": "1",
                   "env": "test", "base_url": "https://desk.example.com", "env_ids": {}, "max_rps": null,
                   "stub": false}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["template_scope"], "tenant");
    assert_eq!(b["status"], "untested");

    let (s, b) = w.read(&w.a_obs, "/api/connections/systems/acme-test").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["template"]["status"], "active");
    assert_eq!(b["template"]["ops"][0]["op"], "case.read");
    assert_eq!(b["template"]["ops"][0]["side_effect"], "R");
    let (s, b) = w.read(&w.a_obs, "/api/connections/catalogue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let row = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == KEY)
        .expect("the tenant template is in the catalogue");
    assert_eq!(row["scope"], "tenant");
    assert_eq!(row["implements"], json!(["case.read"]));
    let (s, b) = w.read(&w.a_obs, "/api/connections/templates/usage").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["pins"],
        json!([{"key": KEY, "version": "1", "instance_id": "acme-test", "label": "Acme (test)",
                "env": "test", "status": "untested"}])
    );
    assert_eq!(b["routes"], json!([]));
    assert!(
        b["platform"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["key"] == "test_shop")
    );

    let (s, b) = w
        .post(KEY, "1", "/status", json!({"status": "retired"}))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "template_in_use"),
        "{b}"
    );
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("acme-test"),
        "{b}"
    );
    // an unknown version of the tenant key is still 404
    let (s, b) = w
        .call(
            &w.a_im,
            "POST",
            "/api/connections/systems",
            json!({"id": "acme-two", "label": "", "template_key": KEY, "template_version": "9",
                   "env": "test", "base_url": "https://desk.example.com", "env_ids": {}, "max_rps": null,
                   "stub": false}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "template_not_found"),
        "{b}"
    );
    // unpinned → retire works, and a retired template is out of the catalogue and cannot be pinned
    sqlx::query("DELETE FROM connector_instance WHERE id = 'acme-test'")
        .execute(&w.e.owner)
        .await
        .unwrap();
    let (s, b) = w
        .post(KEY, "1", "/status", json!({"status": "retired"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w
        .post(KEY, "1", "/status", json!({"status": "retired"}))
        .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "no_change"), "{b}");
    let (s, b) = w
        .call(
            &w.a_im,
            "POST",
            "/api/connections/systems",
            json!({"id": "acme-three", "label": "", "template_key": KEY, "template_version": "1",
                   "env": "test", "base_url": "https://desk.example.com", "env_ids": {}, "max_rps": null,
                   "stub": false}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "template_retired"),
        "{b}"
    );
}

#[tokio::test]
async fn draft_copy_and_draft_exists_409() {
    let w = world().await;
    w.active_template().await;
    let (s, b) = w.post(KEY, "1", "/drafts", json!({})).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["version"], "2");
    assert_eq!(b["status"], "draft");
    assert_eq!(b["validation"], Value::Null);
    assert_eq!(b["contract"], Value::Null);
    assert_eq!(b["ops"][0]["path"], "/tickets/{{canonical.case_id}}");
    assert_eq!(b["ops"].as_array().unwrap().len(), 1);
    let (s, b) = w.post(KEY, "1", "/drafts", json!({})).await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "draft_exists"), "{b}");
    let (s, b) = w.post(KEY, "2", "/drafts", json!({})).await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "draft_exists"), "{b}");
    // the draft is editable, the active version is not
    let (s, b) = w.save(KEY, "2", good_ops()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w.read(&w.a_im, "/api/connections/templates").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let order: Vec<(String, String)> = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["version"].as_str().unwrap().to_owned(),
                r["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        order,
        vec![
            ("1".to_owned(), "active".to_owned()),
            ("2".to_owned(), "draft".to_owned())
        ]
    );
}

/// The tenant `frappe_helpdesk@2` template the validator is checked against.
fn frappe_helpdesk_v2() -> Value {
    serde_json::from_str(include_str!(
        "fixtures/frappe_helpdesk_v2_tenant_template.json"
    ))
    .unwrap()
}

#[tokio::test]
async fn validator_refuses_the_frappe_helpdesk_v2_fixture() {
    let w = world().await;
    let fixture = &frappe_helpdesk_v2();
    // `frappe_helpdesk` is also a platform key: the tenant's copy goes under its own key
    let key = "lumen_helpdesk";
    let (s, b) = w.create(&w.a_im, key).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = w
        .call(
            &w.a_im,
            "PUT",
            &tpl_uri(key, "1", ""),
            json!({"label": fixture["label"], "description": fixture["description"],
                   "credential_schema": fixture["credential_schema"], "ops": fixture["ops"]}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w.post(key, "1", "/validate", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let codes: Vec<&str> = b
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["code"].as_str().unwrap())
        .collect();
    for want in [
        "literal_secret",
        "transform_not_allowed",
        "idempotency_required",
    ] {
        assert!(codes.contains(&want), "{want} missing: {b}");
    }
    // the template list carries exactly what /validate answered
    let validated = b.clone();
    let (s, b) = w.read(&w.a_im, "/api/connections/templates").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"][0]["validation"]["errors"], validated);
}

#[tokio::test]
async fn tenant_scope_instance_contract_test_hits_mock() {
    let w = world().await;
    let (base, m) = mock().await;
    w.active_template().await;
    let (s, b) = w
        .call(
            &w.a_im,
            "POST",
            "/api/connections/systems",
            json!({"id": "acme-test", "label": "Acme", "template_key": KEY, "template_version": "1",
                   "env": "test", "base_url": base, "env_ids": {"probe.case_id": "T-42"}, "max_rps": null,
                   "stub": false}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = w
        .call(
            &w.a_im,
            "PUT",
            "/api/connections/systems/acme-test/credential",
            json!({"api_key": API_KEY}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w
        .call(
            &w.a_im,
            "POST",
            "/api/connections/systems/acme-test/contract-tests",
            json!({"op": "case.read"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (&b["outcome"], &b["side_effect"]),
        (&json!("green"), &json!("R")),
        "{b}"
    );
    assert_eq!(m.hits.load(Ordering::SeqCst), 1);
    // api_key_header without auth_params: `Authorization: {{secret.api_key}}`
    assert_eq!(m.auth.lock().unwrap().as_slice(), [API_KEY.to_string()]);
    let (s, b) = w.read(&w.a_im, "/api/connections/systems/acme-test").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["instance"]["status"], "ok");
    assert_eq!(b["op_readiness"]["case.read"], "ready");
    // a sample of the read op goes to the system too
    let (s, b) = w
        .call(
            &w.a_im,
            "POST",
            "/api/connections/systems/acme-test/samples",
            json!({"op": "case.read"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["body"]["data"]["name"], "T-42");
    assert_eq!(m.hits.load(Ordering::SeqCst), 2);
    // the template's T1 now maps its field map against the recorded response (no call)
    let (s, b) = w.post(KEY, "1", "/contract-tests", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (&b["status"], &b["passed"]),
        (&json!("green"), &json!(1)),
        "{b}"
    );
    assert_eq!(m.hits.load(Ordering::SeqCst), 2);
    // a draft whose field map misses the recorded response is red
    let (s, _) = w.post(KEY, "1", "/drafts", json!({})).await;
    assert_eq!(s, StatusCode::CREATED);
    let mut ops = good_ops();
    ops[0]["field_map"][0]["path"] = json!("$.data.ticket_no");
    let (s, b) = w.save(KEY, "2", ops).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w.post(KEY, "2", "/validate", json!({})).await;
    assert_eq!((s, b.clone()), (StatusCode::OK, json!([])), "{b}");
    let (s, b) = w.post(KEY, "2", "/contract-tests", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (&b["status"], &b["passed"], &b["total"]),
        (&json!("red"), &json!(0), &json!(1)),
        "{b}"
    );
}

async fn draft_revs(owner: &sqlx::PgPool, jobs: &[Uuid]) -> Vec<i64> {
    let mut out = Vec::new();
    for j in jobs {
        out.push(
            sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
                .bind(j)
                .fetch_one(owner)
                .await
                .unwrap(),
        );
    }
    out
}

/// `(job_id, kind, area, page)` of the ledger rows of `key`, by job.
async fn ledger_rows(owner: &sqlx::PgPool, key: &str) -> Vec<(Uuid, String, String, String)> {
    sqlx::query_as(
        "SELECT job_id, kind, area, page FROM draft_ledger WHERE key = $1 ORDER BY job_id",
    )
    .bind(key)
    .fetch_all(owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn every_design_write_records_one_tenant_ledger_row_and_bumps_every_jobs_draft_rev() {
    let w = world().await;
    let t: Uuid = sqlx::query_scalar("SELECT tenant_id FROM users WHERE sub = 'u-im-a'")
        .fetch_one(&w.e.owner)
        .await
        .unwrap();
    let mut jobs: Vec<Uuid> = Vec::new();
    for key in ["care_one", "care_two"] {
        jobs.push(
            sqlx::query_scalar(
                "INSERT INTO jobs (tenant_id, key, label, pins, template_layer, template_version, created_by) \
                 SELECT $1, $2, 'Customer care', pins, layer, version, 'test' FROM job_template_tag \
                  WHERE layer = 'job/customer_care_agent' AND status = 'tagged' ORDER BY version DESC LIMIT 1 \
                 RETURNING id",
            )
            .bind(t)
            .bind(key)
            .fetch_one(&w.e.owner)
            .await
            .expect("a tagged customer-care template"),
        );
    }
    jobs.sort();
    let owner = &w.e.owner;
    let key1 = format!("tenant-template:{KEY}@1");
    let expect = |js: &[Uuid]| -> Vec<(Uuid, String, String, String)> {
        js.iter()
            .map(|j| {
                (
                    *j,
                    "tenant_template".to_string(),
                    "connections".to_string(),
                    "T12".to_string(),
                )
            })
            .collect()
    };

    let r0 = draft_revs(owner, &jobs).await;
    let (s, b) = w.create(&w.a_im, KEY).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(
        ledger_rows(owner, &key1).await,
        expect(&jobs),
        "one row per job"
    );
    let r1 = draft_revs(owner, &jobs).await;
    assert_eq!(r1, r0.iter().map(|r| r + 1).collect::<Vec<_>>());

    // results are not design: no ledger change, no bump
    let (s, b) = w.post(KEY, "1", "/validate", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        draft_revs(owner, &jobs).await,
        r1,
        "validate is not a design write"
    );
    assert_eq!(ledger_rows(owner, &key1).await, expect(&jobs));

    // a save is one more change of the same place: still one row per job, every draft moves
    let (s, b) = w.save(KEY, "1", good_ops()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(ledger_rows(owner, &key1).await, expect(&jobs));
    assert_eq!(
        draft_revs(owner, &jobs).await,
        r1.iter().map(|r| r + 1).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn every_design_write_is_audited_as_draft() {
    let w = world().await;
    w.active_template().await; // create, save, validate, contract-tests, status
    let (s, _) = w.post(KEY, "1", "/drafts", json!({})).await;
    assert_eq!(s, StatusCode::CREATED);
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT action, details->>'draft' FROM audit_event \
          WHERE action LIKE 'connector.tenant_template.%' ORDER BY id",
    )
    .fetch_all(&w.e.owner)
    .await
    .unwrap();
    let draft = Some("tenant_template".to_string());
    assert_eq!(
        rows,
        vec![
            (
                "connector.tenant_template.create".to_string(),
                draft.clone()
            ),
            ("connector.tenant_template.save".to_string(), draft.clone()),
            ("connector.tenant_template.validate".to_string(), None),
            ("connector.tenant_template.contract_test".to_string(), None),
            (
                "connector.tenant_template.status".to_string(),
                draft.clone()
            ),
            ("connector.tenant_template.draft".to_string(), draft),
        ]
    );
    let detail: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE action = 'connector.tenant_template.draft'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(
        (&detail["key"], &detail["version"]),
        (&json!(KEY), &json!("2"))
    );
}

#[tokio::test]
async fn op_defs_fields_io_only() {
    let w = world().await;
    let (s, b) = w.read(&w.a_obs, "/api/operations?fields=io").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let defs = b.as_array().expect("an OpDefLite array");
    let case_read = defs
        .iter()
        .find(|d| d["object"] == "case" && d["verb"] == "read" && d["variant"] == "*")
        .expect("case.read");
    assert_eq!(case_read["side_effect"], "R");
    assert!(
        case_read["output_fields"]
            .as_array()
            .unwrap()
            .contains(&json!("status"))
    );
    assert!(case_read["input_fields"].is_array());
    for uri in ["/api/operations?fields=all", "/api/operations"] {
        let (s, b) = w.read(&w.a_obs, uri).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input"),
            "{uri}: {b}"
        );
    }
}
