//! 0246: a tenant brings its own tools to its own job (owner 2026-10-31: the platform is general purpose; a tenant's
//! tools are never platform catalogue). On the real Postgres harness through the API router: a tenant REST template
//! (an ERP) defines an operation no platform catalogue knows; the job adds it at an identity level of the tenant's
//! choosing; it is then an expected (routable, required) operation of the job, the assistant's toolset holds it,
//! the runtime catalogue of the draft offers it with its own inputs — and Production does not until a release.
//! The design-target contract of `job_operation` (round trip, ledger, release snapshot, apply/rollback) runs too.
use crate::common;

use agent_api::design::DesignOf;
use agent_api::runtime::catalog::OperationCatalog;
use agent_api::runtime::design::RuntimeDesign;
use agent_model::kinds::catalog::IdentityLevel;
use axum::http::StatusCode;
use common::design_contract::{self, ContractJob, new_job};
use common::platform::generic_rest;
use common::{TestEnv, json as req, send, super_admin};
use serde_json::{Value, json};

const OP: &str = "invoice.post";

async fn call(
    e: &TestEnv,
    f: &ContractJob,
    method: &str,
    uri: &str,
    body: Value,
) -> (StatusCode, Value) {
    let mut r = req(method, uri, Some(&super_admin()), body);
    r.headers_mut()
        .insert("x-active-tenant-id", f.tenant.to_string().parse().unwrap());
    send(&e.app, r).await
}

fn erp_ops() -> Value {
    json!([{
        "object": "invoice", "verb": "post", "method": "POST", "path": "/invoices",
        "request": r#"{"amount":"{{canonical.amount}}","customer_ref":"{{canonical.customer_ref}}"}"#,
        "field_map": [{"path": "$.id", "field": "id", "transform": "none"}],
        "status_map": [], "idempotency": {"kind": "nat"},
        "def": {"side_effect": "W", "input_fields": ["amount", "customer_ref"], "output_fields": ["id"],
                "description": "Posts an invoice in the ERP."}
    }])
}

async fn runtime(e: &TestEnv, f: &ContractJob, of: DesignOf) -> RuntimeDesign {
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let d = RuntimeDesign::load(&mut tx, &e.state.design, &f.job_ref(), &of)
        .await
        .unwrap_or_else(|e| panic!("load {of:?}: {e:?}"));
    tx.commit().await.unwrap();
    d
}

