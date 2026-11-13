//! B5 T10a — the verdict cause (D94: a Laya outage at a fail-closed question is `gate_unavailable`, never
//! `gate_blocked`) and the qualifier route hint (F-19, `job_design::gates::route_hint::QualifierHint`): GA restricted
//! to the top-2 GS stages, a confidence line, and the request type of the activity's context space.
use std::collections::BTreeMap;

use agent_api::design::{DesignOf, JobRef};
use agent_api::job_design::gates::evaluate::{Outcome, evaluate};
use agent_api::job_design::gates::route_hint::{ACTIVITY_GATE, QualifierHint, STAGE_GATE};
use agent_api::runtime::design::RuntimeDesign;
use agent_api::runtime::gates::{
    Decision, GateAnswerLite, GateVerdict, RouteHint, RouteHinter, VerdictCause,
};
use agent_api::runtime::spaces::{ContextSpace, SpaceCatalog};
use serde_json::json;
use uuid::Uuid;

use crate::b5::laya::{client, fake, noul, question, rule};

#[tokio::test]
async fn laya_down_on_closed_question_is_unavailable_not_fired() {
    let f = fake().await;
    let closed = noul("SAFE", "P1", "closed", "handover", None);
    let res = evaluate(Some(&client(&f, "down")), json!("x"), &[&closed]).await;
    assert_eq!(res.outcome, Outcome::FailedClosed);
    let v = res.verdict(Some(9));
    assert_eq!(
        (v.decision, v.cause, v.handover_reason()),
        (
            Decision::Handover,
            VerdictCause::Unavailable,
            Some("gate_unavailable")
        )
    );
    assert!(v.answers.is_empty(), "no answers from a failed point");
}

#[tokio::test]
async fn fired_handover_is_cause_fired() {
    let f = fake().await;
    // the fake answers noul 0.94: a gte 0.5 rule fires
    let q = noul("LEGAL", "P1", "closed", "handover", None);
    let res = evaluate(Some(&client(&f, "ok")), json!("x"), &[&q]).await;
    let v = res.verdict(None);
    assert_eq!(
        (v.decision, v.cause, v.handover_reason()),
        (
            Decision::Handover,
            VerdictCause::Fired,
            Some("gate_blocked")
        )
    );
    assert_eq!(v.deciding, ["LEGAL"]);
    // the verdict carries the answer's key and probabilities, never text
    assert_eq!(v.answers.len(), 1);
    assert_eq!(
        (v.answers[0].gate.as_str(), v.answers[0].value.as_str()),
        ("LEGAL", "true")
    );
    assert!((v.answers[0].probabilities["true"] - 0.94).abs() < 1e-9);
    let body = serde_json::to_value(&v).unwrap();
    assert!(body["answers"].is_array());
    assert!(!body.to_string().contains("Question LEGAL"));
}

#[tokio::test]
async fn verdict_carries_choice_probabilities() {
    let f = fake().await;
    let c = question(
        "GT",
        "P1",
        "choice",
        &[
            ("issue", "Issue", "Broken", false),
            ("need", "Need", "", false),
        ],
        rule("in", None, Some(&["issue"]), None),
        "open",
        "annotate",
        None,
    );
    let v = evaluate(Some(&client(&f, "ok")), json!("x"), &[&c])
        .await
        .verdict(None);
    assert_eq!(
        v.decision,
        Decision::Allow,
        "annotate never changes the turn"
    );
    assert_eq!(v.answers[0].value, "issue");
    assert_eq!(v.answers[0].probabilities.len(), 2);
}

fn lite(gate: &str, ps: &[(&str, f64)]) -> GateAnswerLite {
    let probabilities: BTreeMap<String, f64> =
        ps.iter().map(|(k, p)| ((*k).to_owned(), *p)).collect();
    let value = ps
        .iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(k, _)| (*k).to_owned())
        .unwrap_or_default();
    GateAnswerLite {
        gate: gate.into(),
        value,
        probabilities,
    }
}

