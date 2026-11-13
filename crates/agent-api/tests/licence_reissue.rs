//! F-31 licence re-issue (B7 T3a, built early; crate::licensing::reissue): a super_admin replaces the tenant licence
//! and/or one member's licence in the tenant's residency region, keeping the licences subject and disabling nobody.
//! Real Postgres 18 (one container for this binary); Keycloak and licences are the in-process fake.
use crate::common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::fakes::{self, REGION_EU, REGION_US, Upstream, lock};
use common::{TestEnv, insert_tenant, insert_user, json, person, send, super_admin};
use serde_json::{Value, json};
use tokio::sync::Notify;
use uuid::Uuid;

async fn setup() -> (TestEnv, Upstream) {
    let up = fakes::upstream().await;
    let e = common::env_with(|v| fakes::identity_env(&up, v)).await;
    (e, up)
}

const OLD_TOKEN: &str = "v2:gone:old-sealed";

/// A tenant (residency eu) holding its own licence `lic-tenant` and a member `u-m` (enabled) holding `lic-u-m`.
async fn licensed(e: &TestEnv, name: &str) -> Uuid {
    licensed_in(e, name, "eu").await
}

/// [`licensed`] with a residency (immutable once the tenant exists, so set at insert).
async fn licensed_in(e: &TestEnv, name: &str, residency: &str) -> Uuid {
    let t = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO tenants (id, name, residency, updated_by, licence_id, licence_token_encrypted) \
         VALUES ($1, $2, $3, 'test', 'lic-tenant', $4)",
    )
    .bind(t)
    .bind(name)
    .bind(residency)
    .bind(OLD_TOKEN)
    .execute(&e.owner)
    .await
    .unwrap();
    member(e, t, "u-m").await;
    t
}

async fn member(e: &TestEnv, t: Uuid, sub: &str) {
    insert_user(&e.owner, sub, t, "tenant_admin").await;
    sqlx::query(
        "INSERT INTO member_entitlement (tenant_id, user_sub, license_id, license_subject_id, token_encrypted, status) \
         VALUES ($1, $2, 'lic-' || $2, 'subject-old', $3, 'active')",
    )
    .bind(t)
    .bind(sub)
    .bind(OLD_TOKEN)
    .execute(&e.owner)
    .await
    .unwrap();
}

async fn reissue(e: &TestEnv, t: Uuid, body: Value) -> (StatusCode, Value) {
    send(
        &e.app,
        json(
            "POST",
            &format!("/api/tenants/{t}/licence/reissue"),
            Some(&super_admin()),
            body,
        ),
    )
    .await
}

