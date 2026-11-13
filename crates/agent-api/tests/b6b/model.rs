//! B6b T11 / T12c / T12f: the object model, operations and error-state routes over authored rows —
//! super_admin only; catalogue, map, detail, context (B7 connector templates, R9); field / state / transition /
//! relation edits and field retirement as untagged revisions of the catalogue row (one `layer.revision` audit row
//! each); operation editing (T12c). Comparisons sort in Rust, never by SQL collation.
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::platform::{Op, draft_template, publish_template};
use crate::common::{
    TestEnv, get, insert_tenant, insert_user, json as json_req, person, send, super_admin,
};

const OBJ: &str = "/api/platform/objects";
const OPS: &str = "/api/platform/operations";

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

async fn sa_get(e: &TestEnv, uri: &str) -> (StatusCode, Value) {
    send(&e.app, get(uri, Some(&super_admin()))).await
}

async fn ok(e: &TestEnv, uri: &str) -> Value {
    let (s, b) = sa_get(e, uri).await;
    assert_eq!(s, StatusCode::OK, "{uri}: {b}");
    b
}

async fn sa_send(e: &TestEnv, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, json_req(method, uri, Some(&super_admin()), body)).await
}

async fn revisions(e: &TestEnv) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM platform_audit WHERE action = 'layer.revision'")
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

/// (layer key, rev, op) of the current `kind`/`key` rows.
async fn current(e: &TestEnv, kind: &str, key: &str) -> Vec<(String, i32, String)> {
    let mut rows: Vec<(String, i32, String)> = sqlx::query_as(
        "SELECT l.key, r.rev, r.op FROM layer_row r JOIN layer l ON l.id = r.layer_id \
          WHERE r.kind = $1 AND r.key = $2 AND r.is_current",
    )
    .bind(kind)
    .bind(key)
    .fetch_all(&e.owner)
    .await
    .unwrap();
    rows.sort();
    rows
}

fn find<'a>(list: &'a Value, k: &str, v: &str) -> Option<&'a Value> {
    list.as_array()?.iter().find(|x| x[k] == v)
}

fn op_edit() -> Value {
    json!({
        "identity_level": "2", "side_effect": "X", "confirm_back": true, "verify_read": "order.read",
        "idempotency": "key+dup", "input_fields": ["order_id", "reason"],
        "output_fields": ["status", "refund_expected"], "description": "Cancel an order.", "extra_errors": []
    })
}

#[tokio::test]
async fn object_routes_are_super_admin_only() {
    let e = crate::common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant a").await;
    insert_user(&e.owner, "u-ta", t, "tenant_admin").await;
    let ta = person("u-ta", &[]);
    for (method, uri) in [
        ("GET", OBJ.to_string()),
        ("GET", format!("{OBJ}/map")),
        ("GET", format!("{OBJ}/order")),
        ("GET", format!("{OBJ}/order/context")),
        ("PUT", format!("{OBJ}/order/fields/x")),
        ("POST", format!("{OBJ}/order/fields/x/retire")),
        ("PUT", format!("{OBJ}/order/states/x")),
        (
            "PUT",
            format!("{OBJ}/order/transitions/order.cancel/processing"),
        ),
        (
            "DELETE",
            format!("{OBJ}/order/transitions/order.cancel/processing"),
        ),
        ("PUT", format!("{OBJ}/order/relations/paid%20by/payment")),
        ("DELETE", format!("{OBJ}/order/relations/paid%20by/payment")),
        ("GET", OPS.to_string()),
        ("PUT", format!("{OPS}/order/cancel")),
        ("GET", "/api/platform/error-states".to_string()),
    ] {
        let (s, b) = send(&e.app, json_req(method, &uri, Some(&ta), json!({}))).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{method} {uri}: {b}");
        assert_eq!(code(&b), "super_admin_required", "{method} {uri}");
    }
}

