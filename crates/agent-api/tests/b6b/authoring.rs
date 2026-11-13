//! B6b T12b (F-21, on KH): raw layer and layer-row authoring — `POST /api/layers` (createLayer: family rules, region
//! sub-layers D111, thin departments D107) and `POST /api/layers/{id}/rows` (writeLayerRow: add / patch / tombstone
//! with `expected_rev`, the store's kind and region refusals passed through, one platform audit row per write).
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::{
    TestEnv, insert_tenant, insert_user, json as json_req, person, send, super_admin,
};

const LAYERS: &str = "/api/layers";

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

async fn sa(e: &TestEnv, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, json_req(method, uri, Some(&super_admin()), body)).await
}

async fn create(e: &TestEnv, body: Value) -> (StatusCode, Value) {
    sa(e, "POST", LAYERS, body).await
}

async fn created(e: &TestEnv, body: Value) -> Value {
    let (s, b) = create(e, body.clone()).await;
    assert_eq!(s, StatusCode::CREATED, "{body}: {b}");
    b
}

async fn write(e: &TestEnv, layer: &Value, body: Value) -> (StatusCode, Value) {
    let id = layer["id"].as_str().expect("layer id");
    sa(e, "POST", &format!("{LAYERS}/{id}/rows"), body).await
}

async fn layer_by_key(e: &TestEnv, key: &str) -> Value {
    let id: Uuid = sqlx::query_scalar("SELECT id FROM layer WHERE key = $1")
        .bind(key)
        .fetch_one(&e.owner)
        .await
        .unwrap_or_else(|err| panic!("layer {key}: {err}"));
    json!({"id": id.to_string(), "key": key})
}

async fn audit_rows(e: &TestEnv) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM platform_audit")
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn a_region(e: &TestEnv) -> String {
    sqlx::query_scalar(
        "SELECT key FROM dimension_value WHERE dimension = 'region' ORDER BY key LIMIT 1",
    )
    .fetch_one(&e.owner)
    .await
    .expect("the offering has a region vocabulary")
}

fn job(key: &str) -> Value {
    json!({"scope": "job", "job": key, "label": "B6b care"})
}

fn gizmo(label: &str) -> Value {
    json!({"label": label, "synonyms": ["widget"]})
}

#[tokio::test]
async fn authoring_needs_super_admin() {
    let e = crate::common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant a").await;
    insert_user(&e.owner, "u-ta", t, "tenant_admin").await;
    let ta = person("u-ta", &[]);
    let base = layer_by_key(&e, "base").await;
    for uri in [
        LAYERS.to_string(),
        format!("{LAYERS}/{}/rows", base["id"].as_str().unwrap()),
    ] {
        let (s, b) = send(&e.app, json_req("POST", &uri, Some(&ta), json!({}))).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{uri}: {b}");
        assert_eq!(code(&b), "super_admin_required", "{uri}");
    }
}

