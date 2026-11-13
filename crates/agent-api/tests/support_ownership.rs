//! Staff Workspace T1 (SW-1 / SW-2, migration 0190): `conversation_control` is the one owner record of a conversation
//! (`agent | queue | staff`), written only by `conversations::ownership::set`; a handover item's assignee and channel
//! are derived by view `agent_handover_items_owned`. T2: the commands Take / Assign / Hand back on it, the one
//! runtime silence rule (owner ≠ agent), and the retired verbs. T4: Resolve (outcome, closing message, feedback; the
//! owner returns to the agent) and the widened feedback route. Real Postgres 18.
use crate::common;

use agent_api::conversations::ownership::{self, Owner};
use agent_api::conversations::store;
use axum::http::StatusCode;
use common::conv::{self, Ctx, code};
use serde_json::{Value, json};

/// A web chat conversation `wc` whose customer asked for a person: (conversation id, item id).
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

/// `ownership::current` of `conversation_id` in j1's scope.
async fn owner_of(c: &Ctx, conversation_id: &str) -> ownership::OwnerState {
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    let s = ownership::current(&mut tx, conversation_id).await.unwrap();
    tx.commit().await.unwrap();
    s
}

#[tokio::test]
async fn every_conversation_has_an_owner_row() {
    let c = conv::setup().await;
    let (conv_id, _) = c.customer_case("wc-1", "wc-1-1", "Hello").await;
    assert_eq!(
        c.texts(&format!(
            "SELECT mode || ':' || COALESCE(by_sub, '-') FROM conversation_control \
              WHERE conversation_id = '{conv_id}'"
        ))
        .await,
        ["agent:-"],
        "a new conversation is born agent-owned"
    );
    let s = owner_of(&c, &conv_id).await;
    assert_eq!(s.owner, Owner::Agent);
    assert_eq!(s.name, None);
    assert!(!ownership::silenced(&s.owner));
}

#[tokio::test]
async fn handover_creation_sets_queue() {
    let c = conv::setup().await;
    let (conv_id, _) = queued(&c, "wc-1").await;
    let s = owner_of(&c, &conv_id).await;
    assert_eq!(s.owner, Owner::Queue);
    assert!(ownership::silenced(&s.owner));
    // nobody holds the queued item
    let (_, q) = c.get("/api/jobs/j1/handover", "u-obs").await;
    assert_eq!(q["items"][0]["assignee"], serde_json::Value::Null, "{q}");
}

#[tokio::test]
async fn staff_mode_requires_by_sub() {
    let c = conv::setup().await;
    let (conv_id, _) = c.customer_case("wc-1", "wc-1-1", "Hello").await;
    for (mode, by, constraint) in [
        ("staff", None, "conversation_control_staff_by_check"),
        (
            "queue",
            Some("u-agent"),
            "conversation_control_staff_by_check",
        ),
        (
            "agent",
            Some("u-agent"),
            "conversation_control_staff_by_check",
        ),
        ("console", None, "conversation_control_mode_check"),
    ] {
        let err = sqlx::query(
            "UPDATE conversation_control SET mode = $2, by_sub = $3 WHERE conversation_id = $1",
        )
        .bind(&conv_id)
        .bind(mode)
        .bind(by)
        .execute(&c.e.owner)
        .await
        .expect_err(mode);
        let db = err.as_database_error().expect("a database error");
        assert_eq!(db.code().as_deref(), Some("23514"), "{mode}: {err}");
        assert_eq!(db.constraint(), Some(constraint), "{mode}: {err}");
    }
    sqlx::query(
        "UPDATE conversation_control SET mode = 'staff', by_sub = 'u-agent' WHERE conversation_id = $1",
    )
    .bind(&conv_id)
    .execute(&c.e.owner)
    .await
    .expect("a staff owner names its sub");
}

#[tokio::test]
async fn view_derives_assignee_and_channel() {
    let c = conv::setup().await;
    let (conv_id, item) = queued(&c, "wc-1").await;
    // Take writes the owner (ownership::set(User)); the item's assignee is derived from it
    let (s, b) = c.take(&conv_id, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "user:u-agent", "{b}");
    let s = owner_of(&c, &conv_id).await;
    assert_eq!(s.owner, Owner::User("u-agent".into()));
    assert!(s.name.is_some());
    let view = c
        .texts(&format!(
            "SELECT o.assignee || '|' || o.channel || '|' || v.channel \
               FROM agent_handover_items_owned o \
               JOIN conversation v ON v.tenant_id = o.tenant_id AND v.job_id = o.job_id \
                                  AND v.conversation_id = o.conversation_id \
              WHERE o.id = '{item}' AND o.tenant_id = '{}'",
            c.t
        ))
        .await;
    assert_eq!(view, ["u-agent|web_chat|web_chat"]);
    // the base table carries neither column any more
    assert_eq!(
        c.texts(
            "SELECT column_name::text FROM information_schema.columns \
              WHERE table_schema = 'public' AND table_name = 'agent_handover_items' \
                AND column_name IN ('assignee', 'channel')"
        )
        .await,
        Vec::<String>::new()
    );
    // a close (Hand back) hands the conversation back to the agent; the closed item names who closed it
    let (s, b) = c.hand_back(&conv_id, "u-agent", "use the FAQ").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(owner_of(&c, &conv_id).await.owner, Owner::Agent);
    assert_eq!(
        c.texts(&format!(
            "SELECT h.closed_by || '|' || o.assignee FROM agent_handover_items h \
               JOIN agent_handover_items_owned o ON o.tenant_id = h.tenant_id AND o.job_id = h.job_id \
                                                AND o.id = h.id \
              WHERE h.id = '{item}' AND h.tenant_id = '{}'",
            c.t
        ))
        .await,
        ["u-agent|u-agent"]
    );
}

#[test]
fn owner_wire_round_trips() {
    for o in [Owner::Agent, Owner::Queue, Owner::User("a:b".into())] {
        assert_eq!(Owner::parse(&o.wire()), Some(o.clone()), "{o:?}");
    }
    assert_eq!(Owner::Agent.wire(), "agent");
    assert_eq!(Owner::Queue.wire(), "queue");
    assert_eq!(Owner::User("a:b".into()).wire(), "user:a:b");
    assert_eq!(Owner::parse("user:"), None);
    assert_eq!(Owner::parse("staff"), None);
}

// ---- T2: Take / Assign / Hand back ---------------------------------------------------------------------------------

