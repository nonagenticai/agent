//! Staff Workspace T3 (migration 0191): one Send route for staff messages, `POST /api/jobs/{job}/conversations/{id}/
//! messages` `{kind: "reply" | "note", text, client_msg_id}` → `MessageView` (crate::conversations::staff). A reply is
//! the owner's and is delivered; a note is staff-only and never leaves the platform; a repeated `client_msg_id` is the
//! same message; a reply is capped at its channel's limit; a failed delivery is kept and answered 202. Real Postgres 18.
use crate::common;

use std::time::Duration;

use agent_api::conversations::hub::Frame;
use agent_api::conversations::store;
use agent_api::interaction::ConversationRef;
use axum::body::Body;
use axum::http::StatusCode;
use common::conv::{self, Ctx, EDGE_KEY, code};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(5);

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

/// A customer conversation on `channel` (external id `ext`) holding one customer message on its open case, with a
/// customer ref (no binding): a reply on it is delivered through the fake edge. The conversation id.
async fn on_channel(c: &Ctx, channel: &str, ext: &str) -> String {
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    let cref = ConversationRef {
        channel: channel.into(),
        id: ext.into(),
        route: Default::default(),
    };
    let conv = store::upsert_customer_conversation(
        &mut tx,
        c.job,
        channel,
        &cref,
        "cust-1",
        Some("Femke"),
        "prod",
    )
    .await
    .unwrap();
    let design = store::design_of(&mut tx, c.job, "j1", &conv).await.unwrap();
    let case_id = store::case_for(&mut tx, c.job, &conv, "Hello", &design)
        .await
        .unwrap();
    let msg = format!("{ext}-1");
    store::append(
        &mut tx,
        store::NewMessage {
            job_id: c.job,
            conversation_id: &conv.id,
            role: "customer",
            author: "Femke",
            text: "Hello",
            case_id: Some(&case_id),
            external_id: Some(&msg),
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    conv.id
}

/// Every frame the hub holds for `rx` right now.
fn drain(rx: &mut tokio::sync::broadcast::Receiver<Frame>) -> Vec<Frame> {
    let mut out = Vec::new();
    while let Ok(f) = rx.try_recv() {
        out.push(f);
    }
    out
}

/// Read `body` until `want` arrived (or the stream ended / [`WAIT`] passed); everything read.
async fn read_until(body: &mut Body, want: &str) -> String {
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + WAIT;
    while !seen.contains(want) {
        let Ok(Some(frame)) = tokio::time::timeout_at(deadline, body.frame()).await else {
            break;
        };
        if let Ok(d) = frame.unwrap().into_data() {
            seen.push_str(&String::from_utf8_lossy(&d));
        }
    }
    seen
}

#[tokio::test]
async fn reply_by_owner_delivers_and_waits() {
    let c = conv::setup().await;
    let (conv_id, item) = queued(&c, "wc-1").await;
    let (s, b) = c.take(&conv_id, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut rx = c.e.state.events.subscribe();
    let client = Uuid::new_v4();
    let (s, b) = c
        .send_message(
            &conv_id,
            "u-agent",
            "reply",
            "  Hi, Anna here.  ",
            Some(client),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["kind"], "staff");
    assert_eq!(b["event"], Value::Null);
    assert_eq!(b["text"], "Hi, Anna here.", "the text is trimmed");
    assert_eq!(b["delivery"], "delivered");
    assert_eq!(b["client_msg_id"], client.to_string());
    assert_eq!(b["case_id"], "CS-1");
    assert_eq!(b["author_name"], "u-agent");
    assert!(b["at"].as_str().unwrap().ends_with('Z'), "{b}");
    assert_eq!(
        c.texts(&format!(
            "SELECT state || ':' || (first_response_at IS NOT NULL) FROM agent_handover_items WHERE id = '{item}'"
        ))
        .await,
        ["waiting_customer:true"]
    );
    assert_eq!(c.edge.delivered().len(), 1);
    let frames = drain(&mut rx);
    let m = frames
        .iter()
        .find(|f| f.event == "message")
        .expect("a message frame");
    assert_eq!(m.data["client_msg_id"], client.to_string(), "{:?}", m.data);
    assert_eq!(m.data["message_id"], b["message_id"]);
    assert_eq!(m.data["role"], "staff");
    assert_eq!(m.data["kind"], "staff");
    let audit: Vec<Value> = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE action = 'conversation.staff_reply'",
    )
    .fetch_all(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["item_id"], item.as_str());
    assert!(!audit[0].to_string().contains("Anna"), "no text in audit");
}

#[tokio::test]
async fn reply_by_non_owner_403() {
    let c = conv::setup().await;
    let (conv_id, _) = queued(&c, "wc-1").await;
    let (s, b) = c
        .send_message(&conv_id, "u-agent", "reply", "Hello", None)
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "not_owner"),
        "the queue owns it"
    );
    c.take(&conv_id, "u-agent", "queue").await;
    let (s, b) = c
        .send_message(&conv_id, "u-agent2", "reply", "Hello", None)
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "not_owner"));
    // a reply needs support_agent or support_manager before ownership is asked
    for sub in ["u-obs", "u-int"] {
        let (s, b) = c.send_message(&conv_id, sub, "reply", "Hello", None).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{sub}: {b}");
        assert_eq!(b["error"]["missing_role"], "support_agent", "{sub}: {b}");
    }
    let (s, b) = c
        .send_message(&conv_id, "u-agent", "shout", "Hello", None)
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_kind")
    );
    let (s, b) = c
        .send_message("conv-nope", "u-agent", "reply", "Hello", None)
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "conversation_not_found")
    );
    // without a kind the path is still the staff test chat, which serves only its own conversations
    let (s, b) = c
        .call(
            "POST",
            &format!("/api/jobs/j1/conversations/{conv_id}/messages"),
            "u-agent",
            json!({"text": "Hello"}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "conversation_not_found")
    );
    assert_eq!(c.edge.delivered().len(), 0);
    assert!(
        c.texts("SELECT text FROM conversation_message WHERE role IN ('staff', 'note')")
            .await
            .is_empty(),
        "nothing refused was stored"
    );
}

