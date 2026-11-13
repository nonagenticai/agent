//! B4 known customer, Task 9 "resolution + ambiguity + replies" (plan §2.3; owner ruling 2026-10-26): an identified
//! customer's case carries no order id; the orchestrator fills the system slots from the loaded customer context, asks
//! in plain words only when several orders fit, and says a plain sentence when none does. The pure resolver is
//! table-tested in `runtime/resolve.rs`; this drives the orchestrator over real Postgres 18 + Redis with a fake
//! runner that answers the context reads. Nothing here types an id as the customer.
use std::collections::HashSet;

use agent_api::runtime::domain::{CaseOutcome, OpCall, OpState as ResultState};
use agent_api::runtime::orchestrator::{Orchestrator, StaticProcedures};
use agent_api::runtime::reply;
use agent_api::runtime::resolve::{Choice, contains_id_pattern};
use serde_json::{Value, json};

use crate::b4::flow::{
    Fx, answer, calls, current_design, insert_case, orch, qualified, request, resolutions,
};
use crate::b4::runner_checks::setup;

fn is_ctx(c: &OpCall) -> bool {
    c.provenance.as_deref() == Some("context")
}

async fn identify(fx: &Fx, case: &str) {
    crate::b4::flow::identify_conversation(fx, &format!("conv-{case}"), "cust-1", None).await;
}

/// Answer every context read of `case` from `world` (operation key -> answer); every call of the case afterwards.
async fn load(
    fx: &Fx,
    o: &Orchestrator<StaticProcedures>,
    case: &str,
    world: &Value,
) -> Vec<OpCall> {
    let mut done: HashSet<String> = HashSet::new();
    loop {
        let cs = calls(fx, case).await;
        let todo: Vec<OpCall> = cs
            .iter()
            .filter(|c| is_ctx(c) && !done.contains(&c.call_id))
            .cloned()
            .collect();
        if todo.is_empty() {
            return cs;
        }
        for c in todo {
            done.insert(c.call_id.clone());
            let out = world
                .get(c.operation.as_str())
                .cloned()
                .unwrap_or_else(|| json!({}));
            o.on_result(answer(&c, ResultState::Ok, "ok", out))
                .await
                .unwrap();
        }
    }
}

/// A customer with a delivered Jute rug (12 Oct) and a delivered Ceramic lamp (20 Oct): a return could be either.
fn two_orders() -> Value {
    json!({
        "order.list": {
            "order_id": ["A-2", "A-1"], "status": ["delivered", "delivered"],
            "placed_at": ["2026-10-20T09:00:00Z", "2026-10-12T09:00:00Z"],
            "items": [[{"name": "Ceramic lamp", "qty": 1}], [{"name": "Jute rug", "qty": 1}]],
        },
        "payment.list": {"payment_id": ["P-2", "P-1"], "order_id": ["A-2", "A-1"], "status": ["captured", "captured"]},
        "delivery.list": {"delivery_id": ["S-2", "S-1"], "order_id": ["A-2", "A-1"], "status": ["delivered", "delivered"]},
    })
}

/// A case of an identified customer whose request carries only what the qualifier kept: no order id.
async fn start_case(
    fx: &Fx,
    o: &Orchestrator<StaticProcedures>,
    slots: Value,
    world: &Value,
) -> Vec<OpCall> {
    insert_case(fx, "CS-1", &current_design(fx).await).await;
    identify(fx, "CS-1").await;
    let mut req = request("CS-1");
    req.slots = serde_json::from_value(slots).unwrap();
    o.on_qualified(qualified(&req)).await.unwrap();
    load(fx, o, "CS-1", world).await
}

fn plan_call(all: &[OpCall]) -> Option<&OpCall> {
    all.iter().find(|c| !is_ctx(c))
}

/// "The lamp" names one order: it is filled silently, the plan's first step reads it, nobody is asked, the reply
/// label is a fact.
/// Mutations: the resolver skipped (the step reads `null`); the hint ignored (the case parks); the label not stored.
#[tokio::test]
async fn a_product_the_customer_names_resolves_silently() {
    let fx = setup("res-hint").await;
    let o = orch(&fx, StaticProcedures);
    let all = start_case(
        &fx,
        &o,
        json!({"reason": "too big", "item_hint": "the lamp"}),
        &two_orders(),
    )
    .await;
    let step = plan_call(&all).expect("the plan runs after the context");
    assert_eq!(step.operation, "order.read");
    assert_eq!(step.input["order_id"], "A-2");
    assert!(
        resolutions(&fx, "CS-1").await.is_empty(),
        "nothing was asked or answered yet"
    );
    let facts = o.cases().results("CS-1").await.unwrap();
    assert_eq!(
        facts["resolved"]["item_label"],
        "Ceramic lamp ordered on 20 October"
    );
}

