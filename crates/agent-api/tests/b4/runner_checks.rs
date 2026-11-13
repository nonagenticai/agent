//! B4 T6a — the runner checks (F-15, F-20): the per-case cap of X/P operations (step 7b), step pre/postconditions,
//! `performed_by`, the procedure's success criteria and `tool_requirements`. Real Postgres 18 + Redis; the runner and
//! the orchestrator connect as `agent_runtime`. Executors are registered fakes that count their calls (the stand-in
//! for demo-backends, T5: "demo-backends sees one write" = one hit and one `ok` `operation_call` row).
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_api::runtime::Env;
use agent_api::runtime::Envelope;
use agent_api::runtime::conditions::{
    self, CaseFacts, CondOp, Condition, ConditionError, StepFact,
};
use agent_api::runtime::customer_context::MAX_CONTEXT_CALLS;
use agent_api::runtime::design::{DesignCache, RuntimeDesign};
use agent_api::runtime::domain::{
    CaseOutcome, OpCall, OpResult, OpState as ResultState, Phase, Plan, ProcedureRef,
    QualifiedRequest, Step,
};
use agent_api::runtime::orchestrator::{ProcedureChecks, ProcedureSource, StaticProcedures};
use agent_api::runtime::routes::ResolvedTarget;
use agent_api::runtime::runner::Runner;
use agent_api::runtime::runner::ops::{
    Executors, OpExecutor, OpOutcome, OpRequest, OpState, outcome,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use crate::b4::design::put_target;
use crate::b4::flow::{
    Fx, answer, calls, case_row, current_design, insert_case, orch, qualify, resolutions,
    state_events,
};
use crate::redis_env as r;

/// A registered executor: counts its calls and answers `out`.
pub(crate) struct Fake {
    hits: AtomicUsize,
    out: OpOutcome,
}

impl Fake {
    pub(crate) fn ok(output: Value) -> Arc<Fake> {
        Arc::new(Fake {
            hits: AtomicUsize::new(0),
            out: outcome(
                OpState::Ok,
                None,
                None,
                output.as_object().cloned().unwrap_or_default(),
            ),
        })
    }

    pub(crate) fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl OpExecutor for Fake {
    fn execute<'a>(
        &'a self,
        _req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move {
            self.hits.fetch_add(1, Ordering::SeqCst);
            self.out.clone()
        })
    }
}

/// The flow fixture with the job's target selection saved (the catalogue has the demo operations:
/// `payment.refund` is X, `order.read` R).
pub(crate) async fn setup(test: &str) -> Fx {
    let fx = crate::b4::flow::setup_latest(test, Env::Test).await;
    put_target(&fx.d).await;
    fx
}

fn runner(fx: &Fx, x: Executors) -> Runner {
    let mut st = fx.d.e.state.clone();
    st.db = fx.rt.clone();
    Runner::new(fx.bus.clone(), st, Arc::new(DesignCache::default()))
        .unwrap()
        .with_consumer("runner-checks-test")
        .with_consume(r::fast())
        .with_executors(x)
}

/// The staff conversation `conv-<case>` with one inbound customer message; the message id (a write's
/// `confirm_ref`).
pub(crate) async fn conversation(fx: &Fx, case: &str) -> String {
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
    let msg: String = sqlx::query_scalar(
        "INSERT INTO conversation_message (tenant_id, job_id, conversation_id, role, text, at_ms) \
         VALUES ($1, $2, $3, 'customer', 'please do it', 1) RETURNING message_id::text",
    )
    .bind(t)
    .bind(j)
    .bind(format!("conv-{case}"))
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    // A1/A2: the customer is known through conversation STATE (`subject` only: a known-customer `identity_source` would start the context preload), which the catalogue's level-1+ ops require
    crate::common::state::put(
        &fx.d.e.owner,
        t,
        j,
        &format!("conv-{case}"),
        "subject",
        "C-1",
    )
    .await;
    msg
}

/// The case row (stamped now) and its conversation; the inbound message id.
pub(crate) async fn case_with_conversation(fx: &Fx, case: &str) -> String {
    insert_case(fx, case, &current_design(fx).await).await;
    conversation(fx, case).await
}

/// An `op.call` of `case`, step `step` (phase 0, attempt 1); `extra` adds T6a fields (`max_per_case`, …).
pub(crate) fn call(
    fx: &Fx,
    case: &str,
    step: &str,
    operation: &str,
    confirm: Option<&str>,
    extra: Value,
) -> OpCall {
    let mut v = json!({
        "call_id": agent_api::runtime::domain::call_id(case, 0, step, 1),
        "case_id": case, "conversation_id": format!("conv-{case}"), "phase": 0, "step": step,
        "attempt": 1, "agent_role": "procedure", "operation": operation, "input": {"order_id": "ORD-1"},
        "idempotency_key": format!("{case}:0:{step}"), "env": fx.scope.env, "confirm_ref": confirm,
        "initiator": "procedure@test", "identity_level": 3, "deadline_ms": 0
    });
    if let (Some(o), Some(x)) = (v.as_object_mut(), extra.as_object()) {
        o.extend(x.clone());
    }
    serde_json::from_value(v).unwrap()
}

