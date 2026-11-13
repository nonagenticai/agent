//! B8 T4a / T4b / T7a: the template-editing routes of the platform journey designer and the tenant card routes,
//! through the real router. Content: `support::content` (retail / customer_service). Each test reads what it needs (stage and channel keys)
//! from the grid, so it does not pin the seed's content.
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::b8::support;
use crate::common::design_contract::{self, DESIGNER};
use crate::common::{self, insert_user, person, send, super_admin};

async fn platform(
    e: &common::TestEnv,
    method: &str,
    tail: &str,
    body: Value,
) -> (StatusCode, Value) {
    support::content(e).await;
    let sep = if tail.contains('?') { '&' } else { '?' };
    send(
        &e.app,
        common::json(
            method,
            &format!("/api/platform/journeys/retail{tail}{sep}department=customer_service"),
            Some(&super_admin()),
            body,
        ),
    )
    .await
}

async fn grid(e: &common::TestEnv) -> Value {
    support::content(e).await;
    let (s, g) = send(
        &e.app,
        common::get(
            "/api/platform/journeys/retail?department=customer_service",
            Some(&super_admin()),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{g}");
    g
}

fn stage_keys(g: &Value) -> Vec<String> {
    let mut stages: Vec<&Value> = g["stages"].as_array().unwrap().iter().collect();
    stages.sort_by_key(|s| s["order"].as_i64().unwrap());
    stages
        .iter()
        .map(|s| s["key"].as_str().unwrap().to_owned())
        .collect()
}

fn activity<'a>(g: &'a Value, key: &str) -> Option<&'a Value> {
    g["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["key"] == key)
}

async fn new_activity(e: &common::TestEnv, slug: &str) -> String {
    let (s, b) = platform(
        e,
        "POST",
        "/activities",
        json!({"stage":"support","slug":slug,"label":"Question","actor":"customer"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    format!("support:{slug}")
}

fn error_code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn update_activity_patches_label_and_also_in_stages() {
    let e = common::env().await;
    let key = new_activity(&e, "edit_me").await;
    let other = stage_keys(&grid(&e).await)
        .into_iter()
        .find(|s| s != "support")
        .expect("the journey has a second stage");
    let (s, g) = platform(
        &e,
        "PATCH",
        &format!("/activities/{key}"),
        json!({"label":"  Where is my order  ","also_in_stages":[other]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{g}");
    let a = activity(&g, &key).unwrap();
    assert_eq!(a["label"], "Where is my order");
    assert_eq!(a["also_in_stages"], json!([other]));
}

/// A designer save writes the whole activity row back: the technical `summary` and the plain `description` survive
/// every save that does not name them, can be edited, and an empty text clears them.
#[tokio::test]
async fn saving_an_activity_keeps_its_summary_and_description() {
    let e = common::env().await;
    // an authored activity carries both; saving its label changes neither
    let g = grid(&e).await;
    let seeded = activity(&g, "support:return").expect("the authored return activity");
    let (summary, description) = (seeded["summary"].clone(), seeded["description"].clone());
    assert!(summary.is_string() && description.is_string(), "{seeded}");
    let (s, g) = platform(
        &e,
        "PATCH",
        "/activities/support:return",
        json!({"label":"Return an item"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{g}");
    let a = activity(&g, "support:return").unwrap();
    assert_eq!(a["summary"], summary);
    assert_eq!(a["description"], description);

    // a new activity: set both, re-save something else, edit one, clear the other
    let key = new_activity(&e, "keeps_text").await;
    let (s, g) = platform(
        &e,
        "PATCH",
        &format!("/activities/{key}"),
        json!({"summary":"customer · order.read","description":"  Find out where my order is  "}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{g}");
    let a = activity(&g, &key).unwrap();
    assert_eq!(a["summary"], "customer · order.read");
    assert_eq!(a["description"], "Find out where my order is");
    let (s, g) = platform(
        &e,
        "PATCH",
        &format!("/activities/{key}"),
        json!({"trigger":"asks about a parcel"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{g}");
    let a = activity(&g, &key).unwrap();
    assert_eq!(a["summary"], "customer · order.read");
    assert_eq!(a["description"], "Find out where my order is");
    let (s, g) = platform(
        &e,
        "PATCH",
        &format!("/activities/{key}"),
        json!({"description":"Where is my parcel","summary":""}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{g}");
    let a = activity(&g, &key).unwrap();
    assert_eq!(a["description"], "Where is my parcel");
    assert!(a.get("summary").is_none(), "{a}");
}

#[tokio::test]
async fn also_in_unknown_stage_is_422() {
    let e = common::env().await;
    let key = new_activity(&e, "also_bad").await;
    for stages in [json!(["no_such_stage"]), json!(["support"])] {
        let (s, b) = platform(
            &e,
            "PATCH",
            &format!("/activities/{key}"),
            json!({"also_in_stages":stages}),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert_eq!(error_code(&b), "unknown_stage", "{b}");
    }
    let (s, b) = platform(
        &e,
        "PATCH",
        &format!("/activities/{key}"),
        json!({"label":"  "}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
}

#[tokio::test]
async fn retire_tombstones_activity_card_and_references_in_one_txn() {
    let e = common::env().await;
    let key = new_activity(&e, "retire_me").await;
    let (s, b) = platform(
        &e,
        "PUT",
        &format!("/activities/{key}/card"),
        json!({"summary":"Soon gone"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, g) = platform(&e, "DELETE", &format!("/activities/{key}"), Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{g}");
    assert!(activity(&g, &key).is_none(), "retired activity is gone");
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM layer_row r JOIN layer l ON l.id=r.layer_id \
         WHERE l.key='industry_dept/retail/customer_service' AND r.key=$1 AND r.is_current AND r.op <> 'tombstone'",
    )
    .bind(&key)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(live, 0, "activity and card rows are tombstoned");
    let (s, b) = platform(&e, "DELETE", &format!("/activities/{key}"), Value::Null).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
}

#[tokio::test]
async fn add_stage_at_base_appears_in_every_industry() {
    let e = common::env().await;
    let (s, b) = send(
        &e.app,
        common::json(
            "POST",
            "/api/platform/journeys/base/stages",
            Some(&super_admin()),
            json!({"key":"loyalty","label":"Loyalty","description":"Points and rewards"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["stage"], "loyalty");
    assert!(stage_keys(&grid(&e).await).contains(&"loyalty".to_owned()));
}

#[tokio::test]
async fn added_stage_without_activity_turns_rule4_red() {
    let e = common::env().await;
    let (s, g) = platform(
        &e,
        "POST",
        "/stages",
        json!({"key":"loyalty","label":"Loyalty","description":"Points and rewards"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{g}");
    assert_eq!(stage_keys(&g).last().map(String::as_str), Some("loyalty"));
    assert!(
        g["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["rule"] == 4 && f["severity"] == "red" && f["stage"] == "loyalty"),
        "{g}"
    );
    let (s, b) = platform(
        &e,
        "POST",
        "/stages",
        json!({"key":"loyalty","label":"Again","description":"x"}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(error_code(&b), "stage_exists");
}

#[tokio::test]
async fn reorder_must_name_every_stage() {
    let e = common::env().await;
    let mut keys = stage_keys(&grid(&e).await);
    let (s, b) = platform(&e, "PUT", "/stages/order", json!({"stages": &keys[1..]})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(error_code(&b), "order_mismatch");
    keys.reverse();
    let (s, g) = platform(&e, "PUT", "/stages/order", json!({"stages": keys})).await;
    assert_eq!(s, StatusCode::OK, "{g}");
    assert_eq!(stage_keys(&g), keys);
}

#[tokio::test]
async fn theme_weight_bounds() {
    let e = common::env().await;
    for w in [101, -1] {
        let (s, b) = platform(
            &e,
            "POST",
            "/themes",
            json!({"key":"fresh_theme","label":"Fresh","escalation_weight":w}),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert_eq!(error_code(&b), "invalid_weight");
    }
    let (s, b) = platform(
        &e,
        "POST",
        "/themes",
        json!({"key":"fresh_theme","label":"Fresh","escalation_weight":40.5}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    let (s, g) = platform(
        &e,
        "POST",
        "/themes",
        json!({"key":"fresh_theme","label":"Fresh","escalation_weight":100}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{g}");
    let t = g["themes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["key"] == "fresh_theme")
        .unwrap();
    assert_eq!(t["escalation_weight"], 1.0);
    let (s, g) = platform(
        &e,
        "PATCH",
        "/themes/fresh_theme",
        json!({"escalation_weight":0,"label":"Calm"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{g}");
    let t = g["themes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["key"] == "fresh_theme")
        .unwrap();
    assert_eq!(
        (t["escalation_weight"].as_f64(), t["label"].as_str()),
        (Some(0.0), Some("Calm"))
    );
}

#[tokio::test]
async fn stage_kind_set_and_remove() {
    let e = common::env().await;
    let kind = "policy.returns_extra";
    let path = format!("/stages/support/knowledge-kinds/{kind}");
    let (s, b) = platform(
        &e,
        "PUT",
        &path,
        json!({"store_key":"Bad Key","required":true}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    let (s, g) = platform(
        &e,
        "PUT",
        &path,
        json!({"store_key":"returns_policy","required":true}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{g}");
    let found = |g: &Value| {
        g["knowledge_kinds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k["stage"] == "support" && k["kind"] == kind)
    };
    assert!(found(&g), "{g}");
    let (s, g) = platform(&e, "DELETE", &path, Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{g}");
    assert!(!found(&g), "{g}");
    let (s, b) = platform(&e, "DELETE", &path, Value::Null).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(error_code(&b), "kind_not_found");
}

#[tokio::test]
async fn messages_per_channel_trimmed_and_capped() {
    let e = common::env().await;
    let (s, g) = platform(
        &e,
        "PUT",
        "/stages/support/messages/chat",
        json!({"utterances":["  where is my parcel ","where is my parcel","", "help"]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{g}");
    let m = g["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["stage"] == "support" && m["channel"] == "chat")
        .unwrap();
    assert_eq!(m["utterances"], json!(["where is my parcel", "help"]));
    let many: Vec<String> = (0..51).map(|i| format!("message {i}")).collect();
    let (s, b) = platform(
        &e,
        "PUT",
        "/stages/support/messages/chat",
        json!({"utterances":many}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(error_code(&b), "too_many_utterances");
    let (s, b) = platform(
        &e,
        "PUT",
        "/stages/support/messages/chat",
        json!({"utterances":["x".repeat(501)]}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(error_code(&b), "utterance_too_long");
}

#[tokio::test]
async fn unknown_channel_404() {
    let e = common::env().await;
    let (s, b) = platform(
        &e,
        "PUT",
        "/stages/support/messages/carrier_pigeon",
        json!({"utterances":["coo"]}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(error_code(&b), "channel_not_found");
    let (s, b) = platform(
        &e,
        "PUT",
        "/channels/carrier_pigeon",
        json!({"status":"v1"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
}

#[tokio::test]
async fn channel_status_is_per_industry() {
    let e = common::env().await;
    let (s, b) = platform(&e, "PUT", "/channels/chat", json!({"status":"someday"})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(error_code(&b), "invalid_status");
    let (s, g) = platform(&e, "PUT", "/channels/chat", json!({"status":"later"})).await;
    assert_eq!(s, StatusCode::OK, "{g}");
    let chat = g["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["key"] == "chat")
        .unwrap();
    assert_eq!(chat["status"], "later");
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT l.key FROM layer_row r JOIN layer l ON l.id=r.layer_id \
         WHERE r.key='chat' AND r.is_current AND r.kind='journey_channel' AND r.op <> 'tombstone'",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert!(
        rows.iter().any(|(k,)| k == "industry/retail"),
        "the status is written at the industry layer: {rows:?}"
    );
}

#[tokio::test]
async fn template_edit_needs_super_admin() {
    let e = common::env().await;
    let f = design_contract::new_job(&e).await;
    insert_user(&e.owner, DESIGNER, f.tenant, "job_designer").await;
    let designer = person(DESIGNER, &[]);
    for (method, tail, body) in [
        (
            "POST",
            "/stages",
            json!({"key":"nope","label":"N","description":"d"}),
        ),
        ("PUT", "/stages/order", json!({"stages":[]})),
        (
            "POST",
            "/themes",
            json!({"key":"nope","label":"N","escalation_weight":1}),
        ),
        ("PUT", "/channels/chat", json!({"status":"v1"})),
        (
            "PUT",
            "/stages/support/messages/chat",
            json!({"utterances":[]}),
        ),
        ("DELETE", "/activities/support:x", Value::Null),
        (
            "PUT",
            "/activities/support:x/operations/order.read",
            json!({"expectation":"R"}),
        ),
        ("PUT", "/activities/support:x/card", json!({"summary":"s"})),
    ] {
        let (s, b) = send(
            &e.app,
            common::json(
                method,
                &format!("/api/platform/journeys/retail{tail}?department=customer_service"),
                Some(&designer),
                body,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{method} {tail}: {b}");
        assert_eq!(error_code(&b), "super_admin_required");
    }
}

#[tokio::test]
async fn unknown_op_is_422() {
    let e = common::env().await;
    let key = new_activity(&e, "ops_unknown").await;
    let (s, b) = platform(
        &e,
        "PUT",
        &format!("/activities/{key}/operations/order.teleport"),
        json!({"expectation":"R"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(error_code(&b), "unknown_operation");
}

#[tokio::test]
async fn op_of_unbound_object_is_422() {
    let e = common::env().await;
    let key = new_activity(&e, "ops_unbound").await;
    let (s, b) = platform(
        &e,
        "PUT",
        &format!("/activities/{key}/operations/order.read"),
        json!({"expectation":"R"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(error_code(&b), "object_not_bound");
}

#[tokio::test]
async fn optional_expectation_and_remove_unknown_expectation_404() {
    let e = common::env().await;
    let key = new_activity(&e, "ops_optional").await;
    let (s, b) = platform(
        &e,
        "PUT",
        &format!("/activities/{key}/objects/order"),
        json!({"role":"subject","fields":["status"],"required_inputs":[]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, g) = platform(
        &e,
        "PUT",
        &format!("/activities/{key}/operations/order.read"),
        json!({"expectation":"O"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{g}");
    assert_eq!(
        activity(&g, &key).unwrap()["operations"][0]["required"],
        false
    );
    let (s, b) = platform(
        &e,
        "PUT",
        &format!("/activities/{key}/operations/order.read"),
        json!({"expectation":"maybe"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(error_code(&b), "invalid_expectation");
    let (s, g) = platform(
        &e,
        "DELETE",
        &format!("/activities/{key}/operations/order.read"),
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{g}");
    assert!(
        activity(&g, &key).unwrap()["operations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let (s, b) = platform(
        &e,
        "DELETE",
        &format!("/activities/{key}/operations/order.read"),
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(error_code(&b), "operation_not_expected");
}

#[tokio::test]
async fn scenarios_cannot_be_refined_and_reset_card_restores_template() {
    let e = common::env().await;
    let f = support::new_job(&e).await;
    insert_user(&e.owner, DESIGNER, f.tenant, "job_designer").await;
    let token = person(DESIGNER, &[]);
    let (s, journey) = send(
        &e.app,
        common::get(
            &format!("/api/jobs/{}/journey", f.job.job_key),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{journey}");
    let key = journey["activities"][0]["key"].as_str().unwrap().to_owned();
    let card = format!(
        "/api/jobs/{}/refinements/journey/cards/{key}",
        f.job.job_key
    );
    let (s, b) = send(
        &e.app,
        common::json("PUT", &card, Some(&token), json!({"scenarios":[]})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(error_code(&b), "scenarios_not_refinable");
    let (s, b) = send(
        &e.app,
        common::json("DELETE", &card, Some(&token), Value::Null),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(error_code(&b), "not_refined");
    let (s, b) = send(
        &e.app,
        common::json("PUT", &card, Some(&token), json!({"summary":"Ours"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let detail = format!("/api/jobs/{}/journey/activities/{key}", f.job.job_key);
    let tenant_card = |d: &Value| {
        d["use_case"]["lineage"]
            .as_array()
            .and_then(|l| l.iter().find(|x| x["kind"] == "tenant"))
            .map(|x| x["card"].clone())
    };
    let (s, d) = send(&e.app, common::get(&detail, Some(&token))).await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert_eq!(tenant_card(&d), Some(json!("refined")), "{d}");
    let (s, b) = send(
        &e.app,
        common::json("DELETE", &card, Some(&token), Value::Null),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(
        &e.app,
        common::json("DELETE", &card, Some(&token), Value::Null),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "reset twice: {b}");
}

#[tokio::test]
async fn refinement_needs_job_designer() {
    let e = common::env().await;
    let f = design_contract::new_job(&e).await;
    insert_user(&e.owner, "card-viewer", f.tenant, "tenant_user").await;
    let card = format!(
        "/api/jobs/{}/refinements/journey/cards/support:x",
        f.job.job_key
    );
    let (s, b) = send(
        &e.app,
        common::json(
            "PUT",
            &card,
            Some(&person("card-viewer", &[])),
            json!({"summary":"x"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
}

// --- T4: the industry list, stage / prior edits, move, unbind ---------------------------------------------------------

#[tokio::test]
async fn journey_industries_lists_retail_customer_service() {
    let e = common::env().await;
    support::content(&e).await;
    let (s, b) = send(
        &e.app,
        common::get("/api/platform/journeys", Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let row = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["industry"] == "retail" && i["department"] == "customer_service")
        .unwrap_or_else(|| panic!("retail / customer_service is listed: {b}"));
    assert!(row["activities"].as_i64().unwrap() > 0, "{row}");
    for k in ["label", "department_label", "status", "red"] {
        assert!(row.get(k).is_some(), "{k}: {row}");
    }
    let f = design_contract::new_job(&e).await;
    insert_user(&e.owner, DESIGNER, f.tenant, "job_designer").await;
    let (s, b) = send(
        &e.app,
        common::get("/api/platform/journeys", Some(&person(DESIGNER, &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(error_code(&b), "super_admin_required");
}

#[tokio::test]
async fn update_stage_patches_label_and_description_with_errors() {
    let e = common::env().await;
    let (s, g) = platform(
        &e,
        "PATCH",
        "/stages/support",
        json!({"label":"  Help & support  ","description":"  Questions and problems  "}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{g}");
    let st = g["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == "support")
        .unwrap();
    assert_eq!(st["label"], "Help & support");
    assert_eq!(st["description"], "Questions and problems");
    for (body, code) in [
        (json!({"label":"  "}), "label_required"),
        (json!({"description":""}), "description_required"),
    ] {
        let (s, b) = platform(&e, "PATCH", "/stages/support", body).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert_eq!(error_code(&b), code, "{b}");
    }
    let (s, b) = platform(&e, "PATCH", "/stages/no_such_stage", json!({"label":"x"})).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(error_code(&b), "stage_not_found");
    let (s, b) = send(
        &e.app,
        common::json(
            "PATCH",
            "/api/platform/journeys/no_such_industry/stages/support?department=customer_service",
            Some(&super_admin()),
            json!({"label":"x"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(error_code(&b), "journey_not_found");
}

#[tokio::test]
async fn set_theme_stage_prior_rounds_retires_and_rejects_bad_weights() {
    let e = common::env().await;
    let g0 = grid(&e).await;
    let theme = g0["themes"][0]["key"].as_str().unwrap().to_owned();
    let path = format!("/priors/{theme}/support");
    let prior = |g: &Value| {
        g["priors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["theme"] == theme.as_str() && p["stage"] == "support")
            .map(|p| p["weight"].as_f64().unwrap())
    };
    let (s, g) = platform(&e, "PUT", &path, json!({"weight":0.347})).await;
    assert_eq!(s, StatusCode::OK, "{g}");
    assert_eq!(prior(&g), Some(0.35), "rounded to 0.01: {g}");
    let (s, g) = platform(&e, "PUT", &path, json!({"weight":0})).await;
    assert_eq!(s, StatusCode::OK, "{g}");
    assert_eq!(prior(&g), None, "0 retires the row: {g}");
    for w in [json!(1.5), json!(-0.1)] {
        let (s, b) = platform(&e, "PUT", &path, json!({"weight":w})).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert_eq!(error_code(&b), "invalid_weight", "{b}");
    }
    let (s, b) = platform(
        &e,
        "PUT",
        &format!("/priors/{theme}/no_such_stage"),
        json!({"weight":0.5}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(error_code(&b), "stage_not_found");
    let (s, b) = send(
        &e.app,
        common::json(
            "PUT",
            &format!("/api/platform/journeys/no_such_industry{path}?department=customer_service"),
            Some(&super_admin()),
            json!({"weight":0.5}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(error_code(&b), "journey_not_found");
}

#[tokio::test]
async fn move_activity_rekeys_into_the_target_stage() {
    let e = common::env().await;
    let key = new_activity(&e, "move_me").await;
    let target = stage_keys(&grid(&e).await)
        .into_iter()
        .find(|s| s != "support")
        .expect("the journey has a second stage");
    let (s, b) = platform(
        &e,
        "POST",
        &format!("/activities/{key}/move"),
        json!({"stage":"support"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(error_code(&b), "same_stage");
    let (s, g) = platform(
        &e,
        "POST",
        &format!("/activities/{key}/move"),
        json!({"stage":target}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{g}");
    assert!(activity(&g, &key).is_none(), "the old key is gone");
    let moved = activity(&g, &format!("{target}:move_me")).expect("re-keyed into the target");
    assert_eq!(moved["stage"], target.as_str());
    assert_eq!(moved["label"], "Question");
    // the old key no longer exists
    let (s, b) = platform(
        &e,
        "POST",
        &format!("/activities/{key}/move"),
        json!({"stage":target}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(error_code(&b), "activity_not_found");
}

#[tokio::test]
async fn move_activity_onto_an_existing_key_is_409() {
    let e = common::env().await;
    let key = new_activity(&e, "move_clash").await;
    let target = stage_keys(&grid(&e).await)
        .into_iter()
        .find(|s| s != "support")
        .expect("the journey has a second stage");
    let (s, b) = platform(
        &e,
        "POST",
        "/activities",
        json!({"stage":target,"slug":"move_clash","label":"Same name","actor":"customer"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = platform(
        &e,
        "POST",
        &format!("/activities/{key}/move"),
        json!({"stage":target}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(error_code(&b), "activity_exists");
    let (s, b) = platform(
        &e,
        "POST",
        &format!("/activities/{key}/move"),
        json!({"stage":"no_such_stage"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(error_code(&b), "unknown_stage");
}

#[tokio::test]
async fn unbind_object_removes_the_binding_and_refuses_an_unbound_one() {
    let e = common::env().await;
    let key = new_activity(&e, "unbind_me").await;
    let (s, b) = platform(
        &e,
        "PUT",
        &format!("/activities/{key}/objects/order"),
        json!({"role":"subject","fields":["status"],"required_inputs":[]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(activity(&b, &key).unwrap()["objects"][0]["object"], "order");
    let (s, g) = platform(
        &e,
        "DELETE",
        &format!("/activities/{key}/objects/order"),
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{g}");
    assert!(
        activity(&g, &key).unwrap()["objects"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let (s, b) = platform(
        &e,
        "DELETE",
        &format!("/activities/{key}/objects/order"),
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(error_code(&b), "object_not_bound");
    let (s, b) = platform(
        &e,
        "DELETE",
        "/activities/support:no_such/objects/order",
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(error_code(&b), "activity_not_found");
}
