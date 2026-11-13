//! B4 T9b — gate verdict causes (D94, R15) and the route-hint seam (F-19) in the drain (crate::turn::drain,
//! crate::runtime::gates): a P1 handover that fired is `gate_blocked`, one whose gate service did not answer is
//! `gate_unavailable`, both raise ONE item per conversation (N-12: the drain's step 2 absorbs later messages), and
//! the route hint is `null` on the `qualified` event and `request.qualified` until B5's hinter answers. Real
//! Postgres 18 + Redis, a job on a job layer of the test's own, an OpenAI-compatible fake model on 127.0.0.1 as the
//! tenant's internal `qualify` provider, and stub gate hooks passed to `drain::run_with`.
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_api::conversations::store;
use agent_api::runtime::design::RuntimeDesign;
use agent_api::runtime::gates::{
    Decision, GateVerdict, Gates, NoHint, RequestGate, RequestInput, RouteHint, RouteHinter,
    VerdictCause,
};
use agent_api::runtime::{Bus, Env, Scope};
use agent_api::state::AppState;
use agent_api::turn::{self, drain};
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::design_contract::ContractJob;
use crate::common::{self, TestEnv, json as req, send, super_admin};
use crate::redis_env as r;

/// A returns request with both slots: one message completes it.
/// The one space the fake model answers with (slots only; no procedure is needed to qualify).
const RETURNS: [crate::b4::it_desk_identity::OwnSpace; 1] =
    [crate::b4::it_desk_identity::OwnSpace {
        key: "retail:order-support:returns",
        slots: &["order_id", "reason"],
        kinds: &[],
    }];

const RETURN_ANSWER: &str = r#"{"context_space": "retail:order-support:returns", "slots": {"order_id": "ORD-10001", "reason": "too big"}}"#;

struct Fx {
    e: TestEnv,
    f: ContractJob,
    llm_calls: Arc<Mutex<usize>>,
    redis: (String, String),
}

impl Fx {
    fn llm_calls(&self) -> usize {
        *self.llm_calls.lock().unwrap()
    }
}

