//! B4 known customer, Task 8 "context reads" (plan §2.2): the orchestrator's `context` phase, the loader's fan-out
//! over the runner, and the staff route `GET /api/jobs/{job}/conversations/{id}/customer-context`. The runner's own
//! `context` provenance (allowlist, gate, ownership, cap, audit) is in `runner_checks.rs`. Real Postgres 18 + Redis.
use std::collections::HashSet;
use std::sync::Arc;

use agent_api::runtime::customer_context::{
    self as cc, ContextCaller, CtxStep, CustomerContext, MAX_CONTEXT_CALLS, StepRes,
};
use agent_api::runtime::domain::{OpCall, OpState as ResultState};
use agent_api::runtime::orchestrator::StaticProcedures;
use agent_api::runtime::routes::ResolvedTarget;
use agent_api::runtime::runner::ops::{
    Executors, OpExecutor, OpOutcome, OpRequest, OpState, failed,
};
use axum::http::StatusCode;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::b4::flow::{
    Fx, answer, calls, current_design, insert_case, insert_case_in, orch, qualified, request,
};
use crate::b4::runner_checks::{Fake, ctx_call, known_case, setup};
use crate::common::conv::{self, code};
use crate::redis_env as r;

fn is_ctx(c: &OpCall) -> bool {
    c.provenance.as_deref() == Some("context")
}

/// The fake runner's answer to one context read: eight itemless orders, a payment list that times out, the rest ok.
fn runner_answer(c: &OpCall) -> (ResultState, &'static str, Value) {
    match c.operation.as_str() {
        "order.list" => {
            let ids: Vec<String> = (0..8).map(|i| format!("O{i}")).collect();
            let at: Vec<String> = (0..8).map(|i| format!("2026-10-{:02}", 10 + i)).collect();
            (
                ResultState::Ok,
                "ok",
                json!({"order_id": ids, "status": vec!["paid"; 8], "placed_at": at}),
            )
        }
        "order.read" => (
            ResultState::Ok,
            "ok",
            json!({"status": "paid", "customer_id": "C-1", "lines": [{"name": "Rug", "qty": 1}]}),
        ),
        "payment.list" => (ResultState::Error, "timeout", json!({})),
        _ => (ResultState::Ok, "ok", json!({})),
    }
}

/// The case's conversation (`conv-<case>`, the one `fl::insert_case` names) has the known customer C-1.
async fn identify(fx: &Fx, case: &str) {
    crate::b4::flow::identify_conversation(fx, &format!("conv-{case}"), "C-1", None).await;
}

/// Answer every outstanding context read of `case` until none is left; asserts no plan step was sent while one was
/// still outstanding. Returns every call of the case afterwards.
async fn answer_context(
    fx: &Fx,
    o: &agent_api::runtime::orchestrator::Orchestrator<StaticProcedures>,
    case: &str,
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
            assert!(
                calls(fx, case).await.iter().all(is_ctx),
                "no plan step is sent while a context read is out"
            );
            done.insert(c.call_id.clone());
            let (state, code, out) = runner_answer(&c);
            o.on_result(answer(&c, state, code, out)).await.unwrap();
        }
    }
}

