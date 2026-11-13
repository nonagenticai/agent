//! B4 T6 — the operation runner (crate::runtime::runner) over real Postgres 18 + Redis, connected to Postgres as
//! `agent_runtime` (the role the binary runs as). A job created from `job/customer_care_agent` (the design fixture);
//! the orchestrator is stood in for by `op.call`s built here. Executors: B7's `ConnectorExecutor` on the tenant's
//! stub instance (M1), else registered fakes that count their calls.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_api::design::CaseDesign;
use agent_api::runtime::bus::{Envelope, console_view};
use agent_api::runtime::design::DesignCache;
use agent_api::runtime::domain::{OP_CALL, OP_RESULT, OpCall, OpResult, OpState as ResultState};
use agent_api::runtime::routes::{DEMO_STUB_INSTANCE, ResolvedTarget};
use agent_api::runtime::runner::ops::{
    Executors, OpExecutor, OpOutcome, OpRequest, OpState, outcome,
};
use agent_api::runtime::runner::{Runner, result_topic};
use agent_api::runtime::{Env, bus::CONSOLE_OP_FIELDS};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::b4::design::put_target;
use crate::b4::flow::{Fx, current_design, insert_case};
use crate::common::platform::{self, Op};
use crate::redis_env as r;

/// A registered executor: counts its calls, waits `delay`, answers `out` (its output echoes nothing it was given).
struct Fake {
    hits: AtomicUsize,
    delay: Duration,
    out: OpOutcome,
}

impl Fake {
    fn ok(output: Value) -> Arc<Fake> {
        Arc::new(Fake {
            hits: AtomicUsize::new(0),
            delay: Duration::ZERO,
            out: outcome(
                OpState::Ok,
                None,
                None,
                output.as_object().cloned().unwrap_or_default(),
            ),
        })
    }

    fn slow(delay: Duration) -> Arc<Fake> {
        Arc::new(Fake {
            hits: AtomicUsize::new(0),
            delay,
            out: outcome(OpState::Ok, None, None, Map::new()),
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
            tokio::time::sleep(self.delay).await;
            self.out.clone()
        })
    }
}

/// The flow fixture with the job's target selection saved (the pinned template compiles: the catalogue has the
/// demo operations).
async fn setup(test: &str, env: Env) -> Fx {
    let fx = crate::b4::flow::setup_latest(test, env).await;
    put_target(&fx.d).await;
    fx
}

/// A runner for the fixture's scope as `agent_runtime`; `x` = `None` keeps the default executors (B7's).
fn runner(fx: &Fx, x: Option<Executors>) -> Runner {
    let mut st = fx.d.e.state.clone();
    st.db = fx.rt.clone();
    let r = Runner::new(fx.bus.clone(), st, Arc::new(DesignCache::default()))
        .unwrap()
        .with_consumer("runner-test")
        .with_consume(r::fast());
    match x {
        Some(x) => r.with_executors(x),
        None => r,
    }
}

/// The case row (stamped now), its staff conversation and one inbound customer message; the message id.
async fn case_with_conversation(fx: &Fx, case: &str) -> String {
    insert_case(fx, case, &current_design(fx).await).await;
    let (t, j) = (fx.d.f.tenant, fx.d.f.job.job_id);
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, $3, $4, 'staff', 'C-1')",
    )
    .bind(t)
    .bind(j)
    .bind(format!("conv-{case}"))
    .bind(fx.scope.env.as_str())
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    let msg: String = sqlx::query_scalar(
        "INSERT INTO conversation_message (tenant_id, job_id, conversation_id, role, text, case_id, at_ms) \
         VALUES ($1, $2, $3, 'customer', 'please do it', $4, 1) RETURNING message_id::text",
    )
    .bind(t)
    .bind(j)
    .bind(format!("conv-{case}"))
    .bind(case)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    // A1/A2: the conversation's customer is known through STATE (`subject` only, so no known-customer context preload; the catalogue level-1+ ops require it)
    crate::common::state::put(
        &fx.d.e.owner,
        t,
        j,
        &format!("conv-{case}"),
        "subject",
        "C-1",
    )
    .await;
    msg
}

/// An `op.call` of `case` (step `s`, attempt `attempt`).
#[allow(clippy::too_many_arguments)]
fn call(
    fx: &Fx,
    case: &str,
    step: &str,
    attempt: u32,
    operation: &str,
    variant: Option<&str>,
    input: Value,
    key: Option<&str>,
    confirm: Option<&str>,
    identity: u8,
) -> OpCall {
    // no plan design: the runner stamps the call with the design it reads (test: the draft revision)
    serde_json::from_value(json!({
        "call_id": agent_api::runtime::domain::call_id(case, 0, step, attempt),
        "case_id": case, "conversation_id": format!("conv-{case}"), "phase": 0, "step": step,
        "attempt": attempt, "agent_role": "procedure", "operation": operation, "variant": variant,
        "input": input, "idempotency_key": key, "env": fx.scope.env, "release_id": null,
        "draft_rev": null, "confirm_ref": confirm, "initiator": "procedure@test",
        "identity_level": identity, "deadline_ms": 0
    }))
    .unwrap()
}

