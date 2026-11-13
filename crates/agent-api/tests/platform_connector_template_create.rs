//! `POST /api/platform/connector-templates` on a real Postgres 18: a super_admin creates a DRAFT platform connector
//! template (nothing is compiled in), it is readable, duplicates are 409, a non-super-admin is 403, and a published
//! `generic_rest` base is what tenant templates build on (the platform offering is rows, created here).
use crate::common;

use std::collections::HashMap;

use axum::http::StatusCode;
use base64::Engine as _;
use common::{TestEnv, insert_tenant, insert_user, json, person, send, super_admin};
use serde_json::{Value, json};
use uuid::Uuid;

const BASE: &str = "/api/platform/connector-templates";

fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([5u8; 32]);
    vars.insert(
        "TENANT_DATA_KEYS".to_string(),
        format!(r#"{{"k1":"{key}"}}"#),
    );
    vars.insert("TENANT_DATA_KEY_CURRENT".to_string(), "k1".to_string());
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

async fn sa(e: &TestEnv, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, json(method, uri, Some(&super_admin()), body)).await
}

async fn audit_count(e: &TestEnv, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM platform_audit WHERE action = $1")
        .bind(action)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

fn read_op() -> Value {
    json!({
        "object": "case", "verb": "read", "variant": "*", "side_effect": "R", "op_kind": "standard",
        "method": "GET", "path": "/tickets/{{canonical.case_id}}", "request": "",
        "field_map": [
            {"path": "$.data.ticket_type", "field": "case_type", "transform": "lowercase"},
            {"path": "$.data.status", "field": "status", "transform": "lowercase"},
            {"path": "$.data.agent_group", "field": "queue", "transform": "none"}
        ],
        "status_map": [], "fault_map": [], "pagination": null,
        "idempotency": {"kind": "nat"}, "verify_read": null, "timeout_ms": 5000, "fixtures": []
    })
}

fn generic_rest() -> Value {
    json!({
        "key": "generic_rest", "version": "2", "label": "Generic REST", "vendor": "Nonagentic",
        "system_class": "generic", "wave": "base", "transport": "rest", "auth_kind": "api_key_header",
        "description": "Declarative base of every tenant template", "ops": []
    })
}

#[tokio::test]
async fn create_get_duplicate_and_roles() {
    let e = common::env_bare_with(with_keys).await;
    let body = json!({
        "key": "acme_desk", "label": "Acme Desk", "vendor": "Acme", "system_class": "helpdesk", "wave": "w1",
        "transport": "rest", "auth_kind": "api_key_header", "ops": [read_op()]
    });
    let (s, b) = sa(&e, "POST", BASE, body.clone()).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["key"], "acme_desk");
    assert_eq!(b["version"], "1");
    assert_eq!(b["status"], "draft");
    assert_eq!(b["ops"].as_array().unwrap().len(), 1);
    assert_eq!(b["credential_schema"][0]["name"], "api_key");
    assert_eq!(audit_count(&e, "connector_template.create").await, 1);

    let (s, b) = sa(&e, "GET", &format!("{BASE}/acme_desk/1"), json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["template"]["label"], "Acme Desk");
    let (s, b) = sa(&e, "GET", BASE, json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(
        b["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["key"] == "acme_desk" && t["version"] == "1")
    );

    // same key@version again; then the same key with another version while the draft is open
    let (s, b) = sa(&e, "POST", BASE, body.clone()).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "template_exists");
    let mut other = body.clone();
    other["version"] = json!("2");
    let (s, b) = sa(&e, "POST", BASE, other).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "draft_exists");
    assert_eq!(audit_count(&e, "connector_template.create").await, 1);

    // invalid: bad key, empty label, a write op without idempotency
    for bad in [
        json!({"key": "Bad Key", "label": "x", "transport": "rest", "auth_kind": "none"}),
        json!({"key": "ok_key", "label": " ", "transport": "rest", "auth_kind": "none"}),
        json!({"key": "ok_key", "label": "x", "transport": "rest", "auth_kind": "none",
               "rate_limit_model": []}),
    ] {
        let (s, b) = sa(&e, "POST", BASE, bad).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert_eq!(code(&b), "invalid_input");
    }
    let mut write = read_op();
    write["verb"] = json!("create");
    write["side_effect"] = json!("W");
    write["idempotency"] = json!({"kind": "none"});
    let (s, b) = sa(
        &e,
        "POST",
        BASE,
        json!({"key": "ok_key", "label": "x", "transport": "rest", "auth_kind": "none", "ops": [write]}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "idempotency_required");

    // not a super_admin
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant a").await;
    insert_user(&e.owner, "u-im", t, "integrations_manager").await;
    let (s, b) = send(&e.app, json("POST", BASE, Some(&person("u-im", &[])), body)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(code(&b), "super_admin_required");
}

#[tokio::test]
async fn generic_rest_base_is_created_published_and_pinned_by_a_tenant_template() {
    let e = common::env_bare_with(with_keys).await;
    let (s, b) = sa(&e, "POST", BASE, generic_rest()).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["status"], "draft");
    let (s, b) = sa(
        &e,
        "POST",
        &format!("{BASE}/generic_rest/2/publish"),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "active");
    assert_eq!(b["t1"]["status"], "green");
    // a published version never changes
    let (s, b) = sa(&e, "POST", BASE, generic_rest()).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "template_exists");

    // a tenant template (base generic_rest@2) validates
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant a").await;
    insert_user(&e.owner, "u-im", t, "integrations_manager").await;
    let im = person("u-im", &[]);
    let call = |m: &'static str, uri: String, body: Value| {
        let (app, im) = (e.app.clone(), im.clone());
        async move { send(&app, json(m, &uri, Some(&im), body)).await }
    };
    let (s, b) = call(
        "POST",
        "/api/connections/templates".into(),
        json!({"key": "acme_desk", "label": "Acme Desk", "kind": "generic", "auth_kind": "api_key_header"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["base"], "generic_rest@2");
    let (s, b) = call(
        "PUT",
        "/api/connections/templates/acme_desk/1".into(),
        json!({"label": "Acme Desk", "description": "",
        "credential_schema": [{"name": "api_key", "label": "API key", "secret": true}],
        "ops": [{
            "object": "case", "verb": "read", "variant": "*", "method": "get",
            "path": "/tickets/{{canonical.case_id}}", "request": "",
            "field_map": [{"path": "$.data.status", "field": "status", "transform": "lowercase"}],
            "status_map": [], "idempotency": {"kind": "nat"}
        }]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = call(
        "POST",
        "/api/connections/templates/acme_desk/1/validate".into(),
        json!({}),
    )
    .await;
    assert_eq!((s, b.clone()), (StatusCode::OK, json!([])), "{b}");

    let (s, b) = sa(&e, "GET", &format!("{BASE}/generic_rest/2"), json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}
