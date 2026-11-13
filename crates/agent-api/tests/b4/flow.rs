//! B4 T7 — the orchestrator (crate::runtime::orchestrator) over real Postgres 18 + Redis, connected to Postgres as
//! `agent_runtime` (the role the binary runs as). A job created from `job/customer_care_agent`, whose template
//! declares the `returns` context space (area `order-support`) and the `return_request` procedure. The drain (T9)
//! is stood in for by an owner-side `agent_case` insert plus a `request.qualified` envelope; the runner (T6) by a
//! fake that answers each `op.call`.
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_api::design::CaseDesign;
use agent_api::runtime::bus::groups;
use agent_api::runtime::design::DesignCache;
use agent_api::runtime::domain::{
    CaseOutcome, OP_CALL, OpCall, OpResult, OpState, Phase, ProcedureRef, QualifiedRequest,
    REQUEST_QUALIFIED, REQUEST_RESOLVED, Resolution, Step,
};
use agent_api::runtime::orchestrator::{
    Orchestrator, ProcedureSource, SUCCESS_CRITERIA_UNMET, StaticProcedures, cancel_offboarded,
};
use agent_api::runtime::reply;
use agent_api::runtime::routes::{ResolvedTarget, RouteQuery, RouteResolver};
use agent_api::runtime::{Bus, Env, Envelope, Scope, scopes};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::b4::design::{self as d, approve, put_languages, put_target, put_target_seen};
use crate::common;
use crate::redis_env as r;

pub const AREA: &str = "order-support";
pub const SPACE: &str = "retail:order-support:returns";

/// A job of its own tenant (the design fixture), a scope of it in `env` on a fresh Redis prefix, and the
/// `agent_runtime` pool.
pub struct Fx {
    pub d: d::Fx,
    pub root: Bus,
    pub scope: Scope,
    pub bus: Bus,
    pub rt: PgPool,
}

pub async fn setup(test: &str, env: Env) -> Fx {
    setup_on(test, env, false).await
}

/// [`setup`] with the job on the B4 test template (`support::template`: activities, spaces, procedures) and the
/// test environment's routes for `order.read` / `case.create` (`support::route_test`).
pub async fn setup_latest(test: &str, env: Env) -> Fx {
    setup_on(test, env, true).await
}

async fn setup_on(test: &str, env: Env, latest: bool) -> Fx {
    let d = d::setup_on(None, latest).await;
    if latest {
        crate::b4::support::route_test(&d.e, &d.f).await;
    }
    let root = r::root(test).await;
    let scope = Scope::new(d.f.tenant, d.f.job.job_key.clone(), env).unwrap();
    let bus = root.scoped(&scope);
    let rt = common::runtime_pool(&d.e.owner).await;
    Fx {
        d,
        root,
        scope,
        bus,
        rt,
    }
}

/// An orchestrator for the fixture's scope with a fresh design cache (so a plan reads the design as it is now).
pub fn orch<P: ProcedureSource>(fx: &Fx, source: P) -> Orchestrator<P> {
    Orchestrator::new(
        fx.bus.clone(),
        fx.rt.clone(),
        Arc::clone(&fx.d.e.state.design),
        Arc::new(DesignCache::default()),
        source,
    )
    .unwrap()
    .with_consumer("orchestrator-test")
    .with_consume(r::fast())
}

/// What the drain writes before it publishes (R6): the case row, stamped with the qualify-time design.
pub async fn insert_case(fx: &Fx, case_id: &str, design: &CaseDesign) {
    insert_case_in(fx, case_id, &format!("conv-{case_id}"), design).await;
}

/// [`insert_case`] in conversation `conv`.
pub async fn insert_case_in(fx: &Fx, case_id: &str, conv: &str, design: &CaseDesign) {
    let (release_id, draft_rev) = match design {
        CaseDesign::Live { release_id } => (release_id.clone(), None),
        CaseDesign::Draft { draft_rev } => (None, Some(*draft_rev)),
    };
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, release_id, draft_rev, \
                                 qualified_release_id, qualified_draft_rev, status) \
         VALUES ($1, $2, $3, $4, 'staff', $5, $6, $7, $6, $7, 'open')",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .bind(case_id)
    .bind(conv)
    .bind(fx.scope.env.as_str())
    .bind(release_id)
    .bind(draft_rev)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
}

/// The current design of the scope (what the drain stamps at qualification).
pub async fn current_design(fx: &Fx) -> CaseDesign {
    let (rel, rev): (Option<String>, i64) =
        sqlx::query_as("SELECT current_release_id, draft_rev FROM jobs WHERE id = $1")
            .bind(fx.d.f.job.job_id)
            .fetch_one(&fx.d.e.owner)
            .await
            .unwrap();
    match fx.scope.env {
        Env::Prod => CaseDesign::Live { release_id: rel },
        Env::Test => CaseDesign::Draft { draft_rev: rev },
    }
}

/// A returns request for `case` (slots `order_id`), qualified from customer message `msg-<case>`.
pub fn request(case: &str) -> QualifiedRequest {
    serde_json::from_value(json!({
        "case_id": case, "conversation_id": format!("conv-{case}"), "area": AREA, "context_space": SPACE,
        "request_type": "return", "slots": {"order_id": "ORD-58790"}, "customer_id": "C-10482",
        "identity_level": 2, "message_id": format!("msg-{case}")
    }))
    .unwrap()
}

pub fn qualified(req: &QualifiedRequest) -> Envelope {
    Envelope::new(&req.topic(REQUEST_QUALIFIED), "drain", &req.case_id, req).unwrap()
}

/// Insert the case row (stamped now) and deliver its `request.qualified` to `o`.
pub async fn qualify<P: ProcedureSource>(
    fx: &Fx,
    o: &Orchestrator<P>,
    case: &str,
) -> QualifiedRequest {
    insert_case(fx, case, &current_design(fx).await).await;
    let req = request(case);
    o.on_qualified(qualified(&req)).await.unwrap();
    req
}

/// The `op.call`s of `case` on `bus:op`, oldest first.
pub async fn calls(fx: &Fx, case: &str) -> Vec<OpCall> {
    r::entries(&fx.bus, &fx.bus.streams.op())
        .await
        .into_iter()
        .filter(|e| e.correlation_id == case && e.event() == Some(OP_CALL))
        .map(|e| e.payload_as().unwrap())
        .collect()
}

/// The `request.resolved` of `case` on the area's customer stream.
pub async fn resolutions(fx: &Fx, case: &str) -> Vec<Resolution> {
    resolutions_in(fx, AREA, case).await
}

