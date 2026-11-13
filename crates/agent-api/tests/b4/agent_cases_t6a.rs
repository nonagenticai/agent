//! B4 T6a: what `case.create{handover|complaint}` leaves on the handover item for the person who takes it —
//! `priority` (the step arg), `agent_did` (built from the case's `operation_call` ledger, never from input),
//! `collected` (slots + identity, no secrets) and `suggested_next` (a template per reason). Real Postgres 18.
use std::collections::BTreeMap;

use chrono::Utc;
use serde_json::{Map, Value, json};

use crate::b4::hours::{case_in, job};
use crate::common::conv::{self, Ctx};
use agent_api::conversations::store;
use agent_api::runtime::domain::call_id;
use agent_api::runtime::runner::agent_cases;

fn input(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

/// A finished ledger row of `case_id` (owner pool): `state` / `code` as the runner records them.
async fn ledger(c: &Ctx, case_id: &str, step: &str, op: &str, state: &str, code: Option<&str>) {
    let finished = state != "pending";
    sqlx::query(
        "INSERT INTO operation_call (tenant_id, job_id, call_id, case_id, operation, route, env, state, \
                                     outcome_code, finished_at, initiator) \
         VALUES ($1, $2, $3, $4, $5, 'builtin', 'prod', $6, $7, \
                 CASE WHEN $8 THEN now() END, 'procedure@t')",
    )
    .bind(c.t)
    .bind(c.job)
    .bind(call_id(case_id, 0, step, 1))
    .bind(case_id)
    .bind(op)
    .bind(state)
    .bind(code)
    .bind(finished)
    .execute(&c.e.owner)
    .await
    .unwrap();
}

/// Hand `case_id` over through the runner's `agent_cases` (agent_app pool), committed.
async fn hand_over(
    c: &Ctx,
    case_id: &str,
    conv_id: &str,
    d: &agent_api::design::CaseDesign,
    inp: Map<String, Value>,
    collected: &[Value],
) {
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    let o = agent_cases::create_at_collected(
        &mut tx,
        &job(c),
        case_id,
        conv_id,
        "handover",
        &inp,
        d,
        Utc::now(),
        collected,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        o.state,
        agent_api::runtime::runner::ops::OpState::Ok,
        "{o:?}"
    );
}

async fn item(c: &Ctx, case_id: &str) -> (String, Value, Value, String, Option<Value>) {
    sqlx::query_as(
        "SELECT priority, agent_did, collected, COALESCE(suggested_next, ''), proposal_hint \
           FROM agent_handover_items WHERE tenant_id = $1 AND case_id = $2",
    )
    .bind(c.t)
    .bind(case_id)
    .fetch_one(&c.e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn agent_did_comes_from_the_ledger_not_from_input() {
    let c = conv::setup().await;
    let (case_id, d) = case_in(&c, "conv-t6a-1", "prod").await;
    ledger(&c, &case_id, "a", "order.read", "ok", None).await;
    ledger(
        &c,
        &case_id,
        "b",
        "delivery.read",
        "error",
        Some("not_found"),
    )
    .await;
    ledger(
        &c,
        &case_id,
        "c",
        "payment.refund",
        "denied",
        Some("case_cap_reached"),
    )
    .await;
    ledger(&c, &case_id, "d", "carrier.track", "error", Some("timeout")).await;
    ledger(&c, &case_id, "e", "order.cancel", "pending", None).await;
    ledger(&c, &case_id, "f", "case.create{handover}", "ok", None).await;
    // model text in the input never reaches agent_did
    let inp = input(
        json!({"reason": "tool_failed", "agent_did": [{"op": "fake.op", "outcome": "ok", "detail": "x"}]}),
    );
    hand_over(&c, &case_id, "conv-t6a-1", &d, inp, &[]).await;
    let (_, did, _, _, _) = item(&c, &case_id).await;
    let steps: Vec<(String, String, String)> = did
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            let g = |k: &str| s[k].as_str().unwrap().to_owned();
            (g("op"), g("outcome"), g("detail"))
        })
        .collect();
    let want = |a: &str, b: &str, d: &str| (a.to_owned(), b.to_owned(), d.to_owned());
    // ledger order is call time; the five finished rows share a statement time, so compare as a set
    let mut got = steps.clone();
    got.sort();
    let mut exp = vec![
        want("order.read", "ok", "ok"),
        want("delivery.read", "not_found", "not found"),
        want("payment.refund", "blocked", "case cap reached"),
        want("carrier.track", "failed", "timeout"),
    ];
    exp.sort();
    assert_eq!(
        got, exp,
        "pending and case.create rows are left out: {steps:?}"
    );
}

#[tokio::test]
async fn suggested_next_is_the_reason_template_and_proposal_hint_is_null() {
    let c = conv::setup().await;
    let (case_id, d) = case_in(&c, "conv-t6a-2", "prod").await;
    hand_over(
        &c,
        &case_id,
        "conv-t6a-2",
        &d,
        input(json!({"reason": "unknown_outcome"})),
        &[],
    )
    .await;
    let (_, did, collected, next, hint) = item(&c, &case_id).await;
    assert_eq!(next, agent_cases::suggested_next("unknown_outcome"));
    assert_ne!(next, agent_cases::suggested_next("customer_asked"));
    assert_eq!(did, json!([]));
    assert_eq!(
        collected,
        json!([]),
        "nothing collected leaves the column's default"
    );
    assert!(hint.is_none());
}

#[tokio::test]
async fn priority_defaults_to_normal_and_keeps_the_four_levels() {
    let c = conv::setup().await;
    for (n, given, want) in [
        (1, None, "normal"),
        (2, Some("high"), "high"),
        (3, Some("URGENT"), "urgent"),
        (4, Some(" low "), "low"),
        (5, Some("critical"), "normal"),
    ] {
        let conv_id = format!("conv-t6a-p{n}");
        let (case_id, d) = case_in(&c, &conv_id, "prod").await;
        let inp = match given {
            Some(p) => input(json!({"priority": p})),
            None => Map::new(),
        };
        hand_over(&c, &case_id, &conv_id, &d, inp, &[]).await;
        let (p, ..) = item(&c, &case_id).await;
        assert_eq!(p, want, "priority {given:?}");
    }
    // anything else is normal
    for bad in ["critical", "", "  "] {
        assert_eq!(
            agent_cases::priority_of(&input(json!({"priority": bad}))),
            "normal"
        );
    }
    assert_eq!(
        agent_cases::priority_of(&input(json!({"priority": 3}))),
        "normal"
    );
}

/// Needs the migration that turns `agent_handover_items.collected` into an array column (the console reads an array).
#[tokio::test]
async fn collected_lands_in_the_item() {
    let c = conv::setup().await;
    let (case_id, d) = case_in(&c, "conv-t6a-3", "prod").await;
    let rows = agent_cases::collected_of(
        &BTreeMap::from([("order_number".to_owned(), "A-1".to_owned())]),
        &BTreeMap::new(),
        2,
        Some("Ada"),
    );
    hand_over(&c, &case_id, "conv-t6a-3", &d, Map::new(), &rows).await;
    let (_, _, collected, ..) = item(&c, &case_id).await;
    assert_eq!(collected, Value::Array(rows));
}

#[test]
fn collected_of_has_no_secrets_and_resolved_wins() {
    let slots = BTreeMap::from([
        ("order_number".to_owned(), "A-1".to_owned()),
        ("password".to_owned(), "hunter2".to_owned()),
        ("card_number".to_owned(), "4111".to_owned()),
        ("shipping_note".to_owned(), "leave at door".to_owned()),
        ("empty".to_owned(), "  ".to_owned()),
    ]);
    let resolved = BTreeMap::from([("order_number".to_owned(), "A-2".to_owned())]);
    let rows = agent_cases::collected_of(&slots, &resolved, 0, Some("Ada"));
    assert_eq!(
        rows,
        vec![
            json!({"label": "Order number", "value": "A-2", "verified": true}),
            json!({"label": "Shipping note", "value": "leave at door", "verified": false}),
        ],
        "identity level 0 adds no identity rows"
    );
    let rows = agent_cases::collected_of(&BTreeMap::new(), &BTreeMap::new(), 2, Some("Ada"));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["value"], "Ada");
}

