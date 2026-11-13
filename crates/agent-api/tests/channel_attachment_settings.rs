//! Channel settings the tenant admin owns (owner 2026-10-28): a binding's origins and Telegram lists stay editable after
//! create (PUT), and photos and PDFs are a per-binding admin setting (`settings.attachments {customer, staff}`) that
//! every attachment door honours, with the tenant's storage quota set by a platform super_admin and read, with its usage,
//! by the tenant admin. Real Postgres 18.
use crate::common;

use std::io::Cursor;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::conv::{self, Ctx, EDGE_KEY, code};
use common::{json, send, super_admin};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const BASE: &str = "/api/jobs/j1/channel-bindings";
const B: &str = "SW9SETTINGS";

fn png() -> Vec<u8> {
    let img = image::RgbImage::from_pixel(8, 6, image::Rgb([200, 30, 30]));
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn multipart(parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
    let mut b = Vec::new();
    for (name, file, bytes) in parts {
        let disp = match file {
            Some(f) => format!("form-data; name=\"{name}\"; filename=\"{f}\""),
            None => format!("form-data; name=\"{name}\""),
        };
        b.extend_from_slice(format!("--{B}\r\nContent-Disposition: {disp}\r\n\r\n").as_bytes());
        b.extend_from_slice(bytes);
        b.extend_from_slice(b"\r\n");
    }
    b.extend_from_slice(format!("--{B}--\r\n").as_bytes());
    b
}

async fn raw(c: &Ctx, req: Request<Body>) -> (StatusCode, Value) {
    let r = c.e.app.clone().oneshot(req).await.unwrap();
    let s = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    (s, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Set the web chat binding's settings the way the console does: PUT as the integrations manager.
async fn put_settings(c: &Ctx, id: Uuid, settings: Value) -> (StatusCode, Value) {
    c.call(
        "PUT",
        &format!("{BASE}/{id}"),
        "u-int",
        json!({"settings": settings}),
    )
    .await
}

/// A web chat conversation taken by `u-agent`: its id.
async fn taken(c: &Ctx, ext: &str) -> (String, String) {
    let (s, b) = c.inbound(ext, "m0", "I want to talk to a human").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let id = b["conversation_id"].as_str().unwrap().to_string();
    let token = c.sessions.lock().unwrap().get(ext).unwrap().1.clone();
    let (s, b) = c.take(&id, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    (id, token)
}

async fn customer_upload(c: &Ctx, conv: &str, token: &str) -> (StatusCode, Value) {
    let fields = [
        ("tenant", c.t.to_string()),
        ("job_id", c.job.to_string()),
        ("binding_id", c.wc.to_string()),
        ("conversation", conv.to_string()),
        ("session_token", token.to_string()),
    ];
    let png = png();
    let mut parts: Vec<(&str, Option<&str>, &[u8])> = fields
        .iter()
        .map(|(n, v)| (*n, None, v.as_bytes()))
        .collect();
    parts.push(("file", Some("a.png"), &png));
    let req = Request::post("/api/internal/channel/webchat/attachments")
        .header("authorization", format!("Bearer {EDGE_KEY}"))
        .header("content-type", format!("multipart/form-data; boundary={B}"))
        .body(Body::from(multipart(&parts)))
        .unwrap();
    raw(c, req).await
}

async fn staff_upload(c: &Ctx, conv: &str) -> (StatusCode, Value) {
    let png = png();
    let req = Request::post(format!("/api/jobs/j1/conversations/{conv}/attachments"))
        .header(
            "authorization",
            format!("Bearer {}", common::person("u-agent", &[])),
        )
        .header("content-type", format!("multipart/form-data; boundary={B}"))
        .body(Body::from(multipart(&[("file", Some("a.png"), &png)])))
        .unwrap();
    raw(c, req).await
}

async fn staff_reply(c: &Ctx, conv: &str) -> (StatusCode, Value) {
    c.call(
        "POST",
        &format!("/api/jobs/j1/conversations/{conv}/messages"),
        "u-agent",
        json!({"kind": "reply", "text": "Hi", "attachments": [Uuid::new_v4()]}),
    )
    .await
}

// ---- Task 2: origins and Telegram lists are editable after create ------------------------------------------------

#[tokio::test]
async fn origins_and_telegram_lists_are_editable_after_create() {
    let c = conv::setup().await;
    let (s, wc) = c
        .call(
            "POST",
            BASE,
            "u-int",
            json!({"kind": "web_chat", "side": "customer", "label": "Site",
               "settings": {"allowed_origins": ["https://shop.example"], "csat_enabled": false}}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{wc}");
    let id = wc["id"].as_str().unwrap().parse().unwrap();
    let (s, b) = put_settings(
        &c,
        id,
        json!({"allowed_origins": ["https://shop.example", "https://www.shop.example"], "csat_enabled": false}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["settings"]["allowed_origins"],
        json!(["https://shop.example", "https://www.shop.example"])
    );
    // a list that is not https origins is refused and nothing changes
    let (s, b) = put_settings(&c, id, json!({"allowed_origins": ["http://shop.example"]})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_settings")
    );
    let (_, l) = c.get(BASE, "u-int").await;
    let row = l["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == wc["id"])
        .unwrap();
    assert_eq!(
        row["settings"]["allowed_origins"].as_array().unwrap().len(),
        2
    );

    let (s, tg) = c
        .call(
            "POST",
            BASE,
            "u-int",
            json!({"kind": "telegram", "side": "customer", "label": "Bot",
               "settings": {"allowed_user_ids": [42]},
               "credential": {"bot_token": "123456789:test-not-a-real-bot-token-000000",
                              "webhook_secret": "test-webhook-secret-0001"}}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{tg}");
    let tid = tg["id"].as_str().unwrap();
    let (s, b) = c
        .call(
            "PUT",
            &format!("{BASE}/{tid}"),
            "u-int",
            json!({"settings": {"allowed_user_ids": [42, 43], "test_chat_id": -1001234}}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["settings"]["allowed_user_ids"], json!([42, 43]));
    assert_eq!(b["settings"]["test_chat_id"], -1001234);
    let (s, b) = c
        .call(
            "PUT",
            &format!("{BASE}/{tid}"),
            "u-int",
            json!({"settings": {"allowed_user_ids": ["42"]}}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_settings")
    );
}

// ---- Task 3: attachments are a binding setting -----------------------------------------------------------------------

#[tokio::test]
async fn the_attachments_setting_is_validated_per_kind() {
    let c = conv::setup().await;
    let (s, b) = put_settings(
        &c,
        c.wc,
        json!({"attachments": {"customer": true, "staff": false}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["settings"]["attachments"],
        json!({"customer": true, "staff": false})
    );
    for bad in [
        json!({"attachments": true}),
        json!({"attachments": {"customer": "yes"}}),
        json!({"attachments": {"anyone": true}}),
    ] {
        let (s, b) = put_settings(&c, c.wc, bad.clone()).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_settings"),
            "{bad}"
        );
    }
    // a helpdesk binding can say off, never on
    let post = |mut settings: Value| {
        settings["base_url"] = json!("https://helpdesk.example");
        settings["service_user"] = json!("bot@helpdesk.example");
        c.call(
            "POST",
            BASE,
            "u-int",
            json!({"kind": "helpdesk", "side": "worker", "external_id": "helpdesk.test#support",
               "label": "Helpdesk", "settings": settings,
               "credential": {"api_key": "key-test", "api_secret": "secret-test",
                              "webhook_secret": "wh-secret-test-0123456789"}}),
        )
    };
    let (s, b) = post(json!({"attachments": {"customer": true}})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_settings"),
        "{b}"
    );
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("web chat or a Telegram"),
        "{b}"
    );
    let (s, b) = post(json!({"attachments": {"customer": false, "staff": false}})).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
}

#[tokio::test]
async fn staff_files_follow_the_binding_setting_and_the_payload_says_so() {
    let c = conv::setup().await;
    let (conv_id, _) = taken(&c, "wc-set-1").await;
    // default: on (the gate passes; the made-up file id is the 404)
    let (s, b) = c
        .get(&format!("/api/jobs/j1/conversations/{conv_id}"), "u-agent")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["conversation"]["attachments"],
        json!({"customer": true, "staff": true})
    );
    let (s, b) = staff_reply(&c, &conv_id).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");

    let (s, b) = put_settings(&c, c.wc, json!({"attachments": {"staff": false}})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = staff_reply(&c, &conv_id).await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "attachments_not_supported"
        ),
        "{b}"
    );
    let (s, b) = staff_upload(&c, &conv_id).await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "attachments_not_supported"
        ),
        "{b}"
    );
    let (_, b) = c
        .get(&format!("/api/jobs/j1/conversations/{conv_id}"), "u-agent")
        .await;
    assert_eq!(
        b["conversation"]["attachments"],
        json!({"customer": true, "staff": false}),
        "the composer's paperclip follows this"
    );

    // staff on again, customers off: staff may send, the customer's own door is closed
    let (s, _) = put_settings(&c, c.wc, json!({"attachments": {"customer": false}})).await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = staff_upload(&c, &conv_id).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
}

#[tokio::test]
async fn customer_files_follow_the_binding_setting() {
    let c = conv::setup().await;
    let (conv_id, token) = taken(&c, "wc-set-2").await;
    let (s, b) = customer_upload(&c, &conv_id, &token).await;
    assert_eq!(s, StatusCode::CREATED, "default on: {b}");

    let (s, _) = put_settings(&c, c.wc, json!({"attachments": {"customer": false}})).await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = customer_upload(&c, &conv_id, &token).await;
    assert_eq!(
        (s, code(&b), b["error"]["reason"].as_str()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "attachment_refused",
            Some("attachments_off")
        ),
        "{b}"
    );
    let (s, _) = put_settings(&c, c.wc, json!({"attachments": {"customer": true}})).await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = customer_upload(&c, &conv_id, &token).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
}

#[tokio::test]
async fn a_telegram_upload_is_refused_with_attachments_off_when_the_binding_says_so() {
    let c = conv::setup().await;
    let (s, tg) = c
        .call(
            "POST",
            BASE,
            "u-int",
            json!({"kind": "telegram", "side": "customer", "label": "Bot",
               "settings": {"allowed_user_ids": [42], "attachments": {"customer": false}},
               "credential": {"bot_token": "123456789:test-not-a-real-bot-token-000000",
                              "webhook_secret": "test-webhook-secret-0001"}}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{tg}");
    // D130: an API-created customer binding starts in the test env; this upload arrives on the live path
    c.go_live().await;
    let meta = json!({"binding_id": tg["id"], "kind": "telegram", "tenant": c.t, "job_id": c.job,
        "conversation": {"channel": "telegram", "id": "555", "route": {}},
        "message_id": "9", "party_id": "42"})
    .to_string();
    let png = png();
    let req = Request::post("/api/internal/channel/attachments")
        .header("authorization", format!("Bearer {EDGE_KEY}"))
        .header("content-type", format!("multipart/form-data; boundary={B}"))
        .body(Body::from(multipart(&[
            ("meta", None, meta.as_bytes()),
            ("file", Some("photo.png"), &png),
        ])))
        .unwrap();
    let (s, b) = raw(&c, req).await;
    assert_eq!(
        (s, code(&b), b["error"]["reason"].as_str()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "attachment_refused",
            Some("attachments_off")
        ),
        "{b}"
    );
}

// ---- the tenant's quota ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_quota_is_set_by_a_super_admin_and_read_with_usage_by_the_tenant_admin() {
    let c = conv::setup().await;
    let path = "/api/jobs/j1/attachment-quota";
    let (s, b) = c.get(path, "u-int").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["used_bytes"], 0);
    assert_eq!(b["custom"], false);
    assert_eq!(b["quota_bytes"], b["default_quota_bytes"]);
    let (s, _) = c.get(path, "u-admin").await;
    assert_eq!(s, StatusCode::OK);
    for sub in ["u-agent", "u-obs"] {
        let (s, b) = c.get(path, sub).await;
        assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{sub}");
        assert_eq!(
            b["error"]["missing_role"], "integrations_manager",
            "{sub}: {b}"
        );
    }

    // the tenant's own roles cannot set it, a super_admin can, and null returns to the default
    let patch = |token: String, body: Value| {
        let app = c.e.app.clone();
        let uri = format!("/api/tenants/{}", c.t);
        async move { send(&app, json("PATCH", &uri, Some(&token), body)).await }
    };
    let (s, _) = patch(
        common::person("u-admin", &[]),
        json!({"attachment_quota_bytes": 5}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    for bad in [0, -1, 2_i64 << 40] {
        let (s, b) = patch(super_admin(), json!({"attachment_quota_bytes": bad})).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_quota"),
            "{bad}"
        );
    }
    let (s, b) = patch(super_admin(), json!({"attachment_quota_bytes": 10})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["attachment_quota_bytes"], 10);
    let (_, b) = c.get(path, "u-int").await;
    assert_eq!(
        (b["quota_bytes"].as_i64(), b["custom"].as_bool()),
        (Some(10), Some(true))
    );

    // an upload over the tenant's own quota is refused (the default would have taken it)
    let (conv_id, token) = taken(&c, "wc-set-4").await;
    let (s, b) = customer_upload(&c, &conv_id, &token).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::PAYLOAD_TOO_LARGE, "attachment_quota"),
        "{b}"
    );
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE tenant_id = $1 AND action = 'tenant.attachment_quota'",
    )
    .bind(c.t)
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(n, 1, "audited once");

    let (s, b) = patch(super_admin(), json!({"attachment_quota_bytes": null})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["attachment_quota_bytes"], Value::Null);
    let (s, b) = customer_upload(&c, &conv_id, &token).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (_, b) = c.get(path, "u-int").await;
    assert!(b["used_bytes"].as_i64().unwrap() > 0, "{b}");
}
