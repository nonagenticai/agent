//! B4 retail procedure library on the test template (`support::template`): customers are known, never verified by
//! factor (owner ruling 2026-10-26; the #88 factor flow is retired), and a known customer's refund question never
//! hands over because the connector lacks the preload-only `return.list`.
//!
//! The seeded v5 "rows never half run" baseline is gone with the seed: the all-or-nothing step decoding it pinned is
//! `spaces::base_steps`' own contract (a row with one undecodable step reaches the planner as a count only).
//! Real Postgres 18 + the B4 test job.
use std::sync::Arc;

use agent_api::design::DesignOf;
use agent_api::job_design::grant_covers;
use agent_api::job_design::runtime::JobDesign;
use agent_api::job_design::toolset::DesignToolsets;
use agent_api::runtime::design::RuntimeDesign;
use serde_json::json;

use crate::common::design_contract::ContractJob;
use crate::common::{self, TestEnv};

/// The procedures of the B4 test template whose steps are bodies (`name@version`), on the retail spaces.
const LIBRARY: [&str; 4] = [
    "delivery_problem@1",
    "order_status@2",
    "refund_status@2",
    "return_request@3",
];

struct Fx {
    e: TestEnv,
    f: ContractJob,
}

async fn setup_latest() -> Fx {
    let e = common::env().await;
    let f = crate::b4::support::new_job(&e).await;
    Fx { e, f }
}

/// The Draft design with its job design read (the toolsets), as the runner's design cache holds it.
async fn draft(x: &Fx) -> RuntimeDesign {
    let jr = x.f.job_ref();
    let of = DesignOf::Draft;
    let mut tx = x.f.job.begin(&x.e.state.db).await.unwrap();
    let mut d = RuntimeDesign::load(&mut tx, &x.e.state.design, &jr, &of)
        .await
        .unwrap_or_else(|e| panic!("load: {e:?}"));
    let jd = JobDesign::load(
        &mut tx,
        &x.e.state.design,
        x.e.state.compiler.as_ref(),
        &jr,
        &of,
    )
    .await
    .unwrap_or_else(|e| panic!("job design: {e:?}"));
    tx.commit().await.unwrap();
    d.job_design = Arc::new(jd);
    d
}

/// Owner ruling 2026-10-26: customers are known, never verified by factor (the factor flow is gone). So the library
/// never asks a customer to be identified:
/// - none of the template's procedures has a `credential.verify` or `party.list` step;
/// - every step of every library procedure is inside the grant of the activity its space maps to (what the planner
///   checks), so none of them needs the verify grant.
#[tokio::test]
async fn no_seeded_procedure_asks_a_customer_to_be_identified() {
    let x = setup_latest().await;
    let d = draft(&x).await;
    // the template's procedure rows: by name@version, on the retail spaces
    let seeded: Vec<_> = d
        .procedures
        .iter()
        .filter(|p| {
            !p.tenant
                && LIBRARY.contains(&format!("{}@{}", p.name, p.version).as_str())
                && p.context_space
                    .as_deref()
                    .is_some_and(|c| c.starts_with("retail:") || c.starts_with("customer_service:"))
        })
        .collect();
    assert_eq!(
        seeded.len(),
        LIBRARY.len(),
        "the template carries its procedures"
    );
    assert!(seeded.iter().all(|p| !p.step_defs.is_empty()));
    for p in &seeded {
        for s in &p.step_defs {
            assert!(
                !["credential.verify", "party.list"].contains(&s.operation.as_str()),
                "{}@{} step {} asks a customer to be identified or verified",
                p.name,
                p.version,
                s.name
            );
        }
        assert!(
            !p.tool_requirements
                .iter()
                .any(|t| t.starts_with("credential.verify") || t.starts_with("party.list")),
            "{}@{}: {:?}",
            p.name,
            p.version,
            p.tool_requirements
        );
    }

    // every library procedure stays inside the grant of its space's activity
    for p in &seeded {
        let Some(space) = p.context_space.as_deref() else {
            continue;
        };
        let activity = d
            .spaces
            .by_key(space)
            .map(|sp| sp.activity.clone())
            .unwrap_or_default();
        for step in &p.step_defs {
            let key = match step.variant.as_deref().filter(|v| !v.is_empty()) {
                Some(v) => format!("{}{{{v}}}", step.operation),
                None => step.operation.clone(),
            };
            let grant = DesignToolsets.activity_grant(&d, &activity);
            assert!(
                grant.iter().any(|g| grant_covers(g, &key)),
                "{}@{} step {} ({key}) is outside the grant of {activity:?}: {grant:?}",
                p.name,
                p.version,
                step.name
            );
        }
    }
}

/// Every operation is offered except `return.list`: a connector that predates the new op, as the route table of a tenant pinned to it resolves.
struct NoReturnList;

impl agent_api::runtime::routes::RouteResolver for NoReturnList {
    fn resolve<'a>(
        &'a self,
        _c: &'a mut sqlx::PgConnection,
        q: &'a agent_api::runtime::routes::RouteQuery,
    ) -> futures_util::future::BoxFuture<
        'a,
        agent_api::error::ApiResult<agent_api::runtime::routes::ResolvedTarget>,
    > {
        use agent_api::runtime::routes::ResolvedTarget;
        let offered = !q.op.starts_with("return.list");
        Box::pin(async move {
            Ok(if offered {
                ResolvedTarget::Instance {
                    instance_id: "no-return-list".to_owned(),
                }
            } else {
                ResolvedTarget::NotOffered
            })
        })
    }
}

