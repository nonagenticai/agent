//! B4 T9a — `probe_turn` (crate::turn::probe): one test-env answer for B8's draft validation, with no side effects.
//! Real Postgres 18 + Redis, the real router for the fixtures, a job created from `job/customer_care_agent@v4` with
//! its target selection saved (the pinned template compiles: spaces, procedures, the demo operations), the tenant's
//! stub instance on the platform template `test_shop@1` (B7's executor answers `order.read` from its fixtures),
//! and an OpenAI-compatible fake on 127.0.0.1 as the tenant's internal (platform-owned, D70) `qualify` model.
use crate::common::platform;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_api::design::CaseDesign;
use agent_api::runtime::routes::{DEMO_STUB_INSTANCE, ResolvedTarget};
use agent_api::runtime::runner::ops::{
    ConnectorExecutor, Executors, OpExecutor, OpOutcome, OpRequest, OpState, outcome,
};
use agent_api::turn::probe::{self, Probe, ProbeOpts};
use axum::http::StatusCode;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

use crate::common::design_contract::{self, ContractJob};
use crate::common::{self, TestEnv, json as req, send};
use crate::redis_env as r;

/// An order-status request for RET-01 (one message completes it): `order_status@1` = one `order.read`.
const ORDER_STATUS: &str =
    r#"{"context_space": "retail:order-support:order_status", "slots": {"order_id": "RET-01"}}"#;

/// A returns request with both slots: `return_request` = `order.read` → `rule.evaluate{RET-01}` (gate) →
/// `case.create{return}` (a write).
const RETURN: &str = r#"{"context_space": "retail:order-support:returns", "slots": {"order_id": "RET-01", "reason": "too big"}}"#;

const RETURN_OP: &str = "case.create{return}";

/// What the fake `qualify` model answers when it recognises no space at all (2026-11-07 live: it did this for the
/// same close request on one turn and the space on another).
const NO_SPACE: &str = r#"{"context_space": null, "slots": {}}"#;

/// An unclear answer that still carries a slot: the typed lookup key the fold refuses for a live request and a staff
/// dry run keeps.
const NO_SPACE_TYPED: &str = r#"{"context_space": null, "slots": {"order_id": "RET-01"}}"#;

/// An OpenAI-compatible `POST /v1/chat/completions` that records every body and answers `content` after `delay`.
struct Llm {
    url: String,
    bodies: Arc<Mutex<Vec<Value>>>,
    /// Each call's `X-Effective-Tenant-Id` (agent#71; `None` when absent).
    tenants: Arc<Mutex<Vec<Option<String>>>>,
    /// Each call's `X-App-Id`, `X-Project-Id` and `X-Actor-Id` (the router identity; `None` when absent).
    ids: Arc<Mutex<Vec<[Option<String>; 3]>>>,
}

impl Llm {
    fn calls(&self) -> usize {
        self.bodies.lock().unwrap().len()
    }

    /// The one system message of call `i`.
    fn system(&self, i: usize) -> String {
        self.bodies.lock().unwrap()[i]["messages"][0]["content"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }
}

async fn fake_llm(content: &'static str, delay: Duration) -> Llm {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let tenants = Arc::new(Mutex::new(Vec::new()));
    let ids: Arc<Mutex<Vec<[Option<String>; 3]>>> = Arc::default();
    let seen = bodies.clone();
    let named = tenants.clone();
    let identified = ids.clone();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(
            move |h: axum::http::HeaderMap, axum::Json(v): axum::Json<Value>| {
                let seen = seen.clone();
                let named = named.clone();
                let identified = identified.clone();
                async move {
                    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(String::from);
                    identified.lock().unwrap().push([
                        get("x-app-id"),
                        get("x-project-id"),
                        get("x-actor-id"),
                    ]);
                    named.lock().unwrap().push(
                        h.get("x-effective-tenant-id")
                            .and_then(|t| t.to_str().ok())
                            .map(String::from),
                    );
                    seen.lock().unwrap().push(v);
                    tokio::time::sleep(delay).await;
                    axum::Json(json!({
                        "choices": [{"message": {"role": "assistant", "content": content}}],
                        "usage": {"prompt_tokens": 9, "completion_tokens": 5},
                    }))
                }
            },
        ),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app).await.unwrap();
    });
    Llm {
        url: format!("http://{addr}/v1"),
        bodies,
        tenants,
        ids,
    }
}