fn facts(activity: Option<&str>, hidden: bool, empty: bool) -> agent_cases::HintFacts {
    agent_cases::HintFacts {
        activity: activity.map(str::to_owned),
        activity_hidden: hidden,
        knowledge_empty: empty,
    }
}

#[test]
fn proposal_hint_follows_the_plans_order() {
    let kind =
        |f| agent_cases::proposal_hint_of("j", &f).map(|v| v["kind"].as_str().unwrap().to_owned());
    assert_eq!(
        kind(facts(Some("returns:start"), true, true)).as_deref(),
        Some("enable_activity")
    );
    assert_eq!(
        kind(facts(None, false, true)).as_deref(),
        Some("tenant_extension")
    );
    assert_eq!(
        kind(facts(Some(" "), false, false)).as_deref(),
        Some("tenant_extension")
    );
    assert_eq!(
        kind(facts(Some("returns:start"), false, true)).as_deref(),
        Some("knowledge_gap")
    );
    assert_eq!(kind(facts(Some("returns:start"), false, false)), None);
    let h = agent_cases::proposal_hint_of("j", &facts(Some("a:b"), true, false)).unwrap();
    assert_eq!(h["activity"], "a:b");
    assert_eq!(h["href"], "/admin/jobs/j/journey");
}

#[test]
fn hidden_activity_is_a_tombstone_overlay_place() {
    use agent_api::design::DesignRow;
    let row = |obj: &str, key: &str, kind: &str| DesignRow {
        key: format!("{obj}/{key}"),
        kind: "overlay".into(),
        r#ref: input(json!({"object": obj, "key": key})),
        snap: Some(json!({"kind": kind, "item": null})),
    };
    let rows = vec![
        row("activity", "a:hidden", "tombstone"),
        row("activity", "a:ext", "extension"),
        row("instruction", "a:x", "tombstone"),
    ];
    assert!(agent_cases::activity_hidden_in(&rows, "a:hidden"));
    assert!(!agent_cases::activity_hidden_in(&rows, "a:ext"));
    assert!(
        !agent_cases::activity_hidden_in(&rows, "a:x"),
        "another object's tombstone"
    );
}