/// The `request.resolved` of `case` on `area`'s customer stream.
pub async fn resolutions_in(fx: &Fx, area: &str, case: &str) -> Vec<Resolution> {
    r::entries(&fx.bus, &fx.bus.streams.customer(area))
        .await
        .into_iter()
        .filter(|e| e.correlation_id == case && e.event() == Some(REQUEST_RESOLVED))
        .map(|e| e.payload_as().unwrap())
        .collect()
}

/// The runner's answer to `call`.
pub fn answer(call: &OpCall, state: OpState, code: &str, output: Value) -> Envelope {
    let res = OpResult {
        call_id: call.call_id.clone(),
        case_id: call.case_id.clone(),
        phase: call.phase,
        step: call.step.clone(),
        attempt: call.attempt,
        operation: call.operation.clone(),
        state,
        outcome_code: Some(code.to_owned()),
        output: Some(output),
        retryable: false,
        route_kind: Some("builtin".into()),
        latency_ms: Some(3),
        instance_id: None,
    };
    Envelope::new("op.result", "runner", &call.case_id, &res).unwrap()
}

/// `(status, release_id, draft_rev, qualified_release_id, procedure_name, procedure_version)` of the case.
pub async fn case_row(
    fx: &Fx,
    case: &str,
) -> (
    String,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<i32>,
) {
    sqlx::query_as(
        "SELECT status, release_id, draft_rev, qualified_release_id, procedure_name, procedure_version \
           FROM agent_case WHERE tenant_id = $1 AND job_id = $2 AND case_id = $3",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .bind(case)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap()
}

/// The `state` event bodies of the case, oldest first.
pub async fn state_events(fx: &Fx, case: &str) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT body FROM case_event WHERE tenant_id = $1 AND job_id = $2 AND case_id = $3 AND kind = 'state' \
          ORDER BY seq",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .bind(case)
    .fetch_all(&fx.d.e.owner)
    .await
    .unwrap()
}

/// Poll until `f` gives `Some` (10 s).
async fn until<T, F, Fut>(f: F) -> T
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(Instant::now() < deadline, "condition not met within 10 s");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The fake runner's output per operation of the return flow; `eligible` decides the rule.
fn return_flow_output(call: &OpCall, eligible: bool) -> Value {
    match call.operation.as_str() {
        "order.read" => json!({"status": "delivered", "lines": [{"sku": "SKU-1", "qty": 1}],
                               "delivered_at": "2026-10-10", "total": 59.0}),
        "rule.evaluate" if eligible => {
            json!({"outcome": "eligible", "reason": "inside the window"})
        }
        "rule.evaluate" => {
            json!({"outcome": "ineligible", "code": "ineligible:RET-01", "reason": "the return window has closed"})
        }
        "case.create" => json!({"case_id": "RMA-1", "status": "open", "label": "label-1"}),
        _ => json!({}),
    }
}

#[tokio::test]
async fn return_flow_runs_order_read_then_rule_then_rma() {
    let fx = setup_latest("orch-flow", Env::Test).await;
    let o = Arc::new(orch(&fx, StaticProcedures));
    let task = tokio::spawn(o.clone().run());
    // the fake runner: answers every op.call on bus:op as the `runner` group
    let runner_bus = fx.bus.clone();
    let runner = tokio::spawn(async move {
        let b = runner_bus.clone();
        runner_bus
            .consume(
                &runner_bus.streams.op(),
                groups::RUNNER,
                "fake-runner",
                r::fast(),
                move |env| {
                    let b = b.clone();
                    async move {
                        let call: OpCall = env.payload_as()?;
                        let out = return_flow_output(&call, true);
                        b.publish(
                            &b.streams.op_results(),
                            &answer(&call, OpState::Ok, "ok", out),
                        )
                        .await?;
                        Ok(())
                    }
                },
            )
            .await
    });
    insert_case(&fx, "CS-1", &current_design(&fx).await).await;
    let req = request("CS-1");
    fx.bus
        .publish(&fx.bus.streams.customer(AREA), &qualified(&req))
        .await
        .unwrap();
    let res = until(|| async { resolutions(&fx, "CS-1").await.into_iter().next() }).await;
    task.abort();
    runner.abort();

    assert_eq!(res.outcome, CaseOutcome::Resolved, "{res:?}");
    assert_eq!(res.procedure.as_deref(), Some("return_request@3"));
    assert_eq!(res.facts["rma"]["case_id"], "RMA-1");
    let cs = calls(&fx, "CS-1").await;
    let order: Vec<String> = cs
        .iter()
        .map(|c| match &c.variant {
            Some(v) => format!("{}{{{v}}}", c.operation),
            None => c.operation.clone(),
        })
        .collect();
    assert_eq!(
        order,
        ["order.read", "rule.evaluate{RET-01}", "case.create{return}"],
        "phases in order, one call each"
    );
    // args resolve from slots and earlier outputs
    assert_eq!(cs[0].input["order_id"], "ORD-58790");
    assert_eq!(cs[1].input["delivered_at"], "2026-10-10");
    assert_eq!(
        cs[2].input["return_lines"],
        json!([{"sku": "SKU-1", "qty": 1}])
    );
    // reads carry no idempotency key or confirm_ref; the write carries both
    assert!(cs[0].idempotency_key.is_none() && cs[0].confirm_ref.is_none());
    assert_eq!(cs[2].idempotency_key.as_deref(), Some("CS-1:2:rma"));
    assert_eq!(cs[2].confirm_ref.as_deref(), Some("msg-CS-1"));
    assert!(
        cs.iter()
            .all(|c| c.env == Env::Test && c.agent_role == "procedure")
    );
    // the test-env trace input lists the steps and the design places read
    let trace = res.trace.as_ref().expect("a test-env trace input");
    assert_eq!(trace.steps.len(), 3);
    assert!(
        trace.read.contains(&"rule_values:RET-01".to_owned()),
        "{:?}",
        trace.read
    );
    // the test-env turn read its TEST route, keyed like the draft ledger (route:<op>:<env>)
    assert!(
        trace.read.contains(&"route:order.read:test".to_owned()),
        "{:?}",
        trace.read
    );

    let (status, _, _, _, name, version) = case_row(&fx, "CS-1").await;
    assert_eq!(status, "resolved");
    assert_eq!(name.as_deref(), Some("return_request"));
    assert_eq!(version, Some(3));
    let events = state_events(&fx, "CS-1").await;
    assert_eq!(events.first().unwrap()["event"], "planned");
    assert_eq!(events.last().unwrap()["status"], "resolved");
    // K50: no customer data in the state events
    assert!(!json!(events).to_string().contains("ORD-58790"));
}

