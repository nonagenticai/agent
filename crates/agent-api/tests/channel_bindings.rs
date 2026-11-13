//! B12 channel bindings (crate::conversations::bindings): CRUD under `integrations_manager`, the generated web chat
//! key, the BYO credential sealed under the tenant data key and never returned, settings checks (secret-shaped
//! fields), and the audit rows. Real Postgres 18.
use crate::common;

use axum::http::StatusCode;
use common::conv::{self, code};
use serde_json::{Value, json};

const BASE: &str = "/api/jobs/j1/channel-bindings";
/// A credential value (not provider-shaped, so the secret scanner stays quiet).
const TOKEN: &str = "123456789:test-not-a-real-bot-token-000000";
const WEBHOOK: &str = "test-webhook-secret-0001";

#[tokio::test]
async fn crud_with_a_sealed_credential_that_is_never_returned() {
    let c = conv::setup().await;
    let (s, w) = c
        .call(
            "POST",
            BASE,
            "u-int",
            json!({"kind": "web_chat", "side": "customer", "label": "Site",
               "settings": {"allowed_origins": ["https://shop.example"]}}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{w}");
    let key = w["external_id"].as_str().unwrap();
    assert!(key.starts_with("wk_") && key.len() == 27, "{key}");
    assert_eq!(w["has_credential"], false);
    assert_eq!(w["verified"], true, "our own key");
    let (s, b) = c
        .call(
            "POST",
            BASE,
            "u-int",
            json!({"kind": "telegram", "side": "customer",
               "label": "Bot", "settings": {}, "credential": {"bot_token": TOKEN, "webhook_secret": WEBHOOK}}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["has_credential"], true);
    assert_eq!(
        b["verified"], true,
        "holding the bot token proves the account"
    );
    assert_eq!(b["credential_fpr"].as_str().unwrap().len(), 8);
    assert!(!b.to_string().contains(TOKEN), "{b}");
    assert!(b.get("credential").is_none());
    let id = b["id"].as_str().unwrap().to_string();
    let enc: Vec<u8> =
        sqlx::query_scalar("SELECT secret_enc FROM connector_bindings WHERE kind = 'telegram'")
            .fetch_one(&c.e.owner)
            .await
            .unwrap();
    assert!(
        !String::from_utf8_lossy(&enc).contains(TOKEN),
        "sealed at rest"
    );
    let (s, l) = c.get(BASE, "u-int").await;
    assert_eq!(s, StatusCode::OK, "{l}");
    assert_eq!(
        l["items"].as_array().unwrap().len(),
        3,
        "the fixture's wk_test + 2"
    );
    assert!(!l.to_string().contains(TOKEN));
    // same account twice
    let (s, b) = c
        .call(
            "POST",
            BASE,
            "u-int",
            json!({"kind": "telegram", "side": "customer", "label": "Again",
               "credential": {"bot_token": TOKEN, "webhook_secret": WEBHOOK}}),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "binding_unavailable"));
    // update: label and disable
    let (s, b) = c
        .call(
            "PUT",
            &format!("{BASE}/{id}"),
            "u-int",
            json!({"label": "Bot EU", "enabled": false}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (
            b["label"].as_str(),
            b["enabled"].as_bool(),
            b["has_credential"].as_bool()
        ),
        (Some("Bot EU"), Some(false), Some(true))
    );
    let (s, _) = c
        .call("DELETE", &format!("{BASE}/{id}"), "u-int", json!({}))
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, b) = c
        .call("DELETE", &format!("{BASE}/{id}"), "u-int", json!({}))
        .await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "binding_not_found"));
    let audit: Vec<(String, Value)> = sqlx::query_as(
        "SELECT action, details FROM audit_event WHERE action LIKE 'channel_binding.%' ORDER BY id",
    )
    .fetch_all(&c.e.owner)
    .await
    .unwrap();
    let actions: Vec<&str> = audit.iter().map(|(a, _)| a.as_str()).collect();
    assert_eq!(
        actions,
        [
            "channel_binding.create",
            "channel_binding.create",
            "channel_binding.update",
            "channel_binding.delete",
        ]
    );
    for (_, d) in &audit {
        assert!(!d.to_string().contains(TOKEN), "{d}");
    }
}