#[test]
fn knowledge_empty_reads_grounded_and_passages() {
    let r = |v: Value| input(v);
    assert!(agent_cases::knowledge_empty_in(&r(
        json!({"k": {"grounded": false}})
    )));
    assert!(agent_cases::knowledge_empty_in(&r(
        json!({"k": {"passages": []}})
    )));
    assert!(!agent_cases::knowledge_empty_in(&r(
        json!({"k": {"grounded": false}, "k2": {"grounded": true}})
    )));
    assert!(!agent_cases::knowledge_empty_in(&r(
        json!({"k": {"passages": [{"t": 1}]}})
    )));
    assert!(
        !agent_cases::knowledge_empty_in(&r(json!({"o": {"ok": true}}))),
        "no knowledge step, nothing looked for"
    );
}

#[tokio::test]
async fn proposal_hint_lands_in_the_item_and_is_null_without_one() {
    let c = conv::setup().await;
    let (case_id, d) = case_in(&c, "conv-t6a-h1", "prod").await;
    let hint = agent_cases::proposal_hint_of("j", &facts(Some("a:b"), false, true));
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    agent_cases::create_at_hinted(
        &mut tx,
        &job(&c),
        &case_id,
        "conv-t6a-h1",
        "handover",
        &Map::new(),
        &d,
        Utc::now(),
        &[],
        hint.clone(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let (.., stored) = item(&c, &case_id).await;
    assert_eq!(stored, hint);
    let (case2, d2) = case_in(&c, "conv-t6a-h2", "prod").await;
    hand_over(&c, &case2, "conv-t6a-h2", &d2, Map::new(), &[]).await;
    assert!(item(&c, &case2).await.4.is_none());
}
