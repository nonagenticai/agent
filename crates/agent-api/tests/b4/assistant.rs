//! B4 T7a — the `assistant` role runtime (crate::runtime::assistant, F-40, plan R14): an orchestrator with an
//! assistant attached plans a request without procedure as an assistant case; each tool call the model makes runs
//! through the real runner (steps 1–11) and its answer goes back to the model. Real Postgres 18 + Redis, both the
//! orchestrator and the runner connected as `agent_runtime`, an OpenAI-compatible fake model on 127.0.0.1 (the
//! platform-owned tenant's internal `qualify` binding) that answers scripted replies — tool calls, then a final
//! message — and keeps every request body. Executors are registered fakes that count their calls.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_api::design::DesignOf;
use agent_api::runtime::assistant::tools::ToolsetIndex;
use agent_api::runtime::assistant::{Assistant, Budget, RoleSwitch};
use agent_api::runtime::catalog::OperationCatalog;
use agent_api::runtime::design::{DesignCache, RuntimeDesign};
use agent_api::runtime::domain::{
    CaseOutcome, OpResult, Phase, ProcedureRef, QualifiedRequest, Resolution,
};
use agent_api::runtime::orchestrator::{Orchestrator, ProcedureSource, StaticProcedures};
use agent_api::runtime::routes::ResolvedTarget;
use agent_api::runtime::runner::Runner;
use agent_api::runtime::runner::authorise::required_level;
use agent_api::runtime::runner::ops::{
    Executors, OpExecutor, OpOutcome, OpRequest, OpState, outcome,
};
use agent_api::runtime::{Env, Envelope, Scope};
use agent_model::kinds::catalog::SideEffect;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use crate::b4::design::{self as d, load, put_target};
use crate::b4::flow::{
    AREA, Fx, calls, current_design, insert_case, orch, qualified, resolutions, state_events,
};
use crate::common::{self, insert_user, person};
use crate::redis_env as r;

/// A context space of the job's area that no procedure serves (`procedure_for_space` knows no `anything_else`).
const FREE_SPACE: &str = "retail:order-support:anything_else";

// ---- the fake model ---------------------------------------------------------------------------------------------

/// An OpenAI-compatible `POST /v1/chat/completions`: answers the scripted messages in order (then a final
/// "All done."), keeps every request body.
struct Llm {
    url: String,
    script: Arc<Mutex<VecDeque<Value>>>,
    seen: Arc<Mutex<Vec<Value>>>,
}

impl Llm {
    async fn start() -> Llm {
        let script: Arc<Mutex<VecDeque<Value>>> = Arc::default();
        let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
        let (sc, se) = (script.clone(), seen.clone());
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                let (sc, se) = (sc.clone(), se.clone());
                async move {
                    se.lock().unwrap().push(body);
                    let msg = sc
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or_else(|| final_msg("All done."));
                    axum::Json(json!({
                        "choices": [{"message": msg}],
                        "usage": {"prompt_tokens": 20, "completion_tokens": 5},
                    }))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(l, app).await.unwrap();
        });
        Llm {
            url: format!("http://{addr}/v1"),
            script,
            seen,
        }
    }

    fn script(&self, msgs: Vec<Value>) {
        self.script.lock().unwrap().extend(msgs);
    }

    fn requests(&self) -> Vec<Value> {
        self.seen.lock().unwrap().clone()
    }
}

/// An assistant message calling `calls` (`(function name, arguments)`), ids `c0`, `c1`, ….
fn calls_msg(calls: &[(&str, Value)]) -> Value {
    let tc: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, args))| {
            json!({"id": format!("c{i}"), "type": "function",
                   "function": {"name": name, "arguments": args.to_string()}})
        })
        .collect();
    json!({"role": "assistant", "content": null, "tool_calls": tc})
}

fn final_msg(text: &str) -> Value {
    json!({"role": "assistant", "content": text})
}

/// The `tool` messages of a request body, as `(tool_call_id, parsed content)`.
fn tool_answers(body: &Value) -> Vec<(String, Value)> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| {
            (
                m["tool_call_id"].as_str().unwrap().to_owned(),
                serde_json::from_str(m["content"].as_str().unwrap()).unwrap(),
            )
        })
        .collect()
}

