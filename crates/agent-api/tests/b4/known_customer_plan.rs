//! B4 known-customer web chat: the tests the plan's test inventory names that had no test of that name
//! (`~/.claude/handoffs/agent-backend-2026-10-20/plan-b4-known-customer-webchat-2026-10-26.md`, section 7 "Rust test
//! inventory"). The names are fixed so reviewers can grep them. Each test is a thin, property-named check that drives
//! the REAL code at the closest reachable layer; where another module already proves the same thing in depth
//! (`identity_born.rs`, `resolution.rs, `customer_context.rs`, `library_flow.rs`, `conversations.rs`, `known_records.rs`),
//! this file repeats the setup it needs (those helpers are private to their modules) and asserts the property alone.
//!
//! | # | Test | Layer |
//! |---|---|---|
//! | 2 | `known_customer_never_gets_identity_required` | real runner + orchestrator (case row is the truth) |
//! | 4 | `context_phase_reads_only_the_allowlist` | pure planner + orchestrator context phase + runner |
//! | 5 | `where_is_my_parcel_resolves_the_open_delivery` | orchestrator `slots` step over a fake runner |
//! | 6 | `return_the_rug_resolves_by_product` | orchestrator `slots` step |
//! | 7 | `ambiguity_names_product_and_date` | orchestrator `slots` step + `record_choice_question` |
//! | 8 | `no_reply_contains_an_id` | `reply::compose` + the question / sentence builders |
//! | 9 | `no_prompt_asks_for_a_lookup_key` | `qualify::step` / `question` / `unclear` / `body` over a catalogue of own spaces |
//! | 10 | `step_up_hands_over_never_asks` | real runner + orchestrator |
//! | 11 | `refund_status_never_calls_payment_refund` | orchestrator over an own refund space, fake runner |
//! | 12 | `assistant_tools_exclude_x_ops_for_refund_and_cancel_activities` | `tools::offered` over own toolsets |
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_api::conversations::store;
use agent_api::design::{DesignOf, JobRef};
use agent_api::job_design::base::{ActivityOp, BaseActivity, Bases};
use agent_api::job_design::runtime::JobDesign;
use agent_api::job_design::toolset::{DesignToolsets, RoleSetting, Toolsets};
use agent_api::runtime::assistant::tools::offered;
use agent_api::runtime::catalog::{OperationCatalog, PinnedCatalog};
use agent_api::runtime::customer_context::{self as cc, CONTEXT_OPS};
use agent_api::runtime::design::{DesignCache, RuntimeDesign};
use agent_api::runtime::domain::{
    CaseOutcome, OP_CALL, OpCall, OpResult, OpState as ResultState, Phase, ProcedureRef,
    QualifiedRequest, REQUEST_RESOLVED, Resolution, Step,
};
use agent_api::runtime::orchestrator::{Orchestrator, ProcedureSource, StaticProcedures};
use agent_api::runtime::prompt::SystemMessage;
use agent_api::runtime::reply;
use agent_api::runtime::resolve::{self, Choice, Pick, RECORD_KINDS, contains_id_pattern};
use agent_api::runtime::routes::ResolvedTarget;
use agent_api::runtime::runner::ops::{
    Executors, OpExecutor, OpOutcome, OpRequest, OpState, failed, outcome,
};
use agent_api::runtime::runner::{Runner, result_topic};
use agent_api::runtime::spaces::{ContextSpace, SpaceCatalog, TemplateProcedure};
use agent_api::runtime::{Env, Envelope};
use agent_api::turn::qualify::{self, Draft, Extraction, Outcome, never_asked};
use agent_model::catalog::{OperationRow, STAR, op_key};
use agent_model::compile::CatalogueItem;
use agent_model::kinds::catalog::SideEffect;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use crate::b4::design::put_target;
use crate::b4::flow::{self as fl, AREA, EveryOpOffered, Fx, SPACE, current_design, qualified};
use crate::b4::it_desk_identity::{OwnSpace, own_fx};
use crate::b4::runner_checks::{Fake, ctx_call, known_case};
use crate::redis_env as r;

const ANN_EMAIL: &str = "ann@example.org";

// ==== the verify-style world: a real runner over fake systems of record ===========================================

/// The system of record's orders: RET-01 is C-1's (Ann), RET-02 C-2's, RET-03 names no owner.
struct Orders {
    hits: AtomicUsize,
}

impl OpExecutor for Orders {
    fn execute<'a>(
        &'a self,
        req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move {
            self.hits.fetch_add(1, Ordering::SeqCst);
            let record = match req.inputs.get("order_id").and_then(Value::as_str) {
                Some("RET-01") => json!({"status": "shipped", "total": 59.0, "customer_id": "C-1",
                                         "customer_email": ANN_EMAIL}),
                Some("RET-02") => {
                    json!({"status": "delivered", "total": 12.0, "customer_id": "C-2",
                                         "customer_email": "bo@example.org"})
                }
                Some("RET-03") => json!({"status": "shipped", "total": 5.0}),
                _ => return failed("not_found", "no such order"),
            };
            outcome(
                OpState::Ok,
                None,
                None,
                record.as_object().cloned().unwrap(),
            )
        })
    }
}

/// Any other operation: counts its calls, answers `out`.
struct Counting {
    hits: AtomicUsize,
    out: Value,
}

impl Counting {
    fn new(out: Value) -> Arc<Counting> {
        Arc::new(Counting {
            hits: AtomicUsize::new(0),
            out,
        })
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl OpExecutor for Counting {
    fn execute<'a>(
        &'a self,
        _req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move {
            self.hits.fetch_add(1, Ordering::SeqCst);
            outcome(
                OpState::Ok,
                None,
                None,
                self.out.as_object().cloned().unwrap_or_default(),
            )
        })
    }
}

/// A runner over `x` on the fixture's runtime pool (what the binary runs as).
fn runner_with(fx: &Fx, x: Executors) -> Runner {
    let mut st = fx.d.e.state.clone();
    st.db = fx.rt.clone();
    Runner::new(fx.bus.clone(), st, Arc::new(DesignCache::default()))
        .unwrap()
        .with_consumer("runner-known-customer-plan")
        .with_consume(r::fast())
        .with_executors(x)
}

struct World {
    fx: Fx,
    orders: Arc<Orders>,
    offering: Arc<Counting>,
    runner: Runner,
}

/// The default job with its target selection saved, `order.read` served by [`Orders`], `offering.read` by a counter.
async fn world(test: &str) -> World {
    let fx = fl::setup_latest(test, Env::Test).await;
    put_target(&fx.d).await;
    let orders = Arc::new(Orders {
        hits: AtomicUsize::new(0),
    });
    let offering = Counting::new(json!({"name": "Rug", "price": 59.0}));
    let x = Executors::default()
        .register("order.read", orders.clone())
        .register("offering.read", offering.clone());
    let runner = runner_with(&fx, x);
    World {
        fx,
        orders,
        offering,
        runner,
    }
}