/// The item `item` as `state|assignee` (the derived assignee, `-` for none).
async fn item_state(c: &Ctx, item: &str) -> String {
    c.texts(&format!(
        "SELECT state || '|' || COALESCE(assignee, '-') FROM agent_handover_items_owned \
          WHERE id = '{item}' AND tenant_id = '{}'",
        c.t
    ))
    .await
    .remove(0)
}

#[tokio::test]
async fn take_queued_sets_user_and_assigned() {
    let c = conv::setup().await;
    let (conv_id, item) = queued(&c, "wc-1").await;
    let (s, b) = c.take(&conv_id, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "user:u-agent", "{b}");
    assert_eq!(b["item_id"], item.as_str(), "{b}");
    assert_eq!(b["name"], "u-agent", "{b}");
    assert!(b["since"].is_string(), "{b}");
    assert_eq!(item_state(&c, &item).await, "assigned|u-agent");
    assert_eq!(
        owner_of(&c, &conv_id).await.owner,
        Owner::User("u-agent".into())
    );
    // the event names who took it
    assert_eq!(
        c.texts(&format!(
            "SELECT to_state || ':' || assignee || ':' || actor || ':' || note FROM agent_handover_events \
              WHERE item_id = '{item}' AND assignee IS NOT NULL"
        ))
        .await,
        ["assigned:u-agent:u-agent:u-agent took it"]
    );
    // Take writes exactly one `owner` system line
    let owner_lines = format!(
        "SELECT text FROM conversation_message \
          WHERE conversation_id = '{conv_id}' AND role = 'system' AND author = 'owner'"
    );
    assert_eq!(
        c.texts(&owner_lines).await,
        ["u-agent took this conversation"]
    );
    // taking it again as its owner changes nothing
    let (s, again) = c.take(&conv_id, "u-agent", "user:u-agent").await;
    assert_eq!(s, StatusCode::OK, "{again}");
    assert_eq!(again["since"], b["since"], "{again}");
    assert_eq!(
        c.texts(&owner_lines).await.len(),
        1,
        "a no-op Take writes no line"
    );
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = 'conversation.take'")
            .fetch_one(&c.e.owner)
            .await
            .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn take_agent_owned_creates_item() {
    let c = conv::setup().await;
    let (conv_id, case_id) = c.customer_case("wc-1", "m1", "Where is my parcel?").await;
    assert_eq!(
        c.texts("SELECT count(*)::text FROM agent_handover_items")
            .await,
        ["0"]
    );
    let (s, b) = c.take(&conv_id, "u-agent", "agent").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "user:u-agent", "{b}");
    let item = b["item_id"].as_str().expect("SW-3: an item").to_string();
    assert_eq!(
        c.texts(&format!(
            "SELECT reason || '|' || case_id || '|' || state FROM agent_handover_items WHERE id = '{item}'"
        ))
        .await,
        [format!("staff_decision|{case_id}|assigned")]
    );
    assert_eq!(item_state(&c, &item).await, "assigned|u-agent");
    assert_eq!(
        c.texts(&format!(
            "SELECT status FROM agent_case WHERE case_id = '{case_id}'"
        ))
        .await,
        ["handover"]
    );
    let (_, q) = c.get("/api/jobs/j1/handover?state=open", "u-obs").await;
    assert_eq!(q["items"][0]["id"], item.as_str(), "{q}");
    assert_eq!(q["items"][0]["assignee"], "u-agent", "{q}");
    // recorded as a manual hand-over of the case (the same operation call as `cases/{id}/handover`)
    assert_eq!(
        c.texts(&format!(
            "SELECT operation || '|' || route || '|' || detail FROM operation_call WHERE case_id = '{case_id}'"
        ))
        .await,
        ["case.create{handover}|fallback|manual handover by u-agent"]
    );
    assert_eq!(
        c.texts(&format!(
            "SELECT op_calls::text FROM agent_case WHERE case_id = '{case_id}'"
        ))
        .await,
        ["1"]
    );
    // created already assigned: no worker channel is pinged for it
    assert!(c.edge.notified().is_empty(), "{:?}", c.edge.notified());
}

#[tokio::test]
async fn take_with_stale_expected_is_409() {
    let c = conv::setup().await;
    let (conv_id, item) = queued(&c, "wc-1").await;
    let (s, b) = c.take(&conv_id, "u-agent2", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = c.take(&conv_id, "u-agent", "queue").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "owner_changed"),
        "{b}"
    );
    let now = &b["error"]["owner"];
    assert_eq!(now["owner"], "user:u-agent2", "{b}");
    assert_eq!(now["name"], "u-agent2", "{b}");
    assert_eq!(now["item_id"], item.as_str(), "{b}");
    assert!(now["since"].is_string(), "{b}");
    // an unknown owner form, an unknown conversation
    let (s, b) = c.take(&conv_id, "u-agent", "staff").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_owner"),
        "{b}"
    );
    let (s, b) = c.take("conv-nope", "u-agent", "agent").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "conversation_not_found"),
        "{b}"
    );
    assert_eq!(
        owner_of(&c, &conv_id).await.owner,
        Owner::User("u-agent2".into())
    );
}

#[tokio::test]
async fn take_from_colleague_is_409() {
    // Take is for an agent-owned or queued conversation; moving it between people is Assign… (support_manager)
    let c = conv::setup().await;
    let (conv_id, item) = queued(&c, "wc-1").await;
    let (s, b) = c.take(&conv_id, "u-agent2", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = c.take(&conv_id, "u-agent", "user:u-agent2").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "owner_changed"),
        "{b}"
    );
    assert_eq!(b["error"]["owner"]["owner"], "user:u-agent2", "{b}");
    assert_eq!(b["error"]["owner"]["item_id"], item.as_str(), "{b}");
    assert_eq!(
        owner_of(&c, &conv_id).await.owner,
        Owner::User("u-agent2".into())
    );
    assert_eq!(item_state(&c, &item).await, "assigned|u-agent2");
}

#[tokio::test]
async fn concurrent_take_exactly_one_wins() {
    let c = conv::setup().await;
    for round in 0..5 {
        // a queued conversation, and an agent-owned one (both takers race to create its item)
        let (queued_conv, _) = queued(&c, &format!("wc-q{round}")).await;
        let (agent_conv, _) = c.customer_case(&format!("wc-a{round}"), "m1", "Hi").await;
        for (conv_id, expected) in [(&queued_conv, "queue"), (&agent_conv, "agent")] {
            let (a, b) = tokio::join!(
                c.take(conv_id, "u-agent", expected),
                c.take(conv_id, "u-agent2", expected)
            );
            let mut got = [a.0, b.0];
            got.sort();
            assert_eq!(
                got,
                [StatusCode::OK, StatusCode::CONFLICT],
                "round {round} {expected}: {a:?} {b:?}"
            );
            let (winner, loser) = if a.0 == StatusCode::OK {
                (&a.1, &b.1)
            } else {
                (&b.1, &a.1)
            };
            assert_eq!(code(loser), "owner_changed", "{loser}");
            assert_eq!(loser["error"]["owner"]["owner"], winner["owner"]);
            assert_eq!(
                c.texts(&format!(
                    "SELECT count(*)::text FROM agent_handover_items \
                      WHERE conversation_id = '{conv_id}' AND state NOT IN ('resolved', 'returned_to_agent')"
                ))
                .await,
                ["1"],
                "round {round} {expected}: one open item"
            );
        }
    }
}