pub(crate) fn cond(id: &str, fact: &str, op: &str, value: Value) -> Condition {
    serde_json::from_value(json!({"id": id, "fact": fact, "op": op, "value": value})).unwrap()
}

/// `(status, outcome, alert, escalation_reason)` of the case.
async fn case_state(
    fx: &Fx,
    case: &str,
) -> (String, Option<String>, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT status, outcome, alert, escalation_reason FROM agent_case WHERE tenant_id = $1 AND case_id = $2",
    )
    .bind(fx.d.f.tenant)
    .bind(case)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap()
}

/// `(reason, trigger)` of the case's handover items.
async fn handovers(fx: &Fx, case: &str) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT reason, trigger FROM agent_handover_items WHERE tenant_id = $1 AND case_id = $2 ORDER BY queued_at",
    )
    .bind(fx.d.f.tenant)
    .bind(case)
    .fetch_all(&fx.d.e.owner)
    .await
    .unwrap()
}

/// `operation_call` rows of the case for `operation` in `state`.
async fn rows(fx: &Fx, case: &str, operation: &str, state: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM operation_call WHERE tenant_id = $1 AND case_id = $2 AND operation = $3 AND state = $4",
    )
    .bind(fx.d.f.tenant)
    .bind(case)
    .bind(operation)
    .bind(state)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap()
}

/// A procedure source of fixed phases and checks.
pub(crate) struct Source {
    phases: Vec<Phase>,
    checks: ProcedureChecks,
}

impl ProcedureSource for Source {
    fn plan(
        &self,
        _design: &RuntimeDesign,
        _req: &QualifiedRequest,
    ) -> Option<(Option<ProcedureRef>, Vec<Phase>)> {
        Some((None, self.phases.clone()))
    }

    fn checks(&self, _design: &RuntimeDesign, _req: &QualifiedRequest) -> ProcedureChecks {
        self.checks.clone()
    }
}

/// One phase with `step` (an `order.read` of the request's order, named `order`).
pub(crate) fn one_step(step: Step, checks: ProcedureChecks) -> Source {
    Source {
        phases: vec![Phase { steps: vec![step] }],
        checks,
    }
}

pub(crate) fn order_step() -> Step {
    Step::op(
        "order",
        "order.read",
        None,
        json!({"order_id": "$order_id"}),
    )
}

fn result_env(res: &OpResult) -> Envelope {
    Envelope::new("op.result", "runner", &res.case_id, res).unwrap()
}

/// A WRITE the catalogue does not know (gated as the strongest write, P).
pub(crate) const P_OP: &str = "spaceship.launch";

#[tokio::test]
async fn cap_defaults_to_one_for_x_and_p_ops() {
    let fx = setup("t6a-cap").await;
    let msg = case_with_conversation(&fx, "CS-1").await;
    let refund = Fake::ok(json!({"refund_id": "RF-1"}));
    let launch = Fake::ok(json!({"status": "launched"}));
    let rn = runner(
        &fx,
        Executors::default()
            .register("payment.refund", refund.clone())
            .register(P_OP, launch.clone()),
    );
    let first = call(
        &fx,
        "CS-1",
        "refund1",
        "payment.refund",
        Some(&msg),
        json!({}),
    );
    let a = rn.handle(&first).await.unwrap();
    assert_eq!(a.state, ResultState::Ok, "{a:?}");
    // a second refund of the same case (another step) is not sent
    let second = call(
        &fx,
        "CS-1",
        "refund2",
        "payment.refund",
        Some(&msg),
        json!({}),
    );
    let b = rn.handle(&second).await.unwrap();
    assert_eq!(b.state, ResultState::Denied, "{b:?}");
    assert_eq!(b.outcome_code.as_deref(), Some("case_cap_reached"));
    assert!(!b.retryable);
    // a redelivery of the first call is not capped by itself: its stored outcome answers
    let again = rn.handle(&first).await.unwrap();
    assert_eq!(again.state, ResultState::Ok);
    assert_eq!(refund.hits(), 1, "the system saw one write");
    assert_eq!(rows(&fx, "CS-1", "payment.refund", "ok").await, 1);
    assert_eq!(rows(&fx, "CS-1", "payment.refund", "denied").await, 1);
    // the case hands over (tool_failed), its outcome the cap
    assert_eq!(
        handovers(&fx, "CS-1").await,
        [(
            "tool_failed".to_owned(),
            "per-case limit reached for payment.refund".to_owned()
        )]
    );
    let (status, outcome_code, _, reason) = case_state(&fx, "CS-1").await;
    assert_eq!(status, "handover");
    assert_eq!(outcome_code.as_deref(), Some("case_cap_reached"));
    assert_eq!(reason.as_deref(), Some("tool_failed"));
    let (status, details): (String, Value) = sqlx::query_as(
        "SELECT status, details FROM audit_event WHERE tenant_id = $1 AND action = 'execute' AND resource_id = $2",
    )
    .bind(fx.d.f.tenant)
    .bind(&second.call_id)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(status, "denied");
    assert_eq!(details["code"], "case_cap_reached");
    let (status, details): (String, Value) = sqlx::query_as(
        "SELECT status, details FROM audit_event \
          WHERE tenant_id = $1 AND action = 'operation.denied' AND resource_id = $2",
    )
    .bind(fx.d.f.tenant)
    .bind(&second.call_id)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(status, "denied");
    assert_eq!(details["code"], "case_cap_reached");
    assert_eq!(details["operation"], "payment.refund");
    let ops: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM case_event WHERE tenant_id = $1 AND case_id = 'CS-1' AND kind = 'operation'",
    )
    .bind(fx.d.f.tenant)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(ops, 2, "both calls are case events");

    // a P operation is capped the same way
    let msg2 = case_with_conversation(&fx, "CS-2").await;
    for (step, want) in [("l1", ResultState::Ok), ("l2", ResultState::Denied)] {
        let c = call(&fx, "CS-2", step, P_OP, Some(&msg2), json!({}));
        assert_eq!(rn.handle(&c).await.unwrap().state, want, "{step}");
    }
    assert_eq!(launch.hits(), 1);
}

