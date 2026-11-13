//! B1 T21 (D79, plan §R.3–R.4): narrow-only view-as. `PUT /api/me/view-as` narrows the held roles for 60 s..8 h,
//! `DELETE` ends it (always allowed), both answer `Me`; the view is a server-side row (member / per-tenant
//! super_admin), every role check reads the effective set, expiry is silent, and every audit row written under a view
//! carries `details.view_as`. Real Postgres 18 (one container for this binary).
use crate::common;

use agent_api::auth::{Role, RoleSet};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::fakes::{self, Upstream};
use common::router_walk::{self, Guard, RouteDef};
use common::{
    ALL_ROLES, TestEnv, get, insert_tenant, insert_user, json, person, send, sign, super_admin,
};
use serde_json::{Value, json};
use uuid::Uuid;

async fn setup() -> (TestEnv, Upstream, Uuid) {
    let up = fakes::upstream().await;
    let e = common::env_with(|v| fakes::identity_env(&up, v)).await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'j1', 'J1', '[]', 'test')",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    (e, up, t)
}

fn with_tenant(mut r: Request<Body>, t: Uuid) -> Request<Body> {
    r.headers_mut()
        .insert("x-active-tenant-id", t.to_string().parse().unwrap());
    r
}

fn admin() -> String {
    person("u-admin", &[])
}

/// The non-GET routes that demand a role, with that role (the router walk, tests/common/router_walk.rs).
fn role_routes() -> Vec<(RouteDef, String)> {
    router_walk::routes()
        .into_iter()
        .filter(|r| r.method != "GET")
        .filter_map(|r| {
            router_walk::guards(&r.params)
                .into_iter()
                .find_map(|g| match g {
                    Guard::Role(k) => Some(k),
                    _ => None,
                })
                .map(|k| (r, k))
        })
        .collect()
}

async fn probe(
    e: &TestEnv,
    r: &RouteDef,
    token: &str,
    sa_tenant: Option<Uuid>,
) -> (StatusCode, Value) {
    let uri = router_walk::fill(&r.path, "j1", "u-ghost");
    let mut req = json(&r.method, &uri, Some(token), json!({}));
    if let Some(t) = sa_tenant {
        req = with_tenant(req, t);
    }
    send(&e.app, req).await
}

fn refused(s: StatusCode, b: &Value) -> bool {
    s == StatusCode::FORBIDDEN && b["error"]["missing_role"].is_string()
}

async fn view_as(
    e: &TestEnv,
    token: &str,
    body: Value,
    sa_tenant: Option<Uuid>,
) -> (StatusCode, Value) {
    let mut req = json("PUT", "/api/me/view-as", Some(token), body);
    if let Some(t) = sa_tenant {
        req = with_tenant(req, t);
    }
    send(&e.app, req).await
}

async fn clear(e: &TestEnv, token: &str, sa_tenant: Option<Uuid>) -> (StatusCode, Value) {
    let mut req = json("DELETE", "/api/me/view-as", Some(token), json!({}));
    if let Some(t) = sa_tenant {
        req = with_tenant(req, t);
    }
    send(&e.app, req).await
}

