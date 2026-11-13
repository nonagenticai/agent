//! B12 handover queue and console (crate::conversations): the queue shape (web `HandoverQueue`), the D64 state machine
//! and its error codes (closed by SW T4's Resolve or T2's Hand back), assignment rules, the one staff-message path (SW T3 Send) delivering through the (fake) edge
//! and keeping the message when delivery fails, handOverCase, and the console codes. Ownership changes go through SW T2's Take /
//! Assign / Hand back (claim, take over and take next are retired). Real Postgres 18.
use crate::common;

use std::sync::atomic::Ordering;

use agent_api::conversations::{handover, note_mirror, store};
use axum::http::StatusCode;
use common::conv::{self, Ctx, code};
use serde_json::{Value, json};
use uuid::Uuid;

/// A conversation on web chat `wc` whose customer asked for a person: (conversation id, item id).
async fn queued(c: &Ctx, wc: &str) -> (String, String) {
    let (s, b) = c
        .inbound(wc, &format!("{wc}-1"), "Can I speak to a real person?")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "queued", "{b}");
    (
        b["conversation_id"].as_str().unwrap().to_string(),
        b["item_id"].as_str().unwrap().to_string(),
    )
}

async fn post(c: &Ctx, path: &str, sub: &str, body: Value) -> (StatusCode, Value) {
    c.call("POST", path, sub, body).await
}

#[tokio::test]
async fn queue_has_the_web_shape() {
    let c = conv::setup().await;
    let (_, item) = queued(&c, "wc-1").await;
    let (s, q) = c.get("/api/jobs/j1/handover", "u-obs").await;
    assert_eq!(s, StatusCode::OK, "{q}");
    assert!(q["now"].is_string() && q["next"].is_null(), "{q}");
    let items = q["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let h = &items[0];
    assert_eq!(h["id"], item.as_str());
    assert_eq!(h["state"], "queued");
    assert_eq!(h["reason"], "customer_asked");
    assert_eq!(h["channel"], "web_chat");
    assert_eq!(h["case_ref"], "CS-1");
    assert_eq!(h["assignee"], Value::Null);
    for k in [
        "case_type",
        "subject",
        "customer",
        "order_id",
        "trigger",
        "priority",
        "queued_at",
        "first_response_at",
        "closed_at",
        "first_response_due",
        "resolve_due",
        "assignee_name",
        "release_id",
        "queue",
        "confidence",
        "agent_did",
        "collected",
        "suggested_next",
        "proposal_hint",
        "feedback",
    ] {
        assert!(h.get(k).is_some(), "missing {k}: {h}");
    }
    let hist = h["history"].as_array().unwrap();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0]["from"], Value::Null);
    assert_eq!(hist[0]["to"], "queued");
    assert_eq!(hist[0]["actor"], "agent");
    assert_eq!(h["messages"][0]["from"], "customer");
    let subs: Vec<&str> = q["assignees"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["sub"].as_str().unwrap())
        .collect();
    assert_eq!(
        subs,
        ["u-admin", "u-agent", "u-agent2", "u-mgr"],
        "support_agent and support_manager holders only"
    );
    // filters
    let (_, q) = c.get("/api/jobs/j1/handover?state=resolved", "u-obs").await;
    assert_eq!(q["items"].as_array().unwrap().len(), 0);
    let (_, q) = c
        .get("/api/jobs/j1/handover?state=open&assignee=none", "u-obs")
        .await;
    assert_eq!(q["items"].as_array().unwrap().len(), 1);
    let (s, q) = c.get("/api/jobs/j1/handover?state=bogus", "u-obs").await;
    assert_eq!(
        (s, code(&q)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_query")
    );
}

