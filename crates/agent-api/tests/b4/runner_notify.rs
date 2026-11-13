//! Every J21 item the RUNNER queues itself — the per-case cap (step 7b, `tool_failed`), a false postcondition (step
//! 10b, `tool_failed`), a write with no answer (step 10, `unknown_outcome`) and `case.create{handover}` — tells the
//! job's worker channels after its commit (`handover::notify_workers`, the fake agent-edge of tests/common/conv), as
//! the orchestrator's do (tests/b4/orchestrator_notify.rs). The runner's state runs on the `agent_runtime` pool, as
//! the binary's does (`state::build_runtime`), so the notify's reads and the worker-thread upsert prove 0179's
//! grants. Mutation: dropping the runner's `notify_workers` call after `record` (or after `agent_case`) leaves
//! `notified` empty for that trigger.
use std::sync::Arc;
use std::time::Duration;

use agent_api::runtime::design::DesignCache;
use agent_api::runtime::domain::OpState as ResultState;
use agent_api::runtime::orchestrator::ProcedureChecks;
use agent_api::runtime::routes::ResolvedTarget;
use agent_api::runtime::runner::Runner;
use agent_api::runtime::runner::ops::{
    Executors, OpExecutor, OpOutcome, OpRequest, OpState, outcome,
};
use agent_api::state::AppState;
use futures_util::future::BoxFuture;
use serde_json::{Map, json};

use crate::b4::flow::{Fx, calls, orch, qualify};
use crate::b4::orchestrator_notify::{assert_notified, item_id, notifying_state};
use crate::b4::runner_checks::{
    Fake, P_OP, call, case_with_conversation, cond, conversation, one_step, order_step, setup,
};
use crate::redis_env as r;

/// A runner for the fixture's scope on `st` (the notifying `agent_runtime` state).
fn runner(fx: &Fx, st: AppState, x: Executors) -> Runner {
    Runner::new(fx.bus.clone(), st, Arc::new(DesignCache::default()))
        .unwrap()
        .with_consumer("runner-notify-test")
        .with_consume(r::fast())
        .with_executors(x)
}

/// A write that answers after the runner's timeout (and declares no verify read): `unknown_outcome`.
struct Slow;

impl OpExecutor for Slow {
    fn execute<'a>(
        &'a self,
        _req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            outcome(OpState::Ok, None, None, Map::new())
        })
    }
}

/// A second X call of the case is capped: the `tool_failed` item notifies the workers once.
#[tokio::test]
async fn the_case_cap_handover_notifies_the_workers() {
    let fx = setup("runner-notify-cap").await;
    let (e, st) = notifying_state(&fx).await;
    let msg = case_with_conversation(&fx, "CS-1").await;
    let refund = Fake::ok(json!({"refund_id": "RF-1"}));
    let rn = runner(
        &fx,
        st,
        Executors::default().register("payment.refund", refund.clone()),
    );
    let first = call(&fx, "CS-1", "r1", "payment.refund", Some(&msg), json!({}));
    assert_eq!(rn.handle(&first).await.unwrap().state, ResultState::Ok);
    assert!(e.notified().is_empty(), "an ok call queues nothing");
    let second = call(&fx, "CS-1", "r2", "payment.refund", Some(&msg), json!({}));
    let b = rn.handle(&second).await.unwrap();
    assert_eq!(b.outcome_code.as_deref(), Some("case_cap_reached"));
    assert_notified(
        &fx,
        &e,
        "CS-1",
        "tool_failed",
        "per-case limit reached for payment.refund",
    )
    .await;
    // a redelivery finds the stored outcome: no second item, no second notify
    rn.handle(&second).await.unwrap();
    assert_eq!(e.notified().len(), 1);
    assert_eq!(refund.hits(), 1);
}

/// An `ok` answer whose postcondition is false: the `tool_failed` item notifies the workers.
#[tokio::test]
async fn a_false_postcondition_handover_notifies_the_workers() {
    let fx = setup("runner-notify-post").await;
    let (e, st) = notifying_state(&fx).await;
    let lost = Fake::ok(json!({"status": "lost"}));
    let rn = runner(
        &fx,
        st,
        Executors::default().register("order.read", lost.clone()),
    );
    let mut step = order_step();
    step.postconditions = vec![cond(
        "delivered",
        "steps.order.out.status",
        "eq",
        json!("delivered"),
    )];
    // the orchestrator here has no runtime state: every notify below is the runner's
    let o = orch(&fx, one_step(step, ProcedureChecks::default()));
    conversation(&fx, "CS-1").await;
    qualify(&fx, &o, "CS-1").await;
    let c = calls(&fx, "CS-1").await.remove(0);
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.outcome_code.as_deref(), Some("postcondition_failed"));
    assert_eq!(lost.hits(), 1);
    assert_notified(
        &fx,
        &e,
        "CS-1",
        "tool_failed",
        "postcondition failed: delivered",
    )
    .await;
}

/// A write that did not answer and has no verify read: the `unknown_outcome` item notifies the workers.
#[tokio::test]
async fn an_unknown_outcome_handover_notifies_the_workers() {
    let fx = setup("runner-notify-unknown").await;
    let (e, st) = notifying_state(&fx).await;
    let msg = case_with_conversation(&fx, "CS-1").await;
    let rn = runner(&fx, st, Executors::default().register(P_OP, Arc::new(Slow)))
        .with_timeouts(Duration::from_millis(50), Duration::from_millis(50));
    let c = call(&fx, "CS-1", "launch", P_OP, Some(&msg), json!({}));
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::UnknownOutcome, "{res:?}");
    assert_notified(&fx, &e, "CS-1", "unknown_outcome", P_OP).await;
}

/// `case.create{handover}` (the agent_cases fallback): the item it creates notifies the workers; a second
/// `case.create` of the case finds it (`already_handed_over`) and sends nothing more.
#[tokio::test]
async fn a_case_create_handover_notifies_the_workers() {
    let fx = setup("runner-notify-case-create").await;
    let (e, st) = notifying_state(&fx).await;
    let msg = case_with_conversation(&fx, "CS-1").await;
    let rn = runner(&fx, st, Executors::default());
    let extra = json!({"variant": "handover",
                       "input": {"reason": "customer_asked", "subject": "Wants a person"}});
    let c = call(&fx, "CS-1", "h1", "case.create", Some(&msg), extra.clone());
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    let id = res.output.as_ref().unwrap()["handover_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(id, item_id(&fx, "CS-1").await);
    assert_notified(&fx, &e, "CS-1", "customer_asked", "case.create{handover}").await;
    let again = call(&fx, "CS-1", "h2", "case.create", Some(&msg), extra);
    let res = rn.handle(&again).await.unwrap();
    assert_eq!(
        res.output.as_ref().unwrap()["already_handed_over"],
        json!(true)
    );
    assert_eq!(e.notified().len(), 1, "an existing item is not re-sent");
}
