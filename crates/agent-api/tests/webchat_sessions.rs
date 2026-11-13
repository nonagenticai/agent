//! B13 web chat sessions + durable limits (crate::conversations::webchat_sessions, migration 0124): the key-gated
//! sessions route (token shown once, sha256 stored; one 401 for every unusable widget; exact origins; new sessions per
//! IP), the session check on `inbound` (401 `session_invalid`, 403 `session_blocked`, the 24 h slide capped at 7 days),
//! the fixed-window message limits (per session, per IP, per binding), the abuse blocks (3 refusals, 5 identical
//! texts), `rotateWebChatKey`, and counters that live in Postgres (a second app on the same database sees them).
//! Real Postgres 18 and the shared test Redis: D109 — an admitted message the agent answers is a B4 runtime turn, so
//! the fixture's job `j1` is live and served ([`setup`]; it binds no qualifier, so the runtime answers `TURN_FAILED`).
use crate::common;
use crate::redis_env;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_api::conversations::webchat_identity::{self, HsKey};
use axum::http::StatusCode;
use common::conv::{self, Ctx, EDGE_KEY, code};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const SESSIONS: &str = "/api/internal/channel/webchat/sessions";
const INBOUND: &str = "/api/internal/channel/inbound";
const PUBLIC: &str = "https://chat.nonagentic.ai";

/// D109: the B12 fixture on the runtime (job `j1` live, its prod scope served), under a fresh Redis prefix.
async fn setup() -> Ctx {
    conv::setup_runtime(redis_env::api_vars("webchat-sessions").await).await
}

/// A client IP hash (hex sha256).
fn ip(n: u32) -> String {
    hex::encode(Sha256::digest(format!("203.0.113.{n}").as_bytes()))
}

/// Fixed windows reset on their boundary: start a counting test clear of the end of a `window_s` window.
async fn clear_of_window_end(window_s: u64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let left = window_s as f64 - now % window_s as f64;
    if left < 15.0 {
        tokio::time::sleep(Duration::from_secs_f64(left + 0.5)).await;
    }
}

async fn open_with(
    c: &Ctx,
    binding: Uuid,
    key: &str,
    origin: &str,
    ip: &str,
) -> (StatusCode, Value) {
    c.internal(
        SESSIONS,
        json!({"binding_id": binding, "widget_key": key, "origin": origin, "ip_hash": ip}),
    )
    .await
}

async fn open(c: &Ctx, ip: &str) -> (StatusCode, Value) {
    open_with(c, c.wc, "wk_test", PUBLIC, ip).await
}

struct Sess {
    id: String,
    token: String,
    ip: String,
}

async fn session(c: &Ctx, ip: &str) -> Sess {
    let (s, b) = open(c, ip).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    Sess {
        id: b["session_id"].as_str().unwrap().to_string(),
        token: b["session_token"].as_str().unwrap().to_string(),
        ip: ip.to_string(),
    }
}

fn message(c: &Ctx, s: &Sess, key: &str, msg: &str, text: &str) -> Value {
    json!({
        "kind": "web_chat", "external_id": key, "binding_id": c.wc, "tenant": c.t, "job_id": c.job,
        "job_key": "j1", "conversation": {"channel": "web_chat", "id": format!("conv-{}", s.id), "route": {}},
        "message_id": msg, "party": {"id": format!("wc:{}", s.id), "verified": false}, "text": text,
        "session_token": s.token, "ip_hash": s.ip,
    })
}

async fn say(c: &Ctx, s: &Sess, msg: &str, text: &str) -> (StatusCode, Value) {
    c.internal(INBOUND, message(c, s, "wk_test", msg, text))
        .await
}

/// Merge `limits` into the `wk_test` binding's settings.
async fn set_limits(c: &Ctx, limits: Value) {
    sqlx::query(
        "UPDATE connector_bindings SET settings = settings || jsonb_build_object('limits', $2::jsonb) WHERE id = $1",
    )
    .bind(c.wc)
    .bind(limits)
    .execute(&c.e.owner)
    .await
    .unwrap();
}

fn retry_after(b: &Value) -> u64 {
    b["error"]["retry_after_s"].as_u64().unwrap_or(0)
}

async fn blocked_reason(c: &Ctx, s: &Sess) -> Option<String> {
    sqlx::query_scalar("SELECT blocked_reason FROM web_chat_session WHERE id = $1::uuid")
        .bind(&s.id)
        .fetch_one(&c.e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn session_create_returns_token_and_stores_only_hash() {
    let c = setup().await;
    let (s, b) = open(&c, &ip(1)).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let id = b["session_id"].as_str().unwrap();
    let token = b["session_token"].as_str().unwrap();
    let (head, secret) = token.split_once('.').unwrap();
    assert_eq!(head, id);
    assert_eq!(secret.len(), 43, "base64url of 32 bytes");
    assert!(
        b["expires_at"].as_str().is_some_and(|e| e.ends_with('Z')),
        "{b}"
    );
    assert_eq!(b["display"], json!({}));
    assert_eq!(b["csat_enabled"], true);
    let hashes = c
        .texts("SELECT encode(token_hash, 'hex') FROM web_chat_session")
        .await;
    assert_eq!(hashes, vec![hex::encode(Sha256::digest(token.as_bytes()))]);
    let rows = c
        .texts("SELECT row_to_json(s)::text FROM web_chat_session s")
        .await;
    assert!(!rows[0].contains(secret), "the token is never stored");
    let hours: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM expires_at - now())::float8 / 3600 FROM web_chat_session",
    )
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    assert!((23.9..=24.0).contains(&hours), "{hours}");
    // the binding's display goes to the widget
    sqlx::query(
        r##"UPDATE connector_bindings SET settings = '{"display": {"title": "Help", "colour": "#112233"}}' WHERE id = $1"##,
    )
    .bind(c.wc)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (s, b) = open(&c, &ip(1)).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["display"], json!({"title": "Help", "colour": "#112233"}));
    // the door is key-gated
    let (s, _) = common::send(
        &c.e.app,
        common::json(
            "POST",
            SESSIONS,
            Some("nope"),
            json!({"binding_id": c.wc, "widget_key": "wk_test", "origin": PUBLIC, "ip_hash": ip(1)}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, b) = open(&c, "1.2.3.4").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_ip_hash")
    );
}