/// A conversation on `channel` and its case `case` (status open, an unidentified customer); one inbound customer
/// message.
async fn case_on(fx: &Fx, case: &str, conv: &str, channel: &str) {
    let (t, j) = (fx.d.f.tenant, fx.d.f.job.job_id);
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
    )
    .bind(t)
    .bind(j)
    .bind(conv)
    .bind(fx.scope.env.as_str())
    .bind(channel)
    .bind("tg:42")
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
         VALUES ($1, $2, $3, $4, $5, $6, NULL, $7, NULL, $7, 'open')",
    )
    .bind(t)
    .bind(j)
    .bind(case)
    .bind(conv)
    .bind(channel)
    .bind(fx.scope.env.as_str())
    .bind(draft_rev)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_message (tenant_id, job_id, conversation_id, role, text, case_id, at_ms) \
         VALUES ($1, $2, $3, 'customer', 'please help', $4, 1)",
    )
    .bind(t)
    .bind(j)
    .bind(conv)
    .bind(case)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
}

/// A case of the known customer Ann (C-1, level 2, her KYC email): what the drain creates for an identified chat.
async fn case_as_ann(fx: &Fx, case: &str, conv: &str, channel: &str) {
    case_on(fx, case, conv, channel).await;
    crate::common::state::identify(
        &fx.d.e.owner,
        fx.d.f.tenant,
        fx.d.f.job.job_id,
        conv,
        "C-1",
        Some(ANN_EMAIL),
    )
    .await;
}

/// A web chat conversation of `customer` / `source` and its case born through the production insert
/// (`store::create_case`, the drain's): the case id.
async fn born_case(
    fx: &Fx,
    conv_id: &str,
    customer: &str,
    source: Option<&str>,
    email: Option<&str>,
) -> String {
    let (t, j) = (fx.d.f.tenant, fx.d.f.job.job_id);
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, $3, $4, 'web_chat', $5)",
    )
    .bind(t)
    .bind(j)
    .bind(conv_id)
    .bind(fx.scope.env.as_str())
    .bind(customer)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    if let Some(source) = source {
        crate::common::state::identify_as(&fx.d.e.owner, t, j, conv_id, source, customer, email)
            .await;
    }
    let mut tx = store::begin_job(&fx.d.e.state.db, t, j).await.unwrap();
    let conv = store::conversation_or_404(&mut tx, conv_id).await.unwrap();
    let case = store::create_case(
        &mut tx,
        j,
        &conv,
        "where is my order",
        &current_design(fx).await,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    case
}

/// `(identity level, verified party, email)` of case `case`, derived from its conversation's state.
async fn born_row(fx: &Fx, case: &str) -> (i32, Option<String>, Option<String>) {
    crate::common::state::case_identity(&fx.d.e.owner, fx.d.f.tenant, fx.d.f.job.job_id, case).await
}

async fn count_identity_required(fx: &Fx) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM operation_call WHERE tenant_id = $1 AND outcome_code = 'identity_required'",
    )
    .bind(fx.d.f.tenant)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap()
}

/// An `op.call` of a procedure step of `case` at `identity` (the level the plan queued it at).
fn rcall(
    fx: &Fx,
    case: &str,
    conv: &str,
    step: &str,
    operation: &str,
    input: Value,
    identity: u8,
) -> OpCall {
    serde_json::from_value(json!({
        "call_id": agent_api::runtime::domain::call_id(case, 0, step, 1),
        "case_id": case, "conversation_id": conv, "phase": 0, "step": step,
        "attempt": 1, "agent_role": "procedure", "operation": operation, "variant": null,
        "input": input, "idempotency_key": null, "env": fx.scope.env,
        "release_id": null, "draft_rev": null, "confirm_ref": null, "initiator": "procedure@test",
        "identity_level": identity, "deadline_ms": 0
    }))
    .unwrap()
}

/// `offering.read` (level 0), then `order.read` (level 2): a procedure whose second phase needs a verified customer.
struct TwoPhases;

impl ProcedureSource for TwoPhases {
    fn plan(
        &self,
        _design: &RuntimeDesign,
        _req: &QualifiedRequest,
    ) -> Option<(Option<ProcedureRef>, Vec<Phase>)> {
        Some((
            Some(ProcedureRef {
                name: "order_status".into(),
                version: 1,
            }),
            vec![
                Phase {
                    steps: vec![Step::op(
                        "offering",
                        "offering.read",
                        None,
                        json!({"offering_id": "SKU-1"}),
                    )],
                },
                Phase {
                    steps: vec![Step::op(
                        "order",
                        "order.read",
                        None,
                        json!({"order_id": "$order_id"}),
                    )],
                },
            ],
        ))
    }
}

fn level_0_request(case: &str, conv: &str) -> QualifiedRequest {
    serde_json::from_value(json!({
        "case_id": case, "conversation_id": conv, "area": AREA, "context_space": SPACE,
        "request_type": "order_status", "slots": {"order_id": "RET-01"}, "identity_level": 0,
        "message_id": format!("msg-{case}")
    }))
    .unwrap()
}

/// Run every outstanding `op.call` of `case` through the real runner and hand its result to the orchestrator, until
/// none is left. Returns the calls run, oldest first (the customer-context preload is not listed).
async fn pump(w: &World, o: &Orchestrator<TwoPhases>, case: &str, seen: &mut usize) -> Vec<OpCall> {
    let mut ran = Vec::new();
    loop {
        let all: Vec<OpCall> = r::entries(&w.fx.bus, &w.fx.bus.streams.op())
            .await
            .into_iter()
            .filter(|e| e.correlation_id == case && e.event() == Some(OP_CALL))
            .map(|e| e.payload_as().unwrap())
            .collect();
        if all.len() <= *seen {
            return ran;
        }
        let skip = *seen;
        for c in all.into_iter().skip(skip) {
            *seen += 1;
            let res: OpResult = w.runner.handle(&c).await.unwrap();
            let env = Envelope::new(&result_topic("op.call"), "runner", case, &res).unwrap();
            o.on_result(env).await.unwrap();
            if c.provenance.as_deref() != Some(agent_api::runtime::domain::PROVENANCE_CONTEXT) {
                ran.push(c);
            }
        }
    }
}

async fn resolutions_of(fx: &Fx, case: &str) -> Vec<Resolution> {
    r::entries(&fx.bus, &fx.bus.streams.customer(AREA))
        .await
        .into_iter()
        .filter(|e| e.correlation_id == case && e.event() == Some(REQUEST_RESOLVED))
        .map(|e| e.payload_as().unwrap())
        .collect()
}

/// The agent lines of the conversation, oldest first.
async fn agent_lines_of(fx: &Fx, conv: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT text FROM conversation_message WHERE tenant_id = $1 AND conversation_id = $2 AND role = 'agent' \
          ORDER BY at_ms, message_id",
    )
    .bind(fx.d.f.tenant)
    .bind(conv)
    .fetch_all(&fx.d.e.owner)
    .await
    .unwrap()
}

// ==== 2 ===========================================================================================================

