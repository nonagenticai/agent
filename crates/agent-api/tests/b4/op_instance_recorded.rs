//! A finished WRITE records the instance and template it ran on (0154). Live 2026-10-22 (Demo Retail, CS-46):
//! `delivery.report_problem` routed to `demo-retail-backends-test` was listed `instance=null` while the reads of
//! the same case named the instance: a write's row is inserted `pending` and `finish_pending` never set
//! `instance_id` / `template` (0065 granted UPDATE on the outcome columns only). Runs as `agent_runtime`.
use crate::common::platform;
use std::sync::Arc;

use agent_api::runtime::domain::{OpCall, OpState as ResultState};
use agent_api::runtime::routes::{DEMO_STUB_INSTANCE, ResolvedTarget};
use agent_api::runtime::runner::Runner;
use agent_api::runtime::runner::ops::{
    self, CallRow, Executors, Final, OpExecutor, OpOutcome, OpRequest, OpState, Pending, outcome,
};
use agent_api::runtime::{Env, design::DesignCache};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use crate::b4::design::put_target;
use crate::b4::flow::{Fx, current_design, insert_case};
use crate::redis_env as r;

const OP: &str = "delivery.report_problem";
const TEMPLATE: &str = "test_shop@1";

/// B7's instance executor as the runner sees it: answers `ok` naming the routed instance and its pinned template.
struct OnInstance;

impl OpExecutor for OnInstance {
    fn execute<'a>(
        &'a self,
        _req: &'a OpRequest,
        target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move {
            let mut out = outcome(
                OpState::Ok,
                None,
                None,
                json!({"case_id": "SP-3001", "status": "reported"})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            );
            if let ResolvedTarget::Instance { instance_id } = target {
                out.instance_id = Some(instance_id.clone());
                out.template = Some(TEMPLATE.to_owned());
            }
            out
        })
    }
}

/// A failed attempt on the routed instance: answers `error` naming the instance and its template.
struct FailsOnInstance;

impl OpExecutor for FailsOnInstance {
    fn execute<'a>(
        &'a self,
        _req: &'a OpRequest,
        target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move {
            let mut out = outcome(
                OpState::Error,
                Some("upstream_unavailable"),
                Some("attempt 1 failed".into()),
                serde_json::Map::new(),
            );
            if let ResolvedTarget::Instance { instance_id } = target {
                out.instance_id = Some(instance_id.clone());
                out.template = Some(TEMPLATE.to_owned());
            }
            out
        })
    }
}

/// A registered executor that runs the call itself: `ok`, naming no instance or template.
struct NoInstance;

impl OpExecutor for NoInstance {
    fn execute<'a>(
        &'a self,
        _req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move {
            outcome(
                OpState::Ok,
                None,
                None,
                json!({"case_id": "SP-3002", "status": "reported"})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            )
        })
    }
}

fn runner(fx: &Fx) -> Runner {
    runner_with(fx, Arc::new(OnInstance))
}

fn runner_with(fx: &Fx, x: Arc<dyn OpExecutor>) -> Runner {
    let mut st = fx.d.e.state.clone();
    st.db = fx.rt.clone();
    Runner::new(fx.bus.clone(), st, Arc::new(DesignCache::default()))
        .unwrap()
        .with_consumer("op-instance-test")
        .with_consume(r::fast())
        .with_executors(Executors::default().register(OP, x))
}

/// A runtime transaction scoped to the fixture's tenant and job (as `Runner::begin` opens it).
async fn scoped(fx: &Fx) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut tx = agent_api::tenancy::begin_scoped(&fx.rt, fx.d.f.tenant)
        .await
        .unwrap();
    sqlx::query("SELECT set_config('app.current_job', $1, true)")
        .bind(fx.d.f.job.job_id.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    tx
}

/// `(state, instance_id, template)` of row `call_id`.
async fn row_of(fx: &Fx, call_id: &str) -> (String, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT state, instance_id, template FROM operation_call \
          WHERE tenant_id = $1 AND job_id = $2 AND call_id = $3",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .bind(call_id)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap()
}

