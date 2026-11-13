//! Audit cards on #60: (1) a duplicate write key that re-arms an `error` row records the RETRY's route (0181 grants
//! `agent_runtime` UPDATE(route)); (2) a duplicate key answered from the stored outcome names the instance that
//! served the original call, not `None`. Runs as `agent_runtime`.
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
use serde_json::json;

use crate::b4::design::put_target;
use crate::b4::flow::{Fx, current_design, insert_case};
use crate::redis_env as r;

const OP: &str = "delivery.report_problem";

/// Answers `ok` naming the routed instance.
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
                json!({"case_id": "SP-3101", "status": "reported"})
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

async fn route_of(fx: &Fx, call_id: &str) -> (String, String) {
    sqlx::query_as(
        "SELECT state, route FROM operation_call WHERE tenant_id = $1 AND job_id = $2 AND call_id = $3",
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

/// Attempt 1 ran on route A (`instance`) and ended `error`; the retry with the same key is served by route B
/// (`builtin`): the re-armed row names B, and stays B once the retry finishes.
#[tokio::test]
async fn a_rearmed_write_records_the_retrys_route() {
    let fx = crate::b4::flow::setup("rearm-route", Env::Test).await;
    put_target(&fx.d).await;
    let msg = case_with_message(&fx, "CS-61").await;
    let job = fx.d.f.job.job_id;
    let call_id = agent_api::runtime::domain::call_id("CS-61", 0, "report", 1);
    // a test-env row carries the draft it ran on (operation_call_env_design_check)
    let draft_rev = match current_design(&fx).await {
        agent_api::design::CaseDesign::Draft { draft_rev } => Some(draft_rev),
        agent_api::design::CaseDesign::Live { .. } => None,
    };
    let row = |route: &'static str| CallRow {
        call_id: call_id.clone(),
        case_id: Some("CS-61".into()),
        operation: OP.into(),
        route,
        env: fx.scope.env.as_str(),
        initiator: "procedure@test".into(),
        attempt: 1,
        idempotency_key: Some("CS-61:1:report".into()),
        confirm_ref: Some(msg.clone()),
        release_id: None,
        draft_rev,
        step: json!({"phase": 0, "step": "report"}),
    };
    let fin = |state: &'static str| Final {
        state,
        code: (state == "error").then_some("upstream_unavailable"),
        latency_ms: 1,
        detail: None,
        object_change: None,
        instance_id: None,
        template: None,
    };

    let mut tx = scoped(&fx).await;
    assert_eq!(
        ops::insert_pending(&mut tx, job, &row("instance"))
            .await
            .unwrap(),
        Pending::Run {
            call_id: call_id.clone()
        }
    );
    assert!(
        ops::finish_pending(&mut tx, job, &call_id, &fin("error"))
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    assert_eq!(
        route_of(&fx, &call_id).await,
        ("error".to_owned(), "instance".to_owned())
    );

    // the retry resolves to route B: the re-arm assigns it
    let mut tx = scoped(&fx).await;
    assert_eq!(
        ops::insert_pending(&mut tx, job, &row("builtin"))
            .await
            .unwrap(),
        Pending::Retry {
            call_id: call_id.clone()
        }
    );
    tx.commit().await.unwrap();
    assert_eq!(
        route_of(&fx, &call_id).await,
        ("pending".to_owned(), "builtin".to_owned()),
        "the re-arm must not keep the failed attempt's route"
    );

    let mut tx = scoped(&fx).await;
    assert!(
        ops::finish_pending(&mut tx, job, &call_id, &fin("ok"))
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    assert_eq!(
        route_of(&fx, &call_id).await,
        ("ok".to_owned(), "builtin".to_owned())
    );
}

/// A duplicate key of a finished write answers the stored outcome AND names the instance that served the original
/// call (it used to answer `instance_id: None`).
#[tokio::test]
async fn a_stored_replay_names_the_original_instance() {
    let fx = crate::b4::flow::setup("stored-instance", Env::Test).await;
    put_target(&fx.d).await;
    let msg = case_with_message(&fx, "CS-62").await;
    routed_instance(&fx).await;
    let c: OpCall = serde_json::from_value(json!({
        "call_id": agent_api::runtime::domain::call_id("CS-62", 0, "report", 1),
        "case_id": "CS-62", "conversation_id": "conv-CS-62", "phase": 0, "step": "report",
        "attempt": 1, "agent_role": "procedure", "operation": OP, "variant": null,
        "input": {"delivery_id": "SHP-LATE-01", "problem": "Parcel is two days late"},
        "idempotency_key": "CS-62:1:report", "env": "test", "release_id": null, "draft_rev": null,
        "confirm_ref": msg, "initiator": "procedure@test", "identity_level": 3, "deadline_ms": 0
    }))
    .unwrap();
    let mut st = fx.d.e.state.clone();
    st.db = fx.rt.clone();
    let runner = Runner::new(fx.bus.clone(), st, Arc::new(DesignCache::default()))
        .unwrap()
        .with_consumer("rearm-route-test")
        .with_consume(r::fast())
        .with_executors(Executors::default().register(OP, Arc::new(OnInstance)));

    let first = runner.handle(&c).await.unwrap();
    assert_eq!(first.state, ResultState::Ok, "{first:?}");
    assert_eq!(first.instance_id.as_deref(), Some(DEMO_STUB_INSTANCE));

    let again = runner.handle(&c).await.unwrap();
    assert_eq!(again.state, ResultState::Ok, "{again:?}");
    assert_eq!(again.latency_ms, Some(0), "answered from the stored row");
    assert_eq!(again.route_kind.as_deref(), Some("instance"));
    assert_eq!(
        again.instance_id.as_deref(),
        Some(DEMO_STUB_INSTANCE),
        "the stored replay keeps the original call's instance"
    );
}