/// An identified case loads its customer's context BEFORE its first step: the lists first, then at most six
/// `order.read` for the newest itemless orders (never more than 11 reads in all), a failed read narrows the context
/// and does not stop the case, the plan's first step goes out only after the last answer, and a second case of the
/// conversation reuses the snapshot until a write invalidates it.
/// Mutations: the phase skipped; the plan started on the first answer; the fan-out uncapped; a failed read failing the
/// case; the snapshot never reused / never invalidated.
#[tokio::test]
async fn the_context_phase_loads_before_the_first_step_and_tolerates_a_failed_read() {
    let fx = setup("ctx-flow").await;
    let o = orch(&fx, StaticProcedures);
    insert_case(&fx, "CS-1", &current_design(&fx).await).await;
    identify(&fx, "CS-1").await;
    let req = request("CS-1");
    o.on_qualified(qualified(&req)).await.unwrap();

    let first = calls(&fx, "CS-1").await;
    assert!(
        !first.is_empty() && first.iter().all(is_ctx),
        "only context reads before the context is loaded: {first:?}"
    );
    let list = first.iter().find(|c| c.operation == "order.list").unwrap();
    assert_eq!(list.input["party_id"], "C-1");
    assert!(
        list.idempotency_key.is_none() && list.confirm_ref.is_none() && list.performed_by.is_none()
    );

    let all = answer_context(&fx, &o, "CS-1").await;
    let ctx_calls: Vec<&OpCall> = all.iter().filter(|c| is_ctx(c)).collect();
    let reads = ctx_calls
        .iter()
        .filter(|c| c.operation == "order.read")
        .count();
    assert_eq!(reads, cc::MAX_ORDER_READS, "the newest six, not eight");
    assert!(
        i64::try_from(ctx_calls.len()).unwrap() <= MAX_CONTEXT_CALLS,
        "{} context reads",
        ctx_calls.len()
    );
    assert!(ctx_calls.iter().all(|c| matches!(
        c.operation.as_str(),
        "order.list"
            | "order.read"
            | "payment.list"
            | "delivery.list"
            | "delivery.read"
            | "return.list"
    )));
    // the plan starts after the last answer: its first step is the ordinary `order` read
    let plan_call = all
        .iter()
        .find(|c| !is_ctx(c))
        .expect("the plan's first step after the context");
    assert_eq!(
        (plan_call.step.as_str(), plan_call.operation.as_str()),
        ("order", "order.read")
    );

    // the loaded context is on the case and in the conversation's snapshot
    let raw = o
        .cases()
        .customer_context_raw("CS-1")
        .await
        .unwrap()
        .expect("ctx on the case");
    let ctx: CustomerContext = serde_json::from_str(&raw).unwrap();
    assert_eq!(ctx.party, "C-1");
    assert_eq!(ctx.orders.len(), 8);
    assert_eq!(ctx.orders[0].order_id, "O7", "newest first");
    assert_eq!(ctx.orders[0].items[0].name, "Rug");
    assert!(
        ctx.orders[7].items.is_empty(),
        "the two oldest were not read"
    );
    assert!(ctx.payments.is_empty());
    assert_eq!(
        ctx.degraded,
        ["payment.list"],
        "a failed read is named, not fatal"
    );
    let (tenant, job) = (fx.d.f.tenant, fx.d.f.job.job_key.clone());
    let snap = cc::read_snapshot(&fx.bus, tenant, &job, "conv-CS-1")
        .await
        .unwrap()
        .expect("snapshot");
    assert!(!snap.2);

    // a second case of the conversation reuses the snapshot: no context read
    insert_case_in(&fx, "CS-2", "conv-CS-1", &current_design(&fx).await).await;
    identify(&fx, "CS-2").await;
    let mut req2 = request("CS-2");
    req2.conversation_id = "conv-CS-1".into();
    o.on_qualified(qualified(&req2)).await.unwrap();
    let second = calls(&fx, "CS-2").await;
    assert!(
        !second.is_empty() && second.iter().all(|c| !is_ctx(c)),
        "cached: {second:?}"
    );
    assert!(
        o.cases()
            .customer_context_raw("CS-2")
            .await
            .unwrap()
            .is_some()
    );

    // after a write the next case reads again
    cc::invalidate(&fx.bus, tenant, &job, "conv-CS-1").await;
    insert_case_in(&fx, "CS-3", "conv-CS-1", &current_design(&fx).await).await;
    identify(&fx, "CS-3").await;
    let mut req3 = request("CS-3");
    req3.conversation_id = "conv-CS-1".into();
    o.on_qualified(qualified(&req3)).await.unwrap();
    let third = calls(&fx, "CS-3").await;
    assert!(
        third
            .iter()
            .any(|c| is_ctx(c) && c.operation == "order.list"),
        "stale: {third:?}"
    );
}

/// A case with no identified customer (no verified party) runs no context phase: its first call is the plan's.
/// Mutation: the phase run for every case.
#[tokio::test]
async fn an_unidentified_case_loads_no_context() {
    let fx = setup("ctx-anon").await;
    let o = orch(&fx, StaticProcedures);
    insert_case(&fx, "CS-1", &current_design(&fx).await).await;
    o.on_qualified(qualified(&request("CS-1"))).await.unwrap();
    let cs = calls(&fx, "CS-1").await;
    assert_eq!(cs.len(), 1);
    assert!(!is_ctx(&cs[0]) && cs[0].operation == "order.read");
    assert!(
        o.cases()
            .customer_context_raw("CS-1")
            .await
            .unwrap()
            .is_none()
    );
}