#[tokio::test]
async fn unknown_disabled_rotated_widget_are_indistinguishable_401() {
    let c = setup().await;
    let mut seen = Vec::new();
    for (binding, key) in [(Uuid::new_v4(), "wk_test"), (c.wc, "wk_nope")] {
        seen.push(open_with(&c, binding, key, PUBLIC, &ip(1)).await);
    }
    for (break_it, mend) in [
        (
            "UPDATE connector_bindings SET enabled = false WHERE id = $1",
            "UPDATE connector_bindings SET enabled = true WHERE id = $1",
        ),
        (
            "UPDATE connector_bindings SET verified_at = NULL, verify_code = 'ABCDEF' WHERE id = $1",
            "UPDATE connector_bindings SET verified_at = now(), verify_code = NULL WHERE id = $1",
        ),
    ] {
        sqlx::query(break_it)
            .bind(c.wc)
            .execute(&c.e.owner)
            .await
            .unwrap();
        seen.push(open(&c, &ip(1)).await);
        sqlx::query(mend)
            .bind(c.wc)
            .execute(&c.e.owner)
            .await
            .unwrap();
    }
    let (s, b) = c
        .call(
            "POST",
            &format!("/api/jobs/j1/channel-bindings/{}/rotate-key", c.wc),
            "u-int",
            json!({}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let new_key = b["external_id"].as_str().unwrap().to_string();
    assert!(new_key.starts_with("wk_") && new_key != "wk_test", "{b}");
    seen.push(open(&c, &ip(1)).await);
    assert_eq!(seen.len(), 5);
    for (s, b) in &seen {
        assert_eq!((*s, code(b)), (StatusCode::UNAUTHORIZED, "widget_unknown"));
        assert_eq!(b, &seen[0].1, "one answer for every unusable widget");
    }
    let (s, b) = open_with(&c, c.wc, &new_key, PUBLIC, &ip(1)).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
}

#[tokio::test]
async fn origin_must_match_exactly() {
    let c = setup().await;
    sqlx::query(
        r#"UPDATE connector_bindings SET settings = '{"allowed_origins": ["https://shop.example"]}' WHERE id = $1"#,
    )
    .bind(c.wc)
    .execute(&c.e.owner)
    .await
    .unwrap();
    for (n, origin) in ["https://shop.example", PUBLIC].into_iter().enumerate() {
        let (s, b) = open_with(&c, c.wc, "wk_test", origin, &ip(n as u32)).await;
        assert_eq!(s, StatusCode::CREATED, "{origin}: {b}");
    }
    for origin in [
        "https://shop.example/",
        "https://SHOP.example",
        "https://shop.example.evil",
        "https://evil.shop.example",
        "http://shop.example",
        "https://shop.example:443",
        "null",
        "",
    ] {
        let (s, b) = open_with(&c, c.wc, "wk_test", origin, &ip(9)).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::FORBIDDEN, "origin_not_allowed"),
            "{origin}"
        );
    }
}

#[tokio::test]
async fn ip_session_limit_429_with_retry_after() {
    let c = setup().await;
    clear_of_window_end(3600).await;
    for _ in 0..10 {
        let (s, b) = open(&c, &ip(1)).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
    }
    let (s, b) = open(&c, &ip(1)).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::TOO_MANY_REQUESTS, "rate_limited")
    );
    assert!((1..=3600).contains(&retry_after(&b)), "{b}");
    // another IP is not affected; a binding may lower the ceiling
    let (s, _) = open(&c, &ip(2)).await;
    assert_eq!(s, StatusCode::CREATED);
    set_limits(&c, json!({"sessions_per_ip_hour": 1})).await;
    let (s, b) = open(&c, &ip(2)).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::TOO_MANY_REQUESTS, "rate_limited")
    );
}

#[tokio::test]
async fn session_message_limit_429() {
    let c = setup().await;
    set_limits(&c, json!({"msgs_per_session_hour": 2})).await;
    clear_of_window_end(3600).await;
    let s1 = session(&c, &ip(1)).await;
    for m in ["m1", "m2"] {
        let (s, b) = say(&c, &s1, m, &format!("question {m}")).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["status"], "answered", "{b}");
    }
    let (s, b) = say(&c, &s1, "m3", "question m3").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::TOO_MANY_REQUESTS, "rate_limited")
    );
    assert!((1..=3600).contains(&retry_after(&b)), "{b}");
    assert_eq!(
        c.backend.calls(),
        0,
        "the runtime answered, never the playground"
    );
    // another session of the same visitor IP has its own window
    let s2 = session(&c, &ip(1)).await;
    let (s, b) = say(&c, &s2, "m1", "question again").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // the per-IP window: 30 messages per 10 minutes by default, lowered here
    set_limits(
        &c,
        json!({"msgs_per_ip_10m": 2, "msgs_per_session_hour": 60}),
    )
    .await;
    clear_of_window_end(600).await;
    let s3 = session(&c, &ip(3)).await;
    let s4 = session(&c, &ip(3)).await;
    assert_eq!(say(&c, &s3, "m1", "one").await.0, StatusCode::OK);
    assert_eq!(say(&c, &s4, "m1", "two").await.0, StatusCode::OK);
    let (s, b) = say(&c, &s4, "m2", "three").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::TOO_MANY_REQUESTS, "rate_limited")
    );
}

#[tokio::test]
async fn binding_ceiling_applies() {
    let c = setup().await;
    set_limits(&c, json!({"msgs_per_minute": 3})).await;
    clear_of_window_end(60).await;
    let s1 = session(&c, &ip(1)).await;
    let s2 = session(&c, &ip(2)).await;
    assert_eq!(say(&c, &s1, "m1", "a").await.0, StatusCode::OK);
    assert_eq!(say(&c, &s1, "m2", "b").await.0, StatusCode::OK);
    assert_eq!(say(&c, &s2, "m1", "c").await.0, StatusCode::OK);
    let (s, b) = say(&c, &s2, "m2", "d").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::TOO_MANY_REQUESTS, "rate_limited")
    );
    assert!((1..=60).contains(&retry_after(&b)), "{b}");
}

#[tokio::test]
async fn three_refusals_block_session_403() {
    let c = setup().await;
    set_limits(&c, json!({"msgs_per_session_hour": 1})).await;
    clear_of_window_end(3600).await;
    let s1 = session(&c, &ip(1)).await;
    assert_eq!(say(&c, &s1, "m1", "hello").await.0, StatusCode::OK);
    for m in ["m2", "m3", "m4"] {
        let (s, b) = say(&c, &s1, m, &format!("more {m}")).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            "{m}"
        );
    }
    let (s, b) = say(&c, &s1, "m5", "still there?").await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "session_blocked"));
    assert_eq!(
        blocked_reason(&c, &s1).await.as_deref(),
        Some("rate_limited")
    );
    assert_eq!(
        c.texts("SELECT count(*)::text FROM channel_abuse_event WHERE kind = 'rate_limited'")
            .await,
        vec!["3"]
    );
    assert_eq!(
        c.texts("SELECT action FROM audit_event WHERE action = 'web_chat_session.block'")
            .await
            .len(),
        1
    );
    // the block is the session's, not the visitor IP's
    let s2 = session(&c, &ip(1)).await;
    assert_eq!(say(&c, &s2, "m1", "hello").await.0, StatusCode::OK);
}