/// `(state, outcome_code, route, finished_at IS NOT NULL, idempotency_key)` of call `id`.
async fn call_row(fx: &Fx, id: &str) -> (String, Option<String>, String, bool, Option<String>) {
    sqlx::query_as(
        "SELECT state, outcome_code, route, finished_at IS NOT NULL, idempotency_key FROM operation_call \
          WHERE tenant_id = $1 AND job_id = $2 AND call_id = $3",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .bind(id)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap()
}

/// A write op the catalogue does not know (gated as the strongest write, P) — registered executors run it.
const WRITE_OP: &str = "spaceship.launch";

#[tokio::test]
async fn write_without_confirm_ref_is_confirmation_required() {
    let fx = setup("runner-confirm", Env::Test).await;
    let msg = case_with_conversation(&fx, "CS-1").await;
    let fake = Fake::ok(json!({"status": "launched"}));
    let rn = runner(
        &fx,
        Some(Executors::default().register(WRITE_OP, fake.clone())),
    );
    // no confirm_ref, then one that is no customer message of this conversation
    for (i, confirm) in [None, Some("0195f48b-d000-7000-8000-000000000001")]
        .into_iter()
        .enumerate()
    {
        let step = format!("launch{i}");
        let c = call(
            &fx,
            "CS-1",
            &step,
            1,
            WRITE_OP,
            None,
            json!({}),
            Some(&format!("CS-1:0:{step}")),
            confirm,
            2,
        );
        let res = rn.handle(&c).await.unwrap();
        assert_eq!(res.state, ResultState::Denied, "{confirm:?}");
        assert_eq!(res.outcome_code.as_deref(), Some("confirmation_required"));
        assert!(!res.retryable);
        let (state, code, _, finished, key) = call_row(&fx, &c.call_id).await;
        assert_eq!(
            (state.as_str(), code.as_deref()),
            ("denied", Some("confirmation_required"))
        );
        assert!(finished, "a denied call is finished");
        assert_eq!(key, None, "a denial holds no idempotency key");
    }
    assert_eq!(fake.hits(), 0, "a refused write never reaches the executor");
    // control: the inbound customer message confirms it
    let c = call(
        &fx,
        "CS-1",
        "launch",
        1,
        WRITE_OP,
        None,
        json!({}),
        Some("CS-1:0:launch"),
        Some(&msg),
        2,
    );
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(fake.hits(), 1);
    // (A2: the call's own identity level no longer gates a write; the former "low level -> identity_required" check is
    // deleted by design, the state gate is covered by the tests of `state_required`)
}

#[tokio::test]
async fn duplicate_idempotency_key_returns_stored_outcome() {
    let fx = setup("runner-dupe", Env::Test).await;
    let msg = case_with_conversation(&fx, "CS-2").await;
    let fake = Fake::ok(json!({"status": "launched", "count": 1}));
    let rn = runner(
        &fx,
        Some(Executors::default().register(WRITE_OP, fake.clone())),
    );
    let first = call(
        &fx,
        "CS-2",
        "launch",
        1,
        WRITE_OP,
        None,
        json!({}),
        Some("CS-2:0:launch"),
        Some(&msg),
        2,
    );
    let a = rn.handle(&first).await.unwrap();
    assert_eq!(a.state, ResultState::Ok);
    // a second attempt (new call id) with the same key, and a redelivery of the first call
    let second = call(
        &fx,
        "CS-2",
        "launch",
        2,
        WRITE_OP,
        None,
        json!({}),
        Some("CS-2:0:launch"),
        Some(&msg),
        2,
    );
    assert_ne!(first.call_id, second.call_id);
    for c in [&second, &first] {
        let b = rn.handle(c).await.unwrap();
        assert_eq!(b.state, ResultState::Ok);
        assert_eq!(b.output, a.output, "the stored outcome answers");
    }
    assert_eq!(fake.hits(), 1, "the system saw one write");
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM operation_call WHERE tenant_id = $1 AND job_id = $2 AND idempotency_key = 'CS-2:0:launch'",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(n, 1);
    let (state, _, _, finished, _) = call_row(&fx, &first.call_id).await;
    assert_eq!((state.as_str(), finished), ("ok", true));
}

#[tokio::test]
async fn timeout_on_x_op_with_failed_verify_is_unknown_outcome_and_never_retried() {
    let fx = setup("runner-unknown", Env::Test).await;
    let msg = case_with_conversation(&fx, "CS-3").await;
    let fake = Fake::slow(Duration::from_millis(500));
    let rn = runner(
        &fx,
        Some(Executors::default().register(WRITE_OP, fake.clone())),
    )
    .with_timeouts(Duration::from_millis(50), Duration::from_millis(50));
    let c = call(
        &fx,
        "CS-3",
        "launch",
        1,
        WRITE_OP,
        None,
        json!({}),
        Some("CS-3:0:launch"),
        Some(&msg),
        2,
    );
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::UnknownOutcome, "{res:?}");
    assert_eq!(res.outcome_code.as_deref(), Some("unknown_outcome"));
    assert!(!res.retryable, "an unknown outcome is never retried");
    let (state, _, _, finished, _) = call_row(&fx, &c.call_id).await;
    assert_eq!((state.as_str(), finished), ("unknown_outcome", true));
    // the case alert, its event and the handover (reason unknown_outcome), stamped with the draft
    let (alert, status): (Option<String>, String) = sqlx::query_as(
        "SELECT alert, status FROM agent_case WHERE tenant_id = $1 AND case_id = 'CS-3'",
    )
    .bind(fx.d.f.tenant)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(
        (alert.as_deref(), status.as_str()),
        (Some("unknown_outcome"), "handover")
    );
    let (reason, draft_rev): (String, Option<i64>) = sqlx::query_as(
        "SELECT reason, draft_rev FROM agent_handover_items WHERE tenant_id = $1 AND case_id = 'CS-3'",
    )
    .bind(fx.d.f.tenant)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(reason, "unknown_outcome");
    assert!(
        draft_rev.is_some(),
        "a test-env item carries the draft revision"
    );
    let alerts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM case_event WHERE tenant_id = $1 AND case_id = 'CS-3' AND kind = 'alert'",
    )
    .bind(fx.d.f.tenant)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(alerts, 1);
    // a redelivery (or a retry with the key) answers the stored unknown outcome and never runs the write again
    let again = rn.handle(&c).await.unwrap();
    assert_eq!(again.state, ResultState::UnknownOutcome);
    let retry = call(
        &fx,
        "CS-3",
        "launch",
        2,
        WRITE_OP,
        None,
        json!({}),
        Some("CS-3:0:launch"),
        Some(&msg),
        2,
    );
    assert_eq!(
        rn.handle(&retry).await.unwrap().state,
        ResultState::UnknownOutcome
    );
    assert_eq!(fake.hits(), 1);
}

