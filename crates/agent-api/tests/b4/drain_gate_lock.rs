//! B4 hardening (agent#54 audit, N-12: one open J21 item per conversation) — the drain's P1 gate handover and the
//! orchestrator's failed-case item serialise on the conversation's control lock (`store::control_key`).
//!
//! The drain checks for an open item in step 2, then calls the request gate, then creates the gate item in a LATER
//! transaction; the orchestrator's `queue_failed` (crate::runtime::orchestrator) takes the control lock, checks, and
//! queues the failed case's item. The window between the drain's step 2 and its gate item is exactly the gate call,
//! so the stub gate below lands a `queue_failed`-shaped item IN that window — deterministically, no timing luck:
//!
//! - committed before the gate answers: the gate path must re-check under the lock and absorb, not add a 2nd item;
//! - still in flight (control lock held, item inserted, commit 300 ms later): the gate path must wait on the lock
//!   and then absorb. Without the lock it would not see the uncommitted item and would queue a second one.
//!
//! Real Postgres 18 + Redis, a job on a job layer of the test's own, an OpenAI-compatible fake model on 127.0.0.1
//! as the tenant's internal `qualify` provider (the gate path never calls it), stub gate hooks via `drain::run_with`.
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_api::conversations::store;
use agent_api::design::JobRef;
use agent_api::runtime::gates::{
    Decision, GateVerdict, Gates, NoHint, NoReplyGate, RequestGate, RequestInput, VerdictCause,
};
use agent_api::runtime::runner::agent_cases::{self, Queued};
use agent_api::runtime::{Bus, Env, Scope};
use agent_api::state::AppState;
use agent_api::turn::{self, drain};
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::design_contract::ContractJob;
use crate::common::{self, TestEnv, json as req, send, super_admin};
use crate::redis_env as r;

/// The one space the fake model answers with (slots only; no procedure is needed to qualify).
const RETURNS: [crate::b4::it_desk_identity::OwnSpace; 1] =
    [crate::b4::it_desk_identity::OwnSpace {
        key: "retail:order-support:returns",
        slots: &["order_id", "reason"],
        kinds: &[],
    }];

const RETURN_ANSWER: &str = r#"{"context_space": "retail:order-support:returns", "slots": {"order_id": "ORD-10001", "reason": "too big"}}"#;

/// What the qualifier model answers when it recognises no space at all (2026-11-07 live: it did this for the same
/// close request on one turn and the space on another).
const NO_SPACE_ANSWER: &str = r#"{"context_space": null, "slots": {}}"#;

/// A no-space answer that still carries slots: the declared `reason` and the system lookup key's words for an order.
const NO_SPACE_WITH_SLOTS: &str = r#"{"context_space": null, "slots": {"reason": "too big", "order_id": "the one from 12 October"}}"#;

struct Fx {
    e: TestEnv,
    f: ContractJob,
    llm_calls: Arc<Mutex<usize>>,
    redis: (String, String),
}

/// An OpenAI-compatible `POST /v1/chat/completions` answering `answer` on every call (the qualification);
/// returns its url and call count.
async fn fake_llm_answer(answer: &'static str) -> (String, Arc<Mutex<usize>>) {
    let calls = Arc::new(Mutex::new(0usize));
    let seen = calls.clone();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |axum::Json(_): axum::Json<Value>| {
            let seen = seen.clone();
            async move {
                *seen.lock().unwrap() += 1;
                axum::Json(json!({
                    "choices": [{"message": {"role": "assistant", "content": answer}}],
                    "usage": {"prompt_tokens": 11, "completion_tokens": 7},
                }))
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app).await.unwrap();
    });
    (format!("http://{addr}/v1"), calls)
}

/// A platform-owned tenant's job (its `qualify` use is the fake model) on a job layer of the test's own with the `returns` space.
async fn setup(name: &str) -> Fx {
    setup_with(name, RETURN_ANSWER).await
}