#[tokio::test]
async fn repeated_text_blocks_session() {
    let c = setup().await;
    clear_of_window_end(120).await;
    let s1 = session(&c, &ip(1)).await;
    for (i, text) in [
        "Same question",
        "same question",
        "SAME QUESTION ",
        "Same question",
    ]
    .into_iter()
    .enumerate()
    {
        let (s, b) = say(&c, &s1, &format!("m{i}"), text).await;
        assert_eq!(s, StatusCode::OK, "{i}: {b}");
    }
    let (s, b) = say(&c, &s1, "m5", "same question").await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "session_blocked"));
    let (s, b) = say(&c, &s1, "m6", "something else").await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "session_blocked"));
    assert_eq!(
        blocked_reason(&c, &s1).await.as_deref(),
        Some("repeat_text")
    );
    assert_eq!(
        c.backend.calls(),
        0,
        "the runtime answered, never the playground"
    );
    let hashes = c
        .texts(
            "SELECT encode(text_hash, 'hex') FROM channel_abuse_event WHERE kind = 'repeat_text'",
        )
        .await;
    assert_eq!(
        hashes,
        vec![hex::encode(Sha256::digest(b"same question"))],
        "only the text's hash is kept"
    );
}

#[tokio::test]
async fn rotate_key_blocks_old_sessions() {
    let c = setup().await;
    let s1 = session(&c, &ip(1)).await;
    assert_eq!(say(&c, &s1, "m1", "hello").await.0, StatusCode::OK);
    let rotate = format!("/api/jobs/j1/channel-bindings/{}/rotate-key", c.wc);
    let (s, _) = c.call("POST", &rotate, "u-agent", json!({})).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "integrations_manager only");
    let (s, b) = c.call("POST", &rotate, "u-int", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let key = b["external_id"].as_str().unwrap().to_string();
    assert_eq!(
        b["snippet"],
        format!(
            r#"<script src="https://chat.nonagentic.ai/webchat/widget.js" data-widget="{key}" async></script>"#
        )
    );
    // the old session is blocked for good, whatever key the edge now sends
    let (s, b) = c
        .internal(INBOUND, message(&c, &s1, &key, "m2", "hello again"))
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "session_blocked"));
    assert_eq!(blocked_reason(&c, &s1).await.as_deref(), Some("rotated"));
    let (s, b) = say(&c, &s1, "m3", "old key").await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "binding_not_found"));
    let (s, b) = open(&c, &ip(1)).await;
    assert_eq!((s, code(&b)), (StatusCode::UNAUTHORIZED, "widget_unknown"));
    assert_eq!(
        c.texts("SELECT action FROM audit_event WHERE action = 'channel_binding.rotate_key'")
            .await
            .len(),
        1
    );
    // a new session on the new key works
    let (s, b) = open_with(&c, c.wc, &key, PUBLIC, &ip(1)).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    // only web chat has a key to rotate
    let other: Uuid = sqlx::query_scalar(
        "INSERT INTO connector_bindings (tenant_id, job_id, kind, side, external_id, label, created_by, verified_at) \
         VALUES ($1, $2, 'telegram', 'customer', 'T1', 'S', 'test', now()) RETURNING id",
    )
    .bind(c.t)
    .bind(c.job)
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    let (s, b) = c
        .call(
            "POST",
            &format!("/api/jobs/j1/channel-bindings/{other}/rotate-key"),
            "u-int",
            json!({}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "not_web_chat")
    );
    let (s, b) = c
        .call(
            "POST",
            &format!(
                "/api/jobs/j1/channel-bindings/{}/rotate-key",
                Uuid::new_v4()
            ),
            "u-int",
            json!({}),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "binding_not_found"));
}

