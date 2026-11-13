//! Staff Workspace T6: one projection everywhere. The console row, the handover queue item, the case row (list and
//! detail) and the transcript (`GET /api/jobs/{job}/conversations/{id}`, the web's `SupportTranscript`) read the
//! same owner (`ownership::view`) and the conversation's channel; the transcript's messages are typed `MessageView`s
//! (a `system` row is never a bubble), its episodes are the conversation's handover items, and the customer is named
//! without a raw channel id (`support::names`). Also 0192: a staff row records its author's sub. Real Postgres 18.
use crate::common;

use agent_api::conversations::store;
use agent_api::support::names::{display_name, reason_label};
use axum::http::StatusCode;
use common::conv::{self, Ctx};
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

/// The transcript of `conv`, read as the observer.
async fn transcript(c: &Ctx, conv: &str) -> Value {
    let (s, t) = c
        .get(&format!("/api/jobs/j1/conversations/{conv}"), "u-obs")
        .await;
    assert_eq!(s, StatusCode::OK, "{t}");
    t
}

/// The console row of `conv`.
async fn console_row(c: &Ctx, conv: &str) -> Value {
    let (s, page) = c.get("/api/jobs/j1/console/conversations", "u-obs").await;
    assert_eq!(s, StatusCode::OK, "{page}");
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["conversation_id"] == conv)
        .cloned()
        .unwrap_or_else(|| panic!("{conv} is not on the console: {page}"))
}

/// The case detail of `case_id`.
async fn case_detail(c: &Ctx, case_id: &str) -> Value {
    let (s, d) = c
        .get(&format!("/api/jobs/j1/cases/{case_id}"), "u-obs")
        .await;
    assert_eq!(s, StatusCode::OK, "{d}");
    d
}