#[tokio::test]
async fn a_gate_that_answers_false_ends_the_case_not_eligible() {
    let fx = setup_latest("orch-gate", Env::Test).await;
    let o = orch(&fx, StaticProcedures);
    qualify(&fx, &o, "CS-1").await;
    for _ in 0..2 {
        let call = calls(&fx, "CS-1").await.pop().unwrap();
        let out = return_flow_output(&call, false);
        o.on_result(answer(&call, OpState::Ok, "ok", out))
            .await
            .unwrap();
    }
    let rs = resolutions(&fx, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::NotEligible);
    assert_eq!(rs[0].code.as_deref(), Some("ineligible:RET-01"));
    assert_eq!(
        rs[0].reason.as_deref(),
        Some("the return window has closed")
    );
    assert_eq!(calls(&fx, "CS-1").await.len(), 2, "no RMA after the gate");
    assert_eq!(case_row(&fx, "CS-1").await.0, "not_eligible");
}

/// Acceptance defect 2026-10-21: a gate rule that cannot decide (`placeholder`: its values are unset) hands the
/// case over; its internal reason ("RET-01 has no value for …") never becomes the customer's reply.
#[tokio::test]
async fn an_undecided_gate_rule_hands_the_case_over() {
    for outcome in ["placeholder", "escalate"] {
        let fx = setup_latest(&format!("orch-gate-{outcome}"), Env::Test).await;
        let o = orch(&fx, StaticProcedures);
        qualify(&fx, &o, "CS-1").await;
        for _ in 0..2 {
            let call = calls(&fx, "CS-1").await.pop().unwrap();
            let out = if call.operation == "rule.evaluate" {
                json!({"eligible": false, "outcome": outcome, "code": outcome,
                       "reason": "RET-01 has no value for values: the agent cannot decide and hands the case to a person."})
            } else {
                return_flow_output(&call, true)
            };
            o.on_result(answer(&call, OpState::Ok, "ok", out))
                .await
                .unwrap();
        }
        let rs = resolutions(&fx, "CS-1").await;
        assert_eq!(rs.len(), 1, "{outcome}: {rs:?}");
        assert_eq!(rs[0].outcome, CaseOutcome::Handover, "{outcome}");
        assert_eq!(rs[0].code.as_deref(), Some(outcome));
        assert!(
            rs[0].reason.is_none(),
            "{outcome}: the rule's reason stays internal"
        );
        assert_eq!(calls(&fx, "CS-1").await.len(), 2, "no RMA after the gate");
        assert_ne!(case_row(&fx, "CS-1").await.0, "not_eligible", "{outcome}");
    }
}

#[tokio::test]
async fn case_records_plan_time_release_not_qualify_time() {
    let fx = setup_latest("orch-plan-release", Env::Prod).await;
    let at = put_target(&fx.d).await;
    put_languages(&fx.d, json!(["en", "de"])).await;
    let r1 = approve(&fx.d, &at, true).await;
    // qualified on R1 ...
    insert_case(
        &fx,
        "CS-1",
        &CaseDesign::Live {
            release_id: Some(r1.clone()),
        },
    )
    .await;
    // ... and R2 is published before the orchestrator plans
    put_languages(&fx.d, json!(["en", "de", "fr"])).await;
    let r2 = approve(&fx.d, &put_target_seen(&fx.d).await, true).await;
    assert_ne!(r1, r2);
    let o = orch(&fx, StaticProcedures);
    o.on_qualified(qualified(&request("CS-1"))).await.unwrap();

    let plan = o.cases().plan("CS-1").await.unwrap().unwrap();
    assert_eq!(
        plan.design,
        CaseDesign::Live {
            release_id: Some(r2.clone())
        }
    );
    let call = &calls(&fx, "CS-1").await[0];
    assert_eq!(call.release_id.as_deref(), Some(r2.as_str()));
    assert_eq!(call.draft_rev, None);
    let (_, release, rev, qualified_release, _, _) = case_row(&fx, "CS-1").await;
    assert_eq!(release.as_deref(), Some(r2.as_str()), "the plan's release");
    assert_eq!(rev, None);
    assert_eq!(
        qualified_release.as_deref(),
        Some(r1.as_str()),
        "qualify time kept"
    );
    // first plan wins: a redelivery after another publish keeps R2
    put_languages(&fx.d, json!(["en"])).await;
    let r3 = approve(&fx.d, &put_target_seen(&fx.d).await, true).await;
    let o2 = orch(&fx, StaticProcedures);
    o2.on_qualified(qualified(&request("CS-1"))).await.unwrap();
    let again = o2.cases().plan("CS-1").await.unwrap().unwrap();
    assert_eq!(
        again.design,
        CaseDesign::Live {
            release_id: Some(r2)
        }
    );
    assert_ne!(Some(r3), case_row(&fx, "CS-1").await.1);
}

#[tokio::test]
async fn test_env_case_records_draft_rev_and_null_release() {
    let fx = setup_latest("orch-plan-draft", Env::Test).await;
    let rev = crate::b4::design::draft_rev(&fx.d).await;
    let o = orch(&fx, StaticProcedures);
    qualify(&fx, &o, "CS-1").await;
    let plan = o.cases().plan("CS-1").await.unwrap().unwrap();
    assert_eq!(plan.design, CaseDesign::Draft { draft_rev: rev });
    let call = &calls(&fx, "CS-1").await[0];
    assert_eq!((call.release_id.clone(), call.draft_rev), (None, Some(rev)));
    let (_, release, draft, _, _, _) = case_row(&fx, "CS-1").await;
    assert_eq!((release, draft), (None, Some(rev)));
    let planned = &state_events(&fx, "CS-1").await[0];
    assert_eq!(planned["draft_rev"], rev);
    assert_eq!(planned["release_id"], Value::Null);
}

