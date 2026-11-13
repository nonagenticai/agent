//! D125 (migration 0213, crate::support::job_staff): the staff side is configured per job. A job's team limits which
//! support_agents see (inbox), take, reply to and are auto-assigned its conversations; no team = every worker; a
//! manager sees every job but assigns only to an eligible member; `job_routing.mode` gates auto-assignment per job.
//! Real Postgres 18, the support routing setup (tests/support_routing.rs).
use crate::common;

use std::time::{Duration, Instant};

use agent_api::support::routing;
use axum::http::StatusCode;
use common::conv::{self, Ctx, code};
use serde_json::{Value, json};

async fn flag(c: &Ctx, on: bool) {
    sqlx::query("UPDATE tenants SET support_auto_assign = $2 WHERE id = $1")
        .bind(c.t)
        .bind(on)
        .execute(&c.e.owner)
        .await
        .unwrap();
}

/// A web chat conversation `wc` whose customer asked for a person: the conversation id.
async fn queued(c: &Ctx, wc: &str) -> String {
    let (s, b) = c
        .inbound(wc, &format!("{wc}-1"), "Can I speak to a real person?")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "queued", "{b}");
    b["conversation_id"].as_str().unwrap().to_string()
}

async fn online(c: &Ctx, sub: &str) {
    sqlx::query(
        "INSERT INTO member_session (tenant_id, sub, sid, surface, last_seen_at, support_seen_at) \
         VALUES ($1, $2, $3, 'support', now(), now()) \
         ON CONFLICT (tenant_id, sub, sid) DO UPDATE SET support_seen_at = now()",
    )
    .bind(c.t)
    .bind(sub)
    .bind(format!("sid-{sub}"))
    .execute(&c.e.owner)
    .await
    .unwrap();
}

async fn owner(c: &Ctx, conv: &str) -> String {
    c.texts(&format!(
        "SELECT CASE WHEN mode = 'staff' THEN by_sub ELSE mode END FROM conversation_control \
          WHERE conversation_id = '{conv}'"
    ))
    .await
    .remove(0)
}

