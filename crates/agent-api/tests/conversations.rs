//! B12 conversations (crate::conversations): a customer message through the internal door the agent would answer is
//! a runtime turn (D109 — here job `j1` has no live release, so 409 `job_not_live` and nothing stored; the runtime's
//! answers are tests/b4/channels_runtime.rs and tests/kh t6_answer_path); provider-retry dedupe; "talk to a person"
//! queues an item once; messages while a staff member owns the conversation are queued (SW T2 Take / Hand back);
//! CSAT request on resolve, its record and its codes. Real Postgres 18 (one container for this binary); the edge and the playground backend are fakes
//! (tests/common/conv.rs) — the playground backend never sees a customer.
use crate::common;
use crate::redis_env;

use agent_api::conversations::{csat, store};
use axum::http::StatusCode;
use common::conv::{self, Ctx, code};
use serde_json::json;

#[tokio::test]
async fn inbound_of_a_job_without_a_live_release_is_409_and_stores_nothing() {
    let c = conv::setup().await;
    let (s, b) = c.inbound("wc-1", "m1", "Where is my order?").await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "job_not_live"), "{b}");
    assert_eq!(c.backend.calls(), 0, "never the playground backend");
    assert!(
        c.texts("SELECT role FROM conversation_message")
            .await
            .is_empty(),
        "nothing stored: the provider's retry is a fresh attempt"
    );
    assert!(
        c.texts("SELECT conversation_id FROM conversation")
            .await
            .is_empty()
    );
    // a member reads a customer's transcript
    let (conv_id, case_id) = c.customer_case("wc-1", "m1", "Where is my order?").await;
    assert_eq!(case_id, "CS-1");
    let (s, b) = c
        .get(&format!("/api/jobs/j1/conversations/{conv_id}"), "u-obs")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["messages"].as_array().unwrap().len(), 1);
    assert_eq!(b["messages"][0]["kind"], "customer", "{b}");
    assert_eq!(b["owner"]["owner"], "agent");
    assert_eq!(b["conversation"]["case_id"], "CS-1");
    assert_eq!(b["conversation"]["channel"], "web_chat");
}

#[tokio::test]
async fn a_provider_retry_is_a_duplicate() {
    let c = conv::setup().await;
    let (_, b) = c.inbound("wc-1", "m1", "Can I talk to a person?").await;
    assert_eq!(b["status"], "queued", "{b}");
    let (s, b) = c.inbound("wc-1", "m1", "Can I talk to a person?").await;
    assert_eq!(
        (s, b["status"].as_str()),
        (StatusCode::OK, Some("duplicate"))
    );
    assert_eq!(c.backend.calls(), 0);
    assert_eq!(
        c.texts("SELECT role FROM conversation_message WHERE role = 'customer'")
            .await
            .len(),
        1
    );
}

#[tokio::test]
async fn asking_for_a_person_queues_once_with_one_notice() {
    let c = conv::setup().await;
    // a worker target, so the item is announced to staff
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
    let (s, b) = c.inbound("wc-1", "m1", "I want to talk to a human").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "queued");
    assert_eq!(b["notice"], true);
    assert_eq!(b["item_id"], "HO-1");
    let (_, b) = c.inbound("wc-1", "m2", "hello?").await;
    assert_eq!(
        (b["status"].as_str(), b["notice"].as_bool()),
        (Some("queued"), Some(false))
    );
    assert_eq!(b["item_id"], "HO-1");
    assert_eq!(
        c.backend.calls(),
        0,
        "the agent does not answer a queued conversation"
    );
    assert_eq!(
        c.texts("SELECT reason FROM agent_handover_items").await,
        ["customer_asked"]
    );
    assert_eq!(c.texts("SELECT status FROM agent_case").await, ["handover"]);
    // the worker notify runs off the inbound request (spawned): wait for the thread it stores
    for _ in 0..200 {
        if !c
            .texts("SELECT external_id FROM channel_conversation_refs WHERE side = 'worker'")
            .await
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let n = c.edge.notified();
    assert_eq!(n.len(), 1, "workers notified once");
    assert_eq!(n[0]["item"]["id"], "HO-1");
    assert_eq!(n[0]["targets"][0]["kind"], "helpdesk");
    assert_eq!(
        c.texts("SELECT external_id FROM channel_conversation_refs WHERE side = 'worker'")
            .await,
        ["thread-HO-1"]
    );
    // the audit row carries no message text
    let d: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT details FROM audit_event WHERE action = 'case.handover'")
            .fetch_all(&c.e.owner)
            .await
            .unwrap();
    assert_eq!(d.len(), 1);
    assert!(!d[0].to_string().contains("human"), "{}", d[0]);
}

