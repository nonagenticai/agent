//! B12 internal door agent-edge → agent-api (crate::conversations::internal): the `EdgeKey` gate (503 without the
//! key, 401 on a mismatch), the cross-tenant binding list with opened credentials, the worker-thread staff door
//! (member resolved by email, 403 `not_staff`), the web chat reconnect read, and the web chat customer stream an edge replica proxies (crate::conversations::webchat_stream). Every
//! call names its binding; agent-api checks the row. SW T5: a worker-thread reply is the workspace's Take and Send
//! (the owner record shows it; a colleague's conversation is 409 `owner_changed`; a closed item's thread is 409
//! `closed`), and the inbound "talk to a person" queues in the commands' lock order. Real Postgres 18.
use crate::common;
use crate::redis_env;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use agent_api::channels::bindings::Binding;
use agent_api::channels::core::{CoreLink, InternalKey};
use agent_api::channels::webchat::{self, WebChat, WebChatConfig};
use agent_api::channels::{ChannelKind, EdgeState, Registry, Side, router};
use agent_api::conversations::edge_link::DeliverKind;
use agent_api::conversations::ownership::{self, Owner};
use agent_api::conversations::{staff, store};
use agent_api::interaction::{Scope, Unconfigured};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::conv::{self, Ctx, EDGE_KEY, code};
use common::send;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const CRED: &str = "123456789:test-not-a-real-bot-token-000000";
const WEBHOOK: &str = "test-webhook-secret-0001";