/// [`setup`] with the fake model answering `answer` (e.g. [`NO_SPACE_ANSWER`] for the one-space fallback).
async fn setup_with(name: &str, answer: &'static str) -> Fx {
    let (url, llm_calls) = fake_llm_answer(answer).await;
    let vars = r::api_vars(name).await;
    let redis = (vars[0].1.clone(), vars[1].1.clone());
    let e = common::env_with(|v| {
        v.extend(vars.clone());
        v.insert("INTERNAL_LLM_URL".into(), url);
    })
    .await;
    let f = crate::b4::it_desk_identity::own_job(&e, "returns_desk", &RETURNS, false).await;
    sqlx::query("UPDATE tenants SET platform_owned = true WHERE id = $1")
        .bind(f.tenant)
        .execute(&e.owner)
        .await
        .unwrap();
    Fx {
        e,
        f,
        llm_calls,
        redis,
    }
}

/// A super_admin acting in the fixture's tenant.
async fn call(fx: &Fx, method: &str, tail: &str, body: Value) -> (StatusCode, Value) {
    let uri = format!("/api/jobs/{}/{tail}", fx.f.job.job_key);
    let mut q = req(method, &uri, Some(&super_admin()), body);
    q.headers_mut().insert(
        "x-active-tenant-id",
        fx.f.tenant.to_string().parse().unwrap(),
    );
    send(&fx.e.app, q).await
}

/// A test-env staff chat as a test customer.
async fn start(fx: &Fx) -> String {
    let (s, b) = call(
        fx,
        "POST",
        "conversations",
        json!({"env": "test", "customer_email": "ann@shop.example"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    b["conversation_id"].as_str().unwrap().to_string()
}

async fn bus(fx: &Fx) -> Bus {
    let scope = Scope::new(fx.f.tenant, fx.f.job.job_key.clone(), Env::Test).unwrap();
    Bus::connect_with_response_timeout(&fx.redis.0, &fx.redis.1, Duration::from_secs(5))
        .await
        .unwrap()
        .scoped(&scope)
}

/// What `sendChatMessage` does up to the spawn (store, inbox, lock), then the drain itself with `gates`, awaited.
/// Returns the stored customer message id.
async fn drive(fx: &Fx, conv: &str, text: &str, gates: Gates) -> Uuid {
    let st = &fx.e.state;
    let b = bus(fx).await;
    let mut tx = store::begin_job(&st.db, fx.f.tenant, fx.f.job.job_id)
        .await
        .unwrap();
    let stored = store::append(
        &mut tx,
        store::NewMessage {
            job_id: fx.f.job.job_id,
            conversation_id: conv,
            role: "customer",
            author: "Ann de Vries",
            text,
            case_id: None,
            external_id: None,
        },
    )
    .await
    .unwrap()
    .unwrap();
    tx.commit().await.unwrap();
    let _: i64 = redis::cmd("RPUSH")
        .arg(turn::chat_key(&b, conv, ":inbox"))
        .arg(stored.message_id.to_string())
        .query_async(&mut b.conn())
        .await
        .unwrap();
    let token = Uuid::new_v4().to_string();
    assert!(turn::acquire(&b, conv, &token).await.unwrap());
    let ctx = drain::TurnCtx {
        tenant: fx.f.tenant,
        job_id: fx.f.job.job_id,
        job_key: fx.f.job.job_key.clone(),
        env: Env::Test,
        conversation_id: conv.to_string(),
    };
    drain::run_with(st.clone(), ctx, token, gates).await;
    stored.message_id
}

/// A P1 hook that, while the drain waits on it (after step 2, before the gate item), queues a failed case's J21
/// item the way `queue_failed` does: control lock first, open-item check, `agent_cases::queue` with `tool_failed`.
/// `hold`: `None` commits before answering; `Some(d)` answers at once and commits `d` later (lock held meanwhile).
/// Then it answers a fired P1 handover.
struct FailedCaseInTheWindow {
    hold: Option<Duration>,
    calls: Arc<AtomicUsize>,
    /// The failed case's id, once queued.
    case_id: Arc<Mutex<Option<String>>>,
}

impl RequestGate for FailedCaseInTheWindow {
    fn check<'a>(
        &'a self,
        st: &'a AppState,
        input: RequestInput<'a>,
    ) -> Pin<Box<dyn Future<Output = GateVerdict> + Send + 'a>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let job = JobRef {
                id: input.job.id,
                key: input.job.key.clone(),
            };
            let design = input.design.case.clone();
            let mut tx = store::begin_job(&st.db, input.tenant, job.id)
                .await
                .unwrap();
            store::lock(&mut tx, &store::control_key(input.conversation_id))
                .await
                .unwrap();
            assert!(
                agent_api::conversations::handover::open_for_conversation(
                    &mut tx,
                    input.conversation_id
                )
                .await
                .unwrap()
                .is_none(),
                "the conversation starts with no open item"
            );
            let conv = store::conversation_or_404(&mut tx, input.conversation_id)
                .await
                .unwrap();
            let case_id = store::create_case(&mut tx, job.id, &conv, "a failed case", &design)
                .await
                .unwrap();
            let q = agent_cases::queue(
                &mut tx,
                &job,
                &case_id,
                input.conversation_id,
                "tool_failed",
                "case failed: test",
                "retail:order-support:returns",
                &design,
            )
            .await
            .unwrap();
            assert!(
                matches!(q, Queued::Created(_)),
                "the failed case queues its item"
            );
            *self.case_id.lock().unwrap() = Some(case_id);
            match self.hold {
                None => tx.commit().await.unwrap(),
                Some(d) => {
                    tokio::spawn(async move {
                        tokio::time::sleep(d).await;
                        tx.commit().await.unwrap();
                    });
                }
            }
            GateVerdict {
                decision: Decision::Handover,
                cause: VerdictCause::Fired,
                deciding: vec!["gs".to_string()],
                evaluation_id: Some(54),
                answers: Vec::new(),
            }
        })
    }
}