#[tokio::test]
async fn catalogue_map_and_detail_read_the_head() {
    let e = crate::common::env().await;
    crate::b6b::support::object_model(&e).await;
    let cat = ok(&e, OBJ).await;
    let order = find(&cat["objects"], "object", "order").expect("order");
    assert_eq!(order["field_count"], json!(4));
    assert!(order["operation_count"].as_u64().unwrap() >= 3, "{order}");
    assert!(
        find(&cat["objects"], "object", "location").is_some(),
        "industry objects too"
    );
    assert!(cat["folded"].is_array());

    let map = ok(&e, &format!("{OBJ}/map")).await;
    let case = find(&map["nodes"], "object", "case").expect("case node");
    let layers: Vec<&str> = case["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["layer_key"].as_str().unwrap())
        .collect();
    assert_eq!(layers[0], "base", "broadest first: {layers:?}");
    // the IT desk's refinement of case lives on its base department, not on a job
    assert!(layers.contains(&"base_dept/it_service_desk"), "{layers:?}");
    assert!(!map["relations"].as_array().unwrap().is_empty());

    let d = ok(&e, &format!("{OBJ}/order")).await;
    assert_eq!(d["layer_key"], "base");
    let cancel = find(&d["operations"], "key", "order.cancel").expect("order.cancel");
    assert!(
        cancel["error_states"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c.as_str().unwrap().starts_with("invalid_state:")),
        "{cancel}"
    );
    assert!(find(&d["verbs"], "verb", "order.cancel").is_some());
    assert_eq!(d["pending_edits"], json!([]));

    let (s, b) = sa_get(&e, &format!("{OBJ}/nope")).await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "object_not_found"));
}

