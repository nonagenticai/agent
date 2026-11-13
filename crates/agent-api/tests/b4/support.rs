//! The platform content the B4 runtime tests need on top of B5's (`tests/b5/support.rs`, included as `b5_support`),
//! authored through the platform's own routes as super_admin. There is no seeded content: a test that drives a
//! delivery complaint, a refund question or a customer's record lists makes the rows it needs here.
//!
//! - [`template`]: B5's job template plus, once per database, a second tag holding what the orchestrator flows run:
//!   the customer-list operations the context phase reads (`order.list`, `payment.list`, `return.list`), the
//!   delivery-problem operation, two more activities (a delivery complaint, a refund question), their spaces
//!   (`retail:logistics:delivery_problem`, `retail:payments:refunds`) and procedures (`delivery_problem@1`,
//!   `refund_status@2`; the built-in step lists the runtime knows are keyed by these names).
//! - [`ops`]: catalogue operations (from `contracts/opdefs.json`) a test's own job layer needs, e.g. a record kind's
//!   list read; call it BEFORE the job layer is tagged (a job tag pins the base tag that exists then).
//! - [`error_states`]: the base layer's error taxonomy (which outcome codes are user-facing); part of [`template`].
//! - [`route_prod`] / [`route_test`]: the `operation_route` rows a job needs (the routing publish gate on Production;
//!   the working rows the orchestrator reads in the test environment).
//!
//! Names are identifiers of this test world, not content. Everything here is tiny and neutral.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::b4::b5_support as b5;
use crate::common::design_contract::{self, ContractJob};
use crate::common::platform;
use crate::common::{TestEnv, json as req, send, super_admin};

const OPDEFS: &str = include_str!("../../contracts/opdefs.json");

pub const ACTIVITY_DELIVERY_PROBLEM: &str = "delivery:delivery_problem";
pub const ACTIVITY_REFUND_STATUS: &str = "payments:refund_status";

/// The operations the rich template adds to B5's catalogue. Each is authored at the identity level the operation
/// contract (`contracts/opdefs.json`) declares for it ([`op_row`]), so a test that runs one runs the real level.
const EXTRA_OPS: [&str; 9] = [
    "offering.read",
    "party.read",
    "party.list",
    "order.list",
    "payment.list",
    "return.list",
    "delivery.report_problem",
    "claim.read",
    "contact_point.update",
];

type Cells = Mutex<HashMap<String, Arc<OnceCell<i32>>>>;

fn cells() -> &'static Cells {
    static CELLS: OnceLock<Cells> = OnceLock::new();
    CELLS.get_or_init(Cells::default)
}

async fn sa(e: &TestEnv, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, req(method, uri, Some(&super_admin()), body)).await
}

async fn layer_id(e: &TestEnv, key: &str) -> String {
    sqlx::query_scalar("SELECT id::text FROM layer WHERE key = $1")
        .bind(key)
        .fetch_one(&e.owner)
        .await
        .unwrap_or_else(|x| panic!("layer {key}: {x}"))
}

/// Add one row; a row already there is kept. Returns whether it was new.
async fn add(e: &TestEnv, layer: &str, kind: &str, key: &str, fields: Value) -> bool {
    let (s, b) = sa(
        e,
        "POST",
        &format!("/api/layers/{layer}/rows"),
        json!({"kind": kind, "key": key, "op": "add", "fields": fields}),
    )
    .await;
    if s == StatusCode::CONFLICT && b["error"]["code"] == "duplicate_add" {
        return false;
    }
    assert_eq!(s, StatusCode::CREATED, "{kind} {key}: {b}");
    true
}

/// Tag family `family`; nothing new to tag is fine.
async fn tag(e: &TestEnv, family: &str) {
    let (s, b) = sa(
        e,
        "POST",
        &format!("/api/platform/templates/{}/tags", family.replace('/', ".")),
        json!({}),
    )
    .await;
    let nothing = s == StatusCode::CONFLICT && b["error"]["code"] == "nothing_to_tag";
    assert!(s == StatusCode::CREATED || nothing, "tag {family}: {s} {b}");
}

/// The `*` row of catalogue operation `key` (`object.verb`) as the contract catalogue defines it.
fn op_row(key: &str) -> Value {
    let (object, verb) = key.split_once('.').expect("object.verb");
    let defs: Vec<Value> = serde_json::from_str(OPDEFS).expect("contracts/opdefs.json");
    let d = defs
        .into_iter()
        .find(|d| d["object"] == object && d["verb"] == verb && d["variant"] == "*")
        .unwrap_or_else(|| panic!("{key} is not in contracts/opdefs.json"));
    let side = d["side_effect"].as_str().unwrap().to_owned();
    let (modality, idem, confirm) = match side.as_str() {
        "R" => ("observe", "nat", false),
        "W" => ("request", "key+dup", true),
        other => panic!("{key}: side effect {other} is not one a B4 test authors"),
    };
    json!({
        "attrs": {
            "modality": modality, "is_derive": false, "side_effect": side, "idempotency": idem,
            "confirm_back": confirm, "verify_read": null, "builtin": false, "op_kind": "standard",
            "guard_rules": [], "identity_level": d["identity_level"],
            "input_fields": d["input_fields"], "output_fields": d["output_fields"],
        },
        "description": format!("{side} operation of the test catalogue."),
    })
}