/// Both orders fit a return: the case parks with a question in plain words (product and date), the candidates stay
/// on the server, and no plan step runs. The question and the stored choice carry no id.
/// Mutations: the first candidate taken; the question printing ids; no choice stored (the answer cannot resolve).
#[tokio::test]
async fn several_fitting_orders_are_a_question_in_plain_words_never_ids() {
    let fx = setup("res-ask").await;
    let o = orch(&fx, StaticProcedures);
    let all = start_case(&fx, &o, json!({"reason": "too big"}), &two_orders()).await;
    assert!(
        plan_call(&all).is_none(),
        "no step runs while the customer is asked"
    );
    let rs = resolutions(&fx, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::ConfirmationRequired);
    assert_eq!(rs[0].code.as_deref(), Some("choice_required"));
    let text = reply::compose(&rs[0]);
    assert_eq!(
        text,
        "Is this about your Jute rug from 12 October or your Ceramic lamp from 20 October?"
    );
    assert!(!contains_id_pattern(&text));
    let key = agent_api::turn::chat_key(&fx.bus, "conv-CS-1", "");
    let raw: Option<String> = redis::cmd("HGET")
        .arg(&key)
        .arg("choice")
        .query_async(&mut fx.bus.conn())
        .await
        .unwrap();
    let choice: Choice = serde_json::from_str(&raw.expect("the parked choice")).unwrap();
    assert_eq!(choice.case_id, "CS-1");
    assert_eq!(
        choice
            .picks
            .iter()
            .map(|p| p.order_id.as_str())
            .collect::<Vec<_>>(),
        ["A-1", "A-2"]
    );
    assert_eq!(choice.needs, ["order_id"]);
    assert_eq!(
        choice.slots.get("reason").map(String::as_str),
        Some("too big")
    );
}

/// No order fits (a product the customer never bought; an account with no orders): a plain sentence, no key asked,
/// no step run.
/// Mutations: the case falling through to the plan with a null order; the sentence asking for a number.
#[tokio::test]
async fn no_matching_order_is_a_plain_sentence_not_a_request_for_a_number() {
    let fx = setup("res-none").await;
    let o = orch(&fx, StaticProcedures);
    let all = start_case(
        &fx,
        &o,
        json!({"reason": "too big", "item_hint": "the kettle"}),
        &two_orders(),
    )
    .await;
    assert!(plan_call(&all).is_none());
    let rs = resolutions(&fx, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::NotEligible);
    let text = reply::compose(&rs[0]);
    assert_eq!(text, reply::NO_MATCHING_ORDER);
    let low = text.to_lowercase();
    assert!(
        !low.contains("number") && !low.contains("order id"),
        "{text}"
    );

    // an account with no orders at all
    let fx = setup("res-empty").await;
    let o = orch(&fx, StaticProcedures);
    let all = start_case(&fx, &o, json!({"reason": "too big"}), &json!({})).await;
    assert!(plan_call(&all).is_none());
    let rs = resolutions(&fx, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(reply::compose(&rs[0]), reply::NO_MATCHING_ORDER);
}

/// One order fits a return (the other is still on its way): resolved without a hint and without a question.
/// Mutation: ranking ignored (two candidates -> a question).
#[tokio::test]
async fn the_space_ranking_picks_the_one_order_that_fits_without_asking() {
    let fx = setup("res-rank").await;
    let o = orch(&fx, StaticProcedures);
    let world = json!({
        "order.list": {
            "order_id": ["RET-02", "RET-01"], "status": ["in_transit", "delivered"],
            "placed_at": ["2026-10-16T18:02:00Z", "2026-10-02T09:14:00Z"],
            "items": [[{"name": "Stoneware mug set (4)", "qty": 1}],
                      [{"name": "Linen throw", "qty": 1}, {"name": "Wool rug 160x230", "qty": 1}]],
        },
        "delivery.list": {"delivery_id": ["SHP-9102", "SHP-9101"], "order_id": ["RET-02", "RET-01"],
                          "status": ["in_transit", "delivered"]},
    });
    let all = start_case(&fx, &o, json!({"reason": "too big"}), &world).await;
    let step = plan_call(&all).expect("the plan runs");
    assert_eq!(step.input["order_id"], "RET-01");
    assert!(resolutions(&fx, "CS-1").await.is_empty());
}
