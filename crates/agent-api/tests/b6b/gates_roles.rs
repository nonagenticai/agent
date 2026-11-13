//! B6b T12 / T12d: the P14 gate library and the P15 agent roles over authored rows — super_admin only;
//! the library is every live head `gate_question`; edits, options, create, retire, and role grants each write one
//! revision in the row's own layer with one `layer.revision` audit row (an edit that changes nothing writes none);
//! the §5 refusals. `gateLibraryUsage` and `tenant_job_unchanged_until_cut` are the B5 bucket (not here).
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::{
    TestEnv, get, insert_tenant, insert_user, json as json_req, person, send, super_admin,
};

const GATES: &str = "/api/platform/gates";
const ROLES: &str = "/api/platform/agent-roles";

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

async fn sa_get(e: &TestEnv, uri: &str) -> (StatusCode, Value) {
    send(&e.app, get(uri, Some(&super_admin()))).await
}

async fn sa(e: &TestEnv, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, json_req(method, uri, Some(&super_admin()), body)).await
}

async fn audits(e: &TestEnv) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM platform_audit WHERE action = 'layer.revision'")
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn revisions(e: &TestEnv, layer: &str, kind: &str, key: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM layer_row r JOIN layer l ON l.id = r.layer_id \
          WHERE l.key = $1 AND r.kind = $2 AND r.key = $3",
    )
    .bind(layer)
    .bind(kind)
    .bind(key)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

async fn untagged(e: &TestEnv, layer: &str, kind: &str, key: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM layer_untagged WHERE layer_key = $1 AND kind = $2 AND key = $3",
    )
    .bind(layer)
    .bind(kind)
    .bind(key)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

/// Sends `method uri body` as super_admin and asserts it succeeded and left exactly one revision of (layer, kind,
/// key), one `layer.revision` audit row and the row untagged.
async fn one_revision(
    e: &TestEnv,
    layer: &str,
    kind: &str,
    key: &str,
    method: &str,
    uri: &str,
    body: Value,
) -> Value {
    let (revs, audit) = (revisions(e, layer, kind, key).await, audits(e).await);
    let (s, b) = sa(e, method, uri, body).await;
    assert!(s.is_success(), "{s}: {b}");
    assert_eq!(
        revisions(e, layer, kind, key).await,
        revs + 1,
        "{layer} {kind} {key}"
    );
    assert_eq!(audits(e).await, audit + 1);
    assert_eq!(untagged(e, layer, kind, key).await, 1);
    b
}

fn find<'a>(items: &'a Value, key: &str, layer: &str) -> Option<&'a Value> {
    items["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|g| g["key"] == key && g["layer"] == layer)
}