#[tokio::test]
async fn messages_while_staff_own_are_queued_until_hand_back() {
    let c = conv::setup().await;
    let (id, _) = c.customer_case("wc-1", "m1", "Hi").await;
    let (s, b) = c.take(&id, "u-agent", "agent").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "user:u-agent");
    assert_eq!(b["name"], "u-agent");
    let item = b["item_id"]
        .as_str()
        .expect("SW-3: Take queues an item")
        .to_string();
    let (_, t) = c
        .get(&format!("/api/jobs/j1/conversations/{id}"), "u-obs")
        .await;
    assert_eq!(t["owner"]["owner"], "user:u-agent", "{t}");
    assert_eq!(t["owner"]["name"], "u-agent", "{t}");
    assert_eq!(t["owner"]["item_id"], item.as_str(), "{t}");
    let (_, b) = c.inbound("wc-1", "m2", "Are you there?").await;
    assert_eq!(b["status"], "queued", "{b}");
    assert_eq!(
        b["item_id"],
        item.as_str(),
        "the owner's item holds the message"
    );
    assert_eq!(b["notice"], false);
    assert_eq!(c.backend.calls(), 0);
    let (s, b) = c
        .hand_back(&id, "u-agent", "Answer the delivery question")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "agent");
    let (_, t) = c
        .get(&format!("/api/jobs/j1/conversations/{id}"), "u-obs")
        .await;
    assert_eq!(t["owner"]["owner"], "agent", "{t}");
    assert!(t["owner"]["item_id"].is_null(), "{t}");
    // back with the agent: the runtime answers (D109; this job has no live release)
    let (s, b) = c.inbound("wc-1", "m3", "Thanks").await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "job_not_live"), "{b}");
}