#[tokio::test]
async fn create_layer_family_rules() {
    let e = crate::common::env().await;
    let head = created(&e, job("b6b_care")).await;
    assert_eq!(head["key"], "job/b6b_care");
    assert_eq!(head["family"], "job/b6b_care");
    assert_eq!(head["tag"], Value::Null, "a new template layer has no tag");
    let email = created(
        &e,
        json!({"scope": "job", "job": "b6b_care", "dimension": ["channel", "email"], "label": "Email"}),
    )
    .await;
    assert_eq!(email["key"], "job/b6b_care/channel=email");
    assert_eq!(email["family"], "job/b6b_care");
    let dept = created(
        &e,
        json!({"scope": "industry_dept", "industry": "retail", "department": "b6b_returns", "label": "Returns"}),
    )
    .await;
    assert_eq!(dept["family"], "industry_dept/retail/b6b_returns");

    // the parent must exist; a profile layer is createAxisRule's; unknown scope and stray keys are key errors
    let (s, b) = create(
        &e,
        json!({"scope": "job", "job": "b6b_nobody", "dimension": ["channel", "email"], "label": "x"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "parent_layer_not_found");
    assert_eq!(b["error"]["parent"], "job/b6b_nobody");
    let (s, b) = create(
        &e,
        json!({"scope": "industry_dept", "industry": "b6b_nowhere", "department": "x", "label": "x"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "parent_layer_not_found");
    for bad in [
        json!({"scope": "profile", "label": "x"}),
        json!({"scope": "tenant", "label": "x"}),
        json!({"scope": "industry", "label": "x"}),
        json!({"scope": "base", "job": "x", "label": "x"}),
    ] {
        let (s, b) = create(&e, bad.clone()).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {b}");
        assert_eq!(code(&b), "invalid_layer_key", "{bad}");
    }
    let (s, b) = create(
        &e,
        json!({"scope": "job", "job": "b6b_blank", "label": "  "}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "invalid_label");
}

#[tokio::test]
async fn create_layer_exists_409() {
    let e = crate::common::env().await;
    let (s, b) = create(
        &e,
        json!({"scope": "industry", "industry": "retail", "label": "Retail"}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "layer_exists");
    created(&e, job("b6b_twice")).await;
    let (s, b) = create(&e, job("b6b_twice")).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "layer_exists");
}

#[tokio::test]
async fn thin_department_with_extends_is_created() {
    let e = crate::common::env().await;
    let archetype = "b6b_archetype".to_owned();
    crate::b6b::support::row(
        &e,
        "base",
        "department_archetype",
        &archetype,
        json!({"label": "B6b archetype"}),
    )
    .await;
    let thin = created(
        &e,
        json!({"scope": "industry_dept", "industry": "retail", "department": "b6b_thin",
               "label": "Thin", "extends": archetype}),
    )
    .await;
    assert_eq!(thin["extends"], json!(archetype));
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM layer_row r JOIN layer l ON l.id = r.layer_id WHERE l.key = $1",
    )
    .bind("industry_dept/retail/b6b_thin")
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(rows, 0, "a thin department has no rows");
    let (s, b) = create(
        &e,
        json!({"scope": "industry_dept", "industry": "retail", "department": "b6b_ghost",
               "label": "Ghost", "extends": "b6b_no_such_archetype"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "parent_layer_not_found");
    assert_eq!(b["error"]["archetype"], "b6b_no_such_archetype");
}

#[tokio::test]
async fn add_patch_tombstone_cycle_writes_three_revisions() {
    let e = crate::common::env().await;
    let l = created(&e, job("b6b_cycle")).await;
    let (s, b) = write(
        &e,
        &l,
        json!({"kind": "entity", "key": "Gizmo", "op": "add", "fields": gizmo("Gizmo")}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(
        (b["rev"].clone(), b["op"].clone()),
        (json!(1), json!("add"))
    );
    let (s, b) = write(
        &e,
        &l,
        json!({"kind": "entity", "key": "Gizmo", "op": "add", "fields": gizmo("Again")}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "duplicate_add");
    let (s, b) = write(
        &e,
        &l,
        json!({"kind": "entity", "key": "Gizmo", "op": "patch", "fields": {"label": "Gizmo 2"}, "expected_rev": 1}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["rev"], 2);
    let (s, b) = write(
        &e,
        &l,
        json!({"kind": "entity", "key": "Gizmo", "op": "tombstone", "expected_rev": 2}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(
        (b["rev"].clone(), b["op"].clone()),
        (json!(3), json!("tombstone"))
    );
    let (s, b) = write(
        &e,
        &l,
        json!({"kind": "entity", "key": "Gizmo", "op": "add", "fields": gizmo("Back")}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "add_tombstoned");
    let revs: Vec<(i32, String, bool)> = sqlx::query_as(
        "SELECT r.rev, r.op, r.is_current FROM layer_row r JOIN layer l ON l.id = r.layer_id \
          WHERE l.key = 'job/b6b_cycle' AND r.kind = 'entity' AND r.key = 'Gizmo' ORDER BY r.rev",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        revs,
        vec![
            (1, "add".to_string(), false),
            (2, "patch".to_string(), false),
            (3, "tombstone".to_string(), true),
        ]
    );
    // expected_rev is the contract: refused on an add, required on a patch
    let (s, b) = write(
        &e,
        &l,
        json!({"kind": "entity", "key": "Other", "op": "add", "fields": gizmo("Other"), "expected_rev": 1}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "invalid_row");
    let (s, b) = write(
        &e,
        &l,
        json!({"kind": "entity", "key": "Other", "op": "patch", "fields": {}}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "invalid_row");
}

#[tokio::test]
async fn stale_revision_409() {
    let e = crate::common::env().await;
    let l = created(&e, job("b6b_stale")).await;
    let add = json!({"kind": "entity", "key": "Gizmo", "op": "add", "fields": gizmo("Gizmo")});
    assert_eq!(write(&e, &l, add).await.0, StatusCode::CREATED);
    let patch = |rev: i32, label: &str| json!({"kind": "entity", "key": "Gizmo", "op": "patch", "fields": {"label": label}, "expected_rev": rev});
    assert_eq!(write(&e, &l, patch(1, "A")).await.0, StatusCode::CREATED);
    let (s, b) = write(&e, &l, patch(1, "B")).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "stale_revision");
    assert_eq!(b["error"]["current_rev"], 2);
    let (s, b) = write(
        &e,
        &l,
        json!({"kind": "entity", "key": "Nobody", "op": "patch", "fields": {"label": "x"}, "expected_rev": 1}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "row_not_found");
}

#[tokio::test]
async fn kind_not_allowed_in_scope_422() {
    let e = crate::common::env().await;
    let l = created(&e, job("b6b_kinds")).await;
    let (s, b) = write(
        &e,
        &l,
        json!({"kind": "error_state", "key": "b6b_state", "op": "add", "fields": {}}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "kind_not_allowed_in_layer");
    let (s, b) = write(
        &e,
        &l,
        json!({"kind": "nope", "key": "x", "op": "add", "fields": {}}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "invalid_row");
    let (s, b) = sa(
        &e,
        "POST",
        &format!("{LAYERS}/{}/rows", Uuid::new_v4()),
        json!({"kind": "entity", "key": "Gizmo", "op": "add", "fields": gizmo("Gizmo")}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "layer_not_found");
}

#[tokio::test]
async fn write_instruction_row() {
    let e = crate::common::env().await;
    let l = created(&e, job("b6b_voice")).await;
    // every slot at platform scope (tenant refinement of tone / escalation / format only is KH/B2's rule)
    for slot in [
        "identity",
        "policy",
        "tone",
        "safety",
        "escalation",
        "format",
    ] {
        let (s, b) = write(
            &e,
            &l,
            json!({"kind": "instruction", "key": slot, "op": "add", "fields": {"text": format!("B6b {slot}.")}}),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{slot}: {b}");
        assert_eq!(b["kind"], "instruction");
    }
    let (s, b) = write(
        &e,
        &l,
        json!({"kind": "instruction", "key": "b6b_not_a_slot", "op": "add", "fields": {"text": "x"}}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "invalid_key");
}

#[tokio::test]
async fn region_sub_layer_create_and_write() {
    let e = crate::common::env().await;
    let r = a_region(&e).await;
    let sub = created(
        &e,
        json!({"scope": "industry", "industry": "retail", "region": r, "label": "Retail region"}),
    )
    .await;
    assert_eq!(sub["key"], format!("industry/retail/region/{r}"));
    assert_eq!(
        sub["family"], "industry/retail",
        "a region sub-layer joins its parent's family (D111)"
    );
    let region: Option<String> = sqlx::query_scalar("SELECT region FROM layer WHERE key = $1")
        .bind(format!("industry/retail/region/{r}"))
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(region.as_deref(), Some(r.as_str()));
    let (s, b) = write(
        &e,
        &sub,
        json!({"kind": "instruction", "key": "tone", "op": "add", "fields": {"text": "Regional tone."}}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let untagged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM layer_untagged WHERE family = 'industry/retail' AND layer_key = $1",
    )
    .bind(format!("industry/retail/region/{r}"))
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        untagged, 1,
        "the region row is an untagged draft of industry/retail"
    );

    for bad in [
        json!({"scope": "industry", "industry": "retail", "region": "b6b-no-such-region", "label": "x"}),
        json!({"scope": "job", "job": "customer_care_agent", "region": r, "label": "x"}),
        json!({"scope": "industry_dept", "industry": "retail", "department": "customer_service", "region": r,
               "label": "x", "areas": ["returns"]}),
    ] {
        let (s, b) = create(&e, bad.clone()).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {b}");
        assert_eq!(code(&b), "invalid_layer_key", "{bad}");
    }
    let (s, b) = create(
        &e,
        json!({"scope": "industry", "industry": "b6b_nowhere", "region": r, "label": "x"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "parent_layer_not_found");
}

#[tokio::test]
async fn region_sub_layer_refuses_platform_kinds_422() {
    let e = crate::common::env().await;
    let r = a_region(&e).await;
    let sub = created(
        &e,
        json!({"scope": "base", "region": r, "label": "Base region"}),
    )
    .await;
    assert_eq!(sub["family"], "base");
    for kind in ["axis_rule", "profile", "department_archetype"] {
        let (s, b) = write(
            &e,
            &sub,
            json!({"kind": kind, "key": "b6b_x", "op": "add", "fields": {}}),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{kind}: {b}");
        assert_eq!(code(&b), "kind_not_allowed_in_layer", "{kind}");
    }
}

#[tokio::test]
async fn every_authoring_write_one_platform_audit_row() {
    let e = crate::common::env().await;
    let n0 = audit_rows(&e).await;
    let l = created(&e, job("b6b_audit")).await;
    assert_eq!(audit_rows(&e).await, n0 + 1);
    let action: String =
        sqlx::query_scalar("SELECT action FROM platform_audit ORDER BY id DESC LIMIT 1")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(action, "layer.create");
    let (s, b) = write(
        &e,
        &l,
        json!({"kind": "entity", "key": "Gizmo", "op": "add", "fields": gizmo("Gizmo")}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(audit_rows(&e).await, n0 + 2);
    let action: String =
        sqlx::query_scalar("SELECT action FROM platform_audit ORDER BY id DESC LIMIT 1")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(action, "layer.revision");
    // a refused write leaves no audit row
    let (s, _) = write(
        &e,
        &l,
        json!({"kind": "entity", "key": "Gizmo", "op": "add", "fields": gizmo("Gizmo")}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(audit_rows(&e).await, n0 + 2);
}