#[tokio::test]
async fn step_max_per_case_raises_the_cap() {
    let fx = setup("t6a-cap-raised").await;
    let msg = case_with_conversation(&fx, "CS-1").await;
    let refund = Fake::ok(json!({"refund_id": "RF-1"}));
    let rn = runner(
        &fx,
        Executors::default().register("payment.refund", refund.clone()),
    );
    for (step, want) in [
        ("r1", ResultState::Ok),
        ("r2", ResultState::Ok),
        ("r3", ResultState::Denied),
    ] {
        let c = call(
            &fx,
            "CS-1",
            step,
            "payment.refund",
            Some(&msg),
            json!({"max_per_case": 2}),
        );
        let res = rn.handle(&c).await.unwrap();
        assert_eq!(res.state, want, "{step}: {res:?}");
    }
    assert_eq!(refund.hits(), 2);
    assert_eq!(rows(&fx, "CS-1", "payment.refund", "ok").await, 2);
    assert_eq!(case_state(&fx, "CS-1").await.0, "handover");
}

#[tokio::test]
async fn cap_never_applies_to_r_ops() {
    let fx = setup("t6a-cap-reads").await;
    case_with_conversation(&fx, "CS-1").await;
    let read = Fake::ok(json!({"status": "delivered"}));
    let rn = runner(
        &fx,
        Executors::default().register("order.read", read.clone()),
    );
    for step in ["o1", "o2", "o3"] {
        let c = call(&fx, "CS-1", step, "order.read", None, json!({}));
        assert_eq!(
            rn.handle(&c).await.unwrap().state,
            ResultState::Ok,
            "{step}"
        );
    }
    assert_eq!(read.hits(), 3);
    assert!(handovers(&fx, "CS-1").await.is_empty());
    assert_eq!(case_state(&fx, "CS-1").await.0, "open");
}

#[tokio::test]
async fn false_precondition_sends_nothing_and_hands_over() {
    let fx = setup("t6a-pre").await;
    let read = Fake::ok(json!({"status": "delivered"}));
    let rn = runner(
        &fx,
        Executors::default().register("order.read", read.clone()),
    );
    // the request is verified (2); the step needs step-up (3)
    let mut step = order_step();
    step.preconditions = vec![cond("needs_step_up", "identity.level", "gte", json!(3))];
    let o = orch(&fx, one_step(step, ProcedureChecks::default()));
    conversation(&fx, "CS-1").await;
    qualify(&fx, &o, "CS-1").await;
    let c = calls(&fx, "CS-1").await.remove(0);
    assert_eq!(c.preconditions.len(), 1, "the step's conditions travel");
    assert_eq!(c.facts.identity_level, 2);
    assert_eq!(c.facts.slots["order_id"], "ORD-58790");
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::Denied, "{res:?}");
    assert_eq!(res.outcome_code.as_deref(), Some("precondition_unmet"));
    assert_eq!(read.hits(), 0, "nothing was sent");
    let detail: Option<String> = sqlx::query_scalar(
        "SELECT detail FROM operation_call WHERE tenant_id = $1 AND call_id = $2",
    )
    .bind(fx.d.f.tenant)
    .bind(&c.call_id)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(detail.as_deref(), Some("needs_step_up"), "the failed ids");
    o.on_result(result_env(&res)).await.unwrap();
    let rs = resolutions(&fx, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Handover);
    assert_eq!(rs[0].code.as_deref(), Some("precondition_unmet"));
    assert_eq!(
        handovers(&fx, "CS-1").await,
        [(
            "tool_failed".to_owned(),
            "preconditions unmet for order.read".to_owned()
        )]
    );
    let (status, outcome_code, _, reason) = case_state(&fx, "CS-1").await;
    assert_eq!(
        (status.as_str(), outcome_code.as_deref(), reason.as_deref()),
        ("handover", Some("precondition_unmet"), Some("tool_failed"))
    );

    // control: a precondition that holds sends the call
    let mut step = order_step();
    step.preconditions = vec![cond("has_order", "slots.order_id", "exists", Value::Null)];
    let o = orch(&fx, one_step(step, ProcedureChecks::default()));
    conversation(&fx, "CS-2").await;
    qualify(&fx, &o, "CS-2").await;
    let c = calls(&fx, "CS-2").await.remove(0);
    assert_eq!(rn.handle(&c).await.unwrap().state, ResultState::Ok);
    assert_eq!(read.hits(), 1);
}