/// Queue HO-1 on conversation `wc`, take it as u-agent, resolve it: the CSAT request goes out.
async fn resolved(c: &conv::Ctx, wc: &str) -> String {
    let (_, b) = c
        .inbound(wc, &format!("{wc}-m1"), "talk to someone please")
        .await;
    assert_eq!(b["status"], "queued", "{b}");
    let id = b["conversation_id"].as_str().unwrap().to_string();
    let item = b["item_id"].as_str().unwrap().to_string();
    let (s, b) = c.take(&id, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = c
        .resolve(&item, "u-agent", json!({"outcome": "resolved"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["owner"], "agent", "{b}");
    assert_eq!(c.item(&item).await["state"], "resolved");
    id
}

#[tokio::test]
async fn resolve_requests_csat_and_a_lone_digit_records_it() {
    let c = conv::setup().await;
    let id = resolved(&c, "wc-1").await;
    let d = c.edge.delivered();
    let asks: Vec<_> = d
        .iter()
        .filter(|x| x["message"]["kind"] == "csat_request")
        .collect();
    assert_eq!(asks.len(), 1, "{d:?}");
    assert_eq!(asks[0]["conversation"]["id"], "wc-1");
    assert_eq!(c.texts("SELECT status FROM agent_case").await, ["resolved"]);
    let (s, b) = c.inbound("wc-1", "m-rate", "4").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "csat_recorded");
    let rating: Option<i16> = sqlx::query_scalar("SELECT rating FROM case_csat")
        .fetch_one(&c.e.owner)
        .await
        .unwrap();
    assert_eq!(rating, Some(4));
    // submitCsat after the answer
    let (s, b) = c
        .call(
            "POST",
            &format!("/api/jobs/j1/conversations/{id}/csat"),
            "u-obs",
            json!({"case_id": "CS-1", "rating": 5}),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "csat_answered"));
}

#[tokio::test]
async fn submit_csat_codes() {
    let c = conv::setup().await;
    let id = resolved(&c, "wc-1").await;
    let url = format!("/api/jobs/j1/conversations/{id}/csat");
    let (s, b) = c
        .call(
            "POST",
            &url,
            "u-obs",
            json!({"case_id": "CS-1", "rating": 9}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_rating")
    );
    let (s, b) = c
        .call(
            "POST",
            &url,
            "u-obs",
            json!({"case_id": "CS-99", "rating": 3}),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "csat_not_requested"));
    let (s, b) = c
        .call(
            "POST",
            "/api/jobs/j1/conversations/conv-nope/csat",
            "u-obs",
            json!({"case_id": "CS-1", "rating": 3}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "conversation_not_found")
    );
    sqlx::query(
        "UPDATE case_csat SET requested_at = now() - interval '2 days', expires_at = now() - interval '1 day'",
    )
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (s, b) = c
        .call(
            "POST",
            &url,
            "u-obs",
            json!({"case_id": "CS-1", "rating": 3}),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "csat_expired"));
    // an expired request no longer catches a lone digit: the agent answers it (D109: the runtime; not live here)
    let (s, b) = c.inbound("wc-1", "m-late", "3").await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "job_not_live"), "{b}");
}

#[tokio::test]
async fn submit_csat_records_and_audits_the_rating_only() {
    let c = conv::setup().await;
    let id = resolved(&c, "wc-1").await;
    let (s, b) = c
        .call(
            "POST",
            &format!("/api/jobs/j1/conversations/{id}/csat"),
            "u-obs",
            json!({"case_id": "CS-1", "rating": 2}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["rating"], 2);
    let d: serde_json::Value =
        sqlx::query_scalar("SELECT details FROM audit_event WHERE action = 'case.csat'")
            .fetch_one(&c.e.owner)
            .await
            .unwrap();
    assert_eq!(d["rating"], 2);
}

#[tokio::test]
async fn refusals_pass_through() {
    let mut c = conv::setup().await;
    c.e.state.interaction = std::sync::Arc::new(agent_api::interaction::Unconfigured);
    c.e.app = agent_api::router(c.e.state.clone());
    let (s, b) = c.inbound("wc-1", "m1", "Hello").await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "job_not_live"));
    let (s, b) = c.inbound("wc-1", "m2", "   ").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "empty_message")
    );
}

// ---- review regressions (B12 review #3, #4, #5, #7) ---------------------------------------------------------------

#[tokio::test]
async fn a_refused_message_is_not_stored_so_the_provider_retry_is_a_fresh_attempt() {
    let c = conv::setup().await;
    for _ in 0..2 {
        let (s, b) = c.inbound("wc-1", "m1", "Where is my order?").await;
        assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "job_not_live"), "{b}");
    }
    assert!(
        c.texts("SELECT role FROM conversation_message")
            .await
            .is_empty()
    );
    assert_eq!(c.backend.calls(), 0);
}

#[tokio::test]
async fn a_digit_that_is_not_after_the_prompt_is_not_a_rating() {
    let c = conv::setup().await;
    resolved(&c, "wc-1").await;
    // the customer writes on before rating: a new case opens, and a later "4" is a normal message (the runtime's;
    // D109: this job has no live release)
    let (_, case_id) = c
        .customer_case("wc-1", "m-new", "One more question about my order")
        .await;
    assert_eq!(case_id, "CS-2");
    let (s, b) = c.inbound("wc-1", "m-num", "4").await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "job_not_live"), "{b}");
    let rating: Option<i16> = sqlx::query_scalar("SELECT rating FROM case_csat")
        .fetch_one(&c.e.owner)
        .await
        .unwrap();
    assert_eq!(rating, None);
}

