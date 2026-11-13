//! B4 T7 — the case deadline sweeper (ported from care-bus-agents@96c608c `crates/carebus/tests/deadline.rs`) over
//! real Postgres 18 + Redis. Nothing answers `bus:op`, so dispatched calls stay `outstanding`. A swept case ends
//! `failed` on the `agent_case` row too.
use std::sync::Arc;
use std::time::Duration;

use agent_api::runtime::Env;
use agent_api::runtime::case::SWEEP_LEASE;
use agent_api::runtime::domain::{CaseOutcome, OpState, QualifiedRequest};
use agent_api::runtime::orchestrator::Orchestrator;
use redis::AsyncCommands;
use serde_json::json;

use crate::b4::flow::{
    Fx, OneStep, answer, calls, case_row, current_design, insert_case, orch, qualified, request,
    resolutions, setup,
};

fn with_deadline(fx: &Fx, ms: u64) -> Orchestrator<OneStep> {
    orch(fx, OneStep).with_case_deadline(Duration::from_millis(ms))
}

async fn qualify(fx: &Fx, o: &Orchestrator<OneStep>, case: &str) -> QualifiedRequest {
    insert_case(fx, case, &current_design(fx).await).await;
    let req = request(case);
    o.on_qualified(qualified(&req)).await.unwrap();
    req
}

async fn in_zset(fx: &Fx, o: &Orchestrator<OneStep>, case: &str) -> bool {
    let s: Option<f64> = fx
        .bus
        .conn()
        .zscore(o.cases().deadline_key(), case)
        .await
        .unwrap();
    s.is_some()
}