#[tokio::test]
async fn session_slides_until_max() {
    let c = setup().await;
    let s1 = session(&c, &ip(1)).await;
    let left = |sql: &'static str| {
        let owner = c.e.owner.clone();
        let id = s1.id.clone();
        async move {
            let v: f64 = sqlx::query_scalar(sql)
                .bind(id)
                .fetch_one(&owner)
                .await
                .unwrap();
            v
        }
    };
    sqlx::query(
        "UPDATE web_chat_session SET expires_at = now() + interval '1 hour' WHERE id = $1::uuid",
    )
    .bind(&s1.id)
    .execute(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(say(&c, &s1, "m1", "hello").await.0, StatusCode::OK);
    let hours = left(
        "SELECT extract(epoch FROM expires_at - now())::float8 / 3600 FROM web_chat_session WHERE id = $1::uuid",
    )
    .await;
    assert!(hours > 23.9, "a message slides the session 24 h: {hours}");
    // near the 7-day cap, the slide stops at created + 7 days
    sqlx::query(
        "UPDATE web_chat_session SET created_at = now() - interval '6 days 23 hours', \
                expires_at = now() + interval '10 minutes' WHERE id = $1::uuid",
    )
    .bind(&s1.id)
    .execute(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(say(&c, &s1, "m2", "and now?").await.0, StatusCode::OK);
    let off = left(
        "SELECT abs(extract(epoch FROM expires_at - (created_at + interval '7 days')))::float8 \
           FROM web_chat_session WHERE id = $1::uuid",
    )
    .await;
    assert!(off < 1.0, "capped at created + 7 d: {off}");
    // past it, the session is gone
    sqlx::query(
        "UPDATE web_chat_session SET created_at = now() - interval '7 days 1 minute', \
                expires_at = now() - interval '1 minute' WHERE id = $1::uuid",
    )
    .bind(&s1.id)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (s, b) = say(&c, &s1, "m3", "hello?").await;
    assert_eq!((s, code(&b)), (StatusCode::UNAUTHORIZED, "session_invalid"));
}

#[tokio::test]
async fn inbound_web_chat_without_session_is_401() {
    let c = setup().await;
    let s1 = session(&c, &ip(1)).await;
    let good = message(&c, &s1, "wk_test", "m1", "hello");
    let mut bodies = Vec::new();
    // no session at all
    let mut b = good.clone();
    b.as_object_mut().unwrap().remove("session_token");
    b.as_object_mut().unwrap().remove("ip_hash");
    bodies.push(b);
    // a token, but not this session's party
    let mut b = good.clone();
    b["party"]["id"] = json!("cust-1");
    bodies.push(b);
    // an unknown token
    let mut b = good.clone();
    b["session_token"] = json!(format!("{}.not-the-secret", s1.id));
    bodies.push(b);
    // a session of another binding
    let other: Uuid = sqlx::query_scalar(
        "INSERT INTO connector_bindings (tenant_id, job_id, kind, side, external_id, label, created_by, verified_at) \
         VALUES ($1, $2, 'web_chat', 'customer', 'wk_two', 'Two', 'test', now()) RETURNING id",
    )
    .bind(c.t)
    .bind(c.job)
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    let (s, ob) = open_with(&c, other, "wk_two", PUBLIC, &ip(2)).await;
    assert_eq!(s, StatusCode::CREATED, "{ob}");
    let mut b = good.clone();
    b["session_token"] = ob["session_token"].clone();
    b["party"]["id"] = json!(format!("wc:{}", ob["session_id"].as_str().unwrap()));
    bodies.push(b);
    for b in bodies {
        let (s, r) = c.internal(INBOUND, b.clone()).await;
        assert_eq!(
            (s, code(&r)),
            (StatusCode::UNAUTHORIZED, "session_invalid"),
            "{b}"
        );
    }
    let mut b = good.clone();
    b["ip_hash"] = json!("10.0.0.1");
    let (s, r) = c.internal(INBOUND, b).await;
    assert_eq!(
        (s, code(&r)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_ip_hash")
    );
    assert_eq!(c.backend.calls(), 0);
    assert!(
        c.texts("SELECT conversation_id FROM conversation")
            .await
            .is_empty()
    );
    // the real thing is answered, and the session keeps its one conversation
    let (s, r) = c.internal(INBOUND, good).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let tied = c
        .texts("SELECT conversation_id FROM web_chat_session WHERE conversation_id IS NOT NULL")
        .await;
    assert_eq!(
        tied,
        vec![r["conversation_id"].as_str().unwrap().to_string()]
    );
    // one session is one conversation: a message naming another ref still lands in the session's own
    let mut elsewhere = message(&c, &s1, "wk_test", "m2", "hello again");
    elsewhere["conversation"]["id"] = json!("another-conversation");
    let (s, r) = c.internal(INBOUND, elsewhere).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["conversation_id"].as_str(), Some(tied[0].as_str()));
    assert_eq!(
        c.texts("SELECT conversation_id FROM conversation").await,
        tied
    );
}

#[tokio::test]
async fn first_message_retry_reuses_the_session_conversation() {
    let c = setup().await;
    let s1 = session(&c, &ip(1)).await;
    // the first message is stored but not answered (R6's accept; the edge died before its answer call, so the widget
    // holds no conversation token yet)
    let mut first = message(&c, &s1, "wk_test", "m1", "Where is my order?");
    first["accept_only"] = json!(true);
    let (s, b) = c.internal(INBOUND, first).await;
    assert_eq!(
        (s, b["status"].as_str()),
        (StatusCode::OK, Some("accepted")),
        "{b}"
    );
    // the resend: same message_id, but the edge minted another conversation ref
    let mut resent = message(&c, &s1, "wk_test", "m1", "Where is my order?");
    resent["conversation"]["id"] = json!("wc-the-resend");
    let (s, b) = c.internal(INBOUND, resent).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["status"], "answered",
        "the stored message is answered on the retry path"
    );
    assert_eq!(b["text"], agent_api::turn::TURN_FAILED, "{b}");
    let convs = c.texts("SELECT conversation_id FROM conversation").await;
    assert_eq!(convs.len(), 1, "one conversation");
    assert_eq!(b["conversation_id"].as_str(), Some(convs[0].as_str()));
    assert_eq!(
        c.texts("SELECT role FROM conversation_message ORDER BY at_ms, message_id")
            .await,
        ["customer", "agent"]
    );
    assert_eq!(
        c.texts("SELECT conversation_id FROM web_chat_session")
            .await,
        convs
    );
    assert_eq!(c.backend.calls(), 0);
}

#[tokio::test]
async fn shared_window_refusals_do_not_block_session() {
    let c = setup().await;
    set_limits(&c, json!({"msgs_per_minute": 1})).await;
    clear_of_window_end(60).await;
    let s1 = session(&c, &ip(1)).await;
    assert_eq!(say(&c, &s1, "m1", "first").await.0, StatusCode::OK);
    // the per-binding window refuses again and again: 429 each time, never an abuse event of this session
    for m in ["m2", "m3", "m4", "m5"] {
        let (s, b) = say(&c, &s1, m, &format!("more {m}")).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            "{m}"
        );
    }
    assert!(
        c.texts("SELECT kind FROM channel_abuse_event")
            .await
            .is_empty()
    );
    assert_eq!(blocked_reason(&c, &s1).await, None);
    // the binding's ceiling lifted: the same session talks on
    set_limits(&c, json!({"msgs_per_minute": 1000})).await;
    let (s, b) = say(&c, &s1, "m6", "still there?").await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn limits_survive_new_app_state() {
    let c = setup().await;
    set_limits(&c, json!({"msgs_per_session_hour": 2})).await;
    clear_of_window_end(3600).await;
    for _ in 0..6 {
        assert_eq!(open(&c, &ip(1)).await.0, StatusCode::CREATED);
    }
    let s1 = session(&c, &ip(2)).await;
    assert_eq!(say(&c, &s1, "m1", "first").await.0, StatusCode::OK);
    // a second agent-api on the same database (a restart): the counters are in Postgres
    let mut st2 = agent_api::state::build_for_tests((*c.e.state.cfg).clone(), common::TEST_SECRET)
        .await
        .expect("second state");
    st2.interaction = c.backend.clone();
    let app2 = agent_api::router(st2);
    let post = |path: &str, body: Value| {
        let mut r = common::json("POST", path, None, body);
        r.headers_mut().insert(
            "authorization",
            format!("Bearer {EDGE_KEY}").parse().unwrap(),
        );
        r
    };
    let body =
        json!({"binding_id": c.wc, "widget_key": "wk_test", "origin": PUBLIC, "ip_hash": ip(1)});
    for _ in 0..4 {
        let (s, b) = common::send(&app2, post(SESSIONS, body.clone())).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
    }
    let (s, b) = common::send(&app2, post(SESSIONS, body)).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::TOO_MANY_REQUESTS, "rate_limited")
    );
    let (s, b) = common::send(
        &app2,
        post(INBOUND, message(&c, &s1, "wk_test", "m2", "second")),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = common::send(
        &app2,
        post(INBOUND, message(&c, &s1, "wk_test", "m3", "third")),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::TOO_MANY_REQUESTS, "rate_limited")
    );
    // the sweep keeps the current windows
    let n = agent_api::conversations::webchat_sessions::sweep(&c.e.state.db)
        .await
        .unwrap();
    assert_eq!(n, 0);
    sqlx::query("UPDATE channel_rate_counter SET window_start = window_start - interval '3 hours'")
        .execute(&c.e.owner)
        .await
        .unwrap();
    let n = agent_api::conversations::webchat_sessions::sweep(&c.e.state.db)
        .await
        .unwrap();
    assert!(n > 0);
    assert!(
        c.texts("SELECT key FROM channel_rate_counter")
            .await
            .is_empty()
    );
    // sessions expired for over a day go too; a just-expired one stays (it still answers `session_invalid`)
    let s2 = session(&c, &ip(3)).await;
    for (id, ago) in [(&s1.id, "3 days"), (&s2.id, "2 hours")] {
        sqlx::query(
            "UPDATE web_chat_session SET created_at = now() - interval '4 days' - $2::interval, \
                    expires_at = now() - $2::interval WHERE id = $1::uuid",
        )
        .bind(id)
        .bind(ago)
        .execute(&c.e.owner)
        .await
        .unwrap();
    }
    agent_api::conversations::webchat_sessions::sweep(&c.e.state.db)
        .await
        .unwrap();
    // the test opened other (live) sessions above; only s1 is old enough to go
    let left = c.texts("SELECT id::text FROM web_chat_session").await;
    assert!(!left.contains(&s1.id), "{left:?}");
    assert!(left.contains(&s2.id), "{left:?}");
}