#[tokio::test]
async fn hand_back_requires_note() {
    let c = conv::setup().await;
    let (conv_id, _) = queued(&c, "wc-1").await;
    c.take(&conv_id, "u-agent", "queue").await;
    let (s, b) = c.hand_back(&conv_id, "u-agent", "  ").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "note_required"),
        "{b}"
    );
    let (s, b) = c
        .call(
            "POST",
            &format!("/api/jobs/j1/conversations/{conv_id}/hand-back"),
            "u-agent",
            json!({}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "note_required"),
        "{b}"
    );
    assert_eq!(
        owner_of(&c, &conv_id).await.owner,
        Owner::User("u-agent".into())
    );
}

#[tokio::test]
async fn hand_back_unsilences_runtime() {
    let c = conv::setup().await;
    let (conv_id, item) = queued(&c, "wc-1").await;
    let (s, b) = c.take(&conv_id, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // owned by staff: the customer's message goes to the person, no agent turn
    let (s, b) = c.inbound("wc-1", "wc-1-2", "Hello?").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "queued", "{b}");
    assert_eq!(b["item_id"], item.as_str(), "{b}");
    let (s, b) = c.hand_back(&conv_id, "u-agent", "Offer the FAQ link").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "agent", "{b}");
    assert_eq!(b["name"], Value::Null, "{b}");
    assert_eq!(b["item_id"], Value::Null, "{b}");
    // consistent: owner agent, item returned to the agent with the note on its event, a system/hand_back line
    assert_eq!(owner_of(&c, &conv_id).await.owner, Owner::Agent);
    assert_eq!(item_state(&c, &item).await, "returned_to_agent|u-agent");
    assert_eq!(
        c.texts(&format!(
            "SELECT note FROM agent_handover_events WHERE item_id = '{item}' AND to_state = 'returned_to_agent'"
        ))
        .await,
        ["Offer the FAQ link"]
    );
    assert_eq!(
        c.texts(&format!(
            "SELECT author || '|' || text FROM conversation_message \
              WHERE conversation_id = '{conv_id}' AND role = 'system' ORDER BY at_ms, message_id"
        ))
        .await,
        [
            "owner|u-agent took this conversation",
            "hand_back|Handed back by u-agent: Offer the FAQ link"
        ]
    );
    assert_eq!(c.texts("SELECT status FROM agent_case").await, ["open"]);
    // the agent answers the next message (D109: a runtime turn; this job has no live release)
    let (s, b) = c.inbound("wc-1", "wc-1-3", "ok?").await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "job_not_live"), "{b}");
}

#[tokio::test]
async fn non_owner_cannot_hand_back() {
    let c = conv::setup().await;
    let (conv_id, _) = queued(&c, "wc-1").await;
    // the queue owns it: nobody can hand it back
    let (s, b) = c.hand_back(&conv_id, "u-agent", "x").await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "not_owner"), "{b}");
    c.take(&conv_id, "u-agent", "queue").await;
    let (s, b) = c.hand_back(&conv_id, "u-agent2", "x").await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "not_owner"), "{b}");
    let (agent_conv, _) = c.customer_case("wc-2", "m1", "Hi").await;
    let (s, b) = c.hand_back(&agent_conv, "u-agent", "x").await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "not_owner"), "{b}");
    let (s, b) = c.hand_back(&conv_id, "u-agent", "Back to you").await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn legacy_staff_owner_without_item_hands_back() {
    // a pre-SW console takeover: staff-owned, no item
    let c = conv::setup().await;
    let (conv_id, _) = c.customer_case("wc-1", "m1", "Hi").await;
    sqlx::query(
        "UPDATE conversation_control SET mode = 'staff', by_sub = 'u-agent' WHERE conversation_id = $1",
    )
    .bind(&conv_id)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (s, b) = c.hand_back(&conv_id, "u-agent", "All yours").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "agent", "{b}");
    assert_eq!(owner_of(&c, &conv_id).await.owner, Owner::Agent);
    assert_eq!(
        c.texts("SELECT author FROM conversation_message WHERE role = 'system'")
            .await,
        ["hand_back"]
    );
}

/// A platform super_admin token (`sub` a Keycloak uuid, `preferred_username` `username`).
fn platform_staff(sub: &str, username: &str) -> String {
    common::sign(&json!({
        "sub": sub, "preferred_username": username, "exp": common::in_five_minutes(),
        "iss": common::TEST_ISSUER, "aud": common::TEST_AUDIENCE, "azp": common::TEST_AUDIENCE,
        "resource_access": {common::TEST_AUDIENCE: {"roles": ["super_admin"]}}
    }))
}

#[tokio::test]
async fn super_admin_can_take_not_be_assigned() {
    let c = conv::setup().await;
    let sub = uuid::Uuid::new_v4().to_string();
    let tok = platform_staff(&sub, "george");
    let (conv_id, item) = queued(&c, "wc-1").await;
    let mut r = common::json(
        "POST",
        &format!("/api/jobs/j1/conversations/{conv_id}/take"),
        Some(&tok),
        json!({"expected_owner": "queue"}),
    );
    r.headers_mut()
        .insert("x-active-tenant-id", c.t.to_string().parse().unwrap());
    let (s, b) = common::send(&c.e.app, r).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], format!("user:{sub}"), "{b}");
    assert_eq!(b["name"], "george (platform)", "{b}");
    assert_eq!(item_state(&c, &item).await, format!("assigned|{sub}"));
    // never offered to Assign: not a member holding support_agent
    let (other, _) = queued(&c, "wc-2").await;
    let (s, b) = c.assign(&other, "u-mgr", &sub, "queue").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "not_assignable"),
        "{b}"
    );
    assert_eq!(owner_of(&c, &other).await.owner, Owner::Queue);
}