struct Fx {
    e: TestEnv,
    f: ContractJob,
    llm: Llm,
    /// A release manager (`tenant_admin`) of the tenant.
    tok: String,
    /// The state's `AGENT_PREFIX`: every Redis key it writes starts with it.
    prefix: String,
}

async fn setup(name: &str, content: &'static str, delay: Duration) -> Fx {
    setup_with(name, content, delay, &[]).await
}

/// [`setup`] with extra API variables.
async fn setup_with(
    name: &str,
    content: &'static str,
    delay: Duration,
    extra: &[(&str, String)],
) -> Fx {
    let llm = fake_llm(content, delay).await;
    let vars = r::api_vars(name).await;
    let prefix = vars[1].1.clone();
    let url = llm.url.clone();
    let e = common::env_with(|v| {
        v.extend(vars.clone());
        v.insert("INTERNAL_LLM_URL".into(), url);
        for (k, val) in extra {
            v.insert((*k).to_string(), val.clone());
        }
    })
    .await;
    let f = crate::b4::support::new_job(&e).await;
    crate::b4::support::route_prod(&e, &f).await;
    sqlx::query("UPDATE tenants SET platform_owned = true WHERE id = $1")
        .bind(f.tenant)
        .execute(&e.owner)
        .await
        .unwrap();
    platform::stub_shop(&e, f.tenant, DEMO_STUB_INSTANCE).await;
    // the operations the plans of this file name: the order read, and the case create a returns plan requires
    // (its `case.create{return}` write is only predicted, never sent)
    crate::b4::support::route_instance(
        &e,
        &f,
        DEMO_STUB_INSTANCE,
        &[("order", "read"), ("case", "create")],
    )
    .await;
    let tok = design_contract::release_manager(&e, &f).await;
    let fx = Fx {
        e,
        f,
        llm,
        tok,
        prefix,
    };
    put_target(&fx).await;
    fx
}

fn uri(fx: &Fx, tail: &str) -> String {
    format!("/api/jobs/{}/{tail}", fx.f.job.job_key)
}

async fn call(fx: &Fx, method: &str, tail: &str, body: Value) -> (StatusCode, Value) {
    send(&fx.e.app, req(method, &uri(fx, tail), Some(&fx.tok), body)).await
}

