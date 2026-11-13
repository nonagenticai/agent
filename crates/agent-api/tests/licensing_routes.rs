//! B7 T3 licensing ops routes (overview, reseal all / one tenant, verdict refresh) and T3b leaked-token re-mint
//! (crate::routes::licensing, crate::licensing::{overview, reseal, reissue}). Real Postgres 18 (one container for this
//! binary); Keycloak and licences are the in-process fake. Keys are built at runtime: no key literal is committed.
use crate::common;

use std::collections::HashMap;

use agent_api::config::Config;
use axum::http::StatusCode;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use common::fakes::{self, REGION_EU, Upstream, lock};
use common::{TestEnv, get, insert_user, json, person, send, super_admin};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

// ---- helpers ------------------------------------------------------------------------------------------------------

/// `{"k1": A, "k2": B}` with `current` current; the legacy variable removed.
fn keys(v: &mut HashMap<String, String>, current: &str) {
    let k = format!(
        r#"{{"k1":"{}","k2":"{}"}}"#,
        STANDARD.encode([3u8; 32]),
        STANDARD.encode([7u8; 32])
    );
    v.remove("LICENSE_TOKEN_ENC_KEY");
    v.insert("LICENSE_TOKEN_ENC_KEYS".into(), k);
    v.insert("LICENSE_TOKEN_ENC_KEY_CURRENT".into(), current.into());
}

/// Boot the API on `url` with the key set current at `current`.
async fn boot(url: &str, current: &str) -> TestEnv {
    let mut vars = common::base_env(url);
    keys(&mut vars, current);
    let cfg = Config::from_lookup(|k| vars.get(k).cloned()).expect("test config");
    let state = agent_api::state::build_for_tests(cfg, common::TEST_SECRET)
        .await
        .expect("state");
    let owner = PgPool::connect(url).await.expect("owner pool");
    let app = agent_api::router(state.clone());
    TestEnv::new(state, app, owner)
}

/// A tenant holding licence `lic-<name>` with its token sealed by `e` (or `raw` stored as is), plus a member `u-<name>`
/// whose token is sealed too.
async fn sealed_tenant(e: &TestEnv, name: &str, raw: Option<&str>) -> Uuid {
    let t = Uuid::new_v4();
    let own = match raw {
        Some(r) => r.to_string(),
        None => e
            .state
            .identity
            .cipher
            .seal(t, &format!("tok-{name}"))
            .unwrap(),
    };
    sqlx::query(
        "INSERT INTO tenants (id, name, residency, updated_by, licence_id, licence_token_encrypted) \
         VALUES ($1, $2, 'eu', 'test', $3, $4)",
    )
    .bind(t)
    .bind(name)
    .bind(format!("lic-{name}"))
    .bind(own)
    .execute(&e.owner)
    .await
    .unwrap();
    let sub = format!("u-{name}");
    insert_user(&e.owner, &sub, t, "tenant_admin").await;
    sqlx::query(
        "INSERT INTO member_entitlement (tenant_id, user_sub, license_id, license_subject_id, token_encrypted, status) \
         VALUES ($1, $2, 'lic-' || $2, 'subject-1', $3, 'active')",
    )
    .bind(t)
    .bind(&sub)
    .bind(
        e.state
            .identity
            .cipher
            .seal(t, &format!("tok-m-{name}"))
            .unwrap(),
    )
    .execute(&e.owner)
    .await
    .unwrap();
    t
}

async fn tokens(owner: &PgPool, t: Uuid) -> (Option<String>, Option<String>) {
    let own: Option<String> =
        sqlx::query_scalar("SELECT licence_token_encrypted FROM tenants WHERE id = $1")
            .bind(t)
            .fetch_one(owner)
            .await
            .unwrap();
    let member: Option<String> =
        sqlx::query_scalar("SELECT token_encrypted FROM member_entitlement WHERE tenant_id = $1")
            .bind(t)
            .fetch_one(owner)
            .await
            .unwrap();
    (own, member)
}

async fn post(e: &TestEnv, path: &str, token: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, json("POST", path, Some(token), body)).await
}