fn get_with(path: &str, auth: Option<&str>) -> Request<Body> {
    let mut b = Request::get(path);
    if let Some(a) = auth {
        b = b.header("authorization", a);
    }
    b.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn the_door_is_key_gated() {
    let off = common::env().await;
    let (s, b) = send(
        &off.app,
        get_with(
            "/api/internal/channel/bindings",
            Some(&format!("Bearer {EDGE_KEY}")),
        ),
    )
    .await;
    assert_eq!(
        (s, conv::code(&b)),
        (StatusCode::SERVICE_UNAVAILABLE, "edge_not_configured")
    );
    let c = conv::setup().await;
    for auth in [None, Some("Bearer nope"), Some(EDGE_KEY)] {
        let (s, b) = send(&c.e.app, get_with("/api/internal/channel/bindings", auth)).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{auth:?}: {b}");
    }
    let (s, b) = send(
        &c.e.app,
        get_with(
            "/api/internal/channel/bindings",
            Some(&format!("Bearer {EDGE_KEY}")),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // a person's token is not the edge key
    let (s, _) = send(
        &c.e.app,
        common::json(
            "POST",
            "/api/internal/channel/inbound",
            Some(&common::person("u-admin", &[])),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

/// Create a binding as u-int in `job`: its JSON.
async fn create(c: &Ctx, job: &str, sub: &str, body: Value) -> Value {
    let (s, b) = c
        .call(
            "POST",
            &format!("/api/jobs/{job}/channel-bindings"),
            sub,
            body,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    // D130: a customer binding starts in the test env; this file's traffic runs the live path
    c.go_live().await;
    b
}

#[tokio::test]
async fn a_telegram_binding_is_served_to_the_edge_with_its_opened_credential() {
    let c = conv::setup().await;
    let b = create(
        &c,
        "j1",
        "u-int",
        json!({"kind": "telegram", "side": "customer", "label": "Bot",
               "credential": {"bot_token": CRED, "webhook_secret": WEBHOOK}}),
    )
    .await;
    assert_eq!(
        b["verified"], true,
        "holding the bot token proves the account"
    );
    let (_, l) = c.internal_get("/api/internal/channel/bindings").await;
    let tg = l["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["external_id"] == "123456789")
        .unwrap()
        .clone();
    assert_eq!(tg["tenant"], c.t.to_string());
    assert_eq!(tg["job_key"], "j1");
    assert_eq!(tg["credential"]["bot_token"], CRED);
}

/// A verified helpdesk worker binding, then a conversation that asked for a person (worker thread `thread-HO-1`).
async fn worker_thread(c: &Ctx) -> Uuid {
    let b = create(
        c,
        "j1",
        "u-int",
        json!({"kind": "helpdesk", "side": "worker", "external_id": "helpdesk.test#support", "label": "Ops",
               "settings": {"base_url": "https://helpdesk.example", "service_user": "bot@helpdesk.example"},
               "credential": {"api_key": "key-test", "api_secret": "secret-test",
                              "webhook_secret": "wh-secret-test-0123456789"}}),
    )
    .await;
    c.verify_all().await;
    let (_, r) = c.inbound("wc-1", "m1", "I want a real person").await;
    assert_eq!(r["item_id"], "HO-1", "{r}");
    wait_worker_ref(c).await;
    Uuid::parse_str(b["id"].as_str().unwrap()).unwrap()
}

/// The worker notify runs off the inbound request (spawned): wait until its thread ref is stored.
async fn wait_worker_ref(c: &Ctx) {
    for _ in 0..200 {
        let refs = c
            .texts("SELECT external_id FROM channel_conversation_refs WHERE side = 'worker'")
            .await;
        if !refs.is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the worker channels were not notified");
}

fn staff(c: &Ctx, binding: Uuid, email: Option<&str>, thread: &str, msg: &str) -> Value {
    json!({
        "tenant": c.t, "job_id": c.job, "binding_id": binding,
        "kind": "helpdesk", "thread": {"channel": "helpdesk", "id": thread, "route": {}},
        "staff": {"email": email, "external_id": "U1", "name": "Anna"},
        "message_id": msg, "text": "Hi, this is Anna from support.",
    })
}

#[tokio::test]
async fn a_worker_reply_is_replyhandover() {
    let c = conv::setup().await;
    let w = worker_thread(&c).await;
    let door = "/api/internal/channel/staff";
    let (s, b) = c
        .internal(
            door,
            staff(&c, w, Some("obs@alpha.example"), "thread-HO-1", "s0"),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "not_staff"));
    let (s, b) = c
        .internal(door, staff(&c, w, None, "thread-HO-1", "s0"))
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "not_staff"));
    let (s, b) = c
        .internal(
            door,
            staff(&c, w, Some("agent@alpha.example"), "thread-nope", "s0"),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "thread_not_found"));
    // the customer binding is not a worker binding; an unknown id is no binding
    for bad in [c.wc, Uuid::new_v4()] {
        let (s, b) = c
            .internal(
                door,
                staff(&c, bad, Some("agent@alpha.example"), "thread-HO-1", "s0"),
            )
            .await;
        assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "binding_not_found"));
    }
    let (s, b) = c
        .internal(
            door,
            staff(&c, w, Some("AGENT@alpha.example"), "thread-HO-1", "s1"),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b, json!({"status": "delivered", "item_id": "HO-1"}));
    let (_, q) = c.get("/api/jobs/j1/handover", "u-obs").await;
    let h = &q["items"][0];
    assert_eq!(
        (h["assignee"].as_str(), h["state"].as_str()),
        (Some("u-agent"), Some("waiting_customer"))
    );
    assert_eq!(c.edge.delivered().len(), 1);
    // a retry of a delivered worker message is stored once and not delivered again
    let (s, b) = c
        .internal(
            door,
            staff(&c, w, Some("agent@alpha.example"), "thread-HO-1", "s1"),
        )
        .await;
    assert_eq!((s, b["status"].as_str()), (StatusCode::OK, Some("stored")));
    assert_eq!(c.edge.delivered().len(), 1);
    let (s, b) = c
        .internal(
            door,
            staff(&c, w, Some("agent2@alpha.example"), "thread-HO-1", "s2"),
        )
        .await;
    // SW T5: the reply is the workspace's Take first; the conversation is u-agent's
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "owner_changed"));
    assert_eq!(b["error"]["owner"]["owner"], "user:u-agent", "{b}");
}

const STAFF_DOOR: &str = "/api/internal/channel/staff";

/// `ownership::current` of `conversation_id` in j1's scope (what the workspace shows).
async fn owner_of(c: &Ctx, conversation_id: &str) -> ownership::OwnerState {
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    let s = ownership::current(&mut tx, conversation_id).await.unwrap();
    tx.commit().await.unwrap();
    s
}

/// The Take lines (`system` / `owner`) of `conversation_id`.
async fn owner_lines(c: &Ctx, conversation_id: &str) -> Vec<String> {
    c.texts(&format!(
        "SELECT text FROM conversation_message WHERE conversation_id = '{conversation_id}' \
           AND role = 'system' AND author = 'owner' ORDER BY at_ms, message_id"
    ))
    .await
}

/// Wait until the worker thread of item `item` is stored (the notify runs off the request).
async fn wait_thread(c: &Ctx, item: &str) {
    for _ in 0..200 {
        let refs = c
            .texts(&format!(
                "SELECT external_id FROM channel_conversation_refs WHERE side = 'worker' AND item_id = '{item}'"
            ))
            .await;
        if !refs.is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("no worker thread for {item}");
}

#[tokio::test]
async fn helpdesk_reply_takes_queued_item_and_shows_in_workspace_owner() {
    let c = conv::setup().await;
    let w = worker_thread(&c).await;
    let conv_id = c.conversation_of("HO-1").await;
    assert_eq!(owner_of(&c, &conv_id).await.owner, Owner::Queue);
    let (s, b) = c
        .internal(
            STAFF_DOOR,
            staff(&c, w, Some("agent@alpha.example"), "thread-HO-1", "s1"),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b, json!({"status": "delivered", "item_id": "HO-1"}));
    // the workspace's owner record: the staff member the email maps to, by name
    let o = owner_of(&c, &conv_id).await;
    assert_eq!(o.owner, Owner::User("u-agent".into()));
    assert_eq!(o.name.as_deref(), Some("u-agent"));
    let h = c.item("HO-1").await;
    assert_eq!(
        (h["assignee"].as_str(), h["state"].as_str()),
        (Some("u-agent"), Some("waiting_customer"))
    );
    assert_eq!(
        owner_lines(&c, &conv_id).await,
        ["u-agent took this conversation"]
    );
    assert_eq!(
        c.texts(&format!(
            "SELECT author FROM conversation_message WHERE conversation_id = '{conv_id}' AND role = 'staff'"
        ))
        .await,
        ["u-agent"]
    );
    // the same commands as the workspace: a Take, then a staff reply
    let audit: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_event \
          WHERE action IN ('conversation.take', 'conversation.staff_reply', 'handover.assign', 'handover.reply') \
          ORDER BY id",
    )
    .fetch_all(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(audit, ["conversation.take", "conversation.staff_reply"]);
    // a second reply from the owner changes no owner: no second Take line
    let (s, b) = c
        .internal(
            STAFF_DOOR,
            staff(&c, w, Some("agent@alpha.example"), "thread-HO-1", "s2"),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(owner_lines(&c, &conv_id).await.len(), 1);
    // the reverse: the owner hands it back in the workspace; the agent owns the conversation again
    let (s, b) = c.hand_back(&conv_id, "u-agent", "Over to you").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(owner_of(&c, &conv_id).await.owner, Owner::Agent);
}

#[tokio::test]
async fn helpdesk_reply_on_other_persons_conversation_is_refused() {
    let c = conv::setup().await;
    let w = worker_thread(&c).await;
    let conv_id = c.conversation_of("HO-1").await;
    // u-agent2 took it in the workspace
    let (s, b) = c.take(&conv_id, "u-agent2", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = c
        .internal(
            STAFF_DOOR,
            staff(&c, w, Some("agent@alpha.example"), "thread-HO-1", "s1"),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "owner_changed"),
        "{b}"
    );
    assert_eq!(b["error"]["owner"]["owner"], "user:u-agent2", "{b}");
    assert_eq!(b["error"]["owner"]["name"], "u-agent2", "{b}");
    // nothing sent, nothing stored, the owner unchanged; the edge tells the thread (workers::OWNED_ELSEWHERE)
    assert!(c.edge.delivered().is_empty());
    assert_eq!(
        c.texts(&format!(
            "SELECT count(*)::text FROM conversation_message WHERE conversation_id = '{conv_id}' AND role = 'staff'"
        ))
        .await,
        ["0"]
    );
    assert_eq!(
        owner_of(&c, &conv_id).await.owner,
        Owner::User("u-agent2".into())
    );
    assert_eq!(
        agent_api::channels::workers::confirmation(&Err(
            agent_api::channels::core::CoreError::Code(code(&b).to_string())
        )),
        Some(agent_api::channels::workers::OWNED_ELSEWHERE)
    );
    // the owner's own reply from the thread goes through: theirs already, no second Take line
    let (s, b) = c
        .internal(
            STAFF_DOOR,
            staff(&c, w, Some("agent2@alpha.example"), "thread-HO-1", "s2"),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "delivered", "{b}");
    assert_eq!(
        owner_lines(&c, &conv_id).await,
        ["u-agent2 took this conversation"]
    );
}

/// A reply in an old ticket's thread never acts on a newer case of the same conversation: 409 `closed`.
#[tokio::test]
async fn a_reply_in_a_closed_items_thread_is_refused() {
    let c = conv::setup().await;
    let w = worker_thread(&c).await;
    let conv_id = c.conversation_of("HO-1").await;
    let (s, b) = c.take(&conv_id, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = c.hand_back(&conv_id, "u-agent", "Over to you").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // the customer asks again: a newer item, queued
    let (s, r) = c.inbound("wc-1", "m2", "I still want a real person").await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["item_id"], "HO-2", "{r}");
    wait_thread(&c, "HO-2").await;
    let before = owner_lines(&c, &conv_id).await;
    let (s, b) = c
        .internal(
            STAFF_DOOR,
            staff(&c, w, Some("agent@alpha.example"), "thread-HO-1", "s1"),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "closed"), "{b}");
    // HO-2 untouched: still queued, the queue owns the conversation, nothing sent or stored
    assert_eq!(c.item("HO-2").await["state"], "queued");
    assert_eq!(owner_of(&c, &conv_id).await.owner, Owner::Queue);
    assert_eq!(owner_lines(&c, &conv_id).await, before);
    assert!(c.edge.delivered().is_empty());
    // in HO-2's own thread the reply takes HO-2
    let (s, b) = c
        .internal(
            STAFF_DOOR,
            staff(&c, w, Some("agent@alpha.example"), "thread-HO-2", "s2"),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["item_id"], "HO-2", "{b}");
    assert_eq!(
        owner_of(&c, &conv_id).await.owner,
        Owner::User("u-agent".into())
    );
}

/// The T2 race: a customer's "talk to a person" and a Take of the agent-owned conversation at once. Both queue under
/// the conversation's control lock and owner row (ownership::guard): exactly one open item, and the owner matches
/// whichever came first.
#[tokio::test]
async fn an_inbound_handover_request_and_a_take_leave_one_open_item() {
    let c = conv::setup().await;
    // which side came first, per round (logged; the invariant holds either way)
    let (mut take_first, mut inbound_first) = (0, 0);
    for round in 0..5 {
        let wc = format!("wc-race{round}");
        let (conv_id, _) = c.customer_case(&wc, &format!("{wc}-1"), "Hello").await;
        let msg = format!("{wc}-2");
        let (inbound, take) = tokio::join!(
            c.inbound(&wc, &msg, "I want a real person"),
            c.take(&conv_id, "u-agent", "agent")
        );
        assert_eq!(inbound.0, StatusCode::OK, "round {round}: {}", inbound.1);
        assert_eq!(
            inbound.1["status"], "queued",
            "round {round}: {}",
            inbound.1
        );
        let open = c
            .texts(&format!(
                "SELECT id || '|' || state FROM agent_handover_items \
                  WHERE conversation_id = '{conv_id}' AND state NOT IN ('resolved', 'returned_to_agent')"
            ))
            .await;
        assert_eq!(open.len(), 1, "round {round}: {open:?}");
        let item = inbound.1["item_id"].as_str().unwrap_or_default();
        assert!(
            open[0].starts_with(&format!("{item}|")),
            "round {round}: the inbound names the one open item: {open:?} {}",
            inbound.1
        );
        let owner = owner_of(&c, &conv_id).await.owner;
        match take.0 {
            // the Take first: its item absorbs the customer's message
            StatusCode::OK => {
                take_first += 1;
                assert_eq!(owner, Owner::User("u-agent".into()), "round {round}");
                assert_eq!(take.1["item_id"].as_str(), Some(item), "round {round}");
                assert!(open[0].ends_with("|assigned"), "round {round}: {open:?}");
            }
            // the customer first: the queue owns it, the Take sees a changed owner
            StatusCode::CONFLICT => {
                inbound_first += 1;
                assert_eq!(code(&take.1), "owner_changed", "round {round}: {}", take.1);
                assert_eq!(owner, Owner::Queue, "round {round}");
                assert!(open[0].ends_with("|queued"), "round {round}: {open:?}");
            }
            s => panic!("round {round}: take answered {s}: {}", take.1),
        }
    }
    eprintln!("inbound vs Take race: Take first {take_first}, inbound first {inbound_first}");
    assert_eq!(take_first + inbound_first, 5, "every round checked");
}

#[tokio::test]
async fn a_worker_retry_redelivers_what_failed() {
    use std::sync::atomic::Ordering;
    let c = conv::setup().await;
    let w = worker_thread(&c).await;
    let door = "/api/internal/channel/staff";
    c.edge.fail.store(true, Ordering::SeqCst);
    let (s, b) = c
        .internal(
            door,
            staff(&c, w, Some("agent@alpha.example"), "thread-HO-1", "s1"),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::BAD_GATEWAY, "delivery_failed"));
    c.edge.fail.store(false, Ordering::SeqCst);
    let (s, b) = c
        .internal(
            door,
            staff(&c, w, Some("agent@alpha.example"), "thread-HO-1", "s1"),
        )
        .await;
    assert_eq!(
        (s, b["status"].as_str()),
        (StatusCode::OK, Some("delivered")),
        "{b}"
    );
    let (_, b) = c
        .internal(
            door,
            staff(&c, w, Some("agent@alpha.example"), "thread-HO-1", "s1"),
        )
        .await;
    assert_eq!(
        b["status"], "stored",
        "delivered once: the next retry stores nothing new"
    );
    assert_eq!(
        c.edge.delivered().len(),
        2,
        "one failed attempt, one delivery"
    );
    assert_eq!(
        c.texts("SELECT count(*)::text FROM conversation_message WHERE role = 'staff' AND delivered_at IS NOT NULL")
            .await,
        ["1"]
    );
}

fn history_url(c: &Ctx, id: &str, binding: Uuid, after: i64) -> String {
    format!(
        "/api/internal/channel/conversations/{id}/messages?tenant={}&job_id={}&binding_id={binding}&after_ms={after}",
        c.t, c.job
    )
}

#[tokio::test]
async fn web_chat_reconnect_reads_customer_facing_lines_only() {
    let c = conv::setup().await;
    let (id, _) = c.customer_case("wc-1", "m1", "Hello").await;
    // a Take and a Hand back each write a system line (staff names, the note to the agent): never shown to the
    // visitor
    let (s, r) = c.take(&id, "u-agent", "agent").await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (s, r) = c.hand_back(&id, "u-agent", "Over to you").await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(
        c.texts("SELECT count(*)::text FROM conversation_message WHERE role = 'system'")
            .await,
        ["2"]
    );
    let (s, b) = c.internal_get(&history_url(&c, &id, c.wc, 0)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let roles: Vec<&str> = b["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["customer"]);
    let last = b["messages"][0]["at_ms"].as_i64().unwrap();
    let (_, b) = c.internal_get(&history_url(&c, &id, c.wc, last)).await;
    assert_eq!(b["messages"], json!([]));
    let (s, b) = c.internal_get(&history_url(&c, "conv-nope", c.wc, 0)).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "conversation_not_found")
    );
    // the conversation is read only through the binding it arrived on
    let other: Uuid = sqlx::query_scalar(
        "INSERT INTO connector_bindings (tenant_id, job_id, kind, side, external_id, created_by, verified_at) \
         VALUES ($1, $2, 'web_chat', 'customer', 'wk_second', 'test', now()) RETURNING id",
    )
    .bind(c.t)
    .bind(c.job)
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    let (s, b) = c.internal_get(&history_url(&c, &id, other, 0)).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "conversation_not_found")
    );
    let (s, b) = c
        .internal_get(&format!(
            "/api/internal/channel/conversations/{id}/messages"
        ))
        .await;
    assert_eq!((s, code(&b)), (StatusCode::BAD_REQUEST, "invalid_query"));
}