/// Author catalogue operations `keys` (`object.verb`) on the base layer, then tag it when any was new: a job layer
/// tagged AFTER this call pins them. Idempotent.
pub async fn ops(e: &TestEnv, keys: &[&str]) {
    platform::offering(e).await;
    let base = layer_id(e, "base").await;
    let mut any = false;
    for key in keys {
        any |= add(e, &base, "operation", key, op_row(key)).await;
    }
    if any {
        tag(e, "base").await;
    }
}

/// The error taxonomy the console classes outcome codes by (`error_state` rows on the base layer; a job reads the
/// taxonomy its pinned template compiled, so [`template`] authors it before it tags; a connector's insight reads the
/// head): `not_found` and `identity_required` are shown to the customer, `timeout` and
/// `unknown_outcome` go to a colleague.
pub async fn error_states(e: &TestEnv) {
    platform::offering(e).await;
    let base = layer_id(e, "base").await;
    for (code, user_facing, description) in [
        ("not_found", true, "No record matches."),
        (
            "identity_required",
            true,
            "The customer must be identified first.",
        ),
        ("timeout", false, "The system did not answer in time."),
        (
            "unknown_outcome",
            false,
            "A write may or may not have landed.",
        ),
    ] {
        add(
            e,
            &base,
            "error_state",
            code,
            json!({"user_facing": user_facing, "retryable": !user_facing && code == "timeout",
                   "escalate": !user_facing, "description": description}),
        )
        .await;
    }
}

fn step(n: u32, phase: u32, name: &str, op: &str, side: &str, args: Value) -> Value {
    let modality = if side == "R" { "observe" } else { "request" };
    json!({
        "step_number": n, "phase": phase, "name": name, "operation": op, "variant": null,
        "modality": modality, "side_effect": side, "identity_level": "2", "confirm_back": side == "W",
        "gate": false, "args": args, "preconditions": [], "postconditions": [],
        "performed_by": null, "max_per_case": null,
    })
}

fn ops_of(list: &[(&str, bool)]) -> Value {
    Value::Array(
        list.iter()
            .map(|(op, required)| json!({"op": op, "required": required}))
            .collect(),
    )
}

async fn author(e: &TestEnv, o: platform::Offering) {
    let dept = layer_id(e, o.department).await;
    let job = layer_id(e, o.job_layer).await;
    ops(e, &EXTRA_OPS).await;
    error_states(e).await;
    // the department offers every area the template's spaces name (the offering names the first only): a selection
    // may choose them
    sqlx::query("UPDATE layer SET areas = $2 WHERE key = $1")
        .bind(o.department)
        .bind(vec!["order-support", "logistics", "payments", "faq"])
        .execute(&e.owner)
        .await
        .unwrap();

    add(
        e,
        &dept,
        "activity",
        ACTIVITY_DELIVERY_PROBLEM,
        json!({"label": "Report a delivery problem", "actor": "customer", "operations": ops_of(&[
            ("order.read", true),
            ("delivery.report_problem", true),
            ("delivery.read", true),
        ])}),
    )
    .await;
    add(
        e,
        &dept,
        "activity",
        ACTIVITY_REFUND_STATUS,
        json!({"label": "Where is my refund", "actor": "customer", "operations": ops_of(&[
            ("payment.list", true),
            ("order.read", false),
            ("return.list", false),
        ])}),
    )
    .await;
    for (key, label, area, slots, activity) in [
        (
            "retail:logistics:delivery_problem",
            "Delivery problem",
            "logistics",
            vec!["order_id", "problem"],
            ACTIVITY_DELIVERY_PROBLEM,
        ),
        (
            "retail:payments:refunds",
            "Refunds",
            "payments",
            vec!["order_id"],
            ACTIVITY_REFUND_STATUS,
        ),
    ] {
        add(
            e,
            &dept,
            "context_space",
            key,
            json!({"label": label, "area": area, "slots": slots, "activity": activity}),
        )
        .await;
    }
    add(
        e,
        &dept,
        "procedure",
        "delivery_problem@1",
        json!({
            "label": "Delivery problem", "context_space": "retail:logistics:delivery_problem",
            "description": "Report the problem, read the shipment, hand over a late parcel.",
            "steps": [
                step(1, 0, "order", "order.read", "R", json!({"order_id": "$order_id"})),
                step(2, 1, "report", "delivery.report_problem", "W",
                     json!({"order_id": "$order_id", "problem": "$problem"})),
                step(3, 2, "delivery", "delivery.read", "R", json!({"order_id": "$order_id"})),
            ],
            "success_criteria": ["delivery"],
            "tool_requirements": ["order.read", "delivery.report_problem", "delivery.read"],
        }),
    )
    .await;
    add(
        e,
        &dept,
        "procedure",
        "refund_status@2",
        json!({
            "label": "Refund status", "context_space": "retail:payments:refunds",
            "description": "Read the customer's payments and say where the refund is.",
            "steps": [step(1, 0, "payments", "payment.list", "R", json!({"order_id": "$order_id"}))],
            "success_criteria": ["payments"],
            "tool_requirements": ["payment.list"],
        }),
    )
    .await;
    add(
        e,
        &job,
        "job_setting",
        "b4_content",
        json!({"label": "B4 test content"}),
    )
    .await;
    for family in [o.base, o.department, o.job_layer] {
        tag(e, family).await;
    }
}

