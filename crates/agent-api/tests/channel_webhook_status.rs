//! B7 T15b Telegram webhook health: agent-edge posts what it observed of one binding to
//! `POST /api/internal/channel/webhook-status` (EdgeKey only); agent-api checks the binding (kind, customer side,
//! tenant, job) and upserts `channel_webhook_status` (0153, 0155: merged across replicas, `quiet` after 24 h without
//! an update, `down` / `degraded` from the webhook info Telegram reports); the binding view carries `webhook` (or
//! `null` when the edge has reported nothing). Real Postgres 18.
use crate::common;

use axum::http::StatusCode;
use common::conv::{self, Ctx, EDGE_KEY, code};
use common::send;
use serde_json::{Value, json};
use uuid::Uuid;

const PATH: &str = "/api/internal/channel/webhook-status";
const H: i64 = 3_600_000;

/// A Telegram customer binding of job `j1` with bot id `bot` (verified at creation: the token proves it).
async fn telegram(c: &Ctx, bot: &str) -> Value {
    let (s, b) = c
        .call(
            "POST",
            "/api/jobs/j1/channel-bindings",
            "u-int",
            json!({"kind": "telegram", "side": "customer", "label": format!("Bot {bot}"),
                   "settings": {"allowed_user_ids": [42]},
                   "credential": {"bot_token": format!("{bot}:test-not-a-real-bot-token-000000"),
                                  "webhook_secret": "test-webhook-secret-0001"}}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    c.verify_all().await;
    b
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn report(c: &Ctx, tg: &Value) -> Value {
    json!({"binding_id": tg["id"], "kind": "telegram", "tenant": c.t, "job_id": c.job,
           "last_update_at_ms": now_ms() - 60_000, "consecutive_send_failures": 0})
}

/// The `webhook` of binding `id` in the job's binding list.
async fn webhook_of(c: &Ctx, id: &Value) -> Value {
    let (s, l) = c.get("/api/jobs/j1/channel-bindings", "u-int").await;
    assert_eq!(s, StatusCode::OK, "{l}");
    l["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| &b["id"] == id)
        .unwrap()["webhook"]
        .clone()
}

#[tokio::test]
async fn the_door_is_key_gated() {
    let c = conv::setup().await;
    let tg = telegram(&c, "123456789").await;
    for token in [None, Some("nope"), Some("u-admin")] {
        let (s, b) = send(&c.e.app, common::json("POST", PATH, token, report(&c, &tg))).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{token:?}: {b}");
    }
    let (s, b) = send(
        &c.e.app,
        common::json("POST", PATH, Some(EDGE_KEY), report(&c, &tg)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn a_binding_of_another_tenant_or_job_or_kind_is_refused() {
    let c = conv::setup().await;
    let tg = telegram(&c, "123456789").await;
    let beta = Uuid::new_v4();
    common::insert_tenant(&c.e.owner, beta, "Beta").await;
    let mut body = report(&c, &tg);
    body["tenant"] = json!(beta);
    let (s, b) = c.internal(PATH, body).await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "binding_not_found"));
    let mut body = report(&c, &tg);
    body["job_id"] = json!(Uuid::new_v4());
    let (s, b) = c.internal(PATH, body).await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "binding_not_found"));
    // the job's web chat binding is not a Telegram binding
    let mut body = report(&c, &tg);
    body["binding_id"] = json!(c.wc);
    let (s, b) = c.internal(PATH, body).await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "binding_not_found"));
    let mut body = report(&c, &tg);
    body["kind"] = json!("sms");
    let (s, b) = c.internal(PATH, body).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_channel")
    );
    // an error is a code, never text, and comes with its time
    let mut body = report(&c, &tg);
    body["last_error"] = json!("Bad Request: chat not found");
    body["last_error_at_ms"] = json!(now_ms());
    let (s, b) = c.internal(PATH, body).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_error")
    );
    let mut body = report(&c, &tg);
    body["last_error"] = json!("telegram_400_400");
    let (s, b) = c.internal(PATH, body).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_error")
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM channel_webhook_status")
        .fetch_one(&c.e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0, "nothing refused was stored");
}