#[tokio::test]
async fn scope_removed_cancels_open_case() {
    let fx = setup_latest("orch-offboard", Env::Test).await;
    scopes::add(&fx.root, &fx.scope).await.unwrap();
    // another registered scope: an empty registry is never read as an offboarding
    scopes::add(&fx.root, &r::scope_b()).await.unwrap();
    let o = orch(&fx, StaticProcedures);
    qualify(&fx, &o, "CS-1").await;
    assert_eq!(
        cancel_offboarded(&fx.root, &fx.rt).await.unwrap(),
        0,
        "registered: nothing"
    );
    scopes::remove(&fx.root, &fx.scope).await.unwrap();
    assert_eq!(cancel_offboarded(&fx.root, &fx.rt).await.unwrap(), 1);
    assert_eq!(case_row(&fx, "CS-1").await.0, "cancelled_offboard");
    assert!(o.cases().tracked().await.unwrap().is_empty());
    assert!(o.cases().is_resolved("CS-1").await.unwrap());
    assert_eq!(
        state_events(&fx, "CS-1").await.last().unwrap()["status"],
        "cancelled_offboard"
    );
    assert_eq!(
        cancel_offboarded(&fx.root, &fx.rt).await.unwrap(),
        0,
        "once"
    );
    // a late result for the cancelled case changes nothing
    let call = calls(&fx, "CS-1").await[0].clone();
    o.on_result(answer(&call, OpState::Ok, "ok", json!({})))
        .await
        .unwrap();
    assert!(resolutions(&fx, "CS-1").await.is_empty());
    assert_eq!(case_row(&fx, "CS-1").await.0, "cancelled_offboard");
}

#[tokio::test]
async fn staff_handover_stops_the_case_at_the_next_boundary() {
    let fx = setup_latest("orch-handover", Env::Test).await;
    let o = orch(&fx, StaticProcedures);
    qualify(&fx, &o, "CS-1").await;
    // staff take the case (B12 hand_over_case writes only the row)
    sqlx::query(
        "UPDATE agent_case SET status = 'handover' WHERE tenant_id = $1 AND case_id = 'CS-1'",
    )
    .bind(fx.d.f.tenant)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    let call = calls(&fx, "CS-1").await[0].clone();
    let out = return_flow_output(&call, true);
    o.on_result(answer(&call, OpState::Ok, "ok", out))
        .await
        .unwrap();
    assert_eq!(calls(&fx, "CS-1").await.len(), 1, "no next phase");
    let rs = resolutions(&fx, "CS-1").await;
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0].outcome, CaseOutcome::Handover);
    assert_eq!(case_row(&fx, "CS-1").await.0, "handover");
}

#[tokio::test]
async fn retryable_error_retries_once_then_fails_with_a_safe_reason() {
    let fx = setup_latest("orch-retry", Env::Test).await;
    let o = orch(&fx, StaticProcedures);
    qualify(&fx, &o, "CS-1").await;
    for attempt in 1..=2u32 {
        let call = calls(&fx, "CS-1").await.pop().unwrap();
        assert_eq!(call.attempt, attempt);
        let mut env = answer(&call, OpState::Error, "timeout", Value::Null);
        env.payload["retryable"] = json!(true);
        o.on_result(env).await.unwrap();
    }
    let cs = calls(&fx, "CS-1").await;
    assert_eq!(cs.len(), 2);
    assert_ne!(cs[0].call_id, cs[1].call_id, "a retry is a new call");
    let rs = resolutions(&fx, "CS-1").await;
    assert_eq!(rs[0].outcome, CaseOutcome::Failed);
    assert_eq!(
        rs[0].reason.as_deref(),
        Some("order.read did not answer in time")
    );
    assert_eq!(case_row(&fx, "CS-1").await.0, "failed");
}

/// The template's delivery-problem space (area `logistics`, procedure `delivery_problem@1`).
pub const DELIVERY_AREA: &str = "logistics";

/// A late-delivery complaint for `case` (slots `order_id`, `problem`), qualified from customer message `msg-<case>`.
pub fn delivery_request(case: &str) -> QualifiedRequest {
    serde_json::from_value(json!({
        "case_id": case, "conversation_id": format!("conv-{case}"), "area": DELIVERY_AREA,
        "context_space": "retail:logistics:delivery_problem", "request_type": "delivery_problem",
        "slots": {"order_id": "ORD-58790", "problem": "late"}, "customer_id": "C-10482",
        "identity_level": 2, "message_id": format!("msg-{case}")
    }))
    .unwrap()
}

/// The fake runner's output per operation of the delivery flow; `status` is the canonical `delivery.status`.
pub fn delivery_flow_output(call: &OpCall, status: &str) -> Value {
    match call.operation.as_str() {
        "order.read" => json!({"status": "shipped", "lines": [{"sku": "SKU-1", "qty": 1}]}),
        "delivery.read" => json!({"status": status, "carrier": "PostNL", "tracking_ref": "3S-1",
                                  "expected_window": {"from": "2026-10-21", "until": "2026-10-23"}}),
        "delivery.report_problem" => json!({"case_id": "DP-1", "status": "open"}),
        _ => json!({}),
    }
}

/// Run the delivery flow of `case` to its end with `delivery.read` answering `status`: the case row, its staff
/// conversation (the J21 item's), the qualified request, then each call answered as it is sent.
async fn run_delivery(fx: &Fx, case: &str, status: &str) -> Vec<Resolution> {
    insert_case(fx, case, &current_design(fx).await).await;
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, $3, $4, 'staff', 'C-10482')",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .bind(format!("conv-{case}"))
    .bind(fx.scope.env.as_str())
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    let o = orch(fx, StaticProcedures).with_routes(Arc::new(EveryOpOffered));
    o.on_qualified(qualified(&delivery_request(case)))
        .await
        .unwrap();
    // one call per phase; a fourth round would answer the last call again (a no-op on a resolved case)
    for _ in 0..3 {
        let call = calls(fx, case).await.pop().unwrap();
        let out = delivery_flow_output(&call, status);
        o.on_result(answer(&call, OpState::Ok, "ok", out))
            .await
            .unwrap();
    }
    resolutions_in(fx, DELIVERY_AREA, case).await
}

/// The operations `case` called, in order.
async fn operations(fx: &Fx, case: &str) -> Vec<String> {
    calls(fx, case)
        .await
        .into_iter()
        .map(|c| c.operation)
        .collect()
}

/// `(reason, trigger)` of the case's J21 handover items.
pub async fn handover_items(fx: &Fx, case: &str) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT reason, trigger FROM agent_handover_items WHERE tenant_id = $1 AND case_id = $2 ORDER BY queued_at",
    )
    .bind(fx.d.f.tenant)
    .bind(case)
    .fetch_all(&fx.d.e.owner)
    .await
    .unwrap()
}