async fn platform_rows(owner: &PgPool, action: &str) -> Vec<(Option<Uuid>, String, Value)> {
    sqlx::query_as(
        "SELECT subject_tenant, status, details FROM platform_audit WHERE action = $1 ORDER BY id",
    )
    .bind(action)
    .fetch_all(owner)
    .await
    .unwrap()
}

fn row(overview: &Value, t: Uuid) -> &Value {
    overview["tenants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["tenant_id"] == t.to_string())
        .expect("the tenant is listed")
}

fn kid<'a>(overview: &'a Value, k: &str) -> &'a Value {
    overview["keyring"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kid"] == k)
        .expect("the kid is listed")
}

// ---- auth ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn routes_need_super_admin() {
    let e = common::env().await;
    let t = Uuid::new_v4();
    common::insert_tenant(&e.owner, t, "Guardco").await;
    insert_user(&e.owner, "u-ta", t, "tenant_admin").await;
    let ta = person("u-ta", &[]);
    let (s, b) = send(&e.app, get("/api/platform/licensing", Some(&ta))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["code"], "super_admin_required");
    for path in [
        "/api/platform/licence-tokens/reseal".to_string(),
        "/api/platform/licence-verdicts/refresh".to_string(),
        format!("/api/tenants/{t}/licence/reseal"),
    ] {
        let (s, b) = post(&e, &path, &ta, json!({})).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{path}: {b}");
        assert_eq!(b["error"]["code"], "super_admin_required", "{path}");
    }
    let (s, b) = post(
        &e,
        &format!("/api/tenants/{t}/licence/remint"),
        &ta,
        json!({"confirm_name": "Guardco"}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["code"], "super_admin_required");
    assert!(platform_rows(&e.owner, "license.reseal").await.is_empty());
}

// ---- reseal + overview --------------------------------------------------------------------------------------------

