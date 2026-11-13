//! The platform content the B5 tests need beyond the bare offering (`common::platform::offering`), authored through
//! the platform's own routes as super_admin (`POST /api/layers/{id}/rows`, `POST /api/platform/templates/{layer}/tags`):
//! there is no boot seeder and no seeded content, so a test that reads a gate question, an entity, an agent role, an
//! activity, a context space or a procedure makes the few it needs here.
//!
//! What [`template`] adds on top of the offering (one extra tag per family, once per database) and why:
//!
//! - base: the entities `Order`, `Delivery`, `Case` with attributes and a relation (ontology tests); the operations the
//!   tests name (`order.read`, `order.cancel`, `delivery.read{by_tracking_ref}`, ...: side effects and identity levels
//!   come from the catalogue); the gate library (a safety noul `B1`, the language choice `B6`, a plain noul `GC`, a
//!   library choice with a gte rule `GM`, a post choice with an `in` rule `F1`, a score `GU`, a generated choice
//!   `GA`); the four agent roles (`procedure`, `qualifier`, `post_gate`, and the platform-disabled `assistant`);
//! - the department layer: three activities with their operations, five context spaces (four mapped to an activity,
//!   one unmapped) and two base procedures with step bodies;
//! - the job layer: one setting, so the job family can be tagged against the new stack.
//!
//! Names are identifiers of this test world, not content: nothing here names a brand. Everything is tiny and neutral.
//!
//! [`new_job`] / [`new_job_for`] create a job on that template; [`publish`] routes every operation the activities
//! name on Production (the routing publish gate, R5) and then publishes with `design_contract::publish`.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::common::design_contract::{self, ContractJob};
use crate::common::platform;
use crate::common::{TestEnv, json as req, send, super_admin};

/// A context space the template authors: key, label, area, required slots, activity.
type SpaceSpec = (
    &'static str,
    &'static str,
    &'static str,
    &'static [&'static str],
    Option<&'static str>,
);
pub const ACTIVITY_WHERE: &str = "delivery:where_is_my_order";
pub const ACTIVITY_RETURN: &str = "support:return";
pub const ACTIVITY_CANCEL: &str = "change:cancel_order";

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