/// The case's `agent_case.escalation_reason` (the reason of the item that moved it to `handover`).
pub async fn escalation_reason(fx: &Fx, case: &str) -> Option<String> {
    sqlx::query_scalar(
        "SELECT escalation_reason FROM agent_case WHERE tenant_id = $1 AND case_id = $2",
    )
    .bind(fx.d.f.tenant)
    .bind(case)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap()
}

/// Acceptance A4: a late-delivery complaint reads the shipment; a parcel the carrier reports as a problem ends in a
/// J21 handover item (success criteria unmet), never in a "your order is in transit" resolution.
#[tokio::test]
async fn a_late_delivery_complaint_hands_the_case_over() {
    let fx = setup_latest("orch-delivery-late", Env::Test).await;
    let rs = run_delivery(&fx, "CS-1", "problem").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Handover, "{rs:?}");
    assert_eq!(rs[0].code.as_deref(), Some(SUCCESS_CRITERIA_UNMET));
    assert_eq!(rs[0].procedure.as_deref(), Some("delivery_problem@1"));
    assert_eq!(
        operations(&fx, "CS-1").await,
        ["order.read", "delivery.report_problem", "delivery.read"],
        "the problem is reported, the shipment read, then a colleague takes it"
    );
    assert_eq!(rs[0].facts["report"]["case_id"], "DP-1");
    let text = reply::compose(&rs[0]);
    assert!(!text.contains("in transit"), "{text}");
    assert_eq!(
        handover_items(&fx, "CS-1").await,
        [(
            "success_criteria_unmet".to_owned(),
            "success criteria unmet: delivery_on_track".to_owned()
        )]
    );
    assert_eq!(
        escalation_reason(&fx, "CS-1").await.as_deref(),
        Some("success_criteria_unmet")
    );
    assert_eq!(case_row(&fx, "CS-1").await.0, "handover");
}

/// Acceptance A4, the other side: a parcel on its way resolves, and the reply says where it is.
#[tokio::test]
async fn a_delivery_complaint_about_a_parcel_on_track_resolves() {
    let fx = setup_latest("orch-delivery-on-track", Env::Test).await;
    let rs = run_delivery(&fx, "CS-1", "in_transit").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Resolved, "{rs:?}");
    assert_eq!(
        operations(&fx, "CS-1").await,
        ["order.read", "delivery.report_problem", "delivery.read"]
    );
    assert_eq!(
        reply::compose(&rs[0]),
        "Your parcel is in transit, expected by 23 October. I've logged your report."
    );
    assert!(handover_items(&fx, "CS-1").await.is_empty());
    assert_eq!(case_row(&fx, "CS-1").await.0, "resolved");
}

/// A delivery complaint ("tracking says delivered but I never got it") about a parcel the carrier reports
/// `delivered`: the status contradicts the customer, so a colleague takes it — never a "your parcel is delivered"
/// resolution. Mutation: `delivered` back in `DELIVERY_ON_TRACK` resolves here.
#[tokio::test]
async fn a_delivery_complaint_about_a_delivered_parcel_hands_the_case_over() {
    let fx = setup_latest("orch-delivery-delivered", Env::Test).await;
    let rs = run_delivery(&fx, "CS-1", "delivered").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Handover, "{rs:?}");
    assert_eq!(rs[0].code.as_deref(), Some(SUCCESS_CRITERIA_UNMET));
    let text = reply::compose(&rs[0]);
    assert!(!text.contains("delivered"), "{text}");
    assert_eq!(
        handover_items(&fx, "CS-1").await,
        [(
            "success_criteria_unmet".to_owned(),
            "success criteria unmet: delivery_on_track".to_owned()
        )]
    );
    assert_eq!(
        escalation_reason(&fx, "CS-1").await.as_deref(),
        Some("success_criteria_unmet")
    );
    assert_eq!(case_row(&fx, "CS-1").await.0, "handover");
}

/// The staff conversation `conv` of the fixture's job (the J21 item's).
pub async fn insert_conversation(fx: &Fx, conv: &str) {
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

/// The conversation `conv` (created if it does not exist) has the known customer `subject` (state `subject`,
/// `identity_source = assertion`, optional `email`): the cases of `conv` are then at identity level 2 bound to
/// `subject`. What the dropped `agent_case.identity_level / verified_party` used to be seeded with.
pub async fn identify_conversation(fx: &Fx, conv: &str, subject: &str, email: Option<&str>) {
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, $3, $4, 'staff', $5) ON CONFLICT DO NOTHING",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .bind(conv)
    .bind(fx.scope.env.as_str())
    .bind(subject)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    crate::common::state::identify(
        &fx.d.e.owner,
        fx.d.f.tenant,
        fx.d.f.job.job_id,
        conv,
        subject,
        email,
    )
    .await;
}

/// A failed case's reply promises "a colleague will follow up": the case queues its J21 item (`tool_failed`,
/// trigger the customer-safe reason) and stays `handover` for staff. A redelivery after the commit (the case hash
/// lost its `resolved` mark) queues no second item. Mutation: `finish` not queueing leaves no item here.
#[tokio::test]
async fn a_failed_case_queues_one_tool_failed_handover_item() {
    use redis::AsyncCommands;
    let fx = setup_latest("orch-failed-handover", Env::Test).await;
    insert_conversation(&fx, "conv-CS-1").await;
    let o = orch(&fx, StaticProcedures);
    qualify(&fx, &o, "CS-1").await;
    let call = calls(&fx, "CS-1").await[0].clone();
    let env = answer(&call, OpState::Error, "timeout", Value::Null);
    o.on_result(env.clone()).await.unwrap();
    let rs = resolutions(&fx, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Failed);
    assert!(
        reply::compose(&rs[0]).contains("A colleague will follow up"),
        "{rs:?}"
    );
    let item = [(
        "tool_failed".to_owned(),
        "case failed: order.read did not answer in time".to_owned(),
    )];
    assert_eq!(handover_items(&fx, "CS-1").await, item);
    assert_eq!(
        escalation_reason(&fx, "CS-1").await.as_deref(),
        Some("tool_failed"),
        "an operation that did not answer stays tool_failed"
    );
    assert_eq!(case_row(&fx, "CS-1").await.0, "handover");
    // redelivered after the commit, before the case was marked resolved
    let _: i64 = fx
        .bus
        .conn()
        .hdel(o.cases().key("CS-1"), "resolved")
        .await
        .unwrap();
    o.on_result(env).await.unwrap();
    assert_eq!(handover_items(&fx, "CS-1").await, item, "one item per case");
    let rs = resolutions(&fx, "CS-1").await;
    assert!(
        rs.iter().all(|r| r.outcome == CaseOutcome::Failed),
        "the stored resolution is re-published: {rs:?}"
    );
    assert_eq!(case_row(&fx, "CS-1").await.0, "handover");
}