/// Counts every call; answers `out`.
struct Rows {
    out: Value,
}

impl OpExecutor for Rows {
    fn execute<'a>(
        &'a self,
        _req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move {
            agent_api::runtime::runner::ops::outcome(
                OpState::Ok,
                None,
                None,
                self.out.as_object().cloned().unwrap_or_default(),
            )
        })
    }
}

/// Answers every call with a timeout.
struct Down;

impl OpExecutor for Down {
    fn execute<'a>(
        &'a self,
        _req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async { failed("timeout", "down") })
    }
}

/// The loader over the runner: runs the steps as `context` calls of the known customer's case.
struct RunnerCaller<'a> {
    rn: &'a agent_api::runtime::runner::Runner,
    fx: &'a Fx,
}

impl ContextCaller for RunnerCaller<'_> {
    fn call<'a>(&'a self, step: &'a CtxStep) -> BoxFuture<'a, StepRes> {
        Box::pin(async move {
            let c = ctx_call(
                self.fx,
                "CS-1",
                &step.name,
                step.op,
                Value::Object(step.input.clone()),
            );
            let res = self.rn.handle(&c).await.unwrap();
            StepRes {
                ok: res.state == ResultState::Ok,
                code: res.outcome_code,
                output: res
                    .output
                    .and_then(|v| v.as_object().cloned())
                    .unwrap_or_default(),
            }
        })
    }
}

fn runner(fx: &Fx, x: Executors) -> agent_api::runtime::runner::Runner {
    let mut st = fx.d.e.state.clone();
    st.db = fx.rt.clone();
    agent_api::runtime::runner::Runner::new(
        fx.bus.clone(),
        st,
        Arc::new(agent_api::runtime::design::DesignCache::default()),
    )
    .unwrap()
    .with_consumer("ctx-loader")
    .with_consume(r::fast())
    .with_executors(x)
}

/// Fifteen orders and no items in the list: the loader reads the list, the payments and exactly six orders — nine
/// calls, never more than 11 — and the orders carry their items. Mutation: the fan-out reads every order.
#[tokio::test]
async fn the_loader_fans_out_at_most_six_reads_over_the_runner() {
    let fx = setup("ctx-loader").await;
    known_case(&fx, "CS-1", "C-1").await;
    let ids: Vec<String> = (0..15).map(|i| format!("O{i:02}")).collect();
    let list = Arc::new(Rows {
        out: json!({"order_id": ids, "status": vec!["paid"; 15]}),
    });
    let pay = Arc::new(Rows {
        out: json!({"payment_id": ["P1"], "order_id": ["O00"], "customer_id": ["C-1"], "amount": [10.5], "status": ["captured"]}),
    });
    let read = Fake::ok(
        json!({"status": "shipped", "customer_id": "C-1", "lines": [{"name": "Lamp", "qty": 2}]}),
    );
    let rn = runner(
        &fx,
        Executors::default()
            .register("order.list", list)
            .register("payment.list", pay)
            .register("order.read", read.clone()),
    );
    let caller = RunnerCaller { rn: &rn, fx: &fx };
    // stage 1 now reads every list the design offers, so the design offers exactly the retail reads under test
    let ctx = cc::load(&caller, "C-1", |op| {
        matches!(op, "order.list" | "payment.list" | "order.read")
    })
    .await;
    assert_eq!(read.hits(), cc::MAX_ORDER_READS);
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM operation_call WHERE tenant_id = $1 AND case_id = 'CS-1'",
    )
    .bind(fx.d.f.tenant)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(rows, 1 + 1 + 6, "list, payments and six reads");
    assert!(rows <= MAX_CONTEXT_CALLS);
    assert!(ctx.degraded.is_empty(), "{:?}", ctx.degraded);
    assert_eq!(ctx.orders.len(), 15);
    assert_eq!(ctx.orders.iter().filter(|o| !o.items.is_empty()).count(), 6);
    assert_eq!(ctx.payments.len(), 1);
    assert_eq!(ctx.payments[0].amount, Some(10.5));
}

