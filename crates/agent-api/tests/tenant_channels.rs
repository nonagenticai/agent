//! T13 `GET /api/tenant/channels` (`ChannelsView`): any member sees the acting tenant's customer-side channel
//! bindings (never another tenant's), each with its webhook health from `channel_webhook_status` (or `null`), the
//! staff test chat, the catalogue of kinds not bound yet, gate 2 read from the stored licence verdict, the neutral
//! refusal text and the refusals of the last 24 h. Real Postgres 18.
use crate::common;

use axum::http::StatusCode;
use common::conv::{self, Ctx};
use serde_json::{Value, json};
use uuid::Uuid;

const PATH: &str = "/api/tenant/channels";

async fn view(c: &Ctx, sub: &str) -> Value {
    let (s, b) = c.get(PATH, sub).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

fn channel<'a>(v: &'a Value, id: &str) -> Option<&'a Value> {
    v["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
}

/// A Telegram customer binding of job `j1` (verified: the token proves it).
async fn telegram(c: &Ctx, bot: &str) -> Uuid {
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
    b["id"].as_str().unwrap().parse().unwrap()
}

#[tokio::test]
async fn lists_this_tenants_bindings_and_never_another_tenants() {
    let c = conv::setup().await;
    let beta = Uuid::new_v4();
    common::insert_tenant(&c.e.owner, beta, "Beta").await;
    let beta_job: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'j1', 'Beta J1', '[]', 'test') RETURNING id",
    )
    .bind(beta)
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    let beta_binding: Uuid = sqlx::query_scalar(
        "INSERT INTO connector_bindings (tenant_id, job_id, kind, side, external_id, label, created_by, verified_at) \
         VALUES ($1, $2, 'web_chat', 'customer', 'wk_beta', 'Beta site', 'test', now()) RETURNING id",
    )
    .bind(beta)
    .bind(beta_job)
    .fetch_one(&c.e.owner)
    .await
    .unwrap();

    // an observer (tenant_user) reads it: any member
    let v = view(&c, "u-obs").await;
    let wc = channel(&v, &c.wc.to_string()).expect("the job's web chat binding is listed");
    assert_eq!(wc["kind"], "web_chat");
    assert_eq!(wc["label"], "Site");
    assert_eq!(wc["state"], "live");
    assert_eq!(wc["bound_tenant"], c.t.to_string());
    assert_eq!(wc["bound_job"], "j1");
    assert_eq!(wc["env"], "prod");
    assert_eq!(wc["webhook"], Value::Null, "no status row: unknown");
    assert_eq!(wc["conversations_7d"], 0);
    assert_eq!(
        wc["widget_key"], "wk_test",
        "a live web chat door carries its public widget key for Try it"
    );
    assert_eq!(
        wc["answered_by"],
        json!({"job": "j1", "job_label": "J1", "release_id": null, "release_hash": null, "env": "prod"})
    );
    assert!(
        channel(&v, &beta_binding.to_string()).is_none(),
        "another tenant's binding is never listed"
    );
    assert!(!v.to_string().contains("wk_beta") && !v.to_string().contains("Beta site"));

    // the staff test chat answers the tenant's job in the test env
    let staff = channel(&v, "ch-staff").unwrap();
    assert_eq!(
        (staff["state"].clone(), staff["env"].clone()),
        (json!("live"), json!("test"))
    );
    assert_eq!(staff["answered_by"]["job"], "j1");
    assert_eq!(staff["widget_key"], Value::Null);
    // the catalogue: every customer kind no job has bound, and voice later; web chat is bound so not offered
    assert_eq!(channel(&v, "ch-telegram").unwrap()["state"], "available");
    assert!(channel(&v, "ch-web_chat").is_none());
    assert_eq!(channel(&v, "ch-voice").unwrap()["state"], "later");

    // gate 2: Alpha holds a fresh active verdict
    assert_eq!(v["tenant_status"], "active");
    assert_eq!(
        (v["gate"]["status"].clone(), v["gate"]["reason"].clone()),
        (json!("active"), Value::Null)
    );
    assert_eq!(v["refusal_text"], agent_api::turn::REFUSAL_TEXT);
    assert_eq!(v["refusals_24h"], 0);
    assert_eq!(v["job_operations"], json!([]));
}