/// N-12: one open item per conversation. A second case of the same conversation failing while the first case's
/// item is open queues nothing (that item holds the conversation) and ends `failed`. Mutation: dropping the
/// open-item check queues a second item here.
#[tokio::test]
async fn a_failed_case_in_a_conversation_with_an_open_item_queues_no_second_item() {
    let fx = setup_latest("orch-failed-n12", Env::Test).await;
    insert_conversation(&fx, "conv-CS-1").await;
    let o = orch(&fx, StaticProcedures);
    qualify(&fx, &o, "CS-1").await;
    let call = calls(&fx, "CS-1").await[0].clone();
    o.on_result(answer(&call, OpState::Error, "timeout", Value::Null))
        .await
        .unwrap();
    assert_eq!(handover_items(&fx, "CS-1").await.len(), 1);
    // CS-2: the same conversation
    insert_case_in(&fx, "CS-2", "conv-CS-1", &current_design(&fx).await).await;
    let mut req = request("CS-2");
    req.conversation_id = "conv-CS-1".to_owned();
    o.on_qualified(qualified(&req)).await.unwrap();
    let call = calls(&fx, "CS-2").await[0].clone();
    o.on_result(answer(&call, OpState::Error, "timeout", Value::Null))
        .await
        .unwrap();
    let rs = resolutions(&fx, "CS-2").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Failed);
    assert!(handover_items(&fx, "CS-2").await.is_empty(), "N-12");
    assert_eq!(handover_items(&fx, "CS-1").await.len(), 1);
    assert_eq!(case_row(&fx, "CS-2").await.0, "failed");
}

/// N-12 under concurrency (audit of agent#54): two cases of one conversation failing at the same moment (two
/// replicas, or the sweeper beside `on_result`) queue exactly one item between them. Five conversations, all ten
/// resolutions at once. Mutation: dropping the control lock in `queue_failed` lets both inserts through.
#[tokio::test]
async fn failed_cases_of_one_conversation_resolving_at_once_queue_one_item() {
    let fx = setup_latest("orch-failed-n12-race", Env::Test).await;
    let o = orch(&fx, StaticProcedures);
    let design = current_design(&fx).await;
    let mut answers = Vec::new();
    for c in 1..=5 {
        let conv = format!("conv-R{c}");
        insert_conversation(&fx, &conv).await;
        // case ids match `^CS-[1-9][0-9]*$` (0060): CS-11/CS-12 … CS-51/CS-52
        for side in 1..=2 {
            let case = format!("CS-{c}{side}");
            insert_case_in(&fx, &case, &conv, &design).await;
            let mut req = request(&case);
            req.conversation_id = conv.clone();
            o.on_qualified(qualified(&req)).await.unwrap();
            let call = calls(&fx, &case).await[0].clone();
            answers.push(answer(&call, OpState::Error, "timeout", Value::Null));
        }
    }
    for r in futures_util::future::join_all(answers.into_iter().map(|a| o.on_result(a))).await {
        r.unwrap();
    }
    for c in 1..=5 {
        let open: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM agent_handover_items WHERE tenant_id = $1 AND conversation_id = $2",
        )
        .bind(fx.d.f.tenant)
        .bind(format!("conv-R{c}"))
        .fetch_one(&fx.d.e.owner)
        .await
        .unwrap();
        assert_eq!(open, 1, "N-12: conv-R{c}");
    }
}

/// Every operation is offered (an instance): the delivery flow's `tool_requirements` pass, as on a tenant whose
/// routes serve them (B7's connector).
pub struct EveryOpOffered;

impl RouteResolver for EveryOpOffered {
    fn resolve<'a>(
        &'a self,
        _c: &'a mut sqlx::PgConnection,
        _q: &'a RouteQuery,
    ) -> futures_util::future::BoxFuture<'a, agent_api::error::ApiResult<ResolvedTarget>> {
        Box::pin(async {
            Ok(ResolvedTarget::Instance {
                instance_id: "every-op".to_owned(),
            })
        })
    }
}

/// A scope whose routes do not offer `delivery.read` (the default routes, no demo stub): the delivery flow hands
/// over before its first call instead of failing mid-flow with the problem unreported (audit of agent#51).
#[tokio::test]
async fn a_delivery_complaint_without_a_delivery_route_hands_over_before_any_call() {
    let fx = setup_latest("orch-delivery-unrouted", Env::Test).await;
    insert_case(&fx, "CS-1", &current_design(&fx).await).await;
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, 'conv-CS-1', $3, 'staff', 'C-10482')",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .bind(fx.scope.env.as_str())
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    let o = orch(&fx, StaticProcedures);
    o.on_qualified(qualified(&delivery_request("CS-1")))
        .await
        .unwrap();
    let rs = resolutions_in(&fx, DELIVERY_AREA, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Handover, "{rs:?}");
    assert_eq!(rs[0].code.as_deref(), Some("not_offered"));
    assert!(operations(&fx, "CS-1").await.is_empty(), "no call is sent");
    let items = handover_items(&fx, "CS-1").await;
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].0, "not_offered", "{items:?}");
    assert!(
        items[0].1.starts_with("procedure needs ")
            && items[0].1.ends_with(", which is not offered"),
        "{items:?}"
    );
    assert_eq!(
        escalation_reason(&fx, "CS-1").await.as_deref(),
        Some("not_offered")
    );
}