/// The newest tagged version of `layer` (None when it has none).
async fn newest_tag(e: &TestEnv, layer: &str) -> Option<i32> {
    sqlx::query_scalar(
        "SELECT max(version) FROM job_template_tag WHERE layer = $1 AND status = 'tagged'",
    )
    .bind(layer)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

/// The job template (layer, version): B5's with the B4 content on top, authored once per database.
pub async fn template(e: &TestEnv) -> (String, i32) {
    let (layer, b5_version) = b5::template(e).await;
    let o = platform::offering(e).await;
    let db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let cell = cells()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(db)
        .or_default()
        .clone();
    let v = *cell
        .get_or_init(|| async {
            if newest_tag(e, o.job_layer)
                .await
                .is_none_or(|v| v <= b5_version)
            {
                author(e, o).await;
            }
            newest_tag(e, o.job_layer)
                .await
                .expect("a tagged job template")
        })
        .await;
    (layer, v)
}

/// A new contract job (own tenant) on [`template`].
pub async fn new_job(e: &TestEnv) -> ContractJob {
    let t = template(e).await;
    design_contract::new_job_for_on(e, Uuid::new_v4(), t).await
}

async fn route(e: &TestEnv, f: &ContractJob, env: &str, rows: &[(&str, &str, &str)]) {
    for (object, verb, kind) in rows {
        sqlx::query(
            "INSERT INTO operation_route (tenant_id, job_id, object, verb, variant, env, target_kind, updated_by) \
             VALUES ($1, $2, $3, $4, '*', $5, $6, 'test') \
             ON CONFLICT (tenant_id, job_id, object, verb, variant, env) DO NOTHING",
        )
        .bind(f.tenant)
        .bind(f.job.job_id)
        .bind(object)
        .bind(verb)
        .bind(env)
        .bind(kind)
        .execute(&e.owner)
        .await
        .unwrap();
    }
}

/// Every operation the template's activities name, routed on Production (the routing publish gate, R5): B5's, plus
/// the delivery-problem report and the customer lists.
pub async fn route_prod(e: &TestEnv, f: &ContractJob) {
    b5::route_activity_ops(e, f).await;
    route(
        e,
        f,
        "prod",
        &[
            ("delivery", "report_problem", "agent_cases"),
            ("payment", "list", "agent_cases"),
            ("return", "list", "agent_cases"),
            ("order", "list", "agent_cases"),
        ],
    )
    .await;
}

/// What the orchestrator flows run in the test environment: `order.read` and `case.create` are routed (to the case
/// queue; a fake runner answers every call, so the target is not exercised). The delivery operations are NOT routed:
/// a delivery complaint on these routes hands over `not_offered` before any call, until a test offers them.
pub async fn route_test(e: &TestEnv, f: &ContractJob) {
    route(
        e,
        f,
        "test",
        &[
            ("order", "read", "agent_cases"),
            ("case", "create", "agent_cases"),
        ],
    )
    .await;
}

/// Working `operation_route` rows of `f`'s job in the test environment sending each `(object, verb)` to the tenant's
/// instance `instance` (replacing a route already there).
pub async fn route_instance(e: &TestEnv, f: &ContractJob, instance: &str, rows: &[(&str, &str)]) {
    for (object, verb) in rows {
        sqlx::query(
            "INSERT INTO operation_route (tenant_id, job_id, object, verb, variant, env, target_kind, instance_id, updated_by) \
             VALUES ($1, $2, $3, $4, '*', 'test', 'instance', $5, 'test') \
             ON CONFLICT (tenant_id, job_id, object, verb, variant, env) \
             DO UPDATE SET target_kind = 'instance', instance_id = EXCLUDED.instance_id",
        )
        .bind(f.tenant)
        .bind(f.job.job_id)
        .bind(object)
        .bind(verb)
        .bind(instance)
        .execute(&e.owner)
        .await
        .unwrap();
    }
}
