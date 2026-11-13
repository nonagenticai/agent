//! Industries and departments as a super_admin with no tenant (migration 0230, `platform::routes::industries`):
//! create, list, rename, retire (refused while a tenant job selects it), restore; a retired industry leaves the
//! tenant's choices. One platform audit row per write.
use crate::common;

use axum::http::StatusCode;
use common::platform::{DEPARTMENT, DEPARTMENT_LAYER, INDUSTRY, offering};
use common::{TestEnv, get, insert_tenant, insert_user, json, person, send, super_admin};
use serde_json::{Value, json};
use uuid::Uuid;

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

async fn sa(e: &TestEnv, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, json(method, uri, Some(&super_admin()), body)).await
}

async fn audit(e: &TestEnv, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM platform_audit WHERE action = $1")
        .bind(action)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn industries(e: &TestEnv) -> Vec<Value> {
    let (s, b) = send(
        &e.app,
        get("/api/platform/industries", Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["items"].as_array().unwrap().clone()
}

fn find<'a>(items: &'a [Value], industry: &str) -> &'a Value {
    items
        .iter()
        .find(|i| i["industry"] == industry)
        .unwrap_or_else(|| panic!("no industry {industry} in {items:?}"))
}

/// The industries a tenant admin may choose for the company profile (`release::compile::db_options`).
async fn offered(e: &TestEnv) -> Vec<String> {
    let (s, b) = send(
        &e.app,
        get("/api/tenant/profile", Some(&person("u-admin", &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["options"]["industries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["key"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn industries_and_departments_lifecycle() {
    let e = common::env().await;
    let o = offering(&e).await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;

    // a tenant job selects the offering's industry and department
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/jobs",
            Some(&person("u-admin", &[])),
            json!({"template": o.template()}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");

    // not a super_admin: 403
    let (s, _) = send(
        &e.app,
        get("/api/platform/industries", Some(&person("u-admin", &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    let items = industries(&e).await;
    let retail = find(&items, INDUSTRY);
    assert_eq!(retail["key"], "industry/retail");
    assert_eq!(retail["jobs"].as_array().unwrap().len(), 1, "{retail}");
    assert_eq!(retail["jobs"][0]["tenant_name"], "Alpha");
    let dept = &retail["departments"][0];
    assert_eq!(dept["key"], DEPARTMENT_LAYER);
    assert_eq!(dept["jobs"].as_array().unwrap().len(), 1, "{dept}");
    assert!(
        retail["tag"]
            .as_str()
            .unwrap()
            .starts_with("industry/retail@v")
    );

    // create: a new industry; bad key 422, again 409
    let (s, b) = sa(
        &e,
        "POST",
        "/api/platform/industries",
        json!({"industry": "telecom", "label": "Telecom"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["key"], "industry/telecom");
    assert_eq!(b["label"], "Telecom");
    assert_eq!(b["tag"], Value::Null);
    assert_eq!(b["departments"], json!([]));
    assert_eq!(audit(&e, "industry.create").await, 1);
    let (s, b) = sa(
        &e,
        "POST",
        "/api/platform/industries",
        json!({"industry": "Tele com", "label": "X"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "invalid_layer_key");
    let (s, b) = sa(
        &e,
        "POST",
        "/api/platform/industries",
        json!({"industry": "telecom", "label": "Telecom"}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "layer_exists");

    // rename: the label only
    let (s, b) = sa(
        &e,
        "PATCH",
        "/api/platform/industries/telecom",
        json!({"label": "Telecommunications"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["label"], "Telecommunications");
    assert_eq!(b["key"], "industry/telecom");
    let (s, b) = sa(
        &e,
        "PATCH",
        "/api/platform/industries/nope",
        json!({"label": "X"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "industry_not_found");

    // a department; an unknown archetype is 404
    let (s, b) = sa(
        &e,
        "POST",
        "/api/platform/industries/telecom/departments",
        json!({"department": "support", "label": "Support"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["key"], "industry_dept/telecom/support");
    assert_eq!(audit(&e, "department.create").await, 1);
    let (s, b) = sa(
        &e,
        "POST",
        "/api/platform/industries/telecom/departments",
        json!({"department": "billing", "label": "Billing", "extends": "no_such_archetype"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "parent_layer_not_found");
    let (s, b) = sa(
        &e,
        "PATCH",
        "/api/platform/industries/telecom/departments/support",
        json!({"label": "Customer support"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["label"], "Customer support");

    // in use: refused with the jobs that select it
    let (s, b) = sa(
        &e,
        "POST",
        "/api/platform/industries/retail/retire",
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "in_use");
    assert_eq!(b["error"]["jobs"].as_array().unwrap().len(), 1, "{b}");
    let (s, b) = sa(
        &e,
        "POST",
        "/api/platform/industries/retail/departments/customer_service/retire",
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "in_use");

    // publish telecom (one row, one tag): a tenant can choose it
    let id: String =
        sqlx::query_scalar("SELECT id::text FROM layer WHERE key = 'industry/telecom'")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    let (s, b) = sa(
        &e,
        "POST",
        &format!("/api/layers/{id}/rows"),
        json!({"kind": "job_setting", "key": "telecom_default", "op": "add",
               "fields": {"label": "Telecom default"}}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = sa(
        &e,
        "POST",
        "/api/platform/templates/industry.telecom/tags",
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert!(offered(&e).await.contains(&"telecom".to_owned()));

    // retire: gone from the tenant choices; twice is no_change; restore brings it back
    let (s, b) = sa(
        &e,
        "POST",
        "/api/platform/industries/telecom/retire",
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["retired_at"].is_string(), "{b}");
    assert_eq!(audit(&e, "industry.retire").await, 1);
    assert!(!offered(&e).await.contains(&"telecom".to_owned()));
    assert!(offered(&e).await.contains(&INDUSTRY.to_owned()));
    let (s, b) = sa(
        &e,
        "POST",
        "/api/platform/industries/telecom/retire",
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "no_change");
    let (s, b) = sa(
        &e,
        "POST",
        "/api/platform/industries/telecom/departments",
        json!({"department": "sales", "label": "Sales"}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "industry_retired");
    let (s, b) = sa(
        &e,
        "POST",
        "/api/platform/industries/telecom/restore",
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["retired_at"], Value::Null);
    assert_eq!(audit(&e, "industry.restore").await, 1);
    assert!(offered(&e).await.contains(&"telecom".to_owned()));

    // a department retires and restores on its own
    let (s, b) = sa(
        &e,
        "POST",
        "/api/platform/industries/telecom/departments/support/retire",
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["retired_at"].is_string(), "{b}");
    let (s, b) = sa(
        &e,
        "POST",
        "/api/platform/industries/telecom/departments/support/restore",
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(audit(&e, "department.retire").await, 1);
    assert_eq!(audit(&e, "department.restore").await, 1);
}

/// The id of layer `key`.
async fn layer_id(e: &TestEnv, key: &str) -> String {
    sqlx::query_scalar("SELECT id::text FROM layer WHERE key = $1")
        .bind(key)
        .fetch_one(&e.owner)
        .await
        .unwrap_or_else(|x| panic!("layer {key}: {x}"))
}

/// One current row on `layer` (as super_admin, through `writeLayerRow`).
async fn add_row(e: &TestEnv, layer: &str, kind: &str, key: &str, fields: Value) {
    let id = layer_id(e, layer).await;
    let (s, b) = sa(
        e,
        "POST",
        &format!("/api/layers/{id}/rows"),
        json!({"kind": kind, "key": key, "op": "add", "fields": fields}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{layer} {kind} {key}: {b}");
}

/// Publish family `layer` (its next tag): the new version.
async fn publish(e: &TestEnv, layer: &str) -> i64 {
    let (s, b) = sa(
        e,
        "POST",
        &format!("/api/platform/templates/{}/tags", layer.replace('/', ".")),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "tag {layer}: {b}");
    b["version"]
        .as_i64()
        .unwrap_or_else(|| panic!("tag {layer}: {b}"))
}

#[tokio::test]
async fn a_job_inherits_base_industry_and_department_knowledge_with_its_layer() {
    let e = common::env().await;
    let o = offering(&e).await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;

    // another department of the same industry: its knowledge must not reach a customer-service job
    let (s, b) = sa(
        &e,
        "POST",
        "/api/platform/industries/retail/departments",
        json!({"department": "billing", "label": "Billing"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");

    add_row(
        &e,
        "base",
        "knowledge_pack",
        "*|*|base_pack",
        json!({"version": 1}),
    )
    .await;
    add_row(
        &e,
        o.industry,
        "knowledge_pack",
        "*|*|retail_pack",
        json!({"version": 2}),
    )
    .await;
    add_row(
        &e,
        o.department,
        "knowledge_pack",
        "*|*|care_pack",
        json!({"version": 3}),
    )
    .await;
    add_row(
        &e,
        "industry_dept/retail/billing",
        "knowledge_pack",
        "*|*|billing_pack",
        json!({"version": 1}),
    )
    .await;
    publish(&e, "base").await;
    publish(&e, o.industry).await;
    publish(&e, o.department).await;
    publish(&e, "industry_dept/retail/billing").await;
    // a new template tag stacks the newest base / industry / department tags
    add_row(
        &e,
        o.job_layer,
        "job_setting",
        "inherit_probe",
        json!({"label": "Probe"}),
    )
    .await;
    let v = publish(&e, o.job_layer).await;

    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/jobs",
            Some(&person("u-admin", &[])),
            json!({"template": format!("{}@v{v}", o.job_layer)}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let job = b["key"]
        .as_str()
        .or_else(|| b["job"]["key"].as_str())
        .unwrap_or_else(|| panic!("job key in {b}"))
        .to_owned();

    let (s, b) = send(
        &e.app,
        get(
            &format!("/api/jobs/{job}/knowledge-kinds"),
            Some(&person("u-admin", &[])),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut got: Vec<(String, String)> = b["inherited"]
        .as_array()
        .unwrap_or_else(|| panic!("inherited in {b}"))
        .iter()
        .map(|p| {
            (
                p["pin"].as_str().unwrap().to_owned(),
                p["layer"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            ("base_pack@1".to_owned(), "base".to_owned()),
            ("care_pack@3".to_owned(), DEPARTMENT_LAYER.to_owned()),
            ("retail_pack@2".to_owned(), "industry/retail".to_owned()),
        ],
        "{b}"
    );

    // the platform view lists each layer's packs
    let items = industries(&e).await;
    let retail = find(&items, INDUSTRY);
    assert_eq!(retail["knowledge"][0]["pack"], "retail_pack");
    assert_eq!(retail["knowledge"][0]["stage"], "*");
    let (s, b) = send(
        &e.app,
        get("/api/platform/industries", Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["base_knowledge"][0]["pack"], "base_pack");
}

/// The areas a department's target options offer (`release::compile::db_options`): the release target's area
/// choices come from the department layer's `areas` column.
async fn offered_areas(e: &TestEnv, department: &str) -> Vec<String> {
    let (s, b) = send(&e.app, get("/api/compile/options", Some(&super_admin()))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["departments"]
        .as_array()
        .unwrap_or_else(|| panic!("departments in {b}"))
        .iter()
        .find(|d| d["key"] == department)
        .unwrap_or_else(|| panic!("no department {department} in {b}"))["areas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["key"].as_str().unwrap().to_owned())
        .collect()
}

/// A department carries `areas` at creation (the platform journey does), but a department made without them — or
/// one whose choice changes later — can be given them here. They are the release target's area options, and a job
/// on a department with no area cannot cut a release (`bundle_incomplete` "Needs at least one area"), so this is
/// the only way to repair such a department: the layer cannot be re-created (`layer_exists`).
#[tokio::test]
async fn department_areas_can_be_set_after_it_exists() {
    let e = common::env().await;
    let o = offering(&e).await;
    let uri = format!("/api/platform/industries/{INDUSTRY}/departments/{DEPARTMENT}");

    // the fixture's department offers the one area it was created with
    assert_eq!(
        offered_areas(&e, o.department).await,
        ["order-support".to_owned()]
    );

    // add the area its context space serves
    let (s, b) = sa(
        &e,
        "PATCH",
        &uri,
        json!({"areas": ["order-support", "delivery"]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(audit(&e, "department.update").await, 1);
    assert_eq!(
        offered_areas(&e, o.department).await,
        ["order-support".to_owned(), "delivery".to_owned()]
    );

    // a label-only PATCH still renames and leaves the areas alone
    let (s, b) = sa(&e, "PATCH", &uri, json!({"label": "Customer care"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["label"], "Customer care");
    assert_eq!(audit(&e, "department.update").await, 2);
    assert_eq!(
        offered_areas(&e, o.department).await,
        ["order-support".to_owned(), "delivery".to_owned()]
    );

    // a body that names neither field says nothing to change
    let (s, b) = sa(&e, "PATCH", &uri, json!({})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "nothing_to_update");

    // an area is a selection key: blank, duplicated or over-long values are refused, and nothing is written
    for areas in [json!(["delivery", "  "]), json!(["delivery", "delivery"])] {
        let (s, b) = sa(&e, "PATCH", &uri, json!({ "areas": areas })).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert_eq!(code(&b), "invalid_areas");
    }
    assert_eq!(
        offered_areas(&e, o.department).await,
        ["order-support".to_owned(), "delivery".to_owned()],
        "a refused update writes nothing"
    );

    // only an existing department can be changed
    let (s, b) = sa(
        &e,
        "PATCH",
        &format!("/api/platform/industries/{INDUSTRY}/departments/nope"),
        json!({"areas": ["delivery"]}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "department_not_found");
}
