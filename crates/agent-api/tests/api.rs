//! Base surface: /health, /ready, GET /api/me, tenants/users reads, the error envelope, RLS and the
//! no-seed rule. Real Postgres 18 (one container for this binary).
use crate::common;

use axum::http::StatusCode;
use common::{get, insert_tenant, insert_user, person, send, sign, super_admin};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn health_and_ready_need_no_token() {
    let e = common::env().await;
    let (s, b) = send(&e.app, get("/health", None)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["status"], "ok");
    let (s, b) = send(&e.app, get("/ready", None)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["db"], "ok");
    assert_eq!(b["signup"], "dark");
}

#[tokio::test]
async fn migrations_carry_no_seed_data() {
    let e = common::env().await;
    for t in [
        "tenants",
        "users",
        "signup_approvals",
        "signup_provisioning_steps",
        "member_entitlement",
        "member_session",
        "tenant_offboard",
        "super_admin_active",
        "agent_case",
        "agent_case_seq",
        "case_event",
        "conversation",
        "conversation_message",
        "conversation_control",
        "test_case",
        "agent_handover_items",
        "agent_handover_events",
        "agent_handover_feedback",
        "agent_handover_seq",
        "case_csat",
        "connector_bindings",
        "channel_conversation_refs",
    ] {
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {t}")))
            .fetch_one(&e.owner)
            .await
            .unwrap();
        assert_eq!(n, 0, "{t} must start empty");
    }
}

#[tokio::test]
async fn me_needs_a_valid_token_for_the_agent_audience() {
    let e = common::env().await;
    let (s, b) = send(&e.app, get("/api/me", None)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(b["error"]["code"], "unauthenticated");
    // another client's token (aud) is refused
    let other = sign(&json!({"sub": "u1", "exp": common::in_five_minutes(),
        "iss": common::TEST_ISSUER, "aud": "commonpractice-frontend"}));
    assert_eq!(
        send(&e.app, get("/api/me", Some(&other))).await.0,
        StatusCode::UNAUTHORIZED
    );
    // another realm (iss) is refused
    let realm = sign(&json!({"sub": "u1", "exp": common::in_five_minutes(),
        "iss": "https://issuer.test/realms/secure-apps", "aud": "agent"}));
    assert_eq!(
        send(&e.app, get("/api/me", Some(&realm))).await.0,
        StatusCode::UNAUTHORIZED
    );
    // expired
    let old =
        sign(&json!({"sub": "u1", "exp": 1_000_000, "iss": common::TEST_ISSUER, "aud": "agent"}));
    assert_eq!(
        send(&e.app, get("/api/me", Some(&old))).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn me_reports_home_tenant_role_and_super_admin() {
    let e = common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Acme").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;

    let (s, b) = send(&e.app, get("/api/me", Some(&person("u-admin", &[])))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["sub"], "u-admin");
    assert_eq!(b["super_admin"], false);
    assert_eq!(b["tenant"]["id"], t.to_string());
    assert_eq!(b["tenant"]["name"], "Acme");
    assert_eq!(b["roles"], json!(common::ALL_ROLES));
    assert_eq!(b["effective_roles"], json!(common::ALL_ROLES));
    assert!(b["view_as"].is_null());
    assert!(
        b["tenant"].get("role").is_none(),
        "D88: the single role is gone"
    );
    assert_eq!(b["tenant"]["status"], "active");
    assert_eq!(b["active_tenant"], t.to_string());

    // unprovisioned person: verified, no tenant
    let (s, b) = send(&e.app, get("/api/me", Some(&person("u-nobody", &[])))).await;
    assert_eq!(s, StatusCode::OK);
    assert!(b["tenant"].is_null());
    assert!(b["active_tenant"].is_null());

    // super_admin is the client role on `agent` only; a realm role of that name grants nothing
    let (_, b) = send(&e.app, get("/api/me", Some(&super_admin()))).await;
    assert_eq!(b["super_admin"], true);
    let realm_only = sign(&json!({"sub": "u-r", "exp": common::in_five_minutes(),
        "iss": common::TEST_ISSUER, "aud": "agent", "azp": "agent",
        "realm_access": {"roles": ["super_admin"]}}));
    let (s, b) = send(&e.app, get("/api/me", Some(&realm_only))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["super_admin"], false);
}

/// Cross-app token replay (found with live portal tokens): portal clients have fullScopeAllowed, so a
/// person token issued to ANOTHER app carries `aud: agent` and `resource_access.agent.roles = [super_admin]`.
/// Only `azp` names the client it was issued to; anything but `agent` (or no `azp`) is 401.
#[tokio::test]
async fn person_token_issued_to_another_app_is_refused() {
    let e = common::env().await;
    let replay = |azp: Option<&str>| {
        let mut c = json!({"sub": "u-super", "preferred_username": "u-super",
            "exp": common::in_five_minutes(), "iss": common::TEST_ISSUER,
            "aud": ["care-bus", "agent", "account"],
            "resource_access": {"agent": {"roles": ["super_admin"]}}});
        if let Some(a) = azp {
            c["azp"] = json!(a);
        }
        sign(&c)
    };
    for azp in [
        Some("care-bus"),
        Some("strata-configurator"),
        Some("w2v"),
        None,
    ] {
        let t = replay(azp);
        let (s, b) = send(&e.app, get("/api/me", Some(&t))).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "azp {azp:?}: {b}");
        assert_eq!(b["error"]["code"], "unauthenticated");
        // a super_admin-only route is closed too
        let (s, _) = send(&e.app, get("/api/tenants", Some(&t))).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "azp {azp:?}");
    }
    // control: the identical token issued to `agent` is accepted, super_admin intact
    let (s, b) = send(&e.app, get("/api/me", Some(&replay(Some("agent"))))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["super_admin"], true);
    let (s, _) = send(&e.app, get("/api/tenants", Some(&replay(Some("agent"))))).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn tenant_list_is_super_admin_only_and_users_are_member_scoped() {
    let e = common::env().await;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, a, "Alpha").await;
    insert_tenant(&e.owner, b, "Beta").await;
    insert_user(&e.owner, "u-a", a, "tenant_user").await;
    insert_user(&e.owner, "u-b", b, "tenant_admin").await;

    let (s, body) = send(&e.app, get("/api/tenants", Some(&super_admin()))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    let (s, body) = send(&e.app, get("/api/tenants", Some(&person("u-a", &[])))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "super_admin_required");

    let own = format!("/api/tenants/{a}/users");
    let (s, body) = send(&e.app, get(&own, Some(&person("u-a", &[])))).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let subs: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["sub"].as_str().unwrap())
        .collect();
    assert_eq!(subs, ["u-a"], "RLS: only this tenant's members");
    let other = format!("/api/tenants/{b}/users");
    assert_eq!(
        send(&e.app, get(&other, Some(&person("u-a", &[])))).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&e.app, get(&other, Some(&super_admin()))).await.0,
        StatusCode::OK
    );
    let (s, body) = send(
        &e.app,
        get("/api/tenants/not-a-uuid/users", Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_tenant_id");
    let missing = format!("/api/tenants/{}/users", Uuid::new_v4());
    let (s, body) = send(&e.app, get(&missing, Some(&super_admin()))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "tenant_not_found");
}

#[tokio::test]
async fn the_runtime_role_is_confined_by_rls() {
    let e = common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    insert_user(&e.owner, "u-a", t, "tenant_user").await;
    // no tenant GUC: the runtime pool sees no tenant rows at all
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&e.state.db)
        .await
        .unwrap();
    assert_eq!(n, 0);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants")
        .fetch_one(&e.state.db)
        .await
        .unwrap();
    assert_eq!(n, 0);
    // and it may not delete platform signup rows (no grant)
    let err = sqlx::query("DELETE FROM signup_approvals")
        .execute(&e.state.db)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("permission denied"), "{err}");
}