/// The case row, its staff conversation and one inbound customer message (the confirmation); the message id.
async fn case_with_message(fx: &Fx, case: &str) -> String {
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
    sqlx::query_scalar(
        "INSERT INTO conversation_message (tenant_id, job_id, conversation_id, role, text, case_id, at_ms) \
         VALUES ($1, $2, $3, 'customer', 'my parcel is late', $4, 1) RETURNING message_id::text",
    )
    .bind(t)
    .bind(j)
    .bind(format!("conv-{case}"))
    .bind(case)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap()
}

/// The tenant's stub instance (test env) and a working route of `delivery.report_problem` to it.
async fn routed_instance(fx: &Fx) {
    platform::stub_shop(&fx.d.e, fx.d.f.tenant, DEMO_STUB_INSTANCE).await;
    sqlx::query(
        "INSERT INTO operation_route (tenant_id, job_id, object, verb, variant, env, target_kind, instance_id, \
                                      updated_by) \
         VALUES ($1, $2, 'delivery', 'report_problem', '*', $3, 'instance', $4, 'u-test')",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .bind(fx.scope.env.as_str())
    .bind(DEMO_STUB_INSTANCE)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
}

fn report_call(case: &str, key: &str, msg: &str) -> OpCall {
    serde_json::from_value(json!({
        "call_id": agent_api::runtime::domain::call_id(case, 0, "report", 1),
        "case_id": case, "conversation_id": format!("conv-{case}"), "phase": 0, "step": "report",
        "attempt": 1, "agent_role": "procedure", "operation": "delivery.report_problem", "variant": null,
        "input": {"delivery_id": "SHP-LATE-01", "problem": "Parcel is two days late"},
        "idempotency_key": key, "env": "test", "release_id": null, "draft_rev": null,
        "confirm_ref": msg, "initiator": "procedure@test", "identity_level": 3, "deadline_ms": 0
    }))
    .unwrap()
}

#[tokio::test]
async fn a_finished_write_records_its_instance_and_template() {
    let fx = crate::b4::flow::setup("op-instance-write", Env::Test).await;
    put_target(&fx.d).await;
    let msg = case_with_message(&fx, "CS-46").await;
    routed_instance(&fx).await;
    let c = report_call("CS-46", "CS-46:1:report", &msg);
    let res = runner(&fx).handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(res.route_kind.as_deref(), Some("instance"));
    assert_eq!(res.instance_id.as_deref(), Some(DEMO_STUB_INSTANCE));

    // the row went pending → ok (a write, keyed) and names where it ran
    let (state, key, instance, template, change): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<Value>,
    ) = sqlx::query_as(
        "SELECT state, idempotency_key, instance_id, template, object_change FROM operation_call \
          WHERE tenant_id = $1 AND job_id = $2 AND call_id = $3",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .bind(&c.call_id)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(state, "ok");
    assert_eq!(
        key.as_deref(),
        Some("CS-46:1:report"),
        "a write row (finish_pending)"
    );
    assert_eq!(instance.as_deref(), Some(DEMO_STUB_INSTANCE));
    assert_eq!(template.as_deref(), Some(TEMPLATE));
    assert_eq!(change.unwrap()["case_id"], "SP-3001");
}

/// The audit blocker on #60: attempt 1 ran on instance A and ended `error` (recording A + template); the retry with
/// the same idempotency key re-arms the row and is answered by an executor naming no instance. The row describes
/// the attempt that finished it: `ok` with NULL instance/template, never attempt 1's.
#[tokio::test]
async fn a_retried_write_does_not_keep_the_failed_attempts_instance() {
    let fx = crate::b4::flow::setup_latest("op-instance-retry", Env::Test).await;
    put_target(&fx.d).await;
    let msg = case_with_message(&fx, "CS-47").await;
    routed_instance(&fx).await;
    let c = report_call("CS-47", "CS-47:1:report", &msg);

    let first = runner_with(&fx, Arc::new(FailsOnInstance))
        .handle(&c)
        .await
        .unwrap();
    assert_eq!(first.state, ResultState::Error, "{first:?}");
    let (state, instance, template) = row_of(&fx, &c.call_id).await;
    assert_eq!(state, "error");
    assert_eq!(
        instance.as_deref(),
        Some(DEMO_STUB_INSTANCE),
        "attempt 1 recorded A"
    );
    assert_eq!(template.as_deref(), Some(TEMPLATE));

    // the same OpCall (same key) again: the error row is re-armed and the retry runs
    let second = runner_with(&fx, Arc::new(NoInstance))
        .handle(&c)
        .await
        .unwrap();
    assert_eq!(second.state, ResultState::Ok, "{second:?}");
    assert_eq!(
        second.output.as_ref().map(|o| o["case_id"].clone()),
        Some(json!("SP-3002")),
        "the retry ran (not a stored answer)"
    );
    let (state, instance, template) = row_of(&fx, &c.call_id).await;
    assert_eq!(state, "ok");
    assert_eq!(instance, None, "the retry named no instance");
    assert_eq!(template, None, "the retry named no template");
}

/// Each half of the fix on its own (the end-to-end test above passes if either holds): the re-arm clears the failed
/// attempt's instance/template, and `finish_pending` assigns (never COALESCEs) them onto a pending row that still
/// carries one.
#[tokio::test]
async fn rearm_clears_and_finish_pending_assigns_the_instance() {
    let fx = crate::b4::flow::setup("op-instance-rearm", Env::Test).await;
    put_target(&fx.d).await;
    let msg = case_with_message(&fx, "CS-48").await;
    let job = fx.d.f.job.job_id;
    let call_id = agent_api::runtime::domain::call_id("CS-48", 0, "report", 1);
    // a test-env row carries the draft it ran on (operation_call_env_design_check)
    let draft_rev = match current_design(&fx).await {
        agent_api::design::CaseDesign::Draft { draft_rev } => Some(draft_rev),
        agent_api::design::CaseDesign::Live { .. } => None,
    };
    let row = CallRow {
        call_id: call_id.clone(),
        case_id: Some("CS-48".into()),
        operation: OP.into(),
        route: "instance",
        env: fx.scope.env.as_str(),
        initiator: "procedure@test".into(),
        attempt: 1,
        idempotency_key: Some("CS-48:1:report".into()),
        confirm_ref: Some(msg),
        release_id: None,
        draft_rev,
        step: json!({"phase": 0, "step": "report"}),
    };
    let fin = |state: &'static str, instance: Option<&'static str>| Final {
        state,
        code: (state == "error").then_some("upstream_unavailable"),
        latency_ms: 1,
        detail: None,
        object_change: None,
        instance_id: instance,
        template: instance.map(|_| TEMPLATE),
    };
    // attempt 1: pending → error on A
    let mut tx = scoped(&fx).await;
    assert_eq!(
        ops::insert_pending(&mut tx, job, &row).await.unwrap(),
        Pending::Run {
            call_id: call_id.clone()
        }
    );
    assert!(
        ops::finish_pending(
            &mut tx,
            job,
            &call_id,
            &fin("error", Some(DEMO_STUB_INSTANCE))
        )
        .await
        .unwrap()
    );
    tx.commit().await.unwrap();

    // the retry re-arms the row: pending, instance/template cleared
    let mut tx = scoped(&fx).await;
    assert_eq!(
        ops::insert_pending(&mut tx, job, &row).await.unwrap(),
        Pending::Retry {
            call_id: call_id.clone()
        }
    );
    tx.commit().await.unwrap();
    assert_eq!(
        row_of(&fx, &call_id).await,
        ("pending".to_owned(), None, None),
        "the re-arm clears attempt 1's instance/template"
    );

    // a pending row still naming A (as before the re-arm cleared it): finish_pending assigns, so None wins
    sqlx::query(
        "UPDATE operation_call SET instance_id = $3, template = $4 WHERE job_id = $1 AND call_id = $2",
    )
    .bind(job)
    .bind(&call_id)
    .bind(DEMO_STUB_INSTANCE)
    .bind(TEMPLATE)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    let mut tx = scoped(&fx).await;
    assert!(
        ops::finish_pending(&mut tx, job, &call_id, &fin("ok", None))
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    assert_eq!(
        row_of(&fx, &call_id).await,
        ("ok".to_owned(), None, None),
        "finish_pending must not COALESCE the earlier instance/template"
    );
}
