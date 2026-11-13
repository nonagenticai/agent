//! Known-customer web chat, binding side (crate::conversations::bindings): `settings.identity` is checked on write
//! (web chat only), the HS256 secret is made by `POST …/identity-secret` (k1, then k2 with the old one kept as
//! `identity_secret_prev` for 24 h), `?revoke_sessions=true` blocks the identified sessions only, and no secret is ever
//! in a view or accepted from a request. Real Postgres 18.
use crate::common;

use axum::http::StatusCode;
use common::conv::{self, Ctx, code};
use serde_json::{Value, json};
use uuid::Uuid;

const BASE: &str = "/api/jobs/j1/channel-bindings";
const SESSIONS: &str = "/api/internal/channel/webchat/sessions";

fn identity(mode: &str) -> Value {
    json!({"mode": mode, "issuer": "https://shop.example", "session_hours": 8,
           "sign_in_url": "https://shop.example/login"})
}

/// `PUT` the web chat binding's settings.
async fn put_settings(c: &Ctx, settings: Value) -> (StatusCode, Value) {
    c.call(
        "PUT",
        &format!("{BASE}/{}", c.wc),
        "u-int",
        json!({"settings": settings}),
    )
    .await
}

async fn rotate(c: &Ctx, query: &str) -> (StatusCode, Value) {
    c.call(
        "POST",
        &format!("{BASE}/{}/identity-secret{query}", c.wc),
        "u-int",
        json!({}),
    )
    .await
}

/// The binding's opened credential map.
async fn credential(c: &Ctx) -> Value {
    use agent_api::conversations::{bindings, store};
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    let sealed: (Option<Vec<u8>>, Option<Vec<u8>>, Option<i32>) = sqlx::query_as(
        "SELECT secret_enc, secret_nonce, secret_dek_version FROM connector_bindings WHERE id = $1",
    )
    .bind(c.wc)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    bindings::open_credential(&mut tx, &c.e.state, c.t, c.wc, sealed)
        .await
        .expect("the credential opens")
}

