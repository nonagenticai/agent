//! B1 T15: the tenant home (crate::readmodel::home via `GET /api/tenant/home`): attention items and the agents'
//! activity. Real Postgres 18 (one container for this binary).
use crate::common;

use axum::http::StatusCode;
use common::{get, insert_tenant, insert_user, person, send};
use serde_json::{Value, json};
use uuid::Uuid;

async fn home(e: &common::TestEnv) -> Value {
    let (s, b) = send(
        &e.app,
        get("/api/tenant/home", Some(&person("u-admin", &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

async fn tenant(e: &common::TestEnv) -> Uuid {
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;
    t
}

#[tokio::test]
async fn home_attention_lists_failed_provision_and_expired_invite() {
    let e = common::env().await;
    let t = tenant(&e).await;
    let row: Uuid = sqlx::query_scalar(
        "INSERT INTO provision_outbox (tenant_id, target, op, dedupe_key, status, last_error_code, created_by) \
         VALUES ($1, 'strata_rag', 'store_ensure', 'k1', 'dead', 'upstream_5xx', 'test') RETURNING id",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    insert_user(&e.owner, "u-late", t, "tenant_user").await;
    insert_user(&e.owner, "u-fresh", t, "tenant_user").await;
    sqlx::query("UPDATE users SET invited_at = now() - interval '15 days' WHERE sub = 'u-late'")
        .execute(&e.owner)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET invited_at = now() - interval '1 day' WHERE sub = 'u-fresh'")
        .execute(&e.owner)
        .await
        .unwrap();
    let b = home(&e).await;
    let items = b["attention"].as_array().unwrap();
    let kinds: Vec<&str> = items.iter().map(|i| i["kind"].as_str().unwrap()).collect();
    let provision = items
        .iter()
        .find(|i| i["kind"] == "provision")
        .unwrap_or_else(|| panic!("{kinds:?}"));
    assert_eq!(provision["id"], format!("provision:{row}"));
    assert_eq!(provision["severity"], "blocking");
    assert_eq!(items[0]["kind"], "provision", "blocking first: {kinds:?}");
    let invites: Vec<&Value> = items.iter().filter(|i| i["kind"] == "invite").collect();
    assert_eq!(invites.len(), 1, "only the expired invite: {kinds:?}");
    assert_eq!(invites[0]["id"], "invite:u-late");
    assert_eq!(invites[0]["href"], "/admin/users");
    assert!(
        kinds.contains(&"licence"),
        "no tenant licence yet: {kinds:?}"
    );
    assert_eq!(b["activity"]["attributed"], false);
    assert_eq!(b["jobs"], json!([]));
}

#[tokio::test]
async fn home_activity_counts_ai_agent_outcomes() {
    let e = common::env().await;
    let t = tenant(&e).await;
    for (actor_type, status, days) in [
        ("ai_agent", "success", 1),
        ("ai_agent", "success", 2),
        ("ai_agent", "denied", 3),
        ("ai_agent", "error", 1),
        ("ai_agent", "failure", 10),
        ("human", "failure", 1),
    ] {
        sqlx::query(
            "INSERT INTO audit_event (tenant_id, at, actor_id, actor_type, action, resource_type, resource_id, status) \
             VALUES ($1, now() - make_interval(days => $4), 'faq_agent', $2, 'execute', 'operation', 'op-1', $3)",
        )
        .bind(t)
        .bind(actor_type)
        .bind(status)
        .bind(days)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    let b = home(&e).await;
    let a = &b["activity"];
    assert_eq!(
        a["outcomes"],
        json!({"success": 2, "failure": 0, "denied": 1, "error": 1, "fallback": 0}),
        "the last 7 days, agents only"
    );
    let notable: Vec<&str> = a["notable"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["status"].as_str().unwrap())
        .collect();
    assert_eq!(notable, ["error", "denied"], "non-success, newest first");
    assert_eq!(a["daily"].as_array().unwrap().len(), 7);
    assert_eq!(a["turns"], 0);
    assert!(a["notable"][0]["id"].is_string());
}
