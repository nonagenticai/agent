//! Local authoring helpers for the B6b tests. There is no boot seed: a fresh database holds only the platform
//! offering `common` installs (base, `industry/retail`, one retail department, the `customer_care_agent` job layer
//! tagged to v4, the `region` dimension). A test that asserts on an object, operation, gate, role, axis or knowledge
//! kind authors the minimal rows itself, through the authoring routes (`POST /api/layers`, `POST
//! /api/layers/{id}/rows`, `POST /api/platform/templates/{family}/tags`) or, for the vocabulary tables that have no
//! authoring route for what a test needs (axis values, dimensions, renewal presets), the owner pool. The data is
//! tiny and neutral.
#![allow(dead_code)]
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::{TestEnv, json as json_req, send, super_admin};

pub async fn sa(e: &TestEnv, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, json_req(method, uri, Some(&super_admin()), body)).await
}

/// The id of layer `key` (it must exist).
pub async fn layer_id(e: &TestEnv, key: &str) -> String {
    let id: Uuid = sqlx::query_scalar("SELECT id FROM layer WHERE key = $1")
        .bind(key)
        .fetch_one(&e.owner)
        .await
        .unwrap_or_else(|err| panic!("layer {key}: {err}"));
    id.to_string()
}

/// Create a layer from a `POST /api/layers` body, unless `key` exists already; the layer's id either way.
pub async fn ensure_layer(e: &TestEnv, key: &str, body: Value) -> String {
    let found: Option<Uuid> = sqlx::query_scalar("SELECT id FROM layer WHERE key = $1")
        .bind(key)
        .fetch_optional(&e.owner)
        .await
        .unwrap();
    if let Some(id) = found {
        return id.to_string();
    }
    let (s, b) = sa(e, "POST", "/api/layers", body.clone()).await;
    assert_eq!(s, StatusCode::CREATED, "layer {key} {body}: {b}");
    b["id"].as_str().expect("layer id").to_owned()
}