fn gates(hold: Option<Duration>) -> (Gates, Arc<AtomicUsize>, Arc<Mutex<Option<String>>>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let case_id = Arc::new(Mutex::new(None));
    let g = Gates {
        request: Arc::new(FailedCaseInTheWindow {
            hold,
            calls: calls.clone(),
            case_id: case_id.clone(),
        }),
        hinter: Arc::new(NoHint),
        reply: Arc::new(NoReplyGate),
    };
    (g, calls, case_id)
}

fn is_case_id(id: &str) -> bool {
    id.strip_prefix("CS-").is_some_and(|n| {
        !n.is_empty() && !n.starts_with('0') && n.chars().all(|c| c.is_ascii_digit())
    })
}

/// The race's outcome: one open item (the failed case's, `tool_failed`), no case of the gate path, and the turn's
/// customer message joined to the failed case.
async fn assert_one_item(fx: &Fx, conv: &str, msg: Uuid, failed_case: &str) {
    assert!(is_case_id(failed_case), "{failed_case}");
    let items: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT reason, state, case_id FROM agent_handover_items WHERE conversation_id = $1 ORDER BY id",
    )
    .bind(conv)
    .fetch_all(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(
        items.len(),
        1,
        "N-12: one J21 item per conversation: {items:?}"
    );
    assert_eq!(items[0].0, "tool_failed", "{items:?}");
    assert_eq!(items[0].2, failed_case, "{items:?}");
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agent_handover_items \
          WHERE conversation_id = $1 AND state NOT IN ('resolved', 'returned_to_agent')",
    )
    .bind(conv)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(open, 1);
    let cases: Vec<String> =
        sqlx::query_scalar("SELECT case_id FROM agent_case WHERE conversation_id = $1")
            .bind(conv)
            .fetch_all(&fx.e.owner)
            .await
            .unwrap();
    assert_eq!(
        cases,
        vec![failed_case.to_string()],
        "the gate path made no case"
    );
    let linked: Option<String> =
        sqlx::query_scalar("SELECT case_id FROM conversation_message WHERE message_id = $1")
            .bind(msg)
            .fetch_one(&fx.e.owner)
            .await
            .unwrap();
    assert_eq!(
        linked.as_deref(),
        Some(failed_case),
        "the message joined the open item's case"
    );
    assert_eq!(
        *fx.llm_calls.lock().unwrap(),
        0,
        "a P1 handover never qualifies"
    );
}