#[tokio::test]
async fn a_tenant_template_op_added_to_the_job_is_routable_granted_and_offered() {
    let e = common::env().await;
    generic_rest(&e).await.unwrap();
    let f = new_job(&e).await;
    let job = &f.job.job_key;

    // the tenant's ERP template: an op the platform does not know, with its own definition
    let (s, b) = call(&e, &f, "POST", "/api/connections/templates",
        json!({"key": "erp", "label": "ERP", "kind": "custom", "auth_kind": "none", "description": ""})).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = call(
        &e,
        &f,
        "PUT",
        "/api/connections/templates/erp/1",
        json!({"label": "ERP", "description": "", "credential_schema": [], "ops": erp_ops()}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["ops"][0]["def"]["side_effect"], "W", "{b}");
    let (s, b) = call(
        &e,
        &f,
        "POST",
        "/api/connections/templates/erp/1/validate",
        json!({}),
    )
    .await;
    assert_eq!(
        (s, b.clone()),
        (StatusCode::OK, json!([])),
        "the template's own op validates: {b}"
    );
    // an op neither catalogue knows and that carries no definition does not validate
    let mut bare = erp_ops();
    bare[0].as_object_mut().unwrap().remove("def");
    let (s, b) = call(
        &e,
        &f,
        "PUT",
        "/api/connections/templates/erp/1",
        json!({"label": "ERP", "description": "", "credential_schema": [], "ops": bare}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (_, b) = call(
        &e,
        &f,
        "POST",
        "/api/connections/templates/erp/1/validate",
        json!({}),
    )
    .await;
    assert!(b.to_string().contains("unknown_operation"), "{b}");
    let (s, b) = call(
        &e,
        &f,
        "PUT",
        "/api/connections/templates/erp/1",
        json!({"label": "ERP", "description": "", "credential_schema": [], "ops": erp_ops()}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");

    // the job lists it as one it can add, and does not have it yet
    let own_uri = format!("/api/jobs/{job}/own-operations");
    let (s, b) = call(&e, &f, "GET", &own_uri, Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["ops"], json!([]));
    let avail = b["available"].as_array().unwrap();
    let a = avail
        .iter()
        .find(|a| a["op"] == OP)
        .expect("offered to the job");
    assert_eq!(
        (a["own"].as_bool(), a["added"].as_bool()),
        (Some(true), Some(false))
    );
    let rt = runtime(&e, &f, DesignOf::Draft).await;
    assert!(
        rt.catalog.get(OP).is_none(),
        "not the job's before it is added"
    );

    // a missing op or template is refused
    let (s, b) = call(
        &e,
        &f,
        "PUT",
        &format!("{own_uri}/invoice.void"),
        json!({"template": "erp", "version": "1", "identity_level": "1"}),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("operation_not_in_template")),
        "{b}"
    );
    let (s, _) = call(
        &e,
        &f,
        "PUT",
        &format!("{own_uri}/{OP}"),
        json!({"template": "nope", "version": "1", "identity_level": "1"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // added at level 1 (identified): a draft write
    let (s, b) = call(
        &e,
        &f,
        "PUT",
        &format!("{own_uri}/{OP}"),
        json!({"template": "erp", "version": "1", "identity_level": "1"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["value"]["template"], "erp@1");
    assert!(b["draft_rev"].as_i64() > b["previous_rev"].as_i64(), "{b}");

    // an expected, required op of the job: routable like any other
    let (s, b) = call(
        &e,
        &f,
        "GET",
        &format!("/api/jobs/{job}/operations"),
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let ops = b["ops"]
        .as_array()
        .or_else(|| b["data"]["ops"].as_array())
        .expect("ops");
    let mine = ops
        .iter()
        .find(|o| o["object"] == "invoice" && o["verb"] == "post")
        .unwrap_or_else(|| panic!("{OP} expected: {b}"));
    assert_eq!(mine["required"], true);
    assert_eq!(
        (
            mine["side_effect"].as_str(),
            mine["identity_level"].as_str()
        ),
        (Some("W"), Some("1"))
    );

    // the draft runtime catalogue offers it with its own inputs, at the tenant's level
    let rt = runtime(&e, &f, DesignOf::Draft).await;
    let d = rt.catalog.get(OP).expect("in the draft runtime catalogue");
    assert_eq!(d.inputs, ["amount", "customer_ref"]);
    assert_eq!(d.identity_level, IdentityLevel::Identified);
    assert!(d.writes());
    // the assistant's toolset holds it for every activity
    let t = &rt.job_design.toolsets;
    if let Some(act) = t.activities().next().map(str::to_owned)
        && t.role("assistant").is_some()
    {
        let rows = t.rows("assistant", &act).expect("assistant toolset");
        let row = rows
            .iter()
            .find(|r| r.label == OP)
            .expect("the job op is in the assistant's toolset");
        assert!(row.in_grant && row.expected.is_some(), "{row:?}");
    }
    // Production has not got it: the job's live design changes with a release only
    let live = runtime(&e, &f, DesignOf::Live).await;
    assert!(live.catalog.get(OP).is_none(), "not live before a release");

    // listed as added; removing it takes it out of the draft again
    let (_, b) = call(&e, &f, "GET", &own_uri, Value::Null).await;
    assert_eq!(b["ops"][0]["op"], OP, "{b}");
    let (s, b) = call(&e, &f, "DELETE", &format!("{own_uri}/{OP}"), Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = call(&e, &f, "DELETE", &format!("{own_uri}/{OP}"), Value::Null).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("job_operation_not_found")),
        "{b}"
    );
    let rt = runtime(&e, &f, DesignOf::Draft).await;
    assert!(rt.catalog.get(OP).is_none());
}

#[tokio::test]
async fn job_operation_full_contract() {
    let e = common::env().await;
    let value = json!({
        "identity_level": "0", "side_effect": "W", "input_fields": ["title"], "output_fields": ["id"],
        "description": "Starts a project.", "template": "fr_mcp@1",
    });
    let r = match json!({"op": "fr.start_project"}) {
        Value::Object(m) => m,
        _ => unreachable!(),
    };
    design_contract::run_all(&e, "job_operation", &[(r, value)], Some("not on the job")).await;
}