#[tokio::test]
async fn agent_cannot_assign_manager_can() {
    let c = conv::setup().await;
    let (conv_id, item) = queued(&c, "wc-1").await;
    let (s, b) = c.assign(&conv_id, "u-agent", "u-agent", "queue").await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], "support_manager", "{b}");
    // D88: only support_agent holders are assignable
    for who in ["u-obs", "u-int", "u-nobody"] {
        let (s, b) = c.assign(&conv_id, "u-mgr", who, "queue").await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "not_assignable"),
            "{who}: {b}"
        );
    }
    let (s, b) = c.assign(&conv_id, "u-mgr", "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "user:u-agent", "{b}");
    assert_eq!(item_state(&c, &item).await, "assigned|u-agent");
    // a manager may reassign from one agent to another; a stale expectation is 409
    let (s, b) = c.assign(&conv_id, "u-mgr", "u-agent2", "queue").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "owner_changed"),
        "{b}"
    );
    let (s, b) = c
        .assign(&conv_id, "u-mgr", "u-agent2", "user:u-agent")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(item_state(&c, &item).await, "assigned|u-agent2");
    assert_eq!(
        c.texts(&format!(
            "SELECT text FROM conversation_message \
              WHERE conversation_id = '{conv_id}' AND role = 'system' AND author = 'owner' \
              ORDER BY at_ms, message_id"
        ))
        .await,
        [
            "u-mgr assigned this conversation to u-agent",
            "u-mgr assigned this conversation to u-agent2"
        ]
    );
    let (_, q) = c.get("/api/jobs/j1/handover", "u-obs").await;
    let notes: Vec<&str> = q["items"][0]["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["assignee"].as_str().and(h["note"].as_str()))
        .collect();
    assert_eq!(
        notes,
        ["assigned to u-agent", "assigned to u-agent2"],
        "{q}"
    );
}

#[tokio::test]
async fn retired_routes_are_gone() {
    let c = conv::setup().await;
    let (conv_id, item) = queued(&c, "wc-1").await;
    for path in [
        format!("/api/jobs/j1/conversations/{conv_id}/takeover"),
        format!("/api/jobs/j1/conversations/{conv_id}/handback"),
        "/api/jobs/j1/handover/next".to_string(),
        "/api/jobs/j1/cases/CS-1/claim".to_string(),
        format!("/api/jobs/j1/handover/{item}/assign"),
    ] {
        let (s, b) = c.call("POST", &path, "u-admin", json!({})).await;
        assert!(
            s == StatusCode::NOT_FOUND || s == StatusCode::METHOD_NOT_ALLOWED,
            "{path}: {s} {b}"
        );
    }
    // nothing changed hands
    assert_eq!(owner_of(&c, &conv_id).await.owner, Owner::Queue);
}

// ---- T4: Resolve and feedback --------------------------------------------------------------------------------------

/// The frames `rx` receives until `done` holds for them (frames are published after the commit, possibly after the
/// HTTP answer), failing after 5 s with what did arrive.
async fn frames_until(
    rx: &mut tokio::sync::broadcast::Receiver<agent_api::conversations::hub::Frame>,
    done: impl Fn(&[agent_api::conversations::hub::Frame]) -> bool,
) -> Vec<agent_api::conversations::hub::Frame> {
    let mut out = drain(rx);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !done(&out) {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(f)) => out.push(f),
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => panic!("frames never settled: {out:?}"),
        }
    }
    out
}

/// Every frame the hub holds for `rx` now.
fn drain(
    rx: &mut tokio::sync::broadcast::Receiver<agent_api::conversations::hub::Frame>,
) -> Vec<agent_api::conversations::hub::Frame> {
    let mut out = Vec::new();
    while let Ok(f) = rx.try_recv() {
        out.push(f);
    }
    out
}