#[tokio::test]
async fn suspended_tenant_call_is_denied_and_audited() {
    let fx = setup("runner-suspended", Env::Test).await;
    case_with_conversation(&fx, "CS-4").await;
    let fake = Fake::ok(json!({"status": "x"}));
    let rn = runner(
        &fx,
        Some(Executors::default().register("order.read", fake.clone())),
    );
    sqlx::query("UPDATE tenants SET status = 'suspended', suspended_at = now() WHERE id = $1")
        .bind(fx.d.f.tenant)
        .execute(&fx.d.e.owner)
        .await
        .unwrap();
    let c = call(
        &fx,
        "CS-4",
        "order",
        1,
        "order.read",
        None,
        json!({"order_id": "RET-01"}),
        None,
        None,
        3,
    );
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::Denied);
    assert_eq!(res.outcome_code.as_deref(), Some("tenant_suspended"));
    assert_eq!(fake.hits(), 0);
    let (state, code, _, _, _) = call_row(&fx, &c.call_id).await;
    assert_eq!(
        (state.as_str(), code.as_deref()),
        ("denied", Some("tenant_suspended"))
    );
    let (actor_type, status, details): (String, String, Value) = sqlx::query_as(
        "SELECT actor_type, status, details FROM audit_event \
          WHERE tenant_id = $1 AND action = 'execute' AND resource_id = $2",
    )
    .bind(fx.d.f.tenant)
    .bind(&c.call_id)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(status, "denied");
    assert_eq!(actor_type, "ai_agent");
    assert_eq!(details["code"], "tenant_suspended");
    assert_eq!(details["operation"], "order.read");
    // control: active again, the same read runs
    sqlx::query("UPDATE tenants SET status = 'active', suspended_at = NULL WHERE id = $1")
        .bind(fx.d.f.tenant)
        .execute(&fx.d.e.owner)
        .await
        .unwrap();
    let c = call(
        &fx,
        "CS-4",
        "order2",
        1,
        "order.read",
        None,
        json!({"order_id": "RET-01"}),
        None,
        None,
        3,
    );
    assert_eq!(rn.handle(&c).await.unwrap().state, ResultState::Ok);
    assert_eq!(fake.hits(), 1);
}

#[tokio::test]
async fn agent_cases_handover_is_stamped_with_the_scope_design() {
    // due times from the handover policy wait for T16 (B12's create sets none); the design stamp is here
    let fx = setup("runner-agent-cases", Env::Test).await;
    let msg = case_with_conversation(&fx, "CS-5").await;
    let rn = runner(&fx, Some(Executors::default()));
    let c = call(
        &fx,
        "CS-5",
        "handover",
        1,
        "case.create",
        Some("handover"),
        json!({"reason": "customer_asked", "subject": "Wants a person"}),
        Some("CS-5:0:handover"),
        Some(&msg),
        2,
    );
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(res.route_kind.as_deref(), Some("fallback"));
    let id = res.output.as_ref().unwrap()["handover_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (reason, env, release_id, draft_rev, subject): (
        String,
        String,
        Option<String>,
        Option<i64>,
        String,
    ) = sqlx::query_as(
        "SELECT reason, env, release_id, draft_rev, subject FROM agent_handover_items \
              WHERE tenant_id = $1 AND id = $2",
    )
    .bind(fx.d.f.tenant)
    .bind(&id)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(
        (reason.as_str(), env.as_str(), subject.as_str()),
        ("customer_asked", "test", "Wants a person")
    );
    assert_eq!(release_id, None);
    let CaseDesign::Draft { draft_rev: now } = current_design(&fx).await else {
        panic!("a test scope reads the draft");
    };
    assert_eq!(draft_rev, Some(now));
    let (state, _, route, _, _) = call_row(&fx, &c.call_id).await;
    assert_eq!((state.as_str(), route.as_str()), ("ok", "fallback"));
}

#[tokio::test]
async fn op_result_mirror_has_no_input_fields() {
    let fx = setup("runner-mirror", Env::Test).await;
    case_with_conversation(&fx, "CS-6").await;
    let marker_in = "INPUT-MARKER-7731";
    let marker_out = "OUTPUT-MARKER-7731";
    let fake = Fake::ok(json!({"status": marker_out}));
    let rn = runner(&fx, Some(Executors::default().register("order.read", fake)));
    let c = call(
        &fx,
        "CS-6",
        "order",
        1,
        "order.read",
        None,
        json!({"order_id": marker_in}),
        None,
        None,
        3,
    );
    let env = Envelope::new(
        &format!("order-support.returns.{OP_CALL}"),
        "orchestrator",
        "CS-6",
        &c,
    )
    .unwrap();
    rn.on_call(env).await.unwrap();
    // the result stream carries the output for the orchestrator, under the matching topic
    let results = r::wait_entries(&fx.bus, &fx.bus.streams.op_results(), 1, |e| {
        e.correlation_id == "CS-6"
    })
    .await;
    assert_eq!(
        results[0].topic,
        result_topic(&format!("order-support.returns.{OP_CALL}"))
    );
    assert!(results[0].topic.ends_with(OP_RESULT));
    let res: OpResult = results[0].payload_as().unwrap();
    assert_eq!(res.output.as_ref().unwrap()["status"], marker_out);
    // the console view and the recent backlog: ids and codes only
    let view = console_view(&results[0]);
    for k in view["payload"].as_object().unwrap().keys() {
        assert!(CONSOLE_OP_FIELDS.contains(&k.as_str()), "{k}");
    }
    let recent: Vec<String> = redis::cmd("LRANGE")
        .arg(fx.bus.streams.console_recent())
        .arg(0)
        .arg(-1)
        .query_async(&mut fx.bus.conn())
        .await
        .unwrap();
    assert!(!recent.is_empty());
    for frame in &recent {
        assert!(!frame.contains(marker_in), "{frame}");
        assert!(!frame.contains(marker_out), "{frame}");
    }
    // nor the row, the case event or the audit row
    let leaked: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM operation_call WHERE tenant_id = $1 AND (detail LIKE $2 OR (step - 'refs')::text LIKE $2)) \
              + (SELECT count(*) FROM case_event WHERE tenant_id = $1 AND body::text LIKE $2) \
              + (SELECT count(*) FROM audit_event WHERE tenant_id = $1 AND details::text LIKE $2)",
    )
    .bind(fx.d.f.tenant)
    .bind("%MARKER-7731%")
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(leaked, 0);
}