#[tokio::test]
async fn unknown_routes_and_methods_use_the_envelope() {
    let e = common::env().await;
    let (s, b) = send(&e.app, get("/api/nope", None)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b["error"]["code"], "not_found");
    let (s, b) = send(&e.app, common::json("POST", "/api/me", None, json!({}))).await;
    assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(b["error"]["code"], "method_not_allowed");
}

/// Spec 16 §3: a request without a usable token is 401 `unauthenticated` (renamed from `unauthorized`).
#[tokio::test]
async fn no_token_is_401_unauthenticated() {
    let e = common::env().await;
    for uri in ["/api/me", "/api/tenants"] {
        let (s, b) = send(&e.app, get(uri, None)).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(b["error"]["code"], "unauthenticated", "{uri}: {b}");
        let (s, b) = send(&e.app, get(uri, Some("not-a-jwt"))).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(b["error"]["code"], "unauthenticated", "{uri}: {b}");
    }
}

/// The platform tenant list refuses a tenant member with the coded 403 `super_admin_required`.
#[tokio::test]
async fn tenant_list_for_member_is_403_super_admin_required() {
    let e = common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;
    for who in [person("u-admin", &[]), person("u-nobody", &[])] {
        let (s, b) = send(&e.app, get("/api/tenants", Some(&who))).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
        assert_eq!(b["error"]["code"], "super_admin_required", "{b}");
    }
    // control: the super_admin is served
    assert_eq!(
        send(&e.app, get("/api/tenants", Some(&super_admin())))
            .await
            .0,
        StatusCode::OK
    );
}