#[tokio::test]
async fn object_field_edit_is_untagged_revision_and_pending_edit() {
    let e = crate::common::env().await;
    crate::b6b::support::object_model(&e).await;
    let before = current(&e, "object", "order").await;
    let audits = revisions(&e).await;
    let (s, b) = sa_send(
        &e,
        "PUT",
        &format!("{OBJ}/order/fields/channel"),
        json!({"field": "channel", "data_type": "enum", "pii": false, "enum_values": [" web ", "store"],
               "ref": null, "required": false, "description": "where it was placed"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["enum_values"], json!(["web", "store"]));
    assert_eq!(revisions(&e).await, audits + 1, "one audit row");
    let after = current(&e, "object", "order").await;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].0, "base");
    assert_eq!(after[0].1, before[0].1 + 1);
    assert_eq!(after[0].2, before[0].2, "op kept");
    let untagged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM layer_untagged WHERE kind = 'object' AND key = 'order'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(untagged, 1);
    let d = ok(&e, &format!("{OBJ}/order")).await;
    assert!(find(&d["fields"], "field", "channel").is_some());
    let edits = d["pending_edits"].as_array().unwrap();
    assert_eq!(edits.len(), 1, "{edits:?}");
    assert_eq!(edits[0]["what"], "field channel added");
    assert_eq!(edits[0]["by"], "u-super");

    let (s, b) = sa_send(
        &e,
        "PUT",
        &format!("{OBJ}/order/fields/kind"),
        json!({"data_type": "enum", "enum_values": []}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "enum_values_required")
    );
    let (s, b) = sa_send(
        &e,
        "PUT",
        &format!("{OBJ}/order/fields/Bad"),
        json!({"data_type": "text"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_field")
    );
    let (s, b) = sa_send(
        &e,
        "PUT",
        &format!("{OBJ}/nope/fields/x"),
        json!({"data_type": "text"}),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "object_not_found"));
    assert_eq!(revisions(&e).await, audits + 1, "refusals write nothing");
}

#[tokio::test]
async fn states_and_transitions() {
    let e = crate::common::env().await;
    crate::b6b::support::object_model(&e).await;
    let (s, b) = sa_send(
        &e,
        "PUT",
        &format!("{OBJ}/order/states/on_hold"),
        json!({"status": "on_hold", "label": "On hold", "terminal": false, "canonical": false}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["initial"], json!(false));
    let (s, b) = sa_send(
        &e,
        "PUT",
        &format!("{OBJ}/order/states/On%20hold"),
        json!({}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_state_name")
    );

    let uri = format!("{OBJ}/order/transitions/order.hold/processing");
    let (s, b) = sa_send(
        &e,
        "PUT",
        &uri,
        json!({"to_status": "on_hold", "guard_rule": null}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["verb"], "order.hold");
    let d = ok(&e, &format!("{OBJ}/order")).await;
    assert!(find(&d["verbs"], "verb", "order.hold").is_some());
    let (s, b) = sa_send(&e, "DELETE", &uri, json!({})).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    let (s, b) = sa_send(&e, "DELETE", &uri, json!({})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "transition_not_found")
    );
}

#[tokio::test]
async fn transition_unknown_state_422() {
    let e = crate::common::env().await;
    crate::b6b::support::object_model(&e).await;
    let (s, b) = sa_send(
        &e,
        "PUT",
        &format!("{OBJ}/order/transitions/order.lose/processing"),
        json!({"to_status": "lost"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "unknown_state")
    );
    let (s, b) = sa_send(
        &e,
        "PUT",
        &format!("{OBJ}/order/transitions/case.close/processing"),
        json!({"to_status": "cancelled"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_verb")
    );
}

#[tokio::test]
async fn relation_add_remove_revisions() {
    let e = crate::common::env().await;
    crate::b6b::support::object_model(&e).await;
    let audits = revisions(&e).await;
    let uri = format!("{OBJ}/order/relations/settled%20by/payment");
    let (s, b) = sa_send(&e, "PUT", &uri, json!({"cardinality": "1:n"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b,
        json!({"subject": "order", "predicate": "settled by", "object": "payment", "cardinality": "1:n"})
    );
    let (s, b) = sa_send(&e, "PUT", &uri, json!({"cardinality": "n:n"})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_cardinality")
    );
    let (s, b) = sa_send(&e, "DELETE", &uri, json!({})).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    assert_eq!(revisions(&e).await, audits + 2);
    let (s, b) = sa_send(&e, "DELETE", &uri, json!({})).await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "relation_not_found"));
    let (s, b) = sa_send(
        &e,
        "PUT",
        &format!("{OBJ}/order/relations/Bad-Pred/payment"),
        json!({"cardinality": "1:n"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_predicate")
    );
}

#[tokio::test]
async fn relation_unknown_target_404() {
    let e = crate::common::env().await;
    crate::b6b::support::object_model(&e).await;
    let (s, b) = sa_send(
        &e,
        "PUT",
        &format!("{OBJ}/order/relations/has/unicorn"),
        json!({"cardinality": "1:n"}),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "target_not_found"));
}

#[tokio::test]
async fn retire_field_in_use_409_names_ops() {
    let e = crate::common::env().await;
    crate::b6b::support::object_model(&e).await;
    let (s, b) = sa_send(
        &e,
        "POST",
        &format!("{OBJ}/order/fields/order_id/retire"),
        json!({}),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "field_in_use"));
    let ops: Vec<&str> = b["error"]["operations"]
        .as_array()
        .expect("operations[]")
        .iter()
        .map(|o| o.as_str().unwrap())
        .collect();
    assert!(
        ops.contains(&"order.read") && ops.contains(&"order.cancel"),
        "{ops:?}"
    );
    let (s, b) = sa_send(
        &e,
        "POST",
        &format!("{OBJ}/order/fields/nope/retire"),
        json!({}),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "field_not_found"));
}

#[tokio::test]
async fn retired_field_hidden_from_head_io() {
    let e = crate::common::env().await;
    crate::b6b::support::object_model(&e).await;
    let (s, b) = sa_send(
        &e,
        "POST",
        &format!("{OBJ}/order/fields/gift_note/retire"),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["retired"], json!(true));
    let (s, b) = sa_send(
        &e,
        "POST",
        &format!("{OBJ}/order/fields/gift_note/retire"),
        json!({}),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "field_retired"));
    let mut edit = op_edit();
    edit["input_fields"] = json!(["order_id", "gift_note"]);
    let (s, b) = sa_send(&e, "PUT", &format!("{OPS}/order/cancel"), edit).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "field_not_in_contract")
    );
    assert_eq!(b["error"]["field"], "gift_note");

    // A custom operation has no contract: its I/O must still use live object fields.
    let custom = json!({
        "object": "order", "verb": "peek", "modality": "observe", "op_kind": "standard",
        "identity_level": "2", "side_effect": "R", "confirm_back": false,
        "idempotency": "nat", "input_fields": ["order_id"], "output_fields": ["status"],
        "description": "Read the order status."
    });
    let (s, b) = sa_send(&e, "POST", OPS, custom.clone()).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let mut edit = custom;
    for key in ["object", "verb", "modality", "op_kind"] {
        edit.as_object_mut().unwrap().remove(key);
    }
    edit["input_fields"] = json!(["order_id", "gift_note"]);
    let (s, b) = sa_send(&e, "PUT", &format!("{OPS}/order/peek"), edit).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "unknown_field")
    );
    assert_eq!(b["error"]["fields"], json!(["gift_note"]));
}