/// Save the selection (the first save).
async fn put_target(fx: &Fx) {
    let body =
        json!({"selection": design_contract::cuttable_selection(), "expected_updated_at": null});
    let (s, b) = call(fx, "PUT", "target", body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn draft(fx: &Fx) -> CaseDesign {
    let draft_rev: i64 = sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
        .bind(fx.f.job.job_id)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap();
    CaseDesign::Draft { draft_rev }
}

async fn run(fx: &Fx, design: CaseDesign, text: &str, opts: &ProbeOpts) -> Probe {
    probe::probe_turn_with(
        &fx.e.state,
        &fx.f.job.t,
        &fx.f.job_ref(),
        design,
        text,
        opts,
    )
    .await
    .unwrap_or_else(|e| panic!("probe: {:?}", e.status_and_code()))
}

/// Every runtime table B4 writes (and the audit and outbox rows), per tenant.
const COUNTS: [&str; 14] = [
    "SELECT count(*) FROM agent_case WHERE tenant_id = $1",
    "SELECT count(*) FROM case_event WHERE tenant_id = $1",
    "SELECT count(*) FROM conversation WHERE tenant_id = $1",
    "SELECT count(*) FROM conversation_message WHERE tenant_id = $1",
    "SELECT count(*) FROM conversation_control WHERE tenant_id = $1",
    "SELECT count(*) FROM operation_call WHERE tenant_id = $1",
    "SELECT count(*) FROM operation_call_check WHERE tenant_id = $1",
    "SELECT count(*) FROM agent_handover_items WHERE tenant_id = $1",
    "SELECT count(*) FROM agent_handover_events WHERE tenant_id = $1",
    "SELECT count(*) FROM answer_grounding WHERE tenant_id = $1",
    "SELECT count(*) FROM case_csat WHERE tenant_id = $1",
    "SELECT count(*) FROM audit_event WHERE tenant_id = $1",
    "SELECT count(*) FROM metering_outbox WHERE tenant_id = $1",
    "SELECT count(*) FROM usage_event WHERE tenant_id = $1",
];

async fn counts(fx: &Fx) -> Vec<i64> {
    let mut out = Vec::new();
    for sql in COUNTS {
        let n: i64 = sqlx::query_scalar(sql)
            .bind(fx.f.tenant)
            .fetch_one(&fx.e.owner)
            .await
            .unwrap();
        out.push(n);
    }
    out
}

async fn usage_rows(fx: &Fx) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM usage_event WHERE tenant_id = $1")
        .bind(fx.f.tenant)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap()
}

/// The state's Redis keys (its per-test prefix), sorted.
async fn redis_keys(fx: &Fx) -> Vec<String> {
    let bus = fx.e.state.redis.clone().expect("REDIS_URL set");
    let mut keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{}*", fx.prefix))
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    keys.sort();
    keys
}

/// An executor answering `out` and counting its calls.
struct Fixed {
    out: Map<String, Value>,
    hits: Arc<AtomicUsize>,
}

impl OpExecutor for Fixed {
    fn execute<'a>(
        &'a self,
        _req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move {
            self.hits.fetch_add(1, Ordering::SeqCst);
            outcome(OpState::Ok, None, None, self.out.clone())
        })
    }
}

fn fixed(out: Value, hits: &Arc<AtomicUsize>) -> Arc<dyn OpExecutor> {
    Arc::new(Fixed {
        out: out.as_object().cloned().unwrap_or_default(),
        hits: Arc::clone(hits),
    })
}

/// The tenant's own order-status procedure for the order-status space: one `order.read`. It shadows the template's
/// `order_status@2` (which also reads the delivery, an operation this world does not route), as a tenant's published
/// procedure does.
async fn own_order_status(fx: &Fx) {
    let step = json!({
        "step_number": 1, "phase": 0, "name": "order", "operation": "order.read", "variant": null,
        "modality": "observe", "side_effect": "R", "identity_level": "2", "confirm_back": false,
        "gate": false, "args": {"order_id": "$order_id"},
    });
    let mut at = agent_api::design::DraftRef::new();
    at.insert("name".into(), json!("order_status_read"));
    design_contract::tracked_set(
        &fx.e,
        &fx.f,
        "procedure",
        &at,
        Some(json!([{
            "version": 1, "status": "PUBLISHED", "context_space": "retail:order-support:order_status",
            "description": "Read the order", "reviewers": [], "released_in": null, "pddl_domain": null,
            "steps": [step],
            "success_criteria": [{"id": "order", "fact": "steps.order.outcome", "op": "eq", "value": "ok"}],
            "tool_requirements": ["order.read"],
        }])),
    )
    .await;
}