async fn audit_rows(e: &TestEnv, action: &str) -> Vec<Value> {
    sqlx::query_scalar("SELECT details FROM audit_event WHERE action = $1 ORDER BY id")
        .bind(action)
        .fetch_all(&e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn view_as_narrows_every_route() {
    let (e, _up, _t) = setup().await;
    let rr = role_routes();
    assert!(!rr.is_empty());
    let (s, b) = view_as(&e, &admin(), json!({"roles": []}), None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["roles"], json!(ALL_ROLES), "held roles are unchanged");
    assert_eq!(b["effective_roles"], json!([]));
    assert_eq!(b["view_as"]["roles"], json!([]));
    for (r, k) in &rr {
        let (s, b) = probe(&e, r, &admin(), None).await;
        assert!(
            refused(s, &b),
            "{} {} under an empty view: {s} {b}",
            r.method,
            r.path
        );
        assert_eq!(b["error"]["missing_role"], k.as_str());
    }
    // narrowed to exactly one role: that role's routes pass the guard, the others do not
    for role in ALL_ROLES {
        let (s, b) = view_as(&e, &admin(), json!({"roles": [role]}), None).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // D127: viewing as tenant_admin also holds the configuration roles it implies
        let eff = RoleSet::of(&[Role::parse(role).expect("role key")]).effective(None);
        assert_eq!(b["effective_roles"], json!(eff.keys()));
        for (r, k) in &rr {
            let (s, b) = probe(&e, r, &admin(), None).await;
            assert_eq!(
                refused(s, &b),
                !eff.has(Role::parse(k).expect("role key")),
                "view {role}: {} {} → {s} {b}",
                r.method,
                r.path
            );
        }
    }
}

#[tokio::test]
async fn view_as_cannot_widen() {
    let (e, _up, t) = setup().await;
    insert_user(&e.owner, "u-sa", t, "support_agent").await;
    let tok = person("u-sa", &[]);
    let (s, b) = view_as(&e, &tok, json!({"roles": ["tenant_admin"]}), None).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("view_as_not_narrowing")
        ),
        "{b}"
    );
    let (s, b) = view_as(
        &e,
        &tok,
        json!({"roles": ["support_agent", "operator"]}),
        None,
    )
    .await;
    assert_eq!(b["error"]["code"], "view_as_not_narrowing", "{s} {b}");
    for (body, code) in [
        (json!({"roles": ["boss"]}), "invalid_role"),
        (
            json!({"roles": ["support_agent", "support_agent"]}),
            "duplicate_role",
        ),
        (json!({"roles": [], "ttl_seconds": 59}), "invalid_ttl"),
        (json!({"roles": [], "ttl_seconds": 28_801}), "invalid_ttl"),
    ] {
        let (s, b) = view_as(&e, &tok, body.clone(), None).await;
        assert_eq!(
            (s, b["error"]["code"].as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, Some(code)),
            "{body}: {b}"
        );
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM member_view_as")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0, "a refused view stores nothing");
    // the same set, the bounds and the default ttl are fine
    let (s, b) = view_as(
        &e,
        &tok,
        json!({"roles": ["support_agent"], "ttl_seconds": 28_800}),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = view_as(&e, &tok, json!({"roles": []}), None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let left: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM expires_at - now())::float8 FROM member_view_as WHERE sub = 'u-sa'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(
        (3500.0..=3600.0).contains(&left),
        "default ttl is an hour: {left}"
    );
    // no active tenant: a super_admin without a home or header
    let (s, b) = view_as(&e, &super_admin(), json!({"roles": []}), None).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("active_tenant_required")),
        "{b}"
    );
}