#[tokio::test]
async fn false_precondition_on_gate_step_is_not_eligible() {
    let fx = setup("t6a-pre-gate").await;
    let read = Fake::ok(json!({"status": "delivered"}));
    let rn = runner(
        &fx,
        Executors::default().register("order.read", read.clone()),
    );
    let mut step = order_step().gated();
    step.preconditions = vec![cond("gold_only", "customer.tier", "eq", json!("gold"))];
    let o = orch(&fx, one_step(step, ProcedureChecks::default()));
    conversation(&fx, "CS-1").await;
    qualify(&fx, &o, "CS-1").await;
    let c = calls(&fx, "CS-1").await.remove(0);
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.outcome_code.as_deref(), Some("precondition_unmet"));
    o.on_result(result_env(&res)).await.unwrap();
    let rs = resolutions(&fx, "CS-1").await;
    assert_eq!(rs[0].outcome, CaseOutcome::NotEligible, "{rs:?}");
    assert_eq!(rs[0].code.as_deref(), Some("precondition_unmet"));
    assert!(
        handovers(&fx, "CS-1").await.is_empty(),
        "a gate hands nothing over"
    );
    assert_eq!(case_row(&fx, "CS-1").await.0, "not_eligible");
    assert_eq!(read.hits(), 0);
}

#[test]
fn missing_fact_is_false_except_absent() {
    let mut f = CaseFacts::default();
    f.slots.insert("order_id".into(), json!("ORD-1"));
    f.steps.insert(
        "order".into(),
        StepFact::answered(&json!({"status": "delivered"})),
    );
    for fact in [
        "slots.missing",
        "customer.tier",
        "case.activity",
        "steps.nope.outcome",
        "steps.order.out.nope",
        "steps.order.out.status.deeper",
    ] {
        for (op, value) in [
            ("eq", json!("x")),
            ("ne", json!("x")),
            ("in", json!(["x"])),
            ("gte", json!(1)),
            ("lte", json!(1)),
            ("exists", Value::Null),
        ] {
            assert!(
                !conditions::eval(&cond("c", fact, op, value), &f),
                "{fact} {op}"
            );
        }
        assert!(
            conditions::eval(&cond("c", fact, "absent", Value::Null), &f),
            "{fact}"
        );
    }
    // a present fact: absent is false, the comparisons decide
    assert!(!conditions::eval(
        &cond("c", "slots.order_id", "absent", Value::Null),
        &f
    ));
    assert!(conditions::eval(
        &cond("c", "slots.order_id", "ne", json!("x")),
        &f
    ));
    assert!(conditions::eval(
        &cond("c", "steps.order.outcome", "eq", json!("ok")),
        &f
    ));
    assert!(conditions::eval(
        &cond("c", "identity.level", "lte", json!(0)),
        &f
    ));
}

#[tokio::test]
async fn false_postcondition_alerts_and_hands_over_without_retry() {
    let fx = setup("t6a-post").await;
    let lost = Fake::ok(json!({"status": "lost"}));
    let rn = runner(
        &fx,
        Executors::default().register("order.read", lost.clone()),
    );
    let mut step = order_step();
    step.postconditions = vec![cond(
        "delivered",
        "steps.order.out.status",
        "eq",
        json!("delivered"),
    )];
    let o = orch(&fx, one_step(step.clone(), ProcedureChecks::default()));
    conversation(&fx, "CS-1").await;
    qualify(&fx, &o, "CS-1").await;
    let c = calls(&fx, "CS-1").await.remove(0);
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::Ok, "the call itself succeeded");
    assert_eq!(res.outcome_code.as_deref(), Some("postcondition_failed"));
    assert!(!res.retryable);
    assert_eq!(lost.hits(), 1);
    let (status, outcome_code, alert, reason) = case_state(&fx, "CS-1").await;
    assert_eq!(alert.as_deref(), Some("postcondition_failed"));
    assert_eq!(
        (status.as_str(), outcome_code.as_deref(), reason.as_deref()),
        (
            "handover",
            Some("postcondition_failed"),
            Some("tool_failed")
        )
    );
    assert_eq!(
        handovers(&fx, "CS-1").await,
        [(
            "tool_failed".to_owned(),
            "postcondition failed: delivered".to_owned()
        )]
    );
    let alerts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM case_event WHERE tenant_id = $1 AND case_id = 'CS-1' AND kind = 'alert'",
    )
    .bind(fx.d.f.tenant)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(alerts, 1);
    assert!(
        state_events(&fx, "CS-1")
            .await
            .iter()
            .any(|e| e["event"] == "postcondition failed: delivered"),
        "the state event names the failed ids"
    );
    // the orchestrator hands the case over and never retries the step
    o.on_result(result_env(&res)).await.unwrap();
    let rs = resolutions(&fx, "CS-1").await;
    assert_eq!(rs[0].outcome, CaseOutcome::Handover);
    assert_eq!(rs[0].code.as_deref(), Some("postcondition_failed"));
    assert_eq!(calls(&fx, "CS-1").await.len(), 1, "no retry");
    assert_eq!(lost.hits(), 1);

    // control: a postcondition that holds leaves the call and the case alone
    let fine = Fake::ok(json!({"status": "delivered"}));
    let rn = runner(
        &fx,
        Executors::default().register("order.read", fine.clone()),
    );
    let o = orch(&fx, one_step(step, ProcedureChecks::default()));
    conversation(&fx, "CS-2").await;
    qualify(&fx, &o, "CS-2").await;
    let c = calls(&fx, "CS-2").await.remove(0);
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(
        (res.state, res.outcome_code.as_deref()),
        (ResultState::Ok, None)
    );
    o.on_result(result_env(&res)).await.unwrap();
    assert_eq!(
        resolutions(&fx, "CS-2").await[0].outcome,
        CaseOutcome::Resolved
    );
    assert_eq!(case_state(&fx, "CS-2").await.2, None, "no alert");
}