#[tokio::test]
async fn settings_and_kinds_are_checked() {
    let c = conv::setup().await;
    let cr = &c;
    let post = |b: Value| async move { cr.call("POST", BASE, "u-int", b).await };
    let (s, b) = post(
        json!({"kind": "helpdesk", "side": "worker", "external_id": "hd-1", "label": "H",
        "settings": {"base_url": "https://helpdesk.example", "token": "abc"}}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "secret_in_settings")
    );
    let (s, b) = post(json!({"kind": "helpdesk", "side": "worker", "external_id": "hd-1", "label": "H", "settings": {}})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_settings")
    );
    let (s, b) =
        post(json!({"kind": "sms", "side": "customer", "external_id": "x", "label": "S"})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_kind")
    );
    let (s, b) = post(json!({"kind": "web_chat", "side": "worker", "label": "S"})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_side")
    );
    let (s, b) = post(json!({"kind": "helpdesk", "side": "worker", "label": "S",
        "settings": {"base_url": "https://helpdesk.example", "service_user": "bot@helpdesk.example"},
        "credential": {"api_key": "key-test", "api_secret": "secret-test", "webhook_secret": "test-frappe-webhook-secret"}})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "external_id_required")
    );
    let (s, b) = post(
        json!({"kind": "helpdesk", "side": "worker", "external_id": "hd-1", "label": " ", "settings": {"base_url": "https://helpdesk.example", "service_user": "bot@helpdesk.example"},
        "credential": {"api_key": "key-test", "api_secret": "secret-test", "webhook_secret": "test-frappe-webhook-secret"}}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "label_required")
    );
}

#[tokio::test]
async fn bindings_need_integrations_manager() {
    let c = conv::setup().await;
    let body = json!({"kind": "web_chat", "side": "customer", "label": "Site"});
    for sub in ["u-agent", "u-obs", "u-mgr"] {
        let (s, b) = c.call("POST", BASE, sub, body.clone()).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{sub}: {b}");
        assert_eq!(b["error"]["missing_role"], "integrations_manager");
        let (s, _) = c.get(BASE, sub).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{sub} reads");
    }
    let (s, _) = c.call("POST", BASE, "u-admin", body).await;
    assert_eq!(s, StatusCode::CREATED);
}

/// D126: a member holding only `tenant_admin` (no `integrations_manager`) configures both sides of a job; an observer
/// is refused with `integrations_manager`, reads included.
#[tokio::test]
async fn a_tenant_admin_without_integrations_manager_configures_both_sides() {
    let c = conv::setup().await;
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, roles, display_name, updated_by) \
         VALUES ('u-ta', $1, ARRAY['tenant_admin'], 'u-ta', 'test')",
    )
    .bind(c.t)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (s, tg) = c
        .call(
            "POST",
            BASE,
            "u-ta",
            json!({"kind": "telegram", "side": "customer", "label": "Bot",
               "settings": {"allowed_user_ids": [42]},
               "credential": {"bot_token": "123456789:test-not-a-real-bot-token-000000",
                              "webhook_secret": "test-webhook-secret-0001"}}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{tg}");
    let tg_id = tg["id"].as_str().unwrap().to_string();
    let (s, hd) = c
        .call(
            "POST",
            BASE,
            "u-ta",
            json!({"kind": "helpdesk", "side": "worker", "external_id": "helpdesk.test#support",
               "label": "Helpdesk",
               "settings": {"base_url": "https://helpdesk.example", "service_user": "bot@helpdesk.example"},
               "credential": {"api_key": "key-test", "api_secret": "secret-test",
                              "webhook_secret": "test-frappe-webhook-secret"}}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{hd}");
    let hd_id = hd["id"].as_str().unwrap().to_string();
    let (s, l) = c.get(BASE, "u-ta").await;
    assert_eq!(s, StatusCode::OK, "{l}");
    for (id, label) in [(&tg_id, "Bot 2"), (&hd_id, "Helpdesk 2")] {
        let uri = format!("{BASE}/{id}");
        let (s, b) = c.call("PUT", &uri, "u-ta", json!({"label": label})).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["label"], label);
        let (s, b) = c.call("PUT", &uri, "u-ta", json!({"enabled": false})).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["enabled"], false);
    }
    for id in [&tg_id, &hd_id] {
        let (s, b) = c
            .call("DELETE", &format!("{BASE}/{id}"), "u-ta", json!({}))
            .await;
        assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    }
    // an observer is refused every route, with the same role named
    let uri = format!("{BASE}/{tg_id}");
    for (m, path) in [
        ("POST", BASE.to_string()),
        ("PUT", uri.clone()),
        ("DELETE", uri.clone()),
        ("POST", format!("{uri}/rotate-key")),
        ("POST", format!("{uri}/identity-secret")),
        ("POST", format!("{uri}/test-message")),
        ("POST", format!("{uri}/test-session")),
    ] {
        let (s, b) = c.call(m, &path, "u-obs", json!({})).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{m} {path}: {b}");
        assert_eq!(b["error"]["missing_role"], "integrations_manager");
    }
    let (s, _) = c.get(BASE, "u-obs").await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_credential_without_tenant_data_keys_is_503() {
    let c = conv::setup_with(|v| {
        v.remove("TENANT_DATA_KEYS");
        v.remove("TENANT_DATA_KEY_CURRENT");
    })
    .await;
    let (s, b) = c
        .call(
            "POST",
            BASE,
            "u-int",
            json!({"kind": "telegram", "side": "customer",
               "label": "S", "credential": {"bot_token": TOKEN, "webhook_secret": WEBHOOK}}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "tenant_data_key_unconfigured"
        )
    );
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM connector_bindings WHERE kind = 'telegram'")
            .fetch_one(&c.e.owner)
            .await
            .unwrap();
    assert_eq!(n, 0, "nothing stored");
}

#[tokio::test]
async fn another_tenants_account_is_a_generic_409() {
    let c = conv::setup().await;
    let t2 = uuid::Uuid::new_v4();
    common::insert_tenant(&c.e.owner, t2, "Beta").await;
    common::insert_user(&c.e.owner, "u-beta", t2, "integrations_manager").await;
    sqlx::query("INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'b1', 'B1', '[]', 'test')")
        .bind(t2)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let body = json!({"kind": "telegram", "side": "customer", "label": "S",
        "credential": {"bot_token": TOKEN, "webhook_secret": WEBHOOK}});
    let (s, _) = c.call("POST", BASE, "u-int", body.clone()).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, b) = c
        .call("POST", "/api/jobs/b1/channel-bindings", "u-beta", body)
        .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "binding_unavailable"));
    assert!(
        !b.to_string().contains("Alpha"),
        "the owner is not revealed: {b}"
    );
}

