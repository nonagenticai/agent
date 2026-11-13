//! B1 T8: the tenant / job request context (crate::tenancy), `GET /api/me`, member sessions and the invite-accept
//! licence mint (D77 L5). The guards are exercised through probe routes mounted on the real `AppState`. Real
//! Postgres 18 (one container for this binary).
use crate::common;

use agent_api::audit::Actor;
use agent_api::members::{MintOutcome, ensure_member_licence};
use agent_api::tenancy::{JobMember, Member, Requires, SaInTenant, TenantAdmin, TenantCtx};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use common::fakes::{self, Upstream, lock};
use common::{TestEnv, insert_tenant, insert_user, person, send, sign, super_admin};
use serde_json::{Value, json};
use uuid::Uuid;

fn ctx_json(t: &TenantCtx) -> Value {
    json!({"tenant": t.tenant, "status": t.status.as_str(), "roles": t.roles.keys(), "effective": t.effective.keys(),
           "platform": t.platform_actor})
}

fn probe(e: &TestEnv) -> Router {
    Router::new()
        .route(
            "/probe/member",
            get(|Member(t): Member| async move { Json(ctx_json(&t)) }),
        )
        .route(
            "/probe/admin",
            get(|Requires(t, _): TenantAdmin| async move { Json(ctx_json(&t)) }),
        )
        .route(
            "/probe/sa",
            get(|SaInTenant(t): SaInTenant| async move { Json(ctx_json(&t)) }),
        )
        .route(
            "/probe/jobs/{job}",
            get(|JobMember(j): JobMember| async move {
                Json(json!({"job": j.job_key, "job_id": j.job_id, "tenant": j.t.tenant}))
            }),
        )
        .with_state(e.state.clone())
}

fn req(uri: &str, token: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::get(uri).header("authorization", format!("Bearer {token}"));
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::empty()).unwrap()
}

/// A person token carrying a Keycloak `sid`.
fn person_sid(sub: &str, sid: &str) -> String {
    sign(&json!({
        "sub": sub, "preferred_username": sub, "exp": common::in_five_minutes(),
        "iss": common::TEST_ISSUER, "aud": common::TEST_AUDIENCE, "azp": common::TEST_AUDIENCE,
        "sid": sid, "resource_access": {}
    }))
}

async fn set_status(e: &TestEnv, t: Uuid, status: &str) {
    let sql = match status {
        "archived" => "UPDATE tenants SET status = 'archived', archived_at = now() WHERE id = $1",
        "suspended" => {
            "UPDATE tenants SET status = 'suspended', suspended_at = now() WHERE id = $1"
        }
        _ => panic!("status {status}"),
    };
    sqlx::query(sql).bind(t).execute(&e.owner).await.unwrap();
}

async fn tenant_with_member(e: &TestEnv, name: &str, sub: &str, role: &str) -> Uuid {
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, name).await;
    insert_user(&e.owner, sub, t, role).await;
    t
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn member_of_archived_tenant_is_403_tenant_archived() {
    let e = common::env().await;
    let t = tenant_with_member(&e, "Alpha", "u-1", "tenant_admin").await;
    set_status(&e, t, "archived").await;
    let (s, b) = send(&probe(&e), req("/probe/member", &person("u-1", &[]), &[])).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "tenant_archived"),
        "{b}"
    );
}

#[tokio::test]
async fn sa_entering_archived_is_403_tenant_archived() {
    let e = common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    set_status(&e, t, "archived").await;
    let h = t.to_string();
    let (s, b) = send(
        &probe(&e),
        req(
            "/probe/member",
            &super_admin(),
            &[("x-active-tenant-id", &h)],
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "tenant_archived"),
        "{b}"
    );
    let other = Uuid::new_v4().to_string();
    let (s, b) = send(
        &probe(&e),
        req(
            "/probe/member",
            &super_admin(),
            &[("x-active-tenant-id", &other)],
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "tenant_not_found"),
        "{b}"
    );
}

#[tokio::test]
async fn sa_may_enter_suspended() {
    let e = common::env().await;
    let t = tenant_with_member(&e, "Alpha", "u-1", "tenant_user").await;
    set_status(&e, t, "suspended").await;
    let h = t.to_string();
    for uri in ["/probe/member", "/probe/admin", "/probe/sa"] {
        let (s, b) = send(
            &probe(&e),
            req(uri, &super_admin(), &[("x-active-tenant-id", &h)]),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{uri}: {b}");
        assert_eq!(b["status"], "suspended");
        assert_eq!(
            b["roles"],
            json!(common::ALL_ROLES),
            "D88: a super_admin in a tenant holds every role"
        );
        assert_eq!(b["platform"], true);
    }
    // the member itself is refused: the tenant is suspended
    let (s, b) = send(&probe(&e), req("/probe/member", &person("u-1", &[]), &[])).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "tenant_suspended"),
        "{b}"
    );
}