/// Plan test inventory: `known_customer_never_gets_identity_required`. (a) A case born from an identified
/// conversation is level 2 and bound to its customer, so its very first call, queued at level 0 as a procedure
/// does, runs. (b) Ann's case runs both phases of a two-phase procedure in the SAME case whatever level the request
/// was qualified at: resolved, no agent line. No `identity_required` outcome is recorded
/// anywhere. Layer: the real runner and orchestrator.
#[tokio::test]
async fn known_customer_never_gets_identity_required() {
    let w = world("kcp-never-identity-required").await;
    let fx = &w.fx;

    let case = born_case(fx, "conv-1", "C-1", Some("assertion"), Some(ANN_EMAIL)).await;
    assert_eq!(
        born_row(fx, &case).await,
        (2, Some("C-1".into()), Some(ANN_EMAIL.into()))
    );
    let res = w
        .runner
        .handle(&rcall(
            fx,
            &case,
            "conv-1",
            "o1",
            "order.read",
            json!({"order_id": "RET-01"}),
            0,
        ))
        .await
        .unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(res.outcome_code, None, "{res:?}");

    case_as_ann(fx, "CS-2", "conv-2", "web_chat").await;
    let o = fl::orch(fx, TwoPhases);
    o.on_qualified(qualified(&level_0_request("CS-2", "conv-2")))
        .await
        .unwrap();
    let mut seen = 0;
    let ran = pump(&w, &o, "CS-2", &mut seen).await;
    assert_eq!(
        ran.iter().map(|c| c.operation.as_str()).collect::<Vec<_>>(),
        ["offering.read", "order.read"]
    );
    let rs = resolutions_of(fx, "CS-2").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Resolved, "{rs:?}");
    assert_ne!(rs[0].code.as_deref(), Some("identity_required"));
    assert_eq!(w.offering.hits(), 1);
    assert!(
        agent_lines_of(fx, "conv-2").await.is_empty(),
        "the agent asked nothing"
    );
    assert_eq!(count_identity_required(fx).await, 0);
}

// ==== 10 ==========================================================================================================

/// Plan test inventory: `step_up_hands_over_never_asks`. A step the conversation cannot run (an unidentified web
/// chat that still reached the runner: no `subject` in state) is refused `state_required` (A2; it was
/// `identity_required` / `step_up_required` while the runner compared levels); the case does NOT wait for details
/// and asks the customer nothing: it ends as a handover with code and item reason `tool_failed` (the orchestrator's
/// mapping of `state_required`), exactly one item, no record in the facts, and the refused read never reached the
/// system.
/// Layer: the real runner and orchestrator.
#[tokio::test]
async fn step_up_hands_over_never_asks() {
    let w = world("kcp-step-up").await;
    let fx = &w.fx;
    case_on(fx, "CS-1", "conv-1", "web_chat").await;
    let o = fl::orch(fx, TwoPhases);
    o.on_qualified(qualified(&level_0_request("CS-1", "conv-1")))
        .await
        .unwrap();
    let mut seen = 0;
    let ran = pump(&w, &o, "CS-1", &mut seen).await;
    assert_eq!(
        ran.iter().map(|c| c.operation.as_str()).collect::<Vec<_>>(),
        ["offering.read", "order.read"]
    );
    let rs = resolutions_of(fx, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::Handover, "{rs:?}");
    assert_eq!(rs[0].code.as_deref(), Some("tool_failed"));
    assert!(rs[0].facts.get("order").is_none(), "no record");
    let items: Vec<(String, String)> = sqlx::query_as(
        "SELECT reason, trigger FROM agent_handover_items WHERE tenant_id = $1 AND case_id = 'CS-1'",
    )
    .bind(fx.d.f.tenant)
    .fetch_all(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(
        items,
        [(
            "tool_failed".to_owned(),
            "order.read requires state the conversation lacks".to_owned()
        )]
    );
    assert!(
        agent_lines_of(fx, "conv-1").await.is_empty(),
        "no question to the customer"
    );
    assert_eq!(
        w.orders.hits.load(Ordering::SeqCst),
        0,
        "the refused read never reached the system"
    );
}

// ==== 4 ===========================================================================================================

/// Answer every outstanding context read of `case` (`world`: operation key -> answer; anything else is an ok, empty
/// answer) until none is left; every call of the case afterwards.
async fn answer_context(
    fx: &Fx,
    o: &Orchestrator<StaticProcedures>,
    case: &str,
    world: &Value,
) -> Vec<OpCall> {
    let mut done: HashSet<String> = HashSet::new();
    loop {
        let cs = fl::calls(fx, case).await;
        let todo: Vec<OpCall> = cs
            .iter()
            .filter(|c| c.provenance.as_deref() == Some("context") && !done.contains(&c.call_id))
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
            o.on_result(fl::answer(&c, ResultState::Ok, "ok", out))
                .await
                .unwrap();
        }
    }
}

/// Plan test inventory: `context_phase_reads_only_the_allowlist` (adversarial). The customer-context phase reads the
/// customer's records with operations of `CONTEXT_OPS` only, even when the design OFFERS everything:
/// (a) the pure planner (`stage1` / `stage2`) given an `offers` that says yes to every operation, including the
/// writes the job's catalogue knows, plans only allowlisted operations; (b) the orchestrator of a job whose catalogue
/// holds write operations publishes no context call outside the allowlist and none that writes; (c) the runner
/// refuses a context call to a write, a write-X, an ungranted read and an unknown operation with `not_context_op`
/// before any executor runs, while an allowlisted read of the case's own party still runs.
#[tokio::test]
async fn context_phase_reads_only_the_allowlist() {
    let fx = crate::b4::runner_checks::setup("kcp-ctx-allowlist").await;

    // the design the job runs on: which operations it knows, and which of them write
    let design = {
        let mut tx = fx.d.f.job.begin(&fx.d.e.state.db).await.unwrap();
        let d = RuntimeDesign::load(
            &mut tx,
            &fx.d.e.state.design,
            &fx.d.f.job_ref(),
            &DesignOf::Draft,
        )
        .await
        .unwrap_or_else(|e| panic!("load: {e:?}"));
        tx.commit().await.unwrap();
        d
    };
    let writes: Vec<String> = design
        .catalog
        .keys()
        .into_iter()
        .filter(|k| {
            design
                .catalog
                .get(k)
                .is_some_and(|def| def.side_effect != SideEffect::R)
        })
        .map(str::to_owned)
        .collect();
    assert!(
        !writes.is_empty(),
        "the job's catalogue offers write operations (the adversarial premise)"
    );
    for op in CONTEXT_OPS {
        if let Some(def) = design.catalog.get(op) {
            assert_eq!(
                def.side_effect,
                SideEffect::R,
                "{op} is on the context allowlist and must be a read"
            );
        }
    }

    // (a) the pure planner, a design that offers every operation there is
    let everything = |_op: &str| true;
    let first = cc::stage1("C-1", everything);
    assert!(!first.is_empty());
    let list = json!({"order_id": ["O1", "O2"], "status": ["paid", "paid"],
                      "placed_at": ["2026-10-01", "2026-10-02"]});
    let orders = cc::parse_orders(list.as_object().unwrap());
    let fan_out = cc::stage2(&orders, true);
    for step in first.iter().chain(fan_out.iter()) {
        assert!(
            cc::is_context_op(step.op),
            "the planner emitted {} which is not on the allowlist",
            step.op
        );
        assert!(
            !writes.iter().any(|w| w == step.op),
            "the planner emitted the write {}",
            step.op
        );
    }

    // (b) the orchestrator over the real job
    let o = fl::orch(&fx, StaticProcedures);
    fl::insert_case(&fx, "CS-1", &current_design(&fx).await).await;
    fl::identify_conversation(&fx, "conv-CS-1", "C-1", None).await;
    o.on_qualified(qualified(&fl::request("CS-1")))
        .await
        .unwrap();
    let all = answer_context(
        &fx,
        &o,
        "CS-1",
        &json!({"order.list": {"order_id": ["O1", "O2", "O3"], "status": ["paid", "paid", "paid"],
                               "placed_at": ["2026-10-01", "2026-10-02", "2026-10-03"]}}),
    )
    .await;
    let context_calls: Vec<&OpCall> = all
        .iter()
        .filter(|c| c.provenance.as_deref() == Some("context"))
        .collect();
    assert!(
        !context_calls.is_empty(),
        "an identified case loads its context"
    );
    for c in &context_calls {
        assert!(
            cc::is_context_op(&c.operation),
            "a context call to {}",
            c.operation
        );
        assert!(
            !writes.contains(&c.operation),
            "a context call to the write {}",
            c.operation
        );
    }
    assert!(context_calls.len() as i64 <= cc::MAX_CONTEXT_CALLS);

    // (c) the runner refuses what the orchestrator would never send
    known_case(&fx, "CS-2", "C-1").await;
    let refund = Fake::ok(json!({"refund_id": "RF-1"}));
    let cancel = Fake::ok(json!({"status": "cancelled"}));
    let launch = Fake::ok(json!({"status": "launched"}));
    let list_ok = Fake::ok(json!({"order_id": ["O1"], "status": ["paid"]}));
    let rn = runner_with(
        &fx,
        Executors::default()
            .register("payment.refund", refund.clone())
            .register("order.cancel", cancel.clone())
            .register("spaceship.launch", launch.clone())
            .register("order.list", list_ok.clone()),
    );
    for (i, (op, input)) in [
        ("payment.refund", json!({"payment_id": "PAY-1"})),
        ("order.cancel", json!({"order_id": "O1"})),
        ("spaceship.launch", json!({})),
    ]
    .into_iter()
    .enumerate()
    {
        let res = rn
            .handle(&ctx_call(&fx, "CS-2", &format!("ctx:s{i}"), op, input))
            .await
            .unwrap();
        assert_eq!(res.state, ResultState::Denied, "{op}: {res:?}");
        assert_eq!(
            res.outcome_code.as_deref(),
            Some(cc::NOT_CONTEXT_OP),
            "{op}"
        );
    }
    assert_eq!(
        (refund.hits(), cancel.hits(), launch.hits()),
        (0, 0, 0),
        "no executor saw a refused call"
    );
    let ok = rn
        .handle(&ctx_call(
            &fx,
            "CS-2",
            "ctx:order.list",
            "order.list",
            json!({"party_id": "C-1"}),
        ))
        .await
        .unwrap();
    assert_eq!(ok.state, ResultState::Ok, "{ok:?}");
    assert_eq!(list_ok.hits(), 1);
}

