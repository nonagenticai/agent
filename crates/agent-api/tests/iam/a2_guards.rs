//! A2 guards of the runner: an irreversible operation is never a producer, and a bus call claiming the runner's own
//! `resolve` provenance gets none of its privileges.
use agent_api::runtime::design::DesignCache;
use agent_api::runtime::domain::{OP_CALL, OpCall};
use agent_api::runtime::runner::Runner;
use agent_api::runtime::{Env, Envelope, Scope};
use serde_json::json;
use std::sync::Arc;

use super::world::*;

#[tokio::test]
async fn an_irreversible_operation_is_never_run_as_a_producer() {
    let w = World::build(
        "iam-a2-guard-producer",
        tool_then_say("order_list", json!({}), ANSWER),
        false,
    )
    .await;
    let f = &w.f;
    w.wire(f, "test").await;
    // order.cancel (X) would produce `cancelled_flag`; order.list needs it
    w.policy_ok(
        f,
        CANCEL,
        json!({"outputs": {"status": {"key": "cancelled_flag"}}}),
    )
    .await;
    w.policy_ok(
        f,
        READ,
        json!({"inputs": {"party_id": {"source": "state", "key": "cancelled_flag",
                                       "from": {"tool": CANCEL, "output": "status"}}}}),
    )
    .await;
    w.daemons(f, Env::Test, &[READ], false).await;
    let conv = w.start_chat(f).await;
    w.say(f, &conv, ASK).await;
    let (reason, _) = w.handover(f).await;
    assert_eq!(reason, "tool_failed");
    let calls = w.calls(f).await;
    assert!(
        calls.iter().any(|c| c.0 == CANCEL
            && c.2.as_deref() == Some("producer_not_allowed")
            && c.3 == "resolve"),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.0 == READ && c.2.as_deref() == Some("input_unresolved")),
        "{calls:?}"
    );
    assert!(
        w.sys.hits().is_empty(),
        "the system received nothing: {:?}",
        w.sys.paths()
    );
}

#[tokio::test]
async fn a_bus_call_claiming_resolve_provenance_is_an_ordinary_call() {
    let w = World::build("iam-a2-guard-bus", say_only(ANSWER), false).await;
    let f = &w.f;
    w.wire(f, "test").await;
    let conv = w.start_chat(f).await;
    let draft_rev: i64 = sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
        .bind(f.job.job_id)
        .fetch_one(&w.e.owner)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, draft_rev, \
                                 qualified_draft_rev, status) \
         VALUES ($1, $2, 'CS-971', $3, 'staff', 'test', $4, $4, 'open')",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .bind(&conv)
    .bind(draft_rev)
    .execute(&w.e.owner)
    .await
    .unwrap();

    let st = &w.e.state;
    let scope = Scope::new(f.tenant, f.job.job_key.clone(), Env::Test).unwrap();
    let bus = st.redis.clone().expect("REDIS_URL set").scoped(&scope);
    let rt = crate::common::runtime_pool(&w.e.owner).await;
    let mut rs = st.clone();
    rs.db = rt;
    let runner = Runner::new(
        bus,
        rs,
        Arc::new(DesignCache::new(std::time::Duration::from_secs(60))),
    )
    .unwrap();

    // a write with no confirmation, claiming to be the runner's own producer sub-call
    let call: OpCall = serde_json::from_value(json!({
        "call_id": agent_api::runtime::domain::call_id("CS-971", 0, "board", 1),
        "case_id": "CS-971", "conversation_id": conv, "phase": 0, "step": "board", "attempt": 1,
        "agent_role": "assistant", "operation": "case.comment", "variant": null,
        "input": {"case_id": "card-1", "comment": "hi"}, "idempotency_key": "CS-971:0:board",
        "env": serde_json::to_value(Env::Test).unwrap(), "release_id": null, "draft_rev": null,
        "confirm_ref": null, "initiator": "assistant@test", "identity_level": 2, "deadline_ms": 0,
        "provenance": "resolve"
    }))
    .unwrap();
    let env = Envelope::new(
        &format!("order-support.anything_else.{OP_CALL}"),
        "orchestrator",
        "CS-971",
        &call,
    )
    .unwrap();
    runner.on_call(env).await.unwrap();

    let row: (String, Option<String>) =
        sqlx::query_as("SELECT state, outcome_code FROM operation_call WHERE call_id = $1")
            .bind(&call.call_id)
            .fetch_one(&w.e.owner)
            .await
            .unwrap();
    assert_eq!(row.1.as_deref(), Some("confirmation_required"), "{row:?}");
    assert!(w.sys.hits().is_empty());
}