#[tokio::test]
async fn reports_upsert_and_the_view_shows_them() {
    let c = conv::setup().await;
    let tg = telegram(&c, "123456789").await;
    assert_eq!(
        webhook_of(&c, &tg["id"]).await,
        Value::Null,
        "no report: unknown"
    );

    let (s, b) = c.internal(PATH, report(&c, &tg)).await;
    assert_eq!(
        (s, b.clone()),
        (StatusCode::OK, json!({"status": "ok"})),
        "{b}"
    );
    let w = webhook_of(&c, &tg["id"]).await;
    assert_eq!(w["status"], "ok");
    assert!(
        w["last_update_at"].is_string() && w["observed_at"].is_string(),
        "{w}"
    );
    assert_eq!(
        (w["last_error"].clone(), w["last_error_at"].clone()),
        (Value::Null, Value::Null)
    );

    // three sends failed in a row: degraded
    let failed_at = now_ms() - 30_000;
    let mut degraded_report = report(&c, &tg);
    degraded_report["consecutive_send_failures"] = json!(3);
    degraded_report["last_error"] = json!("telegram_403_403");
    degraded_report["last_error_at_ms"] = json!(failed_at);
    let (s, b) = c.internal(PATH, degraded_report.clone()).await;
    assert_eq!(
        (s, b["status"].clone()),
        (StatusCode::OK, json!("degraded")),
        "{b}"
    );
    let w = webhook_of(&c, &tg["id"]).await;
    assert_eq!(
        (w["status"].clone(), w["last_error"].clone()),
        (json!("degraded"), json!("telegram_403_403"))
    );
    assert!(w["last_error_at"].is_string());

    // another replica (or an edge restart) with empty counters: keeps the stored update time and the last error,
    // and cannot clear degraded (0155: only a send that succeeded after the failures does)
    let before = w["last_update_at"].clone();
    let body = json!({"binding_id": tg["id"], "kind": "telegram", "tenant": c.t, "job_id": c.job});
    let (s, b) = c.internal(PATH, body).await;
    assert_eq!(
        (s, b["status"].clone()),
        (StatusCode::OK, json!("degraded")),
        "{b}"
    );
    let w = webhook_of(&c, &tg["id"]).await;
    assert_eq!(w["last_update_at"], before);
    assert_eq!(w["last_error"], "telegram_403_403");
    let mut body = report(&c, &tg);
    body["last_send_ok_at_ms"] = json!(failed_at - 60_000);
    let (s, b) = c.internal(PATH, body).await;
    assert_eq!(
        (s, b["status"].clone()),
        (StatusCode::OK, json!("degraded")),
        "a successful send older than the failures: {b}"
    );
    // a send succeeded after the failures: ok, and the first replica's stale run does not bring degraded back
    let mut body = report(&c, &tg);
    body["last_send_ok_at_ms"] = json!(failed_at + 1);
    let (s, b) = c.internal(PATH, body).await;
    assert_eq!(
        (s, b["status"].clone()),
        (StatusCode::OK, json!("ok")),
        "{b}"
    );
    let (s, b) = c.internal(PATH, degraded_report).await;
    assert_eq!(
        (s, b["status"].clone()),
        (StatusCode::OK, json!("ok")),
        "{b}"
    );
    let (ok_at, degraded_at): (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT (extract(epoch FROM last_send_ok_at) * 1000)::bigint, \
                (extract(epoch FROM degraded_at) * 1000)::bigint \
         FROM channel_webhook_status WHERE binding_id = $1",
    )
    .bind(tg["id"].as_str().unwrap().parse::<Uuid>().unwrap())
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    assert_eq!((ok_at, degraded_at), (Some(failed_at + 1), Some(failed_at)));
    let rows: Vec<(Option<String>, Option<i32>)> = sqlx::query_as(
        "SELECT url, pending_update_count FROM channel_webhook_status WHERE binding_id = $1",
    )
    .bind(tg["id"].as_str().unwrap().parse::<Uuid>().unwrap())
    .fetch_all(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![(None, None)],
        "url / pending count are never faked"
    );

    // a second bot whose last update is older than the stale window: quiet (informational, not down)
    let quiet = telegram(&c, "987654321").await;
    let mut body = report(&c, &quiet);
    body["last_update_at_ms"] = json!(now_ms() - 25 * H);
    let (s, b) = c.internal(PATH, body).await;
    assert_eq!(
        (s, b["status"].clone()),
        (StatusCode::OK, json!("quiet")),
        "{b}"
    );
    assert_eq!(webhook_of(&c, &quiet["id"]).await["status"], "quiet");
    assert_eq!(webhook_of(&c, &tg["id"]).await["status"], "ok");

    // the row goes with its binding
    let (s, b) = c
        .call(
            "DELETE",
            &format!(
                "/api/jobs/j1/channel-bindings/{}",
                quiet["id"].as_str().unwrap()
            ),
            "u-int",
            json!({}),
        )
        .await;
    assert!(s.is_success(), "{s}: {b}");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM channel_webhook_status")
        .fetch_one(&c.e.owner)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

fn with_info(c: &Ctx, tg: &Value, info: Value) -> Value {
    let mut b = report(c, tg);
    b["webhook_info"] = info;
    b
}

/// status, webhook_down_reason, pending_update_count, url of binding `tg`.
async fn stored(c: &Ctx, tg: &Value) -> (String, Option<String>, Option<i32>, Option<String>) {
    sqlx::query_as(
        "SELECT status, webhook_down_reason, pending_update_count, url FROM channel_webhook_status \
         WHERE binding_id = $1",
    )
    .bind(tg["id"].as_str().unwrap().parse::<Uuid>().unwrap())
    .fetch_one(&c.e.owner)
    .await
    .unwrap()
}

async fn status_after(c: &Ctx, body: Value) -> Value {
    let (s, b) = c.internal(PATH, body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["status"].clone()
}

#[tokio::test]
async fn webhook_info_makes_down_and_degraded() {
    let c = conv::setup().await;
    let good = json!({"url_set": true, "url_matches": true, "pending_update_count": 0});

    // no URL registered: down, url_unset; the URL itself is never stored
    let unset = telegram(&c, "100000001").await;
    let body = with_info(
        &c,
        &unset,
        json!({"url_set": false, "pending_update_count": 0}),
    );
    assert_eq!(status_after(&c, body).await, "down");
    assert_eq!(
        stored(&c, &unset).await,
        ("down".into(), Some("url_unset".into()), Some(0), None)
    );
    let w = webhook_of(&c, &unset["id"]).await;
    assert_eq!(
        (
            w["status"].clone(),
            w["down_reason"].clone(),
            w["pending_update_count"].clone()
        ),
        (json!("down"), json!("url_unset"), json!(0))
    );

    // a URL that is not this binding's route: down, url_mismatch
    let other = telegram(&c, "100000002").await;
    let body = with_info(&c, &other, json!({"url_set": true, "url_matches": false}));
    assert_eq!(status_after(&c, body).await, "down");
    assert_eq!(stored(&c, &other).await.1.as_deref(), Some("url_mismatch"));

    // a delivery error newer than the last update, inside the window: down, webhook_error, and it is the last error
    let failing = telegram(&c, "100000003").await;
    let mut info = good.clone();
    info["last_error_code"] = json!("webhook_http_502");
    info["last_error_at_ms"] = json!(now_ms() - 30_000);
    assert_eq!(
        status_after(&c, with_info(&c, &failing, info)).await,
        "down"
    );
    assert_eq!(
        stored(&c, &failing).await.1.as_deref(),
        Some("webhook_error")
    );
    let w = webhook_of(&c, &failing["id"]).await;
    assert_eq!(w["last_error"], "webhook_http_502");
    assert!(w["last_error_at"].is_string(), "{w}");

    // once the stored info is stale, a report without webhook info (an older edge, a failed call) cannot derive
    // down: the observed rule alone; the reason goes, the pending count stays
    age_info(&c, &failing, 6).await;
    let (s, b) = c.internal(PATH, report(&c, &failing)).await;
    assert_eq!(
        (s, b["status"].clone()),
        (StatusCode::OK, json!("ok")),
        "{b}"
    );
    assert_eq!(
        stored(&c, &failing).await,
        ("ok".into(), None, Some(0), None)
    );

    // a delivery error OLDER than the last update (delivery recovered): not down
    let recovered = telegram(&c, "100000004").await;
    let mut info = good.clone();
    info["last_error_code"] = json!("webhook_timeout");
    info["last_error_at_ms"] = json!(now_ms() - 120_000);
    assert_eq!(
        status_after(&c, with_info(&c, &recovered, info)).await,
        "ok"
    );
    assert_eq!(stored(&c, &recovered).await.1, None);
    assert_eq!(
        webhook_of(&c, &recovered["id"]).await["last_error"],
        "webhook_timeout"
    );

    // an error outside the 15 min window with no update since: not down either
    let stale = telegram(&c, "100000005").await;
    let mut info = good.clone();
    info["last_error_code"] = json!("webhook_refused");
    info["last_error_at_ms"] = json!(now_ms() - 20 * 60_000);
    let mut body = with_info(&c, &stale, info);
    body["last_update_at_ms"] = Value::Null;
    assert_eq!(status_after(&c, body).await, "ok");

    // 150 updates pending: degraded (no degraded_at needed), with the count in the view
    let backlog = telegram(&c, "100000006").await;
    let mut info = good.clone();
    info["pending_update_count"] = json!(150);
    assert_eq!(
        status_after(&c, with_info(&c, &backlog, info)).await,
        "degraded"
    );
    assert_eq!(
        stored(&c, &backlog).await,
        ("degraded".into(), None, Some(150), None)
    );
    assert_eq!(
        webhook_of(&c, &backlog["id"]).await["pending_update_count"],
        150
    );
    // healthy info again: ok
    assert_eq!(
        status_after(&c, with_info(&c, &backlog, good.clone())).await,
        "ok"
    );

    // a webhook error is a code with its time, like a send error
    let mut info = good.clone();
    info["last_error_code"] = json!("Wrong response from the webhook: 502");
    info["last_error_at_ms"] = json!(now_ms());
    let (s, b) = c.internal(PATH, with_info(&c, &backlog, info)).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_error")
    );
    let mut info = good;
    info["last_error_code"] = json!("webhook_error");
    let (s, b) = c.internal(PATH, with_info(&c, &backlog, info)).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_error")
    );
}

