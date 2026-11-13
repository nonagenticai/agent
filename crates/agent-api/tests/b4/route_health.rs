//! Runner step 8, ROUTE HEALTH (B7 instance status, `runtime::runner::route_health`): a positively unhealthy
//! instance (`auth_failed`, `invalid_target`) is never called; the call answers `not_offered` with the reason
//! `route_unhealthy` on its row, its audit row and a counter. An unknown or untested state still routes. Reads are
//! cached for the staleness bound. Runs as `agent_runtime`.
use crate::common::platform;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_api::runtime::domain::{OpCall, OpResult, OpState as ResultState};
use agent_api::runtime::routes::{DEMO_STUB_INSTANCE, ResolvedTarget};
use agent_api::runtime::runner::Runner;
use agent_api::runtime::runner::ops::{
    Executors, OpExecutor, OpOutcome, OpRequest, OpState, outcome,
};
use agent_api::runtime::{Env, design::DesignCache};
use futures_util::future::BoxFuture;
use serde_json::json;

use crate::b4::design::put_target;
use crate::b4::flow::{Fx, current_design, insert_case};
use crate::redis_env as r;

const OP: &str = "delivery.report_problem";

/// Counts its calls and answers `ok` naming the routed instance.
struct Counting(Arc<AtomicUsize>);

impl OpExecutor for Counting {
    fn execute<'a>(
        &'a self,
        _req: &'a OpRequest,
        target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let mut out = outcome(
                OpState::Ok,
                None,
                None,
                json!({"case_id": "SP-3201", "status": "reported"})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            );
            if let ResolvedTarget::Instance { instance_id } = target {
                out.instance_id = Some(instance_id.clone());
            }
            out
        })
    }
}

/// The fixture: a case with its staff conversation and confirming message, the test-env stub instance (status
/// `status`, or the column default `untested` for None) and a working route of [`OP`] to it. The message id.
async fn fixture(fx: &Fx, case: &str, status: Option<&str>) -> String {
    insert_case(fx, case, &current_design(fx).await).await;
    let (t, j) = (fx.d.f.tenant, fx.d.f.job.job_id);
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, $3, $4, 'staff', 'C-1')",
    )
    .bind(t)
    .bind(j)
    .bind(format!("conv-{case}"))
    .bind(fx.scope.env.as_str())
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    // A1/A2: the customer is known through conversation STATE (`subject`), which the level-1+ write requires
    crate::common::state::identify(&fx.d.e.owner, t, j, &format!("conv-{case}"), "C-1", None).await;
    let msg: String = sqlx::query_scalar(
        "INSERT INTO conversation_message (tenant_id, job_id, conversation_id, role, text, case_id, at_ms) \
         VALUES ($1, $2, $3, 'customer', 'my parcel is late', $4, 1) RETURNING message_id::text",
    )
    .bind(t)
    .bind(j)
    .bind(format!("conv-{case}"))
    .bind(case)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    platform::stub_shop(&fx.d.e, t, DEMO_STUB_INSTANCE).await;
    if let Some(status) = status {
        set_status(fx, status).await;
    }
    sqlx::query(
        "INSERT INTO operation_route (tenant_id, job_id, object, verb, variant, env, target_kind, instance_id, \
                                      updated_by) \
         VALUES ($1, $2, 'delivery', 'report_problem', '*', $3, 'instance', $4, 'u-test')",
    )
    .bind(t)
    .bind(j)
    .bind(fx.scope.env.as_str())
    .bind(DEMO_STUB_INSTANCE)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    msg
}

fn runner(fx: &Fx, calls: &Arc<AtomicUsize>, ttl: Option<Duration>) -> Runner {
    let mut st = fx.d.e.state.clone();
    st.db = fx.rt.clone();
    let runner = Runner::new(fx.bus.clone(), st, Arc::new(DesignCache::default()))
        .unwrap()
        .with_consumer("route-health-test")
        .with_consume(r::fast())
        .with_executors(Executors::default().register(OP, Arc::new(Counting(Arc::clone(calls)))));
    match ttl {
        Some(ttl) => runner.with_route_health_ttl(ttl),
        None => runner,
    }
}

/// One write call of `case` at `step` (its own call id and idempotency key).
async fn call(runner: &Runner, case: &str, msg: &str, step: &str) -> OpResult {
    let c: OpCall = serde_json::from_value(json!({
        "call_id": agent_api::runtime::domain::call_id(case, 0, step, 1),
        "case_id": case, "conversation_id": format!("conv-{case}"), "phase": 0, "step": step,
        "attempt": 1, "agent_role": "procedure", "operation": OP, "variant": null,
        "input": {"delivery_id": "SHP-LATE-01", "problem": "Parcel is two days late"},
        "idempotency_key": format!("{case}:1:{step}"), "env": "test", "release_id": null, "draft_rev": null,
        "confirm_ref": msg, "initiator": "procedure@test", "identity_level": 3, "deadline_ms": 0
    }))
    .unwrap();
    runner.handle(&c).await.unwrap()
}