// ---- B14 §2: Turnstile on session create ---------------------------------------------------------------------------

const PLATFORM_SITE_KEY: &str = "0x4AAAAAAplatform-site-key";
const PLATFORM_SECRET: &str = "platform-turnstile-secret";

/// A fake siteverify: token `good` passes with action `webchat` on the public page's host, `other-action` passes with
/// another action, `elsewhere` passes on another host (another tenant's page under the shared platform key), `no-host`
/// passes without a hostname, anything else fails. Keeps each form body it received.
async fn siteverify() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use axum::extract::State;
    let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = std::sync::Arc::default();
    async fn verify(
        State(seen): State<std::sync::Arc<std::sync::Mutex<Vec<String>>>>,
        body: String,
    ) -> axum::Json<Value> {
        seen.lock().unwrap().push(body.clone());
        let token = body
            .split('&')
            .find_map(|kv| kv.strip_prefix("response="))
            .unwrap_or_default()
            .to_string();
        axum::Json(match token.as_str() {
            "good" => {
                json!({"success": true, "action": "webchat", "hostname": "chat.nonagentic.ai"})
            }
            "other-action" => json!({"success": true, "action": "signup"}),
            "elsewhere" => {
                json!({"success": true, "action": "webchat", "hostname": "shop.other-tenant.example"})
            }
            "no-host" => json!({"success": true, "action": "webchat"}),
            _ => json!({"success": false, "error-codes": ["invalid-input-response"]}),
        })
    }
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/siteverify", l.local_addr().unwrap());
    let app = axum::Router::new()
        .route("/siteverify", axum::routing::post(verify))
        .with_state(seen.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, seen)
}

/// A web chat env whose platform Turnstile verifies at `verify_url`.
async fn turnstile_setup(verify_url: String) -> Ctx {
    conv::setup_with(move |v| {
        v.insert(
            "WEBCHAT_TURNSTILE_SITE_KEY".into(),
            PLATFORM_SITE_KEY.into(),
        );
        v.insert("WEBCHAT_TURNSTILE_SECRET".into(), PLATFORM_SECRET.into());
        v.insert("TURNSTILE_VERIFY_URL".into(), verify_url);
        v.insert("TURNSTILE_TIMEOUT_MS".into(), "2000".into());
    })
    .await
}

/// Set `settings.turnstile` of the `wk_test` binding (directly: the settings check is the bindings route's own).
async fn set_turnstile(c: &Ctx, t: Value) {
    sqlx::query(
        "UPDATE connector_bindings SET settings = settings || jsonb_build_object('turnstile', $2::jsonb) WHERE id = $1",
    )
    .bind(c.wc)
    .bind(t)
    .execute(&c.e.owner)
    .await
    .unwrap();
}

async fn open_turnstile(c: &Ctx, token: Option<&str>) -> (StatusCode, Value) {
    let mut body = json!({"binding_id": c.wc, "widget_key": "wk_test", "origin": PUBLIC, "ip_hash": ip(9),
               "client_ip": "203.0.113.9"});
    if let Some(t) = token {
        body["turnstile_token"] = json!(t);
    }
    c.internal(SESSIONS, body).await
}

/// Sessions and the session/message counters (the `ts:` Turnstile-attempt windows are counted apart:
/// [`turnstile_attempts`]).
async fn sessions_and_counters(c: &Ctx) -> (usize, usize) {
    (
        c.texts("SELECT id::text FROM web_chat_session").await.len(),
        c.texts("SELECT key FROM channel_rate_counter WHERE key NOT LIKE 'ts:%'")
            .await
            .len(),
    )
}

/// The `ts:<ip_hash>` Turnstile-attempt windows.
async fn turnstile_attempts(c: &Ctx) -> usize {
    c.texts("SELECT key FROM channel_rate_counter WHERE key LIKE 'ts:%'")
        .await
        .len()
}

#[tokio::test]
async fn turnstile_off_leaves_session_create_unchanged() {
    let (url, seen) = siteverify().await;
    let c = turnstile_setup(url).await;
    // no `settings.turnstile`, then an explicit `off`: no token needed, siteverify never asked
    let (s, b) = open_turnstile(&c, None).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    set_turnstile(&c, json!({"mode": "off"})).await;
    let (s, b) = open_turnstile(&c, Some("anything")).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn turnstile_platform_requires_and_verifies_token() {
    let (url, seen) = siteverify().await;
    let c = turnstile_setup(url).await;
    set_turnstile(&c, json!({"mode": "platform"})).await;
    // no token: 428 with the site key the widget renders, and nothing counted or stored
    for token in [None, Some("  ")] {
        let (s, b) = open_turnstile(&c, token).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::PRECONDITION_REQUIRED, "turnstile_required"),
            "{b}"
        );
        assert_eq!(b["error"]["site_key"], PLATFORM_SITE_KEY);
    }
    assert_eq!(sessions_and_counters(&c).await, (0, 0));
    assert_eq!(turnstile_attempts(&c).await, 0, "no token is no attempt");
    // a token siteverify refuses, minted for another action, or solved on another host: 403, still no session counted
    for token in ["bad", "other-action", "elsewhere", "no-host"] {
        let (s, b) = open_turnstile(&c, Some(token)).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::FORBIDDEN, "turnstile_failed"),
            "{token}: {b}"
        );
    }
    assert_eq!(sessions_and_counters(&c).await, (0, 0));
    assert_eq!(turnstile_attempts(&c).await, 1);
    // a good one: 201 with the usual body, verified with the platform secret and the client IP
    let (s, b) = open_turnstile(&c, Some("good")).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert!(b["session_token"].as_str().is_some(), "{b}");
    assert!(b.get("site_key").is_none(), "{b}");
    assert_eq!(sessions_and_counters(&c).await, (1, 1));
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 5, "{seen:?}");
    let last = seen.last().unwrap();
    assert!(
        last.contains(&format!("secret={PLATFORM_SECRET}")),
        "{last}"
    );
    assert!(last.contains("response=good"), "{last}");
    assert!(last.contains("remoteip=203.0.113.9"), "{last}");
}