/// An OpenAI-compatible `POST /v1/chat/completions` answering [`RETURN_ANSWER`]; returns its url and call count.
async fn fake_llm() -> (String, Arc<Mutex<usize>>) {
    let calls = Arc::new(Mutex::new(0usize));
    let seen = calls.clone();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |axum::Json(_): axum::Json<Value>| {
            let seen = seen.clone();
            async move {
                *seen.lock().unwrap() += 1;
                axum::Json(json!({
                    "choices": [{"message": {"role": "assistant", "content": RETURN_ANSWER}}],
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
    let (url, llm_calls) = fake_llm().await;
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

fn test_scope(fx: &Fx) -> Scope {
    Scope::new(fx.f.tenant, fx.f.job.job_key.clone(), Env::Test).unwrap()
}

async fn bus(fx: &Fx) -> Bus {
    Bus::connect_with_response_timeout(&fx.redis.0, &fx.redis.1, Duration::from_secs(5))
        .await
        .unwrap()
        .scoped(&test_scope(fx))
}

/// What `sendChatMessage` does up to the spawn (store, inbox, lock), then the drain itself with `gates`, awaited.
async fn drive(fx: &Fx, conv: &str, text: &str, gates: Gates) {
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
}

/// A P1 hook answering `verdict` and counting its calls.
struct StubGate {
    verdict: GateVerdict,
    calls: Arc<AtomicUsize>,
}

impl RequestGate for StubGate {
    fn check<'a>(
        &'a self,
        _st: &'a AppState,
        _input: RequestInput<'a>,
    ) -> Pin<Box<dyn Future<Output = GateVerdict> + Send + 'a>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let v = self.verdict.clone();
        Box::pin(async move { v })
    }
}

fn handover_verdict(cause: VerdictCause) -> GateVerdict {
    GateVerdict {
        decision: Decision::Handover,
        cause,
        deciding: vec!["gs".to_string()],
        evaluation_id: Some(41),
        answers: Vec::new(),
    }
}

/// Gates whose P1 answers `verdict` (B4's no-hint hinter); the returned counter counts P1 calls.
fn stub_gates(verdict: GateVerdict) -> (Gates, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let gates = Gates {
        request: Arc::new(StubGate {
            verdict,
            calls: calls.clone(),
        }),
        hinter: Arc::new(NoHint),
        ..Gates::default()
    };
    (gates, calls)
}

/// `(reason, state, case_id, trigger)` of every handover item of `conv`.
async fn items(fx: &Fx, conv: &str) -> Vec<(String, String, String, String)> {
    sqlx::query_as(
        "SELECT reason, state, case_id, trigger FROM agent_handover_items WHERE conversation_id = $1 ORDER BY id",
    )
    .bind(conv)
    .fetch_all(&fx.e.owner)
    .await
    .unwrap()
}

async fn count(fx: &Fx, sql: &'static str, conv: &str) -> i64 {
    sqlx::query_scalar(sql)
        .bind(conv)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap()
}

const CASES: &str = "SELECT count(*) FROM agent_case WHERE conversation_id = $1";
const AGENT_MSGS: &str =
    "SELECT count(*) FROM conversation_message WHERE conversation_id = $1 AND role = 'agent'";

/// The case's event kinds, oldest first.
async fn kinds(fx: &Fx, case_id: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT kind FROM case_event WHERE case_id = $1 ORDER BY seq")
        .bind(case_id)
        .fetch_all(&fx.e.owner)
        .await
        .unwrap()
}

async fn event_body(fx: &Fx, conv: &str, kind: &'static str) -> Value {
    sqlx::query_scalar(
        "SELECT e.body FROM case_event e JOIN agent_case c USING (tenant_id, job_id, case_id) \
          WHERE c.conversation_id = $1 AND e.kind = $2 ORDER BY e.seq LIMIT 1",
    )
    .bind(conv)
    .bind(kind)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap()
}

#[test]
fn verdict_maps_to_the_two_reasons() {
    assert_eq!(
        handover_verdict(VerdictCause::Fired).handover_reason(),
        Some("gate_blocked")
    );
    assert_eq!(
        handover_verdict(VerdictCause::Unavailable).handover_reason(),
        Some("gate_unavailable")
    );
    assert_eq!(GateVerdict::allow().handover_reason(), None);
    let block = GateVerdict {
        decision: Decision::Block,
        ..handover_verdict(VerdictCause::Fired)
    };
    assert_eq!(block.handover_reason(), None);
    assert_eq!(
        serde_json::to_value(handover_verdict(VerdictCause::Unavailable)).unwrap(),
        json!({"decision": "handover", "cause": "unavailable", "deciding": ["gs"], "evaluation_id": 41})
    );
}

#[tokio::test]
async fn fired_closed_gate_is_gate_blocked() {
    let fx = setup("t9b-fired").await;
    let conv = start(&fx).await;
    let (gates, calls) = stub_gates(handover_verdict(VerdictCause::Fired));
    drive(&fx, &conv, "I want to return ORD-10001", gates).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let got = items(&fx, &conv).await;
    assert_eq!(got.len(), 1, "{got:?}");
    let (reason, state, case_id, trigger) = &got[0];
    assert_eq!(reason, "gate_blocked");
    assert_eq!(state, "queued");
    assert_eq!(trigger, "a request gate fired");
    let (status, esc): (String, Option<String>) =
        sqlx::query_as("SELECT status, escalation_reason FROM agent_case WHERE case_id = $1")
            .bind(case_id)
            .fetch_one(&fx.e.owner)
            .await
            .unwrap();
    assert_eq!(
        (status.as_str(), esc.as_deref()),
        ("handover", Some("gate_blocked"))
    );
    assert_eq!(kinds(&fx, case_id).await, ["message", "gate"]);
    let gate = event_body(&fx, &conv, "gate").await;
    assert_eq!(gate["point"], "P1");
    assert_eq!(gate["cause"], "fired");
    assert_eq!(gate["deciding"], json!(["gs"]));
    assert_eq!(gate["evaluation_id"], 41);
    let linked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM conversation_message WHERE conversation_id = $1 AND role = 'customer' \
            AND case_id = $2",
    )
    .bind(&conv)
    .bind(case_id)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(linked, 1, "the message joins the handed-over case");
    assert_eq!(fx.llm_calls(), 0, "a handover verdict asks no qualifier");
    assert_eq!(
        count(&fx, AGENT_MSGS, &conv).await,
        1,
        "one handover notice"
    );
}

#[tokio::test]
async fn laya_outage_is_gate_unavailable() {
    let fx = setup("t9b-outage").await;
    let conv = start(&fx).await;
    let (gates, _) = stub_gates(handover_verdict(VerdictCause::Unavailable));
    drive(&fx, &conv, "I want to return ORD-10001", gates).await;
    let got = items(&fx, &conv).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].0, "gate_unavailable");
    assert_eq!(got[0].3, "the gate service did not answer");
    assert_eq!(event_body(&fx, &conv, "gate").await["cause"], "unavailable");
    assert_eq!(fx.llm_calls(), 0);
    // a gate that failed OPEN (allow + unavailable) is no handover: the turn qualifies
    let conv2 = start(&fx).await;
    let open = GateVerdict {
        decision: Decision::Allow,
        cause: VerdictCause::Unavailable,
        deciding: Vec::new(),
        evaluation_id: None,
        answers: Vec::new(),
    };
    let (gates, _) = stub_gates(open);
    drive(
        &fx,
        &conv2,
        "I want to return ORD-10001, it is too big",
        gates,
    )
    .await;
    assert!(items(&fx, &conv2).await.is_empty());
    assert_eq!(count(&fx, CASES, &conv2).await, 1);
    assert_eq!(fx.llm_calls(), 1);
}

#[tokio::test]
async fn gate_unavailable_follows_one_item_per_conversation() {
    let fx = setup("t9b-per-conv").await;
    let a = start(&fx).await;
    let b = start(&fx).await;
    for text in ["hello", "is anyone there?", "I want to return ORD-10001"] {
        let (gates, _) = stub_gates(handover_verdict(VerdictCause::Unavailable));
        drive(&fx, &a, text, gates).await;
    }
    let (gates, _) = stub_gates(handover_verdict(VerdictCause::Unavailable));
    drive(&fx, &b, "where is my order?", gates).await;
    let (ia, ib) = (items(&fx, &a).await, items(&fx, &b).await);
    assert_eq!(ia.len(), 1, "{ia:?}");
    assert_eq!(ib.len(), 1, "{ib:?}");
    assert_ne!(ia[0].2, ib[0].2, "each conversation has its own case");
    assert!(
        ia.iter()
            .chain(ib.iter())
            .all(|i| i.0 == "gate_unavailable")
    );
    let total: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_handover_items WHERE job_id = $1")
            .bind(fx.f.job.job_id)
            .fetch_one(&fx.e.owner)
            .await
            .unwrap();
    assert_eq!(total, 2, "one item per conversation");
    assert_eq!(count(&fx, CASES, &a).await, 1);
}

#[tokio::test]
async fn no_hint_before_b5_serialises_null() {
    let fx = setup("t9b-hint").await;
    // B4's hooks through the real send path: the hint is present and null
    let conv = start(&fx).await;
    let (s, b) = call(
        &fx,
        "POST",
        &format!("conversations/{conv}/messages"),
        json!({"text": "I want to return ORD-10001, it is too big"}),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let until = Instant::now() + Duration::from_secs(15);
    while count(&fx, CASES, &conv).await < 1 {
        assert!(Instant::now() < until, "the turn never qualified");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let until = Instant::now() + Duration::from_secs(15);
    let qualified = loop {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM case_event e JOIN agent_case c USING (tenant_id, job_id, case_id) \
              WHERE c.conversation_id = $1 AND e.kind = 'qualified'",
        )
        .bind(&conv)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap();
        if n > 0 {
            break event_body(&fx, &conv, "qualified").await;
        }
        assert!(Instant::now() < until, "no qualified event");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        qualified.as_object().unwrap().contains_key("route_hint"),
        "{qualified}"
    );
    assert_eq!(qualified["route_hint"], Value::Null);
    let b = bus(&fx).await;
    let entries = r::entries(&b, &b.streams.customer("order-support")).await;
    let mine: Vec<_> = entries
        .iter()
        .filter(|e| e.payload["conversation_id"] == conv)
        .collect();
    assert_eq!(mine.len(), 1);
    assert!(
        mine[0]
            .payload
            .as_object()
            .unwrap()
            .contains_key("route_hint")
    );
    assert_eq!(mine[0].payload["route_hint"], Value::Null);
    // D108 (KH T5): the layer markers of the qualifier's one system message ride on the case's `qualified` event (the
    // record a prod turn writes too) and on the request the orchestrator gets; the job's identity is its own layer's
    let layers = qualified["layers"].as_array().expect("qualified.layers");
    let version: i32 = sqlx::query_scalar(
        "SELECT max(version) FROM job_template_tag WHERE layer = 'job/returns_desk' AND status = 'tagged'",
    )
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    let marker = format!("[layer job/returns_desk@v{version}]");
    assert!(layers.iter().any(|l| *l == marker.as_str()), "{qualified}");
    assert!(
        layers
            .iter()
            .all(|l| l.as_str().is_some_and(|s| s.starts_with("[layer "))),
        "{qualified}"
    );
    assert_eq!(mine[0].payload["layers"], qualified["layers"]);
    assert_eq!(
        qualified["prompt_warning"],
        Value::Null,
        "the template compiled"
    );

    // a hinter that answers: the same two places carry it (the seam B5 T10a fills)
    struct Fixed;
    impl RouteHinter for Fixed {
        fn hint(&self, _p1: &GateVerdict, _d: &RuntimeDesign) -> Option<RouteHint> {
            Some(RouteHint {
                request_type: "return".into(),
                stage: Some("after_sale".into()),
                activity: Some("return_item".into()),
                confidence: 0.75,
            })
        }
    }
    let conv2 = start(&fx).await;
    let gates = Gates {
        hinter: Arc::new(Fixed),
        ..Gates::default()
    };
    drive(
        &fx,
        &conv2,
        "I want to return ORD-10001, it is too big",
        gates,
    )
    .await;
    let want = json!({"request_type": "return", "stage": "after_sale", "activity": "return_item", "confidence": 0.75});
    assert_eq!(
        event_body(&fx, &conv2, "qualified").await["route_hint"],
        want
    );
    let entries = r::entries(&b, &b.streams.customer("order-support")).await;
    let hinted: Vec<_> = entries
        .iter()
        .filter(|e| e.payload["conversation_id"] == conv2)
        .collect();
    assert_eq!(hinted.len(), 1);
    assert_eq!(hinted[0].payload["route_hint"], want);
}

#[tokio::test]
async fn gate_outage_raises_one_handover_per_conversation() {
    let fx = setup("t9b-n12").await;
    let conv = start(&fx).await;
    let calls = Arc::new(AtomicUsize::new(0));
    for text in ["I want to return ORD-10001", "hello?", "please answer"] {
        let gates = Gates {
            request: Arc::new(StubGate {
                verdict: handover_verdict(VerdictCause::Unavailable),
                calls: calls.clone(),
            }),
            hinter: Arc::new(NoHint),
            ..Gates::default()
        };
        drive(&fx, &conv, text, gates).await;
    }
    let got = items(&fx, &conv).await;
    assert_eq!(got.len(), 1, "three messages, one item: {got:?}");
    assert_eq!(got[0].0, "gate_unavailable");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "later messages join the open item before P1"
    );
    let on_case: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM conversation_message WHERE conversation_id = $1 AND role = 'customer' \
            AND case_id = $2",
    )
    .bind(&conv)
    .bind(&got[0].2)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(on_case, 3, "every message is on the handed-over case");
    assert_eq!(count(&fx, CASES, &conv).await, 1);
    assert_eq!(count(&fx, AGENT_MSGS, &conv).await, 1, "one notice");
    assert_eq!(fx.llm_calls(), 0);
}