#[tokio::test]
async fn b7_executor_registration_overrides_instance_routes() {
    let fx = setup("runner-registration", Env::Test).await;
    let msg = case_with_conversation(&fx, "CS-7").await;
    demo_stub(&fx).await;
    // a working route sends order.read to the stub instance; the registration wins over it
    route(&fx, "order", "read", "instance", Some(DEMO_STUB_INSTANCE)).await;
    let fake = Fake::ok(json!({"status": "from-the-registration"}));
    let rn = runner(
        &fx,
        Some(Executors::default().register("order.read", fake.clone())),
    );
    let c = call(
        &fx,
        "CS-7",
        "order",
        1,
        "order.read",
        None,
        json!({"order_id": "RET-01"}),
        None,
        None,
        3,
    );
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::Ok);
    assert_eq!(
        res.route_kind.as_deref(),
        Some("instance"),
        "the route still resolves"
    );
    assert_eq!(
        res.output.as_ref().unwrap()["status"],
        "from-the-registration"
    );
    assert_eq!(fake.hits(), 1);
    // a route row decides over the defaults: not_offered replaces the instance route
    route(&fx, "order", "list", "not_offered", None).await;
    let c = call(
        &fx,
        "CS-7",
        "list",
        1,
        "order.list",
        None,
        json!({}),
        Some("CS-7:0:list"),
        Some(&msg),
        3,
    );
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.outcome_code.as_deref(), Some("not_offered"));
    assert_eq!(res.route_kind.as_deref(), Some("not_offered"));
}

#[tokio::test]
async fn order_read_routes_to_the_stub_instance_in_test() {
    // M1 evidence (§0 R9): an instance route in the test env → B7's executor on the stub
    let fx = setup("runner-m1", Env::Test).await;
    case_with_conversation(&fx, "CS-8").await;
    demo_stub(&fx).await;
    route(&fx, "order", "read", "instance", Some(DEMO_STUB_INSTANCE)).await;
    let rn = Arc::new(runner(&fx, None));
    let task = tokio::spawn(Arc::clone(&rn).run());
    let c = call(
        &fx,
        "CS-8",
        "order",
        1,
        "order.read",
        None,
        json!({"order_id": "RET-01"}),
        None,
        None,
        3,
    );
    let env = Envelope::new(
        &format!("order-support.returns.{OP_CALL}"),
        "orchestrator",
        "CS-8",
        &c,
    )
    .unwrap();
    fx.bus.publish(&fx.bus.streams.op(), &env).await.unwrap();
    let got = r::wait_entries(&fx.bus, &fx.bus.streams.op_results(), 1, |e| {
        e.correlation_id == "CS-8"
    })
    .await;
    task.abort();
    let res: OpResult = got[0].payload_as().unwrap();
    assert_eq!(res.route_kind.as_deref(), Some("instance"));
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    let (instance, template, state): (Option<String>, Option<String>, String) = sqlx::query_as(
        "SELECT instance_id, template, state FROM operation_call WHERE tenant_id = $1 AND call_id = $2",
    )
    .bind(fx.d.f.tenant)
    .bind(&c.call_id)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(instance.as_deref(), Some(DEMO_STUB_INSTANCE));
    assert!(template.unwrap_or_default().starts_with("test_shop@"));
    assert_eq!(state, "ok");
    let ops: i32 = sqlx::query_scalar(
        "SELECT op_calls FROM agent_case WHERE tenant_id = $1 AND case_id = 'CS-8'",
    )
    .bind(fx.d.f.tenant)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    assert_eq!(ops, 1);
}

#[tokio::test]
async fn prod_stub_instance_is_refused() {
    // B7: a stub never serves prod. Even a prod route row naming the stub runs nowhere (the executor reads the
    // release snapshot, and a never-released job has none).
    let fx = setup("runner-prod-stub", Env::Prod).await;
    let msg = case_with_conversation(&fx, "CS-9").await;
    demo_stub(&fx).await;
    route(&fx, "order", "read", "instance", Some(DEMO_STUB_INSTANCE)).await;
    let rn = runner(&fx, None);
    // the prod design may not know order.read (never released: the baseline), so it is confirmed like a write
    let c = call(
        &fx,
        "CS-9",
        "order",
        1,
        "order.read",
        None,
        json!({"order_id": "RET-01"}),
        Some("CS-9:0:order"),
        Some(&msg),
        3,
    );
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::Error, "{res:?}");
    assert_ne!(res.outcome_code, None);
    assert!(res.output.is_none());
    let (state, _, _, finished, _) = call_row(&fx, &c.call_id).await;
    assert_eq!((state.as_str(), finished), ("error", true));
}

/// The tenant's stub instance `DEMO_STUB_INSTANCE` on a platform template `test_shop@1` (test env): the template
/// and the instance only, the test routes what it needs. There is no `by_tracking_ref` operation (B7 serves that
/// variant with the order-keyed `delivery.read`).
async fn demo_stub(fx: &Fx) {
    let ops = [
        Op::star("order", "read"),
        Op::star("order", "list"),
        Op::star("delivery", "read"),
        Op::star("case", "create"),
        Op::new("case", "create", "return"),
    ];
    platform::connector_at(
        &fx.d.e,
        fx.d.f.tenant,
        None,
        "test_shop",
        "1",
        DEMO_STUB_INSTANCE,
        &ops,
    )
    .await
    .unwrap();
}