#[tokio::test]
async fn transitions_and_their_codes() {
    // the free state route is retired (SW T4): items close by Resolve or Hand back, and the customer's answer moves
    // `waiting_customer → assigned` by itself (tests/b4/handover.rs `customer_reply_moves_waiting_to_assigned`)
    let c = conv::setup().await;
    let (conv_id, item) = queued(&c, "wc-1").await;
    // a queued item cannot be resolved before it is taken (the old `assign_first`)
    let (s, b) = c
        .resolve(&item, "u-agent", json!({"outcome": "resolved"}))
        .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "invalid_transition"));
    let (s, b) = c.assign(&conv_id, "u-mgr", "u-obs", "queue").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "not_assignable")
    );
    let (s, b) = c.assign(&conv_id, "u-agent", "u-agent", "queue").await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], "support_manager");
    let (s, b) = c.assign(&conv_id, "u-mgr", "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "user:u-agent", "{b}");
    let (_, q) = c.get("/api/jobs/j1/handover", "u-obs").await;
    let h = &q["items"][0];
    assert_eq!(
        (h["state"].as_str(), h["assignee"].as_str()),
        (Some("assigned"), Some("u-agent"))
    );
    assert_eq!(h["history"][1]["assignee"], "u-agent");
    // the queue no longer owns it: a second Assign expecting the queue is refused
    let (s, b) = c.assign(&conv_id, "u-mgr", "u-agent", "queue").await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "owner_changed"));
    let (s, b) = c
        .resolve(&item, "u-agent", json!({"outcome": "fixed"}))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_outcome")
    );
    let (s, b) = c.hand_back(&conv_id, "u-agent", " ").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "note_required")
    );
    let (s, b) = c
        .resolve("HO-99", "u-agent", json!({"outcome": "resolved"}))
        .await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "case_not_found"));
    let (s, b) = c.take(&conv_id, "u-agent2", "queue").await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "owner_changed"));
    let (s, b) = c.hand_back(&conv_id, "u-agent", "use the FAQ").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "agent", "{b}");
    let h = c.item(&item).await;
    assert_eq!(h["state"], "returned_to_agent", "{h}");
    assert!(h["closed_at"].is_string(), "{h}");
    assert_eq!(c.texts("SELECT status FROM agent_case").await, ["open"]);
    // closed: neither its last owner nor anyone else resolves it now
    let (s, b) = c
        .resolve(&item, "u-agent", json!({"outcome": "resolved"}))
        .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "closed"));
    // returned to the agent: it answers the next message (D109: the runtime; this job has no live release)
    let (s, b) = c.inbound("wc-1", "wc-1-2", "ok?").await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "job_not_live"), "{b}");
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event \
          WHERE action IN ('conversation.assign', 'conversation.hand_back', 'conversation.resolve')",
    )
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(n, 2);
}

/// The unassigned queued item to take next (`handover::next_unassigned`, the queue order Take next used).
async fn next_unassigned(c: &Ctx) -> Option<String> {
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    let it = handover::next_unassigned(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    it.map(|i| i.id)
}

#[tokio::test]
async fn next_unassigned_order_is_priority_then_age() {
    let c = conv::setup().await;
    let (first_conv, first) = queued(&c, "wc-1").await;
    let (second_conv, second) = queued(&c, "wc-2").await;
    sqlx::query("UPDATE agent_handover_items SET priority = 'high' WHERE id = $1")
        .bind(&second)
        .execute(&c.e.owner)
        .await
        .unwrap();
    assert_eq!(
        next_unassigned(&c).await.as_deref(),
        Some(second.as_str()),
        "high priority first"
    );
    let (s, b) = c.take(&second_conv, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(next_unassigned(&c).await.as_deref(), Some(first.as_str()));
    let (s, b) = c.take(&first_conv, "u-agent2", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "user:u-agent2");
    assert_eq!(next_unassigned(&c).await, None, "nothing left unassigned");
}

#[tokio::test]
async fn reply_is_stored_and_delivered_through_the_edge() {
    let c = conv::setup().await;
    let (conv_id, item) = queued(&c, "wc-1").await;
    let (s, b) = c
        .send_message(&conv_id, "u-agent", "reply", "Hello", None)
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "not_owner"));
    c.take(&conv_id, "u-agent", "queue").await;
    let (s, b) = c
        .send_message(&conv_id, "u-agent", "reply", "  ", None)
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "empty_message")
    );
    let (s, b) = c
        .send_message(&conv_id, "u-agent2", "reply", "Hi", None)
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "not_owner"));
    let (s, b) = c
        .send_message(&conv_id, "u-agent", "reply", "Hi, Anna here.", None)
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["kind"].as_str(), b["delivery"].as_str()),
        (Some("staff"), Some("delivered")),
        "{b}"
    );
    let b = queue_item(&c, &item).await;
    assert_eq!(b["state"], "waiting_customer");
    assert!(b["first_response_at"].is_string());
    let last = b["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        (last["from"].as_str(), last["text"].as_str()),
        (Some("staff"), Some("Hi, Anna here."))
    );
    let d = c.edge.delivered();
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0]["conversation"]["id"], "wc-1");
    assert_eq!(d[0]["conversation"]["channel"], "web_chat");
    assert_eq!(
        d[0]["message"],
        json!({"kind": "text", "text": "Hi, Anna here."})
    );
    assert_eq!(c.edge.unauthorized.load(Ordering::SeqCst), 0);
    // the customer answers: back to assigned
    c.inbound("wc-1", "wc-1-2", "thanks, and?").await;
    let (_, q) = c.get("/api/jobs/j1/handover", "u-obs").await;
    assert_eq!(q["items"][0]["state"], "assigned");
    let audit: Vec<Value> = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE action = 'conversation.staff_reply'",
    )
    .fetch_all(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["conversation_id"], conv_id.as_str());
    assert!(!audit[0].to_string().contains("Anna"), "no text in audit");
}