/// Every read but the order list failing: the context holds what answered, names what did not, and nothing panics.
/// Then the order list failing: an empty context, all named. Mutation: a failed read aborting the load.
#[tokio::test]
async fn the_loader_tolerates_partial_failure() {
    let fx = setup("ctx-partial").await;
    known_case(&fx, "CS-1", "C-1").await;
    let list = Arc::new(Rows {
        out: json!({"order_id": ["O1"], "status": ["paid"]}),
    });
    let rn = runner(
        &fx,
        Executors::default()
            .register("order.list", list)
            .register("payment.list", Arc::new(Down))
            .register("order.read", Arc::new(Down)),
    );
    let caller = RunnerCaller { rn: &rn, fx: &fx };
    // order.list is optional now (a design without it reads nothing), so the design offers it and order.read;
    // payment.list is not offered, hence not read, hence not degraded
    let ctx = cc::load(&caller, "C-1", |op| {
        matches!(op, "order.list" | "order.read")
    })
    .await;
    assert_eq!(ctx.orders.len(), 1);
    assert!(ctx.orders[0].items.is_empty());
    assert!(ctx.payments.is_empty());
    assert_eq!(ctx.degraded, ["order.read:0"]);

    let rn = runner(
        &fx,
        Executors::default().register("order.list", Arc::new(Down)),
    );
    let caller = RunnerCaller { rn: &rn, fx: &fx };
    let ctx = cc::load(&caller, "C-1", |op| op == "order.list").await;
    assert!(ctx.orders.is_empty() && ctx.payments.is_empty());
    assert_eq!(ctx.degraded, ["order.list"]);
}

// ---- the staff route ---------------------------------------------------------------------------------------------

const PATH: &str = "/api/jobs/j1/conversations/conv-known/customer-context";

/// The identified conversation `conv-known` of customer C-1 (assertion), one case (CS-9, clear of the store's own
/// numbering from CS-1), one call that named order O2 and
/// one context read, and the snapshot a case start would have stored.
async fn seed(c: &conv::Ctx) {
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, 'conv-known', 'prod', 'web_chat', 'C-1')",
    )
    .bind(c.t)
    .bind(c.job)
    .execute(&c.e.owner)
    .await
    .unwrap();
    crate::common::state::identify(
        &c.e.owner,
        c.t,
        c.job,
        "conv-known",
        "C-1",
        Some("ann@example.org"),
    )
    .await;
    crate::common::state::put(&c.e.owner, c.t, c.job, "conv-known", "name", "Ann").await;
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, status, title) \
         VALUES ($1, $2, 'CS-9', 'conv-known', 'web_chat', 'prod', 'open', 'Where is my parcel')",
    )
    .bind(c.t)
    .bind(c.job)
    .execute(&c.e.owner)
    .await
    .unwrap();
    for (step, op, phase, refs) in [
        ("order", "order.read", json!(0), json!({"order_id": "O2"})),
        (
            "ctx:order.read:0",
            "order.read",
            json!("context"),
            json!({"order_id": "O7"}),
        ),
    ] {
        sqlx::query(
            "INSERT INTO operation_call (tenant_id, job_id, call_id, case_id, operation, route, env, state, \
                                         finished_at, initiator, step) \
             VALUES ($1, $2, $3, 'CS-9', $4, 'builtin', 'prod', 'ok', now(), 'procedure@t', $5)",
        )
        .bind(c.t)
        .bind(c.job)
        .bind(agent_api::runtime::domain::call_id("CS-9", 0, step, 1))
        .bind(op)
        .bind(json!({"phase": phase, "step": step, "refs": refs}))
        .execute(&c.e.owner)
        .await
        .unwrap();
    }
    let ctx: CustomerContext = serde_json::from_value(json!({
        "party": "C-1",
        "orders": [{"order_id": "O2", "status": "shipped", "placed_at": "2026-10-20",
                    "items": [{"name": "Rug", "qty": 2}]}],
        "payments": [{"payment_id": "P1", "order_id": "O2", "amount": 59.0, "currency": "EUR",
                      "status": "captured", "refunded_amount": 0.0, "at": "2026-10-20"}],
        "deliveries": [{"delivery_id": "D1", "order_id": "O2", "carrier": "PostNL", "status": "in_transit",
                        "eta": "2026-10-27", "delivered_at": null}],
        "cases": [], "degraded": []
    }))
    .unwrap();
    cc::put_snapshot(
        c.e.state.redis.as_ref().expect("redis"),
        c.t,
        "j1",
        "conv-known",
        &ctx,
        1,
    )
    .await
    .unwrap();
}