async fn set_status(fx: &Fx, status: &str) {
    sqlx::query("UPDATE connector_instance SET status = $2 WHERE tenant_id = $1 AND id = $3")
        .bind(fx.d.f.tenant)
        .bind(status)
        .bind(DEMO_STUB_INSTANCE)
        .execute(&fx.d.e.owner)
        .await
        .unwrap();
}

/// `(state, outcome_code, detail)` of the stored `operation_call` row.
async fn row(fx: &Fx, result: &OpResult) -> (String, Option<String>, Option<String>, String) {
    sqlx::query_as(
        "SELECT state, outcome_code, detail, route FROM operation_call \
          WHERE tenant_id = $1 AND job_id = $2 AND call_id = $3",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .bind(&result.call_id)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap()
}

async fn audit_reason(fx: &Fx, result: &OpResult) -> Option<String> {
    sqlx::query_scalar(
        "SELECT details->>'reason' FROM audit_event \
          WHERE tenant_id = $1 AND action = 'execute' AND resource_type = 'operation_call' \
            AND resource_id = $2",
    )
    .bind(fx.d.f.tenant)
    .bind(&result.call_id)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_healthy_instance_is_called() {
    let fx = crate::b4::flow::setup("route-health-ok", Env::Test).await;
    put_target(&fx.d).await;
    let msg = fixture(&fx, "CS-71", Some("ok")).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let runner = runner(&fx, &calls, None);
    let res = call(&runner, "CS-71", &msg, "report").await;
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(audit_reason(&fx, &res).await, None, "nothing was blocked");
}

#[tokio::test]
async fn a_positively_unhealthy_instance_is_not_called() {
    for status in ["auth_failed", "invalid_target"] {
        let fx = crate::b4::flow::setup("route-health-down", Env::Test).await;
        put_target(&fx.d).await;
        let msg = fixture(&fx, "CS-72", Some(status)).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = runner(&fx, &calls, None);
        let res = call(&runner, "CS-72", &msg, "report").await;
        // the unrouted-operation outcome, never an id ask
        assert_eq!(res.state, ResultState::Error, "{status}: {res:?}");
        assert_eq!(res.outcome_code.as_deref(), Some("not_offered"), "{status}");
        assert!(
            res.output
                .as_ref()
                .is_none_or(|o| o.as_object().is_none_or(|m| m.is_empty())),
            "{status}: no data from a system that was not called: {res:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{status}: zero calls");
        // the reason is recorded on the row and the audit row
        let (state, code, detail, route) = row(&fx, &res).await;
        assert_eq!(state, "error");
        assert_eq!(code.as_deref(), Some("not_offered"));
        assert!(
            detail
                .as_deref()
                .unwrap_or("")
                .starts_with("route_unhealthy: "),
            "{detail:?}"
        );
        assert_eq!(route, "instance", "the route that was blocked");
        assert_eq!(
            audit_reason(&fx, &res).await.as_deref(),
            Some("route_unhealthy")
        );
    }
}

#[tokio::test]
async fn an_untested_instance_still_routes() {
    let fx = crate::b4::flow::setup("route-health-unknown", Env::Test).await;
    put_target(&fx.d).await;
    // the column default: never probed
    let msg = fixture(&fx, "CS-73", None).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let runner = runner(&fx, &calls, None);
    let res = call(&runner, "CS-73", &msg, "report").await;
    assert_eq!(res.state, ResultState::Ok, "fail open: {res:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// The staleness bound: within the TTL a status change is not seen (no read per call); with no TTL the next call
/// reads the row, so the change is seen in both directions.
#[tokio::test]
async fn health_is_read_at_most_once_per_staleness_bound() {
    let fx = crate::b4::flow::setup_latest("route-health-stale", Env::Test).await;
    put_target(&fx.d).await;
    let msg = fixture(&fx, "CS-74", Some("ok")).await;

    let cached_calls = Arc::new(AtomicUsize::new(0));
    let cached = runner(&fx, &cached_calls, Some(Duration::from_secs(3600)));
    assert_eq!(
        call(&cached, "CS-74", &msg, "one").await.state,
        ResultState::Ok
    );
    set_status(&fx, "auth_failed").await;
    let still = call(&cached, "CS-74", &msg, "two").await;
    assert_eq!(
        still.state,
        ResultState::Ok,
        "within the bound the cached read answers"
    );
    assert_eq!(cached_calls.load(Ordering::SeqCst), 2);

    let fresh_calls = Arc::new(AtomicUsize::new(0));
    let fresh = runner(&fx, &fresh_calls, Some(Duration::ZERO));
    let blocked = call(&fresh, "CS-74", &msg, "three").await;
    assert_eq!(
        blocked.outcome_code.as_deref(),
        Some("not_offered"),
        "{blocked:?}"
    );
    assert_eq!(fresh_calls.load(Ordering::SeqCst), 0);
    set_status(&fx, "ok").await;
    let back = call(&fresh, "CS-74", &msg, "four").await;
    assert_eq!(
        back.state,
        ResultState::Ok,
        "recovery is seen on the next read: {back:?}"
    );
    assert_eq!(fresh_calls.load(Ordering::SeqCst), 1);
}
