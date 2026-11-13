//! A staff test chat on a customer job types the test customer's EMAIL, but the job's system of record keys its
//! customers by its own party id (`C-1`). I KNOW MY CUSTOMER: the context phase resolves the typed email with
//! `party.list{email}` before it reads any record, binds the one party it finds to the case, and when it finds none
//! (or several, or the job cannot look up) finishes degraded, reads nothing and never asks the customer for an id.
//!
//! The orchestrator drives the REAL runner over fake systems of record: the lookup is a `context` call admitted only
//! for a `staff_test` case's own email, and its answer (a party id that is not the typed email) is not an ownership
//! miss.
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_api::conversations::store;
use agent_api::runtime::customer_context::{self as cc, CustomerContext};
use agent_api::runtime::domain::{OpCall, OpState as ResultState};
use agent_api::runtime::orchestrator::{Orchestrator, StaticProcedures};
use agent_api::runtime::routes::ResolvedTarget;
use agent_api::runtime::runner::Runner;
use agent_api::runtime::runner::ops::{
    Executors, OpExecutor, OpOutcome, OpRequest, OpState, outcome,
};
use agent_api::runtime::{Envelope, design::DesignCache};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use crate::b4::flow::{self as fl, Fx, current_design, qualified};
use crate::b4::runner_checks::{ctx_call, setup};
use crate::redis_env as r;

const ANN: &str = "ann@example.org";
const NOBODY: &str = "nobody@example.org";
const TWIN: &str = "twin@example.org";

/// `party.list{email}`: Ann is C-1, the twin address belongs to two customers, anyone else to nobody.
struct Parties {
    hits: AtomicUsize,
}

impl OpExecutor for Parties {
    fn execute<'a>(
        &'a self,
        req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move {
            self.hits.fetch_add(1, Ordering::SeqCst);
            let ids = match req.inputs.get("email").and_then(Value::as_str) {
                Some(ANN) => json!(["C-1"]),
                Some(TWIN) => json!(["C-7", "C-8"]),
                _ => json!([]),
            };
            let roles: Vec<Value> = ids
                .as_array()
                .unwrap()
                .iter()
                .map(|_| json!("customer"))
                .collect();
            outcome(
                OpState::Ok,
                None,
                None,
                json!({"party_id": ids, "roles": roles})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
        })
    }
}

/// A system of record that keys orders by its own party id: the orders of `C-1`, nothing for anyone else. Counts
/// every call (the typed email must never reach it as a party id).
struct Orders {
    hits: AtomicUsize,
    seen: std::sync::Mutex<Vec<String>>,
}

impl OpExecutor for Orders {
    fn execute<'a>(
        &'a self,
        req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move {
            self.hits.fetch_add(1, Ordering::SeqCst);
            let party = req
                .inputs
                .get("party_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            self.seen.lock().unwrap().push(party.clone());
            let out = if party == "C-1" {
                json!({"order_id": ["O-1"], "status": ["shipped"], "placed_at": ["2026-10-20"],
                       "items": [[{"name": "Rug", "qty": 1}]]})
            } else {
                json!({"order_id": [], "status": []})
            };
            outcome(OpState::Ok, None, None, out.as_object().cloned().unwrap())
        })
    }
}

/// The job's other offered context reads (payments, deliveries, returns): the system answers, with nothing to list.
/// Unregistered they would fail and show up as `degraded`, which is not what this file tests.
struct Nothing;

impl OpExecutor for Nothing {
    fn execute<'a>(
        &'a self,
        _req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move { outcome(OpState::Ok, None, None, serde_json::Map::new()) })
    }
}

struct World {
    fx: Fx,
    orch: Orchestrator<StaticProcedures>,
    runner: Runner,
    parties: Arc<Parties>,
    orders: Arc<Orders>,
}