async fn tenant_licence(e: &TestEnv, t: Uuid) -> (Option<String>, Option<String>, Vec<String>) {
    sqlx::query_as(
        "SELECT licence_id, licence_token_encrypted, licence_revoke_pending FROM tenants WHERE id = $1",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

async fn entitlement(e: &TestEnv, t: Uuid, sub: &str) -> (String, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT status, license_id, token_encrypted FROM member_entitlement WHERE tenant_id = $1 AND user_sub = $2",
    )
    .bind(t)
    .bind(sub)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

async fn platform_rows(e: &TestEnv, t: Uuid, action: &str) -> Vec<(String, Value)> {
    sqlx::query_as(
        "SELECT status, details FROM platform_audit WHERE subject_tenant = $1 AND action = $2 ORDER BY id",
    )
    .bind(t)
    .bind(action)
    .fetch_all(&e.owner)
    .await
    .unwrap()
}

async fn tenant_rows(e: &TestEnv, t: Uuid, action: &str) -> Vec<(String, Value)> {
    sqlx::query_as(
        "SELECT status, details FROM audit_event WHERE tenant_id = $1 AND action = $2 ORDER BY id",
    )
    .bind(t)
    .bind(action)
    .fetch_all(&e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn reissue_tenant_swaps_id_and_token_and_revokes_the_old_one() {
    let (e, up) = setup().await;
    let t = licensed(&e, "Demo").await;
    let (s, b) = reissue(&e, t, json!({"tenant_licence": true})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["tenant_id"], t.to_string());
    assert_eq!(b["residency"], "eu");
    assert_eq!(b["members_unchanged"], true);
    let r = &b["results"][0];
    assert_eq!(r["holder"], "tenant");
    assert_eq!(r["old_licence_id"], "lic-tenant");
    assert_eq!(r["status"], "reissued");
    assert_eq!(r["old_revoke"], "revoked");
    let new = r["new_licence_id"].as_str().unwrap().to_string();
    assert_ne!(new, "lic-tenant");
    let (id, token, pending) = tenant_licence(&e, t).await;
    assert_eq!(id.as_deref(), Some(new.as_str()), "the new id is stored");
    let token = token.unwrap();
    assert!(
        token.starts_with("v2:v1:") && token != OLD_TOKEN,
        "the new token is stored sealed"
    );
    assert!(pending.is_empty(), "nothing parked: {pending:?}");
    {
        let g = lock(&up.mock);
        assert_eq!(g.revoked, ["lic-tenant"], "the old licence is revoked");
        // the same licences subject: the tenant's synthetic holder
        assert_eq!(g.issued.last().unwrap().1, format!("agent-tenant:{t}:a"));
        assert_eq!(g.issued.len(), 1, "exactly one new licence");
        // the old one is revoked before the new one is minted (licences keeps one active licence per subject)
        let rev = g
            .calls
            .iter()
            .position(|c| c == "revoke:lic-tenant")
            .unwrap();
        let mint = g
            .calls
            .iter()
            .position(|c| *c == format!("mint:{new}"))
            .unwrap();
        assert!(rev < mint, "{:?}", g.calls);
    }
    // the member is untouched
    let (status, lic, _) = entitlement(&e, t, "u-m").await;
    assert_eq!(
        (status.as_str(), lic.as_deref()),
        ("active", Some("lic-u-m"))
    );
    // audit: request + success (platform), revoke tombstone + issue (tenant)
    assert_eq!(
        platform_rows(&e, t, "license.reissue_request").await.len(),
        1
    );
    let done = platform_rows(&e, t, "license.reissue").await;
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].0, "ok");
    assert_eq!(done[0].1["new_licence_id"], new.as_str());
    let revoke = tenant_rows(&e, t, "license.revoke").await;
    assert_eq!(revoke.len(), 1);
    assert_eq!(revoke[0].1["license_id"], "lic-tenant");
    assert_eq!(revoke[0].1["via"], "reissue");
    assert_eq!(tenant_rows(&e, t, "license.issue").await.len(), 1);
}

#[tokio::test]
async fn reissue_member_keeps_the_subject_and_the_member_enabled() {
    let (e, up) = setup().await;
    let t = licensed(&e, "Memberco").await;
    let (s, b) = reissue(&e, t, json!({"member_sub": "u-m"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["members_unchanged"], false);
    let r = &b["results"][0];
    assert_eq!(
        (r["holder"].as_str(), r["sub"].as_str()),
        (Some("member"), Some("u-m"))
    );
    assert_eq!(r["status"], "reissued");
    let new = r["new_licence_id"].as_str().unwrap();
    let (status, lic, token) = entitlement(&e, t, "u-m").await;
    assert_eq!(status, "active");
    assert_eq!(lic.as_deref(), Some(new));
    assert!(token.unwrap() != OLD_TOKEN);
    let disabled: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT disabled_at FROM users WHERE sub = 'u-m'")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert!(disabled.is_none(), "nobody is disabled");
    {
        let g = lock(&up.mock);
        assert_eq!(g.revoked, ["lic-u-m"]);
        assert_eq!(
            g.issued.last().unwrap().1,
            "u-m",
            "the member's own subject"
        );
    }
    // the tenant licence is untouched
    assert_eq!(tenant_licence(&e, t).await.0.as_deref(), Some("lic-tenant"));
}

#[tokio::test]
async fn reissue_both_in_one_call_and_the_region_is_the_residency() {
    let (e, up) = setup().await;
    let t = licensed_in(&e, "Usco", "us").await;
    let (s, b) = reissue(&e, t, json!({"tenant_licence": true, "member_sub": "u-m"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["residency"], "us");
    assert_eq!(b["results"][0]["holder"], "tenant");
    assert_eq!(b["results"][1]["holder"], "member");
    let g = lock(&up.mock);
    assert_eq!(
        g.issued_regions,
        [Some(REGION_US.to_string()), Some(REGION_US.to_string())],
        "both new licences carry the residency's licences region"
    );
    assert_ne!(REGION_US, REGION_EU);
}

/// The holder's stored licence id now (`tenants.licence_id`, or the member's ACTIVE entitlement).
async fn stored(e: &TestEnv, t: Uuid, member: bool) -> Option<String> {
    if member {
        let (status, lic, _) = entitlement(e, t, "u-m").await;
        lic.filter(|_| status == "active")
    } else {
        tenant_licence(e, t).await.0
    }
}

/// Licences down for the revoke: the old id is parked and the holder cleared (`pending`). While licences still holds
/// the old licence ACTIVE (it answers it again for the same subject, `active_by_holder`), a mint before the drain must
/// not adopt it (review of #14: it would silently undo the re-issue); after the drain the holder gets a NEW licence.
async fn parked_then_tick(member: bool) {
    let (e, up) = setup().await;
    let t = licensed(&e, "Parkco").await;
    let (holder, old, body) = if member {
        ("u-m".to_string(), "lic-u-m", json!({"member_sub": "u-m"}))
    } else {
        (
            format!("agent-tenant:{t}:a"),
            "lic-tenant",
            json!({"tenant_licence": true}),
        )
    };
    lock(&up.mock).active_by_holder.insert(holder, old.into());
    // licences answers the region lookup (cached), then is down for the revoke
    assert_eq!(
        e.state
            .identity
            .licences
            .region_id("eu")
            .await
            .unwrap()
            .as_deref(),
        Some(REGION_EU)
    );
    lock(&up.mock).licences_down = true;
    let (s, b) = reissue(&e, t, body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let r = &b["results"][0];
    assert_eq!(
        (
            r["status"].as_str(),
            r["old_revoke"].as_str(),
            r["code"].as_str()
        ),
        (Some("pending"), Some("pending"), Some("revoke_pending"))
    );
    assert!(r["new_licence_id"].is_null());
    // no token licences may have killed is held, and the old id is recorded for the tick
    assert!(stored(&e, t, member).await.is_none());
    assert_eq!(tenant_licence(&e, t).await.2, [old]);
    let failed = tenant_rows(&e, t, "license.revoke_failed").await;
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].1["code"], "licences_unavailable");
    // licences is back, the old licence still ACTIVE there: a mint before the drain answers the old id and must not
    // store (nor unpark) it
    lock(&up.mock).licences_down = false;
    let actor = agent_api::audit::Actor::system("service:test");
    if member {
        agent_api::members::ensure_member_licence(&e.state, t, "u-m", &actor)
            .await
            .unwrap();
    } else {
        agent_api::tenants::lifecycle::ensure_tenant_licence(&e.state, t, &actor)
            .await
            .unwrap();
    }
    assert!(
        stored(&e, t, member).await.is_none(),
        "the old licence was not adopted"
    );
    assert_eq!(
        tenant_licence(&e, t).await.2,
        [old],
        "still parked for the drain"
    );
    // the tick drains (revokes) the old id and mints a new licence, whatever order its work list takes
    for _ in 0..3 {
        agent_api::signup::provisioning::reconcile(&e.state)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE licence_work_backoff SET next_attempt_at = now() - interval '1 second'",
        )
        .execute(&e.owner)
        .await
        .unwrap();
    }
    assert!(tenant_licence(&e, t).await.2.is_empty(), "drained");
    let now = stored(&e, t, member)
        .await
        .expect("the tick stored a new licence");
    assert_ne!(now, old);
    let g = lock(&up.mock);
    assert!(g.revoked.contains(&old.to_string()), "{:?}", g.revoked);
    assert_eq!(g.issued_regions.last().unwrap().as_deref(), Some(REGION_EU));
}

#[tokio::test]
async fn licences_down_on_revoke_parks_the_tenant_licence_and_the_tick_finishes() {
    parked_then_tick(false).await;
}

#[tokio::test]
async fn licences_down_on_revoke_parks_the_member_licence_and_the_tick_finishes() {
    parked_then_tick(true).await;
}

/// The revoke-before-mint window: the old licence is revoked (tombstoned), then the mint fails (licences 503 on the
/// token mint). The holder is left WITHOUT a licence (`pending`, never the dead token), the answer says so, and the
/// reconciler deterministically mints the replacement (its `tenant` / `mint` work lists every active tenant without a
/// licence and every pending entitlement).
async fn revoked_then_mint_fails_then_tick(member: bool) {
    let (e, up) = setup().await;
    let t = licensed(&e, "Mintdown").await;
    let (old, body) = if member {
        ("lic-u-m", json!({"member_sub": "u-m"}))
    } else {
        ("lic-tenant", json!({"tenant_licence": true}))
    };
    lock(&up.mock).mint_down = true;
    let (s, b) = reissue(&e, t, body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let r = &b["results"][0];
    assert_eq!(
        (
            r["status"].as_str(),
            r["old_revoke"].as_str(),
            r["code"].as_str()
        ),
        (
            Some("pending"),
            Some("revoked"),
            Some("licences_unavailable")
        )
    );
    assert!(r["new_licence_id"].is_null());
    assert!(
        stored(&e, t, member).await.is_none(),
        "no dead token is held"
    );
    assert!(
        tenant_licence(&e, t).await.2.is_empty(),
        "the old id is revoked, not parked"
    );
    if member {
        let (status, lic, token) = entitlement(&e, t, "u-m").await;
        assert_eq!((status.as_str(), lic, token), ("pending", None, None));
    } else {
        let (id, token, _) = tenant_licence(&e, t).await;
        assert!(id.is_none() && token.is_none());
    }
    assert_eq!(lock(&up.mock).revoked, [old]);
    // licences mints again: the tick stores the replacement
    lock(&up.mock).mint_down = false;
    agent_api::signup::provisioning::reconcile(&e.state)
        .await
        .unwrap();
    let now = stored(&e, t, member)
        .await
        .expect("the tick minted the replacement");
    assert_ne!(now, old);
    let g = lock(&up.mock);
    assert_eq!(g.issued_regions.last().unwrap().as_deref(), Some(REGION_EU));
}

#[tokio::test]
async fn tenant_revoked_then_mint_fails_is_pending_and_the_tick_mints_it() {
    revoked_then_mint_fails_then_tick(false).await;
}

#[tokio::test]
async fn member_revoked_then_mint_fails_is_pending_and_the_tick_mints_it() {
    revoked_then_mint_fails_then_tick(true).await;
}

#[tokio::test]
async fn a_refused_revoke_changes_nothing() {
    let (e, up) = setup().await;
    let t = licensed(&e, "Refuseco").await;
    lock(&up.mock).revoke_noop_ids = vec!["lic-tenant".into()];
    let (s, b) = reissue(&e, t, json!({"tenant_licence": true})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "licence_revoke_refused");
    let (id, token, pending) = tenant_licence(&e, t).await;
    assert_eq!(
        (id.as_deref(), token.as_deref()),
        (Some("lic-tenant"), Some(OLD_TOKEN))
    );
    assert!(pending.is_empty());
    assert!(lock(&up.mock).issued.is_empty(), "nothing minted");
    let done = platform_rows(&e, t, "license.reissue").await;
    assert_eq!(done.len(), 1);
    assert_eq!(
        (done[0].0.as_str(), done[0].1["code"].as_str()),
        ("failed", Some("licence_revoke_refused"))
    );
}

#[tokio::test]
async fn licences_down_before_any_change_is_502_and_nothing_changes() {
    let (e, up) = setup().await;
    let t = licensed(&e, "Downco").await;
    lock(&up.mock).licences_down = true;
    let (s, b) = reissue(&e, t, json!({"tenant_licence": true})).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
    assert_eq!(b["error"]["code"], "licences_unavailable");
    let (id, token, _) = tenant_licence(&e, t).await;
    assert_eq!(
        (id.as_deref(), token.as_deref()),
        (Some("lic-tenant"), Some(OLD_TOKEN))
    );
    assert!(lock(&up.mock).revoked.is_empty());
}

#[tokio::test]
async fn no_region_for_the_residency_is_409_and_nothing_changes() {
    let (e, up) = setup().await;
    let t = licensed(&e, "Noregion").await;
    lock(&up.mock).no_regions = true;
    let (s, b) = reissue(&e, t, json!({"tenant_licence": true})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "region_unmapped");
    assert!(lock(&up.mock).revoked.is_empty());
    assert_eq!(tenant_licence(&e, t).await.0.as_deref(), Some("lic-tenant"));
}

#[tokio::test]
async fn reissue_needs_super_admin() {
    let (e, up) = setup().await;
    let t = licensed(&e, "Guardco").await;
    // a tenant admin of that very tenant (every role) is still not a super_admin
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/tenants/{t}/licence/reissue"),
            Some(&person("u-m", &[])),
            json!({"tenant_licence": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["code"], "super_admin_required");
    let g = lock(&up.mock);
    assert!(g.revoked.is_empty() && g.issued.is_empty());
}

#[tokio::test]
async fn cross_tenant_member_and_unknown_tenant_are_404() {
    let (e, up) = setup().await;
    let a = licensed(&e, "Alpha").await;
    let other = Uuid::new_v4();
    insert_tenant(&e.owner, other, "Beta").await;
    member(&e, other, "u-beta").await;
    // a member of Beta addressed under Alpha
    let (s, b) = reissue(&e, a, json!({"member_sub": "u-beta"})).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(b["error"]["code"], "user_not_found");
    let (s, b) = reissue(&e, Uuid::new_v4(), json!({"tenant_licence": true})).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(b["error"]["code"], "tenant_not_found");
    assert!(lock(&up.mock).revoked.is_empty());
    assert_eq!(
        entitlement(&e, other, "u-beta").await.1.as_deref(),
        Some("lic-u-beta")
    );
}

#[tokio::test]
async fn archived_suspended_and_unlicensed_are_409_and_empty_body_is_422() {
    let (e, up) = setup().await;
    let t = licensed(&e, "Stateco").await;
    let (s, b) = reissue(&e, t, json!({})).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid_input"))
    );
    let (s, b) = reissue(&e, t, json!({"tenant_licence": true, "extra": 1})).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_body")),
        "unknown fields are refused: {b}"
    );
    sqlx::query("UPDATE member_entitlement SET status = 'pending', license_id = NULL, token_encrypted = NULL")
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = reissue(&e, t, json!({"member_sub": "u-m"})).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("no_licence"))
    );
    sqlx::query("UPDATE tenants SET status = 'suspended', suspended_at = now() WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = reissue(&e, t, json!({"tenant_licence": true})).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("tenant_suspended"))
    );
    sqlx::query("UPDATE tenants SET status = 'archived', archived_at = now() WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = reissue(&e, t, json!({"tenant_licence": true})).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("tenant_archived"))
    );
    assert!(lock(&up.mock).revoked.is_empty());
    assert_eq!(
        platform_rows(&e, t, "license.reissue_request").await.len(),
        0,
        "refused before any change"
    );
}

#[tokio::test]
async fn two_reissues_at_once_make_exactly_one_new_licence() {
    let (e, up) = setup().await;
    let t = licensed(&e, "Raceco").await;
    // the winner holds the licence lock across a slow (but answered) revoke
    lock(&up.mock).revoke_delay_ms = 600;
    let (r1, r2) = tokio::join!(
        reissue(&e, t, json!({"tenant_licence": true})),
        reissue(&e, t, json!({"tenant_licence": true})),
    );
    let mut all = [r1, r2];
    all.sort_by_key(|(s, _)| *s != StatusCode::OK);
    assert_eq!(all[0].0, StatusCode::OK, "{:?}", all);
    assert_eq!(all[1].0, StatusCode::CONFLICT, "{:?}", all);
    let code = all[1].1["error"]["code"].as_str().unwrap();
    assert!(
        code == "concurrent_reissue" || code == "no_licence",
        "the loser is refused, it mints nothing: {code}"
    );
    let new = all[0].1["results"][0]["new_licence_id"].as_str().unwrap();
    assert_eq!(tenant_licence(&e, t).await.0.as_deref(), Some(new));
    let g = lock(&up.mock);
    assert_eq!(g.issued.len(), 1, "exactly one new licence: {:?}", g.issued);
    assert_eq!(g.revoked, ["lic-tenant"]);
}

#[tokio::test]
async fn reissue_audit_has_no_token() {
    let (e, _up) = setup().await;
    let t = licensed(&e, "Quietco").await;
    let (s, b) = reissue(&e, t, json!({"tenant_licence": true, "member_sub": "u-m"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(
        !b.to_string().contains("tok-") && !b.to_string().contains("old-sealed"),
        "{b}"
    );
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT details FROM platform_audit WHERE subject_tenant = $1 \
         UNION ALL SELECT details FROM audit_event WHERE tenant_id = $1",
    )
    .bind(t)
    .fetch_all(&e.owner)
    .await
    .unwrap();
    for d in rows {
        let s = d.to_string();
        assert!(!s.contains("tok-") && !s.contains("old-sealed"), "{s}");
    }
}

// ---- races with a disable / suspend, and already-revoked ids (forward-fix of #14) ---------------------------------

/// [`setup`] with the revoke bound at its maximum, so a revoke paused by [`gate`] is still answered, not timed out.
async fn setup_slow_revoke() -> (TestEnv, Upstream) {
    let up = fakes::upstream().await;
    let e = common::env_with(|v| {
        fakes::identity_env(&up, v);
        v.insert("LICENCES_REVOKE_TIMEOUT_MS".into(), "8000".into());
    })
    .await;
    (e, up)
}

/// Pause licences' revoke of `id` once: (reached, release).
fn gate(up: &Upstream, id: &str) -> (Arc<Notify>, Arc<Notify>) {
    let reached = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    lock(&up.mock).revoke_gate = Some((id.to_string(), reached.clone(), release.clone()));
    (reached, release)
}

async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), f)
        .await
        .expect("timed out")
}

/// The request in its own task (it holds the licence lock while paused in the fake).
fn spawn_send(
    e: &TestEnv,
    method: &str,
    uri: &str,
    token: &str,
    body: Value,
) -> tokio::task::JoinHandle<(StatusCode, Value)> {
    let app = e.app.clone();
    let req = json(method, uri, Some(token), body);
    tokio::spawn(async move { send(&app, req).await })
}

/// Wait until `sub`'s entitlement is COMMITTED as `status` (read on the owner pool).
async fn committed(e: &TestEnv, t: Uuid, sub: &str, status: &str) {
    within(async {
        while entitlement(e, t, sub).await.0 != status {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
}

fn count_calls(up: &Upstream, call: &str) -> usize {
    lock(&up.mock).calls.iter().filter(|c| *c == call).count()
}

/// The audited race: the re-issue holds the licence lock and its pre-check passed; a tenant-admin disable (no licence
/// lock) commits `revoking` on the same licence while licences revokes it; the disable's own revoke waits on the lock.
/// Before the fix the re-issue's `clear` matched nothing, it answered `pending`, and the disable's revoke got the
/// already-revoked id — the entitlement stayed `revoking` until backoff marked it `error`.
#[tokio::test]
async fn a_reissue_racing_a_disable_leaves_the_member_disabled_and_revoked_once() {
    let (e, up) = setup_slow_revoke().await;
    let t = licensed(&e, "Raceable").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;
    let (reached, release) = gate(&up, "lic-u-m");
    let ri = spawn_send(
        &e,
        "POST",
        &format!("/api/tenants/{t}/licence/reissue"),
        &super_admin(),
        json!({"member_sub": "u-m"}),
    );
    within(reached.notified()).await;
    // the disable runs while the re-issue holds the lock inside the licences call
    let dis = spawn_send(
        &e,
        "POST",
        "/api/tenant/users/u-m/disable",
        &person("u-admin", &[]),
        json!({}),
    );
    committed(&e, t, "u-m", "revoking").await;
    release.notify_one();
    let (s, b) = within(ri).await.unwrap();
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "concurrent_change");
    let (s, b) = within(dis).await.unwrap();
    assert_eq!(s, StatusCode::OK, "the disable stands: {b}");
    assert!(b["disabled_at"].is_string(), "{b}");
    let (status, lic, _) = entitlement(&e, t, "u-m").await;
    assert_eq!(
        (status.as_str(), lic.as_deref()),
        ("revoked", Some("lic-u-m")),
        "terminal, not stuck revoking"
    );
    assert!(tenant_licence(&e, t).await.2.is_empty(), "nothing parked");
    {
        let g = lock(&up.mock);
        assert_eq!(g.revoked, ["lic-u-m"]);
        assert!(g.issued.is_empty(), "no new licence: {:?}", g.issued);
    }
    assert_eq!(count_calls(&up, "revoke:lic-u-m"), 1);
    assert_eq!(
        count_calls(&up, "rerevoke:lic-u-m"),
        0,
        "the re-issue finished the revoking row, so the disable's revoke had nothing to call"
    );
    let done = platform_rows(&e, t, "license.reissue").await;
    assert_eq!(
        (done.len(), done[0].0.as_str(), done[0].1["code"].as_str()),
        (1, "failed", Some("concurrent_change"))
    );
    assert!(tenant_rows(&e, t, "license.revoke_failed").await.is_empty());
    // the tick finds nothing to do and mints nothing for the disabled member
    agent_api::signup::provisioning::reconcile(&e.state)
        .await
        .unwrap();
    assert_eq!(entitlement(&e, t, "u-m").await.0, "revoked");
    assert!(lock(&up.mock).issued.is_empty());
}

/// The same race while licences is down for the re-issue's revoke: the re-issue keeps nothing (no park, no
/// `revoke_failed`), the disable owns the id, and the tick revokes it once licences is back.
#[tokio::test]
async fn a_reissue_racing_a_disable_with_licences_down_hands_the_id_to_the_disable() {
    let (e, up) = setup_slow_revoke().await;
    let t = licensed(&e, "Raceable2").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;
    let (reached, release) = gate(&up, "lic-u-m");
    let ri = spawn_send(
        &e,
        "POST",
        &format!("/api/tenants/{t}/licence/reissue"),
        &super_admin(),
        json!({"member_sub": "u-m"}),
    );
    within(reached.notified()).await;
    let dis = spawn_send(
        &e,
        "POST",
        "/api/tenant/users/u-m/disable",
        &person("u-admin", &[]),
        json!({}),
    );
    committed(&e, t, "u-m", "revoking").await;
    lock(&up.mock).licences_down = true;
    release.notify_one();
    let (s, b) = within(ri).await.unwrap();
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "concurrent_change");
    let (s, b) = within(dis).await.unwrap();
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(entitlement(&e, t, "u-m").await.0, "revoking");
    assert!(
        tenant_licence(&e, t).await.2.is_empty(),
        "the re-issue parked nothing"
    );
    assert!(
        tenant_rows(&e, t, "license.revoke_failed")
            .await
            .iter()
            .all(|(_, d)| d["via"] != "reissue"),
        "no re-issue revoke_failed row (it would block a store of the id)"
    );
    lock(&up.mock).licences_down = false;
    sqlx::query("UPDATE licence_work_backoff SET next_attempt_at = now() - interval '1 second'")
        .execute(&e.owner)
        .await
        .unwrap();
    agent_api::signup::provisioning::reconcile(&e.state)
        .await
        .unwrap();
    assert_eq!(entitlement(&e, t, "u-m").await.0, "revoked");
    let g = lock(&up.mock);
    assert_eq!(g.revoked, ["lic-u-m"]);
    assert!(g.issued.is_empty());
}

/// The tenant path: a suspend commits while the re-issue revokes the tenant licence. The old licence is revoked and
/// cleared (never a dead token held), nothing is minted for the suspended tenant (409 `concurrent_change`), and the
/// tick mints the replacement once it is resumed.
#[tokio::test]
async fn a_tenant_reissue_racing_a_suspend_mints_nothing_until_resume() {
    let (e, up) = setup_slow_revoke().await;
    let t = licensed(&e, "Suspendable").await;
    let (reached, release) = gate(&up, "lic-tenant");
    let ri = spawn_send(
        &e,
        "POST",
        &format!("/api/tenants/{t}/licence/reissue"),
        &super_admin(),
        json!({"tenant_licence": true}),
    );
    within(reached.notified()).await;
    let actor = agent_api::audit::Actor::system("service:test");
    agent_api::tenants::lifecycle::set_status(&e.state, &actor, t, "suspended")
        .await
        .unwrap();
    release.notify_one();
    let (s, b) = within(ri).await.unwrap();
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "concurrent_change");
    let (id, token, pending) = tenant_licence(&e, t).await;
    assert!(id.is_none() && token.is_none(), "no dead token is held");
    assert!(pending.is_empty());
    assert_eq!(lock(&up.mock).revoked, ["lic-tenant"]);
    assert!(
        lock(&up.mock).issued.is_empty(),
        "nothing minted while suspended"
    );
    agent_api::tenants::lifecycle::set_status(&e.state, &actor, t, "active")
        .await
        .unwrap();
    agent_api::signup::provisioning::reconcile(&e.state)
        .await
        .unwrap();
    let now = tenant_licence(&e, t)
        .await
        .0
        .expect("the tick minted the replacement");
    assert_ne!(now, "lic-tenant");
}

/// A parked id that licences already revoked (the revoke landed there, but timed out here: no tombstone). Licences
/// answers `{"revoked": true}` again for it, so the drain unparks it instead of re-parking it forever.
#[tokio::test]
async fn an_already_revoked_parked_id_is_unparked_by_the_drain() {
    let (e, up) = setup().await;
    let t = licensed(&e, "Drainco").await;
    sqlx::query("UPDATE tenants SET licence_revoke_pending = '{lic-gone}' WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    lock(&up.mock).revoked.push("lic-gone".into());
    agent_api::signup::provisioning::reconcile(&e.state)
        .await
        .unwrap();
    assert!(tenant_licence(&e, t).await.2.is_empty(), "unparked");
    assert_eq!(count_calls(&up, "rerevoke:lic-gone"), 1);
    let revoke = tenant_rows(&e, t, "license.revoke").await;
    assert!(
        revoke.iter().any(|(_, d)| d["license_id"] == "lic-gone"),
        "tombstoned: {revoke:?}"
    );
    // tombstoned now: a later revoke of the id (an offboard's, say) makes no licences call
    let actor = agent_api::audit::Actor::system("service:test");
    let r =
        agent_api::tenants::lifecycle::revoke_tenant_licence(&e.state, &actor, t, "lic-gone", true)
            .await
            .unwrap();
    assert_eq!(
        r,
        agent_api::members::RevokeResult::Revoked("lic-gone".into())
    );
    assert_eq!(count_calls(&up, "rerevoke:lic-gone"), 1, "no second call");
}

/// `{"revoked": false}` is licences' "License not found" (an unknown id or another tenant's): it is NOT taken as
/// already revoked. The id stays parked and nothing is tombstoned.
#[tokio::test]
async fn a_revoked_false_answer_stays_failed() {
    let (e, up) = setup().await;
    let t = licensed(&e, "Unknownco").await;
    sqlx::query("UPDATE tenants SET licence_revoke_pending = '{lic-unknown}' WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    lock(&up.mock).revoke_noop_ids = vec!["lic-unknown".into()];
    agent_api::signup::provisioning::reconcile(&e.state)
        .await
        .unwrap();
    assert_eq!(
        tenant_licence(&e, t).await.2,
        ["lic-unknown"],
        "still parked"
    );
    assert!(
        tenant_rows(&e, t, "license.revoke")
            .await
            .iter()
            .all(|(_, d)| d["license_id"] != "lic-unknown"),
        "never tombstoned"
    );
    let actor = agent_api::audit::Actor::system("service:test");
    let r = agent_api::tenants::lifecycle::revoke_tenant_licence(
        &e.state,
        &actor,
        t,
        "lic-unknown",
        true,
    )
    .await
    .unwrap();
    assert!(
        matches!(
            r,
            agent_api::members::RevokeResult::Failed("licence_not_revoked", _)
        ),
        "{r:?}"
    );
}