#[tokio::test]
async fn view_as_expires_without_error() {
    let (e, _up, _t) = setup().await;
    let (s, _) = view_as(&e, &admin(), json!({"roles": []}), None).await;
    assert_eq!(s, StatusCode::OK);
    sqlx::query(
        "UPDATE member_view_as SET started_at = now() - interval '2 hours', expires_at = now() - interval '1 second'",
    )
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = send(&e.app, get("/api/me", Some(&admin()))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["view_as"].is_null(), "{b}");
    assert_eq!(b["effective_roles"], json!(ALL_ROLES));
    let (s, b) = send(
        &e.app,
        json(
            "PATCH",
            "/api/tenant/users/u-ghost",
            Some(&admin()),
            json!({"roles": []}),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("user_not_found")),
        "{b}"
    );
    assert!(
        audit_rows(&e, "user.view_as.end").await.is_empty(),
        "an expiry writes no row"
    );
    // clearing an expired view is a silent no-op
    let (s, _) = clear(&e, &admin(), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(audit_rows(&e, "user.view_as.end").await.is_empty());
}

#[tokio::test]
async fn clear_view_as_always_allowed_even_without_tenant_admin() {
    let (e, _up, t) = setup().await;
    let (s, _) = view_as(&e, &admin(), json!({"roles": ["support_agent"]}), None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = clear(&e, &admin(), None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["view_as"].is_null());
    assert_eq!(b["effective_roles"], json!(ALL_ROLES));
    assert_eq!(
        audit_rows(&e, "user.view_as.end").await,
        [json!({"reason": "cleared"})]
    );
    // idempotent: no view → 200, no row
    let (s, _) = clear(&e, &admin(), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(audit_rows(&e, "user.view_as.end").await.len(), 1);
    // an observer may always clear (and set an empty view)
    insert_user(&e.owner, "u-obs", t, "tenant_user").await;
    let obs = person("u-obs", &[]);
    let (s, b) = view_as(&e, &obs, json!({"roles": []}), None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = clear(&e, &obs, None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["roles"], json!([]));
}

#[tokio::test]
async fn view_as_survives_reload_and_sse() {
    let (e, _up, _t) = setup().await;
    let (s, _) = view_as(&e, &admin(), json!({"roles": ["job_designer"]}), None).await;
    assert_eq!(s, StatusCode::OK);
    // a reload: a new token for a new session, no header of any kind
    let reloaded = sign(&json!({
        "sub": "u-admin", "preferred_username": "u-admin", "exp": common::in_five_minutes(),
        "iss": common::TEST_ISSUER, "aud": common::TEST_AUDIENCE, "azp": common::TEST_AUDIENCE,
        "sid": "another-session", "resource_access": {}
    }));
    let (s, b) = send(&e.app, get("/api/me", Some(&reloaded))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["view_as"]["roles"], json!(["job_designer"]));
    assert_eq!(b["effective_roles"], json!(["job_designer"]));
    // an EventSource request carries no custom header: the server-side view still applies
    let req = Request::post("/api/tenant/users/u-ghost/disable")
        .header("authorization", format!("Bearer {reloaded}"))
        .header("accept", "text/event-stream")
        .body(Body::empty())
        .unwrap();
    let (s, b) = send(&e.app, req).await;
    assert!(refused(s, &b), "{s} {b}");
    assert_eq!(b["error"]["missing_role"], "tenant_admin");
}

#[tokio::test]
async fn audit_rows_under_view_as_carry_details_view_as() {
    let (e, _up, _t) = setup().await;
    let (s, _) = view_as(
        &e,
        &admin(),
        json!({"roles": ["support_agent", "tenant_admin"]}),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let start = audit_rows(&e, "user.view_as.start").await;
    assert_eq!(start.len(), 1);
    assert_eq!(start[0]["roles"], json!(["tenant_admin", "support_agent"]));
    assert!(start[0]["expires_at"].is_string());
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/tenant/invites",
            Some(&admin()),
            json!({"email": "kim@alpha.test", "roles": []}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let invite = audit_rows(&e, "user.invite").await;
    assert_eq!(
        invite[0]["view_as"],
        json!(["tenant_admin", "support_agent"])
    );
    // a second view replaces the first: end(replaced) then start
    let (s, _) = view_as(&e, &admin(), json!({"roles": ["tenant_admin"]}), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        audit_rows(&e, "user.view_as.end").await,
        [json!({"reason": "replaced"})]
    );
    assert_eq!(audit_rows(&e, "user.view_as.start").await.len(), 2);
    // without a view no row carries view_as
    let (s, _) = clear(&e, &admin(), None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/tenant/invites",
            Some(&admin()),
            json!({"email": "lee@alpha.test", "roles": []}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let invite = audit_rows(&e, "user.invite").await;
    assert!(invite[1].get("view_as").is_none(), "{}", invite[1]);
}

#[tokio::test]
async fn sa_view_as_is_per_tenant() {
    let (e, _up, a) = setup().await;
    let b_t = Uuid::new_v4();
    insert_tenant(&e.owner, b_t, "Beta").await;
    let sa = super_admin();
    let (s, b) = view_as(&e, &sa, json!({"roles": ["operator"]}), Some(a)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["roles"], json!(ALL_ROLES));
    assert_eq!(b["effective_roles"], json!(["operator"]));
    let disable = |t: Uuid| {
        with_tenant(
            json(
                "POST",
                "/api/tenant/users/u-ghost/disable",
                Some(&sa),
                json!({}),
            ),
            t,
        )
    };
    let (s, body) = send(&e.app, disable(a)).await;
    assert!(refused(s, &body), "narrowed in A: {s} {body}");
    let (s, body) = send(&e.app, disable(b_t)).await;
    assert!(!refused(s, &body), "B is not narrowed: {s} {body}");
    let (s, me) = send(&e.app, with_tenant(get("/api/me", Some(&sa)), b_t)).await;
    assert_eq!(s, StatusCode::OK);
    assert!(me["view_as"].is_null(), "{me}");
    // the super_admin's view is definer-only state, written to the tenant it was set in
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM super_admin_view_as WHERE tenant_id = $1")
            .bind(a)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(n, 1);
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action = 'user.view_as.start' AND tenant_id = $1 AND platform_actor",
    )
    .bind(a)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(rows, 1);
    // clearing in A restores A
    let (s, me) = clear(&e, &sa, Some(a)).await;
    assert_eq!(s, StatusCode::OK, "{me}");
    assert_eq!(me["effective_roles"], json!(ALL_ROLES));
    let (s, body) = send(&e.app, disable(a)).await;
    assert!(!refused(s, &body), "{s} {body}");
}