#[tokio::test]
async fn turnstile_fails_closed_when_verifier_is_down_or_unconfigured() {
    // a port nobody listens on
    let dead = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        format!("http://{}/siteverify", l.local_addr().unwrap())
    };
    let c = turnstile_setup(dead).await;
    set_turnstile(&c, json!({"mode": "platform"})).await;
    let (s, b) = open_turnstile(&c, Some("good")).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::SERVICE_UNAVAILABLE, "turnstile_unavailable"),
        "{b}"
    );
    assert_eq!(sessions_and_counters(&c).await, (0, 0));
    // `platform` without the platform keys, `own` without its site key or secret, an unknown mode: 503 unconfigured
    let c = setup().await;
    for t in [
        json!({"mode": "platform"}),
        json!({"mode": "own"}),
        json!({"mode": "own", "site_key": "0x4AAAAAAown"}),
        json!({"mode": "always"}),
    ] {
        set_turnstile(&c, t.clone()).await;
        let (s, b) = open_turnstile(&c, None).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::SERVICE_UNAVAILABLE, "turnstile_unconfigured"),
            "{t}: {b}"
        );
    }
    assert_eq!(sessions_and_counters(&c).await, (0, 0));
}

#[tokio::test]
async fn turnstile_own_mode_uses_the_binding_widget() {
    let (url, seen) = siteverify().await;
    let c = turnstile_setup(url).await;
    let (s, b) = c
        .call(
            "PUT",
            &format!("/api/jobs/j1/channel-bindings/{}", c.wc),
            "u-int",
            json!({"credential": {"turnstile_secret": "own-turnstile-secret"}}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    set_turnstile(
        &c,
        json!({"mode": "own", "site_key": "0x4AAAAAAown-site-key"}),
    )
    .await;
    let (s, b) = open_turnstile(&c, None).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::PRECONDITION_REQUIRED, "turnstile_required"),
        "{b}"
    );
    assert_eq!(b["error"]["site_key"], "0x4AAAAAAown-site-key");
    let (s, b) = open_turnstile(&c, Some("good")).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(seen[0].contains("secret=own-turnstile-secret"), "{seen:?}");
    assert!(!seen[0].contains(PLATFORM_SECRET), "{seen:?}");
}

#[tokio::test]
async fn turnstile_hostname_must_be_the_origin_host() {
    let (url, _) = siteverify().await;
    let c = turnstile_setup(url).await;
    sqlx::query(
        r#"UPDATE connector_bindings SET settings = '{"allowed_origins": ["https://shop.example:8443"]}' WHERE id = $1"#,
    )
    .bind(c.wc)
    .execute(&c.e.owner)
    .await
    .unwrap();
    set_turnstile(&c, json!({"mode": "platform"})).await;
    // `good` was solved on chat.nonagentic.ai: accepted from that origin, refused from shop.example
    let body = |origin: &str| {
        json!({"binding_id": c.wc, "widget_key": "wk_test", "origin": origin, "ip_hash": ip(9),
               "turnstile_token": "good"})
    };
    let (s, b) = c
        .internal(SESSIONS, body("https://shop.example:8443"))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "turnstile_failed"),
        "{b}"
    );
    let (s, b) = c.internal(SESSIONS, body(PUBLIC)).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
}

#[tokio::test]
async fn turnstile_attempts_per_ip_are_limited_before_siteverify() {
    let (url, seen) = siteverify().await;
    let c = turnstile_setup(url).await;
    set_turnstile(&c, json!({"mode": "platform"})).await;
    clear_of_window_end(600).await;
    for n in 0..20 {
        let (s, b) = open_turnstile(&c, Some("bad")).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::FORBIDDEN, "turnstile_failed"),
            "attempt {n}: {b}"
        );
    }
    // the 21st, even with a good token: 429 without asking siteverify
    let (s, b) = open_turnstile(&c, Some("good")).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
        "{b}"
    );
    assert!((1..=600).contains(&retry_after(&b)), "{b}");
    assert_eq!(seen.lock().unwrap().len(), 20);
    assert_eq!(sessions_and_counters(&c).await, (0, 0));
    // no token is not an attempt: still the 428 that names the site key
    let (s, b) = open_turnstile(&c, None).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::PRECONDITION_REQUIRED, "turnstile_required"),
        "{b}"
    );
}

// ---- known customer (B7: a verified assertion opens an identified session) ------------------------------------------

const ISSUER: &str = "https://shop.example";

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A host assertion for the `wk_test` binding (valid 2 minutes).
fn assertion(c: &Ctx, key: &HsKey, sub: &str, jti: &str) -> String {
    webchat_identity::mint_hs256(
        key,
        ISSUER,
        c.wc,
        sub,
        Some("ada@customer.example"),
        Some("Ada"),
        unix_now(),
        120,
        jti,
    )
}

