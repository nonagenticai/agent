//! Licence gate 2 at tenant-context resolution and `GET /api/license/status` (spec 04 §4b, C1;
//! crate::licensing::gate::{check_request, status}). Real Postgres 18 (one container for this binary). The gate reads
//! the stored verdict (`tenant_licence_verdict`) and never calls licences: `LICENSE_INTROSPECT_URL` points at a port
//! nothing listens on, so a test that reached licences would fail.
use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{TestEnv, get, insert_tenant, insert_user, licence_active, person, send, super_admin};
use serde_json::{Value, json};
use uuid::Uuid;

/// Gate 2 on.
async fn gated() -> TestEnv {
    common::env_with(|v| {
        v.insert("LICENSE_GATE_2_ENABLED".into(), "true".into());
        v.insert(
            "LICENSE_INTROSPECT_URL".into(),
            format!("{}/license/introspect", common::NO_UPSTREAM),
        );
    })
    .await
}

fn in_tenant(mut r: Request<Body>, t: Uuid) -> Request<Body> {
    r.headers_mut()
        .insert("x-active-tenant-id", t.to_string().parse().unwrap());
    r
}

/// A tenant with a tenant_admin `u-<name>`.
async fn tenant(e: &TestEnv, name: &str) -> Uuid {
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, name).await;
    insert_user(&e.owner, &format!("u-{name}"), t, "tenant_admin").await;
    t
}

/// Overwrite `tenant`'s verdict: `status`, `reason`, checked `age_seconds` ago.
async fn verdict(e: &TestEnv, tenant: Uuid, status: &str, reason: Option<&str>, age_seconds: i64) {
    sqlx::query(
        "INSERT INTO tenant_licence_verdict (tenant_id, status, reason, licence_id, checked_at) \
         VALUES ($1, $2, $3, 'lic-x', now() - make_interval(secs => $4)) \
         ON CONFLICT (tenant_id) DO UPDATE \
         SET status = EXCLUDED.status, reason = EXCLUDED.reason, checked_at = EXCLUDED.checked_at",
    )
    .bind(tenant)
    .bind(status)
    .bind(reason)
    .bind(age_seconds as f64)
    .execute(&e.owner)
    .await
    .unwrap();
}

async fn jobs_as(e: &TestEnv, sub: &str) -> (StatusCode, Value) {
    send(&e.app, get("/api/jobs", Some(&person(sub, &[])))).await
}

async fn status_as(e: &TestEnv, token: &str) -> (StatusCode, Value) {
    send(&e.app, get("/api/license/status", Some(token))).await
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn flag_off_changes_nothing() {
    let e = common::env().await;
    let t = tenant(&e, "off").await;
    // no verdict row at all: the request gate is off
    let (s, body) = jobs_as(&e, "u-off").await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, body) = status_as(&e, &person("u-off", &[])).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        body,
        json!({"enforced": false, "tenant": t, "state": "not_enforced", "code": null})
    );
}

#[tokio::test]
async fn an_active_fresh_verdict_passes() {
    let e = gated().await;
    let t = tenant(&e, "fresh").await;
    licence_active(&e.owner, t).await;
    let (s, body) = jobs_as(&e, "u-fresh").await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, body) = status_as(&e, &person("u-fresh", &[])).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        body,
        json!({"enforced": true, "tenant": t, "state": "active", "code": null})
    );
}

#[tokio::test]
async fn a_tenant_without_a_licence_is_402_license_required() {
    let e = gated().await;
    tenant(&e, "nolic").await;
    let (s, body) = jobs_as(&e, "u-nolic").await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{body}");
    assert_eq!(code(&body), "license_required");
}

#[tokio::test]
async fn license_status_is_never_402() {
    let e = gated().await;
    let t = tenant(&e, "blocked").await;
    let (s, body) = status_as(&e, &person("u-blocked", &[])).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["enforced"], true);
    assert_eq!(body["tenant"], t.to_string());
    assert_eq!(body["state"], "blocked");
    assert_eq!(body["code"], "license_required");
    assert!(body["message"].is_string());
    // a super_admin names the tenant; without one (and no home tenant) it is `no_workspace`
    let (s, body) = send(
        &e.app,
        in_tenant(get("/api/license/status", Some(&super_admin())), t),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["state"], "blocked");
    assert_eq!(body["tenant"], t.to_string());
    let (s, body) = status_as(&e, &super_admin()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        body,
        json!({"enforced": true, "tenant": null, "state": "no_workspace", "code": null})
    );
    // a member's header naming another tenant is ignored: the answer is about their own tenant
    let other = tenant(&e, "other").await;
    licence_active(&e.owner, other).await;
    let (s, body) = send(
        &e.app,
        in_tenant(
            get("/api/license/status", Some(&person("u-blocked", &[]))),
            other,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["tenant"], t.to_string());
    assert_eq!(body["state"], "blocked");
}