async fn world(test: &str) -> World {
    let fx = setup(test).await;
    let parties = Arc::new(Parties {
        hits: AtomicUsize::new(0),
    });
    let orders = Arc::new(Orders {
        hits: AtomicUsize::new(0),
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let mut st = fx.d.e.state.clone();
    st.db = fx.rt.clone();
    let runner = Runner::new(fx.bus.clone(), st, Arc::new(DesignCache::default()))
        .unwrap()
        .with_consumer("staff-test-party")
        .with_consume(r::fast())
        .with_executors(
            Executors::default()
                .register("party.list", parties.clone())
                .register("order.list", orders.clone())
                .register("payment.list", Arc::new(Nothing))
                .register("delivery.list", Arc::new(Nothing))
                .register("return.list", Arc::new(Nothing)),
        );
    let orch = fl::orch(&fx, StaticProcedures);
    World {
        fx,
        orch,
        runner,
        parties,
        orders,
    }
}

/// A staff test conversation `conv` typed as `email`, and its case born through the production insert: the case id.
/// (`routes/chat.rs`: `customer_id` and state `subject` = the typed email, `identity_source` `staff_test`.)
async fn staff_test_case(fx: &Fx, conv: &str, email: &str) -> String {
    let (t, j) = (fx.d.f.tenant, fx.d.f.job.job_id);
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, $3, $4, 'staff', $5)",
    )
    .bind(t)
    .bind(j)
    .bind(conv)
    .bind(fx.scope.env.as_str())
    .bind(email)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    crate::common::state::identify_as(&fx.d.e.owner, t, j, conv, "staff_test", email, Some(email))
        .await;
    crate::common::state::put(&fx.d.e.owner, t, j, conv, "name", email).await;
    let mut tx = store::begin_job(&fx.d.e.state.db, t, j).await.unwrap();
    let c = store::conversation_or_404(&mut tx, conv).await.unwrap();
    let case = store::create_case(
        &mut tx,
        j,
        &c,
        "where is my order",
        &current_design(fx).await,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    case
}

fn is_ctx(c: &OpCall) -> bool {
    c.provenance.as_deref() == Some("context")
}

/// Qualify `case` and run every context call the orchestrator sends through the real runner, feeding the runner's
/// answers back, until none is outstanding. Every call of the case afterwards.
async fn run(w: &World, case: &str, conv: &str) -> Vec<OpCall> {
    let mut req = fl::request(case);
    req.conversation_id = conv.to_owned();
    w.orch.on_qualified(qualified(&req)).await.unwrap();
    let mut done: HashSet<String> = HashSet::new();
    loop {
        let cs = fl::calls(&w.fx, case).await;
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
            let res = w.runner.handle(&c).await.unwrap();
            w.orch
                .on_result(Envelope::new("op.result", "runner", &c.case_id, &res).unwrap())
                .await
                .unwrap();
        }
    }
}

async fn context_of(w: &World, case: &str) -> CustomerContext {
    let raw = w
        .orch
        .cases()
        .customer_context_raw(case)
        .await
        .unwrap()
        .expect("the context phase stored a context");
    serde_json::from_str(&raw).unwrap()
}

/// The case's verified party: the `subject` state of its conversation (the typed email until the lookup binds the
/// system's id).
async fn verified_party(fx: &Fx, case: &str) -> Option<String> {
    crate::common::state::case_identity(&fx.d.e.owner, fx.d.f.tenant, fx.d.f.job.job_id, case)
        .await
        .1
}