/// The route returns exactly the agreed shape for support_agent, support_manager and operator; any other role is 403,
/// another tenant's caller 404; an anonymous conversation has no source, no customer and empty lists. Focus holds the
/// record the conversation's own call named, not the context phase's reads.
/// Mutations: a role added or dropped; the tenant scope dropped; lists filled for an unidentified conversation.
#[tokio::test]
async fn the_customer_context_route_serves_workers_and_nobody_else() {
    let vars = r::api_vars("ctx-route").await;
    let c = conv::setup_with(|v| v.extend(vars)).await;
    crate::common::insert_user(&c.e.owner, "u-op", c.t, "operator").await;
    seed(&c).await;

    for sub in ["u-agent", "u-mgr", "u-op"] {
        let (s, b) = c.get(PATH, sub).await;
        assert_eq!(s, StatusCode::OK, "{sub}: {b}");
        let keys: Vec<&str> = b.as_object().unwrap().keys().map(String::as_str).collect();
        let mut want = [
            "identity",
            "customer",
            "orders",
            "payments",
            "deliveries",
            "cases",
            "focus",
        ];
        want.sort_unstable();
        let mut got = keys.clone();
        got.sort_unstable();
        assert_eq!(got, want, "{sub}: exactly these fields");
        assert_eq!(b["identity"]["source"], "assertion");
        assert_eq!(b["identity"]["level"], 2);
        assert!(b["identity"]["verified_at"].is_string(), "{b}");
        assert_eq!(
            b["customer"],
            json!({"id": "C-1", "name": "Ann", "email": "ann@example.org"})
        );
        assert_eq!(
            b["orders"],
            json!([{"order_id": "O2", "status": "shipped", "placed_at": "2026-10-20",
                    "items": [{"name": "Rug", "qty": 2}]}])
        );
        assert_eq!(b["payments"][0]["payment_id"], "P1");
        assert_eq!(b["payments"][0]["refunded_amount"], 0.0);
        assert_eq!(b["deliveries"][0]["carrier"], "PostNL");
        assert!(b["deliveries"][0].get("tracking_url").is_none());
        assert_eq!(b["cases"][0]["case_id"], "CS-9");
        assert_eq!(b["cases"][0]["state"], "open");
        assert_eq!(b["cases"][0]["title"], "Where is my parcel");
        assert!(b["cases"][0]["opened_at"].is_string());
        assert_eq!(
            b["focus"],
            json!([{"kind": "order", "id": "O2", "why": "looked up for this chat"}]),
            "the context read of O7 is not focus"
        );
    }

    // any other role
    for sub in ["u-int", "u-obs"] {
        let (s, b) = c.get(PATH, sub).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::FORBIDDEN, "forbidden"),
            "{sub}: {b}"
        );
        assert!(b["customer"].is_null() && b.get("orders").is_none(), "{b}");
    }

    // another tenant's caller does not see the conversation
    let t2 = Uuid::new_v4();
    crate::common::insert_tenant(&c.e.owner, t2, "Beta").await;
    crate::common::insert_user(&c.e.owner, "u-beta", t2, "tenant_admin").await;
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'j1', 'J1', '[]', 'test')",
    )
    .bind(t2)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (s, b) = c.get(PATH, "u-beta").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "conversation_not_found"),
        "{b}"
    );

    // an unknown conversation
    let (s, b) = c
        .get(
            "/api/jobs/j1/conversations/nope/customer-context",
            "u-agent",
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "conversation_not_found")
    );

    // an anonymous web chat conversation: nobody is identified, everything is empty
    // written straight to the store: the job has no live release, so the customer inbound would refuse it
    let (conv_id, _case) = c
        .customer_case("wc-anon", "m1", "Where is my parcel?")
        .await;
    let (s, b) = c
        .get(
            &format!("/api/jobs/j1/conversations/{conv_id}/customer-context"),
            "u-agent",
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b,
        json!({
            "identity": {"source": null, "level": 0, "verified_at": null},
            "customer": null, "orders": [], "payments": [], "deliveries": [], "cases": [], "focus": []
        })
    );
}