async fn await_owner(c: &Ctx, conv: &str, want: &str) {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let got = owner(c, conv).await;
        if got == want {
            return;
        }
        assert!(
            Instant::now() < end,
            "{conv} is owned by {got:?}, want {want:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// `PUT /api/jobs/j1/staff` as `sub`.
async fn put(c: &Ctx, sub: &str, members: &[&str], mode: &str) -> (StatusCode, Value) {
    c.call(
        "PUT",
        "/api/jobs/j1/staff",
        sub,
        json!({"members": members, "routing": {"mode": mode}}),
    )
    .await
}

async fn team(c: &Ctx, members: &[&str]) {
    let (s, b) = put(c, "u-mgr", members, "inherit").await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// The conversation ids of `sub`'s open inbox tab and its `counts.open`.
async fn inbox(c: &Ctx, sub: &str) -> (Vec<String>, i64) {
    let (s, b) = c.get("/api/support/inbox?tab=open", sub).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let ids = b["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["conversation_id"].as_str().unwrap().to_string())
        .collect();
    (ids, b["counts"]["open"].as_i64().unwrap())
}

#[tokio::test]
async fn staff_get_put_validation_and_audit() {
    let c = conv::setup().await;
    // an observer reads the team and routing but not the candidate list (emails, roles)
    let (s, b) = c.get("/api/jobs/j1/staff", "u-obs").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["candidates"], json!([]), "{b}");
    // default: empty team, inherit, every worker is a candidate (the observer is not)
    let (s, b) = c.get("/api/jobs/j1/staff", "u-admin").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["members"], json!([]), "{b}");
    assert_eq!(
        b["routing"],
        json!({"mode": "inherit", "reassign_after_secs": null}),
        "{b}"
    );
    let subs: Vec<&str> = b["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["sub"].as_str().unwrap())
        .collect();
    for want in ["u-agent", "u-agent2", "u-mgr", "u-admin"] {
        assert!(subs.contains(&want), "{want} in {subs:?}");
    }
    assert!(
        !subs.contains(&"u-obs") && !subs.contains(&"u-int"),
        "{subs:?}"
    );
    // a plain agent and an observer may not write
    for sub in ["u-agent", "u-obs"] {
        let (s, b) = put(&c, sub, &["u-agent"], "inherit").await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{sub}: {b}");
        assert_eq!(b["error"]["missing_role"], "support_manager", "{b}");
    }
    // members must be enabled workers
    for bad in ["u-obs", "u-int", "nobody"] {
        let (s, b) = put(&c, "u-mgr", &[bad], "inherit").await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {b}");
        assert_eq!(code(&b), "not_a_worker", "{bad}: {b}");
    }
    let (s, b) = put(&c, "u-mgr", &["u-agent"], "sometimes").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "invalid_staff", "{b}");
    let (s, b) = c
        .call(
            "PUT",
            "/api/jobs/j1/staff",
            "u-mgr",
            json!({"members": [], "routing": {"mode": "off", "reassign_after_secs": 30}}),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    // the manager and the tenant admin write; the team is replaced, not merged
    let (s, b) = put(&c, "u-mgr", &["u-agent", "u-agent2"], "round_robin").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["members"].as_array().unwrap().len(), 2, "{b}");
    let (s, b) = c
        .call(
            "PUT",
            "/api/jobs/j1/staff",
            "u-admin",
            json!({"members": ["u-agent2"], "routing": {"mode": "off", "reassign_after_secs": 120}}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["members"][0]["sub"], "u-agent2", "{b}");
    assert_eq!(b["members"].as_array().unwrap().len(), 1, "{b}");
    assert_eq!(
        b["routing"],
        json!({"mode": "off", "reassign_after_secs": 120}),
        "{b}"
    );
    assert_eq!(
        c.texts("SELECT count(*)::text FROM audit_event WHERE action = 'job.staff_update'")
            .await,
        ["2"]
    );
}

#[tokio::test]
async fn a_team_hides_the_job_from_other_agents() {
    let c = conv::setup().await;
    let conv_id = queued(&c, "wc-1").await;
    // no team: every worker sees it
    for sub in ["u-agent", "u-agent2", "u-mgr"] {
        let (ids, open) = inbox(&c, sub).await;
        assert!(ids.contains(&conv_id), "{sub}: {ids:?}");
        assert_eq!(open, 1, "{sub}");
    }
    team(&c, &["u-agent"]).await;
    let (ids, open) = inbox(&c, "u-agent2").await;
    assert!(ids.is_empty(), "{ids:?}");
    assert_eq!(open, 0, "counts follow the same rule");
    for sub in ["u-agent", "u-mgr", "u-admin"] {
        let (ids, _) = inbox(&c, sub).await;
        assert!(ids.contains(&conv_id), "{sub}: {ids:?}");
    }
    // Take and reply are refused for the agent off the team
    let (s, b) = c.take(&conv_id, "u-agent2", "queue").await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(code(&b), "not_job_staff", "{b}");
    let (s, b) = c
        .send_message(&conv_id, "u-agent2", "reply", "Hello", None)
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(code(&b), "not_job_staff", "{b}");
    assert_eq!(owner(&c, &conv_id).await, "queue");
    // the team member takes it
    let (s, b) = c.take(&conv_id, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // an empty team again = every worker
    team(&c, &[]).await;
    let (ids, _) = inbox(&c, "u-agent2").await;
    assert!(ids.contains(&conv_id), "{ids:?}");
}

#[tokio::test]
async fn a_manager_assigns_only_to_the_jobs_team() {
    let c = conv::setup().await;
    let conv_id = queued(&c, "wc-1").await;
    team(&c, &["u-agent"]).await;
    let (s, b) = c.assign(&conv_id, "u-mgr", "u-agent2", "queue").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "not_job_staff", "{b}");
    assert_eq!(owner(&c, &conv_id).await, "queue");
    let (s, b) = c.assign(&conv_id, "u-mgr", "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(owner(&c, &conv_id).await, "u-agent");
}

#[tokio::test]
async fn the_sweeper_assigns_only_to_the_team_and_honours_the_jobs_mode() {
    let c = conv::setup().await;
    flag(&c, true).await;
    online(&c, "u-agent").await;
    online(&c, "u-agent2").await;
    // team [u-agent2]: every item goes to them, never to the other online agent
    team(&c, &["u-agent2"]).await;
    for i in 1..=3 {
        let conv_id = queued(&c, &format!("wc-{i}")).await;
        await_owner(&c, &conv_id, "u-agent2").await;
    }
    // the team's only member is offline: the item stays queued (u-agent is online but not on the team)
    sqlx::query("DELETE FROM member_session WHERE tenant_id = $1 AND sub = 'u-agent2'")
        .bind(c.t)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let waiting = queued(&c, "wc-4").await;
    let out = routing::route_item(&c.e.state, c.t, c.job, &waiting, None, true)
        .await
        .unwrap();
    assert_eq!(out, routing::Outcome::Skipped);
    assert_eq!(routing::tick(&c.e.state).await.unwrap(), 0);
    assert_eq!(owner(&c, &waiting).await, "queue");
    // mode off on the job stops auto-assign for the job even with the team online and the tenant on round_robin.
    // Off goes on BEFORE the member comes online: the inbound handover's own auto-assign runs in a task spawned after
    // its response (conversations/internal.rs), and with the job still on inherit it would rightly take the item.
    let (s, b) = put(&c, "u-mgr", &["u-agent2"], "off").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    online(&c, "u-agent2").await;
    let out = routing::route_item(&c.e.state, c.t, c.job, &waiting, None, true)
        .await
        .unwrap();
    assert_eq!(out, routing::Outcome::Skipped);
    assert_eq!(routing::tick(&c.e.state).await.unwrap(), 0);
    assert_eq!(owner(&c, &waiting).await, "queue");
    // back to inherit: the sweeper hands it to the team member
    team(&c, &["u-agent2"]).await;
    let end = Instant::now() + Duration::from_secs(5);
    while owner(&c, &waiting).await == "queue" {
        routing::tick(&c.e.state).await.unwrap();
        assert!(Instant::now() < end, "{waiting} is still queued");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(owner(&c, &waiting).await, "u-agent2");
}

/// The handover item ids of `GET /api/jobs/j1/handover` as `sub`.
async fn queue_ids(c: &Ctx, sub: &str) -> Vec<String> {
    let (s, b) = c.get("/api/jobs/j1/handover", sub).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn the_handover_queue_follows_the_team_but_keeps_own_items() {
    let c = conv::setup().await;
    let conv_id = queued(&c, "wc-1").await;
    let item = c
        .texts(&format!(
            "SELECT id FROM agent_handover_items WHERE conversation_id = '{conv_id}'"
        ))
        .await
        .remove(0);
    for sub in ["u-agent", "u-agent2", "u-mgr"] {
        assert!(queue_ids(&c, sub).await.contains(&item), "{sub}: no team");
    }
    team(&c, &["u-agent"]).await;
    // off the team: an empty queue (the inbox's rule); the team and the manager still see it
    assert!(queue_ids(&c, "u-agent2").await.is_empty());
    for sub in ["u-agent", "u-mgr", "u-admin"] {
        assert!(queue_ids(&c, sub).await.contains(&item), "{sub}");
    }
    // an item the agent owns stays listed after they leave the team
    team(&c, &[]).await;
    let (s, b) = c.take(&conv_id, "u-agent2", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    team(&c, &["u-agent"]).await;
    assert!(queue_ids(&c, "u-agent2").await.contains(&item));
}

#[tokio::test]
async fn the_owner_keeps_working_a_conversation_after_leaving_the_team() {
    let c = conv::setup().await;
    let conv_id = queued(&c, "wc-1").await;
    let (s, b) = c.take(&conv_id, "u-agent2", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let item = b["item_id"].as_str().unwrap().to_string();
    team(&c, &["u-agent"]).await;
    // the inbox and its counts still list their own conversation
    let (ids, open) = inbox(&c, "u-agent2").await;
    assert_eq!(ids, std::slice::from_ref(&conv_id), "{ids:?}");
    assert_eq!(open, 1);
    // a note and a reply by the owner pass, and so does a team member's note
    for kind in ["note", "reply"] {
        let (s, b) = c
            .send_message(&conv_id, "u-agent2", kind, "Still here", None)
            .await;
        assert!(s.is_success(), "{kind}: {s} {b}");
    }
    let (s, b) = c
        .send_message(&conv_id, "u-agent", "note", "Mine too", None)
        .await;
    assert!(s.is_success(), "{s} {b}");
    // u-agent2 (the owner, off the team) hands it back
    let (s, b) = c.hand_back(&conv_id, "u-agent2", "Back to you").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // now they own nothing: off the team, every write is refused
    let (s, b) = c
        .send_message(&conv_id, "u-agent2", "note", "Hello?", None)
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(code(&b), "not_job_staff", "{b}");
    let (s, b) = c.hand_back(&conv_id, "u-agent2", "Again").await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(code(&b), "not_job_staff", "{b}");
    let (s, b) = c
        .resolve(&item, "u-agent2", json!({"outcome": "resolved"}))
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(code(&b), "not_job_staff", "{b}");
}

#[tokio::test]
async fn the_owner_off_the_team_resolves_their_item() {
    let c = conv::setup().await;
    let conv_id = queued(&c, "wc-1").await;
    let (s, b) = c.take(&conv_id, "u-agent2", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let item = b["item_id"].as_str().unwrap().to_string();
    team(&c, &["u-agent"]).await;
    // the team rule does not stand in the way of the owner's Resolve
    let (s, b) = c
        .resolve(&item, "u-agent2", json!({"outcome": "resolved"}))
        .await;
    assert_ne!(code(&b), "not_job_staff", "{s} {b}");
    assert_eq!(s, StatusCode::OK, "{b}");
}