/// The customer sees the staff member's display name on a staff reply (web chat shows it), never an email or the
/// Keycloak sub: a member without a display name is named by its email in the console, and is sent without a name.
#[tokio::test]
async fn a_staff_reply_carries_the_display_name_never_the_email() {
    let c = conv::setup().await;
    let (conv_id, _) = queued(&c, "wc-1").await;
    c.take(&conv_id, "u-agent", "queue").await;
    named(&c, Some("Mara")).await;
    let (s, b) = c
        .send_message(&conv_id, "u-agent", "reply", "Hi, Mara here.", None)
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    named(&c, None).await;
    let (s, b) = c
        .send_message(&conv_id, "u-agent", "reply", "Still here.", None)
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let d = c.edge.delivered();
    assert_eq!(d.len(), 2, "{d:?}");
    assert_eq!(
        d[0]["message"],
        json!({"kind": "text", "text": "Hi, Mara here.", "name": "Mara"})
    );
    // the console names this member by its email (agent@alpha.example): the customer gets no name
    assert_eq!(
        d[1]["message"],
        json!({"kind": "text", "text": "Still here."})
    );
}

async fn named(c: &Ctx, name: Option<&str>) {
    sqlx::query("UPDATE users SET display_name = $2 WHERE sub = $1")
        .bind("u-agent")
        .bind(name)
        .execute(&c.e.owner)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_failed_delivery_keeps_the_message() {
    let c = conv::setup().await;
    let (conv_id, _) = queued(&c, "wc-1").await;
    c.take(&conv_id, "u-agent", "queue").await;
    c.edge.fail.store(true, Ordering::SeqCst);
    let (s, b) = c
        .send_message(&conv_id, "u-agent", "reply", "Are you still there?", None)
        .await;
    // SW T3: kept and answered 202 with the failed delivery, not an error
    assert_eq!(
        (s, b["delivery"].as_str()),
        (StatusCode::ACCEPTED, Some("failed")),
        "{b}"
    );
    let staff: Vec<String> = sqlx::query_scalar(
        "SELECT text FROM conversation_message WHERE conversation_id = $1 AND role = 'staff'",
    )
    .bind(&conv_id)
    .fetch_all(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(staff, ["Are you still there?"]);
}

#[tokio::test]
async fn hand_over_and_take_a_case() {
    let c = conv::setup().await;
    let (conv_id, _) = c.customer_case("wc-1", "m1", "Where is my parcel?").await;
    let (s, b) = post(
        &c,
        "/api/jobs/j1/cases/CS-1/handover",
        "u-agent",
        json!({"reason": "angry customer"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "handover");
    assert_eq!(
        c.texts("SELECT reason || ':' || trigger FROM agent_handover_items")
            .await,
        ["staff_decision:manual handover by u-agent: angry customer"]
    );
    let (s, b) = post(
        &c,
        "/api/jobs/j1/cases/CS-1/handover",
        "u-agent",
        json!({"reason": "x"}),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "already_handed_over"));
    let (s, b) = post(
        &c,
        "/api/jobs/j1/cases/CS-9/handover",
        "u-agent",
        json!({"reason": "x"}),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "case_not_found"));
    let (s, b) = c.take(&conv_id, "u-agent2", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "user:u-agent2");
    let (_, d) = c.get("/api/jobs/j1/cases/CS-1", "u-obs").await;
    assert_eq!(d["case"]["assignee"], "u-agent2", "{d}");
    let (s, b) = c.take(&conv_id, "u-agent", "queue").await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "owner_changed"));
    // resolve, then a closed case cannot be handed over
    let (s, b) = c
        .resolve("HO-1", "u-agent2", json!({"outcome": "resolved"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = post(
        &c,
        "/api/jobs/j1/cases/CS-1/handover",
        "u-agent",
        json!({"reason": "x"}),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "case_closed"));
    // resolved: the agent owns it again, nothing is queued
    let (s, b) = c.take(&conv_id, "u-agent", "queue").await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "owner_changed"));
    assert_eq!(b["error"]["owner"]["owner"], "agent", "{b}");
}