/// Live CS-58: "Where is my order RET-01?" (space `retail:logistics:track_parcel`, slot `tracking_ref`, activity
/// `delivery:track_parcel`) handed over `out_of_scope` with no call. It runs as `order_status@1`: one
/// `delivery.read{by_tracking_ref}` (the variant the activity grants) carrying only the reference, and — on a system
/// that offers the variant — resolves with the shipment's status, no J21 item; an empty answer asks for the tracking
/// number. Mutation: dropping the `track_parcel` mapping hands the case over before any call here.
#[tokio::test]
async fn a_track_parcel_request_reads_the_shipment_by_reference_and_resolves() {
    let fx = setup_latest("orch-track-parcel", Env::Test).await;
    let o = orch(&fx, StaticProcedures).with_routes(Arc::new(EveryOpOffered));
    for (case, out, want) in [
        (
            "CS-1",
            json!({"status": "in_transit", "carrier": "PostNL",
                   "expected_window": {"from": "2026-10-21", "until": "2026-10-23"}}),
            "Your parcel is in transit with PostNL, expected by 23 October.",
        ),
        ("CS-2", json!({}), reply::NO_TRACKING),
    ] {
        insert_case(&fx, case, &current_design(&fx).await).await;
        insert_conversation(&fx, &format!("conv-{case}")).await;
        let req: QualifiedRequest = serde_json::from_value(json!({
            "case_id": case, "conversation_id": format!("conv-{case}"), "area": DELIVERY_AREA,
            "context_space": "retail:logistics:track_parcel", "request_type": "track_parcel",
            "activity": crate::b4::b5_support::ACTIVITY_WHERE, "slots": {"tracking_ref": "RET-01"}, "customer_id": "C-10482",
            "identity_level": 1, "message_id": format!("msg-{case}")
        }))
        .unwrap();
        o.on_qualified(qualified(&req)).await.unwrap();
        let sent = calls(&fx, case).await;
        assert_eq!(sent.len(), 1, "{sent:?}");
        let call = &sent[0];
        assert_eq!(
            (call.operation.as_str(), call.variant.as_deref()),
            ("delivery.read", Some("by_tracking_ref"))
        );
        assert_eq!(
            call.activity.as_deref(),
            Some(crate::b4::b5_support::ACTIVITY_WHERE)
        );
        assert_eq!(
            Value::Object(call.input.clone()),
            json!({"tracking_ref": "RET-01"}),
            "only the variant's declared input: never an order id the level-1 read was not granted"
        );
        o.on_result(answer(call, OpState::Ok, "ok", out))
            .await
            .unwrap();
        let rs = resolutions_in(&fx, DELIVERY_AREA, case).await;
        assert_eq!(rs.len(), 1, "{rs:?}");
        assert_eq!(rs[0].outcome, CaseOutcome::Resolved, "{rs:?}");
        assert_eq!(rs[0].procedure.as_deref(), Some("order_status@2"));
        assert_eq!(reply::compose(&rs[0]), want);
        assert!(handover_items(&fx, case).await.is_empty());
        assert_eq!(case_row(&fx, case).await.0, "resolved");
    }
}

/// A system with no `by_tracking_ref` operation (Demo Retail: its `delivery.read` is keyed by order id): the runner
/// refuses the read `not_offered` before the call (step 6b) and the case hands over — no "system unavailable"
/// failure, no other customer's shipment. Mutation: the orchestrator treating that denial as any other fails it.
#[tokio::test]
async fn a_track_parcel_read_the_system_cannot_serve_hands_over() {
    let fx = setup_latest("orch-track-parcel-unserved", Env::Test).await;
    insert_case(&fx, "CS-1", &current_design(&fx).await).await;
    insert_conversation(&fx, "conv-CS-1").await;
    let req: QualifiedRequest = serde_json::from_value(json!({
        "case_id": "CS-1", "conversation_id": "conv-CS-1", "area": DELIVERY_AREA,
        "context_space": "retail:logistics:track_parcel", "request_type": "track_parcel",
        "activity": crate::b4::b5_support::ACTIVITY_WHERE, "slots": {"tracking_ref": "RET-01"}, "customer_id": "C-10482",
        "identity_level": 1, "message_id": "msg-CS-1"
    }))
    .unwrap();
    let o = orch(&fx, StaticProcedures).with_routes(Arc::new(EveryOpOffered));
    o.on_qualified(qualified(&req)).await.unwrap();
    let call = calls(&fx, "CS-1").await.pop().unwrap();
    o.on_result(answer(&call, OpState::Denied, "not_offered", Value::Null))
        .await
        .unwrap();
    let rs = resolutions_in(&fx, DELIVERY_AREA, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Handover, "{rs:?}");
    assert_eq!(rs[0].code.as_deref(), Some("not_offered"));
    assert_eq!(
        reply::compose(&rs[0]),
        "I've passed this to a colleague, who will take it from here."
    );
    assert_eq!(
        handover_items(&fx, "CS-1").await,
        [(
            "not_offered".to_owned(),
            "delivery.read{by_tracking_ref} is not offered by the routed system".to_owned()
        )]
    );
    assert_eq!(
        escalation_reason(&fx, "CS-1").await.as_deref(),
        Some("not_offered")
    );
}

/// The track_parcel read unrouted (the default routes, no demo stub): `not_offered` before any call, as the
/// delivery complaint does.
#[tokio::test]
async fn a_track_parcel_request_without_a_route_hands_over_before_any_call() {
    let fx = setup_latest("orch-track-parcel-unrouted", Env::Test).await;
    insert_case(&fx, "CS-1", &current_design(&fx).await).await;
    insert_conversation(&fx, "conv-CS-1").await;
    let req: QualifiedRequest = serde_json::from_value(json!({
        "case_id": "CS-1", "conversation_id": "conv-CS-1", "area": DELIVERY_AREA,
        "context_space": "retail:logistics:track_parcel", "request_type": "track_parcel",
        "activity": crate::b4::b5_support::ACTIVITY_WHERE, "slots": {"tracking_ref": "RET-01"}, "customer_id": "C-10482",
        "identity_level": 1, "message_id": "msg-CS-1"
    }))
    .unwrap();
    orch(&fx, StaticProcedures)
        .on_qualified(qualified(&req))
        .await
        .unwrap();
    let rs = resolutions_in(&fx, DELIVERY_AREA, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Handover, "{rs:?}");
    assert_eq!(rs[0].code.as_deref(), Some("not_offered"));
    assert!(operations(&fx, "CS-1").await.is_empty(), "no call is sent");
}

/// One phase, one step (`order.read`): the source the deadline tests use.
pub struct OneStep;

impl ProcedureSource for OneStep {
    fn plan(
        &self,
        _design: &agent_api::runtime::design::RuntimeDesign,
        _req: &QualifiedRequest,
    ) -> Option<(Option<ProcedureRef>, Vec<Phase>)> {
        Some((
            None,
            vec![Phase {
                steps: vec![Step::op(
                    "order",
                    "order.read",
                    None,
                    json!({"order_id": "$order_id"}),
                )],
            }],
        ))
    }
}