#[tokio::test]
async fn operations_filter_by_side_effect() {
    let e = crate::common::env().await;
    crate::b6b::support::object_model(&e).await;
    let all = ok(&e, OPS).await;
    let n_all = all["items"].as_array().unwrap().len();
    assert!(n_all >= 6, "{n_all}");
    assert_eq!(all["next"], Value::Null);
    let p = ok(&e, &format!("{OPS}?side_effect=P")).await;
    let keys: Vec<&str> = p["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert!(keys.contains(&"order.place"), "{keys:?}");
    assert!(
        p["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["side_effect"] == "P")
    );
    // `q` matches the key or the description, so other order ops whose description mentions "cancel" are in too
    let order = ok(&e, &format!("{OPS}?object=order&q=cancel")).await;
    let items = order["items"].as_array().unwrap();
    assert!(items.iter().any(|r| r["key"] == "order.cancel"), "{order}");
    assert!(items.iter().all(|r| r["object"] == "order"), "{order}");
    assert!(
        items.iter().all(|r| {
            let hay = format!("{} {}", r["key"], r["description"]).to_lowercase();
            hay.contains("cancel")
        }),
        "{order}"
    );
    let builtin = ok(&e, &format!("{OPS}?builtin=true")).await;
    assert!(
        builtin["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["builtin"] == true)
    );
    assert!(all["items"][0]["pending"].is_boolean());
}

#[tokio::test]
async fn error_states_include_d57_codes() {
    let e = crate::common::env().await;
    crate::b6b::support::object_model(&e).await;
    let list = ok(&e, "/api/platform/error-states").await;
    for code in [
        "not_found",
        "invalid_state",
        "identity_required",
        "unknown_outcome",
        "outside_hours",
    ] {
        let row = find(&list, "code", code).unwrap_or_else(|| panic!("{code}"));
        assert!(row["description"].as_str().is_some_and(|d| !d.is_empty()));
        assert!(row.get("user_facing").is_some() && row.get("escalate").is_some());
    }
}

#[tokio::test]
async fn object_context_systems_from_b7() {
    let e = crate::common::env().await;
    crate::b6b::support::object_model(&e).await;
    // the platform templates the object's systems come from (nothing is seeded): the newest PUBLISHED version of
    // each key is listed, drafts are not
    let order_ops = || vec![Op::star("order", "read"), Op::star("order", "cancel")];
    for version in ["3", "9"] {
        publish_template(&e, "test_shop", version, &order_ops())
            .await
            .unwrap();
    }
    publish_template(&e, "test_shop_b", "1", &order_ops())
        .await
        .unwrap();
    publish_template(&e, "test_shop_b", "2", &order_ops())
        .await
        .unwrap();
    draft_template(&e, "test_shop_c", "0", &order_ops())
        .await
        .unwrap();
    let c = ok(&e, &format!("{OBJ}/order/context")).await;
    assert_eq!(c["object"], "order");
    assert!(c["activities"].is_array(), "{c}");
    let systems = c["systems"].as_array().unwrap();
    let shop = find(&c["systems"], "template_key", "test_shop").expect("test_shop");
    assert_eq!(shop["version"], "9", "newest published version");
    assert!(
        shop["ops"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o == "order.read")
    );
    assert!(shop["status_map"].as_array().unwrap().iter().all(|m| {
        !m["canonical"]
            .as_str()
            .unwrap()
            .starts_with("invalid_state")
    }));
    let other = find(&c["systems"], "template_key", "test_shop_b").expect("test_shop_b");
    assert_eq!(other["version"], "2");
    assert!(
        find(&c["systems"], "template_key", "test_shop_c").is_none(),
        "drafts are not published"
    );
    assert!(systems.iter().all(|s| s["instances"].is_array()));
    assert!(c["layers"].is_array(), "{c}");
    let (s, b) = sa_get(&e, &format!("{OBJ}/nope/context")).await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "object_not_found"));
}

#[tokio::test]
async fn operation_edit_is_untagged_revision() {
    let e = crate::common::env().await;
    crate::b6b::support::object_model(&e).await;
    let before = current(&e, "operation", "order.cancel").await;
    let audits = revisions(&e).await;
    let mut edit = op_edit();
    edit["confirm_back"] = json!(false);
    edit["idempotency"] = json!("key");
    let (s, b) = sa_send(&e, "PUT", &format!("{OPS}/order/cancel"), edit).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["key"], "order.cancel");
    assert_eq!(b["pending"], json!(true));
    assert_eq!(b["layer_key"], "base");
    // confirm_back_and_idempotency_round_trip
    assert_eq!(b["confirm_back"], json!(false));
    assert_eq!(b["idempotency"], "key");
    assert_eq!(revisions(&e).await, audits + 1);
    let after = current(&e, "operation", "order.cancel").await;
    assert_eq!(after[0].1, before[0].1 + 1);
    let listed = ok(&e, &format!("{OPS}?object=order")).await;
    let row = find(&listed["items"], "key", "order.cancel").unwrap();
    assert_eq!(row["pending"], json!(true));
    assert_eq!(row["idempotency"], "key");
}

