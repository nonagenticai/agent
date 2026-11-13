//! B1 T13: tenant create / import / suspend / resume / offboard (crate::tenants::lifecycle) with the D77 licence order:
//! everything written in one transaction, licences minted and revoked only after COMMIT, a revoke's body checked.
//! Real Postgres 18 (one container for this binary); Keycloak and licences are the in-process fake.
use crate::common;

use agent_api::audit::Actor;
use agent_api::members::MintOutcome;
use agent_api::tenants::lifecycle::ensure_tenant_licence;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::fakes::{self, Upstream, lock};
use common::{TestEnv, get, insert_tenant, insert_user, json, person, send, super_admin};
use serde_json::{Map, Value, json};
use uuid::Uuid;

async fn setup() -> (TestEnv, Upstream) {
    let up = fakes::upstream().await;
    let e = common::env_with(|v| fakes::identity_env(&up, v)).await;
    (e, up)
}

async fn create(e: &TestEnv, name: &str, email: &str) -> (StatusCode, Value) {
    send(
        &e.app,
        json(
            "POST",
            "/api/tenants",
            Some(&super_admin()),
            json!({
                "name": name, "first_admin_email": email, "residency": "eu",
                "first_admin_first_name": "Ada", "first_admin_last_name": "Admin",
            }),
        ),
    )
    .await
}

fn tenant_of(b: &Value) -> Uuid {
    b["tenant"]["id"].as_str().unwrap().parse().unwrap()
}