async fn add(e: &TestEnv, layer: &str, kind: &str, key: &str, fields: Value) {
    let (s, b) = sa(
        e,
        "POST",
        &format!("/api/layers/{layer}/rows"),
        json!({"kind": kind, "key": key, "op": "add", "fields": fields}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{kind} {key}: {b}");
}

async fn tag(e: &TestEnv, family: &str) {
    let (s, b) = sa(
        e,
        "POST",
        &format!("/api/platform/templates/{}/tags", family.replace('/', ".")),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "tag {family}: {b}");
}

// ---- the rows ---------------------------------------------------------------------------------------------------

/// The `*` row of an operation: every column a base operation row must set.
#[allow(clippy::too_many_arguments)]
fn op(
    modality: &str,
    side: &str,
    idem: &str,
    confirm: bool,
    verify: Option<&str>,
    identity: &str,
    inputs: &[&str],
    outputs: &[&str],
) -> Value {
    json!({
        "attrs": {
            "modality": modality, "is_derive": false, "side_effect": side, "idempotency": idem,
            "confirm_back": confirm, "verify_read": verify, "builtin": false, "op_kind": "standard",
            "guard_rules": [], "identity_level": identity, "input_fields": inputs, "output_fields": outputs,
        },
        "description": format!("{side} operation of the test catalogue."),
    })
}

fn variant(field: &str, value: &str, attrs: Value) -> Value {
    json!({
        "discriminator": {"field": field, "value": value},
        "attrs": attrs,
        "description": format!("{value} variant."),
    })
}

fn operations() -> Vec<(&'static str, Value)> {
    let parcel = [
        "status",
        "carrier",
        "tracking_ref",
        "tracking_url",
        "expected_window",
        "address",
        "events",
    ];
    let mut rule = op(
        "observe",
        "R",
        "nat",
        false,
        None,
        "2",
        &["rule_id", "subject_object"],
        &["outcome", "reason", "options"],
    );
    rule["attrs"]["builtin"] = json!(true);
    rule["attrs"]["is_derive"] = json!(true);
    rule["attrs"]["op_kind"] = json!("builtin");
    vec![
        (
            "knowledge.search",
            op(
                "observe",
                "R",
                "nat",
                false,
                None,
                "0",
                &["query", "kind", "store"],
                &["passages", "citations"],
            ),
        ),
        (
            "order.read",
            op(
                "observe",
                "R",
                "nat",
                false,
                None,
                "2",
                &["order_id"],
                &[
                    "status",
                    "lines",
                    "total",
                    "delivery_type",
                    "payment_method_ref",
                ],
            ),
        ),
        (
            "order.cancel",
            op(
                "do",
                "X",
                "key+dup",
                true,
                Some("order.read"),
                "2",
                &["order_id", "reason"],
                &["status", "refund_expected"],
            ),
        ),
        (
            "delivery.read",
            op(
                "observe",
                "R",
                "nat",
                false,
                None,
                "2",
                &["order_id"],
                &parcel,
            ),
        ),
        (
            "delivery.read{by_tracking_ref}",
            variant(
                "lookup",
                "by_tracking_ref",
                json!({"identity_level": "1", "input_fields": ["tracking_ref"],
                       "output_fields": ["status", "carrier", "expected_window", "events"]}),
            ),
        ),
        (
            "delivery.list",
            op(
                "observe",
                "R",
                "nat",
                false,
                None,
                "2",
                &["party_id"],
                &["status", "carrier", "tracking_ref", "order_id"],
            ),
        ),
        (
            "payment.refund",
            op(
                "do",
                "X",
                "key+dup",
                true,
                None,
                "2",
                &["payment_id", "amount", "refund_reason"],
                &["status", "amount", "expected_at"],
            ),
        ),
        (
            "case.read",
            op(
                "observe",
                "R",
                "nat",
                false,
                None,
                "2",
                &["case_id"],
                &["status", "case_type", "due_at"],
            ),
        ),
        (
            "case.create",
            op(
                "request",
                "W",
                "key+dup",
                true,
                None,
                "2",
                &["case_type", "subject", "subject_object", "description"],
                &["case_id", "status", "due_at"],
            ),
        ),
        (
            "case.create{return}",
            variant(
                "case_type",
                "return",
                json!({"input_fields": ["subject_object", "return_lines", "return_method", "refund_type"],
                       "output_fields": ["case_id", "status", "label", "deadline"]}),
            ),
        ),
        ("rule.evaluate", rule),
        (
            "rule.evaluate{RET-01}",
            variant(
                "rule_id",
                "RET-01",
                json!({"input_fields": ["order_id", "delivered_at", "return_lines"]}),
            ),
        ),
    ]
}

fn opts(pairs: &[(&str, &str)], generated: bool) -> Value {
    Value::Array(
        pairs
            .iter()
            .map(|(k, l)| json!({"key": k, "label": l, "description": "", "generated": generated}))
            .collect(),
    )
}

fn rule_gte(threshold: f64, min_confidence: Option<f64>) -> Value {
    json!({"fire_when": "gte", "threshold": threshold, "fire_values": null, "min_confidence": min_confidence})
}

#[allow(clippy::too_many_arguments)]
fn gate(
    group: &str,
    point: &str,
    kind: &str,
    question: &str,
    options: Value,
    threshold_value: Option<f64>,
    fail: &str,
    safety: bool,
    rule: Value,
    action: &str,
) -> Value {
    json!({
        "group": group, "point": point, "type": kind, "question": question, "options": options,
        "threshold": "", "threshold_value": threshold_value, "fail": fail, "safety": safety,
        "effect": "", "rule": rule, "action": action, "origin": "platform",
    })
}

fn gates() -> Vec<(&'static str, Value)> {
    let yes_no = [("true", "true"), ("false", "false")];
    let mut b6 = gate(
        "base",
        "P1",
        "choice",
        "Which language is the customer writing in?",
        opts(
            &[("en", "English"), ("de", "German"), ("other", "Other")],
            true,
        ),
        Some(0.7),
        "open",
        false,
        rule_gte(0.7, None),
        "annotate",
    );
    b6["generated_from"] = json!("the job's languages served + other");
    let mut ga = gate(
        "base",
        "P1",
        "choice",
        "Which activity does the customer want?",
        opts(
            &[
                (ACTIVITY_WHERE, "Where is my order"),
                (ACTIVITY_RETURN, "Return an item"),
                (ACTIVITY_CANCEL, "Cancel an order"),
            ],
            true,
        ),
        Some(0.7),
        "open",
        false,
        rule_gte(0.7, None),
        "annotate",
    );
    ga["generated_from"] = json!("the activities of this job, generated at the cut");
    vec![
        (
            "B1",
            gate(
                "safety",
                "P1",
                "noul",
                "Does the message describe a risk to someone's safety?",
                opts(&yes_no, false),
                None,
                "closed",
                true,
                rule_gte(0.5, Some(0.7)),
                "handover",
            ),
        ),
        ("B6", b6),
        (
            "F1",
            gate(
                "post",
                "R1",
                "choice",
                "Is the draft reply supported by the knowledge found?",
                opts(
                    &[
                        ("supported", "supported"),
                        ("partly", "partly"),
                        ("not_supported", "not supported"),
                    ],
                    false,
                ),
                Some(0.8),
                "open",
                false,
                json!({"fire_when": "in", "threshold": null,
                       "fire_values": ["partly", "not_supported"], "min_confidence": 0.8}),
                "block",
            ),
        ),
        ("GA", ga),
        (
            "GC",
            gate(
                "base",
                "P1",
                "noul",
                "Does the customer say they are complaining?",
                opts(&yes_no, false),
                None,
                "open",
                false,
                rule_gte(0.5, None),
                "annotate",
            ),
        ),
        (
            "GM",
            gate(
                "base",
                "P1",
                "choice",
                "What kind of request is the customer making?",
                opts(
                    &[
                        ("do", "do"),
                        ("request", "request"),
                        ("observe", "observe"),
                        ("unclear", "unclear"),
                    ],
                    false,
                ),
                Some(0.6),
                "open",
                false,
                rule_gte(0.6, None),
                "annotate",
            ),
        ),
        (
            "GU",
            gate(
                "base",
                "P1",
                "score",
                "How urgent does the customer sound, from 1 to 5?",
                opts(
                    &[("1", "1"), ("2", "2"), ("3", "3"), ("4", "4"), ("5", "5")],
                    false,
                ),
                Some(4.0),
                "open",
                false,
                rule_gte(4.0, None),
                "annotate",
            ),
        ),
    ]
}

fn role(label: &str, max: &str, enabled: bool, grants: Value) -> Value {
    json!({"label": label, "used_by": "test", "max_side_effect": max, "enabled": enabled,
           "grants": grants, "note": ""})
}

fn step(n: u32, phase: u32, name: &str, op: &str, variant: Option<&str>, args: Value) -> Value {
    let effect = match op {
        "case.create" => ("request", "W", true),
        _ => ("observe", "R", false),
    };
    json!({
        "step_number": n, "phase": phase, "name": name, "operation": op, "variant": variant,
        "modality": effect.0, "side_effect": effect.1, "identity_level": "2", "confirm_back": effect.2,
        "gate": op == "rule.evaluate", "args": args, "preconditions": [], "postconditions": [],
        "performed_by": null, "max_per_case": null,
    })
}

fn ops_of(list: &[(&str, Option<&str>, bool)]) -> Value {
    Value::Array(
        list.iter()
            .map(|(op, variant, required)| match variant {
                Some(v) => json!({"op": op, "variant": v, "required": required}),
                None => json!({"op": op, "required": required}),
            })
            .collect(),
    )
}

async fn author(e: &TestEnv, o: platform::Offering) {
    let base = layer_id(e, o.base).await;
    let dept = layer_id(e, o.department).await;
    let job = layer_id(e, o.job_layer).await;

    // base: ontology
    for (name, label) in [
        ("Order", "Order"),
        ("Delivery", "Delivery"),
        ("Case", "Case"),
    ] {
        add(
            e,
            &base,
            "entity",
            name,
            json!({"label": label, "synonyms": []}),
        )
        .await;
    }
    for (key, ty) in [
        ("Order.order_id", "string"),
        ("Order.status", "string"),
        ("Delivery.status", "string"),
        ("Case.case_id", "string"),
    ] {
        add(e, &base, "attribute", key, json!({"data_type": ty})).await;
    }
    add(
        e,
        &base,
        "relation",
        "Case|concerns|Order",
        json!({"cardinality": "n:1"}),
    )
    .await;
    add(
        e,
        &base,
        "relation",
        "Delivery|tracks|Order",
        json!({"cardinality": "n:1"}),
    )
    .await;
    // base: catalogue, gates, roles
    for (key, fields) in operations() {
        add(e, &base, "operation", key, fields).await;
    }
    for (key, fields) in gates() {
        add(e, &base, "gate_question", key, fields).await;
    }
    add(
        e,
        &base,
        "agent_role",
        "procedure",
        role(
            "Procedure",
            "P",
            true,
            json!([{"object": "*", "verb": "*", "variant": null,
                    "modalities": ["do", "request", "observe"], "derive_from_activity": true}]),
        ),
    )
    .await;
    add(
        e,
        &base,
        "agent_role",
        "qualifier",
        role("Qualifier", "R", true, json!([])),
    )
    .await;
    add(
        e,
        &base,
        "agent_role",
        "post_gate",
        role("Post-gate", "R", true, json!([])),
    )
    .await;
    add(
        e,
        &base,
        "agent_role",
        "assistant",
        role("Assistant", "W", false, json!([])),
    )
    .await;

    // department: activities, context spaces, procedures
    add(
        e,
        &dept,
        "activity",
        ACTIVITY_WHERE,
        json!({"label": "Where is my order", "actor": "customer", "operations": ops_of(&[
            ("delivery.read", Some("by_tracking_ref"), true),
            ("order.read", None, true),
            ("knowledge.search", None, true),
            ("delivery.read", None, true),
            ("delivery.list", None, false),
        ])}),
    )
    .await;
    add(
        e,
        &dept,
        "activity",
        ACTIVITY_RETURN,
        json!({"label": "Return an item", "actor": "customer", "operations": ops_of(&[
            ("order.read", None, true),
            ("rule.evaluate", Some("RET-01"), true),
            ("case.create", Some("return"), true),
            ("knowledge.search", None, true),
        ])}),
    )
    .await;
    add(
        e,
        &dept,
        "activity",
        ACTIVITY_CANCEL,
        json!({"label": "Cancel an order", "actor": "customer", "operations": ops_of(&[
            ("order.read", None, true),
            ("order.cancel", None, false),
        ])}),
    )
    .await;
    let spaces: [SpaceSpec; 5] = [
        (
            "retail:logistics:track_parcel",
            "Where is my parcel",
            "logistics",
            &[],
            Some(ACTIVITY_WHERE),
        ),
        (
            "retail:order-support:order_status",
            "Order status",
            "order-support",
            &["order_id"],
            Some(ACTIVITY_WHERE),
        ),
        (
            "retail:order-support:returns",
            "Returns",
            "order-support",
            &["order_id", "reason"],
            Some(ACTIVITY_RETURN),
        ),
        (
            "retail:order-support:cancel_order",
            "Cancel an order",
            "order-support",
            &["order_id", "reason"],
            Some(ACTIVITY_CANCEL),
        ),
        (
            "retail:faq:shipping",
            "Shipping questions",
            "faq",
            &[],
            None,
        ),
    ];
    for (key, label, area, slots, activity) in spaces {
        let mut f = json!({"label": label, "area": area, "slots": slots});
        if let Some(a) = activity {
            f["activity"] = json!(a);
        }
        add(e, &dept, "context_space", key, f).await;
    }
    add(
        e,
        &dept,
        "procedure",
        "return_request@3",
        json!({
            "label": "Return request", "context_space": "retail:order-support:returns",
            "description": "Check the return window, then open the return case.",
            "steps": [
                step(1, 0, "order", "order.read", None, json!({"order_id": "$order_id"})),
                step(2, 1, "rules", "rule.evaluate", Some("RET-01"),
                     json!({"order_id": "$order_id", "delivered_at": "$order.delivered_at"})),
                step(3, 2, "rma", "case.create", Some("return"),
                     json!({"subject_object": "$order_id"})),
            ],
            "success_criteria": ["rma"],
            "tool_requirements": ["order.read", "rule.evaluate{RET-01}", "case.create{return}"],
        }),
    )
    .await;
    add(
        e,
        &dept,
        "procedure",
        "order_status@2",
        json!({
            "label": "Order status", "context_space": "retail:order-support:order_status",
            "description": "Read the order and its delivery, then say where it is.",
            "steps": [
                step(1, 0, "order", "order.read", None, json!({"order_id": "$order_id"})),
                step(2, 1, "delivery", "delivery.read", None, json!({"order_id": "$order_id"})),
            ],
            "success_criteria": ["order"],
            "tool_requirements": ["order.read", "delivery.read"],
        }),
    )
    .await;

    // the job family needs one new untagged row to be tagged against the new stack
    add(
        e,
        &job,
        "job_setting",
        "b5_content",
        json!({"label": "B5 test content"}),
    )
    .await;
    for family in [o.base, o.department, o.job_layer] {
        tag(e, family).await;
    }
}

/// The job template (layer, version) with the B5 content on top of the offering, authored once per database.
pub async fn template(e: &TestEnv) -> (String, i32) {
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
            let newest: Option<i32> = sqlx::query_scalar(
                "SELECT max(version) FROM job_template_tag WHERE layer = $1 AND status = 'tagged'",
            )
            .bind(o.job_layer)
            .fetch_one(&e.owner)
            .await
            .unwrap();
            if newest.is_none_or(|v| v <= o.version) {
                author(e, o).await;
            }
            let after: Option<i32> = sqlx::query_scalar(
                "SELECT max(version) FROM job_template_tag WHERE layer = $1 AND status = 'tagged'",
            )
            .bind(o.job_layer)
            .fetch_one(&e.owner)
            .await
            .unwrap();
            after.expect("a tagged job template")
        })
        .await;
    (o.job_layer.to_owned(), v)
}