/// B4 over B5's procedures (acceptance A7 / A13 / A16): a PUBLISHED tenant procedure for the returns space (the
/// fixture's selection holds only order-support), `order.read` then `case.create{return}` (a write, at most twice
/// per case; both inside `support:return`'s grant), runs end to end through the orchestrator the
/// binary runs ([`DesignProcedures`]): its designed steps reach the runner as ordinary `op.call`s — the refund with
/// its idempotency key and its cap, so the runner's identity check, served-operation check (6b) and per-case cap (7b)
/// apply as to any builtin step — and the case records the designed version. Mutation: `StaticProcedures` has no
/// procedure of that name (it runs the builtin `return_request` instead).
#[tokio::test]
async fn a_designed_refund_procedure_runs_its_own_steps_through_the_runner() {
    use agent_api::design::DraftRef;
    use agent_api::runtime::orchestrator::DesignProcedures;
    let fx = setup_latest("orch-designed-refund", Env::Test).await;
    let step = |number: u32, phase: u32, name: &str, op: &str, effect: &str| {
        json!({
            "step_number": number, "phase": phase, "name": name, "operation": op, "variant": null,
            "modality": if effect == "R" { "observe" } else { "do" }, "side_effect": effect,
            "identity_level": "2", "confirm_back": false, "gate": false,
            "args": if effect == "R" { json!({"order_id": "$order_id"}) }
                    else { json!({"order_id": "$order_id", "amount": "$order.total"}) },
        })
    };
    let mut refund = step(2, 1, "refund", "case.create", "W");
    refund["variant"] = json!("return");
    refund["max_per_case"] = json!(2);
    refund["performed_by"] = json!("procedure");
    let mut at = DraftRef::new();
    at.insert("name".into(), json!("refund_fix"));
    crate::common::design_contract::tracked_set(
        &fx.d.e,
        &fx.d.f,
        "procedure",
        &at,
        Some(json!([{
            "version": 1, "status": "PUBLISHED", "context_space": SPACE,
            "description": "Refund a payment", "reviewers": [], "released_in": null, "pddl_domain": null,
            "steps": [step(1, 0, "order", "order.read", "R"), refund],
            "success_criteria": [{"id": "refunded", "fact": "steps.refund.outcome", "op": "eq", "value": "ok"}],
            "tool_requirements": ["case.create{return}"],
        }])),
    )
    .await;
    insert_case(&fx, "CS-1", &current_design(&fx).await).await;
    let req = request("CS-1");
    let o = orch(&fx, DesignProcedures).with_routes(Arc::new(EveryOpOffered));
    o.on_qualified(qualified(&req)).await.unwrap();
    let read = calls(&fx, "CS-1")
        .await
        .pop()
        .expect("the first designed step");
    assert_eq!((read.operation.as_str(), read.phase), ("order.read", 0));
    assert_eq!(read.input["order_id"], "ORD-58790");
    o.on_result(answer(
        &read,
        OpState::Ok,
        "ok",
        json!({"status": "delivered", "total": 59.0}),
    ))
    .await
    .unwrap();
    let cs = calls(&fx, "CS-1").await;
    assert_eq!(cs.len(), 2, "{cs:?}");
    let pay = &cs[1];
    assert_eq!((pay.operation.as_str(), pay.phase), ("case.create", 1));
    assert_eq!(
        pay.input["amount"],
        json!(59.0),
        "bound to the earlier step's output"
    );
    assert_eq!(
        pay.max_per_case,
        Some(2),
        "the runner's per-case cap reads the designed cap"
    );
    assert_eq!(pay.performed_by.as_deref(), Some("procedure"));
    assert_eq!(pay.idempotency_key.as_deref(), Some("CS-1:1:refund"));
    assert_eq!(pay.procedure.as_deref(), Some("refund_fix@1"));
    o.on_result(answer(pay, OpState::Ok, "ok", json!({"refund_id": "RF-1"})))
        .await
        .unwrap();
    let rs = resolutions_in(&fx, AREA, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Resolved, "{rs:?}");
    assert_eq!(rs[0].procedure.as_deref(), Some("refund_fix@1"));
    let (status, _, _, _, name, version) = case_row(&fx, "CS-1").await;
    assert_eq!(status, "resolved");
    assert_eq!((name.as_deref(), version), (Some("refund_fix"), Some(1)));
}

/// Audit of agent#79 (defence in depth): a PUBLISHED tenant procedure for the request's space (returns) whose step
/// (`payment.refund`) is outside the space's activity grant (`support:return`) is refused at plan time: no call is sent and the case
/// hands over `tool_failed` with code `not_entitled` — never `out_of_scope` (the job does have a procedure; its
/// design is what is wrong), never the assistant. Mutation: without the plan-time grant check the refund is planned
/// and `order.read` is sent.
#[tokio::test]
async fn a_designed_procedure_outside_the_space_grant_hands_over_without_a_call() {
    use agent_api::design::DraftRef;
    use agent_api::runtime::orchestrator::DesignProcedures;
    let fx = setup_latest("orch-designed-refused", Env::Test).await;
    let step = |number: u32, phase: u32, name: &str, op: &str, effect: &str| {
        json!({
            "step_number": number, "phase": phase, "name": name, "operation": op, "variant": null,
            "modality": if effect == "R" { "observe" } else { "do" }, "side_effect": effect,
            "identity_level": "2", "confirm_back": false, "gate": false, "args": {"order_id": "$order_id"},
        })
    };
    let mut at = DraftRef::new();
    at.insert("name".into(), json!("refund_fix"));
    crate::common::design_contract::tracked_set(
        &fx.d.e,
        &fx.d.f,
        "procedure",
        &at,
        Some(json!([{
            "version": 1, "status": "PUBLISHED", "context_space": SPACE,
            "description": "Refund a payment", "reviewers": [], "released_in": null, "pddl_domain": null,
            "steps": [step(1, 0, "order", "order.read", "R"), step(2, 1, "refund", "payment.refund", "X")],
            "success_criteria": [], "tool_requirements": [],
        }])),
    )
    .await;
    insert_conversation(&fx, "conv-CS-1").await;
    insert_case(&fx, "CS-1", &current_design(&fx).await).await;
    let o = orch(&fx, DesignProcedures).with_routes(Arc::new(EveryOpOffered));
    o.on_qualified(qualified(&request("CS-1"))).await.unwrap();
    assert!(calls(&fx, "CS-1").await.is_empty(), "no step is sent");
    let items = handover_items(&fx, "CS-1").await;
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].0, "tool_failed", "{items:?}");
    let rs = resolutions_in(&fx, AREA, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Handover, "{rs:?}");
    assert_eq!(rs[0].code.as_deref(), Some("not_entitled"), "{rs:?}");
    assert_eq!(rs[0].procedure, None, "{rs:?}");
}