#[tokio::test]
async fn inbound_refuses_unknown_scopes() {
    let c = conv::setup().await;
    let mut body = json!({
        "kind": "web_chat", "tenant": Uuid::new_v4(), "job_id": c.job, "binding_id": c.wc,
        "conversation": {"channel": "web_chat", "id": "wc-1", "route": {}},
        "message_id": "m1", "party": {"id": "cust", "verified": false}, "text": "hi",
        "external_id": "wk_test", "job_key": "j1",
    });
    let (s, b) = c
        .internal("/api/internal/channel/inbound", body.clone())
        .await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "tenant_not_found"));
    body["tenant"] = json!(c.t);
    body["job_id"] = json!(Uuid::new_v4());
    let (s, b) = c
        .internal("/api/internal/channel/inbound", body.clone())
        .await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "binding_not_found"));
    body["job_id"] = json!(c.job);
    body["kind"] = json!("staff");
    let (s, b) = c.internal("/api/internal/channel/inbound", body).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_channel")
    );
    let (s, b) = c.internal("/api/internal/channel/inbound", json!({})).await;
    assert_eq!((s, code(&b)), (StatusCode::BAD_REQUEST, "invalid_body"));
}

/// Customer rows stored for Telegram update 9001.
async fn stored(c: &Ctx) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM conversation_message WHERE external_id = '9001' AND role = 'customer'",
    )
    .fetch_one(&c.e.owner)
    .await
    .unwrap()
}

