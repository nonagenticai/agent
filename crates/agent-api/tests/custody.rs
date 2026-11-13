//! B7 T8: credential custody (crate::connectors::custody) on a real Postgres 18: the per-tenant DEK wrapped by the
//! TenantData keyring, credentials sealed under it and bound to (tenant, instance, version), the fingerprint, the
//! crypto-shred on destroy, and 503 `tenant_data_key_unconfigured` without `TENANT_DATA_KEYS`.
use crate::common;

use std::collections::HashMap;

use agent_api::connectors::custody::{destroy_tenant, open_credential, seal_credential};
use agent_api::crypto::keyring::Keyring;
use agent_api::tenancy::begin_scoped;
use axum::http::StatusCode;
use base64::Engine as _;
use common::{TestEnv, insert_tenant};
use serde_json::{Map, Value, json};
use uuid::Uuid;

/// `TENANT_DATA_KEYS` with one kid `k1` (key bytes built at runtime: no key literal is committed).
fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
    vars.insert(
        "TENANT_DATA_KEYS".to_string(),
        format!(r#"{{"k1":"{key}"}}"#),
    );
    vars.insert("TENANT_DATA_KEY_CURRENT".to_string(), "k1".to_string());
}

async fn env_with_keys() -> TestEnv {
    common::env_with(with_keys).await
}

fn keyring(e: &TestEnv) -> Option<&Keyring> {
    e.state.tenant_keyring.as_deref()
}

fn fields(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        other => panic!("not an object: {other}"),
    }
}

/// A tenant with connector instances `ids` on a fixture platform template `t8_sys@1`.
async fn tenant(e: &TestEnv, ids: &[&str]) -> Uuid {
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, &format!("Custody {t}")).await;
    sqlx::query(
        "INSERT INTO connector_template (key, version, label, vendor, system_class, wave, transport, auth_kind, status) \
         VALUES ('t8_sys', '1', 'Sys', 'V', 'commerce', 'v1', 'rest', 'api_key_header', 'active') \
         ON CONFLICT DO NOTHING",
    )
    .execute(&e.owner)
    .await
    .unwrap();
    for id in ids {
        sqlx::query(
            "INSERT INTO connector_instance (tenant_id, id, label, template_scope, template_key, template_version, \
                                             env, created_by) \
             VALUES ($1, $2, 'Shop', 'platform', 't8_sys', '1', 'prod', 'u-admin')",
        )
        .bind(t)
        .bind(*id)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    t
}

async fn seal(
    e: &TestEnv,
    t: Uuid,
    instance: &str,
    secret: Value,
) -> agent_api::connectors::custody::SealedCredential {
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    let sealed = seal_credential(&mut tx, keyring(e), t, instance, &fields(secret), "u-admin")
        .await
        .expect("seal");
    tx.commit().await.unwrap();
    sealed
}

async fn open(
    e: &TestEnv,
    t: Uuid,
    instance: &str,
) -> agent_api::error::ApiResult<Option<Map<String, Value>>> {
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    let r = open_credential(&mut tx, keyring(e), t, instance).await;
    tx.rollback().await.unwrap();
    r
}

