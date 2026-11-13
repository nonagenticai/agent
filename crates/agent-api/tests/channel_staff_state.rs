//! B7 R9 `POST /api/internal/channel/staff-state` (crate::conversations::internal): a worker channel (Frappe
//! Helpdesk) reports that staff closed the handover in their own tool. SW-5 (T5): the workspace's Take (the item is
//! taken by that staff member when still queued), then its Resolve (outcome `resolved`): its case is resolved, the
//! agent owns the conversation again; the customer gets the resolution line and the CSAT ask; a repeat is
//! `duplicate` and sends nothing; a non-agent email is 403 `not_staff`; a colleague's conversation is 409
//! `owner_changed`. The thread here is a helpdesk worker thread. Real Postgres 18.
use crate::common;

use agent_api::conversations::ownership::{self, Owner};
use agent_api::conversations::store;
use axum::http::StatusCode;
use common::conv::{self, Ctx, code};
use serde_json::{Value, json};
use uuid::Uuid;

const RESOLVED_TEXT: &str = agent_api::conversations::internal::RESOLVED_TEXT;

/// A verified helpdesk worker binding, then a conversation that asked for a person (worker thread `thread-HO-1`).
async fn worker_thread(c: &Ctx) -> Uuid {
    let (s, b) = c
        .call(
            "POST",
            "/api/jobs/j1/channel-bindings",
            "u-int",
            json!({"kind": "helpdesk", "side": "worker", "external_id": "helpdesk.test#support", "label": "Ops",
                   "settings": {"base_url": "https://helpdesk.example", "service_user": "bot@helpdesk.example"},
                   "credential": {"api_key": "key-test", "api_secret": "secret-test",
                                  "webhook_secret": "wh-secret-test-0123456789"}}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
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

fn state(c: &Ctx, binding: Uuid, email: Option<&str>, thread: &str, st: &str) -> Value {
    json!({
        "tenant": c.t, "job_id": c.job, "binding_id": binding, "kind": "helpdesk",
        "thread": {"channel": "helpdesk", "id": thread, "route": {}},
        "staff": {"email": email, "external_id": email.unwrap_or("x")},
        "state": st, "message_id": "hd-state:12:Resolved:1",
    })
}

const DOOR: &str = "/api/internal/channel/staff-state";

#[tokio::test]
async fn a_closed_ticket_resolves_the_item_and_the_case_once() {
    let c = conv::setup().await;
    let w = worker_thread(&c).await;
    let refusals = [
        (
            state(&c, w, Some("obs@alpha.example"), "thread-HO-1", "resolved"),
            StatusCode::FORBIDDEN,
            "not_staff",
        ),
        (
            state(&c, w, None, "thread-HO-1", "resolved"),
            StatusCode::FORBIDDEN,
            "not_staff",
        ),
        (
            state(
                &c,
                w,
                Some("agent@alpha.example"),
                "thread-nope",
                "resolved",
            ),
            StatusCode::NOT_FOUND,
            "thread_not_found",
        ),
        (
            state(&c, w, Some("agent@alpha.example"), "thread-HO-1", "open"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_state",
        ),
        (
            state(
                &c,
                c.wc,
                Some("agent@alpha.example"),
                "thread-HO-1",
                "resolved",
            ),
            StatusCode::NOT_FOUND,
            "binding_not_found",
        ),
    ];
    for (body, status, want) in refusals {
        let (s, b) = c.internal(DOOR, body).await;
        assert_eq!((s, code(&b)), (status, want), "{b}");
    }
    let before = c.edge.delivered().len();
    let (s, b) = c
        .internal(
            DOOR,
            state(
                &c,
                w,
                Some("AGENT@alpha.example"),
                "thread-HO-1",
                "resolved",
            ),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b, json!({"status": "resolved", "item_id": "HO-1"}));
    // the queued item was taken by the staff member who closed it, then resolved
    let (_, q) = c.get("/api/jobs/j1/handover", "u-obs").await;
    let h = &q["items"][0];
    assert_eq!(
        (h["assignee"].as_str(), h["state"].as_str()),
        (Some("u-agent"), Some("resolved"))
    );
    let case_status = c
        .texts(&format!(
            "SELECT a.status FROM agent_case a JOIN agent_handover_items h ON h.case_id = a.case_id \
              WHERE h.id = 'HO-1' AND h.tenant_id = '{}'",
            c.t
        ))
        .await;
    assert_eq!(case_status, ["resolved"]);
    // the customer is told, then asked for a rating
    let sent = c.edge.delivered();
    let new: Vec<(String, String)> = sent[before..]
        .iter()
        .map(|d| {
            (
                d["message"]["kind"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                d["message"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect();
    assert_eq!(new.len(), 2, "{new:?}");
    assert_eq!(new[0], ("text".to_string(), RESOLVED_TEXT.to_string()));
    assert_eq!(new[1].0, "csat_request");
    // the Take's owner line and the Resolve's resolution line (system), the customer's lines (agent)
    let lines = c
        .texts(&format!(
            "SELECT role || ':' || author FROM conversation_message WHERE tenant_id = '{}' AND author IN \
             ('resolution', 'csat', 'agent_cases', 'owner') ORDER BY 1",
            c.t
        ))
        .await;
    assert_eq!(
        lines,
        [
            "agent:csat",
            "agent:resolution",
            "system:owner",
            "system:resolution"
        ]
    );
    // a repeated webhook changes nothing and sends nothing
    let (s, b) = c
        .internal(
            DOOR,
            state(
                &c,
                w,
                Some("agent2@alpha.example"),
                "thread-HO-1",
                "resolved",
            ),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b, json!({"status": "duplicate", "item_id": "HO-1"}));
    assert_eq!(c.edge.delivered().len(), before + 2);
    let audit: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_event WHERE action IN \
         ('handover.assign', 'handover.state', 'conversation.take', 'conversation.resolve') ORDER BY id",
    )
    .fetch_all(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(audit, ["conversation.take", "conversation.resolve"]);
}

/// `ownership::current` of `conversation_id` in j1's scope (what the workspace shows).
async fn owner_of(c: &Ctx, conversation_id: &str) -> ownership::OwnerState {
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    let s = ownership::current(&mut tx, conversation_id).await.unwrap();
    tx.commit().await.unwrap();
    s
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
async fn helpdesk_resolve_sets_agent_owner() {
    let c = conv::setup().await;
    let w = worker_thread(&c).await;
    let conv_id = c.conversation_of("HO-1").await;
    let (s, b) = c
        .internal(
            DOOR,
            state(
                &c,
                w,
                Some("agent@alpha.example"),
                "thread-HO-1",
                "resolved",
            ),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "resolved", "{b}");
    // the workspace's Resolve: outcome, closer, the agent owns the conversation again
    assert_eq!(owner_of(&c, &conv_id).await.owner, Owner::Agent);
    assert_eq!(
        c.texts(
            "SELECT resolution || '|' || closed_by FROM agent_handover_items WHERE id = 'HO-1'"
        )
        .await,
        ["resolved|u-agent"]
    );
    assert_eq!(
        c.texts(&format!(
            "SELECT text FROM conversation_message WHERE conversation_id = '{conv_id}' \
               AND role = 'system' AND author = 'resolution'"
        ))
        .await,
        ["Resolved by u-agent."]
    );
    assert_eq!(
        c.texts(
            "SELECT details->>'outcome' || '|' || (details->>'channel') FROM audit_event \
              WHERE action = 'conversation.resolve'"
        )
        .await,
        ["resolved|helpdesk"]
    );
    // a newer case of the conversation, taken by a colleague in the workspace: a close from the thread is refused
    let (s, r) = c.inbound("wc-1", "m2", "I want a real person again").await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["item_id"], "HO-2", "{r}");
    wait_thread(&c, "HO-2").await;
    let (s, b) = c.take(&conv_id, "u-agent2", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let sent = c.edge.delivered().len();
    let (s, b) = c
        .internal(
            DOOR,
            state(
                &c,
                w,
                Some("agent@alpha.example"),
                "thread-HO-2",
                "resolved",
            ),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "owner_changed"),
        "{b}"
    );
    assert_eq!(c.item("HO-2").await["state"], "assigned");
    assert_eq!(
        owner_of(&c, &conv_id).await.owner,
        Owner::User("u-agent2".into())
    );
    assert_eq!(c.edge.delivered().len(), sent, "nothing is sent");
    // the owner resolves in the workspace: the thread's later close is a repeat
    let (s, b) = c
        .resolve("HO-2", "u-agent2", json!({"outcome": "resolved"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let sent = c.edge.delivered().len();
    let (s, b) = c
        .internal(
            DOOR,
            state(
                &c,
                w,
                Some("agent2@alpha.example"),
                "thread-HO-2",
                "resolved",
            ),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b, json!({"status": "duplicate", "item_id": "HO-2"}));
    assert_eq!(c.edge.delivered().len(), sent);
    assert_eq!(owner_of(&c, &conv_id).await.owner, Owner::Agent);
}