#[tokio::test]
async fn reseal_moves_every_token_to_the_current_kid_counts_unreadable_and_is_idempotent() {
    let url = common::fresh_database().await;
    let old = boot(&url, "k1").await;
    let a = sealed_tenant(&old, "alpha", None).await;
    let b = sealed_tenant(&old, "beta", None).await;
    // well-formed but does not open: counted, never fatal
    let bad = sealed_tenant(
        &old,
        "gamma",
        Some("v2:k1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
    )
    .await;
    let (own, _) = tokens(&old.owner, a).await;
    assert!(own.unwrap().starts_with("v2:k1:"));

    let e = boot(&url, "k2").await;
    let sa = super_admin();
    let (s, r) = post(&e, "/api/platform/licence-tokens/reseal", &sa, json!({})).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["kid"], "k2");
    // alpha own + member, beta own + member, gamma's member
    assert_eq!(r["resealed"], 5, "{r}");
    assert_eq!(r["remaining"], 1, "{r}");
    let moved: Vec<&str> = r["tenants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for t in [a, b, bad] {
        assert!(moved.contains(&t.to_string().as_str()), "{r}");
    }
    for (t, name) in [(a, "alpha"), (b, "beta")] {
        let (own, member) = tokens(&e.owner, t).await;
        let (own, member) = (own.unwrap(), member.unwrap());
        assert!(own.starts_with("v2:k2:") && member.starts_with("v2:k2:"));
        assert_eq!(
            e.state.identity.cipher.open(t, &own).unwrap(),
            format!("tok-{name}")
        );
        assert_eq!(
            e.state.identity.cipher.open(t, &member).unwrap(),
            format!("tok-m-{name}")
        );
    }
    let (own, _) = tokens(&e.owner, bad).await;
    assert!(
        own.unwrap().starts_with("v2:k1:"),
        "an unreadable token is left as it is"
    );

    // one run row, audit with counts only
    let run: (String, i32, i32, i32, Vec<Uuid>) = sqlx::query_as(
        "SELECT kid, resealed, remaining, unreadable, tenants FROM licence_reseal_run ORDER BY at DESC LIMIT 1",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!((run.0.as_str(), run.1, run.2, run.3), ("k2", 5, 1, 1));
    assert_eq!(run.4.len(), 3);
    let audit = platform_rows(&e.owner, "license.reseal").await;
    assert_eq!(audit.len(), 1);
    let (subject, status, details) = &audit[0];
    assert!(subject.is_none());
    assert_eq!(status, "ok");
    assert_eq!(details["resealed"], 5);
    let text = details.to_string();
    for t in [a, b, bad] {
        assert!(!text.contains(&t.to_string()), "no tenant id: {text}");
    }
    assert!(!text.contains("v2:") && !text.contains("tok-"), "{text}");

    // idempotent
    let (s, r2) = post(&e, "/api/platform/licence-tokens/reseal", &sa, json!({})).await;
    assert_eq!(s, StatusCode::OK, "{r2}");
    assert_eq!(
        (r2["resealed"].as_u64(), r2["remaining"].as_u64()),
        (Some(0), Some(1))
    );
    assert_eq!(r2["tenants"], json!([]));

    // the overview: kids with their counts, the last run, never a token
    let (s, o) = send(&e.app, get("/api/platform/licensing", Some(&sa))).await;
    assert_eq!(s, StatusCode::OK, "{o}");
    assert_eq!(o["current_kid"], "k2");
    assert_eq!(kid(&o, "k2")["current"], true);
    assert_eq!(kid(&o, "k2")["sealed"], 5);
    assert_eq!(kid(&o, "k1")["sealed"], 1, "only the unreadable token");
    assert_eq!(kid(&o, "k1")["current"], false);
    assert_eq!(o["last_reseal"]["resealed"], 0);
    assert_eq!(o["last_reseal"]["kid"], "k2");
    let ra = row(&o, a);
    assert_eq!(ra["name"], "alpha");
    assert_eq!(ra["licence"]["licence_id"], "lic-alpha");
    assert_eq!(ra["licence"]["kid"], "k2");
    assert!(ra["verdict"].is_null(), "no introspect configured");
    assert!(ra["last_reissue"].is_null());
    assert_eq!(o["verdicts_configured"], false);
    let text = o.to_string();
    assert!(
        !text.contains("v2:") && !text.contains("tok-"),
        "overview never contains a token: {text}"
    );
}

#[tokio::test]
async fn reseal_one_tenant_and_its_refusals() {
    let url = common::fresh_database().await;
    let old = boot(&url, "k1").await;
    let a = sealed_tenant(&old, "solo", None).await;
    let other = sealed_tenant(&old, "other", None).await;
    let e = boot(&url, "k2").await;
    let sa = super_admin();
    let (s, r) = post(
        &e,
        &format!("/api/tenants/{a}/licence/reseal"),
        &sa,
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(
        r,
        json!({"kid": "k2", "resealed": 2, "remaining": 0, "tenants": [a]})
    );
    let (own, _) = tokens(&e.owner, other).await;
    assert!(own.unwrap().starts_with("v2:k1:"), "only that tenant moved");
    let audit = platform_rows(&e.owner, "license.reseal").await;
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].0, Some(a));
    // again: nothing to move
    let (_, r) = post(
        &e,
        &format!("/api/tenants/{a}/licence/reseal"),
        &sa,
        json!({}),
    )
    .await;
    assert_eq!(r["resealed"], 0);

    // 404 unknown tenant, 409 no licence
    let (s, b) = post(
        &e,
        &format!("/api/tenants/{}/licence/reseal", Uuid::new_v4()),
        &sa,
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(b["error"]["code"], "tenant_not_found");
    let bare = Uuid::new_v4();
    common::insert_tenant(&e.owner, bare, "Bare").await;
    let (s, b) = post(
        &e,
        &format!("/api/tenants/{bare}/licence/reseal"),
        &sa,
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "no_licence");
}

#[tokio::test]
async fn nudge_answers_the_overview_with_refresh_counts() {
    let e = common::env().await;
    let t = Uuid::new_v4();
    common::insert_tenant(&e.owner, t, "Nudgeco").await;
    let (s, o) = post(
        &e,
        "/api/platform/licence-verdicts/refresh",
        &super_admin(),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{o}");
    assert_eq!(row(&o, t)["name"], "Nudgeco");
    assert!(row(&o, t)["licence"].is_null());
    // no LICENSE_INTROSPECT_URL: the verdict half idles, nothing written
    assert_eq!(o["refreshed"]["written"], 0);
    assert!(o["current_kid"].is_string());
}

// ---- T3b re-mint ----------------------------------------------------------------------------------------------------

async fn setup() -> (TestEnv, Upstream) {
    let up = fakes::upstream().await;
    let e = common::env_with(|v| fakes::identity_env(&up, v)).await;
    (e, up)
}

/// A tenant (residency eu) holding its own licence `lic-tenant` (token unreadable to this keyring: irrelevant to a
/// re-mint) and a member `u-m` holding `lic-u-m`.
async fn licensed(e: &TestEnv, name: &str) -> Uuid {
    let t = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO tenants (id, name, residency, updated_by, licence_id, licence_token_encrypted) \
         VALUES ($1, $2, 'eu', 'test', 'lic-tenant', 'v2:gone:old-sealed')",
    )
    .bind(t)
    .bind(name)
    .execute(&e.owner)
    .await
    .unwrap();
    insert_user(&e.owner, "u-m", t, "tenant_admin").await;
    sqlx::query(
        "INSERT INTO member_entitlement (tenant_id, user_sub, license_id, license_subject_id, token_encrypted, status) \
         VALUES ($1, 'u-m', 'lic-u-m', 'subject-old', 'v2:gone:old-sealed', 'active')",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    t
}

async fn remint(e: &TestEnv, t: Uuid, body: Value) -> (StatusCode, Value) {
    post(
        e,
        &format!("/api/tenants/{t}/licence/remint"),
        &super_admin(),
        body,
    )
    .await
}

async fn reissue_rows(owner: &PgPool, t: Uuid) -> Vec<(String, String, String, String)> {
    sqlx::query_as(
        "SELECT kind, reason, old_revoke, status FROM licence_reissue WHERE tenant_id = $1 ORDER BY at, id",
    )
    .bind(t)
    .fetch_all(owner)
    .await
    .unwrap()
}

async fn attention(e: &TestEnv, t: Uuid) -> Vec<Value> {
    let mut tx = agent_api::tenancy::begin_scoped(&e.state.db, t)
        .await
        .unwrap();
    let row = agent_api::readmodel::tenant::read(&mut tx, t)
        .await
        .unwrap()
        .unwrap();
    let items = agent_api::readmodel::home::attention(&mut tx, &row, 3600)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    items
}

#[tokio::test]
async fn remint_issues_new_and_revokes_old_at_once() {
    let (e, up) = setup().await;
    let t = licensed(&e, "Leakco").await;
    let (s, b) = remint(
        &e,
        t,
        json!({"confirm_name": " Leakco ", "reason": "pasted in a public channel"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["kind"], "remint");
    assert_eq!(b["members_unchanged"], true);
    let r = &b["results"][0];
    assert_eq!(r["holder"], "tenant");
    assert_eq!(r["old_licence_id"], "lic-tenant");
    assert_eq!(r["old_revoke"], "revoked");
    assert_eq!(r["status"], "reissued");
    let new = r["new_licence_id"].as_str().unwrap().to_string();
    {
        let g = lock(&up.mock);
        assert_eq!(g.revoked, ["lic-tenant"], "the leaked licence is revoked");
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
        assert!(
            rev < mint,
            "revoked before the new one exists: {:?}",
            g.calls
        );
    }
    let stored: Option<String> = sqlx::query_scalar("SELECT licence_id FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(stored.as_deref(), Some(new.as_str()));
    // the tombstone: the old id is never stored again
    let tomb: Vec<(String, String)> =
        sqlx::query_as("SELECT license_id, via FROM licence_revocation WHERE tenant_id = $1")
            .bind(t)
            .fetch_all(&e.owner)
            .await
            .unwrap();
    assert_eq!(tomb, [("lic-tenant".to_string(), "reissue".to_string())]);
    assert_eq!(
        reissue_rows(&e.owner, t).await,
        [(
            "remint".to_string(),
            "leaked".to_string(),
            "revoked".to_string(),
            "reissued".to_string()
        )]
    );
    // audit: the request carries kind + the note, never a token
    let req = platform_rows(&e.owner, "license.reissue_request").await;
    assert_eq!(req.len(), 1);
    assert_eq!(req[0].2["kind"], "remint");
    assert_eq!(req[0].2["note"], "pasted in a public channel");
    let done = platform_rows(&e.owner, "license.reissue").await;
    assert_eq!(done[0].2["kind"], "remint");
    for (_, _, d) in req.iter().chain(done.iter()) {
        assert!(!d.to_string().contains("v2:"), "{d}");
    }
    // the member is untouched; no attention item (the old licence is revoked)
    let lic: Option<String> = sqlx::query_scalar(
        "SELECT license_id FROM member_entitlement WHERE tenant_id = $1 AND user_sub = 'u-m'",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(lic.as_deref(), Some("lic-u-m"));
    assert!(!attention(&e, t).await.iter().any(|i| {
        i["id"]
            .as_str()
            .unwrap_or("")
            .starts_with("licence:remint:")
    }));
    // the overview shows it as the last re-issue
    let (_, o) = send(&e.app, get("/api/platform/licensing", Some(&super_admin()))).await;
    let last = &row(&o, t)["last_reissue"];
    assert_eq!(last["kind"], "remint");
    assert_eq!(last["reason"], "leaked");
    assert_eq!(last["old_still_valid"], false);
}

#[tokio::test]
async fn remint_wrong_name_is_422_and_changes_nothing() {
    let (e, up) = setup().await;
    let t = licensed(&e, "Namecheck").await;
    let (s, b) = remint(&e, t, json!({"confirm_name": "namecheck"})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "confirm_name_mismatch");
    let (s, b) = remint(&e, t, json!({})).await;
    assert!(s.is_client_error(), "confirm_name is required: {s} {b}");
    let (s, b) = remint(
        &e,
        t,
        json!({"confirm_name": "Namecheck", "reason": "mail ops@example.com"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "invalid_input");
    {
        let g = lock(&up.mock);
        assert!(g.revoked.is_empty() && g.issued.is_empty());
    }
    assert!(reissue_rows(&e.owner, t).await.is_empty());
    assert!(
        platform_rows(&e.owner, "license.reissue_request")
            .await
            .is_empty()
    );
    let stored: Option<String> = sqlx::query_scalar("SELECT licence_id FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(stored.as_deref(), Some("lic-tenant"));
}

#[tokio::test]
async fn remint_revoke_failure_answers_pending_and_raises_attention() {
    let (e, up) = setup().await;
    let t = licensed(&e, "Stuckco").await;
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
    let (s, b) = remint(&e, t, json!({"confirm_name": "Stuckco"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let r = &b["results"][0];
    assert_eq!(
        (r["status"].as_str(), r["old_revoke"].as_str()),
        (Some("pending"), Some("pending"))
    );
    let parked: Vec<String> =
        sqlx::query_scalar("SELECT licence_revoke_pending FROM tenants WHERE id = $1")
            .bind(t)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(parked, ["lic-tenant"]);
    let items = attention(&e, t).await;
    let item = items
        .iter()
        .find(|i| i["id"] == "licence:remint:lic-tenant")
        .unwrap_or_else(|| panic!("a licence attention item: {items:?}"));
    assert_eq!(item["kind"], "licence");
    assert_eq!(item["severity"], "blocking");
    // R2: the re-mint is recorded as pending
    assert_eq!(
        reissue_rows(&e.owner, t).await[0],
        (
            "remint".to_string(),
            "leaked".to_string(),
            "pending".to_string(),
            "pending".to_string()
        )
    );
    let (_, o) = send(&e.app, get("/api/platform/licensing", Some(&super_admin()))).await;
    assert_eq!(row(&o, t)["last_reissue"]["old_still_valid"], true, "{o}");
    // once the drain revokes it, the item is gone
    sqlx::query("UPDATE tenants SET licence_revoke_pending = '{}' WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    assert!(
        !attention(&e, t)
            .await
            .iter()
            .any(|i| i["id"] == "licence:remint:lic-tenant")
    );
}