#[tokio::test]
async fn member_of_suspended_is_403_tenant_suspended() {
    let e = common::env().await;
    let t = tenant_with_member(&e, "Alpha", "u-1", "tenant_admin").await;
    set_status(&e, t, "suspended").await;
    let (s, b) = send(&probe(&e), req("/probe/member", &person("u-1", &[]), &[])).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "tenant_suspended"),
        "{b}"
    );
}

#[tokio::test]
async fn disabled_member_is_403_not_provisioned() {
    let e = common::env().await;
    tenant_with_member(&e, "Alpha", "u-1", "tenant_admin").await;
    let (s, _) = send(&probe(&e), req("/probe/member", &person("u-1", &[]), &[])).await;
    assert_eq!(s, StatusCode::OK, "control: enabled");
    sqlx::query("UPDATE users SET disabled_at = now() WHERE sub = 'u-1'")
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = send(&probe(&e), req("/probe/member", &person("u-1", &[]), &[])).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "not_provisioned"),
        "{b}"
    );
    let (s, b) = send(
        &probe(&e),
        req("/probe/member", &person("u-nobody", &[]), &[]),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "not_provisioned"),
        "{b}"
    );
}

#[tokio::test]
async fn repeated_header_is_400_invalid_active_tenant() {
    let e = common::env().await;
    let t = tenant_with_member(&e, "Alpha", "u-1", "tenant_admin").await;
    let h = t.to_string();
    for headers in [
        vec![
            ("x-active-tenant-id", h.as_str()),
            ("x-active-tenant-id", h.as_str()),
        ],
        vec![("x-active-tenant-id", "not-a-uuid")],
        vec![("x-active-tenant-id", "00000000-0000-0000-0000-000000000000")],
    ] {
        let (s, b) = send(&probe(&e), req("/probe/member", &super_admin(), &headers)).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::BAD_REQUEST, "invalid_active_tenant"),
            "{headers:?}: {b}"
        );
        let (s, b) = send(&e.app, req("/api/me", &person("u-1", &[]), &headers)).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::BAD_REQUEST, "invalid_active_tenant"),
            "/api/me {headers:?}: {b}"
        );
    }
}

#[tokio::test]
async fn sa_without_home_or_header_is_409_active_tenant_required() {
    let e = common::env().await;
    let (s, b) = send(&probe(&e), req("/probe/member", &super_admin(), &[])).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "active_tenant_required"),
        "{b}"
    );
    // with a home tenant the super_admin acts there, holding every role, not as a platform actor
    let t = tenant_with_member(&e, "Alpha", "u-super", "tenant_user").await;
    let (s, b) = send(&probe(&e), req("/probe/member", &super_admin(), &[])).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["tenant"], t.to_string());
    assert_eq!(b["roles"], json!(common::ALL_ROLES));
    assert_eq!(b["platform"], false);
}

#[tokio::test]
async fn member_header_for_other_tenant_is_403_forbidden() {
    let e = common::env().await;
    let a = tenant_with_member(&e, "Alpha", "u-1", "tenant_user").await;
    let b_t = Uuid::new_v4();
    insert_tenant(&e.owner, b_t, "Beta").await;
    let other = b_t.to_string();
    let (s, b) = send(
        &probe(&e),
        req(
            "/probe/member",
            &person("u-1", &[]),
            &[("x-active-tenant-id", &other)],
        ),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{b}");
    let own = a.to_string();
    let (s, b) = send(
        &probe(&e),
        req(
            "/probe/member",
            &person("u-1", &[]),
            &[("x-active-tenant-id", &own)],
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "own tenant by header: {b}");
    assert_eq!(b["roles"], json!([]));
    // an observer is not a tenant_admin, and not a super_admin
    let (s, b) = send(&probe(&e), req("/probe/admin", &person("u-1", &[]), &[])).await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{b}");
    let (s, b) = send(&probe(&e), req("/probe/sa", &person("u-1", &[]), &[])).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "super_admin_required"),
        "{b}"
    );
}