#[tokio::test]
async fn a_stale_active_verdict_is_402_license_unavailable() {
    let e = gated().await;
    let t = tenant(&e, "stale").await;
    verdict(
        &e,
        t,
        "active",
        None,
        agent_api::licensing::verdict::MAX_AGE_SECONDS + 60,
    )
    .await;
    let (s, body) = jobs_as(&e, "u-stale").await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{body}");
    assert_eq!(code(&body), "license_unavailable");
    let (_, body) = status_as(&e, &person("u-stale", &[])).await;
    assert_eq!(body["state"], "blocked");
    assert_eq!(body["code"], "license_unavailable");
}

#[tokio::test]
async fn definite_refusals_carry_their_codes() {
    let e = gated().await;
    let t = tenant(&e, "codes").await;
    for (status, reason, want_status, want) in [
        (
            "inactive",
            Some("license_unavailable"),
            StatusCode::PAYMENT_REQUIRED,
            "license_unavailable",
        ),
        (
            "inactive",
            Some("license_unreadable"),
            StatusCode::PAYMENT_REQUIRED,
            "license_unreadable",
        ),
        (
            "none",
            Some("no_licence"),
            StatusCode::PAYMENT_REQUIRED,
            "license_required",
        ),
        (
            "expired",
            Some("license_expired"),
            StatusCode::PAYMENT_REQUIRED,
            "license_expired",
        ),
        (
            "revoked",
            Some("token_invalid"),
            StatusCode::PAYMENT_REQUIRED,
            "license_inactive",
        ),
        (
            "inactive",
            Some("app_mismatch"),
            StatusCode::FORBIDDEN,
            "license_app_mismatch",
        ),
        (
            "inactive",
            Some("tenant_mismatch"),
            StatusCode::FORBIDDEN,
            "license_tenant_mismatch",
        ),
        (
            "inactive",
            Some("tenant_suspended"),
            StatusCode::FORBIDDEN,
            "tenant_suspended",
        ),
    ] {
        verdict(&e, t, status, reason, 1).await;
        let (s, body) = jobs_as(&e, "u-codes").await;
        assert_eq!(s, want_status, "{status}/{reason:?}: {body}");
        assert_eq!(code(&body), want, "{status}/{reason:?}");
        // the gate marks its refusals, so a client tells them from the tenant-status check's
        assert_eq!(
            body["error"]["gate"], "licence",
            "{status}/{reason:?}: {body}"
        );
    }
}

