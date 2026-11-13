//! The tenant-offboard service door (2026-10-25, crate::tenants::offboarder): `POST /api/tenants/{id}/offboard` admits
//! a person super_admin OR the ONE secure-apps service account pinned by `TENANT_OFFBOARD_SA_{ISSUER,AZP,ROLE}`.
//! Nothing else opens to that account, `confirm_name` stays required, and the audit names the service account.
//! Real Postgres 18 (one container for this binary); Keycloak and licences are the in-process fake.
use crate::common;

use axum::http::StatusCode;
use common::fakes::{self, Upstream};
use common::{
    TEST_AUDIENCE, TEST_ISSUER, TestEnv, get, in_five_minutes, insert_tenant, insert_user, json,
    send, sign, super_admin,
};
use serde_json::{Value, json};
use uuid::Uuid;

const SECURE_APPS: &str = "https://issuer.test/realms/secure-apps";
const AZP: &str = "agent-tenant-offboarder";
const ROLE: &str = "agent-tenant-offboard";

fn trusted(v: &mut std::collections::HashMap<String, String>) {
    v.insert(
        "KEYCLOAK_TRUSTED_ISSUERS".into(),
        format!("{TEST_ISSUER},{SECURE_APPS}"),
    );
}

/// The door open: secure-apps trusted and the three pins set.
async fn setup() -> (TestEnv, Upstream) {
    let up = fakes::upstream().await;
    let e = common::env_with(|v| {
        fakes::identity_env(&up, v);
        trusted(v);
        v.insert("TENANT_OFFBOARD_SA_ISSUER".into(), SECURE_APPS.into());
        v.insert("TENANT_OFFBOARD_SA_AZP".into(), AZP.into());
        v.insert("TENANT_OFFBOARD_SA_ROLE".into(), ROLE.into());
    })
    .await;
    (e, up)
}

/// A client-credentials token: `iss`, `azp`, `aud`, realm roles, optional client roles.
fn sa(iss: &str, azp: &str, aud: &[&str], roles: &[&str], client_roles: Value) -> String {
    sign(&json!({
        "sub": "9d3c0c64-6b0e-4a43-9f8e-2b4f6f4e0a11",
        "preferred_username": format!("service-account-{azp}"),
        "exp": in_five_minutes(), "iss": iss, "aud": aud, "azp": azp,
        "realm_access": {"roles": roles},
        "resource_access": client_roles,
    }))
}

/// The pinned service account's token.
fn offboarder() -> String {
    sa(SECURE_APPS, AZP, &[AZP], &[ROLE], json!({}))
}

async fn tenant(e: &TestEnv) -> Uuid {
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Early Co").await;
    insert_user(&e.owner, "u-ea", t, "tenant_admin").await;
    t
}

async fn offboard(e: &TestEnv, t: Uuid, token: &str, body: Value) -> (StatusCode, Value) {
    send(
        &e.app,
        json(
            "POST",
            &format!("/api/tenants/{t}/offboard"),
            Some(token),
            body,
        ),
    )
    .await
}

