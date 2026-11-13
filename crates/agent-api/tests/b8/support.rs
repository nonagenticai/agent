//! B8's own tiny platform content, authored through the platform routes as super_admin (no boot seeder: a fresh
//! database holds only what `common::platform::offering` made). A test that needs a journey (stages, a theme, a
//! channel, an activity bound to an object and an operation) calls [`content`]; the template that carries it is
//! [`content`]'s return value (a job created from `job/customer_care_agent@v4` has none of it: the tags it pins
//! predate these rows), so a test that creates a job from the template uses [`new_job`] / [`template`].
//!
//! What [`content`] makes (identifiers, not product content):
//!
//! - `base`: objects `order` and `delivery`, operations `order.read`, `order.cancel`, `delivery.read` (+ variant
//!   `by_tracking_ref`), theme `issue`, stages `support`, `delivery`, `change`, channel `chat` (status `v1`);
//! - `industry_dept/retail/customer_service`: activities `support:return` (required; summary and description),
//!   `delivery:track`, `delivery:where_is_my_order` (required, with ops), `change:cancel_order`, the context space
//!   `retail:order-support:order_status` (serves `where_is_my_order`) and its procedure `order_status@2` (one `order.read`); `industry/retail`: the prior `issue|support`;
//! - `base`, `industry/retail` and the department are tagged again, and the job layer gets one more setting and a
//!   tag (`@v5`), so the newest tag of `job/customer_care_agent` pins all of it.
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::common::design_contract::{self, ContractJob};
use crate::common::platform::{self, DEPARTMENT_LAYER, INDUSTRY_LAYER, JOB_LAYER};
use crate::common::{TestEnv, json as req, send, super_admin};

async fn sa(e: &TestEnv, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, req(method, uri, Some(&super_admin()), body)).await
}

/// The id of layer `key`.
pub async fn layer_id(e: &TestEnv, key: &str) -> String {
    sqlx::query_scalar::<_, String>("SELECT id::text FROM layer WHERE key = $1")
        .bind(key)
        .fetch_one(&e.owner)
        .await
        .unwrap_or_else(|x| panic!("layer {key}: {x}"))
}

/// Add one row (`op: add`) to layer `layer`; a row already there is kept. Panics on any other refusal.
pub async fn add(e: &TestEnv, layer: &str, kind: &str, key: &str, fields: Value) {
    let id = layer_id(e, layer).await;
    let (s, b) = sa(
        e,
        "POST",
        &format!("/api/layers/{id}/rows"),
        json!({"kind": kind, "key": key, "op": "add", "fields": fields}),
    )
    .await;
    let dup = s == StatusCode::CONFLICT && b["error"]["code"] == "duplicate_add";
    assert!(
        s == StatusCode::CREATED || dup,
        "add {kind} {key} to {layer}: {s} {b}"
    );
}

/// Tag family `layer` (a layer key); nothing to tag is fine.
pub async fn tag(e: &TestEnv, layer: &str) {
    let (s, b) = sa(
        e,
        "POST",
        &format!("/api/platform/templates/{}/tags", layer.replace('/', ".")),
        json!({}),
    )
    .await;
    let nothing = s == StatusCode::CONFLICT && b["error"]["code"] == "nothing_to_tag";
    assert!(s == StatusCode::CREATED || nothing, "tag {layer}: {s} {b}");
}

/// The `operation` row fields of a read op with the given inputs and outputs.
pub fn read_op(inputs: &[&str], outputs: &[&str]) -> Value {
    json!({
        "attrs": {
            "modality": "observe", "is_derive": false, "side_effect": "R", "idempotency": "nat",
            "confirm_back": false, "builtin": false, "op_kind": "standard", "guard_rules": [],
            "identity_level": "2", "input_fields": inputs, "output_fields": outputs
        },
        "description": "Read one record.", "stages": [], "extra_errors": []
    })
}

/// An `object` row with the given (field, type) pairs (all required) and no states.
pub fn object(label: &str, fields: &[(&str, &str)]) -> Value {
    let fields: Vec<Value> = fields
        .iter()
        .map(|(f, t)| json!({"field": f, "data_type": t, "required": true}))
        .collect();
    json!({"label": label, "tier": "base", "fields": fields})
}