// ==== 5, 6, 7 =====================================================================================================

fn is_ctx(c: &OpCall) -> bool {
    c.provenance.as_deref() == Some("context")
}

/// A case `CS-1` of the identified customer `cust-1` in `space`, whose request carries only what the qualifier kept
/// (`slots`: no order id); every context read answered from `world`. Returns every call of the case.
async fn known_case_in(
    fx: &Fx,
    o: &Orchestrator<StaticProcedures>,
    space: &str,
    slots: Value,
    world: &Value,
) -> Vec<OpCall> {
    fl::insert_case(fx, "CS-1", &current_design(fx).await).await;
    fl::identify_conversation(fx, "conv-CS-1", "cust-1", None).await;
    let req: QualifiedRequest = serde_json::from_value(json!({
        "case_id": "CS-1", "conversation_id": "conv-CS-1", "area": AREA, "context_space": space,
        "request_type": space.rsplit(':').next().unwrap(), "slots": slots, "customer_id": "cust-1",
        "identity_level": 2, "message_id": "msg-CS-1"
    }))
    .unwrap();
    o.on_qualified(qualified(&req)).await.unwrap();
    answer_context(fx, o, "CS-1", world).await
}

/// A customer with a Jute rug (12 Oct, delivered) and a Ceramic lamp (20 Oct, delivered): a return could be either.
fn rug_and_lamp() -> Value {
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

/// Plan test inventory: `where_is_my_parcel_resolves_the_open_delivery`. Two orders, one delivered (a rug) and one
/// still in transit (a mug set): "where is my parcel" (the status ranking of `order_status`) is answered about the
/// order whose delivery is open, with no question and no id asked, and the reply label is a fact. Layer: the
/// orchestrator's `slots` step over the newest retail template (a fake runner answers the context reads).
#[tokio::test]
async fn where_is_my_parcel_resolves_the_open_delivery() {
    let fx = fl::setup_latest("kcp-parcel-open-delivery", Env::Test).await;
    let o = fl::orch(&fx, StaticProcedures).with_routes(Arc::new(EveryOpOffered));
    let world = json!({
        "order.list": {
            "order_id": ["A-2", "A-1"], "status": ["in_transit", "delivered"],
            "placed_at": ["2026-10-16T18:02:00Z", "2026-10-02T09:14:00Z"],
            "items": [[{"name": "Stoneware mug set (4)", "qty": 1}], [{"name": "Jute rug", "qty": 1}]],
        },
        "delivery.list": {"delivery_id": ["S-2", "S-1"], "order_id": ["A-2", "A-1"],
                          "status": ["in_transit", "delivered"]},
    });
    let all = known_case_in(
        &fx,
        &o,
        "retail:order-support:order_status",
        json!({}),
        &world,
    )
    .await;
    let step = all
        .iter()
        .find(|c| !is_ctx(c))
        .expect("the plan runs after the context");
    assert_eq!(step.operation, "order.read");
    assert_eq!(
        step.input["order_id"], "A-2",
        "the order whose delivery is open"
    );
    assert!(
        fl::resolutions_in(&fx, AREA, "CS-1").await.is_empty(),
        "nobody was asked"
    );
    let facts = o.cases().results("CS-1").await.unwrap();
    assert_eq!(
        facts["resolved"]["item_label"],
        "Stoneware mug set (4) ordered on 16 October"
    );
}

/// Plan test inventory: `return_the_rug_resolves_by_product`. The customer says only "the rug" (the qualifier keeps
/// those words as `item_hint`; no order id): of a rug and a lamp the return case is filled with the rug's order,
/// silently, and the plan's first step reads it. Layer: the orchestrator's `slots` step (a fake runner answers the
/// context reads).
#[tokio::test]
async fn return_the_rug_resolves_by_product() {
    let fx = crate::b4::runner_checks::setup("kcp-return-the-rug").await;
    let o = fl::orch(&fx, StaticProcedures);
    let all = known_case_in(
        &fx,
        &o,
        SPACE,
        json!({"reason": "too big", "item_hint": "the rug"}),
        &rug_and_lamp(),
    )
    .await;
    let step = all.iter().find(|c| !is_ctx(c)).expect("the plan runs");
    assert_eq!(step.operation, "order.read");
    assert_eq!(step.input["order_id"], "A-1", "the rug, not the lamp");
    assert!(fl::resolutions_in(&fx, AREA, "CS-1").await.is_empty());
    let facts = o.cases().results("CS-1").await.unwrap();
    assert_eq!(
        facts["resolved"]["item_label"],
        "Jute rug ordered on 12 October"
    );
}

/// Plan test inventory: `ambiguity_names_product_and_date`. Both orders fit a return and the customer named neither:
/// the case parks with ONE question that names each candidate by its product and its date (oldest first), carries no
/// id, runs no plan step, and keeps the candidates (with their ids) server-side only. The record-kind question
/// (insurance) names its candidates by label and date the same way. Layer: the orchestrator's `slots` step, and
/// `record_choice_question`.
#[tokio::test]
async fn ambiguity_names_product_and_date() {
    let fx = crate::b4::runner_checks::setup("kcp-ambiguity").await;
    let o = fl::orch(&fx, StaticProcedures);
    let all = known_case_in(
        &fx,
        &o,
        SPACE,
        json!({"reason": "too big"}),
        &rug_and_lamp(),
    )
    .await;
    assert!(
        all.iter().all(is_ctx),
        "no step runs while the customer is asked"
    );
    let rs = fl::resolutions_in(&fx, AREA, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert_eq!(rs[0].outcome, CaseOutcome::ConfirmationRequired);
    assert_eq!(rs[0].code.as_deref(), Some("choice_required"));
    let text = reply::compose(&rs[0]);
    for part in ["Jute rug", "12 October", "Ceramic lamp", "20 October"] {
        assert!(text.contains(part), "`{part}` missing from: {text}");
    }
    assert_eq!(
        text,
        "Is this about your Jute rug from 12 October or your Ceramic lamp from 20 October?"
    );
    assert!(!contains_id_pattern(&text));
    for id in ["A-1", "A-2", "S-1", "S-2", "P-1", "P-2"] {
        assert!(!text.contains(id), "{id} in {text}");
    }
    let key = agent_api::turn::chat_key(&fx.bus, "conv-CS-1", "");
    let raw: Option<String> = redis::cmd("HGET")
        .arg(&key)
        .arg("choice")
        .query_async(&mut fx.bus.conn())
        .await
        .unwrap();
    let choice: Choice = serde_json::from_str(&raw.expect("the parked choice")).unwrap();
    assert_eq!(
        choice
            .picks
            .iter()
            .map(|p| p.order_id.as_str())
            .collect::<Vec<_>>(),
        ["A-1", "A-2"],
        "the ids stay on the server"
    );

    // a registered record kind (insurance policies): label and date, never the policy id
    let policy = RECORD_KINDS
        .iter()
        .find(|k| k.slot == "policy_id")
        .expect("policy_id is a record kind");
    let picks = vec![
        Pick {
            order_id: "POL-5521".into(),
            payment_id: None,
            delivery_id: None,
            items: vec!["Car insurance".into()],
            placed_on: chrono::NaiveDate::from_ymd_opt(2026, 3, 3),
        },
        Pick {
            order_id: "POL-7730".into(),
            payment_id: None,
            delivery_id: None,
            items: vec!["Home insurance".into()],
            placed_on: chrono::NaiveDate::from_ymd_opt(2026, 5, 1),
        },
    ];
    let q = resolve::record_choice_question(policy, &picks, false);
    assert_eq!(
        q,
        "Is this about your Car insurance renewing 3 March or your Home insurance renewing 1 May?"
    );
    assert!(!contains_id_pattern(&q) && !q.contains("POL-"));
}

// ==== 11, 12: a refund-status question never refunds ==============================================================

/// The operation contract the tests below take their operations' side effects from.
const OPDEFS: &str = include_str!("../../contracts/opdefs.json");

/// The catalogue rows of `ops` (`object`, `verb`) as the operation contract (`contracts/opdefs.json`) defines them:
/// the contract's own side effect, inputs and outputs, so a read is a read and `payment.refund` / `order.cancel` are X.
fn op_rows(ops: &[(&str, &str)]) -> Vec<OperationRow> {
    let defs: Vec<Value> = serde_json::from_str(OPDEFS).expect("opdefs.json");
    ops.iter()
        .map(|&(object, verb)| {
            let def = defs
                .iter()
                .find(|d| d["object"] == object && d["verb"] == verb && d["variant"] == STAR)
                .unwrap_or_else(|| panic!("{object}.{verb} is not in contracts/opdefs.json"));
            let side_effect = def["side_effect"].as_str().expect("side_effect");
            let attrs = json!({
                "side_effect": side_effect,
                "identity_level": def["identity_level"],
                "modality": if side_effect == "R" { "observe" } else { "do" },
                "input_fields": def["input_fields"],
                "output_fields": def["output_fields"],
            });
            OperationRow {
                attrs: attrs.as_object().cloned().unwrap(),
                key: op_key(object, verb, STAR),
                object: object.to_owned(),
                verb: verb.to_owned(),
                variant: STAR.to_owned(),
                discriminator: None,
                overridden: Vec::new(),
                inherited: Vec::new(),
                description: String::new(),
                retail: None,
                stages: Vec::new(),
                error_states: Vec::new(),
            }
        })
        .collect()
}

/// The operations of the refund world: reads of orders and payments, and the two side-effecting X writes.
const REFUND_WORLD_OPS: [(&str, &str); 5] = [
    ("order", "read"),
    ("order", "list"),
    ("payment", "list"),
    ("payment", "refund"),
    ("order", "cancel"),
];

/// The own space of a refund-status question.
const REFUNDS: &str = "retail:order-support:refunds";

/// Plan test inventory: `refund_status_never_calls_payment_refund`. A known customer's "where is my refund" in a
/// job of its own (one `refunds` space, a design whose catalogue knows `payment.refund` and `order.cancel` as X
/// writes, the built-in `refund_status` procedure declared) is driven through the orchestrator with a fake runner
/// that answers every call: the procedure's own steps run and the case ends resolved / not_eligible WITHOUT a write,
/// and no `op.call` of `payment.refund` is ever published (as a step or as a context read). Layer: the orchestrator.
#[tokio::test]
async fn refund_status_never_calls_payment_refund() {
    let fx = own_fx(
        "kcp-refund-status",
        "refund_status_desk",
        &[OwnSpace {
            key: REFUNDS,
            slots: &["order_id"],
            kinds: &[],
        }],
        false,
        Env::Test,
    )
    .await;
    let mut design = {
        let mut tx = fx.d.f.job.begin(&fx.d.e.state.db).await.unwrap();
        let d = RuntimeDesign::load(
            &mut tx,
            &fx.d.e.state.design,
            &fx.d.f.job_ref(),
            &DesignOf::Draft,
        )
        .await
        .unwrap_or_else(|e| panic!("load: {e:?}"));
        tx.commit().await.unwrap();
        d
    };
    assert!(
        design.spaces.by_key(REFUNDS).is_some(),
        "the job has its own refunds space"
    );
    design.catalog = PinnedCatalog::from_rows(&op_rows(&REFUND_WORLD_OPS));
    for x in ["payment.refund", "order.cancel"] {
        assert_eq!(
            design.catalog.get(x).map(|d| d.side_effect),
            Some(SideEffect::X),
            "{x} is a side-effecting X operation of the design (the premise)"
        );
    }
    design.procedures = vec![TemplateProcedure {
        name: "refund_status".into(),
        version: 1,
        label: "Refund status".into(),
        uses_slots: Vec::new(),
        steps: 1,
        step_defs: Vec::new(),
        success_criteria: Vec::new(),
        tool_requirements: Vec::new(),
        context_space: None,
        builtin_version: Some(1),
        tenant: false,
    }];
    let cache = Arc::new(DesignCache::default());
    cache.prime(&fx.scope, Arc::new(design));

    fl::insert_conversation(&fx, "conv-CS-1").await;
    fl::insert_case(&fx, "CS-1", &current_design(&fx).await).await;
    fl::identify_conversation(&fx, "conv-CS-1", "cust-1", None).await;
    let req: QualifiedRequest = serde_json::from_value(json!({
        "case_id": "CS-1", "conversation_id": "conv-CS-1", "area": AREA,
        "context_space": REFUNDS, "request_type": "refunds", "slots": {"order_id": "ORD-1"},
        "customer_id": "cust-1", "identity_level": 2, "message_id": "msg-CS-1"
    }))
    .unwrap();
    let o = Orchestrator::new(
        fx.bus.clone(),
        fx.rt.clone(),
        Arc::clone(&fx.d.e.state.design),
        cache,
        StaticProcedures,
    )
    .unwrap()
    .with_consumer("orchestrator-refund-status")
    .with_consume(r::fast())
    .with_routes(Arc::new(EveryOpOffered));
    o.on_qualified(qualified(&req)).await.unwrap();

    let mut done: HashSet<String> = HashSet::new();
    for _ in 0..30 {
        if !fl::resolutions_in(&fx, AREA, "CS-1").await.is_empty() {
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
            let out = match c.operation.as_str() {
                "order.list" => {
                    json!({"order_id": ["ORD-1"], "status": ["delivered"], "placed_at": ["2026-10-20"]})
                }
                "order.read" => {
                    json!({"status": "delivered", "customer_id": "cust-1", "total": 59.0,
                                       "lines": [{"name": "Jute rug", "qty": 1}]})
                }
                "payment.list" => {
                    json!({"payment_id": "PAY-1", "order_id": "ORD-1", "status": "captured"})
                }
                _ => json!({}),
            };
            o.on_result(fl::answer(&c, ResultState::Ok, "ok", out))
                .await
                .unwrap();
        }
    }

    let rs = fl::resolutions_in(&fx, AREA, "CS-1").await;
    assert_eq!(rs.len(), 1, "{rs:?}");
    assert!(
        matches!(
            rs[0].outcome,
            CaseOutcome::Resolved | CaseOutcome::NotEligible
        ),
        "never a handover: {rs:?}"
    );
    assert!(!rs[0].wrote, "a status question wrote nothing: {rs:?}");
    let calls = fl::calls(&fx, "CS-1").await;
    assert!(
        calls.iter().any(|c| !is_ctx(c)),
        "the procedure's own steps ran: {calls:?}"
    );
    let refunds: Vec<&OpCall> = calls
        .iter()
        .filter(|c| c.operation == "payment.refund")
        .collect();
    assert!(
        refunds.is_empty(),
        "refund_status published payment.refund: {refunds:?}"
    );
    let writes: Vec<&OpCall> = calls
        .iter()
        .filter(|c| c.operation == "payment.refund" || c.operation == "order.cancel")
        .collect();
    assert!(
        writes.is_empty(),
        "refund_status published a write: {writes:?}"
    );
    assert!(!contains_id_pattern(&reply::compose(&rs[0])));
}

fn activity(key: &str, ops: &[(&str, bool)]) -> BaseActivity {
    BaseActivity {
        key: key.to_owned(),
        item: serde_json::Map::new(),
        ops: ops
            .iter()
            .map(|&(op, required)| ActivityOp {
                op: op.to_owned(),
                variant: None,
                required,
            })
            .collect(),
    }
}

/// Plan test inventory: `assistant_tools_exclude_x_ops_for_refund_and_cancel_activities`. A design of its own: a
/// read-only `support:refund_status` activity and a `change:request_refund` and `change:cancel_order` activity that
/// expect the side-effect X operations `payment.refund` / `order.cancel` (the assistant role even lists grants for
/// them). The assistant role's maximum is W, so with the role ENABLED (platform row and job switch on, so the check
/// is not vacuous) every X row of those activities is above the max and not granted, and the model's tool list
/// (`tools::offered` through `DesignToolsets`, the one source the runner's gate also asks) holds no X or P tool and
/// is not empty. Layer: the real toolsets and offer over the operation contract's own side effects.
#[test]
#[allow(clippy::field_reassign_with_default)]
fn assistant_tools_exclude_x_ops_for_refund_and_cancel_activities() {
    let rows = op_rows(&REFUND_WORLD_OPS);
    let mut d = RuntimeDesign::blank(
        JobRef {
            id: uuid::Uuid::new_v4(),
            key: "refund_desk".to_owned(),
        },
        DesignOf::Draft,
    );
    d.catalog = PinnedCatalog::from_rows(&rows);

    let grant = |object: &str, verb: &str, modality: &str| json!({"object": object, "verb": verb, "variant": null, "modalities": [modality]});
    let assistant = json!({
        "label": "Assistant", "max_side_effect": "W", "enabled": true,
        "grants": [grant("order", "read", "observe"), grant("payment", "list", "observe"),
                   grant("payment", "refund", "do"), grant("order", "cancel", "do")],
    });
    let mut bases = Bases::default();
    bases.agent_roles = vec![CatalogueItem {
        key: "assistant".to_owned(),
        row_id: "r".to_owned(),
        layer: "base".to_owned(),
        fields: assistant.as_object().cloned().unwrap(),
    }];
    bases.operations = rows;
    bases.activities = vec![
        activity(
            "support:refund_status",
            &[("order.read", true), ("payment.list", false)],
        ),
        activity(
            "change:request_refund",
            &[
                ("order.read", true),
                ("payment.list", false),
                ("payment.refund", false),
            ],
        ),
        activity(
            "change:cancel_order",
            &[("order.read", true), ("order.cancel", false)],
        ),
    ];
    let settings = BTreeMap::from([(
        "assistant".to_owned(),
        RoleSetting {
            enabled: true,
            max_tool_calls: None,
            turn_budget_s: None,
            system_prompt: None,
            case_per_thread: false,
        },
    )]);
    let toolsets = Toolsets::build(&bases, Vec::<(String, String, String)>::new(), settings);
    d.job_design = Arc::new(JobDesign {
        toolsets: toolsets.clone(),
        ..JobDesign::default()
    });

    let activities: Vec<String> = toolsets
        .activities()
        .filter(|a| a.contains("refund") || a.contains("cancel"))
        .map(str::to_owned)
        .collect();
    for named in [
        "support:refund_status",
        "change:request_refund",
        "change:cancel_order",
    ] {
        assert!(
            activities.iter().any(|a| a == named),
            "{named} is an activity of the design: {activities:?}"
        );
    }

    let mut x_rows_seen = 0;
    for act in &activities {
        let rows = toolsets
            .rows("assistant", act)
            .unwrap_or_else(|m| panic!("{act}: {m:?}"));
        for row in rows.iter().filter(|r| r.side_effect == SideEffect::X) {
            x_rows_seen += 1;
            assert!(
                row.above_max && !row.granted,
                "{act}: the X operation {} is granted to the assistant",
                row.label
            );
        }
        let tools = offered(&d.catalog, &d, &DesignToolsets, "assistant", Some(act));
        assert!(
            !tools.is_empty(),
            "{act}: the enabled assistant is offered nothing, so the check proves nothing"
        );
        for t in &tools {
            assert!(
                t.side_effect != "X" && t.side_effect != "P",
                "{act}: the model is offered the side-effect {} tool {}",
                t.side_effect,
                t.op_key
            );
        }
        for x in ["payment.refund", "order.cancel"] {
            assert!(
                !tools.iter().any(|t| t.op_key == x),
                "{act}: {x} is a tool of the assistant"
            );
        }
    }
    assert!(
        x_rows_seen > 0,
        "the refund and cancel activities expect an X operation (payment.refund, order.cancel): {activities:?}"
    );
    // the read-only activity is still useful to the model: it is offered the payment read
    let status = offered(
        &d.catalog,
        &d,
        &DesignToolsets,
        "assistant",
        Some("support:refund_status"),
    );
    assert!(
        status.iter().any(|t| t.op_key == "payment.list"),
        "{status:?}"
    );
}

// ==== 8 ===========================================================================================================

/// A resolution with the defaults a test does not care about, overridden by `v`.
fn res(v: Value) -> Resolution {
    let mut base = json!({
        "case_id": "CS-12", "conversation_id": "conv-12",
        "context_space": "retail:order-support:returns", "outcome": "resolved"
    });
    base.as_object_mut()
        .unwrap()
        .extend(v.as_object().unwrap().clone());
    serde_json::from_value(base).unwrap()
}

/// Plan test inventory: `no_reply_contains_an_id` (owner ruling 2026-10-26). Every reply-producing path reachable
/// from outside the crate holds no record id and asks for no key: `reply::compose` over every outcome x request
/// type x code with facts FULL of ids (and without), the choice question for retail picks, the record choice
/// question and the no-match sentence of every registered record kind, the not-eligible sentences of every known
/// code, and the fixed sentences. Layer: pure reply composition.
#[test]
fn no_reply_contains_an_id() {
    let ids = json!({
        "resolved": {"item_label": "Jute rug ordered on 12 October", "order_id": "RET-01",
                     "item_names": {"RET-01": "Jute rug", "RET-02": "kettle"}},
        "order": {"status": "in_transit", "item": "kettle", "order_id": "RET-01", "lines": []},
        "shipment": {"status": "in_transit", "carrier": "PostNL", "shipment_id": "SHP-9102",
                     "eta": "2026-10-21", "last_event": "Sorted", "delivered_at": "2026-10-07"},
        "delivery": {"status": "in_transit", "carrier": "PostNL", "tracking_ref": "SHP-9102",
                     "delivery_id": "SHP-9102", "expected_window": {"until": "2026-10-23"}},
        "report": {"case_id": "DP-7"},
        "rma": {"rma_id": "RMA-9", "carrier": "DHL"},
        "claim": {"claim_id": "WAR-3", "next_step": "We will contact you."},
        "rules": {"refund_payment_id": "PAY-1001"},
        "refund": {"refund_id": "REF-77", "amount": "42.90", "eta": "5 days"},
        "payments": {"payment_id": "PAY-1006", "status": "captured",
                     "payments": [{"payment_id": "PAY-1006", "direction": "charge", "status": "captured"},
                                  {"payment_id": "REF-77", "direction": "refund", "status": "pending"}]},
        "returns": {"return_id": ["RT-9"], "order_id": ["RET-01"], "status": ["open"],
                    "created_at": ["2026-10-20"]},
    });
    let types = [
        "",
        "return",
        "warranty_claim",
        "order_status",
        "delivery_status",
        "delivery_problem",
        "track_parcel",
        "payment_error",
        "refunds",
        "refund_status",
        "cancel_order",
        "refund_request",
        "redirect",
        "faq",
    ];
    let outcomes = [
        "resolved",
        "not_eligible",
        "failed",
        "handover",
        "identity_required",
        "confirmation_required",
        "cancelled_offboard",
    ];
    let codes = [
        None,
        Some("choice_required"),
        Some("no_matching_order"),
        Some("no_matching_record"),
        Some("step_up_required"),
        Some("not_owner"),
        Some("timeout"),
        Some("ineligible:RET-01"),
    ];
    let mut texts: Vec<String> = Vec::new();
    for t in types {
        for o in outcomes {
            for code in codes {
                for wrote in [true, false] {
                    for facts in [ids.clone(), json!({})] {
                        // `choice_required` carries the question itself as its reason (composed from the candidates
                        // by `resolve::choice_question`, checked below); every other code's reason is internal
                        let reason = if code == Some("choice_required") {
                            "Which of your orders do you mean?"
                        } else {
                            "order.read RET-01 failed"
                        };
                        let mut v = json!({"request_type": t, "outcome": o, "facts": facts,
                                           "wrote": wrote, "reason": reason});
                        if let Some(c) = code {
                            v["code"] = json!(c);
                        }
                        texts.push(reply::compose(&res(v)));
                    }
                }
            }
        }
    }

    // the choice questions: retail picks (with and without products / dates, with and without older ones)
    let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 12);
    let pick = |id: &str, items: &[&str], placed_on| Pick {
        order_id: id.to_owned(),
        payment_id: Some("PAY-1".into()),
        delivery_id: Some("SHP-1".into()),
        items: items.iter().map(|s| (*s).to_owned()).collect(),
        placed_on,
    };
    let picks = [
        pick("RET-01", &["Jute rug"], day),
        pick("RET-02", &["Ceramic lamp", "Linen throw", "Wool rug"], day),
        pick("RET-03", &[], None),
    ];
    for n in 1..=picks.len() {
        for more in [false, true] {
            texts.push(resolve::choice_question(&picks[..n], more));
        }
    }
    // the record choice questions, the no-match sentences, for every registered kind (and an id as the reason)
    for k in RECORD_KINDS {
        let records = vec![
            pick("POL-0042", &["Car insurance"], day),
            pick("POL-0043", &[], None),
        ];
        for more in [false, true] {
            texts.push(resolve::record_choice_question(k, &records, more));
        }
        texts.push(reply::no_matching_record(Some(k.noun)));
    }
    texts.push(reply::no_matching_record(Some("POL-0042")));
    texts.push(reply::no_matching_record(None));
    // the not-eligible sentences of every known code, and of a connector's own
    for code in [
        "ineligible:RET-01",
        "ineligible:WAR-01",
        "ineligible:PAY-04",
        "ineligible:DIS-02",
        "ineligible:ARR-01",
        "not_cancellable",
    ] {
        texts.push(
            reply::not_eligible_by_code(Some(code))
                .unwrap_or_else(|| panic!("{code} has a sentence"))
                .to_owned(),
        );
    }
    // the fixed sentences
    for c in [
        reply::CHECKED,
        reply::NO_PAYMENT,
        reply::FAQ_FALLBACK,
        reply::NOT_ELIGIBLE,
        reply::NO_TRACKING,
        reply::NO_MATCHING_ORDER,
        reply::CHOICE_FALLBACK,
    ] {
        texts.push(c.to_owned());
    }

    assert!(
        texts.len() > 3000,
        "every path was composed: {}",
        texts.len()
    );
    for text in &texts {
        assert!(!contains_id_pattern(text), "an id in: {text}");
        let low = text.to_lowercase();
        for w in [
            "order number",
            "order id",
            "postcode",
            "postal code",
            "tracking number",
            "your email",
            "customer id",
            "reference number",
        ] {
            assert!(!low.contains(w), "`{w}` in: {text}");
        }
    }
}

// ==== 9 ===========================================================================================================

/// The test's own catalogue of context spaces: neutral keys and the slot names a real job uses, chosen to cover every
/// kind of slot the invariant is about: record ids and refs (`*_id`, `*_ref`), contact and address keys (`email`,
/// `postcode`, `msisdn`), a mix of a key and a plain slot, plain slots only, and no slot at all.
const OWN_SPACES: &[(&str, &[&str])] = &[
    ("retail:order-support:order_status", &["order_id"]),
    ("retail:order-support:returns", &["order_id", "reason"]),
    ("retail:order-support:warranty", &["order_id", "fault"]),
    ("retail:logistics:track_parcel", &["tracking_ref"]),
    ("retail:payments:refunds", &["order_id", "payment_id"]),
    (
        "retail:order-support:delivery_problem",
        &["order_id", "delivery_id"],
    ),
    (
        "billing:charges:dispute_a_charge",
        &["account_ref", "document_ref"],
    ),
    (
        "billing:payments:direct_debit_change",
        &["account_ref", "change_type"],
    ),
    (
        "customer_service:complaints:complaint_intake",
        &["complaint_summary", "desired_outcome"],
    ),
    ("customer_service:privacy:data_rights", &["subject"]),
    ("insurance:claims:claim_status", &["policy_id", "claim_ref"]),
    (
        "healthcare:appointments:appointment_change",
        &["appointment_ref", "preferred_time"],
    ),
    ("telecom:mobile:plan_change", &["msisdn", "plan"]),
    ("services:contact:update_details", &["email", "postcode"]),
    (
        "customer_service:handover:handover_to_person",
        &["handover_reason"],
    ),
    ("customer_service:faq:opening_hours", &[]),
];

/// Whole words (lowercase) that would ask the customer for a lookup key.
const ASKING_WORDS: [&str; 9] = [
    "number",
    "reference",
    "ref",
    "id",
    "postcode",
    "postal",
    "zip",
    "identifier",
    "phone",
];

fn own_catalog() -> SpaceCatalog {
    SpaceCatalog::new(
        OWN_SPACES
            .iter()
            .map(|(key, slots)| ContextSpace {
                key: (*key).to_owned(),
                label: String::new(),
                description: String::new(),
                area: key.split(':').nth(1).unwrap_or_default().to_owned(),
                knowledge_kinds: vec![],
                slots: slots.iter().map(|s| (*s).to_owned()).collect(),
                activity: String::new(),
            })
            .collect(),
    )
}

fn words_of(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

fn assert_asks_for_no_key(what: &str, text: &str) {
    assert!(!contains_id_pattern(text), "{what}: an id in: {text}");
    let words = words_of(text);
    for k in ASKING_WORDS {
        assert!(
            !words.iter().any(|w| w == k),
            "{what}: asks for a key (`{k}`): {text}"
        );
    }
}

/// Plan test inventory: `no_prompt_asks_for_a_lookup_key`. Over every space of the test's own catalogue ([`OWN_SPACES`]):
/// (a) a slot named like a lookup key (`*_id`, `*_ref`, `*_number`, `email`, `postcode`, `msisdn` ...) is
/// `never_asked`, and a typed value for it is not kept, so the qualifier fold never produces `Ask` for one; (b) every
/// question the qualifier can still ask (`Outcome::Ask`) and the question text of every slot of every space name no
/// key and no id; (c) `unclear()` (the menu) holds no id and asks for no key; (d) the extraction prompt (system text)
/// asks for none and the slots it lists for the model exclude every `never_asked` slot. Layer: `qualify::step`,
/// `question`, `unclear`, `body`.
#[test]
fn no_prompt_asks_for_a_lookup_key() {
    let cat = own_catalog();
    assert_eq!(cat.spaces().len(), OWN_SPACES.len());
    let key_tokens = [
        "id",
        "number",
        "ref",
        "reference",
        "email",
        "postcode",
        "phone",
        "msisdn",
        "iban",
    ];

    for (space, slots) in OWN_SPACES {
        for slot in *slots {
            // (a) a slot that is a key by name is never asked
            if slot.split('_').any(|t| key_tokens.contains(&t)) {
                assert!(
                    never_asked(slot),
                    "{space}: `{slot}` is a key by name but may be asked"
                );
            }
            // (b) the text of the question for every slot
            assert_asks_for_no_key(
                &format!("{space} / {slot}"),
                &qualify::question(slot, space),
            );
        }

        // the fold: the customer types an id for every never-asked slot and nothing else
        let typed = Extraction {
            context_space: Some((*space).to_owned()),
            slots: slots
                .iter()
                .filter(|s| never_asked(s))
                .map(|s| ((*s).to_owned(), "ZZ-99871".to_owned()))
                .collect(),
        };
        let (out, kept) = qualify::step(None, typed, &cat, "m-typed");
        let kept_slots = match (&out, kept) {
            (Outcome::Ask(s), Some(d)) => {
                assert!(!never_asked(s), "{space}: asked for the key {s}");
                assert_asks_for_no_key(&format!("{space} / ask"), &qualify::question(s, space));
                d.slots
            }
            (Outcome::Ready(d), _) => d.slots.clone(),
            other => panic!("{space}: unexpected {other:?}"),
        };
        for slot in slots.iter().filter(|s| never_asked(s)) {
            assert!(
                !kept_slots.contains_key(*slot),
                "{space}: a typed id was kept as `{slot}`"
            );
        }
    }

    // (c) the menu of an unclear turn
    let menu = qualify::unclear(&cat);
    assert_asks_for_no_key("unclear()", &menu);
    assert_asks_for_no_key(
        "unclear() without spaces",
        &qualify::unclear(&SpaceCatalog::default()),
    );

    // (d) the extraction prompt
    let body = qualify::body(
        "m",
        "where is my parcel",
        None::<&Draft>,
        &cat,
        &BTreeMap::new(),
        &SystemMessage::default(),
    );
    let system = body["messages"][0]["content"]
        .as_str()
        .expect("one system message")
        .to_lowercase();
    for phrase in [
        "order number",
        "order id",
        "postcode",
        "postal code",
        "tracking number",
        "your email",
        "email address",
        "customer id",
        "reference number",
    ] {
        assert!(
            !system.contains(phrase),
            "the extraction prompt mentions `{phrase}`"
        );
    }
    assert!(
        system.contains("never asked"),
        "the prompt says slots are not asked: {system}"
    );
    let user: Value = serde_json::from_str(
        body["messages"][1]["content"]
            .as_str()
            .expect("user content"),
    )
    .unwrap();
    let listed = user["context_spaces"].as_array().expect("context_spaces");
    assert_eq!(listed.len(), OWN_SPACES.len());
    for s in listed {
        for slot in s["slots"].as_array().expect("slots") {
            let slot = slot.as_str().unwrap();
            assert!(
                !never_asked(slot),
                "{}: the model is told to extract the key `{slot}`",
                s["key"]
            );
        }
    }
}