/// A queued conversation that u-agent took: (conversation id, item id).
async fn taken(c: &Ctx, wc: &str) -> (String, String) {
    let (conv_id, item) = queued(c, wc).await;
    let (s, b) = c.take(&conv_id, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    (conv_id, item)
}

/// The item as `state|resolution|closed_by` (`-` for none).
async fn closed_as(c: &Ctx, item: &str) -> String {
    c.texts(&format!(
        "SELECT state || '|' || COALESCE(resolution, '-') || '|' || COALESCE(closed_by, '-') \
           FROM agent_handover_items WHERE id = '{item}' AND tenant_id = '{}'",
        c.t
    ))
    .await
    .remove(0)
}

/// The audit `details` of `action`, oldest first.
async fn audited(c: &Ctx, action: &str) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE tenant_id = $1 AND action = $2 ORDER BY id",
    )
    .bind(c.t)
    .bind(action)
    .fetch_all(&c.e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn resolve_sets_outcome_closed_by_and_agent_owner() {
    let c = conv::setup().await;
    let (conv_id, item) = taken(&c, "wc-1").await;
    // the outcome is one of three; an unknown item is the UI's 404
    let (s, b) = c
        .resolve(&item, "u-agent", json!({"outcome": "done"}))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_outcome"),
        "{b}"
    );
    let (s, b) = c
        .resolve("HO-99", "u-agent", json!({"outcome": "resolved"}))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "case_not_found"),
        "{b}"
    );
    let mut rx = c.e.state.events.subscribe();
    let (s, b) = c
        .resolve(
            &item,
            "u-agent",
            json!({"outcome": "not_resolvable", "ask_rating": true}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // OwnerView: the agent owns it again (SW-4), no item is open
    assert_eq!(b["owner"], "agent", "{b}");
    assert_eq!(b["name"], Value::Null, "{b}");
    assert_eq!(b["item_id"], Value::Null, "{b}");
    assert!(b["since"].is_string(), "{b}");
    assert_eq!(
        closed_as(&c, &item).await,
        "resolved|not_resolvable|u-agent"
    );
    let o = owner_of(&c, &conv_id).await;
    assert_eq!(o.owner, Owner::Agent);
    assert!(!ownership::silenced(&o.owner));
    assert_eq!(c.texts("SELECT status FROM agent_case").await, ["resolved"]);
    // the closed item names who closed it
    let h = c.item(&item).await;
    assert_eq!(
        (h["state"].as_str(), h["assignee"].as_str()),
        (Some("resolved"), Some("u-agent")),
        "{h}"
    );
    // one system/resolution line in the transcript
    assert_eq!(
        c.texts(&format!(
            "SELECT text FROM conversation_message \
              WHERE conversation_id = '{conv_id}' AND role = 'system' AND author = 'resolution'"
        ))
        .await,
        ["Closed as not resolvable by u-agent."]
    );
    // audited with the item and the outcome
    let a = audited(&c, "conversation.resolve").await;
    assert_eq!(a.len(), 1, "{a:?}");
    assert_eq!(a[0]["item_id"], item.as_str());
    assert_eq!(a[0]["outcome"], "not_resolvable");
    // owner and handover frames after the commit. Frames are published after it, so they may still be arriving
    // (and the Take's own frames may arrive after the subscribe): wait until what the UI ends on is settled — the
    // conversation's newest owner frame by `since` (frames can arrive out of order; clients keep the newest, web
    // src/lib/support/owner.ts) and the item's last handover frame — then assert on those.
    let last_owner = |fs: &[agent_api::conversations::hub::Frame]| {
        fs.iter()
            .filter(|f| f.event == "owner" && f.data["conversation_id"] == conv_id.as_str())
            .max_by_key(|f| f.data["since"].as_str().unwrap_or_default().to_string())
            .map(|f| f.data["owner"].clone())
    };
    let last_handover = |fs: &[agent_api::conversations::hub::Frame]| {
        fs.iter()
            .rev()
            .find(|f| f.event == "handover" && f.data["id"] == item.as_str())
            .map(|f| f.data["state"].clone())
    };
    let frames = frames_until(&mut rx, |fs| {
        last_owner(fs).as_ref().is_some_and(|o| o == "agent")
            && last_handover(fs).as_ref().is_some_and(|s| s == "resolved")
    })
    .await;
    assert_eq!(
        last_owner(&frames),
        Some(serde_json::json!("agent")),
        "{frames:?}"
    );
    assert_eq!(
        last_handover(&frames),
        Some(serde_json::json!("resolved")),
        "{frames:?}"
    );
    // CSAT as this Resolve asked it (`ask_rating`, SW-6)
    let asks = c
        .edge
        .delivered()
        .into_iter()
        .filter(|d| d["message"]["kind"] == "csat_request")
        .count();
    assert_eq!(asks, 1);
    assert!(frames.iter().any(|f| f.event == "csat"), "{frames:?}");
    // closed: a second Resolve is refused and changes nothing
    let (s, b) = c
        .resolve(&item, "u-agent", json!({"outcome": "resolved"}))
        .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "closed"), "{b}");
    assert_eq!(
        closed_as(&c, &item).await,
        "resolved|not_resolvable|u-agent"
    );
    // every outcome is stored as given
    for (wc, outcome, line) in [
        ("wc-2", "resolved", "Resolved by u-agent."),
        ("wc-3", "duplicate", "Closed as a duplicate by u-agent."),
    ] {
        let (conv_id, item) = taken(&c, wc).await;
        let (s, b) = c
            .resolve(&item, "u-agent", json!({"outcome": outcome}))
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(
            closed_as(&c, &item).await,
            format!("resolved|{outcome}|u-agent")
        );
        assert_eq!(
            c.texts(&format!(
                "SELECT text FROM conversation_message \
                  WHERE conversation_id = '{conv_id}' AND role = 'system' AND author = 'resolution'"
            ))
            .await,
            [line]
        );
    }
}

#[tokio::test]
async fn resolve_with_closing_message_delivers_first() {
    use std::sync::atomic::Ordering;
    let c = conv::setup().await;
    let (conv_id, item) = taken(&c, "wc-1").await;
    // over the channel's limit: refused before anything is sent or closed
    let (s, b) = c
        .resolve(
            &item,
            "u-agent",
            json!({"outcome": "resolved", "closing_message": "x".repeat(8001)}),
        )
        .await;
    assert_eq!(
        (s, code(&b), b["error"]["limit"].as_u64()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "message_too_long",
            Some(8000)
        ),
        "{b}"
    );
    assert_eq!(item_state(&c, &item).await, "assigned|u-agent");
    assert!(c.edge.delivered().is_empty());
    let (s, b) = c
        .resolve(
            &item,
            "u-agent",
            json!({"outcome": "resolved", "closing_message": "  Your refund is on its way.  ",
                   "ask_rating": true}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "agent", "{b}");
    // the closing message reached the customer before the CSAT request
    let kinds: Vec<String> = c
        .edge
        .delivered()
        .iter()
        .map(|d| {
            format!(
                "{}:{}",
                d["message"]["kind"].as_str().unwrap_or(""),
                d["message"]["text"].as_str().unwrap_or("")
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "text:Your refund is on its way.".to_string(),
            format!("csat_request:{}", agent_api::conversations::csat::PROMPT),
        ]
    );
    // stored as the owner's reply, delivered, and before the resolution line (uuid v7 ids order a shared ms)
    let rows = c
        .texts(&format!(
            "SELECT role || ':' || author || ':' || (delivered_at IS NOT NULL) FROM conversation_message \
              WHERE conversation_id = '{conv_id}' AND role IN ('staff', 'system') \
                AND author IN ('u-agent', 'resolution') \
              ORDER BY at_ms, message_id"
        ))
        .await;
    assert_eq!(rows, ["staff:u-agent:true", "system:resolution:false"]);
    let (msg_at, res_at): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT at_ms FROM conversation_message WHERE tenant_id = $1 AND role = 'staff'), \
                (SELECT at_ms FROM conversation_message WHERE tenant_id = $1 AND author = 'resolution')",
    )
    .bind(c.t)
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    assert!(msg_at <= res_at, "{msg_at} > {res_at}");
    // the reply counts as the first response
    assert_eq!(
        c.texts(&format!(
            "SELECT (first_response_at IS NOT NULL)::text FROM agent_handover_items WHERE id = '{item}' \
               AND tenant_id = '{}'",
            c.t
        ))
        .await,
        ["true"]
    );
    // a failed closing message does not stop the Resolve: the item resolves, the reply stays `failed` (Retry)
    let (conv2, item2) = taken(&c, "wc-2").await;
    c.edge.fail.store(true, Ordering::SeqCst);
    let mut rx = c.e.state.events.subscribe();
    let (s, b) = c
        .resolve(
            &item2,
            "u-agent",
            json!({"outcome": "resolved", "closing_message": "Closing this now."}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "agent", "{b}");
    assert_eq!(closed_as(&c, &item2).await, "resolved|resolved|u-agent");
    let failed = c
        .texts(&format!(
            "SELECT message_id::text || '|' || (delivered_at IS NULL) || '|' || (delivery_error IS NOT NULL) \
               FROM conversation_message WHERE conversation_id = '{conv2}' AND role = 'staff'"
        ))
        .await;
    assert_eq!(failed.len(), 1, "{failed:?}");
    let (mid, rest) = failed[0].split_once('|').unwrap();
    assert_eq!(rest, "true|true");
    let frames = drain(&mut rx);
    assert!(
        frames
            .iter()
            .any(|f| f.event == "delivery_failed" && f.data["message_id"] == mid),
        "{frames:?}"
    );
    assert!(frames.iter().any(|f| f.event == "owner"), "{frames:?}");
}

#[tokio::test]
async fn resolve_by_non_owner_403() {
    let c = conv::setup().await;
    let (conv_id, item) = taken(&c, "wc-1").await;
    // a colleague: refused, and the closing message is never sent
    let (s, b) = c
        .resolve(
            &item,
            "u-agent2",
            json!({"outcome": "resolved", "closing_message": "Bye!"}),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "not_owner"), "{b}");
    // neither support_agent nor support_manager
    for sub in ["u-obs", "u-int"] {
        let (s, b) = c.resolve(&item, sub, json!({"outcome": "resolved"})).await;
        assert_eq!(
            (s, code(&b), b["error"]["missing_role"].as_str()),
            (StatusCode::FORBIDDEN, "forbidden", Some("support_agent")),
            "{sub}: {b}"
        );
    }
    assert!(c.edge.delivered().is_empty());
    assert_eq!(
        c.texts(&format!(
            "SELECT count(*)::text FROM conversation_message WHERE conversation_id = '{conv_id}' \
               AND role IN ('staff', 'system') AND author <> 'owner'"
        ))
        .await,
        ["0"]
    );
    assert_eq!(item_state(&c, &item).await, "assigned|u-agent");
    assert_eq!(
        owner_of(&c, &conv_id).await.owner,
        Owner::User("u-agent".into())
    );
    assert!(audited(&c, "conversation.resolve").await.is_empty());
}