/// B7 R6 persist-before-200 as amended by D109: a message the agent would answer is a runtime turn, so even its
/// `accept_only` call is refused 409 `job_not_live` (nothing stored, the provider resends) while the job has no live
/// release. The runtime's accept-then-answer is tests/kh `t6_answer_path`.
#[tokio::test]
async fn accept_only_of_a_job_without_a_live_release_is_409_and_stores_nothing() {
    let c = conv::setup().await;
    let tg = create(
        &c,
        "j1",
        "u-int",
        json!({"kind": "telegram", "side": "customer", "label": "Test bot",
               "settings": {"allowed_user_ids": [42]},
               "credential": {"bot_token": "123456789:test-not-a-real-bot-token-000000",
                              "webhook_secret": "test-webhook-secret-0001"}}),
    )
    .await;
    for accept in [true, false] {
        let mut b = json!({
            "kind": "telegram", "external_id": "123456789", "binding_id": tg["id"],
            "tenant": c.t, "job_id": c.job, "job_key": "j1",
            "conversation": {"channel": "telegram", "id": "42", "route": {}},
            "message_id": "9001", "party": {"id": "42", "name": "Ada", "verified": true},
            "text": "Where is my order?",
        });
        if accept {
            b["accept_only"] = json!(true);
        }
        let (s, r) = c.internal("/api/internal/channel/inbound", b).await;
        assert_eq!(
            (s, code(&r)),
            (StatusCode::CONFLICT, "job_not_live"),
            "accept_only={accept}: {r}"
        );
    }
    assert_eq!(stored(&c).await, 0);
    assert_eq!(c.backend.calls(), 0);
}

// ---- R6 follow-ups: a resent handover accept, the stuck-accept sweeper -------------------------------------------

const INBOUND: &str = "/api/internal/channel/inbound";

/// A Telegram customer binding (as `accept_only_stores_first_and_the_answer_comes_from_the_second_call`).
async fn telegram(c: &Ctx) -> Value {
    create(
        c,
        "j1",
        "u-int",
        json!({"kind": "telegram", "side": "customer", "label": "Test bot",
               "settings": {"allowed_user_ids": [42]},
               "credential": {"bot_token": "123456789:test-not-a-real-bot-token-000000",
                              "webhook_secret": "test-webhook-secret-0001"}}),
    )
    .await
}