#[tokio::test]
async fn console_owner_codes() {
    let c = conv::setup().await;
    let (id, _) = c.customer_case("wc-1", "m1", "Hi").await;
    let (s, page) = c.get("/api/jobs/j1/console/conversations", "u-obs").await;
    assert_eq!(s, StatusCode::OK, "{page}");
    let row = &page["items"][0];
    assert_eq!(row["conversation_id"], id.as_str());
    assert_eq!(row["owner"]["owner"], "agent");
    assert!(
        row.get("control").is_none(),
        "the owner replaces control: {row}"
    );
    assert_eq!(row["last_message"]["role"], "customer");
    assert!(page["next"].is_null());
    let (s, b) = c.send_message(&id, "u-agent", "reply", "hi", None).await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "not_owner"));
    let (s, b) = c.hand_back(&id, "u-agent", "x").await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "not_owner"));
    let (s, b) = c.take(&id, "u-agent", "agent").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = c.take(&id, "u-agent", "user:u-agent").await;
    assert_eq!(s, StatusCode::OK, "idempotent for the owner: {b}");
    let (s, b) = c.take(&id, "u-agent2", "agent").await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "owner_changed"));
    let (s, b) = c.send_message(&id, "u-agent2", "reply", "hi", None).await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "not_owner"));
    let (s, b) = c.send_message(&id, "u-agent", "reply", "", None).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "empty_message")
    );
    let (s, b) = c
        .send_message(&id, "u-agent", "reply", "Taking over, one moment.", None)
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["kind"], "staff");
    let (_, page) = c.get("/api/jobs/j1/console/conversations", "u-obs").await;
    assert_eq!(page["items"][0]["last_message"]["role"], "staff");
    assert_eq!(c.edge.delivered().len(), 1);
    let (s, b) = c.take("conv-nope", "u-agent", "agent").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "conversation_not_found")
    );
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action IN ('conversation.take', 'conversation.staff_reply')",
    )
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(n, 2);
}

#[tokio::test]
async fn concurrent_takes_have_one_winner() {
    // B12 review #2: with no control row yet, FOR UPDATE locked nothing and both takers won
    let c = conv::setup().await;
    for round in 0..5 {
        let (id, _) = c.customer_case(&format!("wc-{round}"), "m1", "Hi").await;
        let (a, b) = tokio::join!(
            c.take(&id, "u-agent", "agent"),
            c.take(&id, "u-agent2", "agent")
        );
        let mut got = [a.0, b.0];
        got.sort();
        assert_eq!(
            got,
            [StatusCode::OK, StatusCode::CONFLICT],
            "round {round}: {a:?} {b:?}"
        );
        let loser = if a.0 == StatusCode::CONFLICT {
            &a.1
        } else {
            &b.1
        };
        assert_eq!(code(loser), "owner_changed");
    }
}

/// A platform super_admin token (`sub` a Keycloak uuid, `preferred_username` `username`).
fn platform_staff(sub: &str, username: &str) -> String {
    common::sign(&json!({
        "sub": sub, "preferred_username": username, "exp": common::in_five_minutes(),
        "iss": common::TEST_ISSUER, "aud": common::TEST_AUDIENCE, "azp": common::TEST_AUDIENCE,
        "resource_access": {common::TEST_AUDIENCE: {"roles": ["super_admin"]}}
    }))
}