#[tokio::test]
async fn same_client_msg_id_is_idempotent() {
    let c = conv::setup().await;
    let (conv_id, _) = queued(&c, "wc-1").await;
    c.take(&conv_id, "u-agent", "queue").await;
    let client = Uuid::new_v4();
    let (s1, a) = c
        .send_message(&conv_id, "u-agent", "reply", "Once", Some(client))
        .await;
    let (s2, b) = c
        .send_message(&conv_id, "u-agent", "reply", "Once", Some(client))
        .await;
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK), "{a} {b}");
    assert_eq!(a["message_id"], b["message_id"]);
    assert_eq!(b["delivery"], "delivered");
    assert_eq!(c.edge.delivered().len(), 1, "delivered once");
    // a retry after the owner changed still answers the stored message
    let (s, x) = c.hand_back(&conv_id, "u-agent", "over to you").await;
    assert_eq!(s, StatusCode::OK, "{x}");
    let (s, b) = c
        .send_message(&conv_id, "u-agent", "reply", "Once", Some(client))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(a["message_id"], b["message_id"]);
    // two notes with one id at the same time: one row, both answers name it
    let note = Uuid::new_v4();
    let ((s1, a), (s2, b)) = tokio::join!(
        c.send_message(&conv_id, "u-agent", "note", "Twice?", Some(note)),
        c.send_message(&conv_id, "u-agent2", "note", "Twice?", Some(note)),
    );
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK), "{a} {b}");
    assert_eq!(a["message_id"], b["message_id"]);
    assert_eq!(
        c.texts(&format!(
            "SELECT client_msg_id::text FROM conversation_message WHERE client_msg_id IN ('{client}', '{note}') \
              ORDER BY client_msg_id::text"
        ))
        .await
        .len(),
        2,
        "one row per client id"
    );
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action IN ('conversation.staff_reply', 'conversation.note')",
    )
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(n, 2, "audited once each");
}