/// Add one row to layer `layer` (a key); 201 or the test fails.
pub async fn row(e: &TestEnv, layer: &str, kind: &str, key: &str, fields: Value) -> Value {
    let id = layer_id(e, layer).await;
    let (s, b) = sa(
        e,
        "POST",
        &format!("/api/layers/{id}/rows"),
        json!({"kind": kind, "key": key, "op": "add", "fields": fields}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{layer} {kind} {key}: {b}");
    b
}

/// Tag `family` (a layer key; `/` is `.` in the URL slug): 201 or the test fails.
pub async fn tag(e: &TestEnv, family: &str) {
    let (s, b) = sa(
        e,
        "POST",
        &format!("/api/platform/templates/{}/tags", family.replace('/', ".")),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "tag {family}: {b}");
}

// ---- the object model ---------------------------------------------------------------------------------------------

fn field(name: &str, ty: &str, required: bool, pii: bool, values: &[&str]) -> Value {
    json!({"field": name, "data_type": ty, "pii": pii, "required": required, "enum_values": values,
           "ref": null, "description": ""})
}

fn state(status: &str, label: &str, terminal: bool, initial: bool) -> Value {
    json!({"status": status, "label": label, "terminal": terminal, "canonical": true, "initial": initial})
}

/// An `object` row with no fields.
pub fn bare_object(label: &str) -> Value {
    json!({"label": label, "tier": "base", "description": ""})
}

/// An operation `*` row (or a variant, with `discriminator` and a partial `attrs`).
#[allow(clippy::too_many_arguments)]
pub fn operation(
    side_effect: &str,
    modality: &str,
    idempotency: &str,
    confirm_back: bool,
    verify_read: Option<&str>,
    input: &[&str],
    output: &[&str],
    description: &str,
) -> Value {
    json!({
        "discriminator": null,
        "attrs": {
            "modality": modality, "is_derive": false, "side_effect": side_effect, "idempotency": idempotency,
            "confirm_back": confirm_back, "verify_read": verify_read, "builtin": false, "op_kind": "standard",
            "guard_rules": [], "identity_level": "2", "input_fields": input, "output_fields": output
        },
        "description": description, "retail": null, "stages": [], "extra_errors": []
    })
}

/// A read operation row (`R`, observe) that takes `order_id`-like input and nothing else.
pub fn read_operation(description: &str) -> Value {
    operation("R", "observe", "nat", false, None, &[], &[], description)
}

/// The object model the T11 / T12c / T12f tests edit, authored as base rows and tagged (so the head shows no
/// pending edit until a test writes one): objects `order` (4 fields, 2 states, a cancel transition, one relation),
/// `payment`, `case` (also refined in the base department `base_dept/it_service_desk`), `rule`, and `location` in
/// `industry/retail`; operations `order.read`, `order.cancel` (X, verified by `order.read`), `order.place` (P),
/// the built-in `rule.evaluate`, `case.create` with its `return` variant; five error states.
pub async fn object_model(e: &TestEnv) {
    ensure_layer(
        e,
        "base_dept/it_service_desk",
        json!({"scope": "base_dept", "department": "it_service_desk", "label": "IT service desk"}),
    )
    .await;
    row(
        e,
        "base",
        "object",
        "order",
        json!({
            "label": "Order", "tier": "base", "description": "A purchase.", "in_retail_job": true,
            "fields": [
                field("order_id", "id", true, false, &[]),
                field("status", "enum", true, false, &["processing", "cancelled"]),
                field("total", "money", true, false, &[]),
                field("gift_note", "text", false, true, &[]),
            ],
            "relations": [{"subject": "order", "predicate": "paid by", "object": "payment", "cardinality": "1:n"}],
            "states": [state("processing", "Processing", false, true), state("cancelled", "Cancelled", true, false)],
            "transitions": [{"verb": "order.cancel", "from_status": "processing", "to_status": "cancelled",
                             "guard_rule": null}],
        }),
    )
    .await;
    row(e, "base", "object", "payment", bare_object("Payment")).await;
    row(e, "base", "object", "rule", bare_object("Rule")).await;
    let case = |values: &[&str]| {
        json!({
            "label": "Case", "tier": "base", "description": "A ticket.",
            "fields": [field("case_id", "id", true, false, &[]), field("case_type", "enum", true, false, values)],
        })
    };
    row(e, "base", "object", "case", case(&["incident", "return"])).await;
    row(
        e,
        "base_dept/it_service_desk",
        "object",
        "case",
        case(&["incident"]),
    )
    .await;
    row(
        e,
        "industry/retail",
        "object",
        "location",
        bare_object("Location"),
    )
    .await;

    row(
        e,
        "base",
        "operation",
        "order.read",
        operation(
            "R",
            "observe",
            "nat",
            false,
            None,
            &["order_id"],
            &["status", "total"],
            "One order; also the verify_read for modify and cancel.",
        ),
    )
    .await;
    row(
        e,
        "base",
        "operation",
        "order.cancel",
        operation(
            "X",
            "do",
            "key+dup",
            true,
            Some("order.read"),
            &["order_id", "reason"],
            &["status", "refund_expected"],
            "Cancel an order. Only from processing; every other status is an invalid_state error.",
        ),
    )
    .await;
    let mut place = operation(
        "P",
        "do",
        "key+dup",
        true,
        Some("order.read"),
        &["order_id"],
        &["status", "payment_url"],
        "Turn a quote into an order; payment via a hosted link only.",
    );
    place["attrs"]["op_kind"] = json!("hosted_payment_link");
    row(e, "base", "operation", "order.place", place).await;
    let mut evaluate = operation(
        "R",
        "observe",
        "nat",
        false,
        None,
        &["rule_id"],
        &["outcome"],
        "Built-in derive read, runs in agent. Stores nothing.",
    );
    evaluate["attrs"]["builtin"] = json!(true);
    evaluate["attrs"]["is_derive"] = json!(true);
    evaluate["attrs"]["op_kind"] = json!("builtin");
    row(e, "base", "operation", "rule.evaluate", evaluate).await;
    row(
        e,
        "base",
        "operation",
        "case.create",
        operation(
            "W",
            "request",
            "key+dup",
            true,
            None,
            &["case_type", "subject"],
            &["case_id", "status"],
            "Open a case.",
        ),
    )
    .await;
    row(
        e,
        "base",
        "operation",
        "case.create{return}",
        json!({
            "discriminator": {"field": "case_type", "value": "return"},
            "attrs": {"input_fields": ["subject_object", "return_lines"], "output_fields": ["case_id", "status", "label"],
                      "guard_rules": ["RET-01"]},
            "description": "Return / RMA; label as a Document.", "retail": "R", "stages": ["Support"],
            "extra_errors": []
        }),
    )
    .await;

    for (code, user_facing, escalate, description) in [
        ("not_found", true, false, "No record matches."),
        (
            "invalid_state",
            true,
            false,
            "The record's status has no transition for this verb.",
        ),
        (
            "identity_required",
            true,
            false,
            "Needs a higher identity level first.",
        ),
        (
            "unknown_outcome",
            false,
            true,
            "Write sent, no answer: verify before retry.",
        ),
        (
            "outside_hours",
            true,
            false,
            "Nobody is available right now.",
        ),
    ] {
        row(
            e,
            "base",
            "error_state",
            code,
            json!({"user_facing": user_facing, "retryable": false, "escalate": escalate,
                   "description": description}),
        )
        .await;
    }
    for family in ["base", "industry/retail", "base_dept/it_service_desk"] {
        tag(e, family).await;
    }
}

// ---- gates and roles ----------------------------------------------------------------------------------------------

fn gate(
    group: &str,
    kind: &str,
    question: &str,
    options: Value,
    rule: Value,
    threshold: &str,
) -> Value {
    json!({
        "group": group, "point": "P1", "type": kind, "question": question, "options": options,
        "generated_from": null, "threshold": threshold, "threshold_value": null, "fail": "open", "safety": false,
        "effect": "", "updated_by": "owner", "updated_at": "2026-10-12T09:00:00Z", "rule": rule,
        "action": "annotate", "tools": null, "origin": "platform"
    })
}

fn option(key: &str, label: &str, generated: bool) -> Value {
    json!({"key": key, "label": label, "description": "", "generated": generated})
}

fn truth() -> Value {
    json!([
        option("true", "true", false),
        option("false", "false", false)
    ])
}

fn gte(threshold: f64) -> Value {
    json!({"fire_when": "gte", "threshold": threshold, "fire_values": null, "min_confidence": null})
}

/// The gate library the T12 tests edit, authored as base rows (and `J1` / `I1` in the layers that own them):
/// `GT` (a tunable choice that fires on `issue` / `need_change`), `GA` (generated options), `B1` (a safety gate),
/// `GC` (a yes/no that is not tunable), `GF` (a 1-5 score). Base is tagged, so a job can pin the library.
pub async fn gates(e: &TestEnv) {
    ensure_layer(
        e,
        "base_dept/it_service_desk",
        json!({"scope": "base_dept", "department": "it_service_desk", "label": "IT service desk"}),
    )
    .await;
    ensure_layer(
        e,
        "industry/utilities",
        json!({"scope": "industry", "industry": "utilities", "label": "Utilities"}),
    )
    .await;
    let mut gt = gate(
        "base",
        "choice",
        "Which theme is the customer in?",
        json!([
            option("need", "Need", false),
            option("interest", "Interest", false),
            option("issue", "Issue", false),
            option("need_change", "Need Change", false),
            option("unclear", "unclear", false)
        ]),
        json!({"fire_when": "in", "threshold": null, "fire_values": ["issue", "need_change"],
               "min_confidence": 0.5}),
        "top ≥ 0.5",
    );
    gt["threshold_value"] = json!(0.5);
    row(e, "base", "gate_question", "GT", gt).await;

    let mut ga = gate(
        "base",
        "choice",
        "Which activity is the customer doing?",
        json!([
            option("support:start_return", "Start a return", true),
            option("delivery:track_parcel", "Track a parcel", true)
        ]),
        gte(0.7),
        "top ≥ 0.7, margin ≥ 0.25",
    );
    ga["threshold_value"] = json!(0.7);
    ga["generated_from"] = json!("the enabled activities of the top-2 stages");
    row(e, "base", "gate_question", "GA", ga).await;

    let mut b1 = gate(
        "safety",
        "noul",
        "Does the message describe an emergency or a risk to someone’s life or safety?",
        truth(),
        json!({"fire_when": "gte", "threshold": 0.5, "fire_values": null, "min_confidence": 0.7}),
        "any true",
    );
    b1["fail"] = json!("closed");
    b1["safety"] = json!(true);
    b1["action"] = json!("handover");
    row(e, "base", "gate_question", "B1", b1).await;

    row(
        e,
        "base",
        "gate_question",
        "GC",
        gate(
            "base",
            "noul",
            "Is this a complaint about an outcome (late, wrong, damaged, overcharged)?",
            truth(),
            gte(0.5),
            "any true",
        ),
    )
    .await;

    let mut gf = gate(
        "base",
        "score",
        "How frustrated is the customer?",
        json!([
            option("1", "1 · calm", false),
            option("2", "2 · mild", false),
            option("3", "3 · annoyed", false),
            option("4", "4 · frustrated", false),
            option("5", "5 · angry", false)
        ]),
        gte(4.0),
        "≥ 4 (twice → handover)",
    );
    gf["type"] = json!("score");
    gf["threshold_value"] = json!(4);
    row(e, "base", "gate_question", "GF", gf).await;

    row(
        e,
        "base_dept/it_service_desk",
        "gate_question",
        "J1",
        gate(
            "itsd",
            "choice",
            "Request type",
            json!([
                option("password_access", "password/access", false),
                option("other", "other", false)
            ]),
            gte(0.6),
            "top ≥ 0.6",
        ),
    )
    .await;
    row(
        e,
        "industry/utilities",
        "gate_question",
        "I1",
        gate(
            "utilities",
            "noul",
            "Does the user mention a disconnection notice or a shut-off?",
            truth(),
            gte(0.5),
            "any true",
        ),
    )
    .await;
    tag(e, "base").await;
}

const OPDEFS: &str = include_str!("../../contracts/opdefs.json");

/// The identity level the operation contract (`contracts/opdefs.json`) fixes for `key` (`object.verb`): authoring
/// refuses an operation below it.
fn contract_identity_level(key: &str) -> Value {
    let (object, verb) = key.split_once('.').expect("object.verb");
    let defs: Vec<Value> = serde_json::from_str(OPDEFS).expect("contracts/opdefs.json");
    defs.into_iter()
        .find(|d| d["object"] == object && d["verb"] == verb && d["variant"] == "*")
        .map(|d| d["identity_level"].clone())
        .unwrap_or_else(|| json!("2"))
}

/// An operation row for the role tests: side effect `side`, otherwise neutral.
pub async fn role_operations(e: &TestEnv) {
    for (key, side, modality) in [
        ("order.read", "R", "observe"),
        ("delivery.read", "R", "observe"),
        ("party.read", "R", "observe"),
        ("case.comment", "W", "do"),
        ("access_grant.revoke", "X", "do"),
    ] {
        let mut op = operation(
            side,
            modality,
            "nat",
            false,
            None,
            &[],
            &[],
            "A role-test operation.",
        );
        op["attrs"]["identity_level"] = contract_identity_level(key);
        row(e, "base", "operation", key, op).await;
    }
}

/// The platform agent roles (base rows) plus the operations the grant tests name; the `customer_care_agent` job
/// layer already exists (the offering).
pub async fn roles(e: &TestEnv) {
    role_operations(e).await;
    let role = |label: &str, max: &str, enabled: bool, grants: Value| {
        json!({"label": label, "used_by": "test", "max_side_effect": max, "enabled": enabled, "grants": grants,
               "note": ""})
    };
    row(
        e,
        "base",
        "agent_role",
        "assistant",
        role("Assistant", "W", false, json!([])),
    )
    .await;
    row(
        e,
        "base",
        "agent_role",
        "post_gate",
        role("Post-gate", "R", true, json!([])),
    )
    .await;
    row(
        e,
        "base",
        "agent_role",
        "procedure",
        role(
            "Procedure",
            "P",
            true,
            json!([{"object": "*", "verb": "*", "variant": null, "modalities": ["do", "request", "observe"],
                    "derive_from_activity": true}]),
        ),
    )
    .await;
    row(
        e,
        "base",
        "agent_role",
        "qualifier",
        role("Qualifier", "R", true, json!([])),
    )
    .await;
}

// ---- the business-model vocabulary --------------------------------------------------------------------------------

/// An axis (and its values, `(key, parent)`) in the vocabulary tables. Written through the owner pool, not
/// `POST /api/axes`: the tests count the `business_model.axis_create` / `axis_value_add` audit rows their own routes
/// write.
pub async fn axis(
    e: &TestEnv,
    key: &str,
    cardinality: &str,
    object: &str,
    milestone: &str,
    values: &[(&str, Option<&str>)],
) {
    sqlx::query(
        "INSERT INTO axis (key, label, description, cardinality, object, milestone) VALUES ($1, $1, '', $2, $3, $4)",
    )
    .bind(key)
    .bind(cardinality)
    .bind(object)
    .bind(milestone)
    .execute(&e.owner)
    .await
    .unwrap_or_else(|err| panic!("axis {key}: {err}"));
    for (value, parent) in values {
        sqlx::query(
            "INSERT INTO axis_value (axis, key, label, parent, added_by) VALUES ($1, $2, $2, $3, 'test')",
        )
        .bind(key)
        .bind(value)
        .bind(parent)
        .execute(&e.owner)
        .await
        .unwrap_or_else(|err| panic!("axis value {key}·{value}: {err}"));
    }
}

/// A dimension (and its value keys) in the vocabulary tables (owner pool, see [`axis`]); idempotent, the test
/// platform already holds `region` and `channel`.
pub async fn dimension(e: &TestEnv, key: &str, kind: &str, values: &[&str]) {
    sqlx::query(
        "INSERT INTO dimension (key, label, kind) VALUES ($1, $1, $2) ON CONFLICT (key) DO NOTHING",
    )
    .bind(key)
    .bind(kind)
    .execute(&e.owner)
    .await
    .unwrap_or_else(|err| panic!("dimension {key}: {err}"));
    for value in values {
        sqlx::query(
            "INSERT INTO dimension_value (dimension, key, label, added_by) VALUES ($1, $2, $2, 'test') ON CONFLICT DO NOTHING",
        )
        .bind(key)
        .bind(value)
        .execute(&e.owner)
        .await
        .unwrap_or_else(|err| panic!("dimension value {key}·{value}: {err}"));
    }
}

/// The business vocabulary the T10 / T12e / T12g tests use: the catalogue objects the axes classify (`order`,
/// `contract`, `party`, `offering`, `payment`; the entity `Renewal`), the axes `contract_type`, `customer_type`,
/// `order_type`, `payment_method`, `vulnerability`, and the dimensions `area` and `channel`.
pub async fn vocabulary(e: &TestEnv) {
    for (key, label) in [
        ("order", "Order"),
        ("contract", "Contract"),
        ("party", "Party"),
        ("offering", "Offering"),
        ("payment", "Payment"),
    ] {
        row(e, "base", "object", key, bare_object(label)).await;
    }
    row(e, "base", "entity", "Renewal", json!({"label": "Renewal"})).await;
    axis(
        e,
        "contract_type",
        "one",
        "Contract",
        "M4",
        &[
            ("sale", None),
            ("service", None),
            ("lease", None),
            ("subscription", None),
            ("mixed", None),
        ],
    )
    .await;
    axis(
        e,
        "customer_type",
        "many",
        "Party",
        "M1",
        &[
            ("natural_person", None),
            ("household", None),
            ("corporation", None),
        ],
    )
    .await;
    axis(
        e,
        "order_type",
        "one",
        "Order",
        "M1",
        &[
            ("discrete", None),
            ("recurring", None),
            ("continuous", None),
        ],
    )
    .await;
    axis(
        e,
        "payment_method",
        "many",
        "Payment",
        "M1",
        &[
            ("cash", None),
            ("card", None),
            ("card.debit", Some("card")),
            ("card.credit", Some("card")),
        ],
    )
    .await;
    axis(
        e,
        "vulnerability",
        "one",
        "Party",
        "M1",
        &[("none", None), ("potential", None)],
    )
    .await;
    dimension(e, "area", "grouping", &["order-support", "logistics"]).await;
    dimension(e, "channel", "dimension", &["chat", "email"]).await;
}