/// Answer the case's (only) call `ok`.
async fn ok_result(fx: &Fx, o: &Orchestrator<OneStep>, case: &str) {
    let call = calls(fx, case).await.pop().expect("a dispatched call");
    o.on_result(answer(
        &call,
        OpState::Ok,
        "ok",
        json!({"status": "shipped"}),
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn unanswered_call_fails_the_case_at_the_deadline() {
    // 3 s, not 300 ms: the "not overdue yet" sweep must run before the deadline even on a loaded CI runner
    // (300 ms flaked on main e7878d4: qualify + in_zset took longer and the first sweep already claimed it).
    let fx = setup("deadline-fail", Env::Test).await;
    let o = with_deadline(&fx, 3_000);
    qualify(&fx, &o, "CS-1").await;
    assert!(in_zset(&fx, &o, "CS-1").await);
    assert_eq!(o.sweep_once().await.unwrap(), 0, "not overdue yet");
    tokio::time::sleep(Duration::from_millis(3_100)).await;
    assert_eq!(o.sweep_once().await.unwrap(), 1);
    let rs = resolutions(&fx, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Failed);
    assert_eq!(rs[0].code.as_deref(), Some("deadline_exceeded"));
    assert_eq!(
        rs[0].reason.as_deref(),
        Some("order.read did not answer in time")
    );
    let err = o.cases().error("CS-1").await.unwrap().unwrap();
    assert!(
        err.contains("deadline exceeded") && err.contains("order.read"),
        "{err}"
    );
    assert!(!in_zset(&fx, &o, "CS-1").await);
    assert_eq!(case_row(&fx, "CS-1").await.0, "failed");
    assert_eq!(o.sweep_once().await.unwrap(), 0, "swept once only");
}

#[tokio::test]
async fn a_case_that_finishes_in_time_is_never_swept() {
    let fx = setup("deadline-ok", Env::Test).await;
    let o = with_deadline(&fx, 3_000);
    qualify(&fx, &o, "CS-1").await;
    assert!(in_zset(&fx, &o, "CS-1").await, "created with a deadline");
    ok_result(&fx, &o, "CS-1").await;
    assert_eq!(resolutions(&fx, "CS-1").await.len(), 1);
    assert!(
        !in_zset(&fx, &o, "CS-1").await,
        "resolution clears the deadline"
    );
    tokio::time::sleep(Duration::from_millis(3_100)).await;
    assert_eq!(o.sweep_once().await.unwrap(), 0);
    let rs = resolutions(&fx, "CS-1").await;
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0].outcome, CaseOutcome::Resolved);
    assert_eq!(case_row(&fx, "CS-1").await.0, "resolved");
}

#[tokio::test]
async fn two_replicas_sweeping_one_case_publish_one_resolution() {
    let fx = setup("deadline-race", Env::Test).await;
    let a = Arc::new(with_deadline(&fx, 200));
    let b = Arc::new(with_deadline(&fx, 200));
    qualify(&fx, &a, "CS-1").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (x, y) = tokio::join!(a.sweep_once(), b.sweep_once());
    assert_eq!(
        x.unwrap() + y.unwrap(),
        1,
        "exactly one replica claims the case"
    );
    assert_eq!(resolutions(&fx, "CS-1").await.len(), 1);
}

#[tokio::test]
async fn a_late_result_for_a_swept_case_changes_nothing() {
    let fx = setup("deadline-late", Env::Test).await;
    let o = with_deadline(&fx, 100);
    qualify(&fx, &o, "CS-1").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(o.sweep_once().await.unwrap(), 1);
    ok_result(&fx, &o, "CS-1").await;
    let rs = resolutions(&fx, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Failed);
    assert_eq!(case_row(&fx, "CS-1").await.0, "failed");
}

#[tokio::test]
async fn an_expired_case_hash_is_dropped_from_the_deadline_set() {
    let fx = setup("deadline-gone", Env::Test).await;
    let o = with_deadline(&fx, 100);
    qualify(&fx, &o, "CS-1").await;
    let _: i64 = fx.bus.conn().del(o.cases().key("CS-1")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(o.sweep_once().await.unwrap(), 0);
    assert!(!in_zset(&fx, &o, "CS-1").await);
    assert!(resolutions(&fx, "CS-1").await.is_empty());
}

#[tokio::test]
async fn a_claimed_case_stays_leased_and_is_retried_after_the_lease() {
    let fx = setup("deadline-lease", Env::Test).await;
    let o = with_deadline(&fx, 50);
    qualify(&fx, &o, "CS-1").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let cases = o.cases();
    let now = cases.now_ms().await.unwrap();
    assert!(cases.claim_overdue("CS-1", now).await.unwrap());
    assert!(
        !cases.claim_overdue("CS-1", now + 1).await.unwrap(),
        "leased: nobody else sweeps it"
    );
    assert!(
        in_zset(&fx, &o, "CS-1").await,
        "a claim never deletes the entry"
    );
    let after = now + i64::try_from(SWEEP_LEASE.as_millis()).unwrap() + 1;
    assert!(
        cases.claim_overdue("CS-1", after).await.unwrap(),
        "retried once the lease runs out"
    );
}

#[tokio::test]
async fn redelivering_a_created_case_keeps_one_deadline_entry() {
    let fx = setup("deadline-create", Env::Test).await;
    let o = with_deadline(&fx, 60_000);
    let req = qualify(&fx, &o, "CS-1").await;
    assert!(!o.cases().create(&req).await.unwrap());
    // a redelivered request.qualified dispatches nothing new: the same call id
    o.on_qualified(qualified(&req)).await.unwrap();
    let cs = calls(&fx, "CS-1").await;
    assert!(cs.iter().all(|c| c.call_id == cs[0].call_id), "{cs:?}");
    let n: i64 = fx.bus.conn().zcard(o.cases().deadline_key()).await.unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn a_sweep_racing_the_final_result_publishes_one_outcome() {
    let fx = setup("deadline-boundary", Env::Test).await;
    let a = with_deadline(&fx, 40);
    let b = with_deadline(&fx, 40);
    for i in 1..=5 {
        let case = format!("CS-{i}");
        qualify(&fx, &a, &case).await;
        tokio::time::sleep(Duration::from_millis(45)).await;
        let call = calls(&fx, &case).await.pop().unwrap();
        let (swept, answered) = tokio::join!(
            b.sweep_once(),
            a.on_result(answer(&call, OpState::Ok, "ok", json!({})))
        );
        swept.unwrap();
        answered.unwrap();
        let rs = resolutions(&fx, &case).await;
        assert!(!rs.is_empty(), "{case}: resolved");
        assert!(
            rs.iter().all(|r| r.outcome == rs[0].outcome),
            "{case}: one outcome, {rs:?}"
        );
        let status = case_row(&fx, &case).await.0;
        assert_eq!(status, rs[0].outcome.status(), "{case}: the row agrees");
    }
}