async fn library(e: &TestEnv) -> Value {
    let (s, b) = sa_get(e, GATES).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

#[tokio::test]
async fn super_admin_only() {
    let e = crate::common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant a").await;
    insert_user(&e.owner, "u-jd", t, "job_designer").await;
    let jd = person("u-jd", &[]);
    for (method, uri) in [
        ("GET", GATES.to_string()),
        ("POST", GATES.to_string()),
        ("PATCH", format!("{GATES}/GT")),
        ("POST", format!("{GATES}/GT/options")),
        ("DELETE", format!("{GATES}/GT/options/need")),
        ("POST", format!("{GATES}/GT/retire")),
        ("GET", format!("{ROLES}/jobs")),
        ("GET", format!("{ROLES}?job=customer_care_agent")),
        ("PATCH", format!("{ROLES}/customer_care_agent/qualifier")),
        (
            "POST",
            format!("{ROLES}/customer_care_agent/qualifier/grants"),
        ),
        (
            "DELETE",
            format!("{ROLES}/customer_care_agent/qualifier/grants/party.list"),
        ),
    ] {
        let (s, b) = send(&e.app, json_req(method, &uri, Some(&jd), json!({}))).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{method} {uri}: {b}");
        assert_eq!(code(&b), "super_admin_required", "{method} {uri}");
    }
}

#[tokio::test]
async fn library_is_the_live_head_rows() {
    let e = crate::common::env().await;
    crate::b6b::support::gates(&e).await;
    let b = library(&e).await;
    let gt = find(&b, "GT", "base").expect("GT in base");
    assert_eq!(gt["origin"], "platform");
    assert_eq!(gt["type"], "choice");
    assert_eq!(gt["customer_reply"], Value::Null);
    assert!(gt["updated_by"].is_string() && gt["updated_at"].is_string());
    assert!(gt.get("summary").is_none());
    // GA lives in base (KC moved its job-layer patch up); a base department's own question is listed in its layer
    assert!(find(&b, "GA", "base").is_some());
    assert!(find(&b, "GA", "job/customer_care_agent").is_none());
    assert!(find(&b, "J1", "base_dept/it_service_desk").is_some());
    assert!(find(&b, "I1", "industry/utilities").is_some());
}

#[tokio::test]
async fn update_writes_one_revision_in_the_rows_layer() {
    let e = crate::common::env().await;
    crate::b6b::support::gates(&e).await;
    let text = "Which theme is the customer talking about?";
    let b = one_revision(
        &e,
        "base",
        "gate_question",
        "GT",
        "PATCH",
        &format!("{GATES}/GT"),
        json!({"question": text}),
    )
    .await;
    assert_eq!(b["question"], text);
    assert_eq!(b["layer"], "base");
    assert_eq!(b["updated_by"], "u-super");
    // the same edit again changes nothing and writes nothing
    let (revs, audit) = (
        revisions(&e, "base", "gate_question", "GT").await,
        audits(&e).await,
    );
    let (s, _) = sa(
        &e,
        "PATCH",
        &format!("{GATES}/GT"),
        json!({"question": text}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(revisions(&e, "base", "gate_question", "GT").await, revs);
    assert_eq!(audits(&e).await, audit);
    // ?layer= edits the row in the named layer: a base department's question
    one_revision(
        &e,
        "base_dept/it_service_desk",
        "gate_question",
        "J1",
        "PATCH",
        &format!("{GATES}/J1?layer=base_dept/it_service_desk"),
        json!({"question": "Which IT request is this?"}),
    )
    .await;
}

#[tokio::test]
async fn gate_question_customer_reply_round_trips() {
    let e = crate::common::env().await;
    crate::b6b::support::gates(&e).await;
    let reply = "A colleague will pick this up within the hour.";
    one_revision(
        &e,
        "base",
        "gate_question",
        "GT",
        "PATCH",
        &format!("{GATES}/GT"),
        json!({"customer_reply": reply}),
    )
    .await;
    let lib = library(&e).await;
    assert_eq!(find(&lib, "GT", "base").unwrap()["customer_reply"], reply);
    let stored: Value = sqlx::query_scalar(
        "SELECT fields FROM layer_head WHERE layer_key = 'base' AND kind = 'gate_question' AND key = 'GT'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(stored["customer_reply"], reply);
    let (s, b) = sa(
        &e,
        "PATCH",
        &format!("{GATES}/GT"),
        json!({"customer_reply": "x".repeat(501)}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "invalid_customer_reply");
    let (s, b) = sa(
        &e,
        "PATCH",
        &format!("{GATES}/GT"),
        json!({"customer_reply": null}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["customer_reply"], Value::Null);
}

#[tokio::test]
async fn update_refusals() {
    let e = crate::common::env().await;
    crate::b6b::support::gates(&e).await;
    let gte =
        json!({"fire_when": "gte", "threshold": 0.4, "fire_values": null, "min_confidence": null});
    // (gate, body, status, code)
    let cases: Vec<(&str, Value, StatusCode, &str)> = vec![
        (
            "NOPE",
            json!({"question": "Is this a real gate?"}),
            StatusCode::NOT_FOUND,
            "gate_not_found",
        ),
        (
            "GT",
            json!({"question": "Why?"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_question",
        ),
        // safety_gate_rule_409
        (
            "B1",
            json!({"rule": gte}),
            StatusCode::CONFLICT,
            "safety_gate",
        ),
        (
            "GC",
            json!({"threshold_value": 0.5}),
            StatusCode::CONFLICT,
            "threshold_not_tunable",
        ),
        (
            "GT",
            json!({"threshold_value": null}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_threshold",
        ),
        (
            "GT",
            json!({"threshold_value": 1.4}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_threshold",
        ),
        // score_threshold_range: GF has five levels
        (
            "GF",
            json!({"threshold_value": 7}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_threshold",
        ),
        (
            "GF",
            json!({"threshold_value": 2.5}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_threshold",
        ),
        (
            "GT",
            json!({"rule": {"fire_when": "in", "fire_values": ["zz"], "min_confidence": null}}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_rule",
        ),
        (
            "GC",
            json!({"generated": true}),
            StatusCode::CONFLICT,
            "options_fixed",
        ),
        (
            "GT",
            json!({"nope": 1}),
            StatusCode::BAD_REQUEST,
            "invalid_body",
        ),
    ];
    let audit = audits(&e).await;
    for (gate, body, status, want) in cases {
        let (s, b) = sa(&e, "PATCH", &format!("{GATES}/{gate}"), body.clone()).await;
        assert_eq!(s, status, "{gate} {body}: {b}");
        assert_eq!(code(&b), want, "{gate} {body}");
    }
    assert_eq!(audits(&e).await, audit, "a refusal writes nothing");
    let (s, b) = sa(
        &e,
        "PATCH",
        &format!("{GATES}/GA?layer=industry/retail"),
        json!({"threshold_value": 0.8}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "gate_not_found");
    // score_threshold_range: a level inside 1..=5 is taken and written into the threshold text
    let (s, b) = sa(
        &e,
        "PATCH",
        &format!("{GATES}/GF"),
        json!({"threshold_value": 3}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["threshold_value"], 3.0);
    assert!(b["threshold"].as_str().unwrap().contains('3'), "{b}");
}

#[tokio::test]
async fn options_add_and_remove() {
    let e = crate::common::env().await;
    crate::b6b::support::gates(&e).await;
    let b = one_revision(
        &e,
        "base",
        "gate_question",
        "GT",
        "POST",
        &format!("{GATES}/GT/options"),
        json!({"label": "Billing question", "description": "About an invoice"}),
    )
    .await;
    let keys: Vec<&str> = b["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["key"].as_str().unwrap())
        .collect();
    assert!(keys.contains(&"billing_question"), "{keys:?}");
    for (gate, body, status, want) in [
        (
            "GT",
            json!({"label": "billing question"}),
            StatusCode::CONFLICT,
            "option_exists",
        ),
        (
            "GT",
            json!({"label": "  "}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_option",
        ),
        (
            "GA",
            json!({"label": "Other"}),
            StatusCode::CONFLICT,
            "options_generated",
        ),
        (
            "B1",
            json!({"label": "Maybe"}),
            StatusCode::CONFLICT,
            "options_fixed",
        ),
        (
            "NOPE",
            json!({"label": "x"}),
            StatusCode::NOT_FOUND,
            "gate_not_found",
        ),
    ] {
        let (s, b) = sa(&e, "POST", &format!("{GATES}/{gate}/options"), body).await;
        assert_eq!(s, status, "{gate}: {b}");
        assert_eq!(code(&b), want, "{gate}");
    }
    one_revision(
        &e,
        "base",
        "gate_question",
        "GT",
        "DELETE",
        &format!("{GATES}/GT/options/billing_question"),
        json!({}),
    )
    .await;
    // the rule fires on `issue`: change the rule first
    let fires: Vec<String> = find(&library(&e).await, "GT", "base").unwrap()["rule"]["fire_values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let (s, b) = sa(
        &e,
        "DELETE",
        &format!("{GATES}/GT/options/{}", fires[0]),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "invalid_rule");
    for (gate, option, status, want) in [
        ("GT", "nope", StatusCode::NOT_FOUND, "option_not_found"),
        // remove_option_generated_409
        ("GA", "need", StatusCode::CONFLICT, "options_generated"),
        ("B1", "true", StatusCode::CONFLICT, "options_fixed"),
    ] {
        let (s, b) = sa(
            &e,
            "DELETE",
            &format!("{GATES}/{gate}/options/{option}"),
            json!({}),
        )
        .await;
        assert_eq!(s, status, "{gate}/{option}: {b}");
        assert_eq!(code(&b), want, "{gate}/{option}");
    }
}

fn draft(key: &str, layer: &str) -> Value {
    json!({
        "key": key, "layer": layer, "group": "base", "point": "P1", "type": "choice",
        "question": "Is the customer asking about a delivery?",
        "options": [{"label": "Yes"}, {"label": "No"}],
        "threshold": "top ≥ 0.6", "threshold_value": 0.6,
        "rule": {"fire_when": "in", "fire_values": ["yes"], "min_confidence": 0.6},
        "action": "annotate", "fail": "open"
    })
}

#[tokio::test]
async fn create_gate_validates_like_update() {
    let e = crate::common::env().await;
    crate::b6b::support::gates(&e).await;
    let audit = audits(&e).await;
    let mut short = draft("GX1", "base");
    short["question"] = json!("Why?");
    let mut rule = draft("GX1", "base");
    rule["rule"]["fire_values"] = json!(["zz"]);
    let mut threshold = draft("GX1", "base");
    threshold["threshold_value"] = json!(1.5);
    let mut many = draft("GX1", "base");
    many["options"] = Value::Array(
        (0..256)
            .map(|i| json!({"label": format!("Option {i}")}))
            .collect(),
    );
    for (body, status, want) in [
        (short, StatusCode::UNPROCESSABLE_ENTITY, "invalid_question"),
        (rule, StatusCode::UNPROCESSABLE_ENTITY, "invalid_rule"),
        (
            threshold,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_threshold",
        ),
        (many, StatusCode::UNPROCESSABLE_ENTITY, "too_many_options"),
        (
            draft("XX1", "base"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_gate_key",
        ),
        (
            draft("GX1", "industry/nope"),
            StatusCode::NOT_FOUND,
            "layer_not_found",
        ),
        // create_gate_exists_409: GT is live in base
        (
            draft("GT", "industry/retail"),
            StatusCode::CONFLICT,
            "gate_exists",
        ),
    ] {
        let (s, b) = sa(&e, "POST", GATES, body.clone()).await;
        assert_eq!(s, status, "{body}: {b}");
        assert_eq!(code(&b), want, "{body}");
    }
    assert_eq!(audits(&e).await, audit, "a refusal writes nothing");
    let b = one_revision(
        &e,
        "industry/retail",
        "gate_question",
        "GX1",
        "POST",
        GATES,
        draft("GX1", "industry/retail"),
    )
    .await;
    assert_eq!(b["layer"], "industry/retail");
    assert_eq!(b["options"][0]["key"], "yes");
    assert!(find(&library(&e).await, "GX1", "industry/retail").is_some());
    let (s, b) = sa(&e, "POST", GATES, draft("GX1", "base")).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "gate_exists");
}

#[tokio::test]
async fn retire_and_last_option() {
    let e = crate::common::env().await;
    crate::b6b::support::gates(&e).await;
    // retire_safety_gate_409
    let (s, b) = sa(&e, "POST", &format!("{GATES}/B1/retire"), json!({})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "safety_gate");
    let (s, b) = sa(&e, "POST", &format!("{GATES}/NOPE/retire"), json!({})).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "gate_not_found");
    // remove_last_option_409: a one-option choice
    let mut one = draft("GX2", "base");
    one["options"] = json!([{"label": "Yes"}]);
    one["rule"] = json!({"fire_when": "gte", "threshold": 0.6});
    let (s, b) = sa(&e, "POST", GATES, one).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = sa(&e, "DELETE", &format!("{GATES}/GX2/options/yes"), json!({})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "last_option");
    // retire: one tombstone revision; the library no longer lists it; its key is not reused in that layer
    let b = one_revision(
        &e,
        "base",
        "gate_question",
        "GX2",
        "POST",
        &format!("{GATES}/GX2/retire"),
        json!({}),
    )
    .await;
    assert_eq!(b["retired"], true);
    assert_eq!(b["jobs"], json!([]));
    assert!(find(&library(&e).await, "GX2", "base").is_none());
    let (s, b) = sa(&e, "POST", GATES, draft("GX2", "base")).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "add_tombstoned");
}

#[tokio::test]
async fn retire_lists_pinned_jobs() {
    let e = crate::common::env().await;
    crate::b6b::support::gates(&e).await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant a").await;
    let version: i32 = sqlx::query_scalar(
        "SELECT max(version) FROM layer_tag WHERE family = 'base' AND NOT provisional",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'care', 'Care', $2, 'test')",
    )
    .bind(t)
    .bind(json!([{"layer": "base", "version": version}]))
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = sa(&e, "POST", &format!("{GATES}/GC/retire"), json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["jobs"],
        json!([{"tenant_id": t.to_string(), "tenant_name": "Tenant a", "job": "care", "job_label": "Care"}])
    );
}

// ---- agent roles --------------------------------------------------------------------------------------------------

async fn roles(e: &TestEnv, job: &str) -> Vec<Value> {
    let (s, b) = sa_get(e, &format!("{ROLES}?job={job}")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["items"].as_array().unwrap().clone()
}

fn role<'a>(list: &'a [Value], key: &str) -> &'a Value {
    list.iter()
        .find(|r| r["role"] == key)
        .unwrap_or_else(|| panic!("role {key}"))
}

#[tokio::test]
async fn assistant_role_is_disabled() {
    let e = crate::common::env().await;
    crate::b6b::support::roles(&e).await;
    crate::b6b::support::ensure_layer(
        &e,
        "job/b6b_desk_agent",
        json!({"scope": "job", "job": "b6b_desk_agent", "label": "Desk agent"}),
    )
    .await;
    let list = roles(&e, "customer_care_agent").await;
    let a = role(&list, "assistant");
    assert_eq!(a["enabled"], false);
    assert_eq!(a["job"], "customer_care_agent");
    // no job layer carries a qualifier row, so the role is base's (D119)
    let q = role(&list, "qualifier");
    assert_eq!(q["enabled"], true);
    assert_eq!(q["max_side_effect"], "R");
    let (s, b) = sa(
        &e,
        "POST",
        &format!("{ROLES}/customer_care_agent/assistant/grants"),
        json!({"op": "order.read", "variant": null, "modalities": ["observe"]}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "role_disabled");
    let (s, b) = sa_get(&e, &format!("{ROLES}/jobs")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let jobs: Vec<&str> = b
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["job"].as_str().unwrap())
        .collect();
    // every job head is listed, one without a role row of its own too (D119)
    for j in ["customer_care_agent", "b6b_desk_agent"] {
        assert!(jobs.contains(&j), "{jobs:?}");
    }
    let (s, b) = sa_get(&e, &format!("{ROLES}?job=nope")).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
}

#[tokio::test]
async fn grant_refusals() {
    let e = crate::common::env().await;
    crate::b6b::support::roles(&e).await;
    let uri = format!("{ROLES}/customer_care_agent/qualifier/grants");
    for (body, status, want) in [
        // grant_above_max_422: qualifier allows R, access_grant.revoke is X
        (
            json!({"op": "access_grant.revoke", "variant": null, "modalities": ["do"]}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "exceeds_max_side_effect",
        ),
        (
            json!({"op": "order.teleport", "variant": null, "modalities": ["do"]}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_operation",
        ),
        (
            json!({"op": "delivery.read", "variant": null, "modalities": []}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_modalities",
        ),
    ] {
        let (s, b) = sa(&e, "POST", &uri, body.clone()).await;
        assert_eq!(s, status, "{body}: {b}");
        assert_eq!(code(&b), want, "{body}");
    }
    // procedure_role_derived_409
    let (s, b) = sa(
        &e,
        "POST",
        &format!("{ROLES}/customer_care_agent/procedure/grants"),
        json!({"op": "order.read", "variant": null, "modalities": ["observe"]}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "derived_grants");
    let (s, b) = sa(
        &e,
        "POST",
        &format!("{ROLES}/customer_care_agent/nope/grants"),
        json!({"op": "order.read", "variant": null, "modalities": ["observe"]}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "role_not_found");
}

#[tokio::test]
async fn grants_and_max_write_one_revision() {
    let e = crate::common::env().await;
    crate::b6b::support::roles(&e).await;
    let job = "job/customer_care_agent";
    let b = one_revision(
        &e,
        job,
        "agent_role",
        "qualifier",
        "POST",
        &format!("{ROLES}/customer_care_agent/qualifier/grants"),
        json!({"op": "delivery.read", "variant": null, "modalities": ["observe"]}),
    )
    .await;
    assert!(
        b["grants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["object"] == "delivery" && g["verb"] == "read")
    );
    // the duplicate is of the grant just added
    let (s, b) = sa(
        &e,
        "POST",
        &format!("{ROLES}/customer_care_agent/qualifier/grants"),
        json!({"op": "delivery.read", "variant": null, "modalities": ["observe"]}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "grant_exists");
    one_revision(
        &e,
        job,
        "agent_role",
        "qualifier",
        "DELETE",
        &format!("{ROLES}/customer_care_agent/qualifier/grants/delivery.read"),
        json!({}),
    )
    .await;
    let (s, b) = sa(
        &e,
        "DELETE",
        &format!("{ROLES}/customer_care_agent/qualifier/grants/delivery.read"),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "grant_not_found");
    // max_below_grants_422_names_them: raise to W, grant a W op, lowering to R names it
    one_revision(
        &e,
        job,
        "agent_role",
        "qualifier",
        "PATCH",
        &format!("{ROLES}/customer_care_agent/qualifier"),
        json!({"max_side_effect": "W"}),
    )
    .await;
    let (s, b) = sa(
        &e,
        "POST",
        &format!("{ROLES}/customer_care_agent/qualifier/grants"),
        json!({"op": "case.comment", "variant": null, "modalities": ["do"]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = sa(
        &e,
        "PATCH",
        &format!("{ROLES}/customer_care_agent/qualifier"),
        json!({"max_side_effect": "R"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "grants_exceed_max");
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("case.comment"),
        "{b}"
    );
    // another role of the job: one revision of its job row
    one_revision(
        &e,
        job,
        "agent_role",
        "post_gate",
        "POST",
        &format!("{ROLES}/customer_care_agent/post_gate/grants"),
        json!({"op": "party.read", "variant": null, "modalities": ["observe"]}),
    )
    .await;
}