/// Turn known-customer sessions on for the `wk_test` binding: `settings.identity` (directly) and the sealed
/// credential `identity_secret` (generated by the identity-secret route); answers that key.
async fn identity_on(c: &Ctx, extra: Value) -> HsKey {
    let mut identity = json!({"mode": "hs256", "issuer": ISSUER});
    for (k, v) in extra.as_object().unwrap() {
        identity[k] = v.clone();
    }
    sqlx::query(
        "UPDATE connector_bindings SET settings = settings || jsonb_build_object('identity', $2::jsonb) WHERE id = $1",
    )
    .bind(c.wc)
    .bind(identity)
    .execute(&c.e.owner)
    .await
    .unwrap();
    // the secret is made by the identity-secret route (a credential may never carry it), and shown once
    let (s, b) = c
        .call(
            "POST",
            &format!("/api/jobs/j1/channel-bindings/{}/identity-secret", c.wc),
            "u-int",
            json!({}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    HsKey {
        kid: b["kid"].as_str().unwrap().to_string(),
        secret: b["secret"].as_str().unwrap().to_string(),
    }
}

async fn open_known(c: &Ctx, ip: &str, assertion: Option<&str>) -> (StatusCode, Value) {
    let mut body =
        json!({"binding_id": c.wc, "widget_key": "wk_test", "origin": PUBLIC, "ip_hash": ip});
    if let Some(a) = assertion {
        body["customer_assertion"] = json!(a);
    }
    c.internal(SESSIONS, body).await
}

async fn jtis(c: &Ctx) -> usize {
    c.texts("SELECT jti FROM webchat_assertion_jti").await.len()
}

#[tokio::test]
async fn jti_is_single_use_across_two_sessions() {
    let c = setup().await;
    let key = identity_on(&c, json!({})).await;
    let a = assertion(&c, &key, "cust-1", "jti-0123456789abcdef");
    let (s, b) = open_known(&c, &ip(1), Some(&a)).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["identified"], json!(true));
    assert_eq!(b["greeting_name"], json!("Ada"));
    let row: (Option<String>, Option<String>, bool, Option<String>) = sqlx::query_as(
        "SELECT customer_ref, customer_email, identified_at IS NOT NULL, customer_name \
           FROM web_chat_session WHERE id = $1::uuid",
    )
    .bind(b["session_id"].as_str().unwrap())
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            Some("cust-1".into()),
            Some("ada@customer.example".into()),
            true,
            Some("Ada".into())
        )
    );
    // the same assertion again (another visitor, another ip): replayed, and no second session
    let (s, b) = open_known(&c, &ip(2), Some(&a)).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, "assertion_replayed")
    );
    assert_eq!(sessions_and_counters(&c).await.0, 1);
    assert_eq!(jtis(&c).await, 1);
}

/// The signed `name` is stored for display: trimmed, blank → NULL, cut to 200 chars; an anonymous session has none.
#[tokio::test]
async fn session_stores_the_assertion_name_trimmed() {
    let c = setup().await;
    let key = identity_on(&c, json!({})).await;
    let long = "x".repeat(300);
    for (n, (name, want)) in [
        (Some("  Ada Lovelace \t"), Some("Ada Lovelace".to_string())),
        (Some("   "), None),
        (None, None),
        // the verifier drops an over-long name (display data, never a reason to refuse the customer)
        (Some(long.as_str()), None),
    ]
    .into_iter()
    .enumerate()
    {
        let a = webchat_identity::mint_hs256(
            &key,
            ISSUER,
            c.wc,
            "cust-n",
            Some("ada@customer.example"),
            name,
            unix_now(),
            120,
            &format!("jti-name-{n}-0123456789"),
        );
        let (s, b) = open_known(&c, &ip(10 + n as u32), Some(&a)).await;
        assert_eq!(s, StatusCode::CREATED, "{name:?}: {b}");
        let got: Option<String> =
            sqlx::query_scalar("SELECT customer_name FROM web_chat_session WHERE id = $1::uuid")
                .bind(b["session_id"].as_str().unwrap())
                .fetch_one(&c.e.owner)
                .await
                .unwrap();
        assert_eq!(got, want, "case {n}");
    }
    // an anonymous session never carries a name (the CHECK ties it to a customer_ref)
    let (s, b) = open_known(&c, &ip(30), None).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let got: Option<String> =
        sqlx::query_scalar("SELECT customer_name FROM web_chat_session WHERE id = $1::uuid")
            .bind(b["session_id"].as_str().unwrap())
            .fetch_one(&c.e.owner)
            .await
            .unwrap();
    assert_eq!(got, None);
}

#[tokio::test]
async fn a_429_or_428_refusal_burns_no_jti() {
    let (url, _seen) = siteverify().await;
    let c = turnstile_setup(url).await;
    let key = identity_on(&c, json!({})).await;
    set_turnstile(&c, json!({"mode": "platform"})).await;
    // no assertion on a turnstile binding is still the 428
    let (s, b) = open_known(&c, &ip(1), None).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::PRECONDITION_REQUIRED, "turnstile_required")
    );
    // a verified assertion skips the challenge; the per-ip session limit (1) then refuses the second one: 429
    set_limits(&c, json!({"sessions_per_ip_hour": 1})).await;
    clear_of_window_end(3600).await;
    let first = assertion(&c, &key, "cust-1", "jti-first-0123456789");
    let (s, b) = open_known(&c, &ip(3), Some(&first)).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let second = assertion(&c, &key, "cust-2", "jti-second-0123456789");
    let (s, b) = open_known(&c, &ip(3), Some(&second)).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::TOO_MANY_REQUESTS, "rate_limited")
    );
    assert_eq!(jtis(&c).await, 1, "the 429 inserted no jti");
    // so the very same assertion still opens a session from another ip
    let (s, b) = open_known(&c, &ip(4), Some(&second)).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(jtis(&c).await, 2);
}

#[tokio::test]
async fn identified_session_is_capped_at_session_hours() {
    let c = setup().await;
    let key = identity_on(&c, json!({"session_hours": 1})).await;
    let a = assertion(&c, &key, "cust-1", "jti-cap-0123456789ab");
    let (s, b) = open_known(&c, &ip(1), Some(&a)).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let sess = Sess {
        id: b["session_id"].as_str().unwrap().to_string(),
        token: b["session_token"].as_str().unwrap().to_string(),
        ip: ip(1),
    };
    assert_eq!(say(&c, &sess, "m1", "hello").await.0, StatusCode::OK);
    sqlx::query(
        "UPDATE web_chat_session SET identified_at = now() - interval '61 minutes' WHERE id = $1::uuid",
    )
    .bind(&sess.id)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (s, b) = say(&c, &sess, "m2", "still there?").await;
    assert_eq!((s, code(&b)), (StatusCode::UNAUTHORIZED, "session_invalid"));
}

#[tokio::test]
async fn invalid_assertion_is_401_and_counted() {
    let c = setup().await;
    let _key = identity_on(&c, json!({})).await;
    clear_of_window_end(600).await;
    for n in 0..20 {
        let (s, b) = open_known(&c, &ip(5), Some("not.a.jwt")).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNAUTHORIZED, "assertion_invalid"),
            "refusal {n}: {b}"
        );
    }
    // the 21st refusal from the same ip answers 429
    let (s, b) = open_known(&c, &ip(5), Some("not.a.jwt")).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::TOO_MANY_REQUESTS, "rate_limited")
    );
    assert_eq!(
        c.texts("SELECT key FROM channel_rate_counter WHERE key LIKE 'assert:%'")
            .await
            .len(),
        1
    );
    assert_eq!(sessions_and_counters(&c).await.0, 0);
    assert_eq!(jtis(&c).await, 0);
}