async fn platform_count(e: &TestEnv, action: &str, status: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM platform_audit WHERE action = $1 AND status = $2")
        .bind(action)
        .bind(status)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn tenant_count(e: &TestEnv, t: Uuid, action: &str, status: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE tenant_id = $1 AND action = $2 AND status = $3",
    )
    .bind(t)
    .bind(action)
    .bind(status)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

/// A tenant with two members holding active licences and its own tenant licence.
async fn licensed_tenant(e: &TestEnv) -> Uuid {
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Offco").await;
    sqlx::query("UPDATE tenants SET licence_id = 'lic-tenant' WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    for sub in ["u-a", "u-b"] {
        insert_user(&e.owner, sub, t, "tenant_admin").await;
        sqlx::query(
            "INSERT INTO member_entitlement (tenant_id, user_sub, license_id, status) \
             VALUES ($1, $2, 'lic-' || $2, 'active')",
        )
        .bind(t)
        .bind(sub)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    t
}

async fn offboard(e: &TestEnv, t: Uuid, name: &str) -> (StatusCode, Value) {
    send(
        &e.app,
        json(
            "POST",
            &format!("/api/tenants/{t}/offboard"),
            Some(&super_admin()),
            json!({"confirm_name": name}),
        ),
    )
    .await
}

#[tokio::test]
async fn create_mints_tenant_and_member_licence_after_commit_and_seals_both() {
    let (e, up) = setup().await;
    let (s, b) = create(&e, "Acme", "ann@acme.test").await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let t = tenant_of(&b);
    assert_eq!(b["tenant"]["residency"], "eu");
    assert_eq!(b["tenant"]["licence"]["status"], "active");
    assert_eq!(b["invited"], true, "a new identity was invited");
    assert!(b["licence"]["id"].is_string(), "{b}");
    let (lic, token): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT licence_id, licence_token_encrypted FROM tenants WHERE id = $1")
            .bind(t)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(lic.as_deref(), b["licence"]["id"].as_str());
    assert!(
        token.unwrap().starts_with("v2:v1:"),
        "the tenant token is sealed"
    );
    let (status, member_token): (String, Option<String>) = sqlx::query_as(
        "SELECT status, token_encrypted FROM member_entitlement WHERE tenant_id = $1",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(status, "active");
    assert!(member_token.unwrap().starts_with("v2:v1:"));
    {
        let g = lock(&up.mock);
        assert_eq!(g.minted, 2, "tenant + member");
        assert_eq!(g.issued.len(), 2);
        assert!(g.issued[0].1.starts_with("agent-tenant:"), "{:?}", g.issued);
        assert_eq!(g.emails.len(), 1, "the first admin is invited once");
        // both names on the new identity: the portal profile is complete, so the invite link asks only for a password
        assert_eq!(g.created_reps[0]["firstName"], "Ada");
        assert_eq!(g.created_reps[0]["lastName"], "Admin");
    }
    let shown: String = sqlx::query_scalar("SELECT display_name FROM users WHERE tenant_id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(shown, "Ada Admin");
    // the licence rows were written after the creating transaction's audit rows
    let ids: Vec<(String, i64)> =
        sqlx::query_as("SELECT action, id FROM audit_event WHERE tenant_id = $1 ORDER BY id")
            .bind(t)
            .fetch_all(&e.owner)
            .await
            .unwrap();
    let pos = |a: &str| ids.iter().position(|(x, _)| x == a).unwrap();
    assert!(pos("tenant.create") < pos("license.issue"), "{ids:?}");
    assert_eq!(platform_count(&e, "tenant.create", "ok").await, 1);
    let (origin, invited): (String, bool) =
        sqlx::query_as("SELECT origin, invited_at IS NOT NULL FROM users WHERE tenant_id = $1")
            .bind(t)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!((origin.as_str(), invited), ("tenant_create", true));
}

#[tokio::test]
async fn create_licences_down_commits_tenant_with_pending_entitlement_platform_audited() {
    let (e, up) = setup().await;
    lock(&up.mock).licences_down = true;
    let (s, b) = create(&e, "Downco", "dan@down.test").await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert!(b["licence"].is_null());
    let t = tenant_of(&b);
    let lic: Option<String> = sqlx::query_scalar("SELECT licence_id FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert!(lic.is_none());
    let status: String =
        sqlx::query_scalar("SELECT status FROM member_entitlement WHERE tenant_id = $1")
            .bind(t)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(status, "pending");
    assert!(platform_count(&e, "license.issue_failed", "failed").await >= 1);
}

#[tokio::test]
async fn failure_before_commit_mints_nothing() {
    let (e, up) = setup().await;
    // the users insert fails inside the creating transaction
    sqlx::query(
        "CREATE FUNCTION test_boom() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RAISE EXCEPTION 'boom'; END $$",
    )
    .execute(&e.owner)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER test_boom BEFORE INSERT ON users FOR EACH ROW EXECUTE FUNCTION test_boom()",
    )
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = create(&e, "Boomco", "bo@boom.test").await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR, "{b}");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0, "the transaction rolled back");
    let g = lock(&up.mock);
    assert_eq!(g.minted, 0);
    assert!(g.issued.is_empty() && g.registered.is_empty() && g.calls.is_empty());
}

#[tokio::test]
async fn create_lost_race_revokes_the_losing_token() {
    let (e, up) = setup().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Raceco").await;
    lock(&up.mock).mint_delay_ms = 400;
    let actor = Actor::system("service:test");
    let (r1, r2) = tokio::join!(
        ensure_tenant_licence(&e.state, t, &actor),
        ensure_tenant_licence(&e.state, t, &actor),
    );
    let mut outcomes = [r1.unwrap(), r2.unwrap()];
    outcomes.sort_by_key(|o| matches!(o, MintOutcome::LostRace));
    let MintOutcome::Stored(winner) = outcomes[0].clone() else {
        panic!("one runner stores: {outcomes:?}");
    };
    assert_eq!(outcomes[1], MintOutcome::LostRace);
    let stored: Option<String> = sqlx::query_scalar("SELECT licence_id FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(stored.as_deref(), Some(winner.as_str()));
    let g = lock(&up.mock);
    assert_eq!(g.revoked.len(), 1, "the loser's licence is revoked");
    assert_ne!(g.revoked[0], winner);
}

#[tokio::test]
async fn member_elsewhere_409_before_licences_call() {
    let (e, up) = setup().await;
    let other = Uuid::new_v4();
    insert_tenant(&e.owner, other, "Other").await;
    insert_user(&e.owner, "kc-em", other, "tenant_user").await;
    lock(&up.mock)
        .users
        .push(("kc-em".into(), "em@else.test".into(), Map::new()));
    let (s, b) = create(&e, "Newco", "em@else.test").await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "user_exists");
    {
        let g = lock(&up.mock);
        assert!(g.calls.is_empty() && g.registered.is_empty() && g.issued.is_empty());
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants WHERE name = 'Newco'")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn duplicate_name_409() {
    let (e, _up) = setup().await;
    insert_tenant(&e.owner, Uuid::new_v4(), "Acme").await;
    let (s, b) = create(&e, " acme ", "x@acme.test").await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "tenant_name_taken");
}

#[tokio::test]
async fn missing_residency_422() {
    let (e, _up) = setup().await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/tenants",
            Some(&super_admin()),
            json!({"name": "Acme", "first_admin_email": "a@acme.test"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "invalid_residency");
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/tenants",
            Some(&super_admin()),
            json!({"name": "Acme", "first_admin_email": "nope", "residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(b["error"]["code"], "invalid_email");
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/tenants",
            Some(&super_admin()),
            json!({"name": " ", "first_admin_email": "a@acme.test", "residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(b["error"]["code"], "invalid_name");
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            "/api/tenants",
            Some(&person("u-x", &[])),
            json!({"name": "Acme", "first_admin_email": "a@acme.test", "residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn import_keeps_the_id_and_is_idempotent() {
    let (e, up) = setup().await;
    lock(&up.mock)
        .users
        .push(("kc-imp".into(), "imp@imp.test".into(), Map::new()));
    let id = Uuid::new_v4();
    let body = json!({"id": id, "name": "Importco", "first_admin": "kc-imp", "residency": "us"});
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/tenants/import",
            Some(&super_admin()),
            body.clone(),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["imported"], true);
    assert_eq!(b["tenant"]["id"], id.to_string());
    assert_eq!(b["tenant"]["residency"], "us");
    let admin: String = sqlx::query_scalar("SELECT sub FROM users WHERE tenant_id = $1")
        .bind(id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(admin, "kc-imp");
    assert_eq!(platform_count(&e, "tenant.import", "ok").await, 1);
    assert_eq!(tenant_count(&e, id, "tenant.import", "success").await, 1);
    let minted = lock(&up.mock).minted;
    let (s, b) = send(
        &e.app,
        json("POST", "/api/tenants/import", Some(&super_admin()), body),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["imported"], false);
    assert_eq!(lock(&up.mock).minted, minted, "nothing changes");
    assert_eq!(platform_count(&e, "tenant.import", "ok").await, 1);
}

#[tokio::test]
async fn import_bad_id_is_platform_audited() {
    let (e, _up) = setup().await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/tenants/import",
            Some(&super_admin()),
            json!({"id": "0195f48b", "name": "Shortco", "first_admin": "kc-1", "residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "invalid_tenant_id");
    let (label, details): (String, Value) = sqlx::query_as(
        "SELECT subject_label, details FROM platform_audit WHERE action = 'tenant.import' AND status = 'failed'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(label, agent_api::routes::tenants::REJECTED_IMPORT_LABEL);
    assert_eq!(details["code"], "invalid_tenant_id");
}

#[tokio::test]
async fn suspend_resume_audits_both_logs() {
    let (e, _up) = setup().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Susco").await;
    let uri = format!("/api/tenants/{t}");
    let patch = |status: &str| {
        json(
            "PATCH",
            &uri,
            Some(&super_admin()),
            json!({"status": status}),
        )
    };
    let (s, b) = send(&e.app, patch("suspended")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "suspended");
    assert!(b["suspended_at"].is_string());
    let (s, b) = send(&e.app, patch("suspended")).await;
    assert_eq!(s, StatusCode::OK, "same status is a no-op: {b}");
    let (s, b) = send(&e.app, patch("active")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["suspended_at"].is_null());
    for action in ["tenant.suspend", "tenant.resume"] {
        assert_eq!(tenant_count(&e, t, action, "success").await, 1, "{action}");
        assert_eq!(platform_count(&e, action, "ok").await, 1, "{action}");
    }
    let (s, b) = send(&e.app, patch("archived")).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(b["error"]["code"], "invalid_status");
}

#[tokio::test]
async fn patch_archived_409() {
    let (e, _up) = setup().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Gone").await;
    sqlx::query("UPDATE tenants SET status = 'archived', archived_at = now() WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = send(
        &e.app,
        json(
            "PATCH",
            &format!("/api/tenants/{t}"),
            Some(&super_admin()),
            json!({"status": "active"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "tenant_archived");
    let (s, b) = send(
        &e.app,
        json(
            "PATCH",
            &format!("/api/tenants/{}", Uuid::new_v4()),
            Some(&super_admin()),
            json!({"status": "active"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b["error"]["code"], "tenant_not_found");
}

#[tokio::test]
async fn offboard_revokes_archives_disables_and_stores_result() {
    let (e, up) = setup().await;
    let t = licensed_tenant(&e).await;
    sqlx::query(
        "INSERT INTO provision_outbox (tenant_id, target, op, dedupe_key, created_by) \
         VALUES ($1, 'strata_rag', 'connections_put', 'k1', 'test')",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = offboard(&e, t, "Offco").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["archived"], true);
    assert_eq!(b["users_disabled"], 2);
    assert_eq!(b["licence_revoked"], true, "{b}");
    assert_eq!(b["complete"], false);
    assert_eq!(b["outbox"][0]["status"], "cancelled");
    assert_eq!(b["data_retained"].as_array().unwrap().len(), 2);
    let statuses: Vec<String> =
        sqlx::query_scalar("SELECT status FROM member_entitlement WHERE tenant_id = $1")
            .bind(t)
            .fetch_all(&e.owner)
            .await
            .unwrap();
    assert_eq!(statuses, ["revoked", "revoked"]);
    let (status, pending): (String, Vec<String>) =
        sqlx::query_as("SELECT status, licence_revoke_pending FROM tenants WHERE id = $1")
            .bind(t)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(status, "archived");
    assert!(pending.is_empty());
    let mut revoked = lock(&up.mock).revoked.clone();
    revoked.sort();
    assert_eq!(revoked, ["lic-tenant", "lic-u-a", "lic-u-b"]);
    assert_eq!(tenant_count(&e, t, "tenant.offboard", "success").await, 1);
    assert_eq!(platform_count(&e, "tenant.offboard", "ok").await, 1);
    assert_eq!(tenant_count(&e, t, "license.revoke", "success").await, 3);
    let (s, b) = send(
        &e.app,
        get(&format!("/api/tenants/{t}/offboard"), Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["users_disabled"], 2);
}

#[tokio::test]
async fn offboard_revoke_failure_parks_id_and_completes() {
    let (e, up) = setup().await;
    let t = licensed_tenant(&e).await;
    lock(&up.mock).revoke_noop_ids = vec!["lic-u-b".into()];
    let (s, b) = offboard(&e, t, "Offco").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["licence_revoked"], false);
    let (status, pending): (String, Vec<String>) =
        sqlx::query_as("SELECT status, licence_revoke_pending FROM tenants WHERE id = $1")
            .bind(t)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(status, "archived");
    assert_eq!(pending, ["lic-u-b"]);
    assert_eq!(
        tenant_count(&e, t, "license.revoke_failed", "failure").await,
        1
    );
    let mut revoked = lock(&up.mock).revoked.clone();
    revoked.sort();
    assert_eq!(
        revoked,
        ["lic-tenant", "lic-u-a"],
        "the other ids are revoked"
    );
}

#[tokio::test]
async fn offboard_repeat_returns_stored_result_without_licences_call() {
    let (e, up) = setup().await;
    let t = licensed_tenant(&e).await;
    let (s, first) = offboard(&e, t, "Offco").await;
    assert_eq!(s, StatusCode::OK);
    let calls = lock(&up.mock).calls.len();
    let (s, again) = offboard(&e, t, "Offco").await;
    assert_eq!(s, StatusCode::OK, "{again}");
    assert_eq!(again["at"], first["at"]);
    assert_eq!(again["users_disabled"], first["users_disabled"]);
    assert_eq!(lock(&up.mock).calls.len(), calls, "no licences call");
    assert_eq!(platform_count(&e, "tenant.offboard", "ok").await, 1);
}

#[tokio::test]
async fn offboard_wrong_name_422() {
    let (e, up) = setup().await;
    let t = licensed_tenant(&e).await;
    let (s, b) = offboard(&e, t, "Offco Ltd").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "confirm_name_mismatch");
    let status: String = sqlx::query_scalar("SELECT status FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(status, "active");
    assert!(lock(&up.mock).calls.is_empty());
}

fn in_tenant(mut r: Request<Body>, t: Uuid) -> Request<Body> {
    r.headers_mut()
        .insert("x-active-tenant-id", t.to_string().parse().unwrap());
    r
}

#[tokio::test]
async fn archived_tenant_refused_on_tenant_routes() {
    let (e, _up) = setup().await;
    let t = licensed_tenant(&e).await;
    // a member's token before the offboard works
    let (s, _) = send(&e.app, get("/api/jobs", Some(&person("u-a", &[])))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(offboard(&e, t, "Offco").await.0, StatusCode::OK);
    let (s, b) = send(&e.app, get("/api/jobs", Some(&person("u-a", &[])))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(
        b["error"]["code"], "not_provisioned",
        "members are disabled by the offboard"
    );
    let (s, b) = send(&e.app, in_tenant(get("/api/jobs", Some(&super_admin())), t)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["code"], "tenant_archived");
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/tenants/{t}/jobs"),
            Some(&super_admin()),
            json!({"template": "job/customer_care_agent@v4", "key": "care", "label": "Care"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "tenant_archived");
}

// ---- review fixes (checkpoint 3) ----------------------------------------------------------------------------------

#[tokio::test]
async fn reconcile_drains_a_parked_member_licence_once() {
    let (e, up) = setup().await;
    let t = licensed_tenant(&e).await;
    lock(&up.mock).revoke_noop_ids = vec!["lic-u-b".into()];
    assert_eq!(offboard(&e, t, "Offco").await.0, StatusCode::OK);
    lock(&up.mock).revoke_noop_ids.clear();
    // the parked id and the revoking entitlement are the same licence: revoked once, then nothing is left
    let r = agent_api::signup::provisioning::reconcile(&e.state)
        .await
        .unwrap();
    assert_eq!(r.failed, 0, "{r:?}");
    let (pending, status): (Vec<String>, String) = sqlx::query_as(
        "SELECT t.licence_revoke_pending, e.status FROM tenants t \
           JOIN member_entitlement e ON e.tenant_id = t.id AND e.user_sub = 'u-b' WHERE t.id = $1",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(pending.is_empty(), "{pending:?}");
    assert_eq!(status, "revoked");
    let r = agent_api::signup::provisioning::reconcile(&e.state)
        .await
        .unwrap();
    assert_eq!(r, Default::default(), "nothing loops");
    assert_eq!(
        tenant_count(&e, t, "license.revoke_failed", "failure").await,
        1,
        "only the offboard's failure"
    );
    let (s, b) = send(
        &e.app,
        get(&format!("/api/tenants/{t}/offboard"), Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["licence_revoked"], true);
}

#[tokio::test]
async fn tenant_licence_is_not_stored_on_an_archived_tenant() {
    let (e, _up) = setup().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Gone").await;
    sqlx::query("UPDATE tenants SET status = 'archived', archived_at = now() WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    let issued = agent_api::identity::IssuedLicence {
        license_id: "lic-late".into(),
        subject_id: "s".into(),
        token_sealed: "v2:gone:AAAA".into(),
    };
    let mut tx = agent_api::tenancy::begin_scoped(&e.state.db, t)
        .await
        .unwrap();
    let stored = agent_api::identity::store_tenant_licence(&mut tx, t, &issued)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(stored, agent_api::identity::Stored::LostRace);
    let lic: Option<String> = sqlx::query_scalar("SELECT licence_id FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert!(lic.is_none());
}

// ---- post-review fixes (#2, #3, #5, #7) ---------------------------------------------------------------------------

async fn revoking_entitlement(e: &TestEnv, t: Uuid, sub: &str) {
    sqlx::query(
        "INSERT INTO member_entitlement (tenant_id, user_sub, license_id, status) \
         VALUES ($1, $2, 'lic-' || $2, 'revoking')",
    )
    .bind(t)
    .bind(sub)
    .execute(&e.owner)
    .await
    .unwrap();
}

async fn backoff_row(e: &TestEnv, t: Uuid, sub: &str) -> Option<(i32, String, bool)> {
    sqlx::query_as(
        "SELECT attempts, status, next_attempt_at > now() + interval '20 seconds' FROM licence_work_backoff \
          WHERE kind = 'revoke' AND tenant_id = $1 AND item = $2",
    )
    .bind(t)
    .bind(sub)
    .fetch_optional(&e.owner)
    .await
    .unwrap()
}

async fn make_due(e: &TestEnv, attempts: Option<i32>) {
    sqlx::query(
        "UPDATE licence_work_backoff SET next_attempt_at = now() - interval '1 second', \
                attempts = coalesce($1, attempts)",
    )
    .bind(attempts)
    .execute(&e.owner)
    .await
    .unwrap();
}

/// Review #2: a permanently failing item backs off (not re-picked every tick, so it cannot starve the batch) and is
/// audited only on a state change: the first failure and the move to `error`.
#[tokio::test]
async fn licence_reconcile_backs_off_failing_items_and_audits_state_changes_only() {
    use agent_api::signup::provisioning::{RECONCILE_BATCH, RECONCILE_MAX_ATTEMPTS, reconcile};
    let (e, up) = setup().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Backco").await;
    sqlx::query("UPDATE tenants SET licence_id = 'lic-tenant' WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    // its app licences too: only the stuck revokes below are failing work (the boot-ensured platform tenant is no
    // licences customer at all, 0226/0227)
    common::app_licences_held(&e.owner, t).await;
    // a full batch of revokes licences answers with the 200 no-op, forever
    let stuck: Vec<String> = (0..RECONCILE_BATCH)
        .map(|i| format!("u-stuck{i}"))
        .collect();
    for sub in &stuck {
        revoking_entitlement(&e, t, sub).await;
    }
    lock(&up.mock).revoke_noop_ids = stuck.iter().map(|s| format!("lic-{s}")).collect();
    let r = reconcile(&e.state).await.unwrap();
    assert_eq!(r.failed, stuck.len(), "{r:?}");
    assert_eq!(
        tenant_count(&e, t, "license.revoke_failed", "failure").await,
        RECONCILE_BATCH,
        "the first failure of each item is audited"
    );
    assert_eq!(
        backoff_row(&e, t, "u-stuck0").await,
        Some((1, "retrying".into(), true))
    );
    // a good item queued behind them is not starved: the stuck ones are not due
    revoking_entitlement(&e, t, "u-good").await;
    let r = reconcile(&e.state).await.unwrap();
    assert_eq!(r.revoked, 1, "{r:?}");
    assert_eq!(r.failed, 0, "nothing stuck is re-picked before it is due");
    assert!(lock(&up.mock).revoked.contains(&"lic-u-good".to_string()));
    // due again, same failure: attempts grow, no new audit row
    make_due(&e, None).await;
    let r = reconcile(&e.state).await.unwrap();
    assert_eq!(r.failed, stuck.len(), "{r:?}");
    assert_eq!(
        backoff_row(&e, t, "u-stuck0").await,
        Some((2, "retrying".into(), true))
    );
    assert_eq!(
        tenant_count(&e, t, "license.revoke_failed", "failure").await,
        RECONCILE_BATCH,
        "an unchanged failure is not audited again"
    );
    // the last allowed attempt: error, audited once
    make_due(&e, Some(RECONCILE_MAX_ATTEMPTS - 1)).await;
    reconcile(&e.state).await.unwrap();
    assert_eq!(
        backoff_row(&e, t, "u-stuck0").await,
        Some((RECONCILE_MAX_ATTEMPTS, "error".into(), true))
    );
    assert_eq!(
        tenant_count(&e, t, "license.revoke_failed", "failure").await,
        2 * RECONCILE_BATCH,
        "giving up is audited once per item"
    );
    let gave_up: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE tenant_id = $1 AND action = 'license.revoke_failed' \
            AND details ->> 'retry_status' = 'error'",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(gave_up, RECONCILE_BATCH);
    // the items in error are a metric (install the recorder before the gauges are set)
    let handle = agent_api::observability::metrics_handle();
    agent_api::provision::outbox::refresh_gauges(&e.state.db)
        .await
        .unwrap();
    let text = handle.render();
    let line = text
        .lines()
        .find(|l| {
            l.starts_with("agent_licence_work_failing{")
                && l.contains("kind=\"revoke\"")
                && l.contains("status=\"error\"")
        })
        .unwrap_or_else(|| panic!("no failing gauge in\n{text}"));
    let value: f64 = line.rsplit(' ').next().unwrap().parse().unwrap();
    assert_eq!(value, RECONCILE_BATCH as f64, "{line}");
    // recovery clears the backoff
    lock(&up.mock).revoke_noop_ids.clear();
    make_due(&e, None).await;
    let r = reconcile(&e.state).await.unwrap();
    assert_eq!(r.revoked, stuck.len(), "{r:?}");
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM licence_work_backoff")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

/// Review #5: two offboards racing on one tenant both answer the stored result (the loser like a repeat offboard),
/// never 409 `tenant_archived`.
#[tokio::test]
async fn concurrent_offboard_loser_returns_the_stored_result() {
    let (e, _up) = setup().await;
    let t = licensed_tenant(&e).await;
    // hold the tenant row so both requests are in flight before either can archive it
    let mut hold = e.owner.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR UPDATE")
        .bind(t)
        .execute(&mut *hold)
        .await
        .unwrap();
    let racers: Vec<_> = (0..2)
        .map(|_| {
            let app = e.app.clone();
            tokio::spawn(async move {
                send(
                    &app,
                    json(
                        "POST",
                        &format!("/api/tenants/{t}/offboard"),
                        Some(&super_admin()),
                        json!({"confirm_name": "Offco"}),
                    ),
                )
                .await
            })
        })
        .collect();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    hold.commit().await.unwrap();
    let mut answers = Vec::new();
    for r in racers {
        answers.push(r.await.unwrap());
    }
    for (s, b) in &answers {
        assert_eq!(*s, StatusCode::OK, "{b}");
    }
    assert_eq!(answers[0].1["at"], answers[1].1["at"], "one stored result");
    assert_eq!(platform_count(&e, "tenant.offboard", "ok").await, 1);
    assert_eq!(tenant_count(&e, t, "tenant.offboard", "success").await, 1);
}

/// Review #7: a failed import never writes the raw typed name into the platform audit (it may be an address).
#[tokio::test]
async fn failed_import_audit_carries_no_typed_name() {
    let (e, _up) = setup().await;
    let typed = "ceo@acme.test Secret Project";
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            "/api/tenants/import",
            Some(&super_admin()),
            json!({"id": "not-a-uuid", "name": typed, "first_admin": "kc-1", "residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT subject_label, details FROM platform_audit WHERE action = 'tenant.import'",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    for (label, details) in &rows {
        let row = format!("{label} {details}");
        assert!(!row.contains('@'), "an address in {row}");
        assert!(
            !row.contains("acme") && !row.contains("Secret"),
            "the typed name in {row}"
        );
    }
}

/// Review #3: the offboard parks the tenant licence INSIDE its transaction, so the id is already parked (committed)
/// when licences is called, and it stays parked when the revoke fails — the tick owns it even if the request had
/// died after COMMIT.
#[tokio::test]
async fn offboard_parks_tenant_licence_in_txn_when_revoke_fails() {
    let (e, up) = setup().await;
    let t = licensed_tenant(&e).await;
    {
        let mut g = lock(&up.mock);
        g.revoke_noop_ids = vec!["lic-tenant".into()];
        g.revoke_probe = Some(e.owner.clone());
    }
    let (s, b) = offboard(&e, t, "Offco").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["licence_revoked"], false);
    let seen = lock(&up.mock).parked_at_revoke.clone();
    assert!(
        seen.contains(&("lic-tenant".to_string(), true)),
        "the tenant licence was parked before licences was called: {seen:?}"
    );
    let pending: Vec<String> =
        sqlx::query_scalar("SELECT licence_revoke_pending FROM tenants WHERE id = $1")
            .bind(t)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(
        pending,
        ["lic-tenant"],
        "still parked after the failed revoke"
    );
    assert_eq!(
        tenant_count(&e, t, "license.revoke_failed", "failure").await,
        1
    );
}

/// Review #8, the tenant-level `licence_revoke_pending` path: a tenant licence id that is parked when licences answers
/// it again is stored AND unparked, so the tick never revokes the tenant's live licence.
#[tokio::test]
async fn a_stored_parked_tenant_licence_is_unparked_not_revoked() {
    let (e, up) = setup().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Reissued").await;
    sqlx::query("UPDATE tenants SET licence_revoke_pending = '{lic-again}' WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    let issued = agent_api::identity::IssuedLicence {
        license_id: "lic-again".into(),
        subject_id: "s".into(),
        token_sealed: "v2:gone:AAAA".into(),
    };
    let mut tx = agent_api::tenancy::begin_scoped(&e.state.db, t)
        .await
        .unwrap();
    let stored = agent_api::identity::store_tenant_licence(&mut tx, t, &issued)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(stored, agent_api::identity::Stored::Stored);
    agent_api::signup::provisioning::reconcile(&e.state)
        .await
        .unwrap();
    let (lic, pending): (Option<String>, Vec<String>) =
        sqlx::query_as("SELECT licence_id, licence_revoke_pending FROM tenants WHERE id = $1")
            .bind(t)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(lic.as_deref(), Some("lic-again"));
    assert!(pending.is_empty(), "{pending:?}");
    assert!(!lock(&up.mock).revoked.contains(&"lic-again".to_string()));
}

#[tokio::test]
async fn create_requires_both_first_admin_names_and_creates_nothing_without_them() {
    let (e, up) = setup().await;
    for body in [
        json!({"name": "Acme", "first_admin_email": "a@acme.test", "residency": "eu"}),
        json!({"name": "Acme", "first_admin_email": "a@acme.test", "residency": "eu",
               "first_admin_first_name": "Ada"}),
        json!({"name": "Acme", "first_admin_email": "a@acme.test", "residency": "eu",
               "first_admin_first_name": "Ada", "first_admin_last_name": "  "}),
    ] {
        let (s, b) = send(
            &e.app,
            json("POST", "/api/tenants", Some(&super_admin()), body),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert_eq!(b["error"]["code"], "first_admin_name_required");
    }
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/tenants",
            Some(&super_admin()),
            json!({"name": "Acme", "first_admin_email": "a@acme.test", "residency": "eu",
                   "first_admin_first_name": "Ada", "first_admin_last_name": "x".repeat(81)}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "invalid_person_name");
    let g = lock(&up.mock);
    assert!(g.created.is_empty(), "no identity was created");
    assert!(g.emails.is_empty(), "no email was sent");
}

#[tokio::test]
async fn create_fills_an_existing_first_admins_empty_names_never_overwrites() {
    let (e, up) = setup().await;
    lock(&up.mock)
        .users
        .push(("kc-old".into(), "old@acme.test".into(), Map::new()));
    let (s, b) = create(&e, "Acme", "old@acme.test").await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    {
        let g = lock(&up.mock);
        assert!(g.created.is_empty(), "the existing identity is reused");
        assert_eq!(
            g.names.get("kc-old"),
            Some(&(json!("Ada"), json!("Admin"))),
            "its empty names were filled"
        );
    }
    // a name someone set is kept
    lock(&up.mock)
        .users
        .push(("kc-named".into(), "named@beta.test".into(), Map::new()));
    lock(&up.mock)
        .names
        .insert("kc-named".into(), (json!("Bea"), Value::Null));
    let (s, b) = create(&e, "Beta", "named@beta.test").await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(
        lock(&up.mock).names.get("kc-named"),
        Some(&(json!("Bea"), json!("Admin"))),
        "firstName kept, the empty lastName filled"
    );
}

// ---- B1b T11: the first admin's access email outcome is audited, success and failure -----------------------------

async fn email_rows(e: &TestEnv, t: Uuid) -> Vec<(String, Value)> {
    sqlx::query_as(
        "SELECT status, details FROM audit_event WHERE tenant_id = $1 AND action = 'user.invite_email' ORDER BY id",
    )
    .bind(t)
    .fetch_all(&e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn first_admin_email_outcome_is_audited_both_ways() {
    let (e, up) = setup().await;
    let (s, b) = create(&e, "Mailco", "ann@mail.test").await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let rows = email_rows(&e, tenant_of(&b)).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "success");
    assert_eq!(rows[0].1["via"], "first_admin");
    assert_eq!(rows[0].1["access"], "set_password");
    lock(&up.mock).email_down = true;
    let (s, b) = create(&e, "Failco", "bob@fail.test").await;
    assert_eq!(s, StatusCode::CREATED, "the tenant stands: {b}");
    let rows = email_rows(&e, tenant_of(&b)).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "failure");
    assert_eq!(rows[0].1["via"], "first_admin");
    assert_eq!(rows[0].1["code"], "invite_email_failed");
    for (_, d) in &rows {
        assert!(!d.to_string().contains('@'), "{d}");
    }
}

// ---- B2 T0d: one tenants → users lock order -----------------------------------------------------------------------

/// Backends of this test's database waiting on a heavyweight lock (the owner pool sees every session).
async fn lock_waiters(e: &TestEnv) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity \
          WHERE datname = current_database() AND wait_event_type = 'Lock' AND pid <> pg_backend_pid()",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

/// Wait (bounded, never a fixed sleep) until at least `n` backends wait on a lock.
async fn until_waiting(e: &TestEnv, n: i64) {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while lock_waiters(e).await < n {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never saw {n} lock waiter(s)"));
}

/// B2 T0d (audit on main): a member disable locked user rows first (the admins `FOR UPDATE`, then the member) and
/// reached the `tenants` row only through its audit insert (`FOR KEY SHARE`, the foreign key), while an offboard locks
/// the tenants row `FOR UPDATE` and then updates every member: together they deadlocked (40P01, one of them a 500).
/// Both now take the tenants row first. Deterministic: a held lock on the member's entitlement row pauses the disable
/// after its first locks; the offboard starts only once the disable waits, and the hold is released only once the
/// offboard waits too (old code: on the admin's user row, holding the tenants row; now: on the tenants row).
#[tokio::test]
async fn disable_racing_an_offboard_takes_the_tenant_row_first_and_both_finish() {
    let (e, _up) = setup().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Offco").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;
    insert_user(&e.owner, "u-x", t, "tenant_user").await;
    sqlx::query("INSERT INTO member_entitlement (tenant_id, user_sub) VALUES ($1, 'u-x')")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    let mut hold = e.owner.begin().await.unwrap();
    sqlx::query(
        "SELECT 1 FROM member_entitlement WHERE tenant_id = $1 AND user_sub = 'u-x' FOR UPDATE",
    )
    .bind(t)
    .execute(&mut *hold)
    .await
    .unwrap();
    let app = e.app.clone();
    let dis = tokio::spawn(async move {
        send(
            &app,
            json(
                "POST",
                "/api/tenant/users/u-x/disable",
                Some(&person("u-admin", &[])),
                json!({}),
            ),
        )
        .await
    });
    // the disable holds its first lock(s) and waits on the entitlement row
    until_waiting(&e, 1).await;
    let app = e.app.clone();
    let off = tokio::spawn(async move {
        send(
            &app,
            json(
                "POST",
                &format!("/api/tenants/{t}/offboard"),
                Some(&super_admin()),
                json!({"confirm_name": "Offco"}),
            ),
        )
        .await
    });
    // the offboard waits on the disable
    until_waiting(&e, 2).await;
    hold.commit().await.unwrap();
    let within = std::time::Duration::from_secs(20);
    let (s, b) = tokio::time::timeout(within, dis)
        .await
        .expect("disable timed out")
        .unwrap();
    assert_eq!(s, StatusCode::OK, "the disable finishes (no 40P01): {b}");
    assert!(b["disabled_at"].is_string(), "{b}");
    let (s, b) = tokio::time::timeout(within, off)
        .await
        .expect("offboard timed out")
        .unwrap();
    assert_eq!(s, StatusCode::OK, "the offboard finishes (no 40P01): {b}");
    let status: String = sqlx::query_scalar("SELECT status FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(status, "archived");
    let ent: String = sqlx::query_scalar(
        "SELECT status FROM member_entitlement WHERE tenant_id = $1 AND user_sub = 'u-x'",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(ent, "revoked");
    assert_eq!(tenant_count(&e, t, "user.disable", "success").await, 1);
    assert_eq!(tenant_count(&e, t, "tenant.offboard", "success").await, 1);
}

/// B2 T0d minor 1: a rehome took the user row (`agent_user_rehome` `FOR UPDATE`) before either tenants row, the one
/// member path off the tenants → users order. It now locks both tenants rows (id order) before the user row.
/// Deterministic: an offboard-like `FOR UPDATE` held on the source tenants row pauses the rehome; while it waits, the
/// user row must be free (old code: held, so `NOWAIT` failed); once released, the rehome finishes.
#[tokio::test]
async fn rehome_takes_both_tenant_rows_before_the_user_row() {
    let (e, _up) = setup().await;
    let from = Uuid::new_v4();
    let to = Uuid::new_v4();
    insert_tenant(&e.owner, from, "Fromco").await;
    insert_tenant(&e.owner, to, "Toco").await;
    insert_user(&e.owner, "u-move", from, "tenant_user").await;
    sqlx::query("UPDATE users SET disabled_at = now() WHERE sub = 'u-move'")
        .execute(&e.owner)
        .await
        .unwrap();
    let mut hold = e.owner.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR UPDATE")
        .bind(from)
        .execute(&mut *hold)
        .await
        .unwrap();
    let app = e.app.clone();
    let mv = tokio::spawn(async move {
        send(
            &app,
            json(
                "POST",
                "/api/users/u-move/rehome",
                Some(&super_admin()),
                json!({"tenant_id": to}),
            ),
        )
        .await
    });
    // the rehome waits on the source tenants row
    until_waiting(&e, 1).await;
    let free = sqlx::query("SELECT 1 FROM users WHERE sub = 'u-move' FOR UPDATE NOWAIT")
        .execute(&e.owner)
        .await;
    assert!(
        free.is_ok(),
        "the rehome waits on a tenants row without holding the user row: {free:?}"
    );
    hold.commit().await.unwrap();
    let (s, b) = tokio::time::timeout(std::time::Duration::from_secs(20), mv)
        .await
        .expect("rehome timed out")
        .unwrap();
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["tenant_id"], to.to_string());
    let home: Uuid = sqlx::query_scalar("SELECT tenant_id FROM users WHERE sub = 'u-move'")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(home, to);
}

// ---- DR (owner ruling 2026-10-23): PATCH /api/tenants/{id} {"name"} — a recreated demo takes its name back ------

fn rename_req(t: Uuid, token: &str, name: &str) -> Request<Body> {
    json(
        "PATCH",
        &format!("/api/tenants/{t}"),
        Some(token),
        json!({"name": name}),
    )
}

async fn name_of(e: &TestEnv, t: Uuid) -> String {
    sqlx::query_scalar("SELECT name FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn rename_by_super_admin_renames_and_audits_both_logs_in_one_go() {
    let (e, _up) = setup().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Demo Retail (2026-10)").await;
    let (s, b) = send(&e.app, rename_req(t, &super_admin(), "  Demo Retail ")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["name"], "Demo Retail", "trimmed as on create");
    assert_eq!(name_of(&e, t).await, "Demo Retail");
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE tenant_id = $1 AND action = 'tenant.rename' AND status = 'success'",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(details["old_name"], "Demo Retail (2026-10)");
    assert_eq!(details["new_name"], "Demo Retail");
    let (label, pd): (String, Value) = sqlx::query_as(
        "SELECT subject_label, details FROM platform_audit WHERE action = 'tenant.rename' AND subject_tenant = $1",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(label, "Demo Retail");
    assert_eq!(pd["old_name"], "Demo Retail (2026-10)");
    // the same name again is a no-op without a second audit row
    let (s, b) = send(&e.app, rename_req(t, &super_admin(), "Demo Retail")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(tenant_count(&e, t, "tenant.rename", "success").await, 1);
    assert_eq!(platform_count(&e, "tenant.rename", "ok").await, 1);
}

#[tokio::test]
async fn rename_is_super_admin_only() {
    let (e, _up) = setup().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Keepco").await;
    insert_user(&e.owner, "u-adm", t, "tenant_admin").await;
    for token in [person("u-adm", &[]), person("u-x", &[])] {
        let (s, b) = send(&e.app, rename_req(t, &token, "Other")).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    }
    assert_eq!(name_of(&e, t).await, "Keepco");
    assert_eq!(tenant_count(&e, t, "tenant.rename", "success").await, 0);
}

#[tokio::test]
async fn rename_to_a_taken_name_is_409_case_insensitively_archived_included() {
    let (e, _up) = setup().await;
    let (old, new, other) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, old, "Demo Retail").await;
    insert_tenant(&e.owner, new, "Demo Retail (2026-10)").await;
    insert_tenant(&e.owner, other, "Northwind").await;
    sqlx::query("UPDATE tenants SET status = 'archived', archived_at = now() WHERE id = $1")
        .bind(old)
        .execute(&e.owner)
        .await
        .unwrap();
    for taken in ["Demo Retail", "  demo RETAIL ", "NORTHWIND"] {
        let (s, b) = send(&e.app, rename_req(new, &super_admin(), taken)).await;
        assert_eq!(s, StatusCode::CONFLICT, "{taken}: {b}");
        assert_eq!(b["error"]["code"], "tenant_name_taken");
    }
    assert_eq!(name_of(&e, new).await, "Demo Retail (2026-10)");
    // an archived tenant may be renamed (it keeps its name otherwise), which frees the name
    let (s, b) = send(
        &e.app,
        rename_req(old, &super_admin(), "Demo Retail (retired 2026-10)"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "archived");
    let (s, b) = send(&e.app, rename_req(new, &super_admin(), "Demo Retail")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // a case-only change of a tenant's own name is not a collision with itself
    let (s, b) = send(&e.app, rename_req(new, &super_admin(), "DEMO RETAIL")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(name_of(&e, new).await, "DEMO RETAIL");
}

#[tokio::test]
async fn rename_unknown_tenant_404_bad_name_422_empty_patch_422() {
    let (e, _up) = setup().await;
    let (s, b) = send(
        &e.app,
        rename_req(Uuid::new_v4(), &super_admin(), "Anything"),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(b["error"]["code"], "tenant_not_found");
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Namedco").await;
    let long = "x".repeat(121);
    for bad in ["   ", long.as_str(), "two\nlines"] {
        let (s, b) = send(&e.app, rename_req(t, &super_admin(), bad)).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert_eq!(b["error"]["code"], "invalid_name");
    }
    let (s, b) = send(
        &e.app,
        json(
            "PATCH",
            &format!("/api/tenants/{t}"),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "invalid_input");
    assert_eq!(name_of(&e, t).await, "Namedco");
}

// ---- SW-7 (owner 2026-10-27): PATCH /api/tenants/{id} {"support_auto_assign"} — the platform flag, super_admin only --

fn flag_req(t: Uuid, token: &str, on: bool) -> Request<Body> {
    json(
        "PATCH",
        &format!("/api/tenants/{t}"),
        Some(token),
        json!({"support_auto_assign": on}),
    )
}

async fn flag_of(e: &TestEnv, t: Uuid) -> bool {
    sqlx::query_scalar("SELECT support_auto_assign FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn support_auto_assign_flag_is_off_by_default_and_a_super_admin_switches_it_and_audits_both_logs()
 {
    let (e, _up) = setup().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Flagco").await;
    assert!(!flag_of(&e, t).await, "OFF by default");
    let (s, b) = send(&e.app, flag_req(t, &super_admin(), true)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["support_auto_assign"], true, "{b}");
    assert!(flag_of(&e, t).await);
    let (by, at): (Option<String>, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "SELECT support_auto_assign_set_by, support_auto_assign_set_at FROM tenants WHERE id = $1",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(by.is_some() && at.is_some(), "who and when are recorded");
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE tenant_id = $1 AND action = 'tenant.support_auto_assign' AND status = 'success'",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(details["enabled"], true);
    assert_eq!(
        platform_count(&e, "tenant.support_auto_assign", "ok").await,
        1
    );
    // the same value again is a no-op without a second audit row
    let (s, b) = send(&e.app, flag_req(t, &super_admin(), true)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        tenant_count(&e, t, "tenant.support_auto_assign", "success").await,
        1
    );
    // and it switches back off
    let (s, b) = send(&e.app, flag_req(t, &super_admin(), false)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["support_auto_assign"], false, "{b}");
    assert!(!flag_of(&e, t).await);
    // the single-tenant read carries it
    let (s, b) = send(
        &e.app,
        get(&format!("/api/tenants/{t}"), Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["support_auto_assign"], false, "{b}");
}

#[tokio::test]
async fn support_auto_assign_flag_is_refused_to_a_tenant_admin() {
    let (e, _up) = setup().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Flagco2").await;
    insert_user(&e.owner, "u-adm", t, "tenant_admin").await;
    for token in [person("u-adm", &[]), person("u-x", &[])] {
        let (s, b) = send(&e.app, flag_req(t, &token, true)).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    }
    assert!(!flag_of(&e, t).await);
    assert_eq!(
        tenant_count(&e, t, "tenant.support_auto_assign", "success").await,
        0
    );
}

#[tokio::test]
async fn support_auto_assign_flag_is_refused_on_an_archived_tenant() {
    let (e, _up) = setup().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Flagco3").await;
    sqlx::query("UPDATE tenants SET status = 'archived', archived_at = now() WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = send(&e.app, flag_req(t, &super_admin(), true)).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "tenant_archived", "{b}");
}