#[tokio::test]
async fn control_characters_are_422_not_500() {
    let c = conv::setup().await;
    let (s, b) = c.inbound("wc-1", "m1", "hi\u{0}there").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_body")
    );
    let (s, b) = c.inbound("wc\u{1}1", "m2", "hi").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_body")
    );
    // tabs and newlines are text: past the body check, to the agent (D109: the runtime; not live here)
    let (s, b) = c.inbound("wc-1", "m3", "line one\nline two\ttab").await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "job_not_live"), "{b}");
    let (id, _) = c
        .customer_case("wc-1", "m3", "line one\nline two\ttab")
        .await;
    c.take(&id, "u-agent", "agent").await;
    let (s, b) = c
        .call(
            "POST",
            &format!("/api/jobs/j1/conversations/{id}/messages"),
            "u-agent",
            json!({"kind": "reply", "text": "nul\u{0}"}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_body")
    );
    let (s, b) = c
        .call(
            "POST",
            "/api/jobs/j1/channel-bindings",
            "u-int",
            json!({"kind": "web_chat", "side": "customer", "label": "x\u{0}"}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_body")
    );
}

#[tokio::test]
async fn inbound_needs_its_own_verified_binding() {
    let c = conv::setup().await;
    let body = |binding: serde_json::Value, ext: &str| {
        json!({
            "kind": "web_chat", "external_id": ext, "binding_id": binding, "tenant": c.t, "job_id": c.job,
            "job_key": "j1", "conversation": {"channel": "web_chat", "id": "wc-1", "route": {}},
            "message_id": "m1", "party": {"id": "cust-1", "verified": false}, "text": "hi",
        })
    };
    // another job's binding, a wrong account, an unknown id, a missing id
    sqlx::query("INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'j2', 'J2', '[]', 'test')")
        .bind(c.t)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let other: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO connector_bindings (tenant_id, job_id, kind, side, external_id, created_by, verified_at) \
         SELECT $1, id, 'web_chat', 'customer', 'wk_other', 'test', now() FROM jobs WHERE key = 'j2' RETURNING id",
    )
    .bind(c.t)
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    for (b, ext) in [
        (json!(other), "wk_other"),
        (json!(c.wc), "wk_wrong"),
        (json!(uuid::Uuid::new_v4()), "wk_test"),
    ] {
        let (s, r) = c
            .internal("/api/internal/channel/inbound", body(b, ext))
            .await;
        assert_eq!(
            (s, code(&r)),
            (StatusCode::NOT_FOUND, "binding_not_found"),
            "{ext}"
        );
    }
    let (s, r) = c
        .internal(
            "/api/internal/channel/inbound",
            body(serde_json::Value::Null, "wk_test"),
        )
        .await;
    assert_eq!((s, code(&r)), (StatusCode::BAD_REQUEST, "invalid_body"));
    // an unverified binding carries nothing
    sqlx::query(
        "UPDATE connector_bindings SET verified_at = NULL, verify_code = 'ABCDEF' WHERE id = $1",
    )
    .bind(c.wc)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (s, r) = c.inbound("wc-1", "m1", "hi").await;
    assert_eq!((s, code(&r)), (StatusCode::NOT_FOUND, "binding_not_found"));
    assert_eq!(c.backend.calls(), 0);
}

// ---- SW-6: the agent resolved the case and ARMED a request; the customer's next message decides ------------------
// The fixture is the runtime one (job j1 live and served): a message that is not CSAT is a real turn, answered with
// `TURN_FAILED` (no qualifier bound), and its transaction is committed, so a dismissal is observable.

