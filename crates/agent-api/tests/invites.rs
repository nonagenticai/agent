//! B1b T2 (owner ruling F-01): member invites — list (expiry computed on read), resend with a cooldown, revoke (the
//! never-signed-in seat goes with it), the DB-only expiry sweep, and the super_admin variants (crate::routes::invites,
//! crate::members::invites). Real Postgres 18 (one container for this binary); Keycloak and licences are the
//! in-process fake (tests/common/fakes.rs).
use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::fakes::{self, Upstream, lock};
use common::{TestEnv, get, insert_tenant, insert_user, json, person, send, super_admin};
use serde_json::{Value, json};
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

/// Invite `email` through B1's route; the invite's id and the invitee's subject.
async fn invite(e: &TestEnv, email: &str) -> (Uuid, String) {
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/tenant/invites",
            Some(&admin()),
            json!({"email": email, "roles": ["operator"]}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let sub = b["sub"].as_str().unwrap().to_string();
    let id = sqlx::query_scalar("SELECT id FROM member_invites WHERE invitee_sub = $1")
        .bind(&sub)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    (id, sub)
}

async fn post(e: &TestEnv, uri: &str, token: &str) -> (StatusCode, Value) {
    send(&e.app, json("POST", uri, Some(token), json!({}))).await
}

async fn resend(e: &TestEnv, id: Uuid) -> (StatusCode, Value) {
    post(e, &format!("/api/tenant/invites/{id}/resend"), &admin()).await
}

async fn revoke(e: &TestEnv, id: Uuid) -> (StatusCode, Value) {
    post(e, &format!("/api/tenant/invites/{id}/revoke"), &admin()).await
}

async fn list(e: &TestEnv, query: &str) -> (StatusCode, Value) {
    send(
        &e.app,
        get(&format!("/api/tenant/invites{query}"), Some(&admin())),
    )
    .await
}

/// Move the last send `minutes` into the past (out of the cooldown when ≥ 10).
async fn sent_ago(e: &TestEnv, id: Uuid, minutes: i32) {
    sqlx::query(
        "UPDATE member_invites SET last_sent_at = now() - make_interval(mins => $2) WHERE id = $1",
    )
    .bind(id)
    .bind(minutes)
    .execute(&e.owner)
    .await
    .unwrap();
}

/// Make the invite past due: created 8 days ago, expiring yesterday, last sent 8 days ago; `state` stored.
async fn past_due(e: &TestEnv, id: Uuid, state: &str) {
    sqlx::query(
        "UPDATE member_invites SET created_at = now() - interval '8 days', expires_at = now() - interval '1 day', \
                last_sent_at = now() - interval '8 days', state = $2 WHERE id = $1",
    )
    .bind(id)
    .bind(state)
    .execute(&e.owner)
    .await
    .unwrap();
}

async fn stored(e: &TestEnv, id: Uuid) -> (String, i32, bool) {
    sqlx::query_as(
        "SELECT state, send_count, expires_at > now() + interval '6 days' FROM member_invites WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

async fn audit_rows(e: &TestEnv, action: &str) -> Vec<(String, String, Value)> {
    sqlx::query_as(
        "SELECT status, actor_id, details FROM audit_event WHERE action = $1 ORDER BY id",
    )
    .bind(action)
    .fetch_all(&e.owner)
    .await
    .unwrap()
}

fn emails(up: &Upstream) -> usize {
    lock(&up.mock).emails.len()
}

#[tokio::test]
async fn list_shows_expired_computed_on_read() {
    let (e, _up, t) = setup().await;
    let (fresh, _) = invite(&e, "fresh@example.test").await;
    let (late, _) = invite(&e, "late@example.test").await;
    past_due(&e, late, "pending").await;
    let (s, b) = list(&e, "").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let items = b["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let of = |id: Uuid| {
        items
            .iter()
            .find(|i| i["id"] == id.to_string())
            .unwrap()
            .clone()
    };
    assert_eq!(of(fresh)["state"], "pending");
    assert_eq!(of(late)["state"], "expired", "expired on read");
    assert_eq!(of(fresh)["roles"], json!(["operator"]));
    assert_eq!(of(fresh)["send_count"], 1);
    assert_eq!(of(fresh)["via"], "invite");
    assert_eq!(of(fresh)["invited_by"], "u-admin");
    assert_eq!(of(fresh)["invited_by_name"], "u-admin");
    assert_eq!(items[0]["id"], fresh.to_string(), "newest first");
    // the stored state is still pending: read-time expiry is the truth
    assert_eq!(stored(&e, late).await.0, "pending");
    let (_, b) = list(&e, "?state=pending").await;
    assert_eq!(b["items"].as_array().unwrap().len(), 1);
    assert_eq!(b["items"][0]["id"], fresh.to_string());
    let (_, b) = list(&e, "?state=expired").await;
    assert_eq!(b["items"][0]["id"], late.to_string());
    let (s, b) = list(&e, "?state=bogus").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "invalid_state");
    // paging: one per page, both once
    let (_, p1) = list(&e, "?limit=1").await;
    let next = p1["next"].as_str().unwrap().to_string();
    let (_, p2) = list(&e, &format!("?limit=1&before={next}")).await;
    assert_eq!(p2["items"][0]["id"], late.to_string());
    assert!(p2["next"].is_null());
    // any member reads it (an observer included)
    insert_user(&e.owner, "u-obs", t, "tenant_user").await;
    let (s, _) = send(
        &e.app,
        get("/api/tenant/invites", Some(&person("u-obs", &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn list_marks_signup_invites() {
    let (e, _up, t) = setup().await;
    let signup = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO member_invites (tenant_id, email, roles, invitee_sub, invited_by, signup_id) \
         VALUES ($1, 'founder@example.test', $2, 'kc-founder', $3, $4)",
    )
    .bind(t)
    .bind(common::role_keys("tenant_admin"))
    .bind(format!("signup:{signup}"))
    .bind(signup)
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = list(&e, "").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let i = &b["items"][0];
    assert_eq!(i["via"], "signup");
    assert_eq!(i["invited_by_name"], "Sign-up approval");
    assert_eq!(i["send_count"], 0);
    assert!(i["last_sent_at"].is_null());
}

#[tokio::test]
async fn resend_bumps_count_and_rearms_expiry_once() {
    let (e, up, _t) = setup().await;
    let (id, sub) = invite(&e, "ann@example.test").await;
    sent_ago(&e, id, 11).await;
    sqlx::query("UPDATE member_invites SET expires_at = now() + interval '1 hour' WHERE id = $1")
        .bind(id)
        .execute(&e.owner)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET invited_at = now() - interval '2 days' WHERE sub = $1")
        .bind(&sub)
        .execute(&e.owner)
        .await
        .unwrap();
    let before = emails(&up);
    let (s, b) = resend(&e, id).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["state"], "pending");
    assert_eq!(b["send_count"], 2);
    assert_eq!(
        stored(&e, id).await,
        ("pending".into(), 2, true),
        "re-armed to now + 7 days"
    );
    assert_eq!(emails(&up), before + 1, "one email");
    assert_eq!(lock(&up.mock).emails.last().unwrap().0, sub);
    let fresh: bool = sqlx::query_scalar(
        "SELECT invited_at > now() - interval '1 minute' FROM users WHERE sub = $1",
    )
    .bind(&sub)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(fresh, "the member's invite clock follows the resend");
    let rows = audit_rows(&e, "user.invite_resend").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "success");
    assert_eq!(rows[0].1, "u-admin");
    assert_eq!(rows[0].2["invite"], id.to_string());
    assert_eq!(rows[0].2["send_count"], 2);
}

/// `user.invite_email` rows written by a resend (the invite itself writes its own, `via == "invite"`).
async fn resend_email_rows(e: &TestEnv) -> Vec<(String, String, String, Value)> {
    sqlx::query_as(
        "SELECT status, actor_id, resource_id, details FROM audit_event \
          WHERE action = 'user.invite_email' AND details->>'via' = 'invite_resend' ORDER BY id",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn resend_success_audits_invite_email_success_and_cooldown_refusal_writes_none() {
    let (e, _up, _t) = setup().await;
    let (id, sub) = invite(&e, "ann@example.test").await;
    // inside the cooldown: refused, and no resend email row
    assert_eq!(resend(&e, id).await.0, StatusCode::TOO_MANY_REQUESTS);
    assert!(resend_email_rows(&e).await.is_empty());
    sent_ago(&e, id, 11).await;
    let (s, b) = resend(&e, id).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let rows = resend_email_rows(&e).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "success");
    assert_eq!(rows[0].1, "u-admin");
    assert_eq!(rows[0].2, sub);
    assert_eq!(rows[0].3["via"], "invite_resend");
    assert!(!rows[0].3.to_string().contains('@'));
    // refused again straight after: still exactly one row
    assert_eq!(resend(&e, id).await.0, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(resend_email_rows(&e).await.len(), 1);
}

#[tokio::test]
async fn resend_within_cooldown_429_with_retry_after() {
    let (e, up, _t) = setup().await;
    let (id, _) = invite(&e, "ann@example.test").await;
    let before = emails(&up);
    let (s, b) = resend(&e, id).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{b}");
    assert_eq!(b["error"]["code"], "invite_resend_cooldown");
    let left = b["error"]["retry_after_s"].as_u64().unwrap();
    assert!((1..=600).contains(&left), "{left}");
    assert!(
        left > 590,
        "sent just now: about the whole window left ({left})"
    );
    assert_eq!(emails(&up), before, "no email");
    assert_eq!(stored(&e, id).await.1, 1, "nothing counted");
    assert!(audit_rows(&e, "user.invite_resend").await.is_empty());
    // 9 minutes ago: still inside; 10 minutes: out
    sent_ago(&e, id, 9).await;
    let (s, b) = resend(&e, id).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    assert!(b["error"]["retry_after_s"].as_u64().unwrap() <= 60);
    sent_ago(&e, id, 10).await;
    assert_eq!(resend(&e, id).await.0, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn resend_expired_rearms_to_pending() {
    let (e, _up, _t) = setup().await;
    let (stored_expired, _) = invite(&e, "one@example.test").await;
    past_due(&e, stored_expired, "expired").await;
    let (read_expired, _) = invite(&e, "two@example.test").await;
    past_due(&e, read_expired, "pending").await;
    for id in [stored_expired, read_expired] {
        let (s, b) = resend(&e, id).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{b}");
        assert_eq!(b["state"], "pending");
        assert_eq!(stored(&e, id).await, ("pending".into(), 2, true));
    }
    let rows = audit_rows(&e, "user.invite_resend").await;
    assert!(rows.iter().all(|r| r.2["rearmed"] == true), "{rows:?}");
}

#[tokio::test]
async fn resend_superseded_409() {
    let (e, up, t) = setup().await;
    let (old, _) = invite(&e, "ann@example.test").await;
    past_due(&e, old, "expired").await;
    // a newer pending invite now holds the address
    sqlx::query(
        "INSERT INTO member_invites (tenant_id, email, roles, invited_by) VALUES ($1, 'ann@example.test', '{}', 'u-admin')",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    let before = emails(&up);
    let (s, b) = resend(&e, old).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "invite_superseded");
    assert_eq!(emails(&up), before);
    assert_eq!(stored(&e, old).await.0, "expired", "rolled back");
}

#[tokio::test]
async fn resend_accepted_or_revoked_409() {
    let (e, _up, _t) = setup().await;
    let (a, _) = invite(&e, "a@example.test").await;
    sqlx::query(
        "UPDATE member_invites SET state = 'accepted', accepted_at = now(), accepted_by = invitee_sub WHERE id = $1",
    )
    .bind(a)
    .execute(&e.owner)
    .await
    .unwrap();
    let (r, _) = invite(&e, "r@example.test").await;
    sqlx::query("UPDATE member_invites SET state = 'revoked', revoked_at = now() WHERE id = $1")
        .bind(r)
        .execute(&e.owner)
        .await
        .unwrap();
    for id in [a, r] {
        sent_ago(&e, id, 30).await;
        let (s, b) = resend(&e, id).await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert_eq!(b["error"]["code"], "invite_not_pending");
    }
    let (s, b) = resend(&e, Uuid::new_v4()).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(b["error"]["code"], "invite_not_found");
    let (s, _) = post(&e, "/api/tenant/invites/not-a-uuid/resend", &admin()).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn resend_keycloak_down_502_keeps_cooldown_unconsumed() {
    let (e, up, _t) = setup().await;
    let (id, sub) = invite(&e, "ann@example.test").await;
    sent_ago(&e, id, 11).await;
    let was: (
        Option<chrono::DateTime<chrono::Utc>>,
        chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as("SELECT last_sent_at, expires_at FROM member_invites WHERE id = $1")
        .bind(id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    lock(&up.mock).email_down = true;
    let (s, b) = resend(&e, id).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
    assert_eq!(b["error"]["code"], "keycloak_error");
    let now: (
        Option<chrono::DateTime<chrono::Utc>>,
        chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as("SELECT last_sent_at, expires_at FROM member_invites WHERE id = $1")
        .bind(id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(now, was, "last_sent_at and expires_at restored");
    assert_eq!(stored(&e, id).await.1, 1, "send_count restored");
    let failed = audit_rows(&e, "user.invite_email").await;
    let last = failed.last().unwrap();
    assert_eq!(last.0, "failure");
    assert_eq!(last.2["invite"], id.to_string());
    // the cooldown was not consumed: the next try goes through
    lock(&up.mock).email_down = false;
    let (s, b) = resend(&e, id).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(lock(&up.mock).emails.last().unwrap().0, sub);
}

#[tokio::test]
async fn revoke_disables_never_signed_in_member() {
    let (e, _up, t) = setup().await;
    let (id, sub) = invite(&e, "ann@example.test").await;
    let (s, b) = revoke(&e, id).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["state"], "revoked");
    assert!(b["revoked_at"].is_string());
    let disabled: bool = sqlx::query_scalar(
        "SELECT disabled_at IS NOT NULL FROM users WHERE sub = $1 AND tenant_id = $2",
    )
    .bind(&sub)
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(disabled, "the seat goes with the invite");
    let ent: String = sqlx::query_scalar(
        "SELECT status FROM member_entitlement WHERE tenant_id = $1 AND user_sub = $2",
    )
    .bind(t)
    .bind(&sub)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(ent, "revoked", "no licence was minted: revoked at once");
    let rows = audit_rows(&e, "user.invite_revoke").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2["member_disabled"], true);
    // a signed-in member is never disabled by a revoke of a still-pending invite row
    let (id2, sub2) = invite(&e, "bob@example.test").await;
    sqlx::query("UPDATE users SET invited_at = NULL WHERE sub = $1")
        .bind(&sub2)
        .execute(&e.owner)
        .await
        .unwrap();
    assert_eq!(revoke(&e, id2).await.0, StatusCode::OK);
    let disabled: bool =
        sqlx::query_scalar("SELECT disabled_at IS NOT NULL FROM users WHERE sub = $1")
            .bind(&sub2)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert!(!disabled);
}

#[tokio::test]
async fn revoke_accepted_409() {
    let (e, _up, _t) = setup().await;
    let (id, _) = invite(&e, "ann@example.test").await;
    sqlx::query(
        "UPDATE member_invites SET state = 'accepted', accepted_at = now(), accepted_by = invitee_sub WHERE id = $1",
    )
    .bind(id)
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = revoke(&e, id).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "invite_not_pending");
    let (s, b) = revoke(&e, Uuid::new_v4()).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("invite_not_found"))
    );
}

#[tokio::test]
async fn revoke_is_idempotent() {
    let (e, _up, _t) = setup().await;
    let (id, _) = invite(&e, "ann@example.test").await;
    let (s1, b1) = revoke(&e, id).await;
    let (s2, b2) = revoke(&e, id).await;
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK), "{b1} {b2}");
    assert_eq!(b1, b2, "the same answer");
    assert_eq!(
        audit_rows(&e, "user.invite_revoke").await.len(),
        1,
        "one row"
    );
    // an expired invite can be revoked too
    let (late, _) = invite(&e, "late@example.test").await;
    past_due(&e, late, "expired").await;
    let (s, b) = revoke(&e, late).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["state"], "revoked");
}

#[tokio::test]
async fn expire_due_flips_only_past_due_pending() {
    let (e, _up, t) = setup().await;
    let (due, due_sub) = invite(&e, "due@example.test").await;
    past_due(&e, due, "pending").await;
    let (future, _) = invite(&e, "future@example.test").await;
    let (gone, _) = invite(&e, "gone@example.test").await;
    past_due(&e, gone, "pending").await;
    sqlx::query("UPDATE member_invites SET state = 'revoked', revoked_at = now() WHERE id = $1")
        .bind(gone)
        .execute(&e.owner)
        .await
        .unwrap();
    // a second tenant's due invite is swept too (tenant by tenant)
    let t2 = Uuid::new_v4();
    insert_tenant(&e.owner, t2, "Beta").await;
    let other: Uuid = sqlx::query_scalar(
        "INSERT INTO member_invites (tenant_id, email, invited_by, created_at, expires_at) \
         VALUES ($1, 'x@example.test', 'u-x', now() - interval '9 days', now() - interval '2 days') RETURNING id",
    )
    .bind(t2)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let n = agent_api::members::invites::expire_due(&e.state.db)
        .await
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(stored(&e, due).await.0, "expired");
    assert_eq!(stored(&e, future).await.0, "pending");
    assert_eq!(stored(&e, gone).await.0, "revoked");
    assert_eq!(stored(&e, other).await.0, "expired");
    let rows: Vec<(uuid::Uuid, String, String, String)> = sqlx::query_as(
        "SELECT tenant_id, actor_id, actor_type, resource_id FROM audit_event \
          WHERE action = 'user.invite_expire' ORDER BY id",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows.iter().any(|r| r.0 == t && r.3 == due_sub));
    assert!(rows.iter().any(|r| r.0 == t2 && r.3 == other.to_string()));
    assert!(
        rows.iter()
            .all(|r| r.1 == agent_api::members::invites::SWEEP_ACTOR && r.2 == "system")
    );
    assert_eq!(
        agent_api::members::invites::expire_due(&e.state.db)
            .await
            .unwrap(),
        0,
        "a second sweep finds nothing"
    );
}

#[tokio::test]
async fn sweep_makes_no_http_call() {
    let (e, up, _t) = setup().await;
    let (due, _) = invite(&e, "due@example.test").await;
    past_due(&e, due, "pending").await;
    let snapshot = |up: &Upstream| {
        let g = lock(&up.mock);
        (
            g.emails.len(),
            g.created.len(),
            g.calls.len(),
            g.issued.len(),
            g.minted,
            g.registered.len(),
            g.revoked.len(),
            g.client_lookups,
            g.telegram.len(),
            g.verified.len(),
        )
    };
    let before = snapshot(&up);
    assert_eq!(
        agent_api::members::invites::expire_due(&e.state.db)
            .await
            .unwrap(),
        1
    );
    assert_eq!(snapshot(&up), before, "the sweep is database only");
    // and the source says so: no HTTP client, Keycloak, licences or router call in the module
    let src = include_str!("../src/members/invites.rs");
    let sweep = &src[src.find("pub async fn expire_due").unwrap()..];
    for call in ["st.identity", "reqwest", "send_access", "licence", ".http"] {
        assert!(!sweep.contains(call), "expire_due calls {call}");
    }
}

#[tokio::test]
async fn resend_and_revoke_need_tenant_admin() {
    let (e, _up, t) = setup().await;
    let (id, _) = invite(&e, "ann@example.test").await;
    insert_user(&e.owner, "u-obs", t, "tenant_user").await;
    for path in ["resend", "revoke"] {
        let (s, b) = post(
            &e,
            &format!("/api/tenant/invites/{id}/{path}"),
            &person("u-obs", &[]),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{path}: {b}");
        assert_eq!(b["error"]["missing_role"], "tenant_admin");
    }
    // an admin under a view-as without tenant_admin is refused the same way
    let (s, b) = send(
        &e.app,
        json(
            "PUT",
            "/api/me/view-as",
            Some(&admin()),
            json!({"roles": ["operator"]}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for path in ["resend", "revoke"] {
        let (s, b) = post(&e, &format!("/api/tenant/invites/{id}/{path}"), &admin()).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{path}: {b}");
        assert_eq!(b["error"]["missing_role"], "tenant_admin");
    }
    assert_eq!(stored(&e, id).await.0, "pending", "nothing changed");
}

#[tokio::test]
async fn platform_variants_write_tenant_and_platform_rows() {
    let (e, up, t) = setup().await;
    let (id, _) = invite(&e, "ann@example.test").await;
    let sa = super_admin();
    let (s, b) = send(&e.app, get(&format!("/api/tenants/{t}/invites"), Some(&sa))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"][0]["id"], id.to_string());
    // not a super_admin: refused
    let (s, b) = send(
        &e.app,
        get(&format!("/api/tenants/{t}/invites"), Some(&admin())),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("super_admin_required"))
    );
    sent_ago(&e, id, 11).await;
    let before = emails(&up);
    let (s, b) = post(&e, &format!("/api/tenants/{t}/invites/{id}/resend"), &sa).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(emails(&up), before + 1);
    let (s, b) = post(&e, &format!("/api/tenants/{t}/invites/{id}/revoke"), &sa).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["state"], "revoked");
    for action in ["user.invite_resend", "user.invite_revoke"] {
        let tenant_rows: Vec<(Uuid, String, bool)> = sqlx::query_as(
            "SELECT tenant_id, actor_id, platform_actor FROM audit_event WHERE action = $1",
        )
        .bind(action)
        .fetch_all(&e.owner)
        .await
        .unwrap();
        assert_eq!(tenant_rows, [(t, "u-super".to_string(), true)], "{action}");
        let platform_rows: Vec<(Option<Uuid>, String, String)> = sqlx::query_as(
            "SELECT subject_tenant, subject_label, actor_id FROM platform_audit WHERE action = $1",
        )
        .bind(action)
        .fetch_all(&e.owner)
        .await
        .unwrap();
        assert_eq!(
            platform_rows,
            [(Some(t), "Alpha".to_string(), "u-super".to_string())],
            "{action}"
        );
    }
    // unknown tenant 404; archived tenant: resend 409
    let ghost = Uuid::new_v4();
    let (s, b) = post(
        &e,
        &format!("/api/tenants/{ghost}/invites/{id}/resend"),
        &sa,
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("tenant_not_found"))
    );
    let (late, _) = invite(&e, "late@example.test").await;
    sqlx::query("UPDATE tenants SET status = 'archived', archived_at = now() WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    sent_ago(&e, late, 30).await;
    let (s, b) = post(&e, &format!("/api/tenants/{t}/invites/{late}/resend"), &sa).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("tenant_archived"))
    );
}

#[tokio::test]
async fn sa_in_tenant_uses_the_tenant_route() {
    let (e, _up, t) = setup().await;
    let (id, _) = invite(&e, "ann@example.test").await;
    let (s, b) = send(
        &e.app,
        in_tenant(
            json(
                "POST",
                &format!("/api/tenant/invites/{id}/revoke"),
                Some(&super_admin()),
                json!({}),
            ),
            t,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let platform: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform_audit WHERE action = 'user.invite_revoke'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(platform, 0, "the tenant route writes the tenant log only");
}

#[tokio::test]
async fn no_email_in_invite_audit_rows() {
    let (e, up, _t) = setup().await;
    let (a, _) = invite(&e, "ann@example.test").await;
    sent_ago(&e, a, 11).await;
    assert_eq!(resend(&e, a).await.0, StatusCode::ACCEPTED);
    sent_ago(&e, a, 11).await;
    lock(&up.mock).email_down = true;
    assert_eq!(resend(&e, a).await.0, StatusCode::BAD_GATEWAY);
    lock(&up.mock).email_down = false;
    assert_eq!(revoke(&e, a).await.0, StatusCode::OK);
    let (b, _) = invite(&e, "bob@example.test").await;
    past_due(&e, b, "pending").await;
    agent_api::members::invites::expire_due(&e.state.db)
        .await
        .unwrap();
    let rows: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT action, resource_id, details FROM audit_event WHERE action LIKE 'user.invite%' \
         UNION ALL SELECT action, subject_label, details FROM platform_audit",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    for action in [
        "user.invite_resend",
        "user.invite_email",
        "user.invite_revoke",
        "user.invite_expire",
    ] {
        assert!(
            rows.iter().any(|r| r.0 == action),
            "{action} written: {rows:?}"
        );
    }
    for (action, resource, details) in &rows {
        assert!(!resource.contains('@'), "{action}: {resource}");
        assert!(!details.to_string().contains('@'), "{action}: {details}");
    }
}

// ---- review fixes ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_late_first_sign_in_accepts_an_invite_the_sweep_expired() {
    let (e, _up, _t) = setup().await;
    let (id, sub) = invite(&e, "late@example.test").await;
    past_due(&e, id, "pending").await;
    agent_api::members::invites::expire_due(&e.state.db)
        .await
        .unwrap();
    assert_eq!(stored(&e, id).await.0, "expired");
    // the seated invitee signs in after day 7 (e.g. through forgot-password): a member, the invite accepted
    let (s, b) = send(&e.app, get("/api/me", Some(&person(&sub, &[])))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(stored(&e, id).await.0, "accepted");
}

#[tokio::test]
async fn resend_to_a_signed_in_or_disabled_member_409() {
    let (e, up, _t) = setup().await;
    let (signed_in, a) = invite(&e, "a@example.test").await;
    let (disabled, b) = invite(&e, "b@example.test").await;
    // the stored state did not follow (e.g. an older row): the member row decides
    sqlx::query("UPDATE users SET invited_at = NULL WHERE sub = $1")
        .bind(&a)
        .execute(&e.owner)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET disabled_at = now() WHERE sub = $1")
        .bind(&b)
        .execute(&e.owner)
        .await
        .unwrap();
    let before = emails(&up);
    for id in [signed_in, disabled] {
        sent_ago(&e, id, 30).await;
        let (s, body) = resend(&e, id).await;
        assert_eq!(s, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "invite_not_pending");
    }
    assert_eq!(emails(&up), before, "nobody emailed");
}

#[tokio::test]
async fn revoke_keeps_the_last_tenant_admin() {
    let up = fakes::upstream().await;
    let e = common::env_with(|v| fakes::identity_env(&up, v)).await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Solo").await;
    // the only admin is a never-signed-in invitee (e.g. a super_admin-invited first admin)
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, roles, display_name, email, origin, invited_at, updated_by) \
         VALUES ('kc-first', $1, $2, 'First', 'first@example.test', 'invite', now(), 'u-super')",
    )
    .bind(t)
    .bind(common::role_keys("tenant_admin"))
    .execute(&e.owner)
    .await
    .unwrap();
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO member_invites (tenant_id, email, roles, invitee_sub, invited_by) \
         VALUES ($1, 'first@example.test', $2, 'kc-first', 'u-super') RETURNING id",
    )
    .bind(t)
    .bind(common::role_keys("tenant_admin"))
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let (s, b) = post(
        &e,
        &format!("/api/tenants/{t}/invites/{id}/revoke"),
        &super_admin(),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "last_tenant_admin");
    assert_eq!(stored(&e, id).await.0, "pending", "nothing changed");
}