#[tokio::test]
async fn seal_open_round_trip() {
    let e = env_with_keys().await;
    let t = tenant(&e, &["shop"]).await;
    assert_eq!(
        open(&e, t, "shop").await.unwrap(),
        None,
        "no credential yet"
    );
    let secret = json!({"api_key": "sk_live_abc123", "shop_domain_token": "tok"});
    let first = seal(&e, t, "shop", secret.clone()).await;
    assert_eq!(first.credential_version, 1);
    assert_eq!(first.dek_version, 1);
    assert_eq!(first.nonce.len(), 12);
    assert_eq!(first.fingerprint.len(), 8);
    assert!(first.fingerprint.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(
        !String::from_utf8_lossy(&first.ciphertext).contains("sk_live_abc123"),
        "the secret is not stored in clear"
    );
    assert_eq!(
        open(&e, t, "shop").await.unwrap(),
        Some(fields(secret.clone()))
    );
    let second = seal(&e, t, "shop", json!({"api_key": "sk_live_new"})).await;
    assert_eq!(second.credential_version, 2, "every seal bumps the version");
    assert_eq!(
        open(&e, t, "shop").await.unwrap(),
        Some(fields(json!({"api_key": "sk_live_new"})))
    );
    // the stored row and the wrapped DEK
    let (version, fingerprint, by): (i32, String, String) = sqlx::query_as(
        "SELECT credential_version, fingerprint::text, updated_by FROM connector_credential \
          WHERE tenant_id = $1 AND instance_id = 'shop'",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        (version, fingerprint.as_str(), by.as_str()),
        (2, second.fingerprint.as_str(), "u-admin")
    );
    let (wrapped, kek): (Vec<u8>, String) =
        sqlx::query_as("SELECT wrapped_dek, kek_id FROM tenant_data_key WHERE tenant_id = $1")
            .bind(t)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert!(
        wrapped.starts_with(b"v2:k1:"),
        "the DEK is stored wrapped by the keyring"
    );
    assert_eq!(kek, "k1");
}

#[tokio::test]
async fn sealing_needs_the_tenants_instance() {
    let e = env_with_keys().await;
    // another tenant has an instance `shop`; b does not
    tenant(&e, &["shop"]).await;
    let b = tenant(&e, &[]).await;
    let mut tx = begin_scoped(&e.state.db, b).await.unwrap();
    let err = seal_credential(
        &mut tx,
        keyring(&e),
        b,
        "shop",
        &fields(json!({"k": "v"})),
        "u",
    )
    .await
    .expect_err("not b's instance");
    assert_eq!(err.code(), "connector_instance_not_found");
}

#[tokio::test]
async fn ciphertext_moved_to_another_instance_or_tenant_does_not_open() {
    let e = env_with_keys().await;
    let a = tenant(&e, &["shop", "shop2"]).await;
    let b = tenant(&e, &["shop"]).await;
    seal(&e, a, "shop", json!({"api_key": "a-secret"})).await;
    // b has its own DEK and credential
    seal(&e, b, "shop", json!({"api_key": "b-secret"})).await;
    // copy a/shop onto a/shop2 (same tenant, same DEK, other instance)
    sqlx::query(
        "INSERT INTO connector_credential (tenant_id, instance_id, ciphertext, nonce, credential_version, dek_version, \
                                           fingerprint, updated_by) \
         SELECT tenant_id, 'shop2', ciphertext, nonce, credential_version, dek_version, fingerprint, updated_by \
           FROM connector_credential WHERE tenant_id = $1 AND instance_id = 'shop'",
    )
    .bind(a)
    .execute(&e.owner)
    .await
    .unwrap();
    let err = open(&e, a, "shop2")
        .await
        .expect_err("moved to another instance");
    assert_eq!(err.code(), "credential_unreadable");
    // copy a/shop over b/shop (other tenant, other DEK)
    sqlx::query(
        "UPDATE connector_credential d SET ciphertext = s.ciphertext, nonce = s.nonce, \
                credential_version = s.credential_version, dek_version = s.dek_version \
           FROM connector_credential s \
          WHERE s.tenant_id = $1 AND s.instance_id = 'shop' AND d.tenant_id = $2 AND d.instance_id = 'shop'",
    )
    .bind(a)
    .bind(b)
    .execute(&e.owner)
    .await
    .unwrap();
    let err = open(&e, b, "shop")
        .await
        .expect_err("moved to another tenant");
    assert_eq!(err.code(), "credential_unreadable");
    // replayed at another version
    sqlx::query(
        "UPDATE connector_credential SET credential_version = credential_version + 1 \
          WHERE tenant_id = $1 AND instance_id = 'shop'",
    )
    .bind(a)
    .execute(&e.owner)
    .await
    .unwrap();
    let err = open(&e, a, "shop").await.expect_err("another version");
    assert_eq!(err.code(), "credential_unreadable");
}

#[tokio::test]
async fn dek_destroyed_makes_credentials_unreadable() {
    let e = env_with_keys().await;
    let t = tenant(&e, &["shop"]).await;
    seal(&e, t, "shop", json!({"api_key": "s"})).await;
    assert!(open(&e, t, "shop").await.unwrap().is_some());
    for _ in 0..2 {
        // idempotent
        let mut tx = e.state.db.begin().await.unwrap();
        destroy_tenant(&mut tx, t).await.expect("destroy");
        let role: String = sqlx::query_scalar("SELECT current_user::text")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(role, "agent_app", "the caller's role is restored");
        tx.commit().await.unwrap();
    }
    let (wrapped, destroyed): (Option<Vec<u8>>, bool) = sqlx::query_as(
        "SELECT wrapped_dek, destroyed_at IS NOT NULL FROM tenant_data_key WHERE tenant_id = $1",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(wrapped.is_none() && destroyed, "the wrapped key is wiped");
    let err = open(&e, t, "shop").await.expect_err("shredded");
    assert_eq!(err.code(), "tenant_data_key_destroyed");
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    let err = seal_credential(
        &mut tx,
        keyring(&e),
        t,
        "shop",
        &fields(json!({"api_key": "x"})),
        "u",
    )
    .await
    .expect_err("no new key after destroy");
    assert_eq!(err.code(), "tenant_data_key_destroyed");
}

#[tokio::test]
async fn fingerprint_stable_for_the_same_secret_and_changes_on_change() {
    let e = env_with_keys().await;
    let t = tenant(&e, &["shop", "shop2"]).await;
    let a = seal(&e, t, "shop", json!({"api_key": "s1", "user": "u"})).await;
    let b = seal(&e, t, "shop", json!({"user": "u", "api_key": "s1"})).await;
    assert_eq!(a.fingerprint, b.fingerprint, "same secret, any key order");
    assert_ne!(a.ciphertext, b.ciphertext, "a fresh nonce per seal");
    let other_instance = seal(&e, t, "shop2", json!({"api_key": "s1", "user": "u"})).await;
    assert_eq!(
        a.fingerprint, other_instance.fingerprint,
        "the fingerprint is per tenant DEK"
    );
    let changed = seal(&e, t, "shop", json!({"api_key": "s2", "user": "u"})).await;
    assert_ne!(a.fingerprint, changed.fingerprint);
    // another tenant's DEK fingerprints the same secret differently
    let u = tenant(&e, &["shop"]).await;
    let elsewhere = seal(&e, u, "shop", json!({"api_key": "s1", "user": "u"})).await;
    assert_ne!(a.fingerprint, elsewhere.fingerprint);
}

#[tokio::test]
async fn keyring_none_is_503_tenant_data_key_unconfigured() {
    let e = common::env().await;
    assert!(
        e.state.tenant_keyring.is_none(),
        "TENANT_DATA_KEYS is optional"
    );
    let t = tenant(&e, &["shop"]).await;
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    let err = seal_credential(&mut tx, None, t, "shop", &fields(json!({"k": "v"})), "u")
        .await
        .expect_err("unconfigured");
    assert_eq!(
        err.status_and_code(),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "tenant_data_key_unconfigured"
        )
    );
    let err = open_credential(&mut tx, None, t, "shop")
        .await
        .expect_err("unconfigured");
    assert_eq!(
        err.status_and_code(),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "tenant_data_key_unconfigured"
        )
    );
}
