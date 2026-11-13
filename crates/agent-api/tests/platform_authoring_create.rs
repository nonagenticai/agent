//! E2 + E3: a fresh platform is configured through the authoring routes alone. `POST /api/platform/objects` and
//! `POST /api/platform/operations` create catalogue rows (SuperAdmin; 409 on a duplicate key, 422 on an invalid
//! body), and the platform tenant named by `AGENT_PLATFORM_TENANT_ID` exists after boot without any content.
use crate::common;

use agent_api::platform::bootstrap::ensure_platform_tenant;
use axum::http::StatusCode;
use common::{TestEnv, env_bare, env_bare_with, get, json, person, send, super_admin};
use serde_json::{Value, json};
use uuid::Uuid;

/// A fresh environment with the base layer (the target of a create without `layer_key`).
async fn fresh() -> TestEnv {
    let e = env_bare().await;
    let sa = super_admin();
    let (st, body) = send(
        &e.app,
        json(
            "POST",
            "/api/layers",
            Some(&sa),
            json!({"scope": "base", "label": "Base"}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    e
}

fn widget() -> Value {
    json!({"object": "widget", "label": "Widget", "tier": "base", "description": "A thing."})
}

fn read_op() -> Value {
    json!({
        "object": "widget", "verb": "read", "modality": "observe", "op_kind": "standard",
        "identity_level": "1", "side_effect": "R", "confirm_back": false, "idempotency": "nat",
        "description": "Read a widget."
    })
}

fn code(body: &Value) -> &str {
    body["error"]["code"]
        .as_str()
        .or_else(|| body["code"].as_str())
        .unwrap_or("")
}

#[tokio::test]
async fn create_object_then_read_it() {
    let e = fresh().await;
    let sa = super_admin();
    let (st, body) = send(
        &e.app,
        json("POST", "/api/platform/objects", Some(&sa), widget()),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["object"], "widget");
    assert_eq!(body["layer_key"], "base");
    assert_eq!(body["rev"], 1);

    let (st, body) = send(&e.app, get("/api/platform/objects/widget", Some(&sa))).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["label"], "Widget");
    let (st, body) = send(&e.app, get("/api/platform/objects", Some(&sa))).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body["objects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["object"] == "widget")
    );
}

#[tokio::test]
async fn duplicate_object_is_409_and_bad_bodies_are_refused() {
    let e = fresh().await;
    let sa = super_admin();
    let post = |b: Value| json("POST", "/api/platform/objects", Some(&sa), b);
    assert_eq!(send(&e.app, post(widget())).await.0, StatusCode::CREATED);
    let (st, body) = send(&e.app, post(widget())).await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "object_exists");

    let mut bad_key = widget();
    bad_key["object"] = json!("Not A Key");
    let (st, body) = send(&e.app, post(bad_key)).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(code(&body), "invalid_object_key");

    let mut blank = widget();
    blank["object"] = json!("gadget");
    blank["label"] = json!("  ");
    let (st, body) = send(&e.app, post(blank)).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(code(&body), "invalid_row");

    let mut nolayer = widget();
    nolayer["object"] = json!("gizmo");
    nolayer["layer_key"] = json!("industry/nowhere");
    let (st, body) = send(&e.app, post(nolayer)).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn only_super_admin_creates() {
    let e = fresh().await;
    let plain = person("u-plain", &["tenant_admin"]);
    let (st, _) = send(
        &e.app,
        json("POST", "/api/platform/objects", Some(&plain), widget()),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = send(
        &e.app,
        json("POST", "/api/platform/operations", Some(&plain), read_op()),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = send(
        &e.app,
        json("POST", "/api/platform/objects", None, widget()),
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn create_operation_then_read_it() {
    let e = fresh().await;
    let sa = super_admin();
    let post = |b: Value| json("POST", "/api/platform/operations", Some(&sa), b);
    // an operation of an unknown object is refused
    let (st, body) = send(&e.app, post(read_op())).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(code(&body), "unknown_object");

    let (st, _) = send(
        &e.app,
        json("POST", "/api/platform/objects", Some(&sa), widget()),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, body) = send(&e.app, post(read_op())).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["key"], "widget.read");
    assert_eq!(body["modality"], "observe");
    assert_eq!(body["side_effect"], "R");
    assert_eq!(body["pending"], true);

    let (st, body) = send(
        &e.app,
        get("/api/platform/operations?object=widget", Some(&sa)),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body["items"]
            .as_array()
            .is_some_and(|i| i.iter().any(|o| o["key"] == "widget.read")),
        "{body}"
    );

    // duplicate key, a variant without its * row, a built-in, a * row without modality
    let (st, body) = send(&e.app, post(read_op())).await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "operation_exists");
    let mut variant = read_op();
    variant["verb"] = json!("list");
    variant["variant"] = json!("open");
    let (st, body) = send(&e.app, post(variant)).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(code(&body), "star_operation_required");
    let mut builtin = read_op();
    builtin["verb"] = json!("lookup");
    builtin["op_kind"] = json!("builtin");
    assert_eq!(
        send(&e.app, post(builtin)).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let mut bare = read_op();
    bare["verb"] = json!("find");
    bare.as_object_mut().unwrap().remove("modality");
    assert_eq!(
        send(&e.app, post(bare)).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let mut bad_level = read_op();
    bad_level["verb"] = json!("peek");
    bad_level["identity_level"] = json!("9");
    let (st, body) = send(&e.app, post(bad_level)).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(code(&body), "invalid_identity_level");
    // no level at all: 422 with a field error, never a default level (and nothing was created)
    let mut no_level = read_op();
    no_level["verb"] = json!("glance");
    no_level.as_object_mut().unwrap().remove("identity_level");
    let (st, body) = send(&e.app, post(no_level)).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(code(&body), "identity_level_required");
    let (st, body) = send(
        &e.app,
        get("/api/platform/operations?object=widget", Some(&sa)),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body["items"]
            .as_array()
            .is_some_and(|i| !i.iter().any(|o| o["key"] == "widget.glance")),
        "{body}"
    );
}

/// Defense in depth: a super_admin cannot author a CONTRACT operation below its contract level (`contact_point.update`
/// is 3: the account-takeover guard must not depend on an author remembering), may tighten or match it, and a custom
/// operation takes whatever explicit level it is given. Nothing is created by a refused call.
#[tokio::test]
async fn a_contract_operation_cannot_be_authored_below_its_contract_level() {
    let e = fresh().await;
    let sa = super_admin();
    let post = |b: Value| json("POST", "/api/platform/operations", Some(&sa), b);
    for o in [
        json!({"object": "contact_point", "label": "Contact point", "tier": "base", "description": "A way to reach her."}),
        widget(),
    ] {
        let (st, body) = send(&e.app, json("POST", "/api/platform/objects", Some(&sa), o)).await;
        assert_eq!(st, StatusCode::CREATED, "{body}");
    }
    let contact = |level: &str| {
        json!({
            "object": "contact_point", "verb": "update", "modality": "do", "op_kind": "standard",
            "identity_level": level, "side_effect": "W", "confirm_back": true, "idempotency": "key",
            "description": "Change a contact point."
        })
    };
    for level in ["2", "0", "1"] {
        let (st, body) = send(&e.app, post(contact(level))).await;
        assert_eq!(
            st,
            StatusCode::UNPROCESSABLE_ENTITY,
            "level {level}: {body}"
        );
        assert_eq!(code(&body), "identity_level_below_contract", "{body}");
        assert_eq!(
            body["error"]["minimum"]
                .as_str()
                .or(body["minimum"].as_str()),
            Some("3"),
            "{body}"
        );
    }
    let (st, body) = send(
        &e.app,
        get("/api/platform/operations?object=contact_point", Some(&sa)),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(
        body["items"]
            .as_array()
            .is_some_and(|i| !i.iter().any(|o| o["key"] == "contact_point.update")),
        "a refused call created nothing: {body}"
    );
    // the contract's own level (and, below, a stricter one) is allowed
    let (st, body) = send(&e.app, post(contact("3"))).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    // a custom operation (not in the contract) takes any explicit level, 0 included
    let mut custom = read_op();
    custom["identity_level"] = json!("0");
    let (st, body) = send(&e.app, post(custom)).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn platform_tenant_exists_after_boot() {
    let id = Uuid::new_v4();
    let e = env_bare_with(|v| {
        v.insert("AGENT_PLATFORM_TENANT_ID".into(), id.to_string());
    })
    .await;
    let row: (String, bool, String) =
        sqlx::query_as("SELECT name, platform_owned, status FROM tenants WHERE id = $1")
            .bind(id)
            .fetch_one(&e.owner)
            .await
            .expect("the platform tenant row exists");
    assert_eq!(row, ("Platform".into(), true, "active".into()));
    // idempotent: a second ensure changes nothing and does not fail
    let mut c = e.owner.acquire().await.unwrap();
    ensure_platform_tenant(&mut c, Some(id)).await.unwrap();
    drop(c);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 1);
    // identity only: no layers, no rows
    let layers: i64 = sqlx::query_scalar("SELECT count(*) FROM layer")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(layers, 0);
}

#[tokio::test]
async fn no_platform_tenant_when_unset() {
    let e = env_bare().await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

/// The runtime keeps only DECLARED outputs, so a contract operation authored with empty I/O answered `{}` for every
/// read. A contract operation now stores the contract's I/O when none is given, accepts a subset, and refuses a field
/// outside the contract; a custom operation is untouched; the generic layer-rows path meets the same rule.
#[tokio::test]
async fn a_contract_operation_takes_its_declared_io() {
    let e = fresh().await;
    let sa = super_admin();
    let post = |b: Value| json("POST", "/api/platform/operations", Some(&sa), b);
    for o in [
        json!({"object": "order", "label": "Order", "tier": "base", "description": "A purchase."}),
        widget(),
    ] {
        let (st, body) = send(&e.app, json("POST", "/api/platform/objects", Some(&sa), o)).await;
        assert_eq!(st, StatusCode::CREATED, "{body}");
    }
    let order = |verb: &str| {
        json!({
            "object": "order", "verb": verb, "modality": "observe", "op_kind": "standard",
            "identity_level": "2", "side_effect": "R", "confirm_back": false, "idempotency": "nat",
            "description": "An order read."
        })
    };
    let names = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap_or_else(|| panic!("expected operation I/O array: {v}"))
            .iter()
            .map(|s| s.as_str().unwrap().to_owned())
            .collect()
    };
    // no I/O: the contract's is stored, and GET shows it
    let (st, body) = send(&e.app, post(order("list"))).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let (st, body) = send(
        &e.app,
        get("/api/platform/operations?object=order", Some(&sa)),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let row = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["key"] == "order.list")
        .unwrap_or_else(|| panic!("{body}"))
        .clone();
    // Catalogue and create/update responses flatten the resolved operation attributes.
    assert_eq!(names(&row["input_fields"]), ["party_id"], "{row}");
    assert_eq!(
        names(&row["output_fields"]),
        ["order_id", "status", "placed_at", "total", "items"],
        "{row}"
    );
    // a narrower subset is allowed
    let mut subset = order("read");
    subset["input_fields"] = json!(["order_id"]);
    subset["output_fields"] = json!(["status", "total"]);
    let (st, body) = send(&e.app, post(subset)).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(names(&body["output_fields"]), ["status", "total"], "{body}");
    // a field outside the contract is refused, naming it; nothing is created
    let mut bad = order("cancel");
    bad["identity_level"] = json!("3");
    bad["side_effect"] = json!("W");
    bad["output_fields"] = json!(["status", "mood"]);
    let (st, body) = send(&e.app, post(bad)).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(code(&body), "field_not_in_contract", "{body}");
    assert!(body.to_string().contains("mood"), "{body}");
    // PUT: emptying a contract operation's I/O restores the contract's
    let mut put = order("read");
    put.as_object_mut().unwrap().retain(|k, _| {
        [
            "identity_level",
            "side_effect",
            "confirm_back",
            "idempotency",
            "description",
        ]
        .contains(&k.as_str())
    });
    let (st, body) = send(
        &e.app,
        json("PUT", "/api/platform/operations/order/read", Some(&sa), put),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(names(&body["output_fields"]).len(), 10, "{body}");
    // a custom operation with empty I/O is accepted as before
    let (st, body) = send(&e.app, post(read_op())).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["output_fields"], json!([]), "{body}");

    // the generic layer-rows path: empty is filled, a stranger is refused
    let base: String = sqlx::query_scalar("SELECT id::text FROM layer WHERE key = 'base'")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let row = |output: Value| {
        json!({"kind": "operation", "key": "order.modify", "op": "add", "fields": {
            "discriminator": null,
            "attrs": {
                "modality": "request", "is_derive": false, "side_effect": "W", "idempotency": "key",
                "confirm_back": true, "verify_read": null, "builtin": false, "op_kind": "standard",
                "guard_rules": [], "identity_level": "2", "input_fields": [], "output_fields": output
            },
            "description": "Modify.", "retail": null, "stages": [], "extra_errors": []
        }})
    };
    let rows = format!("/api/layers/{base}/rows");
    let (st, body) = send(&e.app, json("POST", &rows, Some(&sa), row(json!(["mood"])))).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(code(&body), "field_not_in_contract", "{body}");
    let (st, body) = send(&e.app, json("POST", &rows, Some(&sa), row(json!([])))).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let (_, body) = send(
        &e.app,
        get("/api/platform/operations?object=order", Some(&sa)),
    )
    .await;
    let modify = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["key"] == "order.modify")
        .unwrap_or_else(|| panic!("{body}"));
    assert_eq!(
        names(&modify["output_fields"]),
        ["status", "lines", "total"],
        "{modify}"
    );
    assert_eq!(names(&modify["input_fields"]).len(), 3, "{modify}");
}