#[tokio::test]
async fn identity_settings_are_checked_and_only_for_web_chat() {
    let c = conv::setup().await;
    let allowed = json!({"allowed_origins": ["https://shop.example"]});
    for mode in ["off", "hs256"] {
        let mut s = allowed.clone();
        s["identity"] = identity(mode);
        let (st, b) = put_settings(&c, s).await;
        assert_eq!(st, StatusCode::OK, "{mode}: {b}");
        assert_eq!(b["settings"]["identity"]["mode"], mode);
    }
    for (why, bad) in [
        ("unknown mode", json!({"mode": "rsa"})),
        ("not an object", json!("hs256")),
        ("session hours out of range", {
            let mut i = identity("hs256");
            i["session_hours"] = json!(25);
            i
        }),
        ("hs256 needs an issuer", json!({"mode": "hs256"})),
        ("es256 needs keys", identity("es256")),
    ] {
        let mut s = allowed.clone();
        s["identity"] = bad;
        let (st, b) = put_settings(&c, s).await;
        assert_eq!(
            (st, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_settings"),
            "{why}: {b}"
        );
    }
    // a binding of another kind refuses it
    let (st, b) = c
        .call(
            "POST",
            BASE,
            "u-int",
            json!({"kind": "telegram", "side": "customer", "label": "S",
                   "settings": {"identity": identity("hs256")},
                   "credential": {"bot_token": "123456789:test-not-a-real-bot-token-000000",
                                  "webhook_secret": "test-webhook-secret-0001"}}),
        )
        .await;
    assert_eq!(
        (st, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_settings"),
        "{b}"
    );
}

#[tokio::test]
async fn the_secret_rotates_k1_to_k2_and_keeps_the_old_one_for_a_day() {
    let c = conv::setup().await;
    let (st, b) = rotate(&c, "").await;
    assert_eq!(
        (st, code(&b)),
        (StatusCode::CONFLICT, "identity_mode_not_hs256"),
        "no identity settings yet: {b}"
    );
    let (st, b) = put_settings(
        &c,
        json!({"allowed_origins": ["https://shop.example"], "identity": identity("es256")}),
    )
    .await;
    assert_ne!(st, StatusCode::OK, "{b}");
    let (st, b) = put_settings(
        &c,
        json!({"allowed_origins": ["https://shop.example"], "identity": identity("hs256")}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");

    let (st, b) = c
        .call(
            "POST",
            &format!("{BASE}/{}/identity-secret", c.wc),
            "u-agent",
            json!({}),
        )
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "integrations_manager only: {b}");

    let (st, one) = rotate(&c, "").await;
    assert_eq!(st, StatusCode::OK, "{one}");
    assert_eq!(one["kid"], "k1");
    let s1 = one["secret"].as_str().unwrap().to_string();
    assert!(s1.starts_with("ids_"), "{s1}");
    let cred = credential(&c).await;
    assert_eq!(cred["identity_secret"], format!("k1:{s1}"));
    assert!(cred.get("identity_secret_prev").is_none(), "{cred}");

    let (st, two) = rotate(&c, "").await;
    assert_eq!(st, StatusCode::OK, "{two}");
    assert_eq!(two["kid"], "k2");
    let s2 = two["secret"].as_str().unwrap().to_string();
    assert_ne!(s1, s2);
    let cred = credential(&c).await;
    assert_eq!(cred["identity_secret"], format!("k2:{s2}"));
    assert_eq!(cred["identity_secret_prev"], format!("k1:{s1}"));
    let hours: Vec<String> = c
        .texts(&format!(
            "SELECT round(extract(epoch FROM ((settings->'identity'->>'prev_valid_until')::timestamptz - now())) / 3600)::text \
               FROM connector_bindings WHERE id = '{}'",
            c.wc
        ))
        .await;
    assert_eq!(hours, vec!["24"]);

    // a third rotation drops k1 for good
    let (_, three) = rotate(&c, "").await;
    assert_eq!(three["kid"], "k3");
    let cred = credential(&c).await;
    assert_eq!(cred["identity_secret_prev"], format!("k2:{s2}"));

    // the secrets are in no view and no audit row
    let (_, list) = c.call("GET", BASE, "u-int", json!({})).await;
    let seen = list.to_string();
    let s3 = three["secret"].as_str().unwrap();
    for s in [s1.as_str(), s2.as_str(), s3] {
        assert!(!seen.contains(s), "a secret is in the view");
    }
    assert!(!seen.contains("identity_secret"), "{seen}");
    let audit = c
        .texts("SELECT details::text FROM audit_event WHERE action = 'channel_binding.identity_secret_rotate'")
        .await;
    assert_eq!(audit.len(), 3);
    for s in [&s1, &s2, s3] {
        assert!(
            audit.iter().all(|a| !a.contains(s)),
            "a secret is in the audit"
        );
    }

    // other settings and credentials updates keep the secrets
    let (st, b) = put_settings(&c, json!({"allowed_origins": ["https://shop.example"], "identity": identity("hs256"), "display": {}})).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert!(
        b["settings"]["identity"]["prev_valid_until"].is_string(),
        "prev_valid_until is the server's: {b}"
    );
    let (st, b) = c
        .call(
            "PUT",
            &format!("{BASE}/{}", c.wc),
            "u-int",
            json!({"credential": {"identity_secret": "k9:ids_forged"}}),
        )
        .await;
    assert_eq!(
        (st, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_credential"),
        "{b}"
    );
    let (st, b) = c
        .call(
            "PUT",
            &format!("{BASE}/{}", c.wc),
            "u-int",
            json!({"credential": null}),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let cred = credential(&c).await;
    assert_eq!(cred["identity_secret"], format!("k3:{s3}"));
}

async fn session(c: &Ctx, identified: bool) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO web_chat_session (tenant_id, job_id, binding_id, token_hash, origin, ip_hash, expires_at, identified_at, \
                                       customer_ref) \
         VALUES ($1, $2, $3, sha256($4::bytea), 'https://shop.example', sha256($4::bytea), now() + interval '1 hour', \
                 CASE WHEN $5 THEN now() END, CASE WHEN $5 THEN 'cust-1001' END) RETURNING id",
    )
    .bind(c.t)
    .bind(c.job)
    .bind(c.wc)
    .bind(Uuid::new_v4().to_string().into_bytes())
    .bind(identified)
    .fetch_one(&c.e.owner)
    .await
    .unwrap()
}

async fn reason(c: &Ctx, id: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT blocked_reason FROM web_chat_session WHERE id = $1")
        .bind(id)
        .fetch_one(&c.e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn revoke_sessions_blocks_identified_sessions_only() {
    let c = conv::setup().await;
    let (st, b) = put_settings(
        &c,
        json!({"allowed_origins": ["https://shop.example"], "identity": identity("hs256")}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let (anon, known) = (session(&c, false).await, session(&c, true).await);

    let (st, b) = rotate(&c, "").await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(
        (reason(&c, anon).await, reason(&c, known).await),
        (None, None),
        "a plain rotation signs nobody out"
    );

    let (st, b) = rotate(&c, "?revoke_sessions=true").await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["kid"], "k2");
    assert_eq!(reason(&c, anon).await, None, "anonymous chats stay open");
    assert_eq!(reason(&c, known).await.as_deref(), Some("identity_rotated"));
}

// ---- the admin tries a TEST channel as a known customer (POST …/test-session) ----------------------------------------

async fn test_session(c: &Ctx, sub: &str, customer: &str) -> (StatusCode, Value) {
    c.call(
        "POST",
        &format!("{BASE}/{}/test-session", c.wc),
        sub,
        json!({"customer_ref": customer}),
    )
    .await
}

async fn set_env(c: &Ctx, env: &str) {
    sqlx::query("UPDATE connector_bindings SET env = $2 WHERE id = $1")
        .bind(c.wc)
        .bind(env)
        .execute(&c.e.owner)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_session_is_refused_for_a_wrong_role_a_live_channel_and_missing_identity() {
    let c = conv::setup().await;
    set_env(&c, "test").await;
    let (st, b) = test_session(&c, "u-agent", "cust-1001").await;
    assert_eq!(st, StatusCode::FORBIDDEN, "integrations_manager only: {b}");
    assert_eq!(b["error"]["missing_role"], "integrations_manager");

    // identity never configured: nothing to sign with
    let (st, b) = test_session(&c, "u-int", "cust-1001").await;
    assert_eq!(
        (st, code(&b)),
        (StatusCode::CONFLICT, "identity_not_configured"),
        "{b}"
    );
    // mode on, but the secret was never made
    let (st, b) = put_settings(
        &c,
        json!({"allowed_origins": ["https://shop.example"], "identity": identity("hs256")}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let (st, b) = test_session(&c, "u-int", "cust-1001").await;
    assert_eq!(
        (st, code(&b)),
        (StatusCode::CONFLICT, "identity_not_configured"),
        "{b}"
    );
    let (st, b) = rotate(&c, "").await;
    assert_eq!(st, StatusCode::OK, "{b}");

    // a customer reference is a plain label: empty or with spaces is refused before anything is signed
    for bad in ["", "   ", "two words", "a/b"] {
        let (st, b) = test_session(&c, "u-int", bad).await;
        assert_eq!(
            (st, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_customer_ref"),
            "{bad:?}: {b}"
        );
    }
    let (st, b) = test_session(&c, "u-int", "cust-1001").await;
    assert_eq!(st, StatusCode::OK, "{b}");

    // a published channel is not a test: refused, production traffic is untouched
    set_env(&c, "prod").await;
    let (st, b) = test_session(&c, "u-int", "cust-1001").await;
    assert_eq!(
        (st, code(&b)),
        (StatusCode::CONFLICT, "binding_live"),
        "{b}"
    );
    assert!(
        c.texts("SELECT jti FROM webchat_assertion_jti")
            .await
            .is_empty(),
        "minting consumes nothing"
    );
}

#[tokio::test]
async fn the_minted_test_assertion_verifies_binds_the_customer_and_is_single_use() {
    let c = conv::setup().await;
    set_env(&c, "test").await;
    let (st, b) = put_settings(
        &c,
        json!({"allowed_origins": ["https://shop.example"], "identity": identity("hs256")}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let (_, secret) = rotate(&c, "").await;
    let secret = secret["secret"].as_str().unwrap().to_string();

    let (st, b) = test_session(&c, "u-int", "cust-1001").await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let url = b["url"].as_str().unwrap().to_string();
    assert!(
        url.contains("/webchat/demo?key=wk_test#assertion="),
        "the hosted test page, assertion in the fragment: {}",
        url.split('#').next().unwrap()
    );
    assert!(
        !url.split('#').next().unwrap().contains("assertion"),
        "never in the query string"
    );
    assert!(!url.contains(&secret), "the secret is never returned");
    let assertion = url.split_once("#assertion=").unwrap().1.to_string();

    // the session route accepts it exactly as it accepts a tenant site's: a known customer, bound to the reference
    let open = |ip: &str| {
        json!({"binding_id": c.wc, "widget_key": "wk_test", "origin": "https://chat.nonagentic.ai",
               "ip_hash": ip, "customer_assertion": assertion})
    };
    let (st, b) = c.internal(SESSIONS, open(&"ab".repeat(32))).await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    assert_eq!(b["identified"], json!(true));
    let who: Vec<String> = c
        .texts(&format!(
            "SELECT customer_ref FROM web_chat_session WHERE id = '{}'",
            b["session_id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(who, vec!["cust-1001"]);

    // single use: the same link opened again (another visitor) is replayed
    let (st, b) = c.internal(SESSIONS, open(&"cd".repeat(32))).await;
    assert_eq!(
        (st, code(&b)),
        (StatusCode::UNAUTHORIZED, "assertion_replayed"),
        "{b}"
    );
    // a second mint is a fresh assertion that works
    let (st, b2) = test_session(&c, "u-int", "cust-1001").await;
    assert_eq!(st, StatusCode::OK, "{b2}");
    assert_ne!(b2["url"].as_str().unwrap(), url);

    // audited with ids only: the assertion and the customer reference are in no audit row
    let n = c
        .texts(
            "SELECT count(*)::text FROM audit_event WHERE action = 'channel_binding.test_session'",
        )
        .await;
    assert_eq!(n, vec!["2"]);
    let leaked = c
        .texts(&format!(
            "SELECT count(*)::text FROM audit_event WHERE coalesce(details::text, '') LIKE '%{assertion}%' \
                OR coalesce(details::text, '') LIKE '%cust-1001%'"
        ))
        .await;
    assert_eq!(leaked, vec!["0"]);
}
