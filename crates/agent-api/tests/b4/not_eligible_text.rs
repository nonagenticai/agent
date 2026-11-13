//! B4 audit card — a `not_eligible` reply shows the gate's `reason` only when the gate is the platform's
//! `rule.evaluate` (the rule's authored, customer-facing text). A connector's gate text is untrusted: the reply is
//! the fixed [`reply::NOT_ELIGIBLE`] and the resolution carries no reason (the gate's output stays in the case
//! results); the rule's passthrough is `flow.rs` `a_gate_that_answers_false_ends_the_case_not_eligible`. Real Postgres 18
//! + Redis.
use agent_api::runtime::Env;
use agent_api::runtime::design::RuntimeDesign;
use agent_api::runtime::domain::{
    CaseOutcome, OpState, Phase, ProcedureRef, QualifiedRequest, Step,
};
use agent_api::runtime::orchestrator::ProcedureSource;
use agent_api::runtime::reply;
use serde_json::json;

use crate::b4::flow::{answer, calls, orch, qualify, resolutions, setup};

const SECRET: &str = "INTERNAL-SECRET-123";

/// One phase, one gate step run by a connector (`eligibility.check`), not the platform's rule.
struct ConnectorGate;

impl ProcedureSource for ConnectorGate {
    fn plan(
        &self,
        _design: &RuntimeDesign,
        _req: &QualifiedRequest,
    ) -> Option<(Option<ProcedureRef>, Vec<Phase>)> {
        let mut gate = Step::op("eligible", "eligibility.check", None, json!({}));
        gate.gate = true;
        Some((None, vec![Phase { steps: vec![gate] }]))
    }
}

#[tokio::test]
async fn a_connector_gates_reason_never_reaches_the_customer() {
    let fx = setup("orch-not-eligible-connector", Env::Test).await;
    let o = orch(&fx, ConnectorGate);
    qualify(&fx, &o, "CS-1").await;
    let call = calls(&fx, "CS-1").await.pop().unwrap();
    assert_eq!(call.operation, "eligibility.check");
    o.on_result(answer(
        &call,
        OpState::Ok,
        "ok",
        json!({"outcome": "ineligible", "code": "ineligible:x", "reason": SECRET}),
    ))
    .await
    .unwrap();
    let rs = resolutions(&fx, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::NotEligible);
    assert_eq!(rs[0].reason, None, "{rs:?}");
    let text = reply::compose(&rs[0]);
    assert_eq!(text, reply::NOT_ELIGIBLE);
    assert!(!text.contains(SECRET), "{text}");
}