/// A working `operation_route` row of the fixture's job in the scope's env.
async fn route(fx: &Fx, object: &str, verb: &str, kind: &str, instance: Option<&str>) {
    sqlx::query(
        "INSERT INTO operation_route (tenant_id, job_id, object, verb, variant, env, target_kind, instance_id, updated_by) \
         VALUES ($1, $2, $3, $4, '*', $5, $6, $7, 'u-test') \
         ON CONFLICT (tenant_id, job_id, object, verb, variant, env) \
         DO UPDATE SET target_kind = EXCLUDED.target_kind, instance_id = EXCLUDED.instance_id",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .bind(object)
    .bind(verb)
    .bind(fx.scope.env.as_str())
    .bind(kind)
    .bind(instance)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
}

#[test]
fn ids_are_deterministic() {
    // the call id the tests build is the orchestrator's (a redelivered dispatch is the same call)
    let a = agent_api::runtime::domain::call_id("CS-1", 0, "x", 1);
    assert_eq!(a, agent_api::runtime::domain::call_id("CS-1", 0, "x", 1));
    assert!(Uuid::parse_str(a.trim_start_matches("opc_")).is_ok());
}

// ---- F4: connector case.create — customer_email from the case party, external_ref from the answer ---------------

/// A registered executor that keeps the inputs of every request and answers `output`.
struct Capture {
    inputs: std::sync::Mutex<Vec<Map<String, Value>>>,
    output: Map<String, Value>,
}

impl Capture {
    fn ok(output: Value) -> Arc<Capture> {
        Arc::new(Capture {
            inputs: std::sync::Mutex::new(Vec::new()),
            output: output.as_object().cloned().unwrap_or_default(),
        })
    }

    fn seen(&self) -> Vec<Map<String, Value>> {
        self.inputs.lock().unwrap().clone()
    }
}

impl OpExecutor for Capture {
    fn execute<'a>(
        &'a self,
        req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move {
            self.inputs.lock().unwrap().push(req.inputs.clone());
            outcome(OpState::Ok, None, None, self.output.clone())
        })
    }
}

const RETURN_OP: &str = "case.create{return}";

/// `(channel, subject, verified, seen_minutes_ago)` of a party identity.
type Identity<'a> = (&'a str, &'a str, bool, i32);

/// A party of the fixture's tenant with `identities` (`(channel, subject, verified, seen_minutes_ago)`), linked to
/// `case`.
async fn case_party(fx: &Fx, case: &str, identities: &[Identity<'_>]) {
    let party: Uuid = sqlx::query_scalar("INSERT INTO party (tenant_id) VALUES ($1) RETURNING id")
        .bind(fx.d.f.tenant)
        .fetch_one(&fx.d.e.owner)
        .await
        .unwrap();
    for (channel, subject, verified, ago) in identities {
        sqlx::query(
            "INSERT INTO party_identity (tenant_id, channel, subject, party_id, verified, first_seen_at, last_seen_at) \
             VALUES ($1, $2, $3, $4, $5, now() - interval '1 day', now() - make_interval(mins => $6))",
        )
        .bind(fx.d.f.tenant)
        .bind(*channel)
        .bind(*subject)
        .bind(party)
        .bind(*verified)
        .bind(*ago)
        .execute(&fx.d.e.owner)
        .await
        .unwrap();
    }
    sqlx::query(
        "UPDATE agent_case SET party_id = $3 WHERE tenant_id = $1 AND job_id = $2 AND case_id = $4",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .bind(party)
    .bind(case)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
}

async fn external_ref_of(fx: &Fx, case: &str) -> Option<String> {
    sqlx::query_scalar(
        "SELECT external_ref FROM agent_case WHERE tenant_id = $1 AND job_id = $2 AND case_id = $3",
    )
    .bind(fx.d.f.tenant)
    .bind(fx.d.f.job.job_id)
    .bind(case)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap()
}

/// The return procedure's `rma` step (`case.create{return}`) of `case`, confirmed by `msg`.
fn rma_call(fx: &Fx, case: &str, step: &str, msg: &str, input: Value) -> OpCall {
    call(
        fx,
        case,
        step,
        1,
        "case.create",
        Some("return"),
        input,
        Some(&format!("{case}:0:{step}")),
        Some(msg),
        3,
    )
}

#[tokio::test]
async fn case_create_gets_the_party_email_and_stores_the_vendor_ticket() {
    let fx = setup("runner-f4-party", Env::Test).await;
    let msg = case_with_conversation(&fx, "CS-21").await;
    // the verified email wins over a more recent unverified one; another channel's subject is never an email
    case_party(
        &fx,
        "CS-21",
        &[
            ("email", "ada@example.test", true, 60),
            ("email", "ada.other@example.test", false, 1),
            ("web_chat", "session-1", true, 0),
        ],
    )
    .await;
    demo_stub(&fx).await;
    route(&fx, "case", "create", "instance", Some(DEMO_STUB_INSTANCE)).await;
    let cap = Capture::ok(json!({"case_id": "HD-4711", "status": "Open"}));
    let rn = runner(
        &fx,
        Some(Executors::default().register(RETURN_OP, cap.clone())),
    );
    let res = rn
        .handle(&rma_call(
            &fx,
            "CS-21",
            "rma",
            &msg,
            json!({"subject_object": "RET-01"}),
        ))
        .await
        .unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(res.route_kind.as_deref(), Some("instance"));
    let seen = cap.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0]["customer_email"], "ada@example.test");
    assert_eq!(seen[0]["subject_object"], "RET-01");
    assert_eq!(
        external_ref_of(&fx, "CS-21").await.as_deref(),
        Some("HD-4711")
    );

    // a second ticket of the same case never replaces the first; a customer_email the call names (e.g. one a model
    // was talked into) never overrides the party's email
    let cap2 = Capture::ok(json!({"case_id": "HD-9999"}));
    let rn = runner(
        &fx,
        Some(Executors::default().register(RETURN_OP, cap2.clone())),
    );
    let mut again = rma_call(
        &fx,
        "CS-21",
        "rma2",
        &msg,
        json!({"customer_email": "attacker@evil.test"}),
    );
    // whatever the template's side effect, the per-case cap (T6a) is not what this asserts
    again.max_per_case = Some(5);
    let res = rn.handle(&again).await.unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(cap2.seen()[0]["customer_email"], "ada@example.test");
    assert_eq!(
        external_ref_of(&fx, "CS-21").await.as_deref(),
        Some("HD-4711")
    );
}