#[tokio::test]
async fn a_platform_owned_tenant_is_exempt() {
    let e = gated().await;
    let t = tenant(&e, "owned").await;
    sqlx::query("UPDATE tenants SET platform_owned = true WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    // no verdict row (and a refused one would not matter either)
    let (s, body) = jobs_as(&e, "u-owned").await;
    assert_eq!(s, StatusCode::OK, "{body}");
    verdict(&e, t, "revoked", Some("token_invalid"), 1).await;
    let (s, body) = jobs_as(&e, "u-owned").await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (_, body) = status_as(&e, &person("u-owned", &[])).await;
    assert_eq!(body["state"], "active");
}

#[tokio::test]
async fn a_super_admin_acting_in_a_tenant_is_gated_like_its_members() {
    let e = gated().await;
    let t = tenant(&e, "sa").await;
    let (s, body) = send(&e.app, in_tenant(get("/api/jobs", Some(&super_admin())), t)).await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{body}");
    assert_eq!(code(&body), "license_required");
    licence_active(&e.owner, t).await;
    let (s, body) = send(&e.app, in_tenant(get("/api/jobs", Some(&super_admin())), t)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn exempt_and_platform_routes_are_not_gated() {
    let e = gated().await;
    let t = tenant(&e, "exempt").await;
    let member = person("u-exempt", &[]);
    // the tenant has no verdict: its tenant routes are 402 ...
    let (s, _) = jobs_as(&e, "u-exempt").await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED);
    // ... but who-am-I, the view-as reset and the status route still answer
    let (s, body) = send(&e.app, get("/api/me", Some(&member))).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let req = Request::delete("/api/me/view-as")
        .header("authorization", format!("Bearer {member}"))
        .body(Body::empty())
        .unwrap();
    let (s, body) = send(&e.app, req).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, _) = status_as(&e, &member).await;
    assert_eq!(s, StatusCode::OK);
    // the super_admin's platform routes (no tenant resolved, or `/api/platform/*`) are reachable
    let (s, body) = send(&e.app, get("/api/tenants", Some(&super_admin()))).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, body) = send(
        &e.app,
        in_tenant(get("/api/platform/licensing", Some(&super_admin())), t),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    for path in ["/health", "/ready"] {
        let (s, body) = send(&e.app, get(path, None)).await;
        assert_ne!(s, StatusCode::PAYMENT_REQUIRED, "{path}: {body}");
    }
}

#[tokio::test]
async fn tenant_suspended_is_checked_before_gate_2() {
    let e = gated().await;
    let t = tenant(&e, "susp").await;
    licence_active(&e.owner, t).await;
    sqlx::query("UPDATE tenants SET status = 'suspended', suspended_at = now() WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, body) = jobs_as(&e, "u-susp").await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(code(&body), "tenant_suspended");
    // the status check's refusal, not the licence gate's: no `gate` mark
    assert!(body["error"].get("gate").is_none(), "{body}");
}

/// Tenant `name` (licence active) with an integrations manager `u-<name>` and the job `faq_agent`, and a key of that
/// job: (tenant, secret).
async fn keyed(e: &TestEnv, name: &str) -> (Uuid, String) {
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, name).await;
    insert_user(&e.owner, &format!("u-{name}"), t, "integrations_manager").await;
    licence_active(&e.owner, t).await;
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'faq_agent', 'J', '[]', 'test')",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = send(
        &e.app,
        common::json(
            "POST",
            "/api/jobs/faq_agent/api-keys",
            Some(&person(&format!("u-{name}"), &[])),
            json!({"label": "bridge"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    (t, b["secret"].as_str().unwrap().to_string())
}

/// A key caller's `GET` of an unknown delivery: 404 `delivery_not_found` once past the guard.
async fn delivery_by_key(e: &TestEnv, secret: &str) -> (StatusCode, Value) {
    let req = Request::get(format!(
        "/api/jobs/faq_agent/interactions/deliveries/int_{}",
        Uuid::new_v4()
    ))
    .header("x-api-key", secret)
    .body(Body::empty())
    .unwrap();
    send(&e.app, req).await
}

#[tokio::test]
async fn a_job_api_key_is_gated_like_a_member() {
    let e = gated().await;
    let (t, secret) = keyed(&e, "keyed").await;
    let (s, body) = delivery_by_key(&e, &secret).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(code(&body), "delivery_not_found");
    for (status, reason, want_status, want) in [
        (
            "expired",
            Some("license_expired"),
            StatusCode::PAYMENT_REQUIRED,
            "license_expired",
        ),
        (
            "inactive",
            Some("app_mismatch"),
            StatusCode::FORBIDDEN,
            "license_app_mismatch",
        ),
        (
            "active",
            None,
            StatusCode::PAYMENT_REQUIRED,
            "license_unavailable",
        ),
    ] {
        let age = if status == "active" {
            agent_api::licensing::verdict::MAX_AGE_SECONDS + 60
        } else {
            1
        };
        verdict(&e, t, status, reason, age).await;
        let (s, body) = delivery_by_key(&e, &secret).await;
        assert_eq!(s, want_status, "{status}/{reason:?}: {body}");
        assert_eq!(code(&body), want, "{status}/{reason:?}");
    }
    // no verdict at all: 402 license_required, as a member gets
    sqlx::query("DELETE FROM tenant_licence_verdict WHERE tenant_id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, body) = delivery_by_key(&e, &secret).await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{body}");
    assert_eq!(code(&body), "license_required");
}

#[tokio::test]
async fn a_job_api_key_is_not_gated_with_the_flag_off() {
    let e = common::env().await;
    let (t, secret) = keyed(&e, "keyoff").await;
    sqlx::query("DELETE FROM tenant_licence_verdict WHERE tenant_id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, body) = delivery_by_key(&e, &secret).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(code(&body), "delivery_not_found");
}

#[tokio::test]
async fn member_tenant_reads_are_gated_and_super_admin_reads_are_not() {
    let e = gated().await;
    let t = tenant(&e, "reads").await;
    let member = person("u-reads", &[]);
    for path in [
        format!("/api/tenants/{t}"),
        format!("/api/tenants/{t}/users"),
    ] {
        // no verdict: the member is refused with the gate's code
        let (s, body) = send(&e.app, get(&path, Some(&member))).await;
        assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{path}: {body}");
        assert_eq!(code(&body), "license_required", "{path}");
        // the super_admin's platform tenant detail stays reachable
        let (s, body) = send(&e.app, get(&path, Some(&super_admin()))).await;
        assert_eq!(s, StatusCode::OK, "{path}: {body}");
    }
    verdict(&e, t, "inactive", Some("tenant_mismatch"), 1).await;
    let (s, body) = send(&e.app, get(&format!("/api/tenants/{t}"), Some(&member))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(code(&body), "license_tenant_mismatch");
    licence_active(&e.owner, t).await;
    for path in [
        format!("/api/tenants/{t}"),
        format!("/api/tenants/{t}/users"),
    ] {
        let (s, body) = send(&e.app, get(&path, Some(&member))).await;
        assert_eq!(s, StatusCode::OK, "{path}: {body}");
    }
}

#[tokio::test]
async fn member_tenant_reads_are_not_gated_with_the_flag_off() {
    let e = common::env().await;
    let t = tenant(&e, "readsoff").await;
    let (s, body) = send(
        &e.app,
        get(
            &format!("/api/tenants/{t}"),
            Some(&person("u-readsoff", &[])),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
}
