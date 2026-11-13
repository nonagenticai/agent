//! B1 T12: invites, add by sub, roles, soft disable / enable, re-home and member activity (crate::routes::users), with
//! the member licence of D77 (pending inside the transaction, mint / revoke after COMMIT). Real Postgres 18 (one
//! container for this binary); Keycloak and licences are the in-process fake (tests/common/fakes.rs).
use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::fakes::{self, Upstream, lock};
use common::{TestEnv, get, insert_tenant, insert_user, json, person, send, super_admin};
use serde_json::{Map, Value, json};
use uuid::Uuid;

async fn setup() -> (TestEnv, Upstream, Uuid) {
    let up = fakes::upstream().await;
    let e = common::env_with(|v| fakes::identity_env(&up, v)).await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;
    (e, up, t)
}

fn admin() -> String {
    person("u-admin", &[])
}

fn in_tenant(mut r: Request<Body>, t: Uuid) -> Request<Body> {
    r.headers_mut()
        .insert("x-active-tenant-id", t.to_string().parse().unwrap());
    r
}

async fn invite(e: &TestEnv, email: &str, role: &str) -> (StatusCode, Value) {
    send(
        &e.app,
        json(
            "POST",
            "/api/tenant/invites",
            Some(&admin()),
            json!({"email": email, "roles": common::role_keys(role)}),
        ),
    )
    .await
}

async fn post(e: &TestEnv, uri: &str, token: &str) -> (StatusCode, Value) {
    send(&e.app, json("POST", uri, Some(token), json!({}))).await
}

async fn entitlement(e: &TestEnv, t: Uuid, sub: &str) -> Option<(String, Option<String>)> {
    sqlx::query_as(
        "SELECT status, license_id FROM member_entitlement WHERE tenant_id = $1 AND user_sub = $2",
    )
    .bind(t)
    .bind(sub)
    .fetch_optional(&e.owner)
    .await
    .unwrap()
}

/// An active member with an active member licence `lic-<sub>`.
async fn licensed_member(e: &TestEnv, t: Uuid, sub: &str, role: &str) {
    insert_user(&e.owner, sub, t, role).await;
    sqlx::query(
        "INSERT INTO member_entitlement (tenant_id, user_sub, license_id, license_subject_id, status) \
         VALUES ($1, $2, 'lic-' || $2, 'subject-1', 'active')",
    )
    .bind(t)
    .bind(sub)
    .execute(&e.owner)
    .await
    .unwrap();
}

async fn audit_count(e: &TestEnv, action: &str, status: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = $1 AND status = $2")
        .bind(action)
        .bind(status)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

fn kc_user(up: &Upstream, id: &str, email: &str) {
    lock(&up.mock)
        .users
        .push((id.into(), email.into(), Map::new()));
}

#[tokio::test]
async fn invite_new_email_creates_identity_member_and_sends_once() {
    let (e, up, t) = setup().await;
    let (s, b) = invite(&e, " Ann@Example.TEST ", "tenant_user").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["status"], "invite_sent");
    assert_eq!(b["email"], "ann@example.test");
    let sub = b["sub"].as_str().unwrap().to_string();
    {
        let g = lock(&up.mock);
        assert_eq!(
            g.created,
            std::slice::from_ref(&sub),
            "one identity created"
        );
        assert_eq!(g.emails.len(), 1, "one email");
        assert_eq!(g.emails[0].0, sub);
        assert!(g.emails[0].2.to_string().contains("UPDATE_PASSWORD"));
    }
    let (origin, invited, added_by, email): (String, bool, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT origin, invited_at IS NOT NULL, added_by, email FROM users WHERE sub = $1 AND tenant_id = $2",
        )
        .bind(&sub)
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(origin, "invite");
    assert!(invited);
    assert_eq!(added_by.as_deref(), Some("u-admin"));
    assert_eq!(email.as_deref(), Some("ann@example.test"));
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE action = 'user.invite' AND resource_id = $1",
    )
    .bind(&sub)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(details["email_domain"], "example.test");
    assert_eq!(details["roles"], json!([]));
    // inviting the same address again is user_exists
    let (s, b) = invite(&e, "ann@example.test", "tenant_user").await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "user_exists");
    let (s, b) = invite(&e, "not-an-email", "tenant_user").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(b["error"]["code"], "invalid_email");
    let (s, b) = invite(&e, "bob@example.test", "owner").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(b["error"]["code"], "invalid_role");
}