type Cells = Mutex<HashMap<String, std::sync::Arc<OnceCell<(String, i32)>>>>;

fn cells() -> &'static Cells {
    static CELLS: OnceLock<Cells> = OnceLock::new();
    CELLS.get_or_init(Cells::default)
}

/// Author the B8 journey content (once per database) and return the template that pins it: (layer, version).
pub async fn content(e: &TestEnv) -> (String, i32) {
    platform::offering(e).await;
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
    cell.get_or_init(|| build(e)).await.clone()
}

async fn build(e: &TestEnv) -> (String, i32) {
    add(
        e,
        "base",
        "object",
        "order",
        object("Order", &[("order_id", "id"), ("status", "string")]),
    )
    .await;
    add(
        e,
        "base",
        "object",
        "delivery",
        object(
            "Delivery",
            &[("tracking_ref", "string"), ("status", "string")],
        ),
    )
    .await;
    add(
        e,
        "base",
        "operation",
        "order.read",
        read_op(&["order_id"], &["status"]),
    )
    .await;
    add(
        e,
        "base",
        "operation",
        "delivery.read",
        read_op(&["order_id"], &["status"]),
    )
    .await;
    add(
        e,
        "base",
        "operation",
        "delivery.read{by_tracking_ref}",
        json!({
            "discriminator": {"field": "lookup", "value": "by_tracking_ref"},
            "attrs": {"identity_level": "1", "input_fields": ["tracking_ref"], "output_fields": ["status"]},
            "description": "Read one delivery by its tracking reference.", "stages": [], "extra_errors": []
        }),
    )
    .await;
    add(
        e,
        "base",
        "operation",
        "order.cancel",
        json!({
            "attrs": {
                "modality": "do", "is_derive": false, "side_effect": "W", "idempotency": "key",
                "confirm_back": true, "verify_read": "order.read", "builtin": false, "op_kind": "standard",
                "guard_rules": [], "identity_level": "2", "input_fields": ["order_id"], "output_fields": ["status"]
            },
            "description": "Cancel one order.", "stages": [], "extra_errors": []
        }),
    )
    .await;
    // the platform's procedure role: its toolset is the activity's operations (derive_from_activity), so a call outside
    // the activity's grant is refused (without a role row the design governs nothing and defers to B4)
    add(
        e,
        "base",
        "agent_role",
        "procedure",
        json!({"label": "Procedure", "used_by": "test", "max_side_effect": "P", "enabled": true, "note": "",
               "grants": [{"object": "*", "verb": "*", "variant": null,
                           "modalities": ["do", "request", "observe"], "derive_from_activity": true}]}),
    )
    .await;
    add(
        e,
        "base",
        "journey_theme",
        "issue",
        json!({"label": "Issue", "order": 1, "escalation_weight": 0.6}),
    )
    .await;
    add(
        e,
        "base",
        "journey_stage",
        "support",
        json!({"label": "Support", "order": 1, "description": "Questions and problems"}),
    )
    .await;
    add(
        e,
        "base",
        "journey_stage",
        "delivery",
        json!({"label": "Delivery", "order": 2, "description": "Shipping and tracking"}),
    )
    .await;
    add(
        e,
        "base",
        "journey_stage",
        "change",
        json!({"label": "Change", "order": 3, "description": "Changing an order"}),
    )
    .await;
    add(
        e,
        "base",
        "journey_channel",
        "chat",
        json!({"label": "Chat", "status": "v1"}),
    )
    .await;
    let order =
        json!({"object": "order", "role": "subject", "fields": ["status"], "required_inputs": []});
    add(
        e,
        DEPARTMENT_LAYER,
        "activity",
        "support:return",
        json!({
            "label": "Return an item", "actor": "customer", "trigger": "asks to send an item back",
            "summary": "customer · order.read", "description": "Send an item back", "required": true,
            "objects": [order],
            "operations": [{"op": "order.read", "required": true}]
        }),
    )
    .await;
    add(
        e,
        DEPARTMENT_LAYER,
        "activity",
        "delivery:track",
        json!({"label": "Track a parcel", "actor": "customer", "trigger": "asks where a parcel is"}),
    )
    .await;
    add(
        e,
        DEPARTMENT_LAYER,
        "activity",
        "delivery:where_is_my_order",
        json!({
            "label": "Where is my order", "actor": "customer", "trigger": "asks where an order is", "required": true,
            "objects": [order, {"object": "delivery", "role": "context", "fields": ["tracking_ref"], "required_inputs": []}],
            "operations": [
                {"op": "order.read", "required": true},
                {"op": "delivery.read", "variant": "by_tracking_ref", "required": true}
            ]
        }),
    )
    .await;
    add(
        e,
        DEPARTMENT_LAYER,
        "activity",
        "change:cancel_order",
        json!({
            "label": "Cancel an order", "actor": "customer", "trigger": "asks to cancel an order",
            "objects": [order],
            "operations": [{"op": "order.read", "required": true}, {"op": "order.cancel", "required": false}]
        }),
    )
    .await;
    add(
        e,
        DEPARTMENT_LAYER,
        "context_space",
        "retail:order-support:order_status",
        json!({
            "label": "Order status", "area": "order-support", "knowledge_kinds": [], "slots": ["order_id"],
            "activity": "delivery:where_is_my_order"
        }),
    )
    .await;
    // the procedure the order-status space runs: one read of the order (the draft-validation probes answer from it)
    add(
        e,
        DEPARTMENT_LAYER,
        "procedure",
        "order_status@2",
        json!({
            "label": "Order status", "context_space": "retail:order-support:order_status",
            "description": "Read the order, then say where it is.",
            "steps": [{
                "step_number": 1, "phase": 0, "name": "order", "operation": "order.read", "variant": null,
                "modality": "observe", "side_effect": "R", "identity_level": "2", "confirm_back": false,
                "gate": false, "args": {"order_id": "$order_id"}, "preconditions": [], "postconditions": [],
                "performed_by": null, "max_per_case": null,
            }],
            "success_criteria": ["order"],
            "tool_requirements": ["order.read"],
        }),
    )
    .await;
    // the industry layer: the designer's `setThemeStagePrior` writes and retires there (a department row would
    // override its value)
    add(
        e,
        INDUSTRY_LAYER,
        "theme_stage_prior",
        "issue|support",
        json!({"label": "Issue to Support", "weight": 0.6}),
    )
    .await;
    for family in ["base", INDUSTRY_LAYER, DEPARTMENT_LAYER] {
        tag(e, family).await;
    }
    add(
        e,
        JOB_LAYER,
        "job_setting",
        "content_note",
        json!({"label": "Content note"}),
    )
    .await;
    tag(e, JOB_LAYER).await;
    design_contract::latest_template(e).await
}