#[tokio::test]
async fn unmet_success_criteria_hand_over_with_outcome() {
    let fx = setup("t6a-success").await;
    let checks = ProcedureChecks {
        success_criteria: vec![cond(
            "delivered",
            "steps.order.out.status",
            "eq",
            json!("delivered"),
        )],
        tool_requirements: Vec::new(),
    };
    for (case, status, want) in [
        ("CS-1", "lost", CaseOutcome::Handover),
        ("CS-2", "delivered", CaseOutcome::Resolved),
    ] {
        let o = orch(&fx, one_step(order_step(), checks.clone()));
        conversation(&fx, case).await;
        qualify(&fx, &o, case).await;
        let plan: Plan = o.cases().plan(case).await.unwrap().unwrap();
        assert_eq!(
            plan.success_criteria.len(),
            1,
            "the plan carries the criteria"
        );
        let c = calls(&fx, case).await.remove(0);
        o.on_result(answer(&c, ResultState::Ok, "ok", json!({"status": status})))
            .await
            .unwrap();
        let rs = resolutions(&fx, case).await;
        assert_eq!(rs[0].outcome, want, "{case}: {rs:?}");
    }
    let rs = resolutions(&fx, "CS-1").await;
    assert_eq!(rs[0].code.as_deref(), Some("success_criteria_unmet"));
    let (status, outcome_code, _, reason) = case_state(&fx, "CS-1").await;
    assert_eq!(
        (status.as_str(), outcome_code.as_deref(), reason.as_deref()),
        (
            "handover",
            Some("success_criteria_unmet"),
            Some("success_criteria_unmet")
        )
    );
    assert_eq!(
        handovers(&fx, "CS-1").await,
        [(
            "success_criteria_unmet".to_owned(),
            "success criteria unmet: delivered".to_owned()
        )]
    );
    assert_eq!(case_row(&fx, "CS-2").await.0, "resolved");
    assert!(handovers(&fx, "CS-2").await.is_empty());
}

#[tokio::test]
async fn unrouted_tool_requirement_hands_over_before_the_first_step() {
    let fx = setup("t6a-tools").await;
    let needs = |op: &str| ProcedureChecks {
        success_criteria: Vec::new(),
        tool_requirements: vec![op.to_owned()],
    };
    // nothing routes spaceship.launch: not_offered
    let o = orch(&fx, one_step(order_step(), needs(P_OP)));
    conversation(&fx, "CS-1").await;
    qualify(&fx, &o, "CS-1").await;
    assert!(calls(&fx, "CS-1").await.is_empty(), "no step ran");
    let rs = resolutions(&fx, "CS-1").await;
    assert_eq!(rs[0].outcome, CaseOutcome::Handover, "{rs:?}");
    assert_eq!(rs[0].code.as_deref(), Some("not_offered"));
    assert_eq!(
        handovers(&fx, "CS-1").await,
        [(
            "not_offered".to_owned(),
            "procedure needs spaceship.launch, which is not offered".to_owned()
        )]
    );
    assert_eq!(case_state(&fx, "CS-1").await.0, "handover");
    // control: a built-in requirement is offered, the first step runs
    let o = orch(&fx, one_step(order_step(), needs("rule.evaluate{RET-01}")));
    conversation(&fx, "CS-2").await;
    qualify(&fx, &o, "CS-2").await;
    assert_eq!(calls(&fx, "CS-2").await.len(), 1);
    assert!(resolutions(&fx, "CS-2").await.is_empty());
}