/// Guards the first v13 mistake: `return.list` in the library procedures' `tool_requirements`. The orchestrator hands
/// a case over (`not_offered`) before its first step when a REQUIRED operation routes `not_offered`
/// (`src/runtime/orchestrator.rs`, T6a checks), so every known customer of a retail job whose connector has no
/// `return.list` lost the refund answer to a colleague. The op is read only by the
/// customer-context preload, where a refused read means "no returns".
///
/// A known customer's "where is my refund" (`retail:payments:refunds`, library `refund_status`, newest template) on a
/// route table without `return.list`: the context read of `return.list` is refused `not_offered`, the procedure's own
/// steps all run, and the case ends resolved / not_eligible with the payment wording: no handover item, no id, no
/// "We have your return". Mutation: `return.list` in `refund_status`'s `tool_requirements` hands the case over with
/// code `not_offered` before the first call.
#[tokio::test]
async fn refund_without_return_list_connector_never_hands_over() {
    use std::collections::HashSet;

    use agent_api::runtime::Env;
    use agent_api::runtime::domain::{CaseOutcome, OpState, QualifiedRequest};
    use agent_api::runtime::orchestrator::DesignProcedures;
    use agent_api::runtime::reply;
    use agent_api::runtime::resolve::contains_id_pattern;

    use crate::b4::flow as fl;

    let fx = fl::setup_latest("lib-refund-no-return-list", Env::Test).await;
    fl::insert_conversation(&fx, "conv-CS-1").await;
    fl::insert_case(&fx, "CS-1", &fl::current_design(&fx).await).await;
    // a known customer: the party is on the conversation's state before the first message is planned
    fl::identify_conversation(&fx, "conv-CS-1", "C-10482", None).await;
    let req: QualifiedRequest = serde_json::from_value(json!({
        "case_id": "CS-1", "conversation_id": "conv-CS-1", "area": "payments",
        "context_space": "retail:payments:refunds", "request_type": "refunds", "slots": {"order_id": "ORD-58790"},
        "customer_id": "C-10482", "identity_level": 2, "message_id": "msg-CS-1"
    }))
    .unwrap();
    let o = fl::orch(&fx, DesignProcedures).with_routes(Arc::new(NoReturnList));
    o.on_qualified(fl::qualified(&req)).await.unwrap();

    // the fake runner: answer every call once, until the case resolves
    let mut done: HashSet<String> = HashSet::new();
    for _ in 0..30 {
        if !fl::resolutions_in(&fx, "payments", "CS-1").await.is_empty() {
            break;
        }
        let todo: Vec<_> = fl::calls(&fx, "CS-1")
            .await
            .into_iter()
            .filter(|c| !done.contains(&c.call_id))
            .collect();
        assert!(
            !todo.is_empty(),
            "the case neither resolved nor has a call out"
        );
        for c in todo {
            done.insert(c.call_id.clone());
            let (state, code, out) = match c.operation.as_str() {
                "return.list" => (OpState::Denied, "not_offered", json!({})),
                "order.list" => (
                    OpState::Ok,
                    "ok",
                    json!({"order_id": ["ORD-58790"], "status": ["delivered"], "placed_at": ["2026-10-20"]}),
                ),
                "order.read" => (
                    OpState::Ok,
                    "ok",
                    json!({"status": "delivered", "customer_id": "C-10482", "total": 59.0,
                           "lines": [{"name": "Jute rug", "qty": 1}]}),
                ),
                "payment.list" => (
                    OpState::Ok,
                    "ok",
                    json!({"payment_id": "PAY-1", "order_id": "ORD-58790", "status": "captured"}),
                ),
                "rule.evaluate" => (OpState::Ok, "ok", json!({"outcome": "eligible"})),
                _ => (OpState::Ok, "ok", json!({})),
            };
            o.on_result(fl::answer(&c, state, code, out)).await.unwrap();
        }
    }

    let rs = fl::resolutions_in(&fx, "payments", "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert!(
        matches!(
            rs[0].outcome,
            CaseOutcome::Resolved | CaseOutcome::NotEligible
        ),
        "never a handover: {rs:?}"
    );
    assert_ne!(rs[0].code.as_deref(), Some("not_offered"), "{rs:?}");
    assert!(fl::handover_items(&fx, "CS-1").await.is_empty());
    assert_ne!(fl::case_row(&fx, "CS-1").await.0, "handover");
    // return.list only ever ran as the context read, never as a plan step
    for c in fl::calls(&fx, "CS-1").await {
        if c.operation == "return.list" {
            assert_eq!(c.provenance.as_deref(), Some("context"), "{c:?}");
        }
    }
    let text = reply::compose(&rs[0]);
    assert!(!text.contains("return for"), "{text}");
    assert!(!text.contains("We have your return"), "{text}");
    assert!(!contains_id_pattern(&text), "{text}");
}