#[tokio::test]
async fn probe_turn_sends_no_write() {
    let fx = setup("t9a-nowrite", ORDER_STATUS, Duration::ZERO).await;
    own_order_status(&fx).await;
    let before = counts(&fx).await;
    let keys = redis_keys(&fx).await;
    let design = draft(&fx).await;
    let p = run(
        &fx,
        design,
        "Where is my order RET-01?",
        &ProbeOpts::new(&fx.e.state),
    )
    .await;
    // it answered from the stub's order.read, as the staff test chat does
    assert_eq!(p.ran, ["order.read"], "{p:?}");
    assert!(p.would_call.is_empty(), "{p:?}");
    assert!(!p.handover, "{p:?}");
    assert!(p.reply.starts_with("Your order is "), "{p:?}");
    let mut after = counts(&fx).await;
    // the one write is the usage row (probe_emits_one_usage_row); every other table is untouched
    let usage = after.pop().unwrap();
    let mut was = before;
    assert_eq!(usage, was.pop().unwrap() + 1);
    assert_eq!(after, was, "no runtime row is written");
    assert_eq!(redis_keys(&fx).await, keys, "no Redis key is written");
}

#[tokio::test]
async fn probe_reports_would_call_for_w_ops() {
    let fx = setup("t9a-wouldcall", RETURN, Duration::ZERO).await;
    let rule_hits = Arc::new(AtomicUsize::new(0));
    let write_hits = Arc::new(AtomicUsize::new(0));
    let instance: Arc<dyn OpExecutor> = Arc::new(ConnectorExecutor::new(fx.e.state.clone()));
    let mut opts = ProbeOpts::new(&fx.e.state);
    // the gate passes (so the plan reaches its write); the write's executor counts every call it gets
    opts.executors = Executors::default()
        .with_instance(instance)
        .register(
            "rule.evaluate{RET-01}",
            fixed(json!({"eligible": true, "outcome": "eligible"}), &rule_hits),
        )
        .register(
            RETURN_OP,
            fixed(json!({"case_id": "RMA-1", "status": "open"}), &write_hits),
        );
    let before = counts(&fx).await;
    let design = draft(&fx).await;
    let p = run(&fx, design, "I want to return RET-01, it is too big", &opts).await;
    assert_eq!(p.ran, ["order.read", "rule.evaluate{RET-01}"], "{p:?}");
    assert_eq!(p.would_call, [RETURN_OP], "{p:?}");
    assert_eq!(rule_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        write_hits.load(Ordering::SeqCst),
        0,
        "a write is never sent"
    );
    assert!(!p.reply.is_empty());
    let mut after = counts(&fx).await;
    after.pop();
    let mut was = before;
    was.pop();
    assert_eq!(after, was, "no operation_call (or any runtime) row");
}

/// Parity with the runtime (acceptance defect 2026-10-21): a gate rule that cannot decide predicts a handover, and
/// the rule's internal reason is never the predicted reply.
#[tokio::test]
async fn probe_undecided_rule_predicts_handover() {
    let fx = setup("t9a-undecided", RETURN, Duration::ZERO).await;
    let rule_hits = Arc::new(AtomicUsize::new(0));
    let write_hits = Arc::new(AtomicUsize::new(0));
    let instance: Arc<dyn OpExecutor> = Arc::new(ConnectorExecutor::new(fx.e.state.clone()));
    let mut opts = ProbeOpts::new(&fx.e.state);
    let reason =
        "RET-01 has no value for values: the agent cannot decide and hands the case to a person.";
    opts.executors = Executors::default()
        .with_instance(instance)
        .register(
            "rule.evaluate{RET-01}",
            fixed(
                json!({"eligible": false, "outcome": "placeholder", "code": "placeholder", "reason": reason}),
                &rule_hits,
            ),
        )
        .register(
            RETURN_OP,
            fixed(json!({"case_id": "RMA-1", "status": "open"}), &write_hits),
        );
    let design = draft(&fx).await;
    let p = run(&fx, design, "I want to return RET-01, it is too big", &opts).await;
    assert_eq!(rule_hits.load(Ordering::SeqCst), 1, "{p:?}");
    assert!(p.handover, "an undecided rule hands over: {p:?}");
    assert!(p.would_call.is_empty(), "no write after the gate: {p:?}");
    assert!(!p.reply.is_empty());
    assert!(
        !p.reply.contains("has no value"),
        "internal reason leaked: {}",
        p.reply
    );
}