/// Item `item` as the queue shows it (`HandoverItem`).
async fn queue_item(c: &Ctx, item: &str) -> Value {
    let (_, q) = c.get("/api/jobs/j1/handover", "u-obs").await;
    q["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == item)
        .cloned()
        .unwrap_or_else(|| panic!("{item} not in {q}"))
}

/// `method path` with `token`, acting in the test tenant.
async fn as_token(
    c: &Ctx,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let mut r = common::json(method, path, Some(token), body);
    r.headers_mut()
        .insert("x-active-tenant-id", c.t.to_string().parse().unwrap());
    common::send(&c.e.app, r).await
}

#[tokio::test]
async fn super_admin_staff_shows_preferred_username_not_sub() {
    // B13 live acceptance: a super_admin acting in a tenant has no `users` row there and showed as its Keycloak sub
    let c = conv::setup().await;
    let sub = uuid::Uuid::new_v4().to_string();
    let tok = platform_staff(&sub, "george");
    let (conv_id, item) = queued(&c, "wc-sa").await;
    let (s, b) = as_token(
        &c,
        "POST",
        &format!("/api/jobs/j1/conversations/{conv_id}/take"),
        &tok,
        json!({"expected_owner": "queue"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["item_id"], item.as_str());
    assert_eq!(b["owner"], format!("user:{sub}"));
    let b = queue_item(&c, &item).await;
    assert_eq!(b["assignee"], sub.as_str());
    assert_eq!(b["assignee_name"], "george (platform)", "{b}");
    let took = b["history"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["actor"] == sub.as_str())
        .cloned()
        .expect("a History line by the super_admin");
    assert_eq!(took["actor_name"], "george (platform)");
    assert_eq!(took["note"], "george (platform) took it");
    let (s, b) = as_token(
        &c,
        "POST",
        &format!("/api/jobs/j1/conversations/{conv_id}/messages"),
        &tok,
        json!({"kind": "reply", "text": "Hello from the platform."}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["author_name"], "george (platform)", "{b}");
    let last = queue_item(&c, &item).await["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["author"], "george (platform)", "{last}");
    // a member reading the queue and the transcript sees the name, never the sub
    let (_, q) = c.get("/api/jobs/j1/handover", "u-obs").await;
    let it = q["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == item.as_str())
        .cloned()
        .unwrap();
    assert_eq!(it["assignee_name"], "george (platform)");
    let (s, t) = c
        .get(&format!("/api/jobs/j1/conversations/{conv_id}"), "u-obs")
        .await;
    assert_eq!(s, StatusCode::OK, "{t}");
    assert!(!t["messages"].to_string().contains(&sub), "{t}");
    let stored: String = sqlx::query_scalar("SELECT name FROM actor_names WHERE sub = $1")
        .bind(&sub)
        .fetch_one(&c.e.owner)
        .await
        .unwrap();
    assert_eq!(stored, "george (platform)");
}

#[tokio::test]
async fn member_name_still_wins() {
    // a super_admin who is also a member of the tenant is named by its `users` row
    let c = conv::setup().await;
    let sub = uuid::Uuid::new_v4().to_string();
    common::insert_user(&c.e.owner, &sub, c.t, "support_agent").await;
    sqlx::query("UPDATE users SET display_name = 'Anna Member' WHERE sub = $1")
        .bind(&sub)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let tok = platform_staff(&sub, "anna.sa");
    let (conv_id, item) = queued(&c, "wc-mem").await;
    let (s, b) = as_token(
        &c,
        "POST",
        &format!("/api/jobs/j1/conversations/{conv_id}/take"),
        &tok,
        json!({"expected_owner": "queue"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["item_id"], item.as_str());
    assert_eq!(b["name"], "Anna Member", "{b}");
    let b = queue_item(&c, &item).await;
    assert_eq!(b["assignee_name"], "Anna Member", "{b}");
    assert!(
        b["history"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|h| h["actor"] == sub.as_str())
            .all(|h| h["actor_name"] == "Anna Member"),
        "{b}"
    );
    // even with a remembered platform name for the same sub
    sqlx::query("SELECT agent_actor_name_seen($1, 'anna.sa (platform)')")
        .bind(&sub)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let (_, q) = c.get("/api/jobs/j1/handover", "u-obs").await;
    let it = q["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == item.as_str())
        .cloned()
        .unwrap();
    assert_eq!(it["assignee_name"], "Anna Member");
}

/// A worker binding of `kind` on j1 (verified), so a handover is announced to it.
async fn worker_binding(c: &Ctx, kind: &str, settings: Value, credential: Value) {
    let mut body = json!({"kind": kind, "side": "worker", "external_id": format!("{kind}.test#support"),
                          "label": kind, "settings": settings});
    if !credential.is_null() {
        body["credential"] = credential;
    }
    let (s, b) = c
        .call("POST", "/api/jobs/j1/channel-bindings", "u-int", body)
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    c.verify_all().await;
}

async fn wait_worker_thread(c: &Ctx) {
    for _ in 0..200 {
        if !c
            .texts("SELECT external_id FROM channel_conversation_refs WHERE side = 'worker'")
            .await
            .is_empty()
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("no worker thread was stored");
}

/// A verified helpdesk worker binding on j1: a handover opens a ticket (`thread-{item}`) and notes mirror to it.
async fn helpdesk(c: &Ctx) {
    worker_binding(
        c,
        "helpdesk",
        json!({"base_url": "https://helpdesk.example", "service_user": "bot@helpdesk.example"}),
        json!({"api_key": "key-test", "api_secret": "secret-test", "webhook_secret": "wh-secret-test-0123456789"}),
    )
    .await;
}

/// A queued conversation on a helpdesk worker binding whose ticket exists: (conversation id, item id).
async fn ticketed(c: &Ctx) -> (String, String) {
    helpdesk(c).await;
    let (conv_id, item) = queued(c, "wc-1").await;
    wait_worker_thread(c).await;
    (conv_id, item)
}

/// A staff note on `conv_id` as `u-agent`: its message id.
async fn note(c: &Ctx, conv_id: &str, text: &str) -> Uuid {
    let (s, b) = c.send_message(conv_id, "u-agent", "note", text, None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["message_id"].as_str().unwrap().parse().unwrap()
}

/// `(mirror_state, mirror_attempts, mirror_unknown, mirror_error, mirror_ticket)` of message `id` (the owner pool).
async fn mirror_row(
    c: &Ctx,
    id: Uuid,
) -> (Option<String>, i32, bool, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT mirror_state, mirror_attempts, mirror_unknown, mirror_error, mirror_ticket \
           FROM conversation_message WHERE message_id = $1",
    )
    .bind(id)
    .fetch_one(&c.e.owner)
    .await
    .unwrap()
}

/// Poll (25 ms steps, at most 5 s) until message `id` matches SQL condition `cond`; the fast path runs off the request.
async fn wait_mirror(c: &Ctx, id: Uuid, cond: &str) {
    for _ in 0..200 {
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM conversation_message WHERE message_id = $1 AND ({cond})"
        )))
        .bind(id)
        .fetch_one(&c.e.owner)
        .await
        .unwrap();
        if n == 1 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!(
        "note {id} never reached `{cond}`: {:?}",
        mirror_row(c, id).await
    );
}

/// The fast path (or a sweep) finished and left the note waiting for its next attempt.
const RESTING: &str = "mirror_state = 'pending' AND mirror_claim IS NULL AND mirror_due_at > now() + interval '5 seconds'";

/// Make the pending note `id` due now (the owner pool bypasses RLS).
async fn force_due(c: &Ctx, id: Uuid) {
    sqlx::query(
        "UPDATE conversation_message SET mirror_due_at = now() WHERE message_id = $1 AND mirror_state = 'pending'",
    )
    .bind(id)
    .execute(&c.e.owner)
    .await
    .unwrap();
}

/// Force note `id` due and run one sweep over it.
async fn sweep(c: &Ctx, id: Uuid) {
    force_due(c, id).await;
    note_mirror::sweep_once(&c.e.state).await.unwrap();
}

/// The `mirror` of note `id` in the conversation transcript.
async fn transcript_mirror(c: &Ctx, conv_id: &str, id: Uuid) -> Value {
    let (s, b) = c
        .get(&format!("/api/jobs/j1/conversations/{conv_id}"), "u-obs")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["message_id"] == id.to_string())
        .unwrap_or_else(|| panic!("{id} not in {b}"))["mirror"]
        .clone()
}

/// Take the helpdesk ref of the worker thread out; the rows as they were, to put back with [`ticket_back`].
async fn ticket_aside(c: &Ctx) -> Vec<Value> {
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT to_jsonb(r) FROM channel_conversation_refs r WHERE r.side = 'worker' AND r.channel = 'helpdesk'",
    )
    .fetch_all(&c.e.owner)
    .await
    .unwrap();
    assert!(!rows.is_empty(), "the worker thread has a helpdesk ref");
    sqlx::query(
        "DELETE FROM channel_conversation_refs WHERE side = 'worker' AND channel = 'helpdesk'",
    )
    .execute(&c.e.owner)
    .await
    .unwrap();
    rows
}

/// Put the helpdesk refs [`ticket_aside`] took out back as they were.
async fn ticket_back(c: &Ctx, rows: &[Value]) {
    for r in rows {
        sqlx::query(
            "INSERT INTO channel_conversation_refs \
             SELECT * FROM jsonb_populate_record(NULL::channel_conversation_refs, $1)",
        )
        .bind(r)
        .execute(&c.e.owner)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn note_mirrors_to_helpdesk_ticket() {
    let c = conv::setup().await;
    let (conv_id, item) = ticketed(&c).await;
    let id = note(&c, &conv_id, "check the VAT number").await;
    // the mirror runs off the request: wait for it to settle
    wait_mirror(&c, id, "mirror_state = 'sent'").await;
    let n = c.edge.notes();
    assert_eq!(n.len(), 1, "{n:?}");
    assert_eq!(n[0]["thread"]["channel"], "helpdesk");
    assert_eq!(n[0]["thread"]["id"], format!("thread-{item}"));
    assert_eq!(n[0]["check_first"], false, "{n:?}");
    assert!(n[0]["binding_id"].is_string());
    let text = n[0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("Note from ") && text.ends_with(": check the VAT number"),
        "{text}"
    );
    assert!(
        c.edge.delivered().is_empty(),
        "a note never reaches the customer's channel: {:?}",
        c.edge.delivered()
    );
    let (state, attempts, unknown, error, ticket) = mirror_row(&c, id).await;
    let b = format!("{state:?} {attempts} {unknown} {error:?} {ticket:?}");
    assert_eq!(state.as_deref(), Some("sent"), "{b}");
    assert_eq!(attempts, 1, "{b}");
    assert_eq!(ticket, Some(format!("thread-{item}")), "{b}");
    assert_eq!(error, None, "{b}");
    assert_eq!(transcript_mirror(&c, &conv_id, id).await, "sent");
}

/// Frappe down (the edge answers 502): the note stays queued, is tried again by the sweeper, and is posted once.
#[tokio::test]
async fn frappe_down_retries_until_sent() {
    let c = conv::setup().await;
    let (conv_id, _item) = ticketed(&c).await;
    c.edge.note_answers(&[502, 502]);
    let id = note(&c, &conv_id, "call back tomorrow").await;
    wait_mirror(&c, id, RESTING).await;
    let b = format!("{:?}", mirror_row(&c, id).await);
    assert_eq!(c.edge.notes().len(), 1, "{b}");
    assert_eq!(mirror_row(&c, id).await.1, 1, "{b}");
    assert_eq!(
        mirror_row(&c, id).await.3.as_deref(),
        Some("delivery_failed"),
        "the last error is kept while it retries: {b}"
    );
    assert_eq!(transcript_mirror(&c, &conv_id, id).await, "pending");
    sweep(&c, id).await;
    wait_mirror(&c, id, RESTING).await;
    assert_eq!(c.edge.notes().len(), 2);
    sweep(&c, id).await;
    let n = c.edge.notes();
    let (state, attempts, unknown, error, ticket) = mirror_row(&c, id).await;
    let b = format!("{state:?} {attempts} {unknown} {error:?} {ticket:?}");
    assert_eq!(n.len(), 3, "{n:?} {b}");
    assert_eq!(state.as_deref(), Some("sent"), "{b}");
    assert_eq!(attempts, 3, "{b}");
    assert_eq!(error, None, "a sent note carries no error: {b}");
    assert!(ticket.is_some(), "{b}");
    assert_eq!(transcript_mirror(&c, &conv_id, id).await, "sent");
}

/// A timeout (504) after the request left: the next attempt asks the edge to look the comment up before posting.
#[tokio::test]
async fn an_unknown_outcome_looks_the_comment_up_before_posting_again() {
    let c = conv::setup().await;
    let (conv_id, _item) = ticketed(&c).await;
    c.edge.note_answers(&[504]);
    let id = note(&c, &conv_id, "refund approved").await;
    wait_mirror(&c, id, RESTING).await;
    let (state, _, unknown, error, _) = mirror_row(&c, id).await;
    let b = format!("{state:?} {unknown} {error:?}");
    assert_eq!(state.as_deref(), Some("pending"), "{b}");
    assert!(unknown, "{b}");
    assert_eq!(error.as_deref(), Some("delivery_unknown"), "{b}");
    sweep(&c, id).await;
    wait_mirror(&c, id, "mirror_state = 'sent'").await;
    let n = c.edge.notes();
    assert_eq!(n.len(), 2, "{n:?}");
    assert_eq!(n[0]["check_first"], false, "{n:?}");
    assert_eq!(n[1]["check_first"], true, "{n:?}");
    assert_eq!(n[0]["text"], n[1]["text"], "the same comment: {n:?}");
}

/// The edge refuses the note (422): never retried, shown as failed.
#[tokio::test]
async fn a_rejected_note_is_failed_and_shown() {
    let c = conv::setup().await;
    let (conv_id, item) = ticketed(&c).await;
    c.edge.note_answers(&[422]);
    let id = note(&c, &conv_id, "too long for the ticket").await;
    wait_mirror(&c, id, "mirror_state = 'failed'").await;
    let (state, attempts, _, error, ticket) = mirror_row(&c, id).await;
    let b = format!("{state:?} {attempts} {error:?} {ticket:?}");
    assert_eq!(error.as_deref(), Some("delivery_rejected"), "{b}");
    assert_eq!(attempts, 1, "{b}");
    assert_eq!(ticket, Some(format!("thread-{item}")), "{b}");
    assert_eq!(transcript_mirror(&c, &conv_id, id).await, "failed");
    // failed is final: a sweep sends nothing more
    note_mirror::sweep_once(&c.e.state).await.unwrap();
    assert_eq!(c.edge.notes().len(), 1);
}

/// A note written before its item's ticket exists waits (not an attempt) and mirrors once the ticket opens.
#[tokio::test]
async fn a_note_waits_for_its_ticket_then_mirrors() {
    let c = conv::setup().await;
    let (conv_id, _item) = ticketed(&c).await;
    let aside = ticket_aside(&c).await;
    let id = note(&c, &conv_id, "waiting for the ticket").await;
    wait_mirror(&c, id, RESTING).await;
    let (state, attempts, _, _, _) = mirror_row(&c, id).await;
    let b = format!("{state:?} {attempts}");
    assert_eq!(state.as_deref(), Some("pending"), "{b}");
    assert_eq!(attempts, 0, "looking for the ticket is not an attempt: {b}");
    assert!(c.edge.notes().is_empty(), "{:?}", c.edge.notes());
    ticket_back(&c, &aside).await;
    sweep(&c, id).await;
    wait_mirror(&c, id, "mirror_state = 'sent'").await;
    assert_eq!(c.edge.notes().len(), 1);
    assert_eq!(transcript_mirror(&c, &conv_id, id).await, "sent");
}

/// No ticket after `TICKET_WAIT`: nothing to mirror to, so the note is skipped.
#[tokio::test]
async fn a_note_whose_ticket_never_opens_is_skipped() {
    let c = conv::setup().await;
    let (conv_id, _item) = ticketed(&c).await;
    let _aside = ticket_aside(&c).await;
    let id = note(&c, &conv_id, "no ticket ever").await;
    wait_mirror(&c, id, RESTING).await;
    sqlx::query(
        "UPDATE conversation_message SET at_ms = at_ms - 16 * 60 * 1000 WHERE message_id = $1",
    )
    .bind(id)
    .execute(&c.e.owner)
    .await
    .unwrap();
    sweep(&c, id).await;
    let (state, _, _, error, _) = mirror_row(&c, id).await;
    let b = format!("{state:?} {error:?}");
    assert_eq!(state.as_deref(), Some("skipped"), "{b}");
    assert_eq!(error.as_deref(), Some("no_ticket"), "{b}");
    assert!(c.edge.notes().is_empty(), "{:?}", c.edge.notes());
    assert_eq!(transcript_mirror(&c, &conv_id, id).await, "skipped");
}

/// Two sweeps over one due note: the claim lets exactly one reach the edge.
#[tokio::test]
async fn two_sweeps_never_post_one_note_twice() {
    let c = conv::setup().await;
    let (conv_id, _item) = ticketed(&c).await;
    c.edge.note_answers(&[502]);
    let id = note(&c, &conv_id, "once only").await;
    wait_mirror(&c, id, RESTING).await;
    assert_eq!(c.edge.notes().len(), 1);
    force_due(&c, id).await;
    let (a, b) = tokio::join!(
        note_mirror::sweep_once(&c.e.state),
        note_mirror::sweep_once(&c.e.state)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(
        a + b,
        1,
        "one sweep claimed the note, the other nothing: {a} {b}"
    );
    let n = c.edge.notes();
    assert_eq!(n.len(), 2, "{n:?}");
    let (state, attempts, _, _, _) = mirror_row(&c, id).await;
    let b = format!("{state:?} {attempts}");
    assert_eq!(state.as_deref(), Some("sent"), "{b}");
    assert_eq!(attempts, 2, "{b}");
}

/// Eight attempts used: the note is failed (`gave_up`), not rescheduled.
#[tokio::test]
async fn an_exhausted_note_is_failed_gave_up() {
    let c = conv::setup().await;
    let (conv_id, _item) = ticketed(&c).await;
    c.edge.note_answers(&[502]);
    let id = note(&c, &conv_id, "last chance").await;
    wait_mirror(&c, id, RESTING).await;
    sqlx::query("UPDATE conversation_message SET mirror_attempts = 7 WHERE message_id = $1")
        .bind(id)
        .execute(&c.e.owner)
        .await
        .unwrap();
    c.edge.note_answers(&[502]);
    sweep(&c, id).await;
    let (state, attempts, _, error, _) = mirror_row(&c, id).await;
    let b = format!("{state:?} {attempts} {error:?}");
    assert_eq!(state.as_deref(), Some("failed"), "{b}");
    assert_eq!(attempts, 8, "{b}");
    assert_eq!(error.as_deref(), Some("gave_up"), "{b}");
    assert_eq!(c.edge.notes().len(), 2);
    assert_eq!(transcript_mirror(&c, &conv_id, id).await, "failed");
}

/// A helpdesk binding but no open handover item (a customer conversation nobody asked a person for): not queued.
#[tokio::test]
async fn a_note_without_an_open_item_is_not_queued() {
    let c = conv::setup().await;
    helpdesk(&c).await;
    let (conv_id, _case) = c.customer_case("wc-1", "wc-1-1", "Hello").await;
    let id = note(&c, &conv_id, "no item yet").await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let b = format!("{:?}", mirror_row(&c, id).await);
    assert_eq!(mirror_row(&c, id).await.0, None, "{b}");
    assert!(c.edge.notes().is_empty(), "{:?}", c.edge.notes());
    assert_eq!(transcript_mirror(&c, &conv_id, id).await, Value::Null);
}
