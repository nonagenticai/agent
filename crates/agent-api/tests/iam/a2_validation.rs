//! A2/A3 declaration validation over the real route `PUT /api/jobs/{job}/tools/{op}/policy`.
use serde_json::json;

use super::world::*;
use crate::common::conv::code;

/// Owner 2026-11-01: the never-ask-for-ids rule governs our demos, not the platform. A declaration — a sign-in
/// procedure in particular — may ask the person for an email, an id or a phone.
#[tokio::test]
async fn asking_the_user_for_any_key_the_tenant_declares_is_accepted() {
    let w = World::build("iam-a2-validation-ask", say_only(ANSWER), false).await;
    for key in ["order_id", "customer_id", "email", "postcode"] {
        let (s, b) = w
            .put_policy(
                &w.f,
                READ,
                json!({"inputs": {"party_id": {"source": "state", "key": key,
                                               "from": {"ask_user": "What is it?"}}}}),
            )
            .await;
        assert_eq!(s, 200, "{key}: {b}");
    }
    // a one-time code too (the sign-in shape)
    let (s, b) = w.put_policy(&w.f, VERIFY, verify_policy()).await;
    assert_eq!(s, 200, "{b}");
}

#[tokio::test]
async fn a_tool_cannot_produce_a_platform_key() {
    let w = World::build("iam-a2-validation-reserved", say_only(ANSWER), false).await;
    let (s, b) = w
        .put_policy(
            &w.f,
            READ,
            json!({"outputs": {"status": {"key": "subject"}}}),
        )
        .await;
    assert_eq!(s, 422, "{b}");
    assert_eq!(code(&b), "reserved_key", "{b}");
    let (s, b) = w
        .put_policy(
            &w.f,
            READ,
            json!({"outputs": {"status": {"key": "Bad-Key"}}}),
        )
        .await;
    assert_eq!((s.as_u16(), code(&b)), (422, "invalid_key"), "{b}");
}

#[tokio::test]
async fn a_producer_cycle_is_refused() {
    let w = World::build("iam-a2-validation-cycle", say_only(ANSWER), false).await;
    // order.list needs k_b from party.list ...
    w.policy_ok(
        &w.f,
        READ,
        json!({"inputs": {"party_id": {"source": "state", "key": "k_b",
                                       "from": {"tool": LOOKUP, "output": "party_id"}}},
               "outputs": {"order_id": {"key": "k_a"}}}),
    )
    .await;
    // ... and party.list producing k_b would need k_a from order.list
    let (s, b) = w
        .put_policy(
            &w.f,
            LOOKUP,
            json!({"inputs": {"email": {"source": "state", "key": "k_a",
                                        "from": {"tool": READ, "output": "order_id"}}},
                   "outputs": {"party_id": {"key": "k_b"}}}),
        )
        .await;
    assert_eq!(s, 422, "{b}");
    assert_eq!(code(&b), "cycle", "{b}");
    let (s, b) = w
        .call(&w.f, "GET", &format!("tools/{LOOKUP}/policy"), json!({}))
        .await;
    assert_eq!(s, 200, "{b}");
    assert!(
        b["policy"].is_null(),
        "the refused declaration was not stored: {b}"
    );
}

#[tokio::test]
async fn an_operation_the_job_does_not_have_is_not_declarable() {
    let w = World::build("iam-a2-validation-unknown", say_only(ANSWER), false).await;
    let (s, b) = w.put_policy(&w.f, "nothing.here", json!({})).await;
    assert_eq!(
        (s.as_u16(), code(&b)),
        (404, "operation_not_expected"),
        "{b}"
    );
}