#[tokio::test]
async fn resolve_records_feedback_once() {
    let c = conv::setup().await;
    let (_, item) = taken(&c, "wc-1").await;
    let (s, b) = c
        .resolve(
            &item,
            "u-agent",
            json!({"outcome": "resolved",
                   "feedback": {"verdict": "agent_should_handle", "note": "  the FAQ covers this  "}}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let f = c.item(&item).await["feedback"].clone();
    assert_eq!(f["verdict"], "agent_should_handle", "{f}");
    assert_eq!(f["note"], "the FAQ covers this");
    assert_eq!(f["by"], "u-agent");
    assert_eq!(f["proposal"]["kind"], "knowledge_gap");
    let a = audited(&c, "handover.feedback").await;
    assert_eq!(a.len(), 1, "{a:?}");
    assert_eq!(a[0]["verdict"], "agent_should_handle");
    assert!(!a[0].to_string().contains("FAQ"), "never the note: {a:?}");
    // a second verdict on the same item is refused
    let (s, b) = c
        .call(
            "POST",
            &format!("/api/jobs/j1/handover/{item}/feedback"),
            "u-agent2",
            json!({"verdict": "right_to_hand_over"}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "feedback_exists"),
        "{b}"
    );
    // a Resolve carrying a second verdict is refused before anything is sent or closed
    let (conv2, item2) = taken(&c, "wc-2").await;
    let (s, b) = c
        .call(
            "POST",
            &format!("/api/jobs/j1/handover/{item2}/feedback"),
            "u-agent2",
            json!({"verdict": "right_to_hand_over"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = c
        .resolve(
            &item2,
            "u-agent",
            json!({"outcome": "resolved", "closing_message": "Bye!",
                   "feedback": {"verdict": "agent_should_handle"}}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "feedback_exists"),
        "{b}"
    );
    assert_eq!(item_state(&c, &item2).await, "assigned|u-agent");
    assert_eq!(
        owner_of(&c, &conv2).await.owner,
        Owner::User("u-agent".into())
    );
    assert!(
        c.edge
            .delivered()
            .iter()
            .all(|d| d["message"]["text"] != "Bye!"),
        "nothing was sent"
    );
    assert_eq!(
        c.item(&item2).await["feedback"]["verdict"],
        "right_to_hand_over"
    );
    assert_eq!(audited(&c, "handover.feedback").await.len(), 2);
}

#[tokio::test]
async fn operator_can_give_feedback_cannot_resolve() {
    let c = conv::setup().await;
    for (sub, role) in [("u-op", "operator"), ("u-des", "job_designer")] {
        common::insert_user(&c.e.owner, sub, c.t, role).await;
    }
    let mut items = Vec::new();
    for wc in ["wc-1", "wc-2", "wc-3", "wc-4"] {
        items.push(taken(&c, wc).await.1);
    }
    // the staff roles that work a conversation give a verdict
    for (item, sub) in items.iter().zip(["u-op", "u-des", "u-mgr", "u-agent2"]) {
        let (s, b) = c
            .call(
                "POST",
                &format!("/api/jobs/j1/handover/{item}/feedback"),
                sub,
                json!({"verdict": "right_to_hand_over", "note": "fine"}),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{sub}: {b}");
        assert_eq!(b["feedback"]["by"], sub, "{b}");
    }
    // anyone else is refused before the body is read
    for sub in ["u-obs", "u-int"] {
        let (s, b) = c
            .call(
                "POST",
                &format!("/api/jobs/j1/handover/{}/feedback", items[0]),
                sub,
                json!({}),
            )
            .await;
        assert_eq!(
            (s, code(&b), b["error"]["missing_role"].as_str()),
            (StatusCode::FORBIDDEN, "forbidden", Some("support_agent")),
            "{sub}: {b}"
        );
    }
    // a staff role with a bad body
    let (s, b) = c
        .call(
            "POST",
            &format!("/api/jobs/j1/handover/{}/feedback", items[0]),
            "u-op",
            json!({}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::BAD_REQUEST, "invalid_body"),
        "{b}"
    );
    // Resolve is the support agent's
    for sub in ["u-op", "u-des"] {
        let (s, b) = c
            .resolve(&items[0], sub, json!({"outcome": "resolved"}))
            .await;
        assert_eq!(
            (s, code(&b), b["error"]["missing_role"].as_str()),
            (StatusCode::FORBIDDEN, "forbidden", Some("support_agent")),
            "{sub}: {b}"
        );
    }
    assert_eq!(item_state(&c, &items[0]).await, "assigned|u-agent");
}

#[tokio::test]
async fn state_route_is_retired() {
    let c = conv::setup().await;
    let (_, item) = taken(&c, "wc-1").await;
    let (s, b) = c
        .call(
            "POST",
            &format!("/api/jobs/j1/handover/{item}/state"),
            "u-admin",
            json!({"to": "resolved"}),
        )
        .await;
    assert!(
        s == StatusCode::NOT_FOUND || s == StatusCode::METHOD_NOT_ALLOWED,
        "{s} {b}"
    );
    assert_eq!(item_state(&c, &item).await, "assigned|u-agent");
}

#[tokio::test]
async fn resolve_queued_item_is_refused() {
    let c = conv::setup().await;
    let (conv_id, item) = queued(&c, "wc-1").await;
    // a queued item is taken first: refused before anything is sent, recorded or closed
    let (s, b) = c
        .resolve(
            &item,
            "u-agent",
            json!({"outcome": "resolved", "closing_message": "Bye!",
                   "feedback": {"verdict": "right_to_hand_over"}}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "invalid_transition"),
        "{b}"
    );
    assert_eq!(item_state(&c, &item).await, "queued|-");
    assert_eq!(owner_of(&c, &conv_id).await.owner, Owner::Queue);
    assert!(c.edge.delivered().is_empty());
    assert_eq!(c.item(&item).await["feedback"], Value::Null);
    assert!(audited(&c, "conversation.resolve").await.is_empty());
    assert!(audited(&c, "handover.feedback").await.is_empty());
}

#[tokio::test]
async fn closing_message_retry_after_resolve_redelivers() {
    use std::sync::atomic::Ordering;
    let c = conv::setup().await;
    let (conv_id, item) = taken(&c, "wc-1").await;
    c.edge.fail.store(true, Ordering::SeqCst);
    let client = uuid::Uuid::new_v4();
    let (s, b) = c
        .resolve(
            &item,
            "u-agent",
            json!({"outcome": "resolved", "closing_message": "Your refund is on its way.",
                   "client_msg_id": client}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "agent", "{b}");
    let rows = c
        .texts(&format!(
            "SELECT message_id::text || '|' || (delivered_at IS NULL) || '|' || (delivery_error IS NOT NULL) \
               FROM conversation_message WHERE conversation_id = '{conv_id}' AND role = 'staff'"
        ))
        .await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (mid, rest) = rows[0].split_once('|').unwrap();
    assert_eq!(rest, "true|true", "the closing message failed");
    c.edge.fail.store(false, Ordering::SeqCst);
    // the agent owns it now: a colleague may not redeliver someone else's reply
    let (s, b) = c
        .send_message(
            &conv_id,
            "u-agent2",
            "reply",
            "Your refund is on its way.",
            Some(client),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "not_owner"), "{b}");
    // its author may
    let before = c.edge.delivered().len();
    let (s, b) = c
        .send_message(
            &conv_id,
            "u-agent",
            "reply",
            "Your refund is on its way.",
            Some(client),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["message_id"].as_str(), b["delivery"].as_str()),
        (Some(mid), Some("delivered")),
        "{b}"
    );
    let d = c.edge.delivered();
    assert_eq!(d.len(), before + 1);
    assert_eq!(d[before]["message"]["text"], "Your refund is on its way.");
    assert_eq!(
        c.texts(&format!(
            "SELECT (delivered_at IS NOT NULL) || '|' || (delivery_error IS NULL) FROM conversation_message \
              WHERE conversation_id = '{conv_id}' AND role = 'staff'"
        ))
        .await,
        ["true|true"]
    );
    // the agent still owns it
    assert_eq!(owner_of(&c, &conv_id).await.owner, Owner::Agent);
}

/// Owner decision 2026-10-26: a `support_manager` works conversations like a `support_agent` (Take, reply, Resolve),
/// so a manager-only member (`u-mgr` holds nothing else) does the whole loop.
#[tokio::test]
async fn manager_only_member_can_take_send_resolve() {
    let c = conv::setup().await;
    let (conv_id, item) = queued(&c, "wc-1").await;
    let (s, b) = c.take(&conv_id, "u-mgr", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "user:u-mgr", "{b}");
    assert_eq!(item_state(&c, &item).await, "assigned|u-mgr");
    let (s, b) = c
        .send_message(&conv_id, "u-mgr", "reply", "On it.", None)
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = c
        .resolve(&item, "u-mgr", json!({"outcome": "resolved"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "agent", "{b}");
    assert!(closed_as(&c, &item).await.starts_with("resolved|"));
    // and Hand back works for a manager-owner too
    let (conv2, _) = queued(&c, "wc-2").await;
    let (s, b) = c.take(&conv2, "u-mgr", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = c
        .call(
            "POST",
            &format!("/api/jobs/j1/conversations/{conv2}/hand-back"),
            "u-mgr",
            json!({"note": "the agent can finish this"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(owner_of(&c, &conv2).await.owner, Owner::Agent);
}

/// A manager-only member is an assignee (Assign target, assignee list); an integrations manager still is not.
#[tokio::test]
async fn manager_only_member_is_assignable() {
    let c = conv::setup().await;
    let (conv_id, item) = queued(&c, "wc-1").await;
    let (s, b) = c.assign(&conv_id, "u-mgr", "u-mgr", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "user:u-mgr", "{b}");
    assert_eq!(item_state(&c, &item).await, "assigned|u-mgr");
    let (s, q) = c.get("/api/jobs/j1/handover", "u-obs").await;
    assert_eq!(s, StatusCode::OK, "{q}");
    let subs: Vec<&str> = q["assignees"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["sub"].as_str().unwrap())
        .collect();
    assert!(subs.contains(&"u-mgr"), "{q}");
    assert!(!subs.contains(&"u-int"), "{q}");
}

/// An operator (a staff role that may write notes and give feedback) still cannot Take, reply or Resolve.
#[tokio::test]
async fn operator_cannot_take() {
    let c = conv::setup().await;
    common::insert_user(&c.e.owner, "u-op", c.t, "operator").await;
    let (conv_id, item) = queued(&c, "wc-1").await;
    let (s, b) = c.take(&conv_id, "u-op", "queue").await;
    assert_eq!(
        (s, code(&b), b["error"]["missing_role"].as_str()),
        (StatusCode::FORBIDDEN, "forbidden", Some("support_agent")),
        "{b}"
    );
    let (s, b) = c.send_message(&conv_id, "u-op", "reply", "Hi", None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    let (s, b) = c
        .resolve(&item, "u-op", json!({"outcome": "resolved"}))
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(owner_of(&c, &conv_id).await.owner, Owner::Queue);
}

#[tokio::test]
async fn take_hand_back_hand_over_and_link_refuse_before_the_body_is_read() {
    let c = conv::setup().await;
    // a well-formed body none of them accepts (a missing or mistyped field)
    let wrong = json!({"note": 7});
    for path in [
        "/api/jobs/j1/conversations/conv-x/take",
        "/api/jobs/j1/conversations/conv-x/hand-back",
        "/api/jobs/j1/cases/CS-1/handover",
        "/api/jobs/j1/cases/CS-1/party",
    ] {
        // neither support_agent nor support_manager: the role, whatever the body
        for sub in ["u-obs", "u-int"] {
            let (s, b) = c.call("POST", path, sub, wrong.clone()).await;
            assert_eq!(
                (s, code(&b), b["error"]["missing_role"].as_str()),
                (StatusCode::FORBIDDEN, "forbidden", Some("support_agent")),
                "{path} {sub}: {b}"
            );
        }
        // a support agent with the same body: the body is still checked
        let (s, b) = c.call("POST", path, "u-agent", wrong.clone()).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::BAD_REQUEST, "invalid_body"),
            "{path}: {b}"
        );
    }
}

/// The conversation's CSAT request as `state|asked_by|asked_by_sub` (`-` for none), one entry per row.
async fn csat_rows(c: &Ctx, conv_id: &str) -> Vec<String> {
    c.texts(&format!(
        "SELECT state || '|' || asked_by || '|' || COALESCE(asked_by_sub, '-') FROM case_csat \
          WHERE conversation_id = '{conv_id}'"
    ))
    .await
}

/// How many CSAT requests reached the customer's channel.
fn csat_deliveries(c: &Ctx) -> usize {
    c.edge
        .delivered()
        .into_iter()
        .filter(|d| d["message"]["kind"] == "csat_request")
        .count()
}

/// A conversation the agent handled and u-agent took (SW-3: a `staff_decision` item, nothing that suppresses CSAT):
/// (conversation id, item id).
async fn taken_from_agent(c: &Ctx, wc: &str, msg: &str) -> (String, String) {
    let (conv_id, _) = c.customer_case(wc, msg, "Where is my parcel?").await;
    let (s, b) = c.take(&conv_id, "u-agent", "agent").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    (conv_id, b["item_id"].as_str().unwrap().to_string())
}

#[tokio::test]
async fn staff_resolve_honours_csat_enabled_and_once_per_conversation() {
    let c = conv::setup().await;
    // a binding with CSAT off is never asked, even when the Resolve says so
    sqlx::query(
        "UPDATE connector_bindings SET settings = '{\"csat_enabled\": false}' WHERE id = $1",
    )
    .bind(c.wc)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (off_conv, off_item) = taken_from_agent(&c, "wc-off", "m1").await;
    let (s, b) = c
        .resolve(
            &off_item,
            "u-agent",
            json!({"outcome": "resolved", "ask_rating": true}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(csat_rows(&c, &off_conv).await.is_empty());
    assert_eq!(csat_deliveries(&c), 0);
    // switched on: the Resolve asks (default: nothing suppresses it)
    sqlx::query("UPDATE connector_bindings SET settings = '{}' WHERE id = $1")
        .bind(c.wc)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let (conv_id, item) = taken_from_agent(&c, "wc-1", "m1").await;
    let (s, b) = c
        .resolve(&item, "u-agent", json!({"outcome": "resolved"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(csat_rows(&c, &conv_id).await, ["asked|staff|u-agent"]);
    assert_eq!(csat_deliveries(&c), 1);
    // a second case of the same conversation resolved: still one request
    let (_, case2) = c.customer_case("wc-1", "m2", "And my other parcel?").await;
    let (s, b) = c.take(&conv_id, "u-agent", "agent").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let item2 = b["item_id"].as_str().unwrap().to_string();
    assert_ne!(case2, "");
    let (s, b) = c
        .resolve(
            &item2,
            "u-agent",
            json!({"outcome": "resolved", "ask_rating": true}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(csat_rows(&c, &conv_id).await.len(), 1);
    assert_eq!(csat_deliveries(&c), 1, "once per conversation");
}

#[tokio::test]
async fn on_email_without_a_closing_message_the_prompt_is_its_own_mail() {
    let c = conv::setup().await;
    let (conv_id, item) = taken_from_agent(&c, "wc-1", "m1").await;
    sqlx::query("UPDATE conversation SET channel = 'email' WHERE conversation_id = $1")
        .bind(&conv_id)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let (s, b) = c
        .resolve(&item, "u-agent", json!({"outcome": "resolved"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(csat_deliveries(&c), 1);
}

#[tokio::test]
async fn resolve_with_ask_rating_false_sends_no_ask() {
    let c = conv::setup().await;
    let (conv_id, item) = taken_from_agent(&c, "wc-1", "m1").await;
    let (s, b) = c
        .resolve(
            &item,
            "u-agent",
            json!({"outcome": "resolved", "ask_rating": false}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(csat_rows(&c, &conv_id).await.is_empty());
    assert_eq!(csat_deliveries(&c), 0);
    assert_eq!(
        c.texts(&format!(
            "SELECT count(*)::text FROM conversation_message \
              WHERE conversation_id = '{conv_id}' AND author = 'csat'"
        ))
        .await,
        ["0"],
        "no prompt line"
    );
}

#[tokio::test]
async fn resolve_defaults_to_no_ask_after_a_complaint_and_asks_after_a_person_was_asked_for() {
    let c = conv::setup().await;
    let (conv_id, item) = taken_from_agent(&c, "wc-1", "m1").await;
    sqlx::query("UPDATE agent_handover_items SET trigger = 'case.create{complaint}' WHERE id = $1")
        .bind(&item)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let (s, b) = c
        .resolve(&item, "u-agent", json!({"outcome": "resolved"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(csat_rows(&c, &conv_id).await.is_empty());
    assert_eq!(csat_deliveries(&c), 0);
    // a customer who asked for a person IS asked by default: the rating measures the staff member's own service
    let (conv2, item2) = taken(&c, "wc-2").await;
    let (s, b) = c
        .resolve(&item2, "u-agent", json!({"outcome": "resolved"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(csat_rows(&c, &conv2).await.len(), 1);
    assert_eq!(csat_deliveries(&c), 1);
}

#[tokio::test]
async fn resolve_records_the_staff_member_as_asker() {
    let c = conv::setup().await;
    let (conv_id, item) = taken_from_agent(&c, "wc-1", "m1").await;
    let (s, b) = c
        .resolve(
            &item,
            "u-agent",
            json!({"outcome": "resolved", "ask_rating": true}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(csat_rows(&c, &conv_id).await, ["asked|staff|u-agent"]);
    assert_eq!(
        c.texts(&format!(
            "SELECT (asked_at IS NOT NULL)::text FROM case_csat WHERE conversation_id = '{conv_id}'"
        ))
        .await,
        ["true"]
    );
}