/// Move the binding's `webhook_info_at` `minutes` into the past (as if the last report with webhook info were older).
async fn age_info(c: &Ctx, tg: &Value, minutes: i32) {
    sqlx::query(
        "UPDATE channel_webhook_status SET webhook_info_at = now() - make_interval(mins => $2) \
         WHERE binding_id = $1",
    )
    .bind(tg["id"].as_str().unwrap().parse::<Uuid>().unwrap())
    .bind(minutes)
    .execute(&c.e.owner)
    .await
    .unwrap();
}

/// Replica A reads the webhook info (no URL: down); replica B, whose call fails, reports a minute later without it:
/// still down, from the stored info (no flap). Once the stored info is older than 5 min: the #63 rule.
#[tokio::test]
async fn stored_webhook_info_keeps_down_across_replicas_while_fresh() {
    let c = conv::setup().await;
    let tg = telegram(&c, "100000011").await;
    let a = with_info(
        &c,
        &tg,
        json!({"url_set": false, "pending_update_count": 0}),
    );
    assert_eq!(status_after(&c, a).await, "down");
    let at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT webhook_info_at FROM channel_webhook_status WHERE binding_id = $1",
    )
    .bind(tg["id"].as_str().unwrap().parse::<Uuid>().unwrap())
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    assert!(
        at.is_some(),
        "a report with webhook info stamps webhook_info_at"
    );

    // replica B, a minute later, without webhook info: still down, reason and count kept, webhook_info_at unchanged
    age_info(&c, &tg, 1).await;
    assert_eq!(status_after(&c, report(&c, &tg)).await, "down");
    assert_eq!(
        stored(&c, &tg).await,
        ("down".into(), Some("url_unset".into()), Some(0), None)
    );
    assert_eq!(webhook_of(&c, &tg["id"]).await["down_reason"], "url_unset");

    // the stored info is older than 5 min: the observed (#63) rule
    age_info(&c, &tg, 6).await;
    assert_eq!(status_after(&c, report(&c, &tg)).await, "ok");
    assert_eq!(stored(&c, &tg).await, ("ok".into(), None, Some(0), None));
}