#[test]
fn validate_rejects_unknown_path_and_forward_step_reference() {
    let good = [
        cond("has_order", "slots.order_id", "exists", Value::Null),
        cond("verified", "identity.level", "gte", json!(2)),
        cond("tier", "customer.tier", "in", json!(["gold", "silver"])),
        cond("read_ok", "steps.order.outcome", "eq", json!("ok")),
        cond(
            "shipped",
            "steps.order.out.ship.status",
            "ne",
            json!("lost"),
        ),
        cond("activity", "case.activity", "absent", Value::Null),
    ];
    assert_eq!(conditions::validate(&good, &["order"]), Ok(()));
    for fact in [
        "order.status",
        "slots.",
        "slots.a.b",
        "steps.order",
        "steps.order.out",
        "identity.tier",
        "case",
    ] {
        assert_eq!(
            conditions::validate(&[cond("c", fact, "exists", Value::Null)], &["order"]),
            Err(ConditionError::UnknownPath {
                id: "c".into(),
                fact: fact.into()
            }),
            "{fact}"
        );
    }
    // a step that runs later (or not at all) cannot be read
    assert_eq!(
        conditions::validate(
            &[cond("refunded", "steps.rma.out.id", "exists", Value::Null)],
            &["order"]
        ),
        Err(ConditionError::ForwardStepReference {
            id: "refunded".into(),
            step: "rma".into()
        })
    );
    for (op, value) in [
        ("in", json!("gold")),
        ("in", json!([])),
        ("eq", json!({"a": 1})),
        ("eq", Value::Null),
        ("exists", json!(true)),
        ("gte", json!(true)),
    ] {
        assert_eq!(
            conditions::validate(&[cond("c", "customer.tier", op, value.clone())], &[]),
            Err(ConditionError::BadValue { id: "c".into() }),
            "{op} {value}"
        );
    }
    assert_eq!(
        conditions::validate(&[cond("Bad", "customer.tier", "exists", Value::Null)], &[]),
        Err(ConditionError::BadId("Bad".into()))
    );
    let twice = cond("a", "customer.tier", "exists", Value::Null);
    assert_eq!(
        conditions::validate(&[twice.clone(), twice], &[]),
        Err(ConditionError::DuplicateId("a".into()))
    );
    assert_eq!(conditions::FACT_PATHS.len(), 6);
    let parsed: Condition =
        serde_json::from_value(json!({"id": "x", "fact": "case.activity", "op": "absent"}))
            .unwrap();
    assert_eq!(parsed.op, CondOp::Absent);
    assert_eq!(parsed.value, None);
}

#[tokio::test]
async fn pre_b5_design_without_fields_runs_unchanged() {
    // a pre-B5 step, plan and op.call carry none of the T6a fields and keep their wire form
    let step: Step = serde_json::from_value(json!({
        "name": "order", "operation": "order.read", "args": {"order_id": "$order_id"}
    }))
    .unwrap();
    assert!(step.preconditions.is_empty() && step.postconditions.is_empty());
    assert!(step.performed_by.is_none() && step.max_per_case.is_none());
    assert!(!step.has_conditions());
    let plan: Plan = serde_json::from_value(json!({
        "procedure": null, "design": {"kind": "draft", "draft_rev": 4},
        "phases": [{"steps": [serde_json::to_value(&step).unwrap()]}]
    }))
    .unwrap();
    assert!(plan.success_criteria.is_empty() && plan.tool_requirements.is_empty());
    let wire = serde_json::to_value(&plan).unwrap();
    assert!(wire.get("success_criteria").is_none() && wire.get("tool_requirements").is_none());

    // the built-in return flow plans and dispatches as before: no checks, no facts on the call
    let fx = setup("t6a-pre-b5").await;
    let o = orch(&fx, StaticProcedures);
    qualify(&fx, &o, "CS-1").await;
    let plan = o.cases().plan("CS-1").await.unwrap().unwrap();
    assert!(plan.success_criteria.is_empty() && plan.tool_requirements.is_empty());
    let c = calls(&fx, "CS-1").await.remove(0);
    assert!(c.preconditions.is_empty() && c.postconditions.is_empty());
    assert!(c.facts.is_empty() && c.max_per_case.is_none() && c.performed_by.is_none());
    let raw: Vec<Envelope> = r::entries(&fx.bus, &fx.bus.streams.op()).await;
    let payload = &raw
        .iter()
        .find(|e| e.correlation_id == "CS-1")
        .unwrap()
        .payload;
    for k in [
        "facts",
        "preconditions",
        "postconditions",
        "performed_by",
        "max_per_case",
    ] {
        assert!(payload.get(k).is_none(), "{k} on a pre-B5 call");
    }
    // and the runner runs it like any call (A2: the conversation's customer is known, `subject` in state)
    conversation(&fx, "CS-1").await;
    let read = Fake::ok(json!({"status": "delivered"}));
    let rn = runner(
        &fx,
        Executors::default().register("order.read", read.clone()),
    );
    let res = rn.handle(&c).await.unwrap();
    assert_eq!((res.state, res.outcome_code), (ResultState::Ok, None));
    assert_eq!(read.hits(), 1);
}

// ---- B4 known customer: the runner's `context` provenance (plan §2.2) ----------------------------------------------