/// The function names a request body offered.
fn offered(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

// ---- fixtures ---------------------------------------------------------------------------------------------------

struct Ax {
    fx: Fx,
    llm: Llm,
}

/// A job of its own tenant with the target selection saved (the demo operations are in its catalogue), the fake
/// model as `INTERNAL_LLM_URL`; `platform` = the tenant is platform-owned (its `qualify` use resolves to the
/// internal model). A test-env scope on a fresh Redis prefix and the `agent_runtime` pool.
async fn setup(test: &str, platform: bool) -> Ax {
    let llm = Llm::start().await;
    let url = llm.url.clone();
    let e = common::env_with(|v| {
        v.insert("INTERNAL_LLM_URL".into(), url);
    })
    .await;
    let f = crate::b4::support::new_job(&e).await;
    crate::b4::support::route_prod(&e, &f).await;
    crate::b4::support::route_test(&e, &f).await;
    if platform {
        sqlx::query("UPDATE tenants SET platform_owned = true WHERE id = $1")
            .bind(f.tenant)
            .execute(&e.owner)
            .await
            .unwrap();
    }
    let sub = format!("u-b4-assistant-{}", f.tenant.simple());
    insert_user(&e.owner, &sub, f.tenant, "tenant_admin").await;
    let tok = person(&sub, &[]);
    let dfx = d::Fx { e, f, tok };
    put_target(&dfx).await;
    let root = r::root(test).await;
    let scope = Scope::new(dfx.f.tenant, dfx.f.job.job_key.clone(), Env::Test).unwrap();
    let bus = root.scoped(&scope);
    let rt = common::runtime_pool(&dfx.e.owner).await;
    Ax {
        fx: Fx {
            d: dfx,
            root,
            scope,
            bus,
            rt,
        },
        llm,
    }
}

/// The switch of the tests: on for every activity.
struct On;

impl RoleSwitch for On {
    fn assistant_enabled(&self, _d: &RuntimeDesign, _activity: &str) -> bool {
        true
    }
}

/// A toolset granting the `assistant` role exactly these operation keys.
struct Grants(Vec<String>);

impl ToolsetIndex for Grants {
    fn allows(&self, _d: &RuntimeDesign, role: &str, _activity: Option<&str>, op: &str) -> bool {
        role == "assistant" && self.0.iter().any(|g| g.as_str() == op)
    }
}

/// The scope's state on the runtime pool (what the runner and the assistant run with).
fn runtime_state(fx: &Fx) -> agent_api::state::AppState {
    let mut st = fx.d.e.state.clone();
    st.db = fx.rt.clone();
    st
}

/// An assistant switched on, granting `grants`.
fn assistant(fx: &Fx, grants: &[&str]) -> Assistant {
    Assistant::new(runtime_state(fx))
        .with_switch(Arc::new(On))
        .with_toolset(Arc::new(Grants(
            grants.iter().map(|s| (*s).to_owned()).collect(),
        )))
}

/// A procedure source with no procedure for any request.
struct NoProcedures;

impl ProcedureSource for NoProcedures {
    fn plan(
        &self,
        _design: &RuntimeDesign,
        _req: &QualifiedRequest,
    ) -> Option<(Option<ProcedureRef>, Vec<Phase>)> {
        None
    }
}

fn orchestrator(fx: &Fx, a: Assistant) -> Orchestrator<NoProcedures> {
    orch(fx, NoProcedures).with_assistant(Arc::new(a))
}

/// A registered executor: counts its calls and answers `out`.
struct Fake {
    hits: AtomicUsize,
    out: OpOutcome,
}

impl Fake {
    fn ok(output: Value) -> Arc<Fake> {
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

    fn hits(&self) -> usize {
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

fn runner(fx: &Fx, x: Executors) -> Runner {
    Runner::new(
        fx.bus.clone(),
        runtime_state(fx),
        Arc::new(DesignCache::default()),
    )
    .unwrap()
    .with_consumer("assistant-test-runner")
    .with_consume(r::fast())
    .with_executors(x)
}

/// The staff test conversation `conv-<case>` (started by `u-staff-assist`) with one inbound customer message; its
/// id (a write's `confirm_ref`).
async fn conversation(fx: &Fx, case: &str) -> String {
    let msg = unknown_conversation(fx, case).await;
    // A1/A2: the customer is known through conversation STATE (`subject` only, no identity_source: no known-customer context preload), which a level-1+ op requires
    crate::common::state::put(
        &fx.d.e.owner,
        fx.d.f.tenant,
        fx.d.f.job.job_id,
        &format!("conv-{case}"),
        "subject",
        "C-1",
    )
    .await;
    msg
}

/// [`conversation`] of a caller nothing is known about (no `subject` in state).
async fn unknown_conversation(fx: &Fx, case: &str) -> String {
    let (t, j) = (fx.d.f.tenant, fx.d.f.job.job_id);
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id, initiator_sub) \
         VALUES ($1, $2, $3, $4, 'staff', 'C-1', 'u-staff-assist')",
    )
    .bind(t)
    .bind(j)
    .bind(format!("conv-{case}"))
    .bind(fx.scope.env.as_str())
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO conversation_message (tenant_id, job_id, conversation_id, role, text, at_ms) \
         VALUES ($1, $2, $3, 'customer', 'where is my order ORD-1?', 1) RETURNING message_id::text",
    )
    .bind(t)
    .bind(j)
    .bind(format!("conv-{case}"))
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap()
}

/// A request in [`FREE_SPACE`] at `identity` (0..=3), qualified from inbound message `msg` (`None`: no
/// confirmation for a write).
fn request(case: &str, identity: u8, msg: Option<&str>) -> QualifiedRequest {
    serde_json::from_value(json!({
        "case_id": case, "conversation_id": format!("conv-{case}"), "area": AREA, "context_space": FREE_SPACE,
        "request_type": "anything_else", "slots": {"order_id": "ORD-1"}, "customer_id": "C-1",
        "identity_level": identity, "message_id": msg
    }))
    .unwrap()
}

/// The case row (stamped now) and its `request.qualified` delivered to `o`.
async fn start<P: ProcedureSource>(fx: &Fx, o: &Orchestrator<P>, req: &QualifiedRequest) {
    insert_case(fx, &req.case_id, &current_design(fx).await).await;
    o.on_qualified(qualified(req)).await.unwrap();
}

fn result_env(res: &OpResult) -> Envelope {
    Envelope::new("op.result", "runner", &res.case_id, res).unwrap()
}

/// Run every `op.call` of `case` through the runner and deliver each `op.result` to `o`, until no new call comes.
async fn drive<P: ProcedureSource>(fx: &Fx, o: &Orchestrator<P>, rn: &Runner, case: &str) {
    let mut done = 0;
    loop {
        let cs = calls(fx, case).await;
        if cs.len() == done {
            return;
        }
        for c in &cs[done..] {
            let res = rn.handle(c).await.unwrap();
            o.on_result(result_env(&res)).await.unwrap();
        }
        done = cs.len();
    }
}

/// The one resolution of `case`.
async fn resolution(fx: &Fx, case: &str) -> Resolution {
    let rs = resolutions(fx, case).await;
    assert_eq!(rs.len(), 1, "one request.resolved: {rs:?}");
    rs.into_iter().next().unwrap()
}

/// `(operation, state, outcome_code, initiator)` of the case's `operation_call` rows, oldest first.
async fn op_rows(fx: &Fx, case: &str) -> Vec<(String, String, Option<String>, String)> {
    sqlx::query_as(
        "SELECT operation, state, outcome_code, initiator FROM operation_call \
          WHERE tenant_id = $1 AND case_id = $2 ORDER BY started_at, call_id",
    )
    .bind(fx.d.f.tenant)
    .bind(case)
    .fetch_all(&fx.d.e.owner)
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

/// `(status, outcome)` of the case row.
async fn status(fx: &Fx, case: &str) -> (String, Option<String>) {
    sqlx::query_as("SELECT status, outcome FROM agent_case WHERE tenant_id = $1 AND case_id = $2")
        .bind(fx.d.f.tenant)
        .bind(case)
        .fetch_one(&fx.d.e.owner)
        .await
        .unwrap()
}

/// A non-built-in operation of the job's catalogue with side effect `effect` that a level-3 case may run.
async fn op_with(fx: &Fx, effect: SideEffect) -> String {
    let design = load(&fx.d, DesignOf::Draft).await;
    let c = &design.catalog;
    c.keys()
        .into_iter()
        .find(|k| {
            c.get(k).is_some_and(|def| {
                def.side_effect == effect
                    && !def.builtin
                    && required_level(def.identity_level).is_some_and(|l| l <= 3)
            })
        })
        .unwrap_or_else(|| panic!("the catalogue has a {effect:?} operation: {:?}", c.keys()))
        .to_owned()
}

/// The function name the loop offers for `op_key` (crate::runtime::assistant::tools::tool_name).
fn fname(op_key: &str) -> String {
    agent_api::runtime::assistant::tools::tool_name(op_key)
}

// ---- tests ------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn assistant_off_by_default_keeps_procedure_path() {
    let ax = setup("t7a-off", true).await;
    let fx = &ax.fx;
    // attached but not switched on: B4's `Disabled`
    let o = orch(fx, StaticProcedures).with_assistant(Arc::new(Assistant::new(runtime_state(fx))));
    // a request a procedure serves runs its phases as before
    conversation(fx, "CS-1").await;
    let mut req = request("CS-1", 2, None);
    req.context_space = crate::b4::flow::SPACE.into();
    req.request_type = "return".into();
    start(fx, &o, &req).await;
    let cs = calls(fx, "CS-1").await;
    assert_eq!(cs.len(), 1, "the procedure's first phase");
    assert_eq!(cs[0].operation, "order.read");
    assert_eq!(cs[0].agent_role, "procedure");
    // a request no procedure serves is handed over out_of_scope, as before
    conversation(fx, "CS-2").await;
    start(fx, &o, &request("CS-2", 2, None)).await;
    let res = resolution(fx, "CS-2").await;
    assert_eq!(res.outcome, CaseOutcome::Handover);
    assert_eq!(res.code.as_deref(), Some("not_offered"));
    assert_eq!(handovers(fx, "CS-2").await[0].0, "out_of_scope");
    assert!(ax.llm.requests().is_empty(), "no model call");

    // control: switched on, the same request runs the assistant (one model call, then resolved)
    let o = orchestrator(fx, assistant(fx, &[]));
    conversation(fx, "CS-3").await;
    start(fx, &o, &request("CS-3", 2, None)).await;
    let res = resolution(fx, "CS-3").await;
    assert_eq!(res.outcome, CaseOutcome::Resolved, "{res:?}");
    assert_eq!(res.facts["assistant"]["answer"], "All done.");
    assert_eq!(ax.llm.requests().len(), 1);
    assert!(handovers(fx, "CS-3").await.is_empty());
}

#[tokio::test]
async fn assistant_tool_call_goes_through_runner_checks() {
    let ax = setup("t7a-identity", true).await;
    let fx = &ax.fx;
    let design = load(&fx.d, DesignOf::Draft).await;
    let need = design
        .catalog
        .get("order.read")
        .map(|d| required_level(d.identity_level))
        .expect("order.read is in the catalogue");
    assert!(
        need.is_some_and(|l| l > 0),
        "order.read needs an identity level above 0: {need:?}"
    );
    let read = Fake::ok(json!({"status": "delivered"}));
    let rn = runner(
        fx,
        Executors::default().register("order.read", read.clone()),
    );
    let o = orchestrator(fx, assistant(fx, &["order.read"]));
    ax.llm.script(vec![
        calls_msg(&[(
            "order_read",
            json!({"order_id": "ORD-1", "not_an_input": 1}),
        )]),
        final_msg("Please tell me who you are first."),
    ]);
    // A2: the refusal is the missing STATE key (`subject`), not the call's identity level; a conversation nothing is
    // known about has no subject (was `identity_required`)
    unknown_conversation(fx, "CS-1").await;
    start(fx, &o, &request("CS-1", 0, None)).await;
    let cs = calls(fx, "CS-1").await;
    assert_eq!(cs.len(), 1, "{cs:?}");
    assert_eq!(cs[0].agent_role, "assistant");
    assert!(
        cs[0].initiator.starts_with("assistant@"),
        "{}",
        cs[0].initiator
    );
    assert_eq!(cs[0].identity_level, 0);
    assert_eq!(
        Value::Object(cs[0].input.clone()),
        json!({"order_id": "ORD-1"}),
        "only the declared inputs"
    );
    assert!(cs[0].idempotency_key.is_none(), "a read");
    drive(fx, &o, &rn, "CS-1").await;
    assert_eq!(read.hits(), 0, "the system was not called");
    assert_eq!(
        op_rows(fx, "CS-1").await,
        [(
            "order.read".to_owned(),
            "denied".to_owned(),
            Some("state_required".to_owned()),
            cs[0].initiator.clone()
        )]
    );
    let reqs = ax.llm.requests();
    assert_eq!(reqs.len(), 2, "the refusal went back to the model");
    assert_eq!(offered(&reqs[0]), ["order_read"]);
    assert_eq!(
        tool_answers(&reqs[1]),
        [("c0".to_owned(), json!({"error": "state_required"}))]
    );
    // the customer's words reach the model; the system message is the first and only one
    let msgs = reqs[0]["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs.iter().filter(|m| m["role"] == "system").count(), 1);
    let user: Value = serde_json::from_str(msgs[1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(user["request"]["context_space"], FREE_SPACE);
    let res = resolution(fx, "CS-1").await;
    assert_eq!(res.outcome, CaseOutcome::Resolved);

    // control: a conversation whose customer is known (`subject` in state) gets the same call run
    ax.llm.script(vec![
        calls_msg(&[("order_read", json!({"order_id": "ORD-1"}))]),
        final_msg("It was delivered."),
    ]);
    conversation(fx, "CS-2").await;
    start(fx, &o, &request("CS-2", 3, None)).await;
    drive(fx, &o, &rn, "CS-2").await;
    assert_eq!(read.hits(), 1);
    let reqs = ax.llm.requests();
    assert_eq!(
        tool_answers(&reqs[3]),
        [("c0".to_owned(), json!({"status": "delivered"}))]
    );
    assert_eq!(resolution(fx, "CS-2").await.outcome, CaseOutcome::Resolved);
}

#[tokio::test]
async fn assistant_w_call_without_confirm_ref_is_confirmation_required() {
    let ax = setup("t7a-confirm", true).await;
    let fx = &ax.fx;
    let w = op_with(fx, SideEffect::W).await;
    let write = Fake::ok(json!({"ok": true}));
    let rn = runner(fx, Executors::default().register(&w, write.clone()));
    let wf = fname(&w);
    let o = orchestrator(fx, assistant(fx, &[w.as_str()]));
    ax.llm.script(vec![
        calls_msg(&[(wf.as_str(), json!({}))]),
        final_msg("Shall I go ahead?"),
    ]);
    conversation(fx, "CS-1").await;
    // no inbound message: no confirm_ref
    start(fx, &o, &request("CS-1", 3, None)).await;
    let cs = calls(fx, "CS-1").await;
    assert_eq!(cs.len(), 1);
    assert!(cs[0].confirm_ref.is_none());
    assert!(cs[0].idempotency_key.is_some(), "a write carries its key");
    drive(fx, &o, &rn, "CS-1").await;
    assert_eq!(write.hits(), 0, "nothing was written");
    let rows = op_rows(fx, "CS-1").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].1.as_str(), rows[0].2.as_deref()),
        ("denied", Some("confirmation_required"))
    );
    let reqs = ax.llm.requests();
    assert_eq!(
        tool_answers(&reqs[1]),
        [("c0".to_owned(), json!({"error": "confirmation_required"}))]
    );
    assert_eq!(resolution(fx, "CS-1").await.outcome, CaseOutcome::Resolved);

    // control: the inbound message confirms the same call
    ax.llm.script(vec![
        calls_msg(&[(wf.as_str(), json!({}))]),
        final_msg("Done."),
    ]);
    let msg = conversation(fx, "CS-2").await;
    start(fx, &o, &request("CS-2", 3, Some(&msg))).await;
    assert_eq!(
        calls(fx, "CS-2").await[0].confirm_ref.as_deref(),
        Some(msg.as_str())
    );
    drive(fx, &o, &rn, "CS-2").await;
    assert_eq!(write.hits(), 1);
    assert_eq!(op_rows(fx, "CS-2").await[0].1, "ok");
}

#[tokio::test]
async fn assistant_respects_per_case_cap() {
    let ax = setup("t7a-cap", true).await;
    let fx = &ax.fx;
    let refund = Fake::ok(json!({"refund_id": "RF-1"}));
    let rn = runner(
        fx,
        Executors::default().register("payment.refund", refund.clone()),
    );
    let o = orchestrator(fx, assistant(fx, &["payment.refund"]));
    let args = json!({"payment_id": "PAY-1", "amount": 10});
    ax.llm.script(vec![
        calls_msg(&[("payment_refund", args.clone())]),
        calls_msg(&[("payment_refund", args)]),
        final_msg("never asked"),
    ]);
    let msg = conversation(fx, "CS-1").await;
    start(fx, &o, &request("CS-1", 3, Some(&msg))).await;
    drive(fx, &o, &rn, "CS-1").await;
    assert_eq!(refund.hits(), 1, "the system saw one refund");
    let states: Vec<(String, Option<String>)> = op_rows(fx, "CS-1")
        .await
        .into_iter()
        .map(|(_, s, c, _)| (s, c))
        .collect();
    assert_eq!(
        states,
        [
            ("ok".to_owned(), None),
            ("denied".to_owned(), Some("case_cap_reached".to_owned()))
        ]
    );
    let res = resolution(fx, "CS-1").await;
    assert_eq!(res.outcome, CaseOutcome::Handover, "{res:?}");
    assert_eq!(res.code.as_deref(), Some("case_cap_reached"));
    assert_eq!(
        handovers(fx, "CS-1").await,
        [(
            "tool_failed".to_owned(),
            "per-case limit reached for payment.refund".to_owned()
        )]
    );
    assert_eq!(ax.llm.requests().len(), 2, "no model call after the cap");
}

#[tokio::test]
async fn assistant_cannot_call_an_op_outside_its_toolset() {
    let ax = setup("t7a-toolset", true).await;
    let fx = &ax.fx;
    let refund = Fake::ok(json!({"refund_id": "RF-1"}));
    let rn = runner(
        fx,
        Executors::default().register("payment.refund", refund.clone()),
    );
    let o = orchestrator(fx, assistant(fx, &["order.read"]));
    ax.llm.script(vec![
        calls_msg(&[(
            "payment_refund",
            json!({"payment_id": "PAY-1", "amount": 10}),
        )]),
        final_msg("I can't do that here."),
    ]);
    let msg = conversation(fx, "CS-1").await;
    start(fx, &o, &request("CS-1", 3, Some(&msg))).await;
    drive(fx, &o, &rn, "CS-1").await;
    assert!(calls(fx, "CS-1").await.is_empty(), "no op.call was sent");
    assert!(op_rows(fx, "CS-1").await.is_empty());
    assert_eq!(refund.hits(), 0);
    let reqs = ax.llm.requests();
    assert_eq!(offered(&reqs[0]), ["order_read"], "only the granted tool");
    assert_eq!(
        tool_answers(&reqs[1]),
        [("c0".to_owned(), json!({"error": "not_entitled"}))]
    );
    assert_eq!(resolution(fx, "CS-1").await.outcome, CaseOutcome::Resolved);
}

#[tokio::test]
async fn assistant_budget_exhausted_hands_over() {
    let ax = setup("t7a-budget", true).await;
    let fx = &ax.fx;
    let read = Fake::ok(json!({"status": "delivered"}));
    let rn = runner(
        fx,
        Executors::default().register("order.read", read.clone()),
    );
    let o = orchestrator(fx, assistant(fx, &["order.read"]));
    let read_call = || ("order_read", json!({"order_id": "ORD-1"}));
    // 4 calls, then 3 more: the 7th exceeds the 6 of one turn
    ax.llm.script(vec![
        calls_msg(&[read_call(), read_call(), read_call(), read_call()]),
        calls_msg(&[read_call(), read_call(), read_call()]),
    ]);
    conversation(fx, "CS-1").await;
    start(fx, &o, &request("CS-1", 3, None)).await;
    drive(fx, &o, &rn, "CS-1").await;
    assert_eq!(
        calls(fx, "CS-1").await.len(),
        4,
        "the second round was not sent"
    );
    assert_eq!(read.hits(), 4);
    let res = resolution(fx, "CS-1").await;
    assert_eq!(res.outcome, CaseOutcome::Handover, "{res:?}");
    assert_eq!(res.code.as_deref(), Some("assistant_budget_exhausted"));
    assert_eq!(
        handovers(fx, "CS-1").await,
        [(
            "tool_failed".to_owned(),
            "assistant tool-call budget (6) exhausted".to_owned()
        )]
    );
    assert_eq!(
        status(fx, "CS-1").await,
        (
            "handover".to_owned(),
            Some("assistant_budget_exhausted".to_owned())
        )
    );

    // the turn's time budget: a round that starts after it hands over before calling the model
    let o = orchestrator(
        fx,
        assistant(fx, &["order.read"]).with_budget(Budget {
            max_tool_calls: 6,
            turn: Duration::from_millis(1),
        }),
    );
    ax.llm.script(vec![calls_msg(&[read_call()])]);
    conversation(fx, "CS-2").await;
    let before = ax.llm.requests().len();
    start(fx, &o, &request("CS-2", 3, None)).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    drive(fx, &o, &rn, "CS-2").await;
    assert_eq!(ax.llm.requests().len(), before + 1, "only the first round");
    let res = resolution(fx, "CS-2").await;
    assert_eq!(res.code.as_deref(), Some("assistant_budget_exhausted"));
    assert_eq!(handovers(fx, "CS-2").await[0].0, "tool_failed");
}

#[tokio::test]
async fn assistant_unbound_llm_hands_over_out_of_scope() {
    // not platform-owned, nothing bound to `qualify`, no inference-router licence: fail closed, no model, a person
    // takes the case (the platform key is never a substitute)
    let ax = setup("t7a-unbound", false).await;
    let fx = &ax.fx;
    let o = orchestrator(fx, assistant(fx, &["order.read"]));
    conversation(fx, "CS-1").await;
    start(fx, &o, &request("CS-1", 3, None)).await;
    let res = resolution(fx, "CS-1").await;
    assert_eq!(res.outcome, CaseOutcome::Handover, "{res:?}");
    assert_eq!(res.code.as_deref(), Some("llm_unavailable"));
    assert_eq!(
        handovers(fx, "CS-1").await,
        [(
            "tool_failed".to_owned(),
            "the tenant's language model access is not provisioned".to_owned()
        )]
    );
    assert!(ax.llm.requests().is_empty(), "no model call");
    assert!(calls(fx, "CS-1").await.is_empty());
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_event WHERE tenant_id = $1")
        .bind(fx.d.f.tenant)
        .fetch_one(&fx.d.e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0, "nothing metered");
    assert_eq!(status(fx, "CS-1").await.0, "handover");
}

#[tokio::test]
async fn assistant_usage_row_per_llm_call() {
    let ax = setup("t7a-usage", true).await;
    let fx = &ax.fx;
    let read = Fake::ok(json!({"status": "delivered"}));
    let rn = runner(
        fx,
        Executors::default().register("order.read", read.clone()),
    );
    let o = orchestrator(fx, assistant(fx, &["order.read"]));
    ax.llm.script(vec![
        calls_msg(&[("order_read", json!({"order_id": "ORD-1"}))]),
        final_msg("It was delivered."),
    ]);
    conversation(fx, "CS-1").await;
    start(fx, &o, &request("CS-1", 3, None)).await;
    drive(fx, &o, &rn, "CS-1").await;
    assert_eq!(resolution(fx, "CS-1").await.outcome, CaseOutcome::Resolved);
    // KH T5: the assistant's system message starts with the job's layers, and round 0 records its markers on a
    // `state` event (a record prod writes too)
    let reqs = ax.llm.requests();
    // the test world's only instruction text is the base layer's job preamble: its marker opens the message
    let first = reqs[0]["messages"][0]["content"]
        .as_str()
        .unwrap_or_default();
    let marker = first.lines().next().unwrap_or_default().to_owned();
    assert!(marker.starts_with("[layer base@v"), "{first}");
    for r in &reqs {
        let system = r["messages"][0]["content"].as_str().unwrap_or_default();
        assert!(system.starts_with(&marker), "{system}");
        assert!(system.contains("\n\n[task assistant]\n"), "{system}");
    }
    let prompts: Vec<Value> = state_events(fx, "CS-1")
        .await
        .into_iter()
        .filter(|b| b["note"] == "assistant prompt")
        .collect();
    assert_eq!(prompts.len(), 1, "one per case (round 0): {prompts:?}");
    assert!(
        prompts[0]["layers"]
            .as_array()
            .is_some_and(|l| l.iter().any(|m| *m == marker)),
        "{prompts:?}"
    );
    let rows: Vec<(
        String,
        String,
        String,
        String,
        String,
        i64,
        i64,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT source, use, actor, provider_id, funding, tokens_in, tokens_out, case_id \
               FROM usage_event WHERE tenant_id = $1 AND job_id = $2 ORDER BY id",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .fetch_all(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(
        rows.len(),
        ax.llm.requests().len(),
        "one row per model call"
    );
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(
            row,
            &(
                "staff_test".to_owned(),
                "assistant".to_owned(),
                "u-staff-assist".to_owned(),
                "internal".to_owned(),
                "local_gpu".to_owned(),
                20,
                5,
                Some("CS-1".to_owned())
            )
        );
    }
}