#[tokio::test]
async fn gate_handover_absorbs_a_failed_case_item_committed_after_step_2() {
    let fx = setup("b4-lock-seq").await;
    let conv = start(&fx).await;
    let (g, calls, case_id) = gates(None);
    let msg = drive(&fx, &conv, "I want to return ORD-10001", g).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the turn reached P1");
    let failed = case_id
        .lock()
        .unwrap()
        .clone()
        .expect("the failed case was queued");
    assert_one_item(&fx, &conv, msg, &failed).await;
}

#[tokio::test]
async fn gate_handover_waits_for_an_in_flight_failed_case_item() {
    let fx = setup("b4-lock-race").await;
    let conv = start(&fx).await;
    // the failed case holds the control lock with its item inserted but uncommitted while the drain's gate path
    // runs: without the lock the drain would not see that item and would queue a second one
    let (g, calls, case_id) = gates(Some(Duration::from_millis(300)));
    let msg = drive(&fx, &conv, "I want to return ORD-10001", g).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the turn reached P1");
    let failed = case_id
        .lock()
        .unwrap()
        .clone()
        .expect("the failed case was queued");
    assert_one_item(&fx, &conv, msg, &failed).await;
}

/// 2026-11-07 live: the devops job has exactly ONE context space, and the qualifier model answered the same close
/// request with no space on one turn and the space on another. A one-space job must not depend on that classifier:
/// an unqualified turn still runs in its only space, and the customer never sees a menu of one.
#[tokio::test]
async fn a_one_space_job_runs_an_unqualified_turn_in_its_only_space_not_the_menu() {
    let fx = setup_with("single-space-fallback", NO_SPACE_ANSWER).await;
    let conv = start(&fx).await;
    drive(
        &fx,
        &conv,
        "Please close the release readiness project you just set up for us.",
        Gates::default(),
    )
    .await;

    let space: Option<String> = sqlx::query_scalar(
        "SELECT context_space FROM agent_case WHERE tenant_id = $1 AND conversation_id = $2 \
         ORDER BY started_at DESC LIMIT 1",
    )
    .bind(fx.f.tenant)
    .bind(&conv)
    .fetch_optional(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(
        space.as_deref(),
        Some(RETURNS[0].key),
        "the only space is chosen deterministically"
    );

    let menus: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM conversation_message WHERE conversation_id = $1 AND role = 'agent' \
         AND text LIKE 'I can help with:%'",
    )
    .bind(&conv)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(menus, 0, "a one-space job has no menu to offer");
}

/// The one-space fallback (#135) must not throw away what the qualifier did extract: the declared slot folds into
/// the only space, a system slot's words become the transient hint, and the lookup key itself is never kept.
#[tokio::test]
async fn a_one_space_job_folds_an_unqualified_turns_slots_into_its_only_space() {
    let fx = setup_with("single-space-slots", NO_SPACE_WITH_SLOTS).await;
    let conv = start(&fx).await;
    drive(
        &fx,
        &conv,
        "I'd like to return the rug, it's too big.",
        Gates::default(),
    )
    .await;

    let body: Value = sqlx::query_scalar(
        "SELECT e.body FROM case_event e JOIN agent_case c USING (tenant_id, job_id, case_id) \
          WHERE c.conversation_id = $1 AND e.kind = 'qualified' ORDER BY e.seq LIMIT 1",
    )
    .bind(&conv)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(
        body["context_space"].as_str(),
        Some(RETURNS[0].key),
        "the only space is chosen deterministically: {body}"
    );
    assert_eq!(
        body["slots"]["reason"],
        json!("too big"),
        "the declared slot is kept: {body}"
    );
    assert_eq!(
        body["slots"]["item_hint"],
        json!("the one from 12 October"),
        "the system slot's words are the hint: {body}"
    );
    assert!(
        body["slots"].get("order_id").is_none(),
        "a lookup key is never kept: {body}"
    );
}