#[tokio::test]
async fn webhook_health_is_mapped_from_the_status_row() {
    let c = conv::setup().await;
    let tg = telegram(&c, "123456789").await;
    let v = view(&c, "u-obs").await;
    let row = channel(&v, &tg.to_string()).unwrap();
    assert_eq!(
        (row["kind"].clone(), row["state"].clone()),
        (json!("telegram"), json!("live"))
    );
    assert_eq!(row["webhook"], Value::Null, "nothing reported yet");
    assert_eq!(
        row["widget_key"],
        Value::Null,
        "only web chat has a widget key"
    );
    // no credential or secret of any binding is ever returned
    let body = v.to_string();
    for secret in [
        "test-not-a-real-bot-token",
        "test-webhook-secret-0001",
        "bot_token",
        "webhook_secret",
        "\"credential\"",
    ] {
        assert!(!body.contains(secret), "{secret} leaked: {body}");
    }
    assert!(
        channel(&v, "ch-telegram").is_none(),
        "telegram is bound now"
    );

    sqlx::query(
        "INSERT INTO channel_webhook_status \
           (binding_id, tenant_id, job_id, status, last_update_at, pending_update_count, last_error, last_error_at, \
            observed_at, degraded_at) \
         VALUES ($1, $2, $3, 'degraded', now() - interval '1 minute', 4, 'telegram_403_403', now(), now(), now())",
    )
    .bind(tg)
    .bind(c.t)
    .bind(c.job)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let v = view(&c, "u-obs").await;
    let w = &channel(&v, &tg.to_string()).unwrap()["webhook"];
    assert_eq!(w["status"], "degraded");
    assert_eq!(w["pending_update_count"], 4);
    assert_eq!(w["last_error"], "telegram_403_403");
    assert!(
        w["last_update_at"].is_string()
            && w["last_error_at"].is_string()
            && w["observed_at"].is_string()
    );
    assert!(
        w.get("url").is_none(),
        "the webhook URL is never stored, so never shown: {w}"
    );
    // the web chat binding still has no row
    assert_eq!(
        channel(&v, &c.wc.to_string()).unwrap()["webhook"],
        Value::Null
    );

    // pending count unknown (as the edge reports it today): null, quiet passes through
    sqlx::query(
        "UPDATE channel_webhook_status SET status = 'quiet', degraded_at = NULL, pending_update_count = NULL, \
           last_error = NULL, last_error_at = NULL WHERE binding_id = $1",
    )
    .bind(tg)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let v = view(&c, "u-obs").await;
    let w = &channel(&v, &tg.to_string()).unwrap()["webhook"];
    assert_eq!(
        (w["status"].clone(), w["pending_update_count"].clone()),
        (json!("quiet"), Value::Null)
    );
}

#[tokio::test]
async fn a_disabled_or_unverified_binding_is_not_live() {
    let c = conv::setup().await;
    sqlx::query("UPDATE connector_bindings SET enabled = false WHERE id = $1")
        .bind(c.wc)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let v = view(&c, "u-obs").await;
    let wc = channel(&v, &c.wc.to_string()).unwrap();
    assert_eq!(wc["state"], "not_ready");
    assert_eq!(wc["answered_by"], Value::Null);
    assert_eq!(wc["bound_job"], "j1");
    assert_eq!(
        wc["widget_key"],
        Value::Null,
        "a disabled door offers no Try it"
    );
    assert!(!v.to_string().contains("wk_test"));

    // enabled but unverified: still no key
    sqlx::query("UPDATE connector_bindings SET enabled = true, verified_at = NULL, verify_code = 'ABC234' WHERE id = $1")
        .bind(c.wc)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let v = view(&c, "u-obs").await;
    let wc = channel(&v, &c.wc.to_string()).unwrap();
    assert_eq!(wc["state"], "not_ready");
    assert_eq!(
        wc["widget_key"],
        Value::Null,
        "an unverified door offers no Try it"
    );
}