/// Telegram update `msg` from Ada (chat 42) on `tg`.
fn tg_body(c: &Ctx, tg: &Value, msg: &str, text: &str, accept: bool) -> Value {
    let mut b = json!({
        "kind": "telegram", "external_id": "123456789", "binding_id": tg["id"],
        "tenant": c.t, "job_id": c.job, "job_key": "j1",
        "conversation": {"channel": "telegram", "id": "42", "route": {}},
        "message_id": msg, "party": {"id": "42", "name": "Ada", "verified": true},
        "text": text,
    });
    if accept {
        b["accept_only"] = json!(true);
    }
    b
}

/// One text count through the owner pool.
async fn count(c: &Ctx, sql: &str) -> String {
    c.texts(sql).await.into_iter().next().unwrap_or_default()
}

/// R6 + handover: the first accept of "a person, please" queued the item, but the edge's accept budget ran out
/// (it delivered nothing). The provider's resend of the SAME message gets the same `queued` with the notice (not
/// `duplicate`), so the customer is told once; the item and the stored notice exist once. Once a later message
/// arrived, a resend of either is a duplicate (no second notice).
#[tokio::test]
async fn a_resent_handover_accept_gets_its_notice() {
    let c = conv::setup().await;
    let tg = telegram(&c).await;
    let (s, a) = c
        .internal(
            INBOUND,
            tg_body(&c, &tg, "9001", "I want to talk to a human", true),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{a}");
    assert_eq!(
        (
            a["status"].as_str(),
            a["notice"].as_bool(),
            a["item_id"].as_str()
        ),
        (Some("queued"), Some(true), Some("HO-1")),
        "{a}"
    );
    // the edge timed out and forgot the update: Telegram resends it
    let (s, r) = c
        .internal(
            INBOUND,
            tg_body(&c, &tg, "9001", "I want to talk to a human", true),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(
        (
            r["status"].as_str(),
            r["notice"].as_bool(),
            r["item_id"].as_str()
        ),
        (Some("queued"), Some(true), Some("HO-1")),
        "the resend carries the notice: {r}"
    );
    assert_eq!(r["case_id"], a["case_id"]);
    assert_eq!(
        count(&c, "SELECT count(*)::text FROM agent_handover_items").await,
        "1"
    );
    assert_eq!(
        count(
            &c,
            "SELECT count(*)::text FROM conversation_message WHERE author = 'handover_notice'"
        )
        .await,
        "1",
        "the notice is stored once"
    );
    assert_eq!(stored(&c).await, 1);
    // a later message while the item is open: queued, no notice
    let (_, m2) = c
        .internal(INBOUND, tg_body(&c, &tg, "9002", "hello?", true))
        .await;
    assert_eq!(
        (m2["status"].as_str(), m2["notice"].as_bool()),
        (Some("queued"), Some(false)),
        "{m2}"
    );
    for msg in ["9001", "9002"] {
        let (_, d) = c
            .internal(INBOUND, tg_body(&c, &tg, msg, "again", true))
            .await;
        assert_eq!(d["status"], "duplicate", "{msg}: {d}");
    }
    assert_eq!(
        c.backend.calls(),
        0,
        "a queued conversation is not answered"
    );
    assert_eq!(
        count(&c, "SELECT count(*)::text FROM agent_handover_items").await,
        "1"
    );
}

/// R6 across edge pods: once an edge reported the notice delivered (`handover/notice-delivered`), a resent accept of
/// the queuing message is `duplicate` (the edge's dedupe is per process, so another pod would send it again). The ack
/// is idempotent, 204 also for an unknown item, and key-gated like every internal route.
#[tokio::test]
async fn a_delivered_notice_is_not_sent_again() {
    let c = conv::setup().await;
    let tg = telegram(&c).await;
    let (_, a) = c
        .internal(
            INBOUND,
            tg_body(&c, &tg, "9101", "I want to talk to a human", true),
        )
        .await;
    assert_eq!(
        (a["status"].as_str(), a["notice"].as_bool()),
        (Some("queued"), Some(true)),
        "{a}"
    );
    let conv_id = a["conversation_id"].as_str().unwrap().to_string();
    let ack_in = |conv: &str, item: &str| json!({"binding_id": tg["id"], "kind": "telegram", "tenant": c.t, "job_id": c.job, "conversation_id": conv, "item_id": item});
    let ack = |item: &str| ack_in(&conv_id, item);
    // no edge key: refused
    let (s, _) = send(
        &c.e.app,
        common::json("POST", NOTICE_DELIVERED, None, ack("HO-1")),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // before the ack a resend still carries the notice (the first accept may have timed out at the edge)
    let (_, r) = c
        .internal(
            INBOUND,
            tg_body(&c, &tg, "9101", "I want to talk to a human", true),
        )
        .await;
    assert_eq!(
        (r["status"].as_str(), r["notice"].as_bool()),
        (Some("queued"), Some(true)),
        "{r}"
    );
    // another conversation's ack does not stamp this item
    let (s, b) = c
        .internal(NOTICE_DELIVERED, ack_in("CONV-OTHER", "HO-1"))
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    assert_eq!(
        count(
            &c,
            "SELECT count(*)::text FROM agent_handover_items WHERE notice_delivered_at IS NOT NULL"
        )
        .await,
        "0"
    );
    for item in ["HO-1", "HO-1", "HO-999"] {
        let (s, b) = c.internal(NOTICE_DELIVERED, ack(item)).await;
        assert_eq!(s, StatusCode::NO_CONTENT, "{item}: {b}");
    }
    assert_eq!(
        count(
            &c,
            "SELECT count(*)::text FROM agent_handover_items WHERE notice_delivered_at IS NOT NULL"
        )
        .await,
        "1"
    );
    let (s, d) = c
        .internal(
            INBOUND,
            tg_body(&c, &tg, "9101", "I want to talk to a human", true),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert_eq!(
        d["status"], "duplicate",
        "no second notice once delivered: {d}"
    );
    // the item is still queued; the stored notice exists once
    assert_eq!(
        count(&c, "SELECT state FROM agent_handover_items").await,
        "queued"
    );
    assert_eq!(
        count(
            &c,
            "SELECT count(*)::text FROM conversation_message WHERE author = 'handover_notice'"
        )
        .await,
        "1"
    );
}

/// R6 "once" on the sweeper's path: when the stuck-accept sweeper's answer is a newly queued handover, it delivers
/// the handover notice itself (`answer_sweeper::deliver_answer`, the edge's `/internal/deliver`) and, once the edge
/// took it, stamps the item `notice_delivered_at` as the edge's `handover/notice-delivered` ack would. Without the
/// stamp the provider's resend of the queuing accept would get `queued` with the notice again (a second notice). A
/// delivery the edge refused leaves the item unstamped, so the resend still carries the notice.
#[tokio::test]
async fn a_handover_notice_the_sweeper_delivered_is_not_sent_again() {
    use agent_api::channels::core::InboundResp;
    use agent_api::conversations::answer_sweeper::deliver_answer;
    use std::sync::atomic::Ordering;
    let c = conv::setup().await;
    let tg = telegram(&c).await;
    let (_, a) = c
        .internal(
            INBOUND,
            tg_body(&c, &tg, "9201", "I want to talk to a human", true),
        )
        .await;
    assert_eq!(
        (a["status"].as_str(), a["notice"].as_bool()),
        (Some("queued"), Some(true)),
        "{a}"
    );
    let conv_id = a["conversation_id"].as_str().unwrap().to_string();
    let item_id = a["item_id"].as_str().unwrap().to_string();
    let resp = InboundResp::Queued {
        conversation_id: conv_id.clone(),
        case_id: a["case_id"].as_str().unwrap().to_string(),
        item_id: item_id.clone(),
        notice: true,
    };
    let scope = Scope {
        tenant: c.t,
        job_id: c.job,
        job_key: "j1".to_string(),
    };
    const STAMPED: &str =
        "SELECT count(*)::text FROM agent_handover_items WHERE notice_delivered_at IS NOT NULL";
    // the edge refuses every attempt: nothing is stamped, the resend still carries the notice
    c.edge.fail.store(true, Ordering::SeqCst);
    deliver_answer(&c.e.state, &scope, &conv_id, &resp)
        .await
        .unwrap();
    c.edge.fail.store(false, Ordering::SeqCst);
    assert!(!c.edge.delivered().is_empty(), "the notice was attempted");
    assert_eq!(count(&c, STAMPED).await, "0");
    let (_, r) = c
        .internal(
            INBOUND,
            tg_body(&c, &tg, "9201", "I want to talk to a human", true),
        )
        .await;
    assert_eq!(
        (r["status"].as_str(), r["notice"].as_bool()),
        (Some("queued"), Some(true)),
        "an undelivered notice is offered again: {r}"
    );
    // the sweeper delivers it: the item is stamped
    let before = c.edge.delivered().len();
    deliver_answer(&c.e.state, &scope, &conv_id, &resp)
        .await
        .unwrap();
    let d = c.edge.delivered();
    assert_eq!(d.len(), before + 1, "delivered once: {d:?}");
    assert_eq!(d[before]["message"]["kind"], "handover_notice", "{d:?}");
    assert_eq!(d[before]["binding_id"], tg["id"]);
    assert_eq!(count(&c, STAMPED).await, "1");
    // the provider resends the queuing accept: no second notice
    let (s, r) = c
        .internal(
            INBOUND,
            tg_body(&c, &tg, "9201", "I want to talk to a human", true),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(
        r["status"], "duplicate",
        "no second notice once delivered: {r}"
    );
    assert_eq!(
        count(&c, "SELECT state FROM agent_handover_items").await,
        "queued"
    );
    assert_eq!(c.backend.calls(), 0);
}

const NOTICE_DELIVERED: &str = "/api/internal/channel/handover/notice-delivered";

/// Make every armed message due now (as if `answer_sweeper::STUCK_AFTER` had passed).
async fn age(c: &Ctx) {
    sqlx::query(
        "UPDATE conversation_message SET answer_due_at = now() - interval '1 second' \
          WHERE answer_due_at IS NOT NULL",
    )
    .execute(&c.e.owner)
    .await
    .unwrap();
}

/// R6: a message stored by `accept_only` whose edge never asked for the answer (every provider retry timed out) is
/// answered by the sweeper once it is due — on the runtime (D109: job `j1` is live and served, binds no qualifier, so
/// the turn answers `TURN_FAILED`; the model backend is never called) — and delivered through the edge's
/// `/internal/deliver`; a second sweep does nothing, and the edge's late answer call is a duplicate.
#[tokio::test]
async fn a_stuck_accept_is_answered_and_delivered_once_by_the_sweeper() {
    use agent_api::conversations::answer_sweeper::sweep_once;
    use agent_api::turn::TURN_FAILED;
    let c = conv::setup_runtime(redis_env::api_vars("ci-stuck-accept").await).await;
    let tg = telegram(&c).await;
    let (_, a) = c
        .internal(
            INBOUND,
            tg_body(&c, &tg, "9001", "Where is my order?", true),
        )
        .await;
    assert_eq!(a["status"], "accepted", "{a}");
    // not due yet: the edge's own answer call has its minute
    assert_eq!(sweep_once(&c.e.state).await.unwrap(), 0);
    assert_eq!(c.backend.calls(), 0);
    age(&c).await;
    assert_eq!(sweep_once(&c.e.state).await.unwrap(), 1);
    assert_eq!(c.backend.calls(), 0, "never the model: the runtime answers");
    assert_eq!(
        c.texts("SELECT role || ':' || text FROM conversation_message ORDER BY at_ms, message_id")
            .await,
        [
            "customer:Where is my order?".to_string(),
            format!("agent:{TURN_FAILED}")
        ],
        "answered once"
    );
    let d = c.edge.delivered();
    assert_eq!(d.len(), 1, "delivered once: {d:?}");
    assert_eq!(d[0]["message"]["text"], TURN_FAILED);
    assert_eq!(d[0]["conversation"]["id"], "42");
    assert_eq!(d[0]["binding_id"], tg["id"]);
    // the second tick finds nothing
    age(&c).await;
    assert_eq!(sweep_once(&c.e.state).await.unwrap(), 0);
    assert_eq!(c.edge.delivered().len(), 1);
    // the edge's late answer call: already answered
    let (_, r) = c
        .internal(
            INBOUND,
            tg_body(&c, &tg, "9001", "Where is my order?", false),
        )
        .await;
    assert_eq!(r["status"], "duplicate", "{r}");
    assert_eq!(
        c.texts("SELECT role FROM conversation_message WHERE role = 'agent'")
            .await
            .len(),
        1,
        "one turn"
    );
    assert_eq!(c.backend.calls(), 0);
}

/// R6: the sweeper and the edge never both answer. While a sweeper holds a live claim the edge's answer call is a
/// duplicate (the sweeper delivers); once the edge's answer call answered (a runtime turn, D109), the sweeper finds
/// nothing to do.
#[tokio::test]
async fn the_sweeper_and_the_edge_answer_call_exclude_each_other() {
    use agent_api::conversations::answer_sweeper::sweep_once;
    use agent_api::turn::TURN_FAILED;
    let c = conv::setup_runtime(redis_env::api_vars("ci-sweep-exclusive").await).await;
    let tg = telegram(&c).await;
    for msg in ["9001", "9002"] {
        let (_, a) = c
            .internal(INBOUND, tg_body(&c, &tg, msg, "Where is my order?", true))
            .await;
        assert_eq!(a["status"], "accepted", "{a}");
    }
    // 9001: a sweeper (another replica) holds a live claim on it
    sqlx::query(
        "UPDATE conversation_message \
            SET answer_claim = gen_random_uuid(), answer_due_at = now() + interval '5 minutes' \
          WHERE external_id = '9001'",
    )
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (_, r) = c
        .internal(
            INBOUND,
            tg_body(&c, &tg, "9001", "Where is my order?", false),
        )
        .await;
    assert_eq!(r["status"], "duplicate", "the sweeper answers it: {r}");
    assert_eq!(c.backend.calls(), 0);
    // 9002: the edge's answer call answers it and clears the mark; the sweeper then has nothing of it
    let (_, r) = c
        .internal(
            INBOUND,
            tg_body(&c, &tg, "9002", "Where is my order?", false),
        )
        .await;
    assert_eq!(r["status"], "answered", "{r}");
    assert_eq!(r["text"], TURN_FAILED, "{r}");
    assert_eq!(c.backend.calls(), 0, "never the model");
    assert_eq!(
        count(
            &c,
            "SELECT count(*)::text FROM conversation_message \
              WHERE external_id = '9002' AND answer_due_at IS NOT NULL"
        )
        .await,
        "0"
    );
    sqlx::query(
        "UPDATE conversation_message SET answer_due_at = now() - interval '1 second' \
          WHERE answer_due_at IS NOT NULL AND external_id <> '9001'",
    )
    .execute(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(sweep_once(&c.e.state).await.unwrap(), 0);
    assert_eq!(c.backend.calls(), 0);
    assert!(
        c.edge.delivered().is_empty(),
        "the edge delivered 9002 itself"
    );
}

// ---- the web chat customer stream (crate::conversations::webchat_stream) -----------------------------------------

fn stream_url(c: &Ctx, conv: &str, binding: Uuid) -> String {
    format!(
        "/api/internal/channel/webchat/conversations/{conv}/events?tenant={}&job_id={}&binding_id={binding}",
        c.t, c.job
    )
}

#[tokio::test]
async fn the_web_chat_stream_is_key_gated_and_read_through_its_binding() {
    let c = conv::setup().await;
    let (id, _) = c.customer_case("wc-1", "m1", "Hello").await;
    for auth in [None, Some("Bearer nope")] {
        let (s, _) = send(&c.e.app, get_with(&stream_url(&c, &id, c.wc), auth)).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{auth:?}");
    }
    let (s, b) = c.internal_get(&stream_url(&c, "conv-nope", c.wc)).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "conversation_not_found")
    );
    let (s, b) = c.internal_get(&stream_url(&c, &id, Uuid::new_v4())).await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "binding_not_found"));
}

/// One agent-edge replica in front of the agent-api at `core`, serving the job's web chat binding (`wk_test`).
fn edge_replica(c: &Ctx, core: &Arc<CoreLink>) -> EdgeState {
    let mut st = EdgeState::new(Registry::default(), Arc::new(Unconfigured));
    st.core = Some(core.clone());
    st.bindings.replace_core(vec![Binding {
        id: Some(c.wc),
        kind: ChannelKind::WebChat,
        side: Side::Customer,
        external_id: "wk_test".into(),
        scope: Scope {
            tenant: c.t,
            job_id: c.job,
            job_key: "j1".into(),
        },
        env: "prod".into(),
        settings: json!({}),
        credential: None,
    }]);
    st.webchat = Some(Arc::new(WebChat::new(WebChatConfig {
        bindings: BTreeMap::new(),
        allowed_origins: BTreeSet::new(),
        public_url: "https://chat.nonagentic.ai".into(),
    })));
    st
}

/// Read `body` until `want` arrived (or the stream ended / 5 s passed); everything read.
async fn read_until(body: &mut Body, want: &str) -> String {
    read_until_matching(body, |seen| seen.contains(want)).await
}

async fn read_until_matching(body: &mut Body, arrived: impl Fn(&str) -> bool) -> String {
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !arrived(&seen) {
        let Ok(Some(frame)) = tokio::time::timeout_at(deadline, body.frame()).await else {
            break;
        };
        if let Ok(d) = frame.unwrap().into_data() {
            seen.push_str(&String::from_utf8_lossy(&d));
        }
    }
    seen
}

fn owner_events(seen: &str) -> Vec<Value> {
    seen.split("\n\n")
        .filter(|frame| frame.lines().any(|line| line == "event: owner"))
        .filter_map(|frame| {
            let data = frame.lines().find_map(|line| line.strip_prefix("data: "))?;
            serde_json::from_str(data).ok()
        })
        .collect()
}

#[tokio::test]
async fn a_web_chat_delivery_reaches_the_customer_through_every_edge_replica() {
    // agent-api on Redis (the fan-out every replica shares), served on a port the edge replicas call
    let vars = redis_env::api_vars("ci-wc-stream").await;
    let c = conv::setup_with(|v| v.extend(vars)).await;
    let (id, _) = c.customer_case("wc-1", "m1", "Hello").await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = c.e.app.clone();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let key = InternalKey::new(EDGE_KEY).unwrap();
    let core = Arc::new(CoreLink::new(format!("http://{addr}"), key.clone()).unwrap());
    // the widget's conversation token, as the edge issued it on the first answer
    let conv_id = webchat::new_conversation_id();
    let token = webchat::issue_token(&key, &conv_id, &format!("{id}~{}", c.wc));

    // the customer token is still the edge's check, before core is asked
    for bad in ["x", "0199aaaa-0000-7000-8000-000000000009.AAAA"] {
        let (s, b) = send(
            &router(edge_replica(&c, &core)),
            get_with(
                &format!("/api/webchat/conversations/{conv_id}/events?token={bad}"),
                None,
            ),
        )
        .await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::FORBIDDEN, "invalid_token"),
            "{bad}"
        );
    }

    // the stream also proves the SESSION: no session token, or one core does not know, opens nothing
    let session = format!("{}.c2Vzc2lvbi1zZWNyZXQ", Uuid::new_v4());
    for q in [String::new(), format!("&session={session}")] {
        let (s, b) = send(
            &router(edge_replica(&c, &core)),
            get_with(
                &format!("/api/webchat/conversations/{conv_id}/events?token={token}{q}"),
                None,
            ),
        )
        .await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNAUTHORIZED, "session_invalid"),
            "{q}"
        );
    }
    sqlx::query(
        "INSERT INTO web_chat_session (tenant_id, job_id, binding_id, token_hash, origin, ip_hash, expires_at, \
                                       conversation_id) \
         VALUES ($1, $2, $3, sha256($4::bytea), 'https://shop.example', sha256($4::bytea), now() + interval '1 hour', $5)",
    )
    .bind(c.t)
    .bind(c.job)
    .bind(c.wc)
    .bind(session.clone().into_bytes())
    .bind(&id)
    .execute(&c.e.owner)
    .await
    .unwrap();

    // a live session of the same binding that is not THIS conversation's own does not open its stream
    let other = format!("{}.b3RoZXItc2Vzc2lvbg", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO web_chat_session (tenant_id, job_id, binding_id, token_hash, origin, ip_hash, expires_at) \
         VALUES ($1, $2, $3, sha256($4::bytea), 'https://shop.example', sha256($4::bytea), now() + interval '1 hour')",
    )
    .bind(c.t)
    .bind(c.job)
    .bind(c.wc)
    .bind(other.clone().into_bytes())
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (s, b) = send(
        &router(edge_replica(&c, &core)),
        get_with(
            &format!("/api/webchat/conversations/{conv_id}/events?token={token}&session={other}"),
            None,
        ),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::UNAUTHORIZED, "session_invalid"));

    // two edge replicas, each holding a stream of the same conversation
    let url =
        format!("/api/webchat/conversations/{conv_id}/events?token={token}&session={session}");
    let mut bodies = Vec::new();
    for _ in 0..2 {
        let r = router(edge_replica(&c, &core))
            .oneshot(get_with(&url, None))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        bodies.push(r.into_body());
    }
    // a delivery through core (the CSAT ask after a resolve): one edge accepts it, core publishes it
    let scope = Scope {
        tenant: c.t,
        job_id: c.job,
        job_key: "j1".into(),
    };
    assert!(
        staff::notify_customer(
            &c.e.state,
            &scope,
            &id,
            DeliverKind::CsatRequest,
            "How did we do? Reply 1-5",
        )
        .await
    );
    assert_eq!(c.edge.delivered().len(), 1, "accepted once");
    for body in &mut bodies {
        let seen = read_until(body, "How did we do? Reply 1-5").await;
        assert!(seen.contains("event: message"), "{seen}");
        assert!(seen.contains(r#""kind":"csat_request""#), "{seen}");
        // the id the accepting edge named
        assert!(seen.contains("id: out-"), "{seen}");
    }
}

#[tokio::test]
async fn a_console_frame_reaches_a_feed_on_another_replica_once() {
    use agent_api::conversations::hub::{EventHub, LocalHub};
    use agent_api::sse_hub::Bridge;
    let vars = redis_env::api_vars("ci-staff-frames").await;
    let c = conv::setup_with(|v| v.extend(vars)).await;
    let (id, _) = c.customer_case("wc-1", "m1", "Hello").await;
    // replica B: the same database and Redis, its own hub and bridge (what each agent-api process holds)
    let mut b = c.e.state.clone();
    let hub: Arc<dyn EventHub> = Arc::new(LocalHub::default());
    b.sse = b.redis.as_ref().map(|r| Bridge::new(r, hub.clone()));
    b.events = hub;
    let app_b = agent_api::router(b);
    let feed = format!("/api/jobs/j1/conversations/{id}/events");
    let token = common::person("u-admin", &[]);
    let mut bodies = Vec::new();
    for app in [&app_b, &c.e.app] {
        let r = app
            .clone()
            .oneshot(common::get(&feed, Some(&token)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        bodies.push(r.into_body());
    }
    // staff actions on replica A: Take, then Hand back (each publishes an `owner` frame)
    let (s, r) = c.take(&id, "u-agent", "agent").await;
    assert_eq!(s, StatusCode::OK, "take: {r}");
    let (s, r) = c.hand_back(&id, "u-agent", "Over to you").await;
    assert_eq!(s, StatusCode::OK, "hand back: {r}");
    // B (another process) gets both frames; A (the publisher) gets them back through the bridge, once
    for (replica, body) in ["B", "A"].into_iter().zip(&mut bodies) {
        let seen = read_until_matching(body, |seen| {
            owner_events(seen)
                .iter()
                .any(|event| event["owner"] == "agent")
        })
        .await;
        // Handover snapshots also embed an owner. Count only owner events, once for Take and once for Hand back.
        let owners = owner_events(&seen);
        for owner in ["user:u-agent", "agent"] {
            assert_eq!(
                owners
                    .iter()
                    .filter(|event| event["owner"] == owner)
                    .count(),
                1,
                "{replica}, {owner}: {seen}"
            );
        }
    }
}