/// A web chat case of the known customer `party` (identity level 2, bound), its conversation `conv-<case>` and one
/// inbound customer message. Production shape: ownership binds every call to `party`.
pub(crate) async fn known_case(fx: &Fx, case: &str, party: &str) {
    let (t, j) = (fx.d.f.tenant, fx.d.f.job.job_id);
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, $3, $4, 'web_chat', $5)",
    )
    .bind(t)
    .bind(j)
    .bind(format!("conv-{case}"))
    .bind(fx.scope.env.as_str())
    .bind(party)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    let draft_rev = match current_design(fx).await {
        agent_api::design::CaseDesign::Draft { draft_rev } => Some(draft_rev),
        agent_api::design::CaseDesign::Live { .. } => None,
    };
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, release_id, draft_rev, \
                                 qualified_release_id, qualified_draft_rev, status) \
         VALUES ($1, $2, $3, $4, 'web_chat', $5, NULL, $6, NULL, $6, 'open')",
    )
    .bind(t)
    .bind(j)
    .bind(case)
    .bind(format!("conv-{case}"))
    .bind(fx.scope.env.as_str())
    .bind(draft_rev)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    crate::common::state::identify(&fx.d.e.owner, t, j, &format!("conv-{case}"), party, None).await;
}

/// An `op.call` of the context phase: provenance `context`, a read (no key, no confirmation), level 2.
pub(crate) fn ctx_call(fx: &Fx, case: &str, step: &str, operation: &str, input: Value) -> OpCall {
    call(
        fx,
        case,
        step,
        operation,
        None,
        json!({"provenance": "context", "input": input, "idempotency_key": null, "identity_level": 2}),
    )
}

/// A gate that refuses every call (B5's ToolPoint on an activity that grants nothing).
struct DenyAll;

impl agent_api::runtime::runner::authorise::ToolGate for DenyAll {
    fn check<'a>(
        &'a self,
        _st: &'a agent_api::state::AppState,
        _tenant: uuid::Uuid,
        _design: &'a RuntimeDesign,
        _call: &'a OpCall,
        _op_key: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Err("not_entitled".to_owned()) })
    }
}

/// A `context` call to anything outside the read allowlist is refused `not_context_op` before an executor sees it:
/// a write, a write the catalogue knows as X, a read that is not on the list, and an operation nobody knows.
/// Mutation: the allowlist check removed (the refund would run: its executor counts the hit).
#[tokio::test]
async fn a_context_call_outside_the_allowlist_is_refused_not_context_op() {
    let fx = setup("ctx-allowlist").await;
    known_case(&fx, "CS-1", "C-1").await;
    let refund = Fake::ok(json!({"refund_id": "RF-1"}));
    let cancel = Fake::ok(json!({"status": "cancelled"}));
    let parties = Fake::ok(json!({"party_id": ["C-1"]}));
    let rn = runner(
        &fx,
        Executors::default()
            .register("payment.refund", refund.clone())
            .register("order.cancel", cancel.clone())
            .register("party.list", parties.clone()),
    );
    for (i, (op, input)) in [
        ("payment.refund", json!({"payment_id": "PAY-1"})),
        ("order.cancel", json!({"order_id": "O1"})),
        ("party.list", json!({"email": "ann@example.org"})),
        (P_OP, json!({})),
    ]
    .into_iter()
    .enumerate()
    {
        let res = rn
            .handle(&ctx_call(&fx, "CS-1", &format!("ctx:s{i}"), op, input))
            .await
            .unwrap();
        assert_eq!(res.state, ResultState::Denied, "{op}: {res:?}");
        assert_eq!(res.outcome_code.as_deref(), Some("not_context_op"), "{op}");
        assert!(res.output.is_none() && !res.retryable);
    }
    assert_eq!(
        (refund.hits(), cancel.hits(), parties.hits()),
        (0, 0, 0),
        "no executor saw a refused call"
    );
    // each refusal is a denied, audited row, and does not hand the case over
    let denied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM operation_call WHERE tenant_id = $1 AND case_id = 'CS-1' AND state = 'denied' \
            AND outcome_code = 'not_context_op'",
    )
    .bind(fx.d.f.tenant)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(denied, 4);
    assert!(handovers(&fx, "CS-1").await.is_empty());
}

/// A context read skips the activity / ToolPoint gate (the request's activity lists none of the context reads), and
/// ONLY a context read does: the same read as a plain step is still refused by the gate. Mutation: the gate asked
/// for context calls too; or skipped for every call.
#[tokio::test]
async fn a_context_read_skips_the_tool_gate_and_a_plain_read_does_not() {
    let fx = setup("ctx-gate").await;
    known_case(&fx, "CS-1", "C-1").await;
    let list = Fake::ok(json!({"order_id": ["O1"], "status": ["paid"]}));
    let rn = runner(
        &fx,
        Executors::default().register("order.list", list.clone()),
    )
    .with_gate(Arc::new(DenyAll));
    let ctx = rn
        .handle(&ctx_call(
            &fx,
            "CS-1",
            "ctx:order.list",
            "order.list",
            json!({"party_id": "C-1"}),
        ))
        .await
        .unwrap();
    assert_eq!(ctx.state, ResultState::Ok, "{ctx:?}");
    let plain = call(
        &fx,
        "CS-1",
        "plain",
        "order.list",
        None,
        json!({"input": {"party_id": "C-1"}, "idempotency_key": null, "identity_level": 2}),
    );
    let res = rn.handle(&plain).await.unwrap();
    assert_eq!(res.state, ResultState::Denied, "{res:?}");
    assert_eq!(res.outcome_code.as_deref(), Some("not_entitled"));
    assert_eq!(list.hits(), 1);
}