#[tokio::test]
async fn case_create_without_a_party_email_sends_none_and_the_service_user_answers() {
    // B7 R10: no customer_email → the template's configured service user (`?? env.requester_id`)
    let fx = setup("runner-f4-none", Env::Test).await;
    let msg = case_with_conversation(&fx, "CS-22").await;
    let msg2 = case_with_conversation(&fx, "CS-23").await;
    // CS-22: no party at all; CS-23: a party with no email identity
    case_party(&fx, "CS-23", &[("web_chat", "session-2", true, 0)]).await;
    demo_stub(&fx).await;
    route(&fx, "case", "create", "instance", Some(DEMO_STUB_INSTANCE)).await;
    let cap = Capture::ok(json!({"case_id": "HD-1"}));
    let rn = runner(
        &fx,
        Some(Executors::default().register(RETURN_OP, cap.clone())),
    );
    for (case, m) in [("CS-22", &msg), ("CS-23", &msg2)] {
        let res = rn
            .handle(&rma_call(
                &fx,
                case,
                "rma",
                m,
                json!({"subject_object": "RET-01"}),
            ))
            .await
            .unwrap();
        assert_eq!(res.state, ResultState::Ok, "{case}: {res:?}");
        assert_eq!(external_ref_of(&fx, case).await.as_deref(), Some("HD-1"));
    }
    for inputs in cap.seen() {
        assert!(inputs.get("customer_email").is_none(), "{inputs:?}");
    }

    // with no party email, a value the call names is dropped too: a designed step can bind it from a slot the
    // customer filled, so only the runtime sets customer_email and the service user answers
    let cap = Capture::ok(json!({"case_id": "HD-2"}));
    let rn = runner(
        &fx,
        Some(Executors::default().register(RETURN_OP, cap.clone())),
    );
    let mut named = rma_call(
        &fx,
        "CS-22",
        "rma2",
        &msg,
        json!({"subject_object": "RET-01", "customer_email": "victim@example.test"}),
    );
    named.max_per_case = Some(5);
    let res = rn.handle(&named).await.unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert!(
        cap.seen()[0].get("customer_email").is_none(),
        "{:?}",
        cap.seen()[0]
    );
}

/// A `case.create` call whose case has NO `agent_case` row (the runner takes no party email then): the caller's
/// `customer_email` is still dropped and the service user answers (audit of agent#92, fail closed).
#[tokio::test]
async fn case_create_without_a_case_row_still_drops_the_callers_email() {
    let fx = setup("runner-f4-nocase", Env::Test).await;
    // a conversation and the customer's message to confirm with — but no case row
    let (t, j) = (fx.d.f.tenant, fx.d.f.job.job_id);
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, 'conv-CS-90', $3, 'staff', 'C-1')",
    )
    .bind(t)
    .bind(j)
    .bind(fx.scope.env.as_str())
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    let msg: String = sqlx::query_scalar(
        "INSERT INTO conversation_message (tenant_id, job_id, conversation_id, role, text, at_ms) \
         VALUES ($1, $2, 'conv-CS-90', 'customer', 'please do it', 1) RETURNING message_id::text",
    )
    .bind(t)
    .bind(j)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap();
    demo_stub(&fx).await;
    route(&fx, "case", "create", "instance", Some(DEMO_STUB_INSTANCE)).await;
    let cap = Capture::ok(json!({"case_id": "HD-90"}));
    let rn = runner(
        &fx,
        Some(Executors::default().register(RETURN_OP, cap.clone())),
    );
    let res = rn
        .handle(&rma_call(
            &fx,
            "CS-90",
            "rma",
            &msg,
            json!({"subject_object": "RET-01", "customer_email": "victim@example.test"}),
        ))
        .await
        .unwrap();
    // B4 customer verification: a call with no case row is judged as a customer's (fail closed), and an
    // unverified customer's case.create is refused before any connector runs — the caller's email never leaves
    assert_eq!(res.state, ResultState::Denied, "{res:?}");
    assert!(cap.seen().is_empty(), "{:?}", cap.seen());
}

/// An instance executor that records the inputs it runs and says a `case.create` serves every call.
struct ServesCaseCreate {
    inputs: std::sync::Mutex<Vec<Map<String, Value>>>,
}

impl OpExecutor for ServesCaseCreate {
    fn execute<'a>(
        &'a self,
        req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move {
            self.inputs.lock().unwrap().push(req.inputs.clone());
            outcome(OpState::Ok, None, None, Map::new())
        })
    }

    fn served<'a>(
        &'a self,
        _req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, Result<Option<agent_api::runtime::runner::ops::Served>, String>> {
        Box::pin(std::future::ready(Ok(Some(
            agent_api::runtime::runner::ops::Served {
                op_key: "case.create".to_owned(),
                needs: Vec::new(),
            },
        ))))
    }
}

/// The email strip follows the operation that SERVES the call (audit of agent#92): a call requested under another
/// key (`delivery.read{by_tracking_ref}`) but served by a `case.create` route never sends the caller's
/// `customer_email`; the case party's email is set instead. Mutation: deciding on the requested key only sends
/// `victim@example.test`.
#[tokio::test]
async fn the_email_strip_follows_the_operation_that_serves_the_call() {
    let fx = setup("runner-f4-served", Env::Test).await;
    case_with_conversation(&fx, "CS-91").await;
    case_party(&fx, "CS-91", &[("email", "ada@example.test", true, 0)]).await;
    demo_stub(&fx).await;
    route(
        &fx,
        "delivery",
        "read",
        "instance",
        Some(DEMO_STUB_INSTANCE),
    )
    .await;
    let x = Arc::new(ServesCaseCreate {
        inputs: std::sync::Mutex::new(Vec::new()),
    });
    let rn = runner(&fx, Some(Executors::default().with_instance(x.clone())));
    let c = by_ref_call(
        &fx,
        "CS-91",
        "served",
        json!({"tracking_ref": "3S-1", "customer_email": "victim@example.test"}),
        3,
    );
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    let seen = x.inputs.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0]["customer_email"], "ada@example.test",
        "{:?}",
        seen[0]
    );
    assert_eq!(seen[0]["tracking_ref"], "3S-1");
}

#[tokio::test]
async fn only_an_instance_case_create_stores_an_external_ref() {
    // the fallback store (agent_cases) leaves external_ref NULL and gets no party email
    let fx = setup("runner-f4-fallback", Env::Test).await;
    let msg = case_with_conversation(&fx, "CS-24").await;
    case_party(&fx, "CS-24", &[("email", "ada@example.test", true, 0)]).await;
    route(&fx, "case", "create", "agent_cases", None).await;
    let cap = Capture::ok(json!({"case_id": "HO-1"}));
    let rn = runner(
        &fx,
        Some(Executors::default().register(RETURN_OP, cap.clone())),
    );
    let res = rn
        .handle(&rma_call(&fx, "CS-24", "rma", &msg, json!({})))
        .await
        .unwrap();
    assert_eq!(res.route_kind.as_deref(), Some("fallback"));
    assert!(
        cap.seen()[0].get("customer_email").is_none(),
        "no party email off an instance route"
    );
    assert_eq!(external_ref_of(&fx, "CS-24").await, None);
}

