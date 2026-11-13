//! The operations a customer-care job expects, for the connector route / spec tests (`mod ops_world;`). There is no
//! boot seed: `common::platform::offering` makes a job template with no catalogue and no `operation_expectation`, so
//! a job built from it expects no operation and every route write answers `operation_not_expected`. [`template`]
//! authors, through the super_admin layer routes, what those tests read:
//!
//! - `base`: the objects `order` (two states, a `cancel` transition), `offering`, `case`, `document`, `credential`,
//!   `knowledge` and the operations `order.read`, `order.list`, `order.cancel` (X, verified by `order.read`),
//!   `offering.list`, `offering.search` (offered, never expected), `case.read`, `case.create` (+ `{return}`), `document.read` (+ `{invoice}`) and the built-in
//!   `knowledge.search`;
//! - the department layer: one activity whose operations are exactly `knowledge.search`, `case.read` and
//!   `case.create{return}` (what a release of the retail selection requires on Production), and its context space;
//! - the job layer: one `operation_expectation` row per operation (`required`, or `optional`; `case.create` names its
//!   `{return}` variant);
//! - base, department and job layer tagged again, so the newest tag of `job/customer_care_agent` pins all of it.
//!
//! Identifiers, not product content. Idempotent per database.
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::common::platform::{self, DEPARTMENT_LAYER, JOB_LAYER};
use crate::common::{TestEnv, json as req, send, super_admin};

async fn sa(e: &TestEnv, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, req("POST", uri, Some(&super_admin()), body)).await
}

async fn add(e: &TestEnv, layer: &str, kind: &str, key: &str, fields: Value) {
    let id: String = sqlx::query_scalar("SELECT id::text FROM layer WHERE key = $1")
        .bind(layer)
        .fetch_one(&e.owner)
        .await
        .unwrap_or_else(|x| panic!("layer {layer}: {x}"));
    let (s, b) = sa(
        e,
        &format!("/api/layers/{id}/rows"),
        json!({"kind": kind, "key": key, "op": "add", "fields": fields}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "add {kind} {key} to {layer}: {b}");
}

async fn tag(e: &TestEnv, family: &str) {
    let (s, b) = sa(
        e,
        &format!("/api/platform/templates/{}/tags", family.replace('/', ".")),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "tag {family}: {b}");
}

fn field(name: &str, ty: &str, values: &[&str]) -> Value {
    json!({"field": name, "data_type": ty, "pii": false, "required": true, "enum_values": values,
           "ref": null, "description": ""})
}

fn object(label: &str, fields: Vec<Value>) -> Value {
    json!({"label": label, "tier": "base", "description": "", "fields": fields})
}

#[allow(clippy::too_many_arguments)]
fn op(
    side: &str,
    modality: &str,
    idem: &str,
    confirm: bool,
    verify: Option<&str>,
    input: &[&str],
    output: &[&str],
    builtin: bool,
) -> Value {
    json!({
        "discriminator": null,
        "attrs": {
            "modality": modality, "is_derive": builtin, "side_effect": side, "idempotency": idem,
            "confirm_back": confirm, "verify_read": verify, "builtin": builtin,
            "op_kind": if builtin { "builtin" } else { "standard" }, "guard_rules": [],
            "identity_level": "2", "input_fields": input, "output_fields": output
        },
        "description": "A test operation.", "retail": null, "stages": [], "extra_errors": []
    })
}

fn variant(field: &str, value: &str, input: &[&str], output: &[&str]) -> Value {
    json!({
        "discriminator": {"field": field, "value": value},
        "attrs": {"input_fields": input, "output_fields": output},
        "description": "A test variant.", "retail": null, "stages": [], "extra_errors": []
    })
}

fn expect(key: &str, summary: &str) -> Value {
    json!({"label": key, "summary": summary})
}

/// Author the operations once per database and return the job template `(layer, version)` that pins them.
pub async fn template(e: &TestEnv) -> (String, i32) {
    let o = platform::offering(e).await;
    let done: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM layer_row WHERE kind = 'operation_expectation')",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    if !done {
        author(e).await;
    }
    sqlx::query_as(
        "SELECT layer, version FROM job_template_tag \
          WHERE layer = $1 AND status = 'tagged' ORDER BY version DESC LIMIT 1",
    )
    .bind(o.job_layer)
    .fetch_one(&e.owner)
    .await
    .expect("a tagged customer-care template")
}