/// A new contract job (own tenant) on [`template`].
pub async fn new_job(e: &TestEnv) -> ContractJob {
    new_job_for(e, Uuid::new_v4()).await
}

/// [`new_job`] for a tenant id chosen beforehand (the tenant must not exist yet).
pub async fn new_job_for(e: &TestEnv, tenant: Uuid) -> ContractJob {
    let t = template(e).await;
    let f = design_contract::new_job_for_on(e, tenant, t).await;
    route_activity_ops(e, &f).await;
    f
}

/// Route every operation the activities name on Production (the routing publish gate, R5): the built-ins run in the
/// agent, the rest fall back to the case queue. Idempotent; a route already there is kept.
pub async fn route_activity_ops(e: &TestEnv, f: &ContractJob) {
    let o = platform::offering(e).await;
    let rows: Vec<Option<Value>> = sqlx::query_scalar(
        "SELECT r.fields->'operations' FROM layer_row r JOIN layer l ON l.id = r.layer_id \
          WHERE l.key = $1 AND r.kind = 'activity' AND r.is_current AND r.op <> 'tombstone'",
    )
    .bind(o.department)
    .fetch_all(&e.owner)
    .await
    .unwrap();
    let mut ops = std::collections::BTreeSet::new();
    for list in rows.iter().flatten().filter_map(Value::as_array) {
        for item in list {
            if let Some((object, verb)) = item["op"].as_str().and_then(|s| s.split_once('.')) {
                ops.insert((object.to_owned(), verb.to_owned()));
            }
        }
    }
    for (object, verb) in ops {
        let kind = match (object.as_str(), verb.as_str()) {
            ("knowledge", "search") | ("rule", "evaluate") => "builtin",
            _ => "agent_cases",
        };
        sqlx::query(
            "INSERT INTO operation_route (tenant_id, job_id, object, verb, variant, env, target_kind, updated_by) \
             VALUES ($1, $2, $3, $4, '*', 'prod', $5, 'test') \
             ON CONFLICT (tenant_id, job_id, object, verb, variant, env) DO NOTHING",
        )
        .bind(f.tenant)
        .bind(f.job.job_id)
        .bind(&object)
        .bind(&verb)
        .bind(kind)
        .execute(&e.owner)
        .await
        .unwrap();
    }
}

/// `design_contract::publish` after routing the activities' operations on Production.
pub async fn publish(e: &TestEnv, f: &ContractJob) -> String {
    route_activity_ops(e, f).await;
    design_contract::publish(e, f).await
}