async fn status_of(e: &TestEnv, t: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn the_pinned_service_account_offboards_and_is_audited_as_itself() {
    let (e, _up) = setup().await;
    let t = tenant(&e).await;
    let (s, b) = offboard(&e, t, &offboarder(), json!({"confirm_name": "Early Co"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["archived"], true, "{b}");
    assert_eq!(b["users_disabled"], 1, "{b}");
    assert_eq!(status_of(&e, t).await, "archived");
    let actor = format!("system:{AZP}");
    let (id, kind, platform): (String, String, bool) = sqlx::query_as(
        "SELECT actor_id, actor_type, platform_actor FROM audit_event \
          WHERE tenant_id = $1 AND action = 'tenant.offboard'",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        (id.as_str(), kind.as_str(), platform),
        (actor.as_str(), "system", true)
    );
    let (id, kind): (String, String) = sqlx::query_as(
        "SELECT actor_id, actor_type FROM platform_audit \
          WHERE action = 'tenant.offboard' AND subject_tenant = $1",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!((id.as_str(), kind.as_str()), (actor.as_str(), "system"));
    let by: String = sqlx::query_scalar("SELECT \"by\" FROM tenant_offboard WHERE tenant_id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(by, actor);
}

#[tokio::test]
async fn confirm_name_is_still_required_for_the_service_account() {
    let (e, _up) = setup().await;
    let t = tenant(&e).await;
    let (s, b) = offboard(&e, t, &offboarder(), json!({"confirm_name": "Early"})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "confirm_name_mismatch");
    let (s, b) = offboard(&e, t, &offboarder(), json!({})).await;
    assert!(s.is_client_error(), "{s} {b}");
    assert_eq!(status_of(&e, t).await, "active");
}

#[tokio::test]
async fn a_secure_apps_token_without_the_door_azp_role_or_aud_is_403() {
    let (e, _up) = setup().await;
    let t = tenant(&e).await;
    for (what, token) in [
        ("no role", sa(SECURE_APPS, AZP, &[AZP], &[], json!({}))),
        (
            "another role",
            sa(
                SECURE_APPS,
                AZP,
                &[AZP],
                &["agent-signup-intake"],
                json!({}),
            ),
        ),
        (
            "role only as a client role",
            sa(
                SECURE_APPS,
                AZP,
                &[AZP],
                &[],
                json!({AZP: {"roles": [ROLE]}}),
            ),
        ),
        (
            "wrong azp",
            sa(
                SECURE_APPS,
                "agent-signup-intake",
                &["agent-signup-intake"],
                &[ROLE],
                json!({}),
            ),
        ),
        (
            "aud without the azp",
            sa(SECURE_APPS, AZP, &["agent"], &[ROLE], json!({})),
        ),
        (
            "super_admin as well",
            sa(SECURE_APPS, AZP, &[AZP], &[ROLE, "super_admin"], json!({})),
        ),
        (
            "super_admin as a client role",
            sa(
                SECURE_APPS,
                AZP,
                &[AZP],
                &[ROLE],
                json!({TEST_AUDIENCE: {"roles": ["super_admin"]}}),
            ),
        ),
    ] {
        let (s, b) = offboard(&e, t, &token, json!({"confirm_name": "Early Co"})).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{what}: {b}");
        assert_eq!(
            b["error"]["code"], "tenant_offboard_forbidden",
            "{what}: {b}"
        );
    }
    assert_eq!(status_of(&e, t).await, "active");
}

#[tokio::test]
async fn the_door_client_from_any_other_issuer_is_401() {
    let (e, _up) = setup().await;
    let t = tenant(&e).await;
    for (what, iss) in [
        ("untrusted issuer", "https://evil.test/realms/secure-apps"),
        // trusted, but a person realm: read as a person, whose azp is not the web client
        ("the portal realm", TEST_ISSUER),
    ] {
        let token = sa(iss, AZP, &[AZP], &[ROLE], json!({}));
        let (s, b) = offboard(&e, t, &token, json!({"confirm_name": "Early Co"})).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{what}: {b}");
    }
    assert_eq!(status_of(&e, t).await, "active");
}

#[tokio::test]
async fn a_human_super_admin_still_offboards_with_the_door_open() {
    let (e, _up) = setup().await;
    let t = tenant(&e).await;
    // a person without super_admin is still refused
    let (s, _) = offboard(
        &e,
        t,
        &common::person("u-ea", &[]),
        json!({"confirm_name": "Early Co"}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, b) = offboard(&e, t, &super_admin(), json!({"confirm_name": "Early Co"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (id, kind): (String, String) = sqlx::query_as(
        "SELECT actor_id, actor_type FROM audit_event WHERE tenant_id = $1 AND action = 'tenant.offboard'",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!((id.as_str(), kind.as_str()), ("u-super", "human"));
}

/// The door opens the offboard POST and nothing else: every other super_admin route refuses the token.
#[tokio::test]
async fn the_service_account_opens_no_other_route() {
    let (e, _up) = setup().await;
    let t = tenant(&e).await;
    let tok = offboarder();
    for req in [
        get("/api/tenants", Some(&tok)),
        get(&format!("/api/tenants/{t}"), Some(&tok)),
        get(&format!("/api/tenants/{t}/offboard"), Some(&tok)),
        json(
            "PATCH",
            &format!("/api/tenants/{t}"),
            Some(&tok),
            json!({"status": "suspended"}),
        ),
        get("/api/me", Some(&tok)),
    ] {
        let uri = req.uri().to_string();
        let (s, b) = send(&e.app, req).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{uri}: {b}");
    }
    assert_eq!(status_of(&e, t).await, "active");
}

/// Fail closed: with the pins unset, the same token is a secure-apps token on a person route — 401.
#[tokio::test]
async fn with_the_door_unset_the_service_account_is_401() {
    let up = fakes::upstream().await;
    let e = common::env_with(|v| {
        fakes::identity_env(&up, v);
        trusted(v);
    })
    .await;
    assert!(e.state.cfg.tenant_offboard_service.is_none());
    let t = tenant(&e).await;
    let (s, b) = offboard(&e, t, &offboarder(), json!({"confirm_name": "Early Co"})).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{b}");
    assert_eq!(status_of(&e, t).await, "active");
    let (s, b) = offboard(&e, t, &super_admin(), json!({"confirm_name": "Early Co"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}
