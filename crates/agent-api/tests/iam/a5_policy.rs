//! A5 per-job runtime policy: the job's own system prompt reaches the model, `max_tool_calls` and `turn_budget_s`
//! end a turn in a handover, and a machine caller's turn is metered `turn` / `callers:api`.
use agent_api::runtime::Env;
use serde_json::{Value, json};

use super::world::*;

const MARKER: &str = "BOARD WORKER MARKER-7731: answer in one short line.";

#[tokio::test]
async fn the_jobs_system_prompt_reaches_the_model() {
    let w = World::build("iam-a5-prompt", say_only(ANSWER), false).await;
    let f = &w.f;
    w.role_setting(f, json!({"system_prompt": MARKER})).await;
    w.daemons(f, Env::Test, &[], true).await;
    let conv = w.start_chat(f).await;
    w.say(f, &conv, "Hello there").await;
    assert!(!w.agent_lines(&conv, 1).await.is_empty());
    let bodies = w.llm.assistant_bodies();
    assert!(!bodies.is_empty(), "the assistant ran");
    assert!(
        bodies.iter().all(|b| system_of(b).contains(MARKER)),
        "{bodies:?}"
    );
}

fn two_calls() -> Script {
    std::sync::Arc::new(|_: &Value| {
        (
            0,
            calls_msg(&[
                ("order_list", json!({"party_id": CUSTOMER})),
                ("order_list", json!({"party_id": CUSTOMER})),
            ]),
        )
    })
}

#[tokio::test]
async fn more_tool_calls_than_the_job_allows_hand_over() {
    let w = World::build("iam-a5-calls", two_calls(), false).await;
    let f = &w.f;
    w.wire(f, "test").await;
    w.role_setting(f, json!({"max_tool_calls": 1})).await;
    w.daemons(f, Env::Test, &[READ], true).await;
    let conv = w.start_chat(f).await;
    w.say(f, &conv, ASK).await;
    let (reason, trigger) = w.handover(f).await;
    assert_eq!(reason, "tool_failed");
    assert!(trigger.contains("tool-call budget (1)"), "{trigger}");
    assert!(w.sys.hits().is_empty(), "no tool was dispatched");
}

#[tokio::test]
async fn a_turn_over_its_time_budget_hands_over() {
    let script: Script = std::sync::Arc::new(|b: &Value| {
        if has_tool_result(b) {
            (0, final_msg(ANSWER))
        } else {
            (
                5_500,
                calls_msg(&[("order_list", json!({"party_id": CUSTOMER}))]),
            )
        }
    });
    let w = World::build("iam-a5-budget", script, false).await;
    let f = &w.f;
    w.wire(f, "test").await;
    w.role_setting(f, json!({"turn_budget_s": 5})).await;
    w.daemons(f, Env::Test, &[READ], true).await;
    let conv = w.start_chat(f).await;
    w.say(f, &conv, ASK).await;
    let (reason, trigger) = w.handover(f).await;
    assert_eq!(reason, "tool_failed");
    assert!(trigger.contains("turn budget"), "{trigger}");
}

#[tokio::test]
async fn a_machine_callers_turn_is_metered_as_turn_by_its_channel() {
    let script = tool_then_say(
        "case_comment",
        json!({"case_id": "card-7", "comment": "ok"}),
        ANSWER,
    );
    let (w, key) = World::live("iam-a5-usage", script, json!({})).await;
    let f = &w.f;
    let (s, b) = w
        .keyed(
            f,
            &key,
            json!({"input": "Card card-7 moved", "conversation": {"id": "card-7"}}),
        )
        .await;
    assert_eq!(s, 200, "{b}");
    let rows: Vec<(String, String, String)> = until("assistant usage", || async {
        let rows: Vec<(String, String, String)> =
            sqlx::query_as("SELECT source, actor, use FROM usage_event WHERE job_id = $1")
                .bind(f.job.job_id)
                .fetch_all(&w.e.owner)
                .await
                .unwrap();
        rows.iter().any(|r| r.2 == "assistant").then_some(rows)
    })
    .await;
    assert!(
        rows.iter().all(|r| r.0 == "turn" && r.1 == "callers:api"),
        "{rows:?}"
    );
}
