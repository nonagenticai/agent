//! Licence tokens under the `LicenceToken` keyring (B7 T1, crate::crypto::licence): the move from the legacy
//! `LICENSE_TOKEN_ENC_KEY` to `LICENSE_TOKEN_ENC_KEYS`, and the `v2:`-only CHECKs of 0151. Real Postgres 18 (one
//! container for this binary). Keys are built at runtime: no key literal is committed.
use crate::common;

use std::collections::HashMap;

use agent_api::config::Config;
use axum::http::StatusCode;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use common::{TestEnv, get, insert_tenant, send};
use sqlx::PgPool;
use uuid::Uuid;

/// The legacy key bytes of every test (`common::test_token_key`: bytes 0..32).
fn legacy_bytes() -> [u8; 32] {
    let v: Vec<u8> = (0u8..32).collect();
    v.try_into().unwrap()
}

/// Boot the API on `url` with the test environment adjusted by `tweak`.
async fn boot(url: &str, tweak: impl FnOnce(&mut HashMap<String, String>)) -> TestEnv {
    let mut vars = common::base_env(url);
    tweak(&mut vars);
    let cfg = Config::from_lookup(|k| vars.get(k).cloned()).expect("test config");
    let state = agent_api::state::build_for_tests(cfg, common::TEST_SECRET)
        .await
        .expect("state");
    let owner = PgPool::connect(url).await.expect("owner pool");
    let app = agent_api::router(state.clone());
    TestEnv::new(state, app, owner)
}

async fn tenant_token(owner: &PgPool, t: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT licence_token_encrypted FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn tokens_sealed_under_the_legacy_key_open_under_licence_token_enc_keys() {
    let url = common::fresh_database().await;
    // transitional: only LICENSE_TOKEN_ENC_KEY, so the set is {v1: legacy} and tokens are `v2:v1:`
    let first = boot(&url, |_| {}).await;
    assert_eq!(first.state.identity.cipher.current_kid(), "v1");
    let t = Uuid::new_v4();
    insert_tenant(&first.owner, t, "Gamma").await;
    let sealed = first
        .state
        .identity
        .cipher
        .seal(t, &format!("tok-t-{t}"))
        .unwrap();
    assert!(sealed.starts_with("v2:v1:"), "{sealed}");
    sqlx::query(
        "UPDATE tenants SET licence_id = 'lic-t', licence_token_encrypted = $2 WHERE id = $1",
    )
    .bind(t)
    .bind(&sealed)
    .execute(&first.owner)
    .await
    .unwrap();

    // the legacy bytes live on under kid `v1`; `k2` is current; the legacy variable is gone
    let keys = format!(
        r#"{{"v1":"{}","k2":"{}"}}"#,
        STANDARD.encode(legacy_bytes()),
        STANDARD.encode([9u8; 32])
    );
    let e = boot(&url, |v| {
        v.remove("LICENSE_TOKEN_ENC_KEY");
        v.insert("LICENSE_TOKEN_ENC_KEYS".into(), keys);
        v.insert("LICENSE_TOKEN_ENC_KEY_CURRENT".into(), "k2".into());
    })
    .await;
    let c = &e.state.identity.cipher;
    assert_eq!(c.current_kid(), "k2");
    let stored = tenant_token(&e.owner, t).await.unwrap();
    assert_eq!(stored, sealed, "the boot rewrites no token");
    assert_eq!(c.open(t, &stored).unwrap(), format!("tok-t-{t}"));
    let (s, body) = send(&e.app, get("/ready", None)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn the_tables_refuse_a_v1_or_unversioned_token() {
    let url = common::fresh_database().await;
    let e = boot(&url, |_| {}).await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Eps").await;
    sqlx::query(
        "INSERT INTO member_entitlement (tenant_id, user_sub, license_id, license_subject_id, token_encrypted, status) \
         VALUES ($1, 'u-1', 'lic-u-1', 'subject-1', 'v2:k1:AAAA', 'active')",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    for bad in [
        "v1:AAAA",
        "v1:c2VhbGVk",
        "plaintext",
        "v2:",
        "v2:bad kid:AAAA",
        "v3:k1:AAAA",
    ] {
        let err = sqlx::query("UPDATE tenants SET licence_token_encrypted = $2 WHERE id = $1")
            .bind(t)
            .bind(bad)
            .execute(&e.owner)
            .await
            .unwrap_err();
        assert_eq!(
            common::sqlstate(&err).as_deref(),
            Some("23514"),
            "tenant {bad}"
        );
        let err = sqlx::query(
            "UPDATE member_entitlement SET token_encrypted = $2 WHERE tenant_id = $1 AND user_sub = 'u-1'",
        )
        .bind(t)
        .bind(bad)
        .execute(&e.owner)
        .await
        .unwrap_err();
        assert_eq!(
            common::sqlstate(&err).as_deref(),
            Some("23514"),
            "member {bad}"
        );
    }
    for ok in [Some("v2:k1:AAAA"), None] {
        sqlx::query("UPDATE tenants SET licence_token_encrypted = $2 WHERE id = $1")
            .bind(t)
            .bind(ok)
            .execute(&e.owner)
            .await
            .unwrap();
    }
}