#[tokio::test]
async fn the_gate_reads_the_verdict_and_counts_refusals() {
    let c = conv::setup().await;
    sqlx::query(
        "INSERT INTO audit_event (tenant_id, actor_id, actor_type, action, resource_type, resource_id, job, status) \
         VALUES ($1, 'service:test', 'system', 'channel_turn_refused', 'conversation', 'c-1', 'j1', 'success')",
    )
    .bind(c.t)
    .execute(&c.e.owner)
    .await
    .unwrap();
    sqlx::query("UPDATE tenant_licence_verdict SET checked_at = now() - interval '10 minutes' WHERE tenant_id = $1")
        .bind(c.t)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let v = view(&c, "u-obs").await;
    assert_eq!(v["refusals_24h"], 1);
    assert_eq!(
        (v["gate"]["status"].clone(), v["gate"]["reason"].clone()),
        (json!("inactive"), json!("verdict_stale"))
    );
    sqlx::query("DELETE FROM tenant_licence_verdict WHERE tenant_id = $1")
        .bind(c.t)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let v = view(&c, "u-obs").await;
    assert_eq!(
        (v["gate"]["status"].clone(), v["gate"]["reason"].clone()),
        (json!("none"), json!("verdict_missing"))
    );
}

#[tokio::test]
async fn a_person_outside_the_tenant_is_refused() {
    let c = conv::setup().await;
    let (s, _) = c.get(PATH, "u-nobody").await;
    assert!(s.is_client_error(), "{s}");
}

async fn conversation(c: &Ctx, id: &str, env: &str, channel: &str) {
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(c.t)
    .bind(c.job)
    .bind(id)
    .bind(env)
    .bind(channel)
    .execute(&c.e.owner)
    .await
    .unwrap();
}

#[tokio::test]
async fn conversations_are_counted_once_per_job_kind_env_and_staff_counts_test_only() {
    let c = conv::setup().await;
    // a second prod web chat binding of the same job: conversations name no binding
    let second: Uuid = sqlx::query_scalar(
        "INSERT INTO connector_bindings (tenant_id, job_id, kind, side, external_id, label, created_by, verified_at) \
         VALUES ($1, $2, 'web_chat', 'customer', 'wk_second', 'Second site', 'test', now()) RETURNING id",
    )
    .bind(c.t)
    .bind(c.job)
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    conversation(&c, "wc-1", "prod", "web_chat").await;
    conversation(&c, "wc-2", "prod", "web_chat").await;
    conversation(&c, "st-1", "test", "staff").await;
    conversation(&c, "st-2", "prod", "staff").await;

    let v = view(&c, "u-obs").await;
    let first = channel(&v, &c.wc.to_string()).unwrap();
    let other = channel(&v, &second.to_string()).unwrap();
    assert_eq!(
        first["conversations_7d"], 2,
        "the job's prod web chat count, once"
    );
    assert_eq!(other["conversations_7d"], Value::Null, "not counted twice");
    assert!(
        first["note"]
            .as_str()
            .unwrap()
            .contains("per job, kind and env")
    );
    assert!(
        other["note"]
            .as_str()
            .unwrap()
            .contains("per job, kind and env")
    );
    assert_eq!(
        channel(&v, "ch-staff").unwrap()["conversations_7d"],
        1,
        "the staff chat row is the test env: prod staff conversations are not in it"
    );
}

/// D126: the overview also lists a job's worker-side binding, with `side` "worker" and its `job_key`; the customer
/// rows say "customer".
#[tokio::test]
async fn lists_worker_side_bindings_with_their_side_and_job() {
    let c = conv::setup().await;
    let (s, hd) = c
        .call(
            "POST",
            "/api/jobs/j1/channel-bindings",
            "u-int",
            json!({"kind": "helpdesk", "side": "worker", "external_id": "helpdesk.test#support",
                   "label": "Helpdesk",
                   "settings": {"base_url": "https://helpdesk.example", "service_user": "bot@helpdesk.example"},
                   "credential": {"api_key": "key-test", "api_secret": "secret-test",
                                  "webhook_secret": "test-frappe-webhook-secret"}}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{hd}");
    let v = view(&c, "u-obs").await;
    let w = channel(&v, hd["id"].as_str().unwrap()).expect("the worker binding is listed");
    assert_eq!(
        (w["side"].as_str(), w["job_key"].as_str()),
        (Some("worker"), Some("j1")),
        "{w}"
    );
    assert_eq!(w["kind"], "helpdesk");
    assert!(w["conversations_7d"].is_null());
    let site = channel(&v, &c.wc.to_string()).unwrap();
    assert_eq!(site["side"], "customer");
    assert_eq!(site["job_key"], "j1");
}