async fn runtime(tag: &str) -> Ctx {
    conv::setup_runtime(redis_env::api_vars(tag).await).await
}

/// A resolved case on web chat conversation `wc` with the agent's request armed (`awaiting_confirmation`): the case id.
async fn armed(c: &Ctx, wc: &str) -> String {
    let (_, case_id) = c
        .customer_case(wc, &format!("{wc}-m0"), "Where is my order?")
        .await;
    sqlx::query("UPDATE agent_case SET status = 'resolved' WHERE case_id = $1")
        .bind(&case_id)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    assert!(csat::arm(&mut tx, &case_id).await.unwrap());
    tx.commit().await.unwrap();
    case_id
}

async fn csat_state(c: &Ctx, case_id: &str) -> String {
    c.texts(&format!(
        "SELECT state FROM case_csat WHERE case_id = '{case_id}'"
    ))
    .await
    .remove(0)
}

async fn rating_of(c: &Ctx, case_id: &str) -> Option<i16> {
    sqlx::query_scalar("SELECT rating FROM case_csat WHERE case_id = $1")
        .bind(case_id)
        .fetch_one(&c.e.owner)
        .await
        .unwrap()
}

/// `text` as customer message `msg` on `wc`: the request is asked (`csat_request`, the prompt).
async fn asked_by_acknowledging(c: &Ctx, wc: &str, text: &str) {
    let (s, b) = c.inbound(wc, &format!("{wc}-ack"), text).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "csat_request", "{b}");
    assert_eq!(b["text"], csat::PROMPT, "{b}");
}

#[tokio::test]
async fn an_acknowledgement_after_resolution_asks_for_the_rating() {
    let c = runtime("csat-ack").await;
    for (wc, text) in [("wc-en", "thanks!"), ("wc-nl", "bedankt")] {
        let case_id = armed(&c, wc).await;
        assert_eq!(csat_state(&c, &case_id).await, "awaiting_confirmation");
        asked_by_acknowledging(&c, wc, text).await;
        assert_eq!(csat_state(&c, &case_id).await, "asked");
        // the prompt is the conversation's last line, so the digit is the rating
        let (s, b) = c.inbound(wc, &format!("{wc}-rate"), "5").await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["status"], "csat_recorded", "{b}");
        assert_eq!(b["text"], csat::THANKS, "{b}");
        assert_eq!(rating_of(&c, &case_id).await, Some(5));
    }
}

#[tokio::test]
async fn a_new_question_after_resolution_dismisses_and_never_asks() {
    let c = runtime("csat-newq").await;
    let case_id = armed(&c, "wc-1").await;
    let (s, b) = c
        .inbound("wc-1", "m-q", "And where is my other parcel?")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "answered", "answered as a normal turn: {b}");
    assert_eq!(csat_state(&c, &case_id).await, "dismissed");
    // a later acknowledgement asks nothing: the conversation's one request is spent
    let (s, b) = c.inbound("wc-1", "m-thx", "thanks").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_ne!(b["status"], "csat_request", "{b}");
    let prompts = c
        .texts("SELECT text FROM conversation_message WHERE author = 'csat'")
        .await;
    assert!(prompts.is_empty(), "{prompts:?}");
}

#[tokio::test]
async fn a_word_reply_to_the_prompt_dismisses_and_a_later_digit_is_not_scored() {
    let c = runtime("csat-word").await;
    let case_id = armed(&c, "wc-1").await;
    asked_by_acknowledging(&c, "wc-1", "thanks").await;
    let (s, b) = c.inbound("wc-1", "m-word", "it was fine I guess").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "answered", "{b}");
    assert_eq!(csat_state(&c, &case_id).await, "dismissed");
    let (s, b) = c.inbound("wc-1", "m-digit", "4").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_ne!(b["status"], "csat_recorded", "{b}");
    assert_eq!(rating_of(&c, &case_id).await, None);
}