#[tokio::test]
async fn a_blank_vendor_ticket_id_is_not_stored() {
    let fx = setup("runner-f4-blank", Env::Test).await;
    let msg = case_with_conversation(&fx, "CS-25").await;
    demo_stub(&fx).await;
    route(&fx, "case", "create", "instance", Some(DEMO_STUB_INSTANCE)).await;
    let cap = Capture::ok(json!({"case_id": "   "}));
    let rn = runner(
        &fx,
        Some(Executors::default().register(RETURN_OP, cap.clone())),
    );
    let res = rn
        .handle(&rma_call(&fx, "CS-25", "rma", &msg, json!({})))
        .await
        .unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(external_ref_of(&fx, "CS-25").await, None);
}

// ---- step 6b (audit of agent#64): a variant served by the routed system's `*` operation ---------------------------

/// The variant the track_parcel plan calls: level 1 (the reference is a possession factor); its `*` is level 2.
const BY_REF: &str = "delivery.read{by_tracking_ref}";

/// An instance executor that says which operation serves a call ([`OpExecutor::served`]) and counts the calls
/// that reach it.
struct Serves {
    hits: AtomicUsize,
    served: agent_api::runtime::runner::ops::Served,
}

impl Serves {
    /// Served by `op_key`, whose request needs `needs` (each: any one of).
    fn by(op_key: &str, needs: &[&[&str]]) -> Arc<Serves> {
        Arc::new(Serves {
            hits: AtomicUsize::new(0),
            served: agent_api::runtime::runner::ops::Served {
                op_key: op_key.to_owned(),
                needs: needs
                    .iter()
                    .map(|any| any.iter().map(|k| (*k).to_owned()).collect())
                    .collect(),
            },
        })
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl OpExecutor for Serves {
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
                json!({"status": "in_transit"})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            )
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

/// Forget the conversation's state (the caller becomes unknown), or give it back its `subject`.
async fn set_subject(fx: &Fx, case: &str, known: bool) {
    let (t, j, conv) = (fx.d.f.tenant, fx.d.f.job.job_id, format!("conv-{case}"));
    if known {
        crate::common::state::put(&fx.d.e.owner, t, j, &conv, "subject", "C-1").await;
    } else {
        sqlx::query(
            "DELETE FROM conversation_state WHERE tenant_id = $1 AND job_id = $2 AND conversation_id = $3",
        )
        .bind(t)
        .bind(j)
        .bind(conv)
        .execute(&fx.d.e.owner)
        .await
        .unwrap();
    }
}

/// A read of [`BY_REF`] for `case` (step `step`) at identity level `level` with `input`.
fn by_ref_call(fx: &Fx, case: &str, step: &str, input: Value, level: u8) -> OpCall {
    call(
        fx,
        case,
        step,
        1,
        "delivery.read",
        Some("by_tracking_ref"),
        input,
        None,
        None,
        level,
    )
}

/// The catalogue's level of [`BY_REF`] is 1 and its `*`'s 2 — what makes the tests below able to fail (were the
/// variant unknown, the runner would gate it at the `*`'s level already, before step 6b).
async fn assert_levels(fx: &Fx) {
    use agent_api::runtime::catalog::OperationCatalog;
    use agent_api::runtime::runner::authorise::identity;
    let mut tx = fx.d.f.job.begin(&fx.d.e.state.db).await.unwrap();
    let d = agent_api::runtime::design::RuntimeDesign::load(
        &mut tx,
        &fx.d.e.state.design,
        &fx.d.f.job_ref(),
        &agent_api::design::DesignOf::Draft,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let level = |k: &str| d.catalog.get(k).map(|o| o.identity_level).unwrap();
    assert!(identity(1, level(BY_REF)).is_ok(), "{BY_REF} is level 1");
    assert!(
        identity(1, level("delivery.read")).is_err(),
        "its * is level 2"
    );
    assert!(identity(2, level("delivery.read")).is_ok());
}

/// (a) + (b): a variant the routed system serves with its `*` needs the state the serving operation requires (A2: the
/// `subject` key, not an identity level). With no `subject` in the conversation the call is refused `state_required`
/// with no call reaching the system; with it, it runs.
#[tokio::test]
async fn a_variant_served_by_a_higher_level_fallback_needs_the_fallbacks_level() {
    let fx = setup("runner-6b-level", Env::Test).await;
    case_with_conversation(&fx, "CS-61").await;
    // the route names the stub instance (FK); the fake executor, not its template, says what serves the call
    demo_stub(&fx).await;
    assert_levels(&fx).await;
    route(
        &fx,
        "delivery",
        "read",
        "instance",
        Some(DEMO_STUB_INSTANCE),
    )
    .await;
    let x = Serves::by("delivery.read", &[&["order_id"]]);
    let rn = runner(&fx, Some(Executors::default().with_instance(x.clone())));
    let input = json!({"tracking_ref": "3S-1", "order_id": "RET-01"});
    set_subject(&fx, "CS-61", false).await;
    let c = by_ref_call(&fx, "CS-61", "lvl1", input.clone(), 1);
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::Denied, "{res:?}");
    assert_eq!(res.outcome_code.as_deref(), Some("state_required"));
    assert!(res.output.is_none());
    assert_eq!(x.hits(), 0, "no call reaches the system");
    assert_eq!(call_row(&fx, &c.call_id).await.0, "denied");
    set_subject(&fx, "CS-61", true).await;
    let c = by_ref_call(&fx, "CS-61", "lvl2", input, 2);
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(x.hits(), 1);
}

/// (c): a system that offers the variant itself serves it at the variant's own level (1).
#[tokio::test]
async fn a_variant_the_system_offers_runs_at_its_own_level() {
    let fx = setup("runner-6b-native", Env::Test).await;
    case_with_conversation(&fx, "CS-62").await;
    // the route names the stub instance (FK); the fake executor, not its template, says what serves the call
    demo_stub(&fx).await;
    assert_levels(&fx).await;
    route(
        &fx,
        "delivery",
        "read",
        "instance",
        Some(DEMO_STUB_INSTANCE),
    )
    .await;
    let x = Serves::by(BY_REF, &[&["tracking_ref"]]);
    let rn = runner(&fx, Some(Executors::default().with_instance(x.clone())));
    let c = by_ref_call(&fx, "CS-62", "native", json!({"tracking_ref": "3S-1"}), 1);
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(x.hits(), 1);
}

/// A fallback whose request needs an input the variant does not carry (an order id for a tracking reference) is
/// refused `not_offered` before the call, at any level — never rendered half-empty, never another key's data.
#[tokio::test]
async fn a_fallback_missing_its_inputs_is_not_offered_before_the_call() {
    let fx = setup("runner-6b-inputs", Env::Test).await;
    case_with_conversation(&fx, "CS-63").await;
    // the route names the stub instance (FK); the fake executor, not its template, says what serves the call
    demo_stub(&fx).await;
    route(
        &fx,
        "delivery",
        "read",
        "instance",
        Some(DEMO_STUB_INSTANCE),
    )
    .await;
    let x = Serves::by("delivery.read", &[&["order_id"]]);
    let rn = runner(&fx, Some(Executors::default().with_instance(x.clone())));
    for (step, input) in [
        ("none", json!({"tracking_ref": "3S-1"})),
        ("blank", json!({"tracking_ref": "3S-1", "order_id": "  "})),
    ] {
        let c = by_ref_call(&fx, "CS-63", step, input, 3);
        let res = rn.handle(&c).await.unwrap();
        assert_eq!(res.state, ResultState::Denied, "{step}: {res:?}");
        assert_eq!(res.outcome_code.as_deref(), Some("not_offered"), "{step}");
    }
    assert_eq!(x.hits(), 0);
}

/// An instance executor whose serving operation cannot be read ([`OpExecutor::served`] errs): step 6b fails closed.
struct Unreadable {
    hits: AtomicUsize,
}

impl OpExecutor for Unreadable {
    fn execute<'a>(
        &'a self,
        _req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async move {
            self.hits.fetch_add(1, Ordering::SeqCst);
            outcome(OpState::Ok, None, None, Map::new())
        })
    }