/// A new tenant and a job created from [`content`]'s template (`design_contract::new_job` on the newest tag).
pub async fn new_job(e: &TestEnv) -> ContractJob {
    let t = content(e).await;
    let f = design_contract::new_job_for_on(e, Uuid::new_v4(), t).await;
    route_content_ops(e, &f).await;
    f
}

/// Route the operations [`content`]'s activities name on Production (the routing publish gate, R5): the job's
/// selection folds `order.read`, `order.cancel` and `delivery.read{by_tracking_ref}` (the `*` row's route serves the
/// variant). `design_contract` only routes what the retired seed's activities named. Idempotent.
pub async fn route_content_ops(e: &TestEnv, f: &ContractJob) {
    for (object, verb) in [("order", "read"), ("order", "cancel"), ("delivery", "read")] {
        sqlx::query(
            "INSERT INTO operation_route (tenant_id, job_id, object, verb, variant, env, target_kind, updated_by) \
             VALUES ($1, $2, $3, $4, '*', 'prod', 'agent_cases', 'test') \
             ON CONFLICT (tenant_id, job_id, object, verb, variant, env) DO NOTHING",
        )
        .bind(f.tenant)
        .bind(f.job.job_id)
        .bind(object)
        .bind(verb)
        .execute(&e.owner)
        .await
        .unwrap();
    }
}