#[tokio::test]
async fn mode_off_ignores_assertion() {
    let c = setup().await;
    // no `settings.identity` at all: even a garbage assertion opens an anonymous session
    let (s, b) = open_known(&c, &ip(1), Some("garbage")).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["identified"], json!(false));
    assert!(b["greeting_name"].is_null(), "{b}");
    let r: Option<String> =
        sqlx::query_scalar("SELECT customer_ref FROM web_chat_session WHERE id = $1::uuid")
            .bind(b["session_id"].as_str().unwrap())
            .fetch_one(&c.e.owner)
            .await
            .unwrap();
    assert_eq!(r, None);
    assert_eq!(jtis(&c).await, 0);
}

#[tokio::test]
async fn admit_returns_known_customer_from_the_row() {
    let c = setup().await;
    sqlx::query(
        r#"UPDATE connector_bindings SET settings = settings || '{"allowed_origins": ["https://shop.example"]}'::jsonb WHERE id = $1"#,
    )
    .bind(c.wc)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let key = identity_on(&c, json!({"sign_in_url": "https://shop.example/login"})).await;
    let a = assertion(&c, &key, "cust-9", "jti-admit-0123456789");
    let (s, b) = open_known(&c, &ip(1), Some(&a)).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["sign_in_url"], json!("https://shop.example/login"));
    let sess = Sess {
        id: b["session_id"].as_str().unwrap().to_string(),
        token: b["session_token"].as_str().unwrap().to_string(),
        ip: ip(1),
    };
    // a message on the identified session is admitted; the customer is the ROW's, so a request cannot claim another
    let mut m = message(&c, &sess, "wk_test", "m1", "hello");
    m["customer_ref"] = json!("someone-else");
    let (s, b) = c.internal(INBOUND, m).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let r: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT customer_ref, customer_email FROM web_chat_session WHERE id = $1::uuid",
    )
    .bind(&sess.id)
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(
        r,
        (Some("cust-9".into()), Some("ada@customer.example".into()))
    );
}

/// The sign-in URL handed to a visitor is re-validated against the binding's allowed origins (and works in mode off):
/// a stored URL on another origin, or `http:`, is never returned; a same-origin path is.
#[tokio::test]
async fn sign_in_url_is_returned_only_when_it_validates() {
    let c = setup().await;
    sqlx::query(
        r#"UPDATE connector_bindings SET settings = settings || '{"allowed_origins": ["https://shop.example"]}'::jsonb WHERE id = $1"#,
    )
    .bind(c.wc)
    .execute(&c.e.owner)
    .await
    .unwrap();
    for (stored, want) in [
        (
            "https://shop.example/login",
            json!("https://shop.example/login"),
        ),
        ("/account/sign-in", json!("/account/sign-in")),
        ("https://evil.example/login", Value::Null),
        ("http://shop.example/login", Value::Null),
        ("//evil.example/x", Value::Null),
    ] {
        sqlx::query(
            "UPDATE connector_bindings SET settings = settings || jsonb_build_object('identity', jsonb_build_object('mode', 'off', 'sign_in_url', $2::text)) WHERE id = $1",
        )
        .bind(c.wc)
        .bind(stored)
        .execute(&c.e.owner)
        .await
        .unwrap();
        let (s, b) = open_known(&c, &ip(2), None).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        assert_eq!(b["sign_in_url"], want, "{stored}");
    }
}

/// `identity_rotated` is permanent like `rotated`: the one-hour block window does not lift it, and a later abuse
/// block does not overwrite it with `rate_limited` (which would lift after an hour).
#[tokio::test]
async fn identity_rotated_block_is_permanent() {
    let c = setup().await;
    let sess = session(&c, &ip(1)).await;
    assert_eq!(say(&c, &sess, "m1", "hello").await.0, StatusCode::OK);
    sqlx::query(
        "UPDATE web_chat_session SET blocked_at = now() - interval '3 hours', blocked_reason = 'identity_rotated' \
          WHERE id = $1::uuid",
    )
    .bind(&sess.id)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (s, b) = say(&c, &sess, "m2", "still blocked?").await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "session_blocked"));
    // control: a timed `rate_limited` block of the same age has lifted
    let other = session(&c, &ip(3)).await;
    sqlx::query(
        "UPDATE web_chat_session SET blocked_at = now() - interval '3 hours', blocked_reason = 'rate_limited' \
          WHERE id = $1::uuid",
    )
    .bind(&other.id)
    .execute(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(say(&c, &other, "m1", "hello").await.0, StatusCode::OK);
}

/// Turn the `wk_test` binding into a known-customer one (the admit test's setup), open a session with `assertion`
/// and say one message on it; answers the stored `customer_id|identity_source` (state) of the one conversation.
async fn bound_by(c: &Ctx, key: &HsKey, sub: &str, email: &str, name: &str, jti: &str) -> String {
    let a = webchat_identity::mint_hs256(
        key,
        ISSUER,
        c.wc,
        sub,
        Some(email),
        Some(name),
        unix_now(),
        120,
        jti,
    );
    let (s, b) = open_known(c, &ip(1), Some(&a)).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let sess = Sess {
        id: b["session_id"].as_str().unwrap().to_string(),
        token: b["session_token"].as_str().unwrap().to_string(),
        ip: ip(1),
    };
    let (s, b) = say(c, &sess, "m1", "hello").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let rows = c
        .texts(&format!(
            "SELECT conversation.customer_id || '|' || COALESCE({}, '') FROM conversation",
            agent_api::conversations::state::sub(
                "conversation",
                agent_api::conversations::state::IDENTITY_SOURCE
            )
        ))
        .await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    rows[0].clone()
}

/// A staff-audience job (`jobs.audience = 'employees'`, D129): the minted employee assertion binds the
/// conversation's party to the employee's lower-cased verified email, `identity_source` `assertion`.
#[tokio::test]
async fn staff_job_assertion_binds_the_verified_email_as_the_party() {
    let c = setup().await;
    sqlx::query("UPDATE jobs SET audience = 'employees' WHERE id = $1")
        .bind(c.job)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let key = identity_on(&c, json!({})).await;
    let email = "anna.kowalski@northwind-it.example";
    let got = bound_by(
        &c,
        &key,
        email,
        email,
        "Anna Kowalski",
        "jti-staff-0123456789",
    )
    .await;
    assert_eq!(got, format!("{email}|assertion"));
}

/// A customer-audience job is unchanged: the party is the assertion's `sub` (`cust-1001`), not its email.
#[tokio::test]
async fn customer_job_assertion_still_binds_the_customer_id() {
    let c = setup().await;
    let key = identity_on(&c, json!({})).await;
    let got = bound_by(
        &c,
        &key,
        "cust-1001",
        "emma@customer.example",
        "Emma Clarke",
        "jti-cust-0123456789",
    )
    .await;
    assert_eq!(got, "cust-1001|assertion");
}
