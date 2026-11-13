//! D130: a customer channel is tested before it is published. A customer binding is created `env='test'` (a worker
//! binding stays `prod`); `POST …/publish` (release_manager) moves it to `prod` once the job has a release (409
//! `no_release` before), `…/unpublish` moves it back, both audited and idempotent; a message on a test binding runs
//! the draft design in the test scope (conversation env `test`), and after publish the same chat starts a NEW prod
//! conversation. Real Postgres 18 and the shared test Redis.
use agent_api::runtime::Env;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::common::conv::{self, Ctx, code};
use crate::redis_env;

const BASE: &str = "/api/jobs/j1/channel-bindings";

fn telegram_body() -> Value {
    json!({"kind": "telegram", "side": "customer", "label": "Test bot",
           "settings": {"allowed_user_ids": [42]},
           "credential": {"bot_token": "123456789:test-not-a-real-bot-token-000000",
                          "webhook_secret": "test-webhook-secret-0001"}})
}

async fn create_telegram(c: &Ctx) -> Value {
    let (s, b) = c.call("POST", BASE, "u-int", telegram_body()).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    c.verify_all().await;
    b
}

fn message(c: &Ctx, tg: &Value, id: &str) -> Value {
    json!({
        "kind": "telegram", "external_id": "123456789", "binding_id": tg["id"],
        "tenant": c.t, "job_id": c.job, "job_key": "j1",
        "conversation": {"channel": "telegram", "id": "42", "route": {}},
        "message_id": id, "party": {"id": "42", "name": "Ada", "verified": true},
        "text": "Where is my parcel?",
    })
}

async fn action(c: &Ctx, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = $1")
        .bind(action)
        .fetch_one(&c.e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn customer_bindings_start_in_test_and_worker_bindings_in_prod() {
    let c = conv::setup().await;
    let tg = create_telegram(&c).await;
    assert_eq!(tg["env"], "test", "{tg}");
    let (s, w) = c
        .call(
            "POST",
            BASE,
            "u-int",
            json!({"kind": "helpdesk", "side": "worker", "external_id": "helpdesk.test#support", "label": "Ops",
                   "settings": {"base_url": "https://helpdesk.example", "service_user": "bot@helpdesk.example"},
                   "credential": {"api_key": "key-test", "api_secret": "secret-test",
                                  "webhook_secret": "wh-secret-test-0123456789"}}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{w}");
    assert_eq!(w["env"], "prod", "{w}");
    let (_, l) = c.get(BASE, "u-int").await;
    let envs: Vec<(&str, &str)> = l["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| (b["side"].as_str().unwrap(), b["env"].as_str().unwrap()))
        .collect();
    assert!(envs.contains(&("customer", "test")) && envs.contains(&("worker", "prod")));
}

#[tokio::test]
async fn publish_needs_a_release_and_the_release_manager_role() {
    // `setup` leaves j1 without a release
    let c = conv::setup().await;
    let tg = create_telegram(&c).await;
    let path = format!("{BASE}/{}/publish", tg["id"].as_str().unwrap());
    let (s, b) = c.call("POST", &path, "u-admin", json!({})).await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "no_release"), "{b}");
    let (_, l) = c.get(BASE, "u-int").await;
    let row = l["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == tg["id"])
        .unwrap();
    assert_eq!(row["env"], "test", "a refused publish changes nothing");
    // integrations_manager alone does not hold release_manager
    let (s, b) = c.call("POST", &path, "u-int", json!({})).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], "release_manager", "{b}");
    assert_eq!(action(&c, "channel_binding.publish").await, 0);
}

#[tokio::test]
async fn publish_and_unpublish_round_trip_is_audited_and_idempotent() {
    let c = conv::setup_live(|_| {}).await;
    let tg = create_telegram(&c).await;
    let id = tg["id"].as_str().unwrap();
    let publish = format!("{BASE}/{id}/publish");
    let unpublish = format!("{BASE}/{id}/unpublish");
    for _ in 0..2 {
        let (s, b) = c.call("POST", &publish, "u-admin", json!({})).await;
        assert_eq!(
            (s, b["env"].as_str()),
            (StatusCode::OK, Some("prod")),
            "{b}"
        );
    }
    assert_eq!(action(&c, "channel_binding.publish").await, 1, "idempotent");
    for _ in 0..2 {
        let (s, b) = c.call("POST", &unpublish, "u-admin", json!({})).await;
        assert_eq!(
            (s, b["env"].as_str()),
            (StatusCode::OK, Some("test")),
            "{b}"
        );
    }
    assert_eq!(action(&c, "channel_binding.unpublish").await, 1);
    let (s, b) = c
        .call(
            "POST",
            &format!("{BASE}/{}/publish", uuid::Uuid::new_v4()),
            "u-admin",
            json!({}),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "binding_not_found"));
}

#[tokio::test]
async fn a_test_binding_answers_in_the_test_env_and_publish_starts_a_prod_conversation() {
    let c = conv::setup_runtime(redis_env::api_vars("d130-publish").await).await;
    c.serve(Env::Test).await;
    let tg = create_telegram(&c).await;
    const INBOUND: &str = "/api/internal/channel/inbound";
    let (s, b) = c.internal(INBOUND, message(&c, &tg, "9001")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "answered", "{b}");
    let test_conv = b["conversation_id"].as_str().unwrap().to_string();
    let env_of = |id: String| {
        let c = &c;
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT env FROM conversation WHERE conversation_id = $1",
            )
            .bind(id)
            .fetch_one(&c.e.owner)
            .await
            .unwrap()
        }
    };
    assert_eq!(env_of(test_conv.clone()).await, "test");
    // the same chat after publish: a new conversation, in prod
    let (s, p) = c
        .call(
            "POST",
            &format!("{BASE}/{}/publish", tg["id"].as_str().unwrap()),
            "u-admin",
            json!({}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{p}");
    let (s, b) = c.internal(INBOUND, message(&c, &tg, "9002")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let prod_conv = b["conversation_id"].as_str().unwrap().to_string();
    assert_ne!(prod_conv, test_conv, "{b}");
    assert_eq!(env_of(prod_conv).await, "prod");
}