fn verdict(answers: Vec<GateAnswerLite>) -> GateVerdict {
    GateVerdict {
        answers,
        ..GateVerdict::allow()
    }
}

fn space(key: &str, activity: &str) -> ContextSpace {
    ContextSpace {
        key: key.into(),
        label: key.into(),
        description: String::new(),
        area: "order-support".into(),
        knowledge_kinds: vec![],
        slots: vec![],
        activity: activity.into(),
    }
}

fn design() -> RuntimeDesign {
    let mut d = RuntimeDesign::blank(
        JobRef {
            id: Uuid::new_v4(),
            key: "customer_care_agent".into(),
        },
        DesignOf::Draft,
    );
    d.spaces = SpaceCatalog::new(vec![
        space("retail:order-support:returns", "support:start_return"),
        space("retail:logistics:track_parcel", "delivery:track_parcel"),
        space("retail:order-support:cancel_order", "change:cancel_order"),
    ]);
    d
}

fn gs() -> GateAnswerLite {
    lite(
        STAGE_GATE,
        &[
            ("support", 0.55),
            ("delivery", 0.3),
            ("change", 0.1),
            ("small_talk", 0.05),
        ],
    )
}

#[test]
fn route_hint_restricts_ga_to_top_two_gs_stages() {
    // GA's top (change:cancel_order) is outside the top-2 stages: the hint is the best within support / delivery
    let ga = lite(
        ACTIVITY_GATE,
        &[
            ("change:cancel_order", 0.35),
            ("delivery:track_parcel", 0.1),
            ("support:start_return", 0.52),
        ],
    );
    let hint = QualifierHint.hint(&verdict(vec![gs(), ga]), &design());
    assert_eq!(
        hint,
        Some(RouteHint {
            request_type: "retail:order-support:returns".into(),
            stage: Some("support".into()),
            activity: Some("support:start_return".into()),
            confidence: 0.52,
        })
    );
    let ga = lite(
        ACTIVITY_GATE,
        &[("change:cancel_order", 0.9), ("support:start_return", 0.05)],
    );
    assert_eq!(
        QualifierHint.hint(&verdict(vec![gs(), ga]), &design()),
        None,
        "nothing in the top-2 stages reaches the line"
    );
}

#[test]
fn no_hint_without_gs_and_ga_answers() {
    let ga = lite(ACTIVITY_GATE, &[("support:start_return", 0.9)]);
    assert_eq!(QualifierHint.hint(&verdict(vec![ga]), &design()), None);
    assert_eq!(QualifierHint.hint(&verdict(vec![gs()]), &design()), None);
    assert_eq!(QualifierHint.hint(&GateVerdict::allow(), &design()), None);
}

#[test]
fn no_hint_below_min_confidence() {
    let ga = lite(
        ACTIVITY_GATE,
        &[
            ("support:start_return", 0.49),
            ("delivery:track_parcel", 0.3),
        ],
    );
    assert_eq!(
        QualifierHint.hint(&verdict(vec![gs(), ga]), &design()),
        None
    );
    let ga = lite(ACTIVITY_GATE, &[("support:start_return", 0.5)]);
    assert!(
        QualifierHint
            .hint(&verdict(vec![gs(), ga]), &design())
            .is_some()
    );
}

#[test]
fn hint_carries_request_type_of_the_activity_context_space() {
    let ga = lite(ACTIVITY_GATE, &[("delivery:track_parcel", 0.8)]);
    let hint = QualifierHint
        .hint(&verdict(vec![gs(), ga]), &design())
        .expect("a hint");
    assert_eq!(hint.request_type, "retail:logistics:track_parcel");
    assert_eq!(hint.activity.as_deref(), Some("delivery:track_parcel"));
    // an activity no context space serves gives no hint
    let ga = lite(ACTIVITY_GATE, &[("support:complain", 0.8)]);
    assert_eq!(
        QualifierHint.hint(&verdict(vec![gs(), ga]), &design()),
        None
    );
}