#[tokio::test]
async fn star_and_slash_forms_score() {
    let c = runtime("csat-forms").await;
    for (wc, text, want) in [
        ("wc-a", "4/5", 4),
        ("wc-b", "⭐⭐⭐", 3),
        ("wc-c", "5 stars", 5),
    ] {
        let case_id = armed(&c, wc).await;
        asked_by_acknowledging(&c, wc, "thanks").await;
        let (s, b) = c.inbound(wc, &format!("{wc}-rate"), text).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["status"], "csat_recorded", "{text}: {b}");
        assert_eq!(rating_of(&c, &case_id).await, Some(want), "{text}");
    }
}

#[tokio::test]
async fn a_low_score_asks_one_follow_up_and_stores_the_comment() {
    let c = runtime("csat-low").await;
    let case_id = armed(&c, "wc-1").await;
    asked_by_acknowledging(&c, "wc-1", "thanks").await;
    let (s, b) = c.inbound("wc-1", "m-low", "2").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "csat_recorded", "{b}");
    assert_eq!(b["text"], csat::FOLLOW_UP, "{b}");
    assert_eq!(rating_of(&c, &case_id).await, Some(2));
    let secret = "the courier left it with a neighbour nobody knows";
    let (s, b) = c.inbound("wc-1", "m-comment", secret).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "csat_recorded", "{b}");
    assert_eq!(b["text"], csat::COMMENT_THANKS, "{b}");
    let comment: Option<String> =
        sqlx::query_scalar("SELECT comment FROM case_csat WHERE case_id = $1")
            .bind(&case_id)
            .fetch_one(&c.e.owner)
            .await
            .unwrap();
    assert_eq!(comment.as_deref(), Some(secret));
    // the audit event names the case and the channel, never the text
    let details: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT details FROM audit_event WHERE action = 'case.csat_comment'")
            .fetch_all(&c.e.owner)
            .await
            .unwrap();
    assert_eq!(details.len(), 1, "{details:?}");
    assert!(!details[0].to_string().contains("courier"), "{details:?}");
    // a third message is a normal turn: the follow-up is asked once
    let (s, b) = c.inbound("wc-1", "m-third", "one more thing").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "answered", "{b}");
    let comment_after: Option<String> =
        sqlx::query_scalar("SELECT comment FROM case_csat WHERE case_id = $1")
            .bind(&case_id)
            .fetch_one(&c.e.owner)
            .await
            .unwrap();
    assert_eq!(comment_after.as_deref(), Some(secret));
}

#[tokio::test]
async fn a_question_after_the_follow_up_is_answered_not_kept_as_the_comment() {
    let c = runtime("csat-low-question").await;
    let case_id = armed(&c, "wc-1").await;
    asked_by_acknowledging(&c, "wc-1", "thanks").await;
    let (s, b) = c.inbound("wc-1", "m-low", "1").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["text"], csat::FOLLOW_UP, "{b}");
    let (s, b) = c.inbound("wc-1", "m-question", "Where is my refund?").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "answered", "a question is a turn: {b}");
    let comment: Option<String> =
        sqlx::query_scalar("SELECT comment FROM case_csat WHERE case_id = $1")
            .bind(&case_id)
            .fetch_one(&c.e.owner)
            .await
            .unwrap();
    assert_eq!(comment, None);
}

#[tokio::test]
async fn an_expired_request_is_dismissed_silently() {
    let c = runtime("csat-expired").await;
    let case_id = armed(&c, "wc-1").await;
    sqlx::query(
        "UPDATE case_csat SET requested_at = now() - interval '2 days', expires_at = now() - interval '1 day'",
    )
    .execute(&c.e.owner)
    .await
    .unwrap();
    // even an acknowledgement asks nothing once the request ran out: it is answered as a normal turn
    let (s, b) = c.inbound("wc-1", "m-late", "thanks").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "answered", "{b}");
    assert_eq!(csat_state(&c, &case_id).await, "dismissed");
}