/// The typed email maps to C-1: the lookup runs first and alone, the case is bound to C-1, the record reads go out
/// keyed by C-1 (never by the email), C-1's orders are the context, and the plan's first step then goes out - "my
/// order" is known, nothing asks for an id. A second case of the conversation rebinds without a second lookup.
/// Mutation: the reads keyed by the typed email (the old behaviour: an empty context).
#[tokio::test]
async fn a_staff_test_email_resolves_to_the_external_party_and_reads_its_orders() {
    let w = world("stp-resolve").await;
    let (fx, conv) = (&w.fx, "conv-stp-1");
    let case = staff_test_case(fx, conv, ANN).await;
    assert_eq!(verified_party(fx, &case).await.as_deref(), Some(ANN));

    // before any answer, the only call out is the lookup, by the typed email
    let mut req = fl::request(&case);
    req.conversation_id = conv.to_owned();
    w.orch.on_qualified(qualified(&req)).await.unwrap();
    let first = fl::calls(fx, &case).await;
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(
        (first[0].operation.as_str(), first[0].step.as_str()),
        ("party.list", "ctx:party.list")
    );
    assert_eq!(first[0].input["email"], ANN);
    assert!(is_ctx(&first[0]));
    assert_eq!(first[0].input.len(), 1);

    let case2 = {
        // finish the first case through the real runner
        let mut done: HashSet<String> = HashSet::new();
        loop {
            let cs = fl::calls(fx, &case).await;
            let todo: Vec<OpCall> = cs
                .iter()
                .filter(|c| is_ctx(c) && !done.contains(&c.call_id))
                .cloned()
                .collect();
            if todo.is_empty() {
                break;
            }
            for c in todo {
                done.insert(c.call_id.clone());
                let res = w.runner.handle(&c).await.unwrap();
                if c.operation == "party.list" {
                    assert_eq!(res.state, ResultState::Ok, "{res:?}");
                    assert_eq!(
                        res.output.clone().unwrap()["party_id"],
                        json!(["C-1"]),
                        "the system's id is not hidden as another party's"
                    );
                }
                w.orch
                    .on_result(Envelope::new("op.result", "runner", &c.case_id, &res).unwrap())
                    .await
                    .unwrap();
            }
        }
        let all = fl::calls(fx, &case).await;
        // every record read is keyed by the system's id
        for c in all
            .iter()
            .filter(|c| is_ctx(c) && c.operation != "party.list")
        {
            if let Some(p) = c.input.get("party_id") {
                assert_eq!(p, "C-1", "{c:?}");
            }
        }
        assert!(
            all.iter().any(|c| is_ctx(c) && c.operation == "order.list"),
            "the reads went out after the lookup"
        );
        assert!(
            w.orders.seen.lock().unwrap().iter().all(|p| p == "C-1"),
            "the typed email never reached the system as a party id"
        );
        // the plan's first step goes out: the known customer is not asked anything
        let plan = all
            .iter()
            .find(|c| !is_ctx(c))
            .expect("the plan starts after the context");
        assert_eq!(plan.operation, "order.read");
        assert!(
            fl::resolutions(fx, &case).await.is_empty(),
            "no handover, no question"
        );

        let ctx = context_of(&w, &case).await;
        assert_eq!(ctx.party, "C-1");
        assert_eq!(ctx.resolved_from.as_deref(), Some(ANN));
        assert_eq!(ctx.orders.len(), 1);
        assert_eq!(ctx.orders[0].order_id, "O-1");
        assert!(ctx.degraded.is_empty(), "{:?}", ctx.degraded);
        assert_eq!(verified_party(fx, &case).await.as_deref(), Some("C-1"));
        assert_eq!(w.parties.hits.load(Ordering::SeqCst), 1);

        // a later case of the same conversation is born on the system's id (the conversation's `subject` was
        // rebound), reads the snapshot and does NOT look the email up a second time
        let mut tx = store::begin_job(&fx.d.e.state.db, fx.d.f.tenant, fx.d.f.job.job_id)
            .await
            .unwrap();
        let c = store::conversation_or_404(&mut tx, conv).await.unwrap();
        let next = store::create_case(
            &mut tx,
            fx.d.f.job.job_id,
            &c,
            "and my refund",
            &current_design(fx).await,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        next
    };
    assert_eq!(verified_party(fx, &case2).await.as_deref(), Some("C-1"));
    let all2 = run(&w, &case2, conv).await;
    assert!(
        all2.iter().all(|c| c.operation != "party.list"),
        "no second lookup: {all2:?}"
    );
    assert_eq!(w.parties.hits.load(Ordering::SeqCst), 1);
    assert_eq!(verified_party(fx, &case2).await.as_deref(), Some("C-1"));
    assert_eq!(context_of(&w, &case2).await.party, "C-1");
}

/// An email the system does not know: the lookup is the ONLY context call, the context is degraded
/// `party_not_found` with nothing in it, no record is read, the case goes on as an unknown customer (the plan's step
/// goes out; no handover, nothing asks the customer for an id), and the staff route tells the admin in plain words.
/// Mutation: the unmatched email used as the party (reads keyed by the email), or a question for an id.
#[tokio::test]
async fn an_unknown_staff_test_email_is_degraded_party_not_found_and_reads_nothing() {
    let w = world("stp-unknown").await;
    let (fx, conv) = (&w.fx, "conv-stp-2");
    let case = staff_test_case(fx, conv, NOBODY).await;
    let all = run(&w, &case, conv).await;

    let ctx_calls: Vec<&OpCall> = all.iter().filter(|c| is_ctx(c)).collect();
    assert_eq!(ctx_calls.len(), 1, "{ctx_calls:?}");
    assert_eq!(ctx_calls[0].operation, "party.list");
    assert_eq!(w.orders.hits.load(Ordering::SeqCst), 0, "no record read");
    let ctx = context_of(&w, &case).await;
    assert_eq!(ctx.degraded, [cc::PARTY_NOT_FOUND]);
    assert!(ctx.orders.is_empty() && ctx.payments.is_empty() && ctx.records.is_empty());
    assert!(ctx.resolved_from.is_none());
    assert_eq!(
        verified_party(fx, &case).await.as_deref(),
        Some(NOBODY),
        "never bound to a guess"
    );
    // the case proceeds; it is neither handed over nor asking for a lookup key
    assert!(
        all.iter().any(|c| !is_ctx(c)),
        "the plan goes on as an unknown customer"
    );
    assert!(
        fl::resolutions(fx, &case).await.is_empty(),
        "no question, no handover"
    );
    let note = cc::lookup_note(&ctx.degraded).expect("the admin is told");
    assert!(note.contains("No customer with that email"), "{note}");
    for banned in ["order number", "order id", "customer id", "postcode"] {
        assert!(!note.to_lowercase().contains(banned), "{note}");
    }
}

/// Two customers share the typed email: never guessed. Degraded `party_ambiguous`, nothing read, the case unbound.
/// A lookup that fails (no route, system down) is `party_lookup_unavailable`, same handling.
#[tokio::test]
async fn an_ambiguous_or_failed_staff_test_lookup_is_degraded_and_reads_nothing() {
    let w = world("stp-ambiguous").await;
    let (fx, conv) = (&w.fx, "conv-stp-3");
    let case = staff_test_case(fx, conv, TWIN).await;
    let all = run(&w, &case, conv).await;
    assert_eq!(all.iter().filter(|c| is_ctx(c)).count(), 1);
    assert_eq!(w.orders.hits.load(Ordering::SeqCst), 0);
    let ctx = context_of(&w, &case).await;
    assert_eq!(ctx.degraded, [cc::PARTY_AMBIGUOUS]);
    assert_eq!(verified_party(fx, &case).await.as_deref(), Some(TWIN));
    assert!(
        all.iter().any(|c| !is_ctx(c)),
        "the case goes on as an unknown customer"
    );

    // the lookup fails for good: the orchestrator is told so, whatever the runner would have said
    let conv4 = "conv-stp-4";
    let case4 = staff_test_case(fx, conv4, ANN).await;
    let mut req = fl::request(&case4);
    req.conversation_id = conv4.to_owned();
    w.orch.on_qualified(qualified(&req)).await.unwrap();
    let look = fl::calls(fx, &case4).await.remove(0);
    w.orch
        .on_result(fl::answer(
            &look,
            ResultState::Error,
            "not_offered",
            json!({}),
        ))
        .await
        .unwrap();
    let ctx4 = context_of(&w, &case4).await;
    assert_eq!(ctx4.degraded, [cc::PARTY_LOOKUP_UNAVAILABLE]);
    assert!(
        fl::calls(fx, &case4)
            .await
            .iter()
            .all(|c| !is_ctx(c) || c.operation == "party.list")
    );
}

/// The lookup is admitted for a staff test case's own typed email and nothing else: another email, an extra key or a
/// different operation is `not_context_op`, and a web chat's `assertion` case may not look anyone up (its party
/// already is the system's id).
#[tokio::test]
async fn the_party_lookup_is_admitted_only_for_a_staff_test_cases_own_email() {
    let w = world("stp-admit").await;
    let fx = &w.fx;
    let case = staff_test_case(fx, "conv-stp-5", ANN).await;
    let refused = |res: &agent_api::runtime::domain::OpResult| {
        res.state == ResultState::Denied && res.outcome_code.as_deref() == Some("not_context_op")
    };
    for (i, input) in [
        json!({"email": TWIN}),
        json!({"email": ANN, "phone": "1"}),
        json!({"phone": "1"}),
    ]
    .into_iter()
    .enumerate()
    {
        let res = w
            .runner
            .handle(&ctx_call(
                fx,
                &case,
                &format!("ctx:x{i}"),
                "party.list",
                input,
            ))
            .await
            .unwrap();
        assert!(refused(&res), "{res:?}");
    }
    assert_eq!(
        w.parties.hits.load(Ordering::SeqCst),
        0,
        "no executor saw a refused lookup"
    );
    // the call names the staff test conversation (A2: step 3 reads ITS state, the typed email as `subject`)
    let mut own_call = ctx_call(
        fx,
        &case,
        "ctx:party.list",
        "party.list",
        json!({"email": ANN}),
    );
    own_call.conversation_id = "conv-stp-5".to_owned();
    let own = w.runner.handle(&own_call).await.unwrap();
    assert_eq!(own.state, ResultState::Ok, "{own:?}");
    assert_eq!(own.output.unwrap()["party_id"], json!(["C-1"]));

    // a web chat's asserted party is the system's id already: no lookup
    crate::b4::runner_checks::known_case(fx, "CS-9001", "C-1").await;
    let web = w
        .runner
        .handle(&ctx_call(
            fx,
            "CS-9001",
            "ctx:party.list",
            "party.list",
            json!({"email": ANN}),
        ))
        .await
        .unwrap();
    assert!(refused(&web), "{web:?}");
}