    fn served<'a>(
        &'a self,
        _req: &'a OpRequest,
        _target: &'a ResolvedTarget,
    ) -> BoxFuture<'a, Result<Option<agent_api::runtime::runner::ops::Served>, String>> {
        Box::pin(std::future::ready(Err(
            "the serving operation cannot be read".to_owned(),
        )))
    }
}

/// Audit 3 of agent#64: when the operation serving a variant cannot be read (a load error, a repin mid-call), the
/// call is refused before the executor — never run at the variant's level on a guess.
#[tokio::test]
async fn a_variant_whose_serving_operation_cannot_be_read_is_refused() {
    let fx = setup("runner-6b-unreadable", Env::Test).await;
    case_with_conversation(&fx, "CS-65").await;
    demo_stub(&fx).await;
    route(
        &fx,
        "delivery",
        "read",
        "instance",
        Some(DEMO_STUB_INSTANCE),
    )
    .await;
    let x = Arc::new(Unreadable {
        hits: AtomicUsize::new(0),
    });
    let rn = runner(&fx, Some(Executors::default().with_instance(x.clone())));
    let c = by_ref_call(
        &fx,
        "CS-65",
        "unreadable",
        json!({"tracking_ref": "3S-1"}),
        3,
    );
    let res = rn.handle(&c).await.unwrap();
    assert_eq!(res.state, ResultState::Denied, "{res:?}");
    assert_eq!(res.outcome_code.as_deref(), Some("not_offered"), "{res:?}");
    assert_eq!(x.hits.load(Ordering::SeqCst), 0, "the executor never ran");
}

/// The real chain on the tenant's stub instance (`test_shop@1`, no `by_tracking_ref` operation: B7 serves the variant
/// with its order-keyed `delivery.read`): B7's executor reports the fallback, so a caller with no `subject` in
/// state quoting an order id is refused `state_required` (A2), a tracking reference alone is not offered, and a
/// known customer's order id runs.
#[tokio::test]
async fn the_demo_stub_serves_by_tracking_ref_with_its_order_keyed_read() {
    let fx = setup("runner-6b-demo", Env::Test).await;
    case_with_conversation(&fx, "CS-64").await;
    demo_stub(&fx).await;
    route(
        &fx,
        "delivery",
        "read",
        "instance",
        Some(DEMO_STUB_INSTANCE),
    )
    .await;
    let rn = runner(&fx, None);
    let with_order = json!({"tracking_ref": "3S-1", "order_id": "RET-01"});
    // A2: without a `subject` in state the order-keyed serving read is refused `state_required` (no level compare)
    set_subject(&fx, "CS-64", false).await;
    let res = rn
        .handle(&by_ref_call(&fx, "CS-64", "l1", with_order.clone(), 1))
        .await
        .unwrap();
    assert_eq!(
        (res.state, res.outcome_code.as_deref()),
        (ResultState::Denied, Some("state_required")),
        "l1: {res:?}"
    );
    set_subject(&fx, "CS-64", true).await;
    for (step, input, level, state, code) in [(
        "ref",
        json!({"tracking_ref": "SHP-9101"}),
        2,
        ResultState::Denied,
        Some("not_offered"),
    )] {
        let res = rn
            .handle(&by_ref_call(&fx, "CS-64", step, input, level))
            .await
            .unwrap();
        assert_eq!(
            (res.state, res.outcome_code.as_deref()),
            (state, code),
            "{step}: {res:?}"
        );
    }
    let res = rn
        .handle(&by_ref_call(&fx, "CS-64", "l2", with_order, 2))
        .await
        .unwrap();
    assert_ne!(res.state, ResultState::Denied, "{res:?}");
    assert_eq!(res.route_kind.as_deref(), Some("instance"));
}