#[tokio::test]
async fn probe_against_release_ignores_draft() {
    let fx = setup("t9a-release", ORDER_STATUS, Duration::ZERO).await;
    // R1 is approved and applied; then the draft edits the stance (nothing published)
    let (s, b) = call(&fx, "PUT", "languages", json!({"languages": ["en", "de"]})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(&fx.e.app, common::get(&uri(&fx, "target"), Some(&fx.tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let at = b["selection_updated_at"].clone();
    let body = json!({"apply": true, "selection_seen_at": at});
    let (s, b) = call(&fx, "POST", "releases", body).await;
    assert!(s == StatusCode::CREATED || s == StatusCode::OK, "{s}: {b}");
    assert_eq!(b["applied"], true, "{b}");
    let r1 = b["release"]["id"].as_str().expect("release id").to_string();
    const EDITED: &str = "warm, brief (T9a draft edit)";
    let (s, b) = call(&fx, "PUT", "settings", json!({"stance": EDITED})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let opts = ProbeOpts::new(&fx.e.state);
    let text = "Where is my order RET-01?";

    let live = CaseDesign::Live {
        release_id: Some(r1),
    };
    run(&fx, live, text, &opts).await;
    let design = draft(&fx).await;
    run(&fx, design, text, &opts).await;
    assert_eq!(fx.llm.calls(), 2);
    assert!(
        !fx.llm.system(0).contains(EDITED),
        "the release's probe never sees the draft: {}",
        fx.llm.system(0)
    );
    assert!(
        fx.llm.system(1).contains(&format!("stance: {EDITED}")),
        "the draft's probe does: {}",
        fx.llm.system(1)
    );
    // a release that is not the live one is refused, before any model call
    let stale = CaseDesign::Live {
        release_id: Some("rel-not-live".into()),
    };
    let err = probe::probe_turn(&fx.e.state, &fx.f.job.t, &fx.f.job_ref(), stale, text)
        .await
        .unwrap_err();
    assert_eq!(
        err.status_and_code(),
        (StatusCode::CONFLICT, "design_changed")
    );
    assert_eq!(fx.llm.calls(), 2);
}

#[tokio::test]
async fn probe_emits_one_usage_row() {
    let fx = setup("t9a-usage", ORDER_STATUS, Duration::ZERO).await;
    let design = draft(&fx).await;
    let p = run(
        &fx,
        design,
        "Where is my order RET-01?",
        &ProbeOpts::new(&fx.e.state),
    )
    .await;
    assert_eq!(fx.llm.calls(), 1);
    // the internal (platform-owned, `local_gpu`) qualify call names the billed tenant (agent#71): guards
    // turn/qualify.rs `model` (the internal arm's `r.effective_tenant`) and `extract` (the header line)
    assert_ne!(fx.f.tenant, uuid::Uuid::nil());
    assert_eq!(
        *fx.llm.tenants.lock().unwrap(),
        [Some(fx.f.tenant.to_string())]
    );
    let rows: Vec<(String, String, String, String, String, i64, i64, i32, bool)> = sqlx::query_as(
        "SELECT source, use, actor, provider_id, funding, tokens_in, tokens_out, calls, \
                    conversation_id IS NULL AND case_id IS NULL \
               FROM usage_event WHERE tenant_id = $1",
    )
    .bind(fx.f.tenant)
    .fetch_all(&fx.e.owner)
    .await
    .unwrap();
    let actor = fx.f.job.t.caller.sub.clone();
    assert_eq!(
        rows,
        [(
            "validation".to_string(),
            "qualify".to_string(),
            actor,
            "internal".to_string(),
            "local_gpu".to_string(),
            9,
            5,
            1,
            true
        )]
    );
    assert_eq!(
        (p.usage.calls, p.usage.tokens_in, p.usage.tokens_out),
        (1, 9, 5)
    );
}

// Guards turn/probe.rs `turn` (`r.acting_for(Some(caller.sub))`): the staff member running the probe is the
// router's X-Actor-Id, not agent's service account; a sub that is no uuid falls back to `SERVICE_ACTOR_ID`.
#[tokio::test]
async fn probe_names_the_acting_person_to_the_router() {
    let service = uuid::Uuid::new_v4();
    let fx = setup_with(
        "t9a-actor",
        ORDER_STATUS,
        Duration::ZERO,
        &[("SERVICE_ACTOR_ID", service.to_string())],
    )
    .await;
    let design = draft(&fx).await;
    let person = uuid::Uuid::new_v4();
    let mut acting = fx.f.job.t.clone();
    acting.caller.sub = person.to_string();
    let opts = ProbeOpts::new(&fx.e.state);
    probe::probe_turn_with(
        &fx.e.state,
        &acting,
        &fx.f.job_ref(),
        design.clone(),
        "Where is my order RET-01?",
        &opts,
    )
    .await
    .unwrap_or_else(|e| panic!("probe: {:?}", e.status_and_code()));
    // the fixture's caller (`u-designer`) has no uuid sub: the service account
    run(&fx, design, "Where is my order RET-01?", &opts).await;
    let project = agent_api::providers::llm::tenant_level_project_id(fx.f.tenant).to_string();
    let id = |actor: uuid::Uuid| {
        [
            Some("agent".to_string()),
            Some(project.clone()),
            Some(actor.to_string()),
        ]
    };
    assert_eq!(*fx.llm.ids.lock().unwrap(), [id(person), id(service)]);
}

#[tokio::test]
async fn probe_over_budget_is_503() {
    let fx = setup("t9a-budget", ORDER_STATUS, Duration::from_secs(6)).await;
    let mut opts = ProbeOpts::new(&fx.e.state);
    opts.budget = Duration::from_secs(2);
    assert_eq!(probe::BUDGET, Duration::from_secs(20), "B8-5");
    let before = counts(&fx).await;
    let design = draft(&fx).await;
    let err = probe::probe_turn_with(
        &fx.e.state,
        &fx.f.job.t,
        &fx.f.job_ref(),
        design,
        "Where is my order RET-01?",
        &opts,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.status_and_code(),
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
    );
    // the model was called (the time went there) and nothing is stored: not even its usage
    assert_eq!(fx.llm.calls(), 1);
    assert_eq!(counts(&fx).await, before);
    assert_eq!(usage_rows(&fx).await, 0);
}

/// A where-is-my-parcel request: `track_parcel` (as `order_status@1`) = one `delivery.read{by_tracking_ref}`.
const TRACK: &str =
    r#"{"context_space": "retail:logistics:track_parcel", "slots": {"tracking_ref": "RET-01"}}"#;

const BY_REF: &str = "delivery.read{by_tracking_ref}";

/// An instance executor that says which operation serves a call and counts the calls that reach it.
struct Serves {
    hits: Arc<AtomicUsize>,
    served: agent_api::runtime::runner::ops::Served,
}

impl OpExecutor for Serves {
    fn execute<'a>(
        &'a self,
        _req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move {
            self.hits.fetch_add(1, Ordering::SeqCst);
            let mut out = Map::new();
            out.insert("status".into(), json!("in_transit"));
            outcome(OpState::Ok, None, None, out)
        })
    }

    fn served<'a>(
        &'a self,
        _req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, Result<Option<agent_api::runtime::runner::ops::Served>, String>> {
        Box::pin(std::future::ready(Ok(Some(self.served.clone()))))
    }
}

/// Runner step 6b in the probe ([`agent_api::runtime::runner::served_refusal`], the one function both call): a
/// variant served by the system's order-keyed `delivery.read` (`*`), whose `order_id` the call does not carry, is
/// never sent to the executor — the probe predicts the live hand-over. The control: the same served operation with
/// nothing missing is called. (A probe runs at the staff-picked level 2, so the identity branch of the same function
/// is covered by tests/b4/runner.rs.) Mutation: dropping the probe's check sends the call (hits 1).
#[tokio::test]
async fn probe_never_calls_a_fallback_the_variant_cannot_serve() {
    let fx = setup("t9a-served", TRACK, Duration::ZERO).await;
    // the selection adds the logistics area (as Demo Retail has), so the qualifier can pick track_parcel
    let (s, b) = call(&fx, "GET", "target", Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut selection = design_contract::cuttable_selection();
    selection["areas"] = json!(["order-support", "logistics"]);
    let (s, b) = call(
        &fx,
        "PUT",
        "target",
        json!({"selection": selection, "expected_updated_at": b["selection_updated_at"]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut tx = fx.f.job.begin(&fx.e.state.db).await.unwrap();
    let d = agent_api::runtime::design::RuntimeDesign::load(
        &mut tx,
        &fx.e.state.design,
        &fx.f.job_ref(),
        &agent_api::design::DesignOf::Draft,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        d.spaces
            .by_key("retail:logistics:track_parcel")
            .map(|s| s.area.as_str()),
        Some("logistics"),
        "track_parcel is selectable"
    );
    sqlx::query(
        "INSERT INTO operation_route (tenant_id, job_id, object, verb, variant, env, target_kind, instance_id, updated_by) \
         VALUES ($1, $2, 'delivery', 'read', '*', 'test', 'instance', $3, 'u-test')",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(DEMO_STUB_INSTANCE)
    .execute(&fx.e.owner)
    .await
    .unwrap();
    // the control first: with nothing missing the call is sent — proof the probe qualified into track_parcel
    // (only its plan runs this variant), with the same model answer and design as the refused case
    for (needs, sent) in [(Vec::new(), 1), (vec![vec!["order_id".to_owned()]], 0)] {
        let hits = Arc::new(AtomicUsize::new(0));
        let mut opts = ProbeOpts::new(&fx.e.state);
        opts.executors = Executors::default().with_instance(Arc::new(Serves {
            hits: Arc::clone(&hits),
            served: agent_api::runtime::runner::ops::Served {
                op_key: "delivery.read".to_owned(),
                needs: needs.clone(),
            },
        }));
        let design = draft(&fx).await;
        let p = run(&fx, design, "Where is my order RET-01?", &opts).await;
        assert_eq!(hits.load(Ordering::SeqCst), sent, "{needs:?}: {p:?}");
        if sent == 0 {
            assert!(p.ran.is_empty(), "{p:?}");
            assert!(p.handover, "the live case hands over: {p:?}");
            assert_eq!(
                p.reply, "I've passed this to a colleague, who will take it from here.",
                "the handover reply, not the space menu: {p:?}"
            );
        } else {
            assert_eq!(p.ran, [BY_REF], "{p:?}");
        }
    }
}

/// A selection of the payments area alone: exactly one context space (`retail:payments:refunds`) compiles, the
/// premise of the sole-space fallback (job #135).
fn payments_only() -> Value {
    let mut selection = design_contract::cuttable_selection();
    selection["areas"] = json!(["payments"]);
    selection
}

/// Save `selection` over the fixture's (the console's target step): the second save names the current
/// `selection_updated_at`.
async fn put_selection(fx: &Fx, selection: Value) {
    let (s, b) = call(fx, "GET", "target", Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = call(
        fx,
        "PUT",
        "target",
        json!({"selection": selection, "expected_updated_at": b["selection_updated_at"]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// The draft design's context spaces, as the probe reads them.
async fn draft_space_keys(fx: &Fx) -> Vec<String> {
    let mut tx = fx.f.job.begin(&fx.e.state.db).await.unwrap();
    let d = agent_api::runtime::design::RuntimeDesign::load(
        &mut tx,
        &fx.e.state.design,
        &fx.f.job_ref(),
        &agent_api::design::DesignOf::Draft,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    d.spaces.spaces().iter().map(|s| s.key.clone()).collect()
}

/// An executor answering `out` and keeping every call's resolved inputs (a step's args, resolved from the planned
/// request's slots).
struct Recording {
    out: Map<String, Value>,
    inputs: Arc<Mutex<Vec<Map<String, Value>>>>,
}

impl OpExecutor for Recording {
    fn execute<'a>(
        &'a self,
        req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move {
            self.inputs.lock().unwrap().push(req.inputs.clone());
            outcome(OpState::Ok, None, None, self.out.clone())
        })
    }
}

/// The sole-space fallback (#135) at the staff draft probe: a one-space job answers an unqualified message in its
/// only space, and the staff member is never shown the one-item menu.
#[tokio::test]
async fn a_one_space_job_probes_an_unqualified_message_in_its_only_space() {
    let fx = setup("t9a-sole-space", NO_SPACE, Duration::ZERO).await;
    put_selection(&fx, payments_only()).await;
    assert_eq!(draft_space_keys(&fx).await, ["retail:payments:refunds"]);
    // the refunds procedure's one read: routed to the job's instance, answered here
    crate::b4::support::route_instance(&fx.e, &fx.f, DEMO_STUB_INSTANCE, &[("payment", "list")])
        .await;
    let hits = Arc::new(AtomicUsize::new(0));
    let mut opts = ProbeOpts::new(&fx.e.state);
    opts.executors = Executors::default().register(
        "payment.list",
        fixed(
            json!({"rows": [{"payment_id": "PAY-1", "status": "refunded"}]}),
            &hits,
        ),
    );
    let design = draft(&fx).await;
    let p = run(&fx, design, "Where is my refund?", &opts).await;
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "the only space's plan ran: {p:?}"
    );
    assert_eq!(p.ran, ["payment.list"], "{p:?}");
    assert!(
        !p.reply.contains("I can help with:"),
        "a one-space job has no menu to offer: {p:?}"
    );
}

/// The sole-space fallback keeps the slots too: the typed lookup key the fold refuses as a request slot is kept for
/// the staff dry run (the probe's `typed`, as on the Ready path) and reaches the planned step's args.
#[tokio::test]
async fn a_one_space_probe_keeps_the_slots_refused_and_typed() {
    let fx = setup("t9a-sole-space-typed", NO_SPACE_TYPED, Duration::ZERO).await;
    put_selection(&fx, payments_only()).await;
    assert_eq!(draft_space_keys(&fx).await, ["retail:payments:refunds"]);
    crate::b4::support::route_instance(&fx.e, &fx.f, DEMO_STUB_INSTANCE, &[("payment", "list")])
        .await;
    let inputs: Arc<Mutex<Vec<Map<String, Value>>>> = Arc::default();
    let mut opts = ProbeOpts::new(&fx.e.state);
    opts.executors = Executors::default().register(
        "payment.list",
        Arc::new(Recording {
            out: Map::new(),
            inputs: Arc::clone(&inputs),
        }),
    );
    let design = draft(&fx).await;
    let p = run(&fx, design, "Where is my refund? RET-01", &opts).await;
    assert_eq!(p.ran, ["payment.list"], "{p:?}");
    let seen = inputs.lock().unwrap();
    assert_eq!(seen.len(), 1, "{p:?}");
    assert_eq!(
        seen[0].get("order_id"),
        Some(&json!("RET-01")),
        "the typed id the fold refused as a request slot is kept for the dry run: {p:?}"
    );
}