#[tokio::test]
async fn operation_edit_refusals() {
    let e = crate::common::env().await;
    crate::b6b::support::object_model(&e).await;
    let (s, b) = sa_send(&e, "PUT", &format!("{OPS}/order/teleport"), op_edit()).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "operation_not_found")
    );
    // builtin_operation_409
    let (s, b) = sa_send(&e, "PUT", &format!("{OPS}/rule/evaluate"), op_edit()).await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "builtin_operation"));
    // A contract operation rejects fields outside its declared output, even on an existing object.
    let mut edit = op_edit();
    edit["output_fields"] = json!(["status", "unicorn"]);
    let (s, b) = sa_send(&e, "PUT", &format!("{OPS}/order/cancel"), edit).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "field_not_in_contract")
    );
    assert_eq!(b["error"]["field"], "unicorn");
    assert_eq!(b["error"]["declared"], json!(["status", "refund_expected"]));
    let mut edit = op_edit();
    edit["verify_read"] = json!("order.cancel");
    let (s, b) = sa_send(&e, "PUT", &format!("{OPS}/order/cancel"), edit).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "unknown_verify_read")
    );
    // raises_identity_level is a verify op's column only: order.cancel is not one
    let mut edit = op_edit();
    edit["raises_identity_level"] = json!("2");
    let (s, b) = sa_send(&e, "PUT", &format!("{OPS}/order/cancel"), edit).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_operation")
    );
    let mut edit = op_edit();
    edit["identity_level"] = json!("9");
    let (s, b) = sa_send(&e, "PUT", &format!("{OPS}/order/cancel"), edit).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_identity_level")
    );
}

#[tokio::test]
async fn variant_edit_keeps_inheriting() {
    let e = crate::common::env().await;
    crate::b6b::support::object_model(&e).await;
    let ops = ok(&e, &format!("{OPS}?object=case")).await;
    let v = find(&ops["items"], "key", "case.create{return}")
        .expect("variant")
        .clone();
    let edit = json!({
        "identity_level": v["identity_level"], "side_effect": v["side_effect"], "confirm_back": v["confirm_back"],
        "verify_read": v["verify_read"], "idempotency": v["idempotency"], "input_fields": v["input_fields"],
        "output_fields": v["output_fields"], "description": "Open a return case (edited).", "extra_errors": []
    });
    let (s, b) = sa_send(
        &e,
        "PUT",
        &format!("{OPS}/case/create?variant=return"),
        edit,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["description"], "Open a return case (edited).");
    let inherited: Vec<&str> = b["inherited"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    let before: Vec<&str> = v["inherited"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    for c in &before {
        assert!(inherited.contains(c), "{c} still inherited: {inherited:?}");
    }
}