#[tokio::test]
async fn owner_identical_on_all_four_projections() {
    let c = conv::setup().await;
    let (conv_id, item) = queued(&c, "wc-own").await;
    // queued: every surface says the queue owns it, with the item
    let it = c.item(&item).await;
    assert_eq!(it["owner"]["owner"], "queue", "{it}");
    assert_eq!(it["owner"]["item_id"], item.as_str(), "{it}");
    let (s, b) = c.take(&conv_id, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let row = console_row(&c, &conv_id).await;
    let it = c.item(&item).await;
    assert_eq!(it["conversation_id"], conv_id.as_str(), "{it}");
    let case_id = it["case_ref"].as_str().unwrap().to_string();
    let case = case_detail(&c, &case_id).await;
    let t = transcript(&c, &conv_id).await;
    let want = json!({"owner": "user:u-agent", "name": "u-agent", "since": b["since"], "item_id": item, "auto_assigned": false});
    assert_eq!(b, want, "the Take's answer");
    assert_eq!(row["owner"], want, "console row: {row}");
    assert_eq!(it["owner"], want, "handover item: {it}");
    assert_eq!(case["case"]["owner"], want, "case detail: {case}");
    assert_eq!(t["owner"], want, "transcript: {t}");
    assert_eq!(row["open_item_id"], item.as_str());
    assert!(
        row.get("control").is_none() && row.get("controller_name").is_none(),
        "{row}"
    );
    // the case list's rows carry it too
    let (_, cases) = c.get("/api/jobs/j1/cases", "u-obs").await;
    let listed = cases["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["case_id"] == case_id.as_str())
        .cloned()
        .unwrap();
    assert_eq!(listed["owner"], want, "{listed}");
    // after Hand back every surface reads the agent again, no item
    let (s, b) = c.hand_back(&conv_id, "u-agent", "Over to you").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let row = console_row(&c, &conv_id).await;
    let it = c.item(&item).await;
    let case = case_detail(&c, &case_id).await;
    let t = transcript(&c, &conv_id).await;
    for (what, o) in [
        ("console", &row["owner"]),
        ("item", &it["owner"]),
        ("case", &case["case"]["owner"]),
        ("transcript", &t["owner"]),
    ] {
        assert_eq!(*o, b, "{what}");
        assert_eq!(o["owner"], "agent", "{what}: {o}");
        assert!(o["item_id"].is_null(), "{what}: {o}");
    }
}

#[tokio::test]
async fn channel_identical_on_all_projections() {
    let c = conv::setup().await;
    let (conv_id, item) = queued(&c, "wc-chan").await;
    let case_id = c.item(&item).await["case_ref"]
        .as_str()
        .unwrap()
        .to_string();
    // a stale copy on the case row is not what any surface shows: the conversation's channel is
    sqlx::query("UPDATE agent_case SET channel = 'telegram' WHERE case_id = $1 AND tenant_id = $2")
        .bind(&case_id)
        .bind(c.t)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let it = c.item(&item).await;
    let case = case_detail(&c, &case_id).await;
    let row = console_row(&c, &conv_id).await;
    let t = transcript(&c, &conv_id).await;
    assert_eq!(it["channel"], "web_chat", "{it}");
    assert_eq!(case["case"]["channel"], "web_chat", "{case}");
    assert_eq!(case["envelope"]["channel"], "web_chat", "{case}");
    assert_eq!(row["channel"], "web_chat", "{row}");
    assert_eq!(t["conversation"]["channel"], "web_chat", "{t}");
    assert_eq!(t["conversation"]["conversation_id"], conv_id.as_str());
    assert_eq!(t["conversation"]["job"], "j1");
    assert_eq!(t["conversation"]["case_id"], case_id.as_str());
    assert_eq!(t["conversation"]["display_name"], "Femke", "{t}");
}

#[tokio::test]
async fn system_rows_are_typed_not_bubbles() {
    let c = conv::setup().await;
    let (conv_id, item) = queued(&c, "wc-sys").await;
    let (s, b) = c.take(&conv_id, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // a runtime `system` line (the agent_cases route), without a case
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    store::append(
        &mut tx,
        store::NewMessage {
            job_id: c.job,
            conversation_id: &conv_id,
            role: "system",
            author: "agent_cases",
            text: "case.create{handover} routed to agent_cases",
            case_id: None,
            external_id: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let (s, b) = c
        .send_message(&conv_id, "u-agent", "note", "check the order first", None)
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let t = transcript(&c, &conv_id).await;
    let msgs = t["messages"].as_array().unwrap();
    let typed = |m: &Value| {
        let kind = m["kind"].as_str().unwrap_or("");
        ["customer", "agent", "staff", "note", "system"].contains(&kind)
            && m["message_id"].is_string()
            && m["at"].is_string()
            && m.get("client_msg_id").is_some()
            && m.get("delivery").is_some()
    };
    assert!(msgs.iter().all(typed), "{t}");
    let sys: Vec<&Value> = msgs
        .iter()
        .filter(|m| m["event"] == "agent_cases")
        .collect();
    assert_eq!(sys.len(), 1, "{t}");
    assert_eq!(sys[0]["kind"], "system");
    assert!(sys[0]["author_name"].is_null(), "{}", sys[0]);
    assert!(sys[0]["case_id"].is_null(), "{}", sys[0]);
    // the Take's owner line is a typed system row too; every system row names its event, and only system rows do
    assert!(
        msgs.iter()
            .any(|m| m["kind"] == "system" && m["event"] == "owner"),
        "{t}"
    );
    for m in msgs {
        assert_eq!(m["kind"] == "system", m["event"].is_string(), "{m}");
    }
    assert!(
        msgs.iter()
            .any(|m| m["kind"] == "note" && m["author_name"] == "u-agent"),
        "{t}"
    );
    // the handover drawer's messages are the same typed rows
    let it = c.item(&item).await;
    let drawer = it["messages"].as_array().unwrap();
    assert!(drawer.iter().all(typed), "{it}");
    assert!(
        drawer
            .iter()
            .any(|m| m["kind"] == "system" && m["event"] == "agent_cases"),
        "{it}"
    );
}

#[tokio::test]
async fn transcript_episodes_split_by_item() {
    let c = conv::setup().await;
    let (conv_id, a) = queued(&c, "wc-ep").await;
    let (s, b) = c.take(&conv_id, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = c
        .resolve(&a, "u-agent", json!({"outcome": "resolved"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // the customer comes back and asks again: a new case, a new item
    let (s, b) = c
        .inbound("wc-ep", "wc-ep-2", "I want to talk to a human again")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "queued", "{b}");
    let bb = b["item_id"].as_str().unwrap().to_string();
    assert_ne!(a, bb);
    let t = transcript(&c, &conv_id).await;
    let eps = t["episodes"].as_array().unwrap();
    assert_eq!(eps.len(), 2, "{t}");
    assert_eq!(eps[0]["item_id"], a.as_str());
    assert_eq!(eps[1]["item_id"], bb.as_str());
    assert!(
        eps[0]["queued_at"].as_str().unwrap() <= eps[1]["queued_at"].as_str().unwrap(),
        "{t}"
    );
    assert!(eps[0]["closed_at"].is_string(), "{t}");
    assert_eq!(eps[0]["resolution"], "resolved");
    assert!(eps[1]["closed_at"].is_null(), "{t}");
    assert!(eps[1]["resolution"].is_null(), "{t}");
    for e in eps {
        assert_eq!(e["reason_label"], "Customer asked", "{e}");
        assert_eq!(e["details"]["reason"], "customer_asked", "{e}");
    }
    let (case_a, case_b) = (
        eps[0]["case_id"].as_str().unwrap(),
        eps[1]["case_id"].as_str().unwrap(),
    );
    assert_ne!(case_a, case_b, "{t}");
    // nothing is attributed to the wrong case: the resolution line is the first episode's, the new ask the second's
    let msgs = t["messages"].as_array().unwrap();
    let resolution = msgs
        .iter()
        .find(|m| m["event"] == "resolution")
        .unwrap_or_else(|| panic!("{t}"));
    assert_eq!(resolution["case_id"], case_a, "{resolution}");
    let ask = msgs
        .iter()
        .rfind(|m| m["kind"] == "customer")
        .unwrap_or_else(|| panic!("{t}"));
    assert_eq!(ask["text"], "I want to talk to a human again");
    assert_eq!(ask["case_id"], case_b, "{ask}");
    assert_eq!(t["conversation"]["case_id"], case_b, "the newest case");
    // the owner is the queue again, with the second item
    assert_eq!(t["owner"]["owner"], "queue");
    assert_eq!(t["owner"]["item_id"], bb.as_str());
}

#[tokio::test]
async fn transcript_names_a_customer_without_a_raw_id() {
    let c = conv::setup().await;
    // a Telegram customer whose profile gave no name
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    let cref = agent_api::interaction::ConversationRef {
        channel: "telegram".into(),
        id: "tg-123456".into(),
        route: Default::default(),
    };
    let conv = store::upsert_customer_conversation(
        &mut tx,
        c.job,
        "telegram",
        &cref,
        "tg-123456",
        None,
        "prod",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let t = transcript(&c, &conv.id).await;
    let chars: Vec<char> = conv.id.chars().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    assert_eq!(
        t["conversation"]["display_name"],
        format!("Telegram user · {tail}"),
        "{t}"
    );
    assert!(!t["conversation"].to_string().contains("tg-123456"), "{t}");
    assert_eq!(t["owner"]["owner"], "agent");
    assert_eq!(t["episodes"], json!([]));
    assert_eq!(t["messages"], json!([]));
    assert_eq!(
        console_row(&c, &conv.id).await["display_name"],
        format!("Telegram user · {tail}")
    );
}

#[tokio::test]
async fn transcript_keeps_the_staff_test_chat_fields() {
    let c = conv::setup().await;
    let (conv_id, case_id) = c.customer_case("wc-chat", "m1", "Where is my order?").await;
    let t = transcript(&c, &conv_id).await;
    assert_eq!(t["conversation_id"], conv_id.as_str());
    assert_eq!(t["tenant_id"], c.t.to_string());
    assert_eq!(t["job"], "j1");
    assert_eq!(t["customer_id"], "cust-1");
    assert_eq!(t["env"], "prod");
    assert_eq!(t["case_ids"], json!([case_id]));
    assert!(t["csat_request"].is_null(), "{t}");
    let m = &t["messages"][0];
    assert_eq!(m["kind"], "customer", "{m}");
    assert_eq!(m["role"], "customer", "{m}");
    assert_eq!(m["author"], "Femke", "{m}");
    assert_eq!(m["author_name"], "Femke", "{m}");
    assert_eq!(m["conversation_id"], conv_id.as_str(), "{m}");
    assert!(m["at_ms"].is_i64(), "{m}");
}

#[tokio::test]
async fn staff_rows_record_their_author() {
    let c = conv::setup().await;
    let (conv_id, _) = queued(&c, "wc-auth").await;
    let (s, b) = c.take(&conv_id, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = c
        .send_message(&conv_id, "u-agent", "reply", "Hello Femke", None)
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = c
        .send_message(&conv_id, "u-mgr", "note", "watch this one", None)
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        c.texts(&format!(
            "SELECT role || ':' || author_sub FROM conversation_message \
              WHERE conversation_id = '{conv_id}' AND author_sub IS NOT NULL ORDER BY at_ms, message_id"
        ))
        .await,
        ["staff:u-agent", "note:u-mgr"]
    );
    assert_eq!(
        c.texts(&format!(
            "SELECT role FROM conversation_message \
              WHERE conversation_id = '{conv_id}' AND author_sub IS NULL AND role IN ('staff', 'note')"
        ))
        .await
        .len(),
        0
    );
}

#[test]
fn display_name_fallbacks() {
    assert_eq!(
        display_name(None, None, "web_chat", "wc:79cc2204-aa-4f2a"),
        "Web visitor · 4f2a"
    );
    assert_eq!(
        display_name(None, None, "telegram", "tg-123456"),
        "Telegram user · 3456"
    );
    assert_eq!(
        display_name(Some("Ann"), Some("x"), "telegram", "tg-123456"),
        "Ann"
    );
    assert_eq!(display_name(None, Some("Bob"), "telegram", "tg-1"), "Bob");
    assert_eq!(
        display_name(Some("  "), None, "telegram", "tg-98765"),
        "Telegram user · 8765",
        "a blank name does not count"
    );
    assert_eq!(
        display_name(None, None, "staff", "c-1"),
        "Staff test chat user · c-1"
    );
    assert_eq!(display_name(None, None, "api", "ab"), "API user · ab");
    assert_eq!(reason_label("gate_unavailable"), "Gate service unavailable");
    assert_eq!(reason_label("not_offered"), "Required action not available");
}