#[tokio::test]
async fn sa_switch_audits_once_in_target() {
    let e = common::env().await;
    let (a, b_t) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, a, "Alpha").await;
    insert_tenant(&e.owner, b_t, "Beta").await;
    let (ha, hb) = (a.to_string(), b_t.to_string());
    for h in [&ha, &ha, &hb] {
        let (s, b) = send(
            &probe(&e),
            req(
                "/probe/member",
                &super_admin(),
                &[("x-active-tenant-id", h)],
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
    let rows: Vec<(Uuid, String, bool, Value)> = sqlx::query_as(
        "SELECT tenant_id, actor_id, platform_actor, details FROM audit_event \
          WHERE action = 'tenant.switch' ORDER BY id",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(
        (rows[0].0, rows[0].1.as_str(), rows[0].2),
        (a, "u-super", true)
    );
    assert_eq!(rows[0].3["from"], Value::Null);
    assert_eq!(rows[1].0, b_t, "audited in the target tenant");
    assert_eq!(rows[1].3["from"], ha.as_str());
}

#[tokio::test]
async fn me_reports_header_tenant_for_sa() {
    let e = common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    let h = t.to_string();
    let (s, b) = send(
        &e.app,
        req("/api/me", &super_admin(), &[("x-active-tenant-id", &h)]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["tenant"]["id"], h);
    assert_eq!(b["roles"], json!(common::ALL_ROLES));
    assert_eq!(b["active_tenant"], h);
    // archived is reported, not refused
    set_status(&e, t, "archived").await;
    let (s, b) = send(
        &e.app,
        req("/api/me", &super_admin(), &[("x-active-tenant-id", &h)]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["tenant"]["status"], "archived");
    let missing = Uuid::new_v4().to_string();
    let (s, b) = send(
        &e.app,
        req(
            "/api/me",
            &super_admin(),
            &[("x-active-tenant-id", &missing)],
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "tenant_not_found"),
        "{b}"
    );
}

#[tokio::test]
async fn job_guard_404_job_not_found_in_other_tenant() {
    let e = common::env().await;
    let a = tenant_with_member(&e, "Alpha", "u-a", "tenant_user").await;
    let b_t = tenant_with_member(&e, "Beta", "u-b", "tenant_user").await;
    let job: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'faq_agent', 'FAQ', '[]', 't') \
         RETURNING id",
    )
    .bind(b_t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let (s, b) = send(
        &probe(&e),
        req("/probe/jobs/faq_agent", &person("u-a", &[]), &[]),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "job_not_found"),
        "{b}"
    );
    let (s, b) = send(
        &probe(&e),
        req("/probe/jobs/faq_agent", &person("u-b", &[]), &[]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["job_id"], job.to_string());
    assert_eq!(b["tenant"], b_t.to_string());
    let _ = a;
}

#[tokio::test]
async fn seen_is_throttled_and_clears_invited_at() {
    let e = common::env().await;
    let t = tenant_with_member(&e, "Alpha", "u-1", "tenant_user").await;
    sqlx::query("UPDATE users SET invited_at = now(), origin = 'invite' WHERE sub = 'u-1'")
        .execute(&e.owner)
        .await
        .unwrap();
    let tok = person_sid("u-1", "kc-session-1");
    let (s, _) = send(&e.app, req("/api/me", &tok, &[])).await;
    assert_eq!(s, StatusCode::OK);
    let rows: Vec<(Uuid, String, String)> =
        sqlx::query_as("SELECT tenant_id, sid, surface FROM member_session WHERE sub = 'u-1'")
            .fetch_all(&e.owner)
            .await
            .unwrap();
    assert_eq!(rows, [(t, "kc-session-1".to_string(), "admin".to_string())]);
    let invited: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT invited_at FROM users WHERE sub = 'u-1'")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert!(
        invited.is_none(),
        "the first seen request accepts the invite"
    );
    // within 5 minutes the same (sub, session, surface) writes nothing
    sqlx::query("UPDATE member_session SET last_seen_at = '2026-01-01T00:00:00Z'")
        .execute(&e.owner)
        .await
        .unwrap();
    send(&e.app, req("/api/me", &tok, &[])).await;
    send(&probe(&e), req("/probe/member", &tok, &[])).await;
    let old: bool = sqlx::query_scalar(
        "SELECT last_seen_at = '2026-01-01T00:00:00Z' FROM member_session WHERE sub = 'u-1'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(old, "throttled");
    // another surface is another key
    send(
        &probe(&e),
        req("/probe/member", &tok, &[("x-agent-surface", "chat")]),
    )
    .await;
    let (surface, fresh): (String, bool) = sqlx::query_as(
        "SELECT surface, last_seen_at > now() - interval '1 minute' FROM member_session WHERE sub = 'u-1'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!((surface.as_str(), fresh), ("chat", true));
}

#[tokio::test]
async fn seen_failure_does_not_fail_request() {
    let handle = agent_api::observability::metrics_handle();
    let e = common::env().await;
    tenant_with_member(&e, "Alpha", "u-1", "tenant_user").await;
    sqlx::query("REVOKE INSERT, UPDATE ON member_session FROM agent_app")
        .execute(&e.owner)
        .await
        .unwrap();
    let tok = person_sid("u-1", "kc-session-9");
    let (s, b) = send(&e.app, req("/api/me", &tok, &[])).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(
        &probe(&e),
        req("/probe/member", &tok, &[("x-agent-surface", "console")]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM member_session")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
    assert!(
        handle.render().contains("agent_member_seen_errors_total"),
        "the failure is counted"
    );
}

async fn identity_env() -> (TestEnv, Upstream) {
    let up = fakes::upstream().await;
    let e = common::env_with(|v| fakes::identity_env(&up, v)).await;
    (e, up)
}

async fn pending_member(e: &TestEnv, sub: &str) -> Uuid {
    let t = tenant_with_member(e, "Globex", sub, "tenant_user").await;
    sqlx::query("INSERT INTO member_entitlement (tenant_id, user_sub) VALUES ($1, $2)")
        .bind(t)
        .bind(sub)
        .execute(&e.owner)
        .await
        .unwrap();
    t
}

async fn entitlement(e: &TestEnv, sub: &str) -> (String, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT status, license_id, error_code FROM member_entitlement WHERE user_sub = $1",
    )
    .bind(sub)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn me_mints_pending_invite_entitlement_once() {
    let (e, up) = identity_env().await;
    let t = pending_member(&e, "u-1").await;
    let (s, b) = send(&e.app, req("/api/me", &person("u-1", &[]), &[])).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (status, lic, err) = entitlement(&e, "u-1").await;
    assert_eq!(status, "active");
    assert!(lic.is_some());
    assert!(err.is_none());
    {
        let g = lock(&up.mock);
        assert_eq!(g.minted, 1);
        assert_eq!(g.issued, [(t.to_string(), "u-1".to_string())]);
    }
    let audited: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = 'license.issue'")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(audited, 1);
    let (s, _) = send(&e.app, req("/api/me", &person("u-1", &[]), &[])).await;
    assert_eq!(s, StatusCode::OK);
    let g = lock(&up.mock);
    assert_eq!(g.minted, 1, "the second /api/me makes no licences call");
    assert_eq!(g.issued.len(), 1);
}

#[tokio::test]
async fn me_mint_failure_keeps_pending_and_answers_200() {
    let (e, up) = identity_env().await;
    pending_member(&e, "u-1").await;
    lock(&up.mock).mint_down = true;
    let (s, b) = send(&e.app, req("/api/me", &person("u-1", &[]), &[])).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["tenant"]["name"], "Globex");
    let (status, lic, err) = entitlement(&e, "u-1").await;
    assert_eq!(status, "pending");
    assert!(lic.is_none());
    assert_eq!(err.as_deref(), Some("licences_unavailable"));
}

#[tokio::test]
async fn me_concurrent_mint_lost_race_revokes_the_loser() {
    let (e, up) = identity_env().await;
    let t = pending_member(&e, "u-1").await;
    // both runners read `pending` before either stores: the mint is slow
    lock(&up.mock).mint_delay_ms = 400;
    let actor = Actor::member("u-1", false);
    let (r1, r2) = tokio::join!(
        ensure_member_licence(&e.state, t, "u-1", &actor),
        ensure_member_licence(&e.state, t, "u-1", &actor),
    );
    let mut outcomes = [r1.unwrap(), r2.unwrap()];
    outcomes.sort_by_key(|o| matches!(o, MintOutcome::LostRace));
    let MintOutcome::Stored(winner) = outcomes[0].clone() else {
        panic!("one runner stores: {outcomes:?}");
    };
    assert_eq!(outcomes[1], MintOutcome::LostRace, "{outcomes:?}");
    let (status, lic, _) = entitlement(&e, "u-1").await;
    assert_eq!(
        (status.as_str(), lic.as_deref()),
        ("active", Some(winner.as_str()))
    );
    let g = lock(&up.mock);
    assert_eq!(g.minted, 2);
    assert_eq!(g.revoked.len(), 1, "the loser's licence is revoked");
    assert_ne!(g.revoked[0], winner, "never the winner's");
}
