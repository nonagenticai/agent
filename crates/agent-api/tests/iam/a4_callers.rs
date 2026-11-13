//! A4 + A6 "Done when": an API key labelled for a board on a live job with `case_per_thread` and `confirmation: auto`
//! on the board's op. Two async `/interactions` with the same `conversation.id` are two tool-using turns of ONE case,
//! both answers delivered to a signed callback; the key is recorded as the caller; a party-less member request still
//! goes to the playground.
use serde_json::{Value, json};

use super::world::*;
use crate::common::{insert_user, json as req, person, send};

#[tokio::test]
async fn two_board_events_on_one_card_are_two_turns_of_one_case() {
    let script = tool_then_say(
        "case_comment",
        json!({"case_id": "card-42", "comment": "Moved to Doing"}),
        ANSWER,
    );
    let (w, key) = World::live("iam-a4-board", script, json!({})).await;
    let f = &w.f;
    let secret = w.webhook_secret(f).await;
    let mut rx = Receiver::start().await;

    for n in 0..2 {
        let (s, b) = w
            .keyed(
                f,
                &key,
                json!({"input": format!("Card card-42, event {n}: moved to Doing"),
                       "callback_url": rx.url, "conversation": {"id": "card-42"}}),
            )
            .await;
        assert_eq!(s, 202, "{b}");
        let (h, body) = rx.next().await;
        verify_signature(&secret, &h, &body);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["status"], "completed", "{v}");
        assert_eq!(v["conversation"]["id"], "card-42", "{v}");
        // NOTE: needs the assistant's words to reach the reply (runtime::reply::compose reads no facts.assistant)
        assert!(
            v["output_text"].as_str().unwrap_or("").contains(ANSWER),
            "{v}"
        );
    }
    rx.quiet(300).await;

    // two tool-using turns, one conversation, one case
    // turn 1: tool round + final round; turn 2 continues the case, whose history already holds the tool answer: one round
    assert_eq!(
        w.llm.first_rounds(),
        1,
        "only the first turn calls the tool"
    );
    assert_eq!(w.llm.assistant_bodies().len(), 3, "two assistant turns");
    assert_eq!(
        w.count("SELECT count(*) FROM agent_case WHERE job_id = $1", f)
            .await,
        1
    );
    assert_eq!(
        w.count("SELECT count(*) FROM conversation WHERE job_id = $1", f)
            .await,
        1
    );
    let calls = w.calls(f).await;
    assert!(
        calls.iter().any(|c| c.0 == BOARD && c.1 == "ok"),
        "the write ran with confirmation auto: {calls:?}"
    );
    assert!(!w.sys.hits().is_empty());

    // the key is the caller, as state; its messages are role 'caller'
    let conv: String =
        sqlx::query_scalar("SELECT conversation_id FROM conversation WHERE job_id = $1")
            .bind(f.job.job_id)
            .fetch_one(&w.e.owner)
            .await
            .unwrap();
    let state = w.state(f, &conv).await;
    let val = |k: &str| state.iter().find(|s| s.0 == k).and_then(|s| s.2.clone());
    assert_eq!(val("caller").as_deref(), Some(key.id.as_str()), "{state:?}");
    assert_eq!(
        val("caller_label").as_deref(),
        Some("Kanban board"),
        "{state:?}"
    );
    assert_eq!(
        val("identity_source").as_deref(),
        Some("api_key"),
        "{state:?}"
    );
    let roles: Vec<(String, i64)> = sqlx::query_as(
        "SELECT role, count(*) FROM conversation_message WHERE job_id = $1 AND role IN ('customer', 'caller') \
          GROUP BY role",
    )
    .bind(f.job.job_id)
    .fetch_all(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(roles, [("caller".to_string(), 2)]);

    // a member without a party is the draft playground: no case, no conversation
    let sub = format!("u-member-{}", f.tenant.simple());
    insert_user(&w.e.owner, &sub, f.tenant, "tenant_user").await;
    let (s, b) = send(
        &w.e.app,
        req(
            "POST",
            &format!("/api/jobs/{}/interactions", f.job.job_key),
            Some(&person(&sub, &[])),
            json!({"input": "Draft check"}),
        ),
    )
    .await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["output_text"], PLAYGROUND, "{b}");
    assert_eq!(w.play.calls(), 1);
    assert_eq!(
        w.count("SELECT count(*) FROM agent_case WHERE job_id = $1", f)
            .await,
        1
    );
}