/// Context calls have their own cap: at most `MAX_CONTEXT_CALLS` run in one case, the next is `context_cap_reached` (no
/// executor, no handover), every call is an `operation_call` row, an `op_calls` tick (bookkeeping only; they never
/// spend the per-case step cap, see the test below) and an `execute` audit row with phase `context`. Mutation: no count.
#[tokio::test]
async fn context_calls_have_their_own_cap_and_are_audited_with_phase_context() {
    let fx = setup("ctx-cap").await;
    known_case(&fx, "CS-1", "C-1").await;
    let list = Fake::ok(json!({"order_id": ["O1"], "status": ["paid"]}));
    let rn = runner(
        &fx,
        Executors::default().register("order.list", list.clone()),
    );
    let cap = usize::try_from(MAX_CONTEXT_CALLS).unwrap();
    for i in 0..cap {
        let res = rn
            .handle(&ctx_call(
                &fx,
                "CS-1",
                &format!("ctx:n{i}"),
                "order.list",
                json!({"party_id": "C-1"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.state, ResultState::Ok, "call {i}: {res:?}");
    }
    let over = rn
        .handle(&ctx_call(
            &fx,
            "CS-1",
            &format!("ctx:n{cap}"),
            "order.list",
            json!({"party_id": "C-1"}),
        ))
        .await
        .unwrap();
    assert_eq!(over.state, ResultState::Denied, "{over:?}");
    assert_eq!(over.outcome_code.as_deref(), Some("context_cap_reached"));
    assert_eq!(list.hits(), cap);
    assert!(
        handovers(&fx, "CS-1").await.is_empty(),
        "a context cap never hands the case over"
    );
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE tenant_id = $1 AND action = 'execute' \
            AND details->>'phase' = 'context' AND details->>'case_id' = 'CS-1'",
    )
    .bind(fx.d.f.tenant)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(audited, MAX_CONTEXT_CALLS + 1);
    let (ctx_rows, ops): (i64, i32) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM operation_call WHERE tenant_id = $1 AND case_id = 'CS-1' \
                   AND step->>'phase' = 'context'), op_calls FROM agent_case WHERE tenant_id = $1 AND case_id = 'CS-1'",
    )
    .bind(fx.d.f.tenant)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    let over_cap = i32::try_from(MAX_CONTEXT_CALLS).unwrap() + 1;
    assert_eq!(
        (ctx_rows, ops),
        (MAX_CONTEXT_CALLS + 1, over_cap),
        "they are operations of the case"
    );
}

/// The preload's reads never spend the case's step budget: after `MAX_CONTEXT_CALLS` context reads the case still
/// runs a capped write (`max_per_case` 2) twice, and only the third is `case_cap_reached`. The per-case cap counts
/// `operation_call` rows of the same X/P operation; context calls are reads of other operations.
#[tokio::test]
async fn a_full_context_preload_leaves_the_steps_their_whole_case_cap() {
    let fx = setup("ctx-leaves-cap").await;
    let msg = case_with_conversation(&fx, "CS-1").await;
    let list = Fake::ok(json!({"order_id": ["O1"], "status": ["paid"]}));
    let refund = Fake::ok(json!({"refund_id": "RF-1"}));
    let rn = runner(
        &fx,
        Executors::default()
            .register("order.list", list.clone())
            .register("payment.refund", refund.clone()),
    );
    let cap = usize::try_from(MAX_CONTEXT_CALLS).unwrap();
    for i in 0..cap {
        let res = rn
            .handle(&ctx_call(
                &fx,
                "CS-1",
                &format!("ctx:n{i}"),
                "order.list",
                json!({"party_id": "C-1"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.state, ResultState::Ok, "context {i}: {res:?}");
    }
    for (step, want) in [
        ("r1", ResultState::Ok),
        ("r2", ResultState::Ok),
        ("r3", ResultState::Denied),
    ] {
        let c = call(
            &fx,
            "CS-1",
            step,
            "payment.refund",
            Some(&msg),
            json!({"max_per_case": 2}),
        );
        let res = rn.handle(&c).await.unwrap();
        assert_eq!(res.state, want, "{step}: {res:?}");
        if want == ResultState::Denied {
            assert_eq!(res.outcome_code.as_deref(), Some("case_cap_reached"));
        }
    }
    assert_eq!(refund.hits(), 2);
    assert_eq!(list.hits(), cap);
}