async fn author(e: &TestEnv) {
    let order = json!({
        "label": "Order", "tier": "base", "description": "A purchase.", "in_retail_job": true,
        "fields": [field("order_id", "id", &[]), field("status", "enum", &["processing", "cancelled"])],
        "states": [
            {"status": "processing", "label": "Processing", "terminal": false, "canonical": true, "initial": true},
            {"status": "cancelled", "label": "Cancelled", "terminal": true, "canonical": true, "initial": false}
        ],
        "transitions": [{"verb": "order.cancel", "from_status": "processing", "to_status": "cancelled",
                         "guard_rule": null}],
    });
    add(e, "base", "object", "order", order).await;
    add(
        e,
        "base",
        "object",
        "offering",
        object("Offering", vec![field("offering_id", "id", &[])]),
    )
    .await;
    add(
        e,
        "base",
        "object",
        "case",
        object(
            "Case",
            vec![
                field("case_id", "id", &[]),
                field("case_type", "enum", &["incident", "return"]),
            ],
        ),
    )
    .await;
    add(
        e,
        "base",
        "object",
        "document",
        object(
            "Document",
            vec![
                field("document_id", "id", &[]),
                field("document_type", "enum", &["invoice"]),
            ],
        ),
    )
    .await;
    add(
        e,
        "base",
        "object",
        "credential",
        object("Credential", vec![field("credential_id", "id", &[])]),
    )
    .await;
    add(
        e,
        "base",
        "object",
        "knowledge",
        object("Knowledge", vec![field("query", "text", &[])]),
    )
    .await;

    let read = |input: &[&str], output: &[&str]| {
        op("R", "observe", "nat", false, None, input, output, false)
    };
    add(
        e,
        "base",
        "operation",
        "order.read",
        read(&["order_id"], &["status"]),
    )
    .await;
    add(
        e,
        "base",
        "operation",
        "order.list",
        read(&["party_id"], &["order_id", "status"]),
    )
    .await;
    add(
        e,
        "base",
        "operation",
        "order.cancel",
        op(
            "X",
            "do",
            "key+dup",
            true,
            Some("order.read"),
            &["order_id", "reason"],
            &["status"],
            false,
        ),
    )
    .await;
    add(
        e,
        "base",
        "operation",
        "offering.list",
        read(&[], &["offering_id"]),
    )
    .await;
    // offered by the catalogue but NOT expected by the job layer below: only a procedure can make it routable
    add(
        e,
        "base",
        "operation",
        "offering.search",
        read(&[], &["offering_id"]),
    )
    .await;
    add(
        e,
        "base",
        "operation",
        "case.read",
        read(&["case_id"], &["status"]),
    )
    .await;
    add(
        e,
        "base",
        "operation",
        "case.create",
        op(
            "W",
            "request",
            "key+dup",
            true,
            None,
            &["case_type", "subject"],
            &["case_id", "status"],
            false,
        ),
    )
    .await;
    add(
        e,
        "base",
        "operation",
        "case.create{return}",
        variant(
            "case_type",
            "return",
            &["subject_object", "return_lines"],
            &["case_id", "status"],
        ),
    )
    .await;
    add(
        e,
        "base",
        "operation",
        "document.read",
        read(&["document_id"], &["kind", "issue_date", "url"]),
    )
    .await;
    add(
        e,
        "base",
        "operation",
        "document.read{invoice}",
        variant(
            "document_type",
            "invoice",
            &["order_id"],
            &["kind", "issue_date", "url"],
        ),
    )
    .await;
    add(
        e,
        "base",
        "operation",
        "knowledge.search",
        op(
            "R",
            "observe",
            "nat",
            false,
            None,
            &["query"],
            &["passages"],
            true,
        ),
    )
    .await;

    // the retail selection's Production requirements: the operations of the folded activities
    add(
        e,
        DEPARTMENT_LAYER,
        "activity",
        "support:return",
        json!({
            "label": "Return an item", "actor": "customer", "trigger": "asks to send an item back",
            "operations": [
                {"op": "knowledge.search", "required": true},
                {"op": "case.read", "required": true},
                {"op": "case.create", "variant": "return", "required": true}
            ]
        }),
    )
    .await;
    add(
        e,
        DEPARTMENT_LAYER,
        "context_space",
        "retail:order-support:returns",
        json!({"label": "Returns", "area": platform::AREA, "knowledge_kinds": [], "slots": ["order_id"],
               "activity": "support:return"}),
    )
    .await;

    // what the job expects (summary = `required` | `optional`, variants in braces)
    for (key, summary) in [
        ("order.read", "required"),
        ("order.list", "required"),
        ("order.cancel", "optional"),
        ("offering.list", "optional"),
        ("case.read", "required"),
        ("case.create", "required · {return}"),
        ("document.read", "optional · {invoice}"),
        ("knowledge.search", "required"),
    ] {
        add(
            e,
            JOB_LAYER,
            "operation_expectation",
            key,
            expect(key, summary),
        )
        .await;
    }
    for family in ["base", DEPARTMENT_LAYER, JOB_LAYER] {
        tag(e, family).await;
    }
}