/// 0130 widened `connector_bindings.kind`: a Telegram customer binding (BYO bot, proven by its token) and a Frappe
/// Helpdesk worker binding are stored, verified on creation and served to the edge; the other sides are refused.
#[tokio::test]
async fn telegram_customer_and_helpdesk_worker_bindings_are_stored() {
    let c = conv::setup().await;
    let (s, tg) = c
        .call(
            "POST",
            BASE,
            "u-int",
            json!({"kind": "telegram", "side": "customer", "label": "Test bot",
               "settings": {"allowed_user_ids": [42]},
               "credential": {"bot_token": "123456789:test-not-a-real-bot-token-000000",
                              "webhook_secret": "test-webhook-secret-0001"}}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{tg}");
    assert_eq!(tg["external_id"], "123456789", "the token's bot id");
    assert_eq!(tg["verified"], true, "{tg}");
    assert_eq!(tg["has_credential"], true);
    let (s, hd) = c
        .call(
            "POST",
            BASE,
            "u-int",
            json!({"kind": "helpdesk", "side": "worker", "external_id": "helpdesk.test#support",
               "label": "Helpdesk",
               "settings": {"base_url": "https://helpdesk.example", "service_user": "bot@helpdesk.example"},
               "credential": {"api_key": "key-test", "api_secret": "secret-test",
                              "webhook_secret": "test-frappe-webhook-secret"}}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{hd}");
    assert_eq!(hd["verified"], true, "{hd}");
    let kinds: Vec<(String, String)> = sqlx::query_as(
        "SELECT kind, side FROM connector_bindings WHERE kind IN ('telegram', 'helpdesk') ORDER BY kind",
    )
    .fetch_all(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(
        kinds,
        [
            ("helpdesk".to_string(), "worker".to_string()),
            ("telegram".to_string(), "customer".to_string())
        ]
    );
    let (s, l) = c.internal_get("/api/internal/channel/bindings").await;
    assert_eq!(s, StatusCode::OK, "{l}");
    let served = |kind: &str, side: &str| {
        l["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["kind"] == kind && x["side"] == side)
    };
    assert!(
        served("telegram", "customer") && served("helpdesk", "worker"),
        "{l}"
    );
    for (kind, side) in [("telegram", "worker"), ("helpdesk", "customer")] {
        let (s, b) = c
            .call(
                "POST",
                BASE,
                "u-int",
                json!({"kind": kind, "side": side, "external_id": "x-1", "label": "L"}),
            )
            .await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_side"),
            "{kind}/{side}"
        );
    }
}
