//! N-12 (one open J21 item per conversation) for `Orchestrator::hand_over` (audit of agent#54: the sibling
//! `queue_failed` was fixed, `hand_over` was not). The trigger is T6a's `not_offered` hand-over: a delivery
//! complaint on the default routes (no demo stub, `delivery.read` not offered) hands over before its first call.
//! A case that reaches `hand_over` while another case's item is open in its conversation queues nothing and
//! resolves `failed` (that item holds the conversation; a `handover` case with no item of its own would sit where
//! no colleague sees it).
use agent_api::runtime::Env;
use agent_api::runtime::domain::{CaseOutcome, QualifiedRequest};
use agent_api::runtime::orchestrator::StaticProcedures;
use serde_json::json;

use crate::b4::flow::{
    case_row, current_design, insert_case_in, orch, qualified, resolutions_in, setup_latest,
};

/// The template's delivery-problem space (area `logistics`, procedure `delivery_problem@1`).
const DELIVERY_AREA: &str = "logistics";

/// A late-delivery complaint for `case` in conversation `conv` (copied from tests/b4/flow.rs, private there).
fn delivery_request(case: &str, conv: &str) -> QualifiedRequest {
    serde_json::from_value(json!({
        "case_id": case, "conversation_id": conv, "area": DELIVERY_AREA,
        "context_space": "retail:logistics:delivery_problem", "request_type": "delivery_problem",
        "slots": {"order_id": "ORD-58790", "problem": "late"}, "customer_id": "C-10482",
        "identity_level": 2, "message_id": format!("msg-{case}")
    }))
    .unwrap()
}

/// The staff conversation `conv` of the fixture's job (the J21 item's).
async fn insert_conversation(fx: &crate::b4::flow::Fx, conv: &str) {
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, $3, $4, 'staff', 'C-10482')",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .bind(conv)
    .bind(fx.scope.env.as_str())
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
}

/// The number of J21 items of `case`.
async fn items_of_case(fx: &crate::b4::flow::Fx, case: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM agent_handover_items WHERE tenant_id = $1 AND case_id = $2",
    )
    .bind(fx.d.f.tenant)
    .bind(case)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap()
}

/// The number of J21 items of conversation `conv`.
async fn items_of_conversation(fx: &crate::b4::flow::Fx, conv: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM agent_handover_items WHERE tenant_id = $1 AND conversation_id = $2",
    )
    .bind(fx.d.f.tenant)
    .bind(conv)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap()
}

/// CS-1 hands over (`not_offered`) and holds the conversation's item; CS-2 of the same conversation then reaches
/// `hand_over`: no second item, CS-2 resolves `failed` with the hand-over code and a customer-safe reason, and
/// CS-1 keeps its item. Mutation: dropping the open-item check in `hand_over` queues a second item here.
#[tokio::test]
async fn a_hand_over_in_a_conversation_with_an_open_item_queues_no_second_item() {
    let fx = setup_latest("orch-handover-n12", Env::Test).await;
    insert_conversation(&fx, "conv-CS-1").await;
    let design = current_design(&fx).await;
    let o = orch(&fx, StaticProcedures);

    insert_case_in(&fx, "CS-1", "conv-CS-1", &design).await;
    o.on_qualified(qualified(&delivery_request("CS-1", "conv-CS-1")))
        .await
        .unwrap();
    let rs = resolutions_in(&fx, DELIVERY_AREA, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Handover, "{rs:?}");
    assert_eq!(items_of_case(&fx, "CS-1").await, 1);
    assert_eq!(case_row(&fx, "CS-1").await.0, "handover");

    insert_case_in(&fx, "CS-2", "conv-CS-1", &design).await;
    o.on_qualified(qualified(&delivery_request("CS-2", "conv-CS-1")))
        .await
        .unwrap();
    let rs = resolutions_in(&fx, DELIVERY_AREA, "CS-2").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Failed, "{rs:?}");
    assert_eq!(rs[0].code.as_deref(), Some("not_offered"));
    // the trigger ("procedure needs …") is internal: it must never become the customer-facing reason
    // (reply::compose prints it). Mutation: passing the trigger as the reason fails here.
    assert_eq!(rs[0].reason, None, "{rs:?}");
    assert!(
        !agent_api::runtime::reply::compose(&rs[0]).contains("procedure needs"),
        "{rs:?}"
    );
    assert_eq!(items_of_case(&fx, "CS-2").await, 0, "N-12");
    assert_eq!(items_of_case(&fx, "CS-1").await, 1);
    assert_eq!(items_of_conversation(&fx, "conv-CS-1").await, 1);
    assert_eq!(case_row(&fx, "CS-2").await.0, "failed");
    assert_eq!(case_row(&fx, "CS-1").await.0, "handover");
}

/// N-12 under concurrency: two cases of one conversation reaching `hand_over` at the same moment queue exactly one
/// item between them; the other case ends `failed`. Five conversations, all ten hand-overs at once. Mutation:
/// dropping the control lock in `hand_over` lets both inserts through.
#[tokio::test]
async fn hand_overs_of_one_conversation_at_once_queue_one_item() {
    let fx = setup_latest("orch-handover-n12-race", Env::Test).await;
    let o = orch(&fx, StaticProcedures);
    let design = current_design(&fx).await;
    let mut envs = Vec::new();
    for c in 1..=5 {
        let conv = format!("conv-R{c}");
        insert_conversation(&fx, &conv).await;
        // case ids match `^CS-[1-9][0-9]*$` (0060): CS-11/CS-12 … CS-51/CS-52
        for side in 1..=2 {
            let case = format!("CS-{c}{side}");
            insert_case_in(&fx, &case, &conv, &design).await;
            envs.push(qualified(&delivery_request(&case, &conv)));
        }
    }
    for r in futures_util::future::join_all(envs.into_iter().map(|e| o.on_qualified(e))).await {
        r.unwrap();
    }
    for c in 1..=5 {
        assert_eq!(
            items_of_conversation(&fx, &format!("conv-R{c}")).await,
            1,
            "N-12: conv-R{c}"
        );
        let mut statuses = Vec::new();
        for side in 1..=2 {
            statuses.push(case_row(&fx, &format!("CS-{c}{side}")).await.0);
        }
        statuses.sort();
        assert_eq!(statuses, ["failed", "handover"], "conv-R{c}");
    }
}