/// SECURITY (CP invite-service): a PRE-EXISTING portal account is never seated by an invite — no member row, no
/// roles, no entitlement, no licence. A pending invite bound to it is recorded and the access email sent; it is seated
/// (with the invited roles, then licensed) only when it signs in with the VERIFIED invited email.
#[tokio::test]
async fn invite_never_seats_an_existing_identity_until_a_verified_sign_in() {
    let (e, up, t) = setup().await;
    kc_user(&up, "kc-bob", "bob@example.test");
    lock(&up.mock).with_password.push("kc-bob".into());
    let (s, b) = invite(&e, "bob@example.test", "tenant_admin").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["sub"], "kc-bob");
    assert_eq!(b["seated"], false);
    {
        let g = lock(&up.mock);
        assert!(g.created.is_empty(), "the existing identity is reused");
        assert!(g.issued.is_empty(), "no licence");
        assert_eq!(g.emails.len(), 1);
        let body = g.emails[0].2.to_string();
        assert!(
            body.contains("VERIFY_EMAIL") && !body.contains("UPDATE_PASSWORD"),
            "an identity that signs in gets the access email only: {body}"
        );
    }
    assert_eq!(home(&e, "kc-bob").await, None, "not seated");
    assert_eq!(entitlement(&e, t, "kc-bob").await, None, "no entitlement");
    let (state, roles, bound): (String, Vec<String>, Option<String>) = sqlx::query_as(
        "SELECT state, roles, invitee_sub FROM member_invites WHERE tenant_id = $1 AND email = 'bob@example.test'",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(state, "pending");
    assert_eq!(roles, common::role_keys("tenant_admin"));
    assert_eq!(bound.as_deref(), Some("kc-bob"));
    let existed: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE action = 'user.invite' AND tenant_id = $1",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(existed["existing_identity"], true);
    // a plain token (no verified email) changes nothing
    send(&e.app, get("/api/me", Some(&person("kc-bob", &[])))).await;
    assert_eq!(home(&e, "kc-bob").await, None);
    // the verified sign-in seats it with the invited roles and licenses it
    let (s, me) = send(
        &e.app,
        get(
            "/api/me",
            Some(&email_token("kc-bob", "bob@example.test", Some(true))),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{me}");
    assert_eq!(me["tenant"]["id"], t.to_string(), "{me}");
    let roles: Vec<String> = sqlx::query_scalar("SELECT roles FROM users WHERE sub = 'kc-bob'")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(roles, common::role_keys("tenant_admin"));
    assert_eq!(entitlement(&e, t, "kc-bob").await.unwrap().0, "active");
}

#[tokio::test]
async fn invite_member_elsewhere_409() {
    let (e, up, _) = setup().await;
    let other = Uuid::new_v4();
    insert_tenant(&e.owner, other, "Beta").await;
    insert_user(&e.owner, "kc-cy", other, "tenant_user").await;
    kc_user(&up, "kc-cy", "cy@example.test");
    let (s, b) = invite(&e, "cy@example.test", "tenant_user").await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "user_exists_elsewhere");
    assert!(lock(&up.mock).emails.is_empty());
}

#[tokio::test]
async fn invite_keycloak_down_502_and_nothing_created() {
    let (e, up, _) = setup().await;
    lock(&up.mock).keycloak_down = true;
    let (s, b) = invite(&e, "dee@example.test", "tenant_user").await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
    assert_eq!(b["error"]["code"], "keycloak_error");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 1, "only the admin");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM member_entitlement")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
    assert!(lock(&up.mock).emails.is_empty());
}

#[tokio::test]
async fn invite_email_failure_keeps_member_and_audits_failure() {
    let (e, up, t) = setup().await;
    lock(&up.mock).email_down = true;
    let (s, b) = invite(&e, "eve@example.test", "tenant_user").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let sub = b["sub"].as_str().unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE sub = $1 AND tenant_id = $2")
        .bind(sub)
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 1, "the member row stays");
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE action = 'user.invite_email' AND status = 'failure'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(details["code"], "invite_email_failed");
}

