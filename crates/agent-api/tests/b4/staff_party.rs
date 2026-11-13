//! A staff-audience job (IT service desk, HR) binds its conversation to the signed-in employee's VERIFIED Keycloak
//! email: `customer_id` = `customer_email` = the lower-cased token email, `identity_source = 'staff_pick'` (state), so the
//! case is born at level 2 with `verified_party` = that email. Never the request body, a message or the model.
//! A customer-audience job takes the typed test customer (`staff_test`).
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::design_contract;
use crate::common::{self, TestEnv, json as req, send};
use crate::redis_env as r;
use agent_api::conversations::store;

const ANNA: &str = "anna.kowalski@northwind-it.example";
const BEN: &str = "ben.osei@northwind-it.example";

/// A super_admin token (it may act in any tenant) carrying `email` and, when given, `email_verified`.
fn employee(sub: &str, email: Option<&str>, verified: Option<bool>) -> String {
    let mut c = json!({
        "sub": sub, "preferred_username": sub, "exp": common::in_five_minutes(),
        "iss": common::TEST_ISSUER, "aud": common::TEST_AUDIENCE, "azp": common::TEST_AUDIENCE,
        "resource_access": {common::TEST_AUDIENCE: {"roles": ["super_admin"]}}
    });
    if let Some(e) = email {
        c["email"] = json!(e);
    }
    if let Some(v) = verified {
        c["email_verified"] = json!(v);
    }
    common::sign(&c)
}

async fn start(
    e: &TestEnv,
    f: &design_contract::ContractJob,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let mut q = req(
        "POST",
        &format!("/api/jobs/{}/conversations", f.job.job_key),
        Some(token),
        body,
    );
    q.headers_mut()
        .insert("x-active-tenant-id", f.tenant.to_string().parse().unwrap());
    send(&e.app, q).await
}

/// A job of the offering that serves employees (`jobs.audience = 'employees'`, D129, set through `PATCH /api/jobs/{job}`).
async fn it_desk_job(e: &TestEnv) -> design_contract::ContractJob {
    let f = design_contract::new_job(e).await;
    let mut q = req(
        "PATCH",
        &format!("/api/jobs/{}", f.job.job_key),
        Some(&common::super_admin()),
        json!({"audience": "employees"}),
    );
    q.headers_mut()
        .insert("x-active-tenant-id", f.tenant.to_string().parse().unwrap());
    let (s, b) = send(&e.app, q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    f
}

/// `(customer_id, customer_email, identity_source)` and the case birth `(verified_party, level)` of conversation `id`.
async fn bound(
    e: &TestEnv,
    id: &str,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    u8,
) {
    let mut conn = e.owner.acquire().await.unwrap();
    let conv = store::conversation(&mut conn, id)
        .await
        .unwrap()
        .expect("the conversation");
    let (party, _email) = conv.verified();
    (
        conv.customer_id.clone(),
        conv.customer_email.clone(),
        conv.identity_source.clone(),
        party,
        conv.identity_level(),
    )
}

async fn rows(e: &TestEnv, tenant: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM conversation WHERE tenant_id = $1")
        .bind(tenant)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn verified_employee_conversation_is_bound_to_the_token_email() {
    let vars = r::api_vars("staff-party-bind").await;
    let e = common::env_with(|v| v.extend(vars.clone())).await;
    let f = it_desk_job(&e).await;
    // a mixed-case token email: the party is its lower-cased form
    let tok = employee(
        "u-anna",
        Some("Anna.Kowalski@Northwind-IT.example"),
        Some(true),
    );
    let (s, b) = start(
        &e,
        &f,
        &tok,
        json!({"env": "test", "customer_email": "ignored@x.example"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["customer_id"], ANNA, "{b}");
    let id = b["conversation_id"].as_str().unwrap();
    let (cid, email, source, party, level) = bound(&e, id).await;
    assert_eq!(cid.as_deref(), Some(ANNA));
    assert_eq!(email.as_deref(), Some(ANNA));
    assert_eq!(source.as_deref(), Some("staff_pick"));
    assert_eq!(
        party.as_deref(),
        Some(ANNA),
        "the case's verified party (state `subject`)"
    );
    assert_eq!(level, 2);
}

#[tokio::test]
async fn unverified_or_missing_email_refuses_and_stores_nothing() {
    let vars = r::api_vars("staff-party-unverified").await;
    let e = common::env_with(|v| v.extend(vars.clone())).await;
    let f = it_desk_job(&e).await;
    for (what, tok) in [
        (
            "email_verified false",
            employee("u-a", Some(ANNA), Some(false)),
        ),
        ("email_verified absent", employee("u-a", Some(ANNA), None)),
        ("no email", employee("u-a", None, Some(true))),
    ] {
        let (s, b) = start(&e, &f, &tok, json!({"env": "test", "customer_email": ANNA})).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{what}: {b}");
        assert_eq!(b["error"]["code"], "email_not_verified", "{what}: {b}");
    }
    assert_eq!(rows(&e, f.tenant).await, 0);
}

#[tokio::test]
async fn a_typed_or_other_email_never_changes_the_party() {
    let vars = r::api_vars("staff-party-typed").await;
    let e = common::env_with(|v| v.extend(vars.clone())).await;
    let f = it_desk_job(&e).await;
    let tok = employee("u-anna", Some(ANNA), Some(true));
    // the body names Ben (as customer_id and in extra fields): Anna is still the party
    let (s, b) = start(
        &e,
        &f,
        &tok,
        json!({"env": "test", "customer_id": BEN, "email": BEN, "customer_email": BEN, "verified_party": BEN}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let id = b["conversation_id"].as_str().unwrap();
    let (cid, email, _, party, _) = bound(&e, id).await;
    assert_eq!(cid.as_deref(), Some(ANNA));
    assert_eq!(email.as_deref(), Some(ANNA));
    assert_eq!(party.as_deref(), Some(ANNA));
    // and the customer_id may be omitted altogether
    let (s, b) = start(&e, &f, &tok, json!({"env": "test"})).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
}

#[tokio::test]
async fn customer_audience_job_takes_the_typed_test_customer() {
    let vars = r::api_vars("staff-party-customer-job").await;
    let e = common::env_with(|v| v.extend(vars.clone())).await;
    let f = design_contract::new_job(&e).await;
    // even an unverified email starts: a customer-audience job does not read the token's email at all
    let tok = employee("u-anna", Some(ANNA), Some(false));
    let (s, b) = start(
        &e,
        &f,
        &tok,
        json!({"env": "test", "customer_email": "Ann@Shop.Example"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let id = b["conversation_id"].as_str().unwrap();
    let (cid, email, source, party, level) = bound(&e, id).await;
    assert_eq!(cid.as_deref(), Some("ann@shop.example"));
    assert_eq!(email.as_deref(), Some("ann@shop.example"));
    assert_eq!(source.as_deref(), Some("staff_test"));
    assert_eq!(party.as_deref(), Some("ann@shop.example"));
    assert_eq!(level, 2);
}