#[tokio::test]
async fn note_is_stored_never_delivered() {
    let c = conv::setup().await;
    let (conv_id, item) = queued(&c, "wc-1").await;
    c.take(&conv_id, "u-agent", "queue").await;
    for (sub, role) in [("u-op", "operator"), ("u-des", "job_designer")] {
        common::insert_user(&c.e.owner, sub, c.t, role).await;
    }
    // not the owner, and not even support_agent: a note is open to every staff role that works a conversation
    for sub in ["u-agent2", "u-mgr", "u-op", "u-des", "u-agent"] {
        let (s, b) = c
            .send_message(
                &conv_id,
                sub,
                "note",
                &format!("check the order ({sub})"),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{sub}: {b}");
        assert_eq!(
            (b["kind"].as_str(), &b["delivery"]),
            (Some("note"), &Value::Null),
            "{b}"
        );
    }
    for sub in ["u-obs", "u-int"] {
        let (s, b) = c.send_message(&conv_id, sub, "note", "x", None).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{sub}: {b}");
        assert_eq!(
            (s, code(&b)),
            (StatusCode::FORBIDDEN, "note_not_allowed"),
            "{sub}: {b}"
        );
    }
    assert_eq!(c.edge.delivered().len(), 0, "a note never reaches the edge");
    assert_eq!(
        c.texts("SELECT role FROM conversation_message WHERE text LIKE 'check the order%'")
            .await,
        ["note"; 5]
    );
    // the item does not move: no first response, still assigned
    assert_eq!(
        c.texts(&format!(
            "SELECT state || ':' || (first_response_at IS NOT NULL) FROM agent_handover_items WHERE id = '{item}'"
        ))
        .await,
        ["assigned:false"]
    );
    // nor is it the agent's history or the last customer-facing line
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    let history = store::history(&mut tx, &conv_id, 20).await.unwrap();
    let last = store::last_outbound(&mut tx, &conv_id).await.unwrap();
    tx.commit().await.unwrap();
    assert!(history.iter().all(|m| m.role != "note"), "{history:?}");
    assert_ne!(last.map(|(role, _)| role).as_deref(), Some("note"));
    // the staff transcript shows it
    let (s, t) = c
        .get(&format!("/api/jobs/j1/conversations/{conv_id}"), "u-obs")
        .await;
    assert_eq!(s, StatusCode::OK, "{t}");
    assert!(
        t["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["kind"] == "note"),
        "{t}"
    );
    let audit: Vec<Value> =
        sqlx::query_scalar("SELECT details FROM audit_event WHERE action = 'conversation.note'")
            .fetch_all(&c.e.owner)
            .await
            .unwrap();
    assert_eq!(audit.len(), 5);
    assert!(
        audit
            .iter()
            .all(|d| !d.to_string().contains("check the order")),
        "no text in audit"
    );
}

#[tokio::test]
async fn note_is_never_delivered_on_any_binding() {
    let c = conv::setup().await;
    // the web chat customer stream of the web chat conversation, open before anything is sent
    let (wc, _) = c.customer_case("wc-1", "wc-1-1", "Hello").await;
    let q = format!("tenant={}&job_id={}&binding_id={}", c.t, c.job, c.wc);
    let url = format!("/api/internal/channel/webchat/conversations/{wc}/events?{q}");
    let r = tokio::time::timeout(
        WAIT,
        c.e.app.clone().oneshot(common::get(&url, Some(EDGE_KEY))),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let mut stream = r.into_body();
    for (ch, ext) in [("web_chat", "wc-1"), ("telegram", "tg-1")] {
        let id = if ch == "web_chat" {
            wc.clone()
        } else {
            on_channel(&c, ch, ext).await
        };
        let before = c.edge.delivered().len();
        let (s, b) = c
            .send_message(&id, "u-agent", "note", &format!("internal-{ch}"), None)
            .await;
        assert_eq!(s, StatusCode::OK, "{ch}: {b}");
        assert_eq!(
            c.edge.delivered().len(),
            before,
            "{ch}: a note never reaches the edge"
        );
        // the conversation does deliver: the owner's reply after the note goes out
        let (s, b) = c.take(&id, "u-agent", "agent").await;
        assert_eq!(s, StatusCode::OK, "{ch}: {b}");
        let (s, b) = c
            .send_message(&id, "u-agent", "reply", &format!("public-{ch}"), None)
            .await;
        assert_eq!(s, StatusCode::OK, "{ch}: {b}");
        assert_eq!(b["delivery"], "delivered", "{ch}: {b}");
        let d = c.edge.delivered();
        assert_eq!(d.len(), before + 1, "{ch}: {d:?}");
        assert!(
            !serde_json::to_string(&d).unwrap().contains("internal-"),
            "{ch}: {d:?}"
        );
    }
    // the web chat customer stream carried the reply, never the note
    let seen = read_until(&mut stream, "public-web_chat").await;
    assert!(seen.contains("public-web_chat"), "{seen}");
    assert!(!seen.contains("internal-"), "{seen}");
    // the customer's history read (the widget's reconnect) leaves it out
    let (s, h) = c
        .internal_get(&format!(
            "/api/internal/channel/conversations/{wc}/messages?{q}"
        ))
        .await;
    assert_eq!(s, StatusCode::OK, "{h}");
    let texts: Vec<&str> = h["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["text"].as_str())
        .collect();
    assert!(texts.contains(&"public-web_chat"), "{h}");
    assert!(!texts.iter().any(|t| t.starts_with("internal-")), "{h}");
}

#[tokio::test]
async fn a_note_is_never_in_the_worker_handover_excerpt() {
    let c = conv::setup().await;
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
    let (id, _) = c.customer_case("wc-1", "wc-1-1", "My parcel is late").await;
    let (s, b) = c
        .send_message(&id, "u-agent", "note", "internal-vip customer", None)
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = c
        .inbound("wc-1", "wc-1-2", "Can I speak to a real person?")
        .await;
    assert_eq!(
        (s, b["status"].as_str()),
        (StatusCode::OK, Some("queued")),
        "{b}"
    );
    // the worker notify runs off the inbound request (spawned)
    let mut notified = Vec::new();
    for _ in 0..200 {
        notified = c.edge.notified();
        if !notified.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(notified.len(), 1, "the worker channel was notified");
    let excerpt = notified[0]["item"]["excerpt"].as_array().unwrap().clone();
    assert!(
        excerpt.iter().any(|l| l["text"] == "My parcel is late"),
        "{excerpt:?}"
    );
    assert!(
        excerpt
            .iter()
            .all(|l| l["role"] != "note" && !l["text"].as_str().unwrap_or("").contains("internal-")),
        "{excerpt:?}"
    );
}

#[tokio::test]
async fn telegram_limit_4000() {
    let c = conv::setup().await;
    let id = on_channel(&c, "telegram", "tg-1").await;
    c.take(&id, "u-agent", "agent").await;
    let (s, b) = c
        .send_message(&id, "u-agent", "reply", &"a".repeat(4001), None)
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "message_too_long")
    );
    assert_eq!(b["error"]["limit"], 4000, "{b}");
    let (s, b) = c
        .send_message(&id, "u-agent", "reply", &"a".repeat(4000), None)
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // a note is not sent on the channel: the stored limit applies
    let (s, b) = c
        .send_message(&id, "u-agent", "note", &"n".repeat(4001), None)
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // every other channel's the stored limit
    let (wc, _) = c.customer_case("wc-1", "wc-1-1", "Hello").await;
    c.take(&wc, "u-agent", "agent").await;
    let (s, b) = c
        .send_message(
            &wc,
            "u-agent",
            "reply",
            &"a".repeat(store::MAX_TEXT + 1),
            None,
        )
        .await;
    assert_eq!(
        (s, b["error"]["limit"].as_u64()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some(store::MAX_TEXT as u64)
        )
    );
}

#[tokio::test]
async fn failed_delivery_returns_failed_state() {
    use std::sync::atomic::Ordering;
    let c = conv::setup().await;
    let (conv_id, _) = queued(&c, "wc-1").await;
    c.take(&conv_id, "u-agent", "queue").await;
    c.edge.fail.store(true, Ordering::SeqCst);
    let mut rx = c.e.state.events.subscribe();
    let client = Uuid::new_v4();
    let (s, b) = c
        .send_message(&conv_id, "u-agent", "reply", "Are you there?", Some(client))
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["delivery"], "failed");
    assert_eq!(b["client_msg_id"], client.to_string());
    let mid = b["message_id"].as_str().unwrap().to_string();
    assert_eq!(
        c.texts(&format!(
            "SELECT text || ':' || (delivered_at IS NULL) || ':' || (delivery_error IS NOT NULL) \
               FROM conversation_message WHERE message_id = '{mid}'"
        ))
        .await,
        ["Are you there?:true:true"],
        "the message is kept, undelivered, with its error"
    );
    let frames = drain(&mut rx);
    let failed: Vec<&Frame> = frames
        .iter()
        .filter(|f| f.event == "delivery_failed")
        .collect();
    assert_eq!(failed.len(), 1, "{frames:?}");
    let mut keys: Vec<&String> = failed[0].data.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["code", "conversation_id", "message_id"],
        "{:?}",
        failed[0].data
    );
    assert_eq!(failed[0].data["conversation_id"], conv_id.as_str());
    assert_eq!(failed[0].data["message_id"], mid.as_str());
    assert!(failed[0].data["code"].is_string());
    // the same Send again while the channel still fails: one new attempt, the same message, still failed
    let attempts = c.edge.delivered().len();
    let (s, b) = c
        .send_message(&conv_id, "u-agent", "reply", "Are you there?", Some(client))
        .await;
    assert_eq!(
        (s, b["message_id"].as_str(), b["delivery"].as_str()),
        (StatusCode::ACCEPTED, Some(mid.as_str()), Some("failed")),
        "{b}"
    );
    assert_eq!(c.edge.delivered().len(), attempts + 1);
}

#[tokio::test]
async fn failed_reply_retry_redelivers() {
    use std::sync::atomic::Ordering;
    let c = conv::setup().await;
    let (conv_id, _) = queued(&c, "wc-1").await;
    c.take(&conv_id, "u-agent", "queue").await;
    let client = Uuid::new_v4();
    c.edge.fail.store(true, Ordering::SeqCst);
    let (s, first) = c
        .send_message(&conv_id, "u-agent", "reply", "Still there?", Some(client))
        .await;
    assert_eq!(
        (s, first["delivery"].as_str()),
        (StatusCode::ACCEPTED, Some("failed")),
        "{first}"
    );
    // a colleague cannot push the owner's failed reply out
    c.edge.fail.store(false, Ordering::SeqCst);
    let (s, b) = c
        .send_message(&conv_id, "u-agent2", "reply", "Still there?", Some(client))
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "not_owner"), "{b}");
    let mut rx = c.e.state.events.subscribe();
    let (s, b) = c
        .send_message(&conv_id, "u-agent", "reply", "Still there?", Some(client))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["message_id"], first["message_id"]);
    assert_eq!(b["delivery"], "delivered");
    assert_eq!(
        c.texts(&format!(
            "SELECT (delivered_at IS NOT NULL) || ':' || (delivery_error IS NULL) FROM conversation_message \
              WHERE client_msg_id = '{client}'"
        ))
        .await,
        ["true:true"],
        "one row, delivered, its error cleared"
    );
    // every staff viewer learns the new state: the message frame again, delivered
    let frames = drain(&mut rx);
    let m = frames
        .iter()
        .find(|f| f.event == "message")
        .expect("a message frame after the delivery");
    assert_eq!(m.data["message_id"], first["message_id"]);
    assert_eq!(m.data["client_msg_id"], client.to_string());
    assert_eq!(m.data["delivery"], "delivered");
    // delivered: a further retry changes nothing and sends nothing
    let sent = c.edge.delivered().len();
    let (s, b) = c
        .send_message(&conv_id, "u-agent", "reply", "Still there?", Some(client))
        .await;
    assert_eq!(
        (s, b["delivery"].as_str()),
        (StatusCode::OK, Some("delivered"))
    );
    assert_eq!(c.edge.delivered().len(), sent);
}

#[tokio::test]
async fn message_view_types_system_rows() {
    let c = conv::setup().await;
    let (conv_id, _) = queued(&c, "wc-1").await;
    c.take(&conv_id, "u-agent", "queue").await;
    let id: Uuid = sqlx::query_scalar(
        "SELECT message_id FROM conversation_message WHERE role = 'system' AND author = 'owner'",
    )
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    let v = store::message_view(&mut tx, id).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        (
            v.kind.as_str(),
            v.event.as_deref(),
            v.author_name.as_deref(),
            v.delivery
        ),
        ("system", Some("owner"), None, None)
    );
    assert_eq!(v.text, "u-agent took this conversation");
}