#[tokio::test]
async fn add_by_sub_unknown_404() {
    let (e, up, _) = setup().await;
    let (s, b) = send(
        &e.app,
        json(
            "PUT",
            "/api/tenant/users/kc-ghost",
            Some(&admin()),
            json!({"roles": []}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(b["error"]["code"], "unknown_subject");
    lock(&up.mock).keycloak_down = true;
    let (s, b) = send(
        &e.app,
        json(
            "PUT",
            "/api/tenant/users/kc-ghost",
            Some(&admin()),
            json!({"roles": []}),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_GATEWAY,
        "an outage is never 'no such user': {b}"
    );
    assert_eq!(b["error"]["code"], "keycloak_error");
}

#[tokio::test]
async fn add_super_admin_409() {
    let (e, up, _) = setup().await;
    kc_user(&up, "kc-op", "op@example.test");
    lock(&up.mock)
        .client_roles
        .push(("kc-op".into(), "agent".into(), "super_admin".into()));
    let (s, b) = send(
        &e.app,
        json(
            "PUT",
            "/api/tenant/users/kc-op",
            Some(&admin()),
            json!({"roles": []}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "super_admin_not_member");
    assert_eq!(
        lock(&up.mock).client_lookups,
        0,
        "no clients?clientId= call"
    );
}

#[tokio::test]
async fn last_admin_guard_applies_to_super_admin() {
    let (e, _up, t) = setup().await;
    insert_user(&e.owner, "u-user", t, "tenant_user").await;
    let sa = super_admin();
    let (s, b) = send(
        &e.app,
        in_tenant(
            json(
                "PATCH",
                "/api/tenant/users/u-admin",
                Some(&sa),
                json!({"roles": []}),
            ),
            t,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "last_tenant_admin");
    let (s, b) = send(
        &e.app,
        in_tenant(
            json(
                "POST",
                "/api/tenant/users/u-admin/disable",
                Some(&sa),
                json!({}),
            ),
            t,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "last_tenant_admin");
    // with a second (accepted) admin the change goes through and is audited once
    let (s, b) = send(
        &e.app,
        in_tenant(
            json(
                "PATCH",
                "/api/tenant/users/u-user",
                Some(&sa),
                json!({"roles": common::ALL_ROLES}),
            ),
            t,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["roles"], json!(common::ALL_ROLES));
    let (s, b) = send(
        &e.app,
        in_tenant(
            json(
                "PATCH",
                "/api/tenant/users/u-admin",
                Some(&sa),
                json!({"roles": []}),
            ),
            t,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(audit_count(&e, "user.role", "success").await, 2);
    let (s, _) = send(
        &e.app,
        in_tenant(
            json(
                "PATCH",
                "/api/tenant/users/nobody",
                Some(&sa),
                json!({"roles": []}),
            ),
            t,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn disable_self_409() {
    let (e, _up, t) = setup().await;
    insert_user(&e.owner, "u-admin2", t, "tenant_admin").await;
    let (s, b) = post(&e, "/api/tenant/users/u-admin/disable", &admin()).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "cannot_disable_self");
}

#[tokio::test]
async fn disabled_member_token_is_not_provisioned() {
    let (e, _up, t) = setup().await;
    insert_user(&e.owner, "u-user", t, "tenant_user").await;
    let (s, b) = post(&e, "/api/tenant/users/u-user/disable", &admin()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["disabled_at"].is_string());
    let (s, b) = send(&e.app, get("/api/jobs", Some(&person("u-user", &[])))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["code"], "not_provisioned");
    // idempotent: a second disable answers the row and writes no audit row
    let (s, _) = post(&e, "/api/tenant/users/u-user/disable", &admin()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(audit_count(&e, "user.disable", "success").await, 1);
    // a tenant_user may not disable anyone
    insert_user(&e.owner, "u-other", t, "tenant_user").await;
    let (s, b) = post(
        &e,
        "/api/tenant/users/u-admin/disable",
        &person("u-other", &[]),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(b["error"]["code"], "forbidden");
}

#[tokio::test]
async fn rehome_disabled_user_enables_as_tenant_user_and_audits_three_rows() {
    let (e, _up, from) = setup().await;
    let to = Uuid::new_v4();
    insert_tenant(&e.owner, to, "Beta").await;
    insert_user(&e.owner, "u-move", from, "tenant_admin").await;
    sqlx::query("UPDATE users SET disabled_at = now() WHERE sub = 'u-move'")
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/users/u-move/rehome",
            Some(&super_admin()),
            json!({"tenant_id": to}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["tenant_id"], to.to_string());
    assert_eq!(b["roles"], json!([]));
    assert!(b["disabled_at"].is_null());
    let tenants: Vec<Uuid> = sqlx::query_scalar(
        "SELECT tenant_id FROM audit_event WHERE action = 'user.rehome' ORDER BY id",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(tenants, [from, to], "one row in each tenant log");
    let (n, subject): (i64, Option<Uuid>) = sqlx::query_as(
        "SELECT count(*), max(subject_tenant::text)::uuid FROM platform_audit WHERE action = 'user.rehome'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!((n, subject), (1, Some(to)));
    let (s, _) = send(&e.app, get("/api/jobs", Some(&person("u-move", &[])))).await;
    assert_eq!(s, StatusCode::OK, "the member signs in to the new tenant");
    for (body, code, status) in [
        (
            json!({"tenant_id": to}),
            "same_tenant",
            StatusCode::CONFLICT,
        ),
        (
            json!({"tenant_id": Uuid::new_v4()}),
            "user_active",
            StatusCode::CONFLICT,
        ),
    ] {
        let (s, b) = send(
            &e.app,
            json(
                "POST",
                "/api/users/u-move/rehome",
                Some(&super_admin()),
                body,
            ),
        )
        .await;
        assert_eq!(
            (s, b["error"]["code"].as_str()),
            (status, Some(code)),
            "{b}"
        );
    }
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/users/u-nobody/rehome",
            Some(&super_admin()),
            json!({"tenant_id": to}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b["error"]["code"], "user_not_found");
}

#[tokio::test]
async fn rehome_active_409_user_active() {
    let (e, _up, _from) = setup().await;
    let to = Uuid::new_v4();
    insert_tenant(&e.owner, to, "Beta").await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/users/u-admin/rehome",
            Some(&super_admin()),
            json!({"tenant_id": to}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "user_active");
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/users/u-admin/rehome",
            Some(&admin()),
            json!({"tenant_id": to}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(b["error"]["code"], "super_admin_required");
}

#[tokio::test]
async fn activity_counts_sessions_and_origin() {
    let (e, _up, t) = setup().await;
    insert_user(&e.owner, "u-seen", t, "tenant_user").await;
    sqlx::query("UPDATE users SET origin = 'invite', added_by = 'u-admin' WHERE sub = 'u-seen'")
        .execute(&e.owner)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET origin = 'tenant_create' WHERE sub = 'u-admin'")
        .execute(&e.owner)
        .await
        .unwrap();
    for (sid, surface, days) in [("s1", "console", 1), ("s2", "chat", 2), ("s3", "admin", 40)] {
        sqlx::query(
            "INSERT INTO member_session (tenant_id, sub, sid, surface, first_seen_at, last_seen_at) \
             VALUES ($1, 'u-seen', $2, $3, now() - make_interval(days => $4), now() - make_interval(days => $4))",
        )
        .bind(t)
        .bind(sid)
        .bind(surface)
        .bind(days)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    insert_user(&e.owner, "u-invited", t, "tenant_user").await;
    sqlx::query("UPDATE users SET origin = 'invite', invited_at = now() WHERE sub = 'u-invited'")
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = send(
        &e.app,
        // asked by another member: the caller's own request records a session for the caller
        get("/api/tenant/users/activity", Some(&admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let rows = b.as_array().unwrap();
    let of = |sub: &str| rows.iter().find(|r| r["sub"] == sub).unwrap().clone();
    let seen = of("u-seen");
    assert_eq!(
        seen["sign_ins_30d"], 2,
        "distinct sessions in 30 days: {seen}"
    );
    assert_eq!(seen["last_surface"], "console");
    assert!(seen["last_seen_at"].is_string());
    assert_eq!(seen["added_via"], "invite");
    assert_eq!(seen["added_by"], "u-admin");
    assert_eq!(seen["added_by_name"], "u-admin");
    assert!(seen["invite_expires_at"].is_null());
    assert_eq!(of("u-admin")["added_via"], "tenant_create");
    let inv = of("u-invited");
    assert_eq!(inv["sign_ins_30d"], 0);
    assert!(inv["last_seen_at"].is_null());
    assert!(inv["invite_expires_at"].is_string());
}

#[tokio::test]
async fn no_email_in_any_audit_row() {
    let (e, up, _) = setup().await;
    kc_user(&up, "kc-fay", "fay@example.test");
    invite(&e, "gus@example.test", "tenant_user").await;
    send(
        &e.app,
        json(
            "PUT",
            "/api/tenant/users/kc-fay",
            Some(&admin()),
            json!({"roles": []}),
        ),
    )
    .await;
    let rows: Vec<(String, Value)> = sqlx::query_as("SELECT action, details FROM audit_event")
        .fetch_all(&e.owner)
        .await
        .unwrap();
    assert!(rows.iter().any(|(a, _)| a == "user.invite"));
    assert!(rows.iter().any(|(a, _)| a == "user.add"));
    for (action, details) in &rows {
        assert!(
            !details.to_string().contains('@'),
            "{action} carries an address: {details}"
        );
    }
}

#[tokio::test]
async fn invite_writes_pending_entitlement_and_makes_no_licences_call() {
    let (e, up, t) = setup().await;
    let (s, b) = invite(&e, "hal@example.test", "tenant_user").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let sub = b["sub"].as_str().unwrap();
    assert_eq!(
        entitlement(&e, t, sub).await,
        Some(("pending".to_string(), None))
    );
    let g = lock(&up.mock);
    assert_eq!(g.minted, 0);
    assert!(g.issued.is_empty() && g.registered.is_empty() && g.calls.is_empty());
}

#[tokio::test]
async fn add_by_sub_mints_after_commit_and_stores_active() {
    let (e, up, t) = setup().await;
    kc_user(&up, "kc-ivy", "Ivy@Example.test");
    let (s, b) = send(
        &e.app,
        json(
            "PUT",
            "/api/tenant/users/kc-ivy",
            Some(&admin()),
            json!({"roles": []}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["sub"], "kc-ivy");
    assert_eq!(b["email"], "ivy@example.test");
    let (status, lic) = entitlement(&e, t, "kc-ivy").await.unwrap();
    assert_eq!(status, "active");
    assert!(lic.is_some());
    {
        let g = lock(&up.mock);
        assert_eq!(g.minted, 1);
        assert_eq!(g.issued, [(t.to_string(), "kc-ivy".to_string())]);
    }
    assert_eq!(audit_count(&e, "license.issue", "success").await, 1);
    let origin: String = sqlx::query_scalar("SELECT origin FROM users WHERE sub = 'kc-ivy'")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(origin, "add_by_sub");
    // licences down: the member is added, the entitlement stays pending for the tick
    kc_user(&up, "kc-jo", "jo@example.test");
    lock(&up.mock).licences_down = true;
    let (s, _) = send(
        &e.app,
        json(
            "PUT",
            "/api/tenant/users/kc-jo",
            Some(&admin()),
            json!({"roles": []}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(entitlement(&e, t, "kc-jo").await.unwrap().0, "pending");
}

#[tokio::test]
async fn disable_revokes_entitlement_and_checks_the_body() {
    let (e, up, t) = setup().await;
    licensed_member(&e, t, "u-kim", "tenant_user").await;
    let (s, b) = post(&e, "/api/tenant/users/u-kim/disable", &admin()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        entitlement(&e, t, "u-kim").await,
        Some(("revoked".to_string(), Some("lic-u-kim".to_string())))
    );
    assert_eq!(lock(&up.mock).revoked, ["lic-u-kim"]);
    assert_eq!(audit_count(&e, "license.revoke", "success").await, 1);
}

#[tokio::test]
async fn disable_revoke_failure_stays_revoking_and_audits() {
    let (e, up, t) = setup().await;
    licensed_member(&e, t, "u-lou", "tenant_user").await;
    lock(&up.mock).revoke_noop = true;
    let (s, b) = post(&e, "/api/tenant/users/u-lou/disable", &admin()).await;
    assert_eq!(s, StatusCode::OK, "the disable stands: {b}");
    assert_eq!(entitlement(&e, t, "u-lou").await.unwrap().0, "revoking");
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE action = 'license.revoke_failed' AND status = 'failure'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        details["code"], "licence_not_revoked",
        "a 200 no-op is not a revoke"
    );
    assert_eq!(audit_count(&e, "license.revoke", "success").await, 0);
}

#[tokio::test]
async fn enable_remints_after_commit() {
    let (e, up, t) = setup().await;
    licensed_member(&e, t, "u-max", "tenant_user").await;
    post(&e, "/api/tenant/users/u-max/disable", &admin()).await;
    assert_eq!(entitlement(&e, t, "u-max").await.unwrap().0, "revoked");
    let (s, b) = post(&e, "/api/tenant/users/u-max/enable", &admin()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["disabled_at"].is_null());
    let (status, lic) = entitlement(&e, t, "u-max").await.unwrap();
    assert_eq!(status, "active");
    assert_ne!(lic.as_deref(), Some("lic-u-max"), "a new licence");
    {
        let g = lock(&up.mock);
        assert_eq!(g.minted, 1);
        assert_eq!(
            g.calls.first().map(String::as_str),
            Some("revoke:lic-u-max")
        );
    }
    assert_eq!(audit_count(&e, "user.enable", "success").await, 1);
    // enabling an enabled member is a no-op
    post(&e, "/api/tenant/users/u-max/enable", &admin()).await;
    assert_eq!(audit_count(&e, "user.enable", "success").await, 1);
}

#[tokio::test]
async fn rehome_revokes_source_entitlement_and_mints_target_after_commit() {
    let (e, up, from) = setup().await;
    let to = Uuid::new_v4();
    insert_tenant(&e.owner, to, "Beta").await;
    licensed_member(&e, from, "u-ned", "tenant_user").await;
    sqlx::query("UPDATE users SET disabled_at = now() WHERE sub = 'u-ned'")
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/users/u-ned/rehome",
            Some(&super_admin()),
            json!({"tenant_id": to}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(entitlement(&e, from, "u-ned").await.unwrap().0, "revoked");
    let (status, lic) = entitlement(&e, to, "u-ned").await.unwrap();
    assert_eq!(status, "active");
    let g = lock(&up.mock);
    assert_eq!(
        g.calls,
        [
            "revoke:lic-u-ned".to_string(),
            format!("mint:{}", lic.unwrap())
        ],
        "revoke the source, then mint the target"
    );
    assert_eq!(g.issued, [(to.to_string(), "u-ned".to_string())]);
}

#[tokio::test]
async fn add_by_sub_role_lookup_failure_is_502_keycloak_error() {
    // D90: super_admin is granted by hand and only READ here; a failed read is 502, never a guess either way.
    let (e, up, t) = setup().await;
    kc_user(&up, "kc-new", "new@example.test");
    lock(&up.mock).role_mappings_down = true;
    let (s, b) = send(
        &e.app,
        json(
            "PUT",
            "/api/tenant/users/kc-new",
            Some(&admin()),
            json!({"roles": ["support_agent"]}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
    assert_eq!(b["error"]["code"], "keycloak_error");
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE sub = 'kc-new' AND tenant_id = $1")
            .bind(t)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(n, 0, "nothing was added");
}

/// 0040 (owner F-01): an invite leaves a `member_invites` record — pending, 7-day expiry, the role set, one send
/// counted — accepted on the invitee's first sign-in. An email that failed counts no send.
#[tokio::test]
async fn invite_records_a_member_invite_accepted_on_first_sign_in() {
    let (e, up, t) = setup().await;
    let (s, b) = invite(&e, "Ann@Example.test", "job_designer,operator").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let sub = b["sub"].as_str().unwrap().to_string();
    let row = |e: &TestEnv, sub: String| {
        let db = e.owner.clone();
        async move {
            sqlx::query_as::<_, (String, String, Vec<String>, String, i32, bool, bool)>(
                "SELECT state, email, roles, invited_by, send_count, last_sent_at IS NOT NULL, \
                        expires_at BETWEEN created_at + interval '6 days 23 hours' AND created_at + interval '7 days' \
                   FROM member_invites WHERE invitee_sub = $1",
            )
            .bind(sub)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    let (state, email, roles, by, sends, sent, week) = row(&e, sub.clone()).await;
    assert_eq!(state, "pending");
    assert_eq!(email, "ann@example.test");
    assert_eq!(roles, ["job_designer", "operator"]);
    assert_eq!(by, "u-admin");
    assert_eq!((sends, sent, week), (1, true, true));
    let tenant: Uuid =
        sqlx::query_scalar("SELECT tenant_id FROM member_invites WHERE invitee_sub = $1")
            .bind(&sub)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(tenant, t);
    // first sign-in accepts it
    let (s, _) = send(&e.app, get("/api/me", Some(&person(&sub, &[])))).await;
    assert_eq!(s, StatusCode::OK);
    let (state, accepted_by): (String, Option<String>) =
        sqlx::query_as("SELECT state, accepted_by FROM member_invites WHERE invitee_sub = $1")
            .bind(&sub)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(
        (state.as_str(), accepted_by.as_deref()),
        ("accepted", Some(sub.as_str()))
    );

    // a failed email counts no send
    lock(&up.mock).email_down = true;
    let (s, b) = invite(&e, "eve@example.test", "tenant_user").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let (_, _, _, _, sends, sent, _) = row(&e, b["sub"].as_str().unwrap().to_string()).await;
    assert_eq!((sends, sent), (0, false));
}

// ---- invite acceptance on sign-in (CP acceptInvite; crate::members::accept) -------------------------------------

/// A person token carrying `email` and, when given, `email_verified`.
fn email_token(sub: &str, email: &str, verified: Option<bool>) -> String {
    let mut claims = json!({
        "sub": sub, "preferred_username": sub, "email": email, "exp": common::in_five_minutes(),
        "iss": common::TEST_ISSUER, "aud": common::TEST_AUDIENCE, "azp": common::TEST_AUDIENCE,
        "resource_access": {common::TEST_AUDIENCE: {"roles": []}}
    });
    if let Some(v) = verified {
        claims["email_verified"] = json!(v);
    }
    common::sign(&claims)
}

/// A pending invite of `email` into `t` with `roles`.
async fn pending_invite(e: &TestEnv, t: Uuid, email: &str, roles: &[&str]) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO member_invites (tenant_id, email, roles, invited_by) VALUES ($1, $2, $3, 'u-admin') RETURNING id",
    )
    .bind(t)
    .bind(email)
    .bind(roles.iter().map(|r| r.to_string()).collect::<Vec<_>>())
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

async fn invite_state(e: &TestEnv, id: Uuid) -> (String, Option<String>) {
    sqlx::query_as("SELECT state, accepted_by FROM member_invites WHERE id = $1")
        .bind(id)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn home(e: &TestEnv, sub: &str) -> Option<Uuid> {
    sqlx::query_scalar("SELECT tenant_id FROM users WHERE sub = $1")
        .bind(sub)
        .fetch_optional(&e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_verified_email_accepts_its_invite_on_sign_in() {
    let (e, up, t) = setup().await;
    let inv = pending_invite(&e, t, "zoe@example.test", &["support_agent", "operator"]).await;
    let (s, me) = send(
        &e.app,
        get(
            "/api/me",
            Some(&email_token("kc-zoe", "Zoe@Example.TEST", Some(true))),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{me}");
    assert_eq!(me["tenant"]["id"], t.to_string(), "seated at once: {me}");
    assert_eq!(me["roles"], json!(["support_agent", "operator"]));
    assert_eq!(
        invite_state(&e, inv).await,
        ("accepted".into(), Some("kc-zoe".into()))
    );
    let (origin, email): (String, Option<String>) =
        sqlx::query_as("SELECT origin, email FROM users WHERE sub = 'kc-zoe'")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(
        (origin.as_str(), email.as_deref()),
        ("invite", Some("zoe@example.test"))
    );
    assert_eq!(audit_count(&e, "user.invite_accept", "success").await, 1);
    // licensed after COMMIT, the same path as any new member
    let (status, lic) = entitlement(&e, t, "kc-zoe").await.unwrap();
    assert_eq!(status, "active");
    assert!(lic.is_some());
    assert!(lock(&up.mock).issued.iter().any(|(_, h)| h == "kc-zoe"));
}

#[tokio::test]
async fn an_unverified_or_absent_email_verification_accepts_nothing() {
    let (e, _up, t) = setup().await;
    let inv = pending_invite(&e, t, "zoe@example.test", &[]).await;
    for verified in [Some(false), None] {
        send(
            &e.app,
            get(
                "/api/me",
                Some(&email_token("kc-zoe", "zoe@example.test", verified)),
            ),
        )
        .await;
        assert_eq!(home(&e, "kc-zoe").await, None, "{verified:?}");
        assert_eq!(invite_state(&e, inv).await.0, "pending");
    }
    // another verified address does not match
    send(
        &e.app,
        get(
            "/api/me",
            Some(&email_token("kc-zoe", "zoe@elsewhere.test", Some(true))),
        ),
    )
    .await;
    assert_eq!(home(&e, "kc-zoe").await, None);
    assert_eq!(audit_count(&e, "user.invite_accept", "success").await, 0);
}

#[tokio::test]
async fn an_expired_invite_is_not_claimed() {
    let (e, _up, t) = setup().await;
    let inv = pending_invite(&e, t, "zoe@example.test", &[]).await;
    sqlx::query(
        "UPDATE member_invites SET created_at = now() - interval '8 days', expires_at = now() - interval '1 day' \
          WHERE id = $1",
    )
    .bind(inv)
    .execute(&e.owner)
    .await
    .unwrap();
    send(
        &e.app,
        get(
            "/api/me",
            Some(&email_token("kc-zoe", "zoe@example.test", Some(true))),
        ),
    )
    .await;
    assert_eq!(home(&e, "kc-zoe").await, None);
    assert_eq!(invite_state(&e, inv).await.0, "pending");
}

/// The 409 rule: an invite never moves a member. A member elsewhere (enabled or disabled) accepts nothing.
#[tokio::test]
async fn a_member_elsewhere_does_not_accept() {
    let (e, _up, t) = setup().await;
    let other = Uuid::new_v4();
    insert_tenant(&e.owner, other, "Beta").await;
    insert_user(&e.owner, "kc-zoe", other, "tenant_user").await;
    let inv = pending_invite(&e, t, "zoe@example.test", &[]).await;
    let token = email_token("kc-zoe", "zoe@example.test", Some(true));
    let (_, me) = send(&e.app, get("/api/me", Some(&token))).await;
    assert_eq!(me["tenant"]["id"], other.to_string(), "{me}");
    assert_eq!(invite_state(&e, inv).await.0, "pending");
    sqlx::query("UPDATE users SET disabled_at = now() WHERE sub = 'kc-zoe'")
        .execute(&e.owner)
        .await
        .unwrap();
    send(&e.app, get("/api/me", Some(&token))).await;
    assert_eq!(home(&e, "kc-zoe").await, Some(other), "not moved");
    assert_eq!(invite_state(&e, inv).await.0, "pending");
}

#[tokio::test]
async fn concurrent_sign_ins_accept_once() {
    let (e, up, t) = setup().await;
    let inv = pending_invite(&e, t, "zoe@example.test", &["operator"]).await;
    let token = email_token("kc-zoe", "zoe@example.test", Some(true));
    let reqs: Vec<_> = (0..8)
        .map(|_| {
            let app = e.app.clone();
            let token = token.clone();
            tokio::spawn(async move { send(&app, get("/api/me", Some(&token))).await })
        })
        .collect();
    for r in reqs {
        let (s, me) = r.await.unwrap();
        assert_eq!(s, StatusCode::OK, "{me}");
        assert_eq!(me["tenant"]["id"], t.to_string(), "{me}");
    }
    assert_eq!(invite_state(&e, inv).await.0, "accepted");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE sub = 'kc-zoe'")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(audit_count(&e, "user.invite_accept", "success").await, 1);
    // One live licence: the accept's mint and a concurrent first-sign-in mint may both run; the conditional store
    // keeps one and the loser revokes its own (D77 L4).
    let (status, stored) = entitlement(&e, t, "kc-zoe").await.unwrap();
    assert_eq!(status, "active");
    let stored = stored.expect("a stored licence");
    let g = lock(&up.mock);
    assert!(
        !g.revoked.contains(&stored),
        "the winner's licence is never revoked: {stored} in {:?}",
        g.revoked
    );
    assert_eq!(
        g.mint_ids.len() - g.revoked.len(),
        1,
        "one live licence: {:?} / {:?}",
        g.mint_ids,
        g.revoked
    );
}

/// CP `invite_email_mismatch`: an invite bound to this account is NOT accepted when the token's verified email is
/// not the invited address; it stays pending and the refusal is audited.
#[tokio::test]
async fn a_bound_invite_needs_the_invited_email() {
    let (e, _up, t) = setup().await;
    let inv: Uuid = sqlx::query_scalar(
        "INSERT INTO member_invites (tenant_id, email, roles, invitee_sub, invited_by) \
         VALUES ($1, 'zoe@example.test', '{}', 'kc-zoe', 'u-admin') RETURNING id",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    send(
        &e.app,
        get(
            "/api/me",
            Some(&email_token("kc-zoe", "other@example.test", Some(true))),
        ),
    )
    .await;
    assert_eq!(home(&e, "kc-zoe").await, None);
    assert_eq!(invite_state(&e, inv).await.0, "pending");
    assert_eq!(audit_count(&e, "user.invite_accept", "failure").await, 1);
    let code: Value = sqlx::query_scalar(
        "SELECT details->'code' FROM audit_event WHERE action = 'user.invite_accept'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(code, "invite_email_mismatch");
    // the invited address accepts it
    send(
        &e.app,
        get(
            "/api/me",
            Some(&email_token("kc-zoe", "ZOE@example.test", Some(true))),
        ),
    )
    .await;
    assert_eq!(home(&e, "kc-zoe").await, Some(t));
}

// ---- review #8: a re-entitle before the tick must not have its re-issued licence revoked --------------------------

async fn parked(e: &TestEnv, t: Uuid) -> Vec<String> {
    sqlx::query_scalar("SELECT licence_revoke_pending FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

/// The traced sequence: disable → the revoke fails (row `revoking`, licence still active) → re-enable before the
/// reconciler retries (`entitle_pending` parks the id, row `pending`) → licences' `find_active_license` answers the
/// same still-active id → stored `active`. Before the fix the next tick drained the parked id and revoked the
/// licence the active entitlement points at.
#[tokio::test]
async fn reenable_before_the_tick_keeps_the_reissued_licence() {
    let (e, up, t) = setup().await;
    licensed_member(&e, t, "u-ivy", "tenant_user").await;
    {
        let mut g = lock(&up.mock);
        g.revoke_noop = true;
        g.active_by_holder
            .insert("u-ivy".into(), "lic-u-ivy".into());
    }
    let (s, b) = post(&e, "/api/tenant/users/u-ivy/disable", &admin()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        entitlement(&e, t, "u-ivy").await,
        Some(("revoking".to_string(), Some("lic-u-ivy".to_string())))
    );
    lock(&up.mock).revoke_noop = false;
    let (s, b) = post(&e, "/api/tenant/users/u-ivy/enable", &admin()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        entitlement(&e, t, "u-ivy").await,
        Some(("active".to_string(), Some("lic-u-ivy".to_string()))),
        "licences answered the still-active licence"
    );
    assert!(
        parked(&e, t).await.is_empty(),
        "the stored id is this member's live licence: unparked with the store"
    );
    for _ in 0..2 {
        agent_api::signup::provisioning::reconcile(&e.state)
            .await
            .unwrap();
    }
    assert_eq!(
        entitlement(&e, t, "u-ivy").await,
        Some(("active".to_string(), Some("lic-u-ivy".to_string())))
    );
    assert!(
        !lock(&up.mock).revoked.contains(&"lic-u-ivy".to_string()),
        "the active entitlement's licence was revoked"
    );
    assert!(parked(&e, t).await.is_empty());
}

/// A parked id that an `active` entitlement already holds (state written before the fix) is unparked by the drain
/// without a licences call.
#[tokio::test]
async fn drain_skips_a_parked_id_an_active_entitlement_holds() {
    let (e, up, t) = setup().await;
    licensed_member(&e, t, "u-jay", "tenant_user").await;
    sqlx::query("UPDATE tenants SET licence_revoke_pending = '{lic-u-jay}' WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    agent_api::signup::provisioning::reconcile(&e.state)
        .await
        .unwrap();
    assert!(parked(&e, t).await.is_empty());
    assert!(!lock(&up.mock).revoked.contains(&"lic-u-jay".to_string()));
    assert_eq!(
        entitlement(&e, t, "u-jay").await,
        Some(("active".to_string(), Some("lic-u-jay".to_string())))
    );
}

/// The other order: the revoke landed (audited `license.revoke`) before the store of the id licences had answered
/// earlier. The store refuses it (`Stale`) and the row stays `pending` for a fresh mint.
#[tokio::test]
async fn a_store_of_an_id_revoked_since_is_stale() {
    let (e, up, t) = setup().await;
    licensed_member(&e, t, "u-kat", "tenant_user").await;
    let (s, b) = post(&e, "/api/tenant/users/u-kat/disable", &admin()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(lock(&up.mock).revoked, ["lic-u-kat"]);
    sqlx::query(
        "UPDATE member_entitlement SET status = 'pending', license_id = NULL, license_subject_id = NULL, \
                revoked_at = NULL WHERE tenant_id = $1 AND user_sub = 'u-kat'",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    let issued = agent_api::identity::IssuedLicence {
        license_id: "lic-u-kat".into(),
        subject_id: "subject-1".into(),
        token_sealed: "v2:gone:AAAA".into(),
    };
    let mut tx = agent_api::tenancy::begin_scoped(&e.state.db, t)
        .await
        .unwrap();
    let stored = agent_api::identity::store_member_entitlement(&mut tx, t, "u-kat", &issued)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(stored, agent_api::identity::Stored::Stale);
    assert_eq!(
        entitlement(&e, t, "u-kat").await,
        Some(("pending".to_string(), None))
    );
}

async fn invite_named(e: &TestEnv, body: Value) -> (StatusCode, Value) {
    send(
        &e.app,
        json("POST", "/api/tenant/invites", Some(&admin()), body),
    )
    .await
}

#[tokio::test]
async fn invite_explicit_names_win_so_the_invitee_only_sets_a_password() {
    let (e, up, t) = setup().await;
    let (s, b) = invite_named(
        &e,
        json!({"email": "zoe@alpha.test", "roles": common::role_keys("tenant_user"),
               "display_name": "Zed", "first_name": "Zoë", "last_name": "van der Berg"}),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    {
        let g = lock(&up.mock);
        assert_eq!(g.created_reps[0]["firstName"], "Zoë");
        assert_eq!(g.created_reps[0]["lastName"], "van der Berg");
    }
    // without a display name the member is shown by the explicit names
    let (s, b) = invite_named(
        &e,
        json!({"email": "max@alpha.test", "roles": common::role_keys("tenant_user"),
               "first_name": "Max", "last_name": "Muster"}),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let shown: String = sqlx::query_scalar(
        "SELECT display_name FROM users WHERE tenant_id = $1 AND lower(email) = 'max@alpha.test'",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(shown, "Max Muster");
    // no explicit names: the display name is split (commonpractice splitName)
    let (s, b) = invite_named(
        &e,
        json!({"email": "ann@alpha.test", "roles": common::role_keys("tenant_user"),
               "display_name": "Ann  Early Bird"}),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let g = lock(&up.mock);
    assert_eq!(g.created_reps[2]["firstName"], "Ann");
    assert_eq!(g.created_reps[2]["lastName"], "Early Bird");
}

#[tokio::test]
async fn invite_bad_name_422_and_nothing_created() {
    let (e, up, _t) = setup().await;
    let (s, b) = invite_named(
        &e,
        json!({"email": "zoe@alpha.test", "roles": common::role_keys("tenant_user"),
               "first_name": "Zoë", "last_name": "a\nb"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "invalid_person_name");
    assert!(lock(&up.mock).created.is_empty());
}

// ---- B1b T8: a hung licences service cannot drain the pool --------------------------------------------------------

#[tokio::test]
async fn a_hung_licences_revoke_is_bounded_and_releases_the_pool() {
    let up = fakes::upstream().await;
    let e = common::env_with(|v| {
        fakes::identity_env(&up, v);
        v.insert("LICENCES_REVOKE_TIMEOUT_MS".into(), "300".into());
    })
    .await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;
    // more members than the pool has connections (10), each holding a licence
    let subs: Vec<String> = (0..12).map(|i| format!("u-m{i}")).collect();
    for s in &subs {
        licensed_member(&e, t, s, "tenant_user").await;
    }
    // licences accepts the connection and never answers in time
    lock(&up.mock).revoke_delay_ms = 30_000;
    let started = std::time::Instant::now();
    let mut calls = tokio::task::JoinSet::new();
    for s in &subs {
        let app = e.app.clone();
        let uri = format!("/api/tenant/users/{s}/disable");
        calls.spawn(async move { send(&app, json("POST", &uri, Some(&admin()), json!({}))).await });
    }
    let all = tokio::time::timeout(std::time::Duration::from_secs(8), calls.join_all())
        .await
        .expect("every disable answers within the bound, not the licences hang");
    for (s, b) in &all {
        assert_eq!(*s, StatusCode::OK, "the disable stands: {b}");
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    // the rows wait for the tick, audited as unavailable
    for s in &subs {
        assert_eq!(entitlement(&e, t, s).await.unwrap().0, "revoking", "{s}");
    }
    let unavailable: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action = 'license.revoke_failed' \
          AND details->>'code' = 'licences_unavailable'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(unavailable, 12);
    // every pooled connection is back: none is held by an abandoned licences call
    let mut idle = false;
    for _ in 0..50 {
        if e.state.db.num_idle() == e.state.db.size() as usize {
            idle = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        idle,
        "pool {} open, {} idle",
        e.state.db.size(),
        e.state.db.num_idle()
    );
    // and the API still answers at once
    let quick = std::time::Instant::now();
    let (s, _) = send(&e.app, get("/api/tenant/users/activity", Some(&admin()))).await;
    assert_eq!(s, StatusCode::OK);
    assert!(quick.elapsed() < std::time::Duration::from_secs(1));
}

// ---- B1b T11: an invite's access email outcome is audited on success too ------------------------------------------

#[tokio::test]
async fn invite_email_success_is_audited() {
    let (e, _up, t) = setup().await;
    let (s, b) = invite(&e, "ann@example.test", "tenant_user").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let sub = b["sub"].as_str().unwrap();
    let rows: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT status, resource_id, details FROM audit_event \
          WHERE tenant_id = $1 AND action = 'user.invite_email'",
    )
    .bind(t)
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].0.as_str(), rows[0].1.as_str()), ("success", sub));
    assert_eq!(rows[0].2["via"], "invite");
    assert_eq!(rows[0].2["access"], "set_password");
    assert!(!rows[0].2.to_string().contains('@'));
}
