//! B4 T9 — the staff test chat turn (crate::turn, crate::routes::{chat, chat_profile}): `startChat`,
//! `sendChatMessage` (202 before the turn runs, 409 busy storing nothing), the drain (gate 2, qualify through the
//! job's `qualify` binding, usage, the stamped case, `request.qualified`), card redaction (N-03) and the demo
//! customers of the stub instance (Q-B4-2). Real Postgres 18 + Redis, the real router, a job created from
//! `job/customer_care_agent@v4`, and an OpenAI-compatible fake model on 127.0.0.1 serving the tenant as its internal
//! (platform-owned, D70) `qualify` provider.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_api::runtime::{Bus, Env, Scope};
use axum::http::StatusCode;
use futures_util::StreamExt;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::design_contract::{self, ContractJob};
use crate::common::{self, TestEnv, json as req, send, super_admin};
use crate::redis_env as r;

/// The spaces the fake model answers with (the job layer of this test's own; no procedure is needed to qualify): a
/// returns request, an order-status question, and a general (FAQ) question served from knowledge.
const RETURNS: [crate::b4::it_desk_identity::OwnSpace; 3] = [
    crate::b4::it_desk_identity::OwnSpace {
        key: "retail:order-support:returns",
        slots: &["order_id", "reason"],
        kinds: &[],
    },
    crate::b4::it_desk_identity::OwnSpace {
        key: "retail:order-support:order_status",
        slots: &["order_id"],
        kinds: &[],
    },
    crate::b4::it_desk_identity::OwnSpace {
        key: "retail:faq:shipping",
        slots: &[],
        kinds: &["faq"],
    },
];

/// What the fake model answers: a returns request with both slots, so one message completes it.
const RETURN_ANSWER: &str = r#"{"context_space": "retail:order-support:returns", "slots": {"order_id": "ORD-10001", "reason": "too big"}}"#;

/// An OpenAI-compatible `POST /v1/chat/completions` that records every body and answers `content` after `delay`.
struct FakeLlm {
    url: String,
    bodies: Arc<Mutex<Vec<Value>>>,
    /// Each call's `X-App-Id`, `X-Project-Id` and `X-Actor-Id` (the router identity; `None` when absent).
    ids: Arc<Mutex<Vec<[Option<String>; 3]>>>,
    /// Each call's `Authorization` header (`None` when absent): which licence the caller presented.
    auths: Arc<Mutex<Vec<Option<String>>>>,
}

/// What the platform presents to the router for itself (`INTERNAL_LLM_KEY`); a tenant must never present it.
const PLATFORM_KEY: &str = "platform-router-key";
/// A provisioned tenant's own inference-router licence token.
const TENANT_ROUTER_TOKEN: &str = "tenant-router-token";

/// Store `token` as `tenant`'s `app_key` licence as the fake bundle minter (common::minter, shared by every test env) hands it out on demand.
async fn seed_licence(_e: &TestEnv, tenant: Uuid, app_key: &str, token: &str) {
    common::minter::grant_shared(tenant, app_key, token);
}

impl FakeLlm {
    fn auths(&self) -> Vec<Option<String>> {
        self.auths.lock().unwrap().clone()
    }

    fn calls(&self) -> usize {
        self.bodies.lock().unwrap().len()
    }
}

async fn fake_llm(content: &'static str, delay: Duration) -> FakeLlm {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let ids: Arc<Mutex<Vec<[Option<String>; 3]>>> = Arc::default();
    let auths: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let seen = bodies.clone();
    let identified = ids.clone();
    let authed = auths.clone();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(
            move |h: axum::http::HeaderMap, axum::Json(v): axum::Json<Value>| {
                let seen = seen.clone();
                let identified = identified.clone();
                let authed = authed.clone();
                async move {
                    let get = |k: &str| h.get(k).and_then(|x| x.to_str().ok()).map(String::from);
                    authed.lock().unwrap().push(get("authorization"));
                    identified.lock().unwrap().push([
                        get("x-app-id"),
                        get("x-project-id"),
                        get("x-actor-id"),
                    ]);
                    seen.lock().unwrap().push(v);
                    tokio::time::sleep(delay).await;
                    axum::Json(json!({
                        "choices": [{"message": {"role": "assistant", "content": content}}],
                        "usage": {"prompt_tokens": 11, "completion_tokens": 7},
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
    FakeLlm {
        url: format!("http://{addr}/v1"),
        bodies,
        ids,
        auths,
    }
}

struct Fx {
    e: TestEnv,
    f: ContractJob,
    llm: FakeLlm,
    /// The test's Redis URL and `AGENT_PREFIX`.
    redis: (String, String),
}

/// A job of its own tenant (platform-owned: its `qualify` use is the fake model), on a job layer of the test's own with the
/// `returns` space.
async fn setup(name: &str, delay: Duration) -> Fx {
    setup_with(name, delay, &[]).await
}

/// [`setup`] with extra API variables.
async fn setup_with(name: &str, delay: Duration, extra: &[(&str, String)]) -> Fx {
    setup_answering(name, delay, extra, RETURN_ANSWER).await
}

/// [`setup_with`] whose fake model answers `answer` to every qualifier call.
async fn setup_answering(
    name: &str,
    delay: Duration,
    extra: &[(&str, String)],
    answer: &'static str,
) -> Fx {
    let llm = fake_llm(answer, delay).await;
    let vars = r::api_vars(name).await;
    let redis = (vars[0].1.clone(), vars[1].1.clone());
    let url = llm.url.clone();
    let e = common::env_with(|v| {
        v.extend(vars.clone());
        v.insert("INTERNAL_LLM_URL".into(), url);
        v.insert("INTERNAL_LLM_KEY".into(), PLATFORM_KEY.into());
        for (k, val) in extra {
            v.insert((*k).to_string(), val.clone());
        }
    })
    .await;
    let f = crate::b4::it_desk_identity::own_job(&e, "returns_desk", &RETURNS, false).await;
    sqlx::query("UPDATE tenants SET platform_owned = true WHERE id = $1")
        .bind(f.tenant)
        .execute(&e.owner)
        .await
        .unwrap();
    Fx { e, f, llm, redis }
}

/// A job of a tenant that is NOT platform-owned and binds no `qualify` provider. The router is the fake; when
/// `provisioned` the tenant holds its own inference-router licence ([`TENANT_ROUTER_TOKEN`]), else none (fail closed).
async fn setup_tenant(name: &str, provisioned: bool) -> Fx {
    let llm = fake_llm(RETURN_ANSWER, Duration::ZERO).await;
    let vars = r::api_vars(name).await;
    let redis = (vars[0].1.clone(), vars[1].1.clone());
    let url = llm.url.clone();
    let tenant = Uuid::new_v4();
    let e = common::env_with(|v| {
        v.extend(vars.clone());
        v.insert("INTERNAL_LLM_URL".into(), url);
        v.insert("INTERNAL_LLM_KEY".into(), PLATFORM_KEY.into());
    })
    .await;
    let f = design_contract::new_job_for_on(
        &e,
        tenant,
        crate::b4::it_desk_identity::own_template(&e, "returns_desk", &RETURNS).await,
    )
    .await;
    if provisioned {
        seed_licence(&e, tenant, "inference-router", TENANT_ROUTER_TOKEN).await;
    }
    Fx { e, f, llm, redis }
}

fn uri(fx: &Fx, tail: &str) -> String {
    format!("/api/jobs/{}/{tail}", fx.f.job.job_key)
}

/// A super_admin acting in the fixture's tenant.
async fn call(fx: &Fx, method: &str, tail: &str, body: Value) -> (StatusCode, Value) {
    let mut q = if method == "GET" {
        common::get(&uri(fx, tail), Some(&super_admin()))
    } else {
        req(method, &uri(fx, tail), Some(&super_admin()), body)
    };
    q.headers_mut().insert(
        "x-active-tenant-id",
        fx.f.tenant.to_string().parse().unwrap(),
    );
    send(&fx.e.app, q).await
}

/// Start a staff test chat as the test customer `customer` (an email; a bare name becomes `<name>@test.example`).
async fn start(fx: &Fx, env: &str, customer: &str) -> String {
    let email = if customer.contains('@') {
        customer.to_owned()
    } else {
        format!("{customer}@test.example")
    };
    let (s, b) = call(
        fx,
        "POST",
        "conversations",
        json!({"env": env, "customer_email": email}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["channel"], "staff");
    b["conversation_id"].as_str().unwrap().to_string()
}

async fn say(fx: &Fx, conv: &str, text: &str) -> (StatusCode, Value) {
    call(
        fx,
        "POST",
        &format!("conversations/{conv}/messages"),
        json!({"text": text}),
    )
    .await
}

/// Poll `sql` (one bigint, `$1` = the conversation) until it is at least `n` (15 s).
async fn wait_count(fx: &Fx, sql: &'static str, conv: &str, n: i64) -> i64 {
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        let got: i64 = sqlx::query_scalar(sql)
            .bind(conv)
            .fetch_one(&fx.e.owner)
            .await
            .unwrap();
        if got >= n || Instant::now() > until {
            return got;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

const CASES: &str = "SELECT count(*) FROM agent_case WHERE conversation_id = $1";
const CUSTOMER_MSGS: &str =
    "SELECT count(*) FROM conversation_message WHERE conversation_id = $1 AND role = 'customer'";
const AGENT_MSGS: &str =
    "SELECT count(*) FROM conversation_message WHERE conversation_id = $1 AND role = 'agent'";

async fn draft_rev(fx: &Fx) -> i64 {
    sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
        .bind(fx.f.job.job_id)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap()
}

/// The test's root bus (same Redis and prefix as the state's).
async fn root(fx: &Fx) -> Bus {
    Bus::connect_with_response_timeout(&fx.redis.0, &fx.redis.1, Duration::from_secs(5))
        .await
        .unwrap()
}

fn test_scope(fx: &Fx) -> Scope {
    Scope::new(fx.f.tenant, fx.f.job.job_key.clone(), Env::Test).unwrap()
}

/// A conversation nobody identified: the staff chat's row with its customer and its identity state removed (what a
/// web chat visitor with no assertion has).
async fn start_anonymous(fx: &Fx) -> String {
    let conv = start(fx, "test", "C-1").await;
    sqlx::query("UPDATE conversation SET customer_id = NULL WHERE conversation_id = $1")
        .bind(&conv)
        .execute(&fx.e.owner)
        .await
        .unwrap();
    sqlx::query("DELETE FROM conversation_state WHERE conversation_id = $1")
        .bind(&conv)
        .execute(&fx.e.owner)
        .await
        .unwrap();
    conv
}

/// The agent lines of `conv`, oldest first.
async fn agent_lines(fx: &Fx, conv: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT text FROM conversation_message WHERE conversation_id = $1 AND role = 'agent' ORDER BY at_ms, message_id",
    )
    .bind(conv)
    .fetch_all(&fx.e.owner)
    .await
    .unwrap()
}

/// Words that would ask the customer for a lookup key (owner ruling 2026-10-26).
fn assert_no_id_words(text: &str) {
    let t = text.to_lowercase();
    for w in [
        "order number",
        "order id",
        "order no",
        "postcode",
        "postal code",
        "zip",
        "customer id",
        "tracking number",
        "your email",
        "email address",
        "ret-",
        "ord-",
    ] {
        assert!(!t.contains(w), "an id word `{w}` in: {text}");
    }
}

/// No visitor gate: a conversation nobody identified is served like any other (the tenant's systems authorize, the
/// runner holds a step for state it requires). A FAQ question and a parcel question each start a case, and no reply
/// asks for an id or sends the old sign-in text. Mutation: a gate that answers a sign-in prompt instead of a case.
#[tokio::test]
async fn an_unidentified_conversation_is_served_never_gated_nor_asked_for_an_id() {
    for (name, answer, text) in [
        (
            "t7-anon-faq",
            r#"{"context_space": "retail:faq:shipping", "slots": {}}"#,
            "What are your delivery times?",
        ),
        (
            "t7-anon-parcel",
            r#"{"context_space": "retail:order-support:order_status", "slots": {}}"#,
            "Where is my parcel?",
        ),
    ] {
        let fx = setup_answering(name, Duration::ZERO, &[], answer).await;
        let conv = start_anonymous(&fx).await;
        let (s, b) = say(&fx, &conv, text).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{b}");
        assert_eq!(
            wait_count(&fx, CASES, &conv, 1).await,
            1,
            "{name}: the request starts a case"
        );
        for line in agent_lines(&fx, &conv).await {
            assert!(!line.contains("signed in"), "{name}: {line}");
            assert_no_id_words(&line);
        }
    }
}

/// Park `choice` in `conv` the way the orchestrator does (`chat:{conv}` field `choice`).
async fn park_choice(fx: &Fx, conv: &str, choice: &agent_api::runtime::resolve::Choice) {
    let bus = root(fx).await.scoped(&test_scope(fx));
    let _: () = redis::cmd("HSET")
        .arg(agent_api::turn::chat_key(&bus, conv, ""))
        .arg("choice")
        .arg(serde_json::to_string(choice).unwrap())
        .query_async(&mut bus.conn())
        .await
        .unwrap();
}

/// The request.qualified payloads on the test scope's `order-support` stream.
async fn qualified_payloads(fx: &Fx) -> Vec<Value> {
    let bus = root(fx).await.scoped(&test_scope(fx));
    r::entries(&bus, &bus.streams.customer("order-support"))
        .await
        .into_iter()
        .map(|e| e.payload)
        .collect()
}

/// An identified customer who says "where is my parcel?" (or points at "the rug") gets a case at once: zero
/// questions, zero ids anywhere in what the conversation shows, and the qualified request carries NO order id (the
/// orchestrator resolves it from the customer's own records; `tests/b4/resolution.rs`), only the transient hint.
/// Mutations: the qualifier asking for the order; the model's `order_id` kept as the slot; the hint dropped.
#[tokio::test]
async fn an_identified_customer_pointing_at_an_order_is_never_asked_for_it() {
    for (name, answer, text, hint) in [
        (
            "t9-parcel",
            r#"{"context_space": "retail:order-support:order_status", "slots": {}}"#,
            "Where is my parcel?",
            None,
        ),
        (
            "t9-rug",
            r#"{"context_space": "retail:order-support:returns", "slots": {"order_id": "the rug", "reason": "too big"}}"#,
            "I want to return the rug, it is too big",
            Some("the rug"),
        ),
    ] {
        let fx = setup_answering(name, Duration::ZERO, &[], answer).await;
        let conv = start(&fx, "test", "C-1").await;
        let (s, b) = say(&fx, &conv, text).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{b}");
        assert_eq!(wait_count(&fx, CASES, &conv, 1).await, 1, "{name}");
        for line in agent_lines(&fx, &conv).await {
            assert_no_id_words(&line);
        }
        assert_eq!(
            agent_lines(&fx, &conv).await.len(),
            0,
            "{name}: no question, no menu"
        );
        let got = qualified_payloads(&fx).await;
        assert_eq!(got.len(), 1, "{name}");
        let slots = &got[0]["slots"];
        assert!(slots.get("order_id").is_none(), "{name}: {slots}");
        assert_eq!(
            slots.get("item_hint").and_then(Value::as_str),
            hint,
            "{name}: {slots}"
        );
    }
}

/// The orchestrator parked a question (which order?) in the conversation; the customer's next message answers it in
/// words ("the lamp please"): a case is qualified at once with the picked order's id (server-side), NO model call,
/// and the question is gone. An answer that points at nothing is an ordinary turn (the model is asked once).
/// Mutations: the answer going to the model; the candidates' ids lost; the choice kept after the answer.
#[tokio::test]
async fn the_answer_to_a_which_order_question_resolves_without_a_model_call() {
    use agent_api::runtime::resolve::{Choice, Pick};
    let fx = setup("t9-choice", Duration::ZERO).await;
    let pick = |id: &str, name: &str, day: u32| Pick {
        order_id: id.into(),
        payment_id: None,
        delivery_id: None,
        items: vec![name.into()],
        placed_on: chrono::NaiveDate::from_ymd_opt(2026, 10, day),
    };
    let choice = Choice {
        case_id: "CS-0".into(),
        context_space: "retail:order-support:returns".into(),
        slots: [("reason".to_owned(), "too big".to_owned())].into(),
        needs: vec!["order_id".into()],
        picks: vec![pick("A-1", "Jute rug", 12), pick("A-2", "Ceramic lamp", 20)],
        until_ms: chrono::Utc::now().timestamp_millis() + 60_000,
    };
    let conv = start(&fx, "test", "C-1").await;
    park_choice(&fx, &conv, &choice).await;
    let (s, b) = say(&fx, &conv, "the lamp please").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(wait_count(&fx, CASES, &conv, 1).await, 1);
    assert_eq!(fx.llm.calls(), 0, "the answer is mapped server-side");
    let got = qualified_payloads(&fx).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["slots"]["order_id"], "A-2");
    assert_eq!(got[0]["slots"]["reason"], "too big");
    assert_eq!(got[0]["context_space"], "retail:order-support:returns");
    let bus = root(&fx).await.scoped(&test_scope(&fx));
    let left: Option<String> = redis::cmd("HGET")
        .arg(agent_api::turn::chat_key(&bus, &conv, ""))
        .arg("choice")
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert!(left.is_none(), "the question is answered and gone");

    // an answer that points at none of them: the model is asked (an ordinary turn), the question is gone
    let conv2 = start(&fx, "test", "C-1").await;
    park_choice(&fx, &conv2, &choice).await;
    let (s, _) = say(&fx, &conv2, "what are your opening hours").await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let until = Instant::now() + Duration::from_secs(15);
    while fx.llm.calls() < 1 && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(fx.llm.calls(), 1);
}

/// A staff test chat over a customer job runs as the known customer the staff member typed: `identity_source =
/// 'staff_test'` (state), the email lower-cased as customer_id and state `email`.
#[tokio::test]
async fn staff_test_customer_sets_identity_source_and_email() {
    let fx = setup("t9-staff-test", Duration::ZERO).await;
    let conv = start(&fx, "test", " Anna@Shop.Example ").await;
    use agent_api::conversations::state as st;
    let row: (Option<String>, Option<String>, Option<String>) =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {source}, {email}, customer_id FROM conversation WHERE conversation_id = $1",
            source = st::sub("conversation", st::IDENTITY_SOURCE),
            email = st::sub("conversation", st::EMAIL),
        )))
        .bind(&conv)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap();
    assert_eq!(row.0.as_deref(), Some("staff_test"));
    assert_eq!(row.1.as_deref(), Some("anna@shop.example"));
    assert_eq!(row.2.as_deref(), Some("anna@shop.example"));
}

/// A customer job needs the test customer's email: missing or malformed is 422 and nothing is stored.
#[tokio::test]
async fn staff_test_without_a_valid_customer_email_is_422() {
    let fx = setup("t9-staff-test-422", Duration::ZERO).await;
    for (body, code) in [
        (json!({"env": "test"}), "test_customer_required"),
        (
            json!({"env": "test", "customer_email": "  "}),
            "test_customer_required",
        ),
        (
            json!({"env": "test", "customer_email": "not-an-email"}),
            "invalid_test_customer",
        ),
    ] {
        let (s, b) = call(&fx, "POST", "conversations", body).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert_eq!(b["error"]["code"], code, "{b}");
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM conversation WHERE tenant_id = $1")
        .bind(fx.f.tenant)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn post_is_202_before_the_turn_runs() {
    let fx = setup("t9-202", Duration::from_secs(3)).await;
    let conv = start(&fx, "test", "C-1").await;
    let t0 = Instant::now();
    let (s, b) = say(&fx, &conv, "I want to return the rug, it is too big").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "the 202 waited for the model"
    );
    assert_eq!(b["status"], 202);
    assert_eq!(b["conversation_id"], conv);
    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM conversation_message WHERE message_id = $1::uuid AND role = 'customer'",
    )
    .bind(b["message_id"].as_str().unwrap())
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(stored, 1, "the message is stored before the 202");
    let cases: i64 = sqlx::query_scalar(CASES)
        .bind(&conv)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap();
    assert_eq!(cases, 0, "the turn had not run at the 202");
    assert_eq!(
        wait_count(&fx, CASES, &conv, 1).await,
        1,
        "the turn ran after"
    );
}

#[tokio::test]
async fn busy_conversation_is_409_and_stores_nothing() {
    let fx = setup("t9-busy", Duration::from_secs(3)).await;
    let conv = start(&fx, "test", "C-1").await;
    let (s, b) = say(&fx, &conv, "I want to return the rug").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let (s, b) = say(&fx, &conv, "hello? anyone?").await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "conversation_busy");
    let n: i64 = sqlx::query_scalar(CUSTOMER_MSGS)
        .bind(&conv)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap();
    assert_eq!(n, 1, "the refused message is not stored");
    // once the turn is done the lock is free again
    wait_count(&fx, CASES, &conv, 1).await;
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let (s, _) = say(&fx, &conv, "thanks").await;
        if s == StatusCode::ACCEPTED {
            break;
        }
        assert!(Instant::now() < until, "the lock was never released");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn prod_chat_without_release_is_409() {
    let fx = setup("t9-prod", Duration::ZERO).await;
    let (s, b) = call(
        &fx,
        "POST",
        "conversations",
        json!({"env": "prod", "customer_email": "c-1@test.example"}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "no_release");
    let (s, b) = call(&fx, "GET", "chat/profile", Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["has_release"], false);
    assert_eq!(b["live_systems"], Value::Null);
    assert_eq!(b["audience"], "customers");
    // no content is compiled in: no quick prompts, and no connector instance of the tenant's own yet
    assert_eq!(b["prompts"], json!([]), "{b}");
    assert_eq!(b["test_systems"], "", "{b}");
    let (s, b) = call(
        &fx,
        "POST",
        "conversations",
        json!({"env": "staging", "customer_email": "c-1@test.example"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
}

#[tokio::test]
async fn test_turn_qualifies_against_the_draft() {
    let fx = setup("t9-draft", Duration::ZERO).await;
    let conv = start(&fx, "test", "C-1").await;
    let rev = draft_rev(&fx).await;
    let (s, b) = say(&fx, &conv, "I want to return the rug, it is too big").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(wait_count(&fx, CASES, &conv, 1).await, 1);
    let row: (
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT env, release_id, draft_rev, qualified_draft_rev, context_space, initiator_sub \
               FROM agent_case WHERE conversation_id = $1",
    )
    .bind(&conv)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "test".to_string(),
            None,
            Some(rev),
            Some(rev),
            Some("retail:order-support:returns".to_string()),
            Some("u-super".to_string())
        )
    );
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT kind FROM case_event e JOIN agent_case c USING (tenant_id, job_id, case_id) \
          WHERE c.conversation_id = $1 ORDER BY seq",
    )
    .bind(&conv)
    .fetch_all(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(kinds, ["message", "qualified"]);
    // request.qualified is on the test scope's department stream, after the row
    let bus = root(&fx).await.scoped(&test_scope(&fx));
    let stream = bus.streams.customer("order-support");
    let entries = r::entries(&bus, &stream).await;
    assert_eq!(entries.len(), 1, "one request.qualified");
    let env = &entries[0];
    assert_eq!(env.topic, "order-support.returns.request.qualified");
    assert_eq!(env.payload["conversation_id"], conv);
    assert_eq!(env.payload["qualified_draft_rev"], rev);
    // the order is a system slot: whatever the model extracted for it is not kept (resolved from the customer's own
    // records later); `reason` is
    assert!(
        env.payload["slots"].get("order_id").is_none(),
        "{}",
        env.payload["slots"]
    );
    assert_eq!(env.payload["slots"]["reason"], "too big");
    assert!(env.payload["message_id"].is_string());
    // a draft edit (no publish) moves the next case to the new draft revision
    let (s, b) = call(
        &fx,
        "PUT",
        "languages",
        json!({"languages": ["nl", "fr", "de"]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let rev2 = draft_rev(&fx).await;
    assert!(rev2 > rev, "the draft edit bumps draft_rev");
    let conv2 = start(&fx, "test", "C-1").await;
    let (s, _) = say(&fx, &conv2, "I want to return the rug, it is too big").await;
    assert_eq!(s, StatusCode::ACCEPTED);
    assert_eq!(wait_count(&fx, CASES, &conv2, 1).await, 1);
    let got: Option<i64> =
        sqlx::query_scalar("SELECT draft_rev FROM agent_case WHERE conversation_id = $1")
            .bind(&conv2)
            .fetch_one(&fx.e.owner)
            .await
            .unwrap();
    assert_eq!(got, Some(rev2));
}

#[tokio::test]
async fn suspended_tenant_gets_neutral_text_and_no_qualifier_call() {
    let fx = setup("t9-suspended", Duration::ZERO).await;
    let conv = start(&fx, "test", "C-1").await;
    sqlx::query("UPDATE tenants SET status = 'suspended' WHERE id = $1")
        .bind(fx.f.tenant)
        .execute(&fx.e.owner)
        .await
        .unwrap();
    let (s, b) = say(&fx, &conv, "I want to return the rug").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(wait_count(&fx, AGENT_MSGS, &conv, 1).await, 1);
    let (text, refused): (String, bool) = sqlx::query_as(
        "SELECT text, refused FROM conversation_message WHERE conversation_id = $1 AND role = 'agent'",
    )
    .bind(&conv)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(text, agent_api::turn::REFUSAL_TEXT);
    assert!(refused);
    assert_eq!(fx.llm.calls(), 0, "no qualifier call for a refused turn");
    let cases: i64 = sqlx::query_scalar(CASES)
        .bind(&conv)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap();
    assert_eq!(cases, 0);
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE tenant_id = $1 AND action = 'channel_turn_refused'",
    )
    .bind(fx.f.tenant)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(audited, 1);
    // the transcript marks the refused reply
    let (s, t) = call(&fx, "GET", &format!("conversations/{conv}"), Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{t}");
    let refused_lines = t["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["refused"] == true)
        .count();
    assert_eq!(refused_lines, 1, "{t}");
}

#[tokio::test]
async fn revoked_licence_verdict_gets_neutral_text_and_no_qualifier_call() {
    let fx = setup("b7-gate-revoked", Duration::ZERO).await;
    let conv = start(&fx, "test", "C-1").await;
    // not nonagentic's own (D70 is always admitted), and licences said the licence is revoked
    sqlx::query("UPDATE tenants SET platform_owned = false WHERE id = $1")
        .bind(fx.f.tenant)
        .execute(&fx.e.owner)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO tenant_licence_verdict (tenant_id, status, reason, licence_id) \
         VALUES ($1, 'revoked', 'token_invalid', 'lic-revoked') \
         ON CONFLICT (tenant_id) DO UPDATE \
         SET status = 'revoked', reason = 'token_invalid', licence_id = 'lic-revoked', checked_at = now()",
    )
    .bind(fx.f.tenant)
    .execute(&fx.e.owner)
    .await
    .unwrap();
    let (s, b) = say(&fx, &conv, "I want to return the rug").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(wait_count(&fx, AGENT_MSGS, &conv, 1).await, 1);
    let (text, refused): (String, bool) = sqlx::query_as(
        "SELECT text, refused FROM conversation_message WHERE conversation_id = $1 AND role = 'agent'",
    )
    .bind(&conv)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(text, agent_api::turn::REFUSAL_TEXT);
    assert!(refused);
    assert_eq!(fx.llm.calls(), 0, "no qualifier call for a refused turn");
    let cases: i64 = sqlx::query_scalar(CASES)
        .bind(&conv)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap();
    assert_eq!(cases, 0);
}

/// B7 gate 2 fails closed (owner, 2026-10-22): a tenant that is not platform-owned and has NO verdict row gets the
/// neutral refusal and no qualifier call; once it holds a fresh `active` verdict its next turn runs.
#[tokio::test]
async fn missing_licence_verdict_fails_closed_and_a_fresh_active_one_admits() {
    let fx = setup_tenant("b7-gate-missing", true).await;
    let owned: bool = sqlx::query_scalar("SELECT platform_owned FROM tenants WHERE id = $1")
        .bind(fx.f.tenant)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap();
    assert!(!owned, "the fixture's tenant is not platform-owned");
    sqlx::query("DELETE FROM tenant_licence_verdict WHERE tenant_id = $1")
        .bind(fx.f.tenant)
        .execute(&fx.e.owner)
        .await
        .unwrap();
    let refused_conv = start(&fx, "test", "C-1").await;
    let (s, b) = say(
        &fx,
        &refused_conv,
        "I want to return the rug, it is too big",
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(wait_count(&fx, AGENT_MSGS, &refused_conv, 1).await, 1);
    let (text, refused): (String, bool) = sqlx::query_as(
        "SELECT text, refused FROM conversation_message WHERE conversation_id = $1 AND role = 'agent'",
    )
    .bind(&refused_conv)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(
        text,
        agent_api::turn::REFUSAL_TEXT,
        "no verdict row: fail closed"
    );
    assert!(refused);
    assert_eq!(fx.llm.calls(), 0, "no qualifier call for a refused turn");
    let cases: i64 = sqlx::query_scalar(CASES)
        .bind(&refused_conv)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap();
    assert_eq!(cases, 0);

    // a fresh active verdict: the same tenant's next turn qualifies
    common::licence_active(&fx.e.owner, fx.f.tenant).await;
    let conv = start(&fx, "test", "C-1").await;
    let (s, b) = say(&fx, &conv, "I want to return the rug, it is too big").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(
        wait_count(&fx, CASES, &conv, 1).await,
        1,
        "a fresh active verdict admits the turn"
    );
    assert_eq!(fx.llm.calls(), 1);
    let refusals: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM conversation_message WHERE conversation_id = $1 AND role = 'agent' AND refused",
    )
    .bind(&conv)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(refusals, 0);
}

#[tokio::test]
async fn two_test_customers_record_two_customer_ids() {
    let fx = setup("t9-customers", Duration::ZERO).await;
    let (ann, bo) = ("ann@shop.example", "bo@shop.example");
    let a = start(&fx, "test", ann).await;
    let c = start(&fx, "test", bo).await;
    for conv in [&a, &c] {
        let (s, b) = say(&fx, conv, "I want to return the rug, it is too big").await;
        assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    }
    for (conv, want) in [(&a, ann), (&c, bo)] {
        assert_eq!(wait_count(&fx, CASES, conv, 1).await, 1);
        let got: Option<String> =
            sqlx::query_scalar("SELECT customer_id FROM agent_case WHERE conversation_id = $1")
                .bind(conv)
                .fetch_one(&fx.e.owner)
                .await
                .unwrap();
        assert_eq!(got.as_deref(), Some(want));
        // the typed test customer is an identified conversation: persisted source, case at level 2 bound to them
        let src: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT {} FROM conversation WHERE conversation_id = $1",
            agent_api::conversations::state::sub(
                "conversation",
                agent_api::conversations::state::IDENTITY_SOURCE
            ),
        )))
        .bind(conv)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap();
        assert_eq!(src.as_deref(), Some("staff_test"));
        let row: (i32, Option<String>) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {level}, CASE WHEN {known} THEN {subject} END FROM agent_case WHERE conversation_id = $1",
            level = agent_api::conversations::state::level_sql("agent_case"),
            known = agent_api::conversations::state::known_sql("agent_case"),
            subject = agent_api::conversations::state::sub(
                "agent_case",
                agent_api::conversations::state::SUBJECT
            ),
        )))
        .bind(conv)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap();
        assert_eq!(row, (2, Some(want.to_owned())));
    }
    // the transcript carries the web `Conversation` fields
    let (s, t) = call(&fx, "GET", &format!("conversations/{a}"), Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{t}");
    assert_eq!(t["customer_id"], ann);
    assert_eq!(t["env"], "test");
    assert_eq!(t["initiator_sub"], "u-super");
    assert_eq!(t["case_ids"].as_array().map(Vec::len), Some(1), "{t}");
}

#[tokio::test]
async fn usage_event_per_qualifier_call() {
    let fx = setup("t9-usage", Duration::ZERO).await;
    let conv = start(&fx, "test", "C-1").await;
    let (s, _) = say(&fx, &conv, "I want to return the rug, it is too big").await;
    assert_eq!(s, StatusCode::ACCEPTED);
    wait_count(&fx, CASES, &conv, 1).await;
    let rows: Vec<(String, String, String, String, String, i64, i64, i32)> = sqlx::query_as(
        "SELECT source, use, actor, provider_id, funding, tokens_in, tokens_out, calls FROM usage_event \
          WHERE conversation_id = $1",
    )
    .bind(&conv)
    .fetch_all(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(fx.llm.calls(), 1);
    assert_eq!(
        rows,
        [(
            "staff_test".to_string(),
            "qualify".to_string(),
            "u-super".to_string(),
            "internal".to_string(),
            "local_gpu".to_string(),
            11,
            7,
            1
        )]
    );
}

// Guards turn/drain.rs (`r.acting_for(conv.initiator_sub)`): the staff member who started a test chat is the
// router's X-Actor-Id; a starter whose sub is no uuid (`u-super`) falls back to `SERVICE_ACTOR_ID`.
#[tokio::test]
async fn the_qualifier_call_names_the_chat_starter_to_the_router() {
    let service = Uuid::new_v4();
    let fx = setup_with(
        "t9-actor",
        Duration::ZERO,
        &[("SERVICE_ACTOR_ID", service.to_string())],
    )
    .await;
    let person = Uuid::new_v4();
    let staff = start(&fx, "test", "C-1").await;
    sqlx::query("UPDATE conversation SET initiator_sub = $2 WHERE conversation_id = $1")
        .bind(&staff)
        .bind(person.to_string())
        .execute(&fx.e.owner)
        .await
        .unwrap();
    let (s, _) = say(&fx, &staff, "I want to return the rug, it is too big").await;
    assert_eq!(s, StatusCode::ACCEPTED);
    wait_count(&fx, CASES, &staff, 1).await;
    let other = start(&fx, "test", "C-2").await;
    let (s, _) = say(&fx, &other, "I want to return the rug, it is too big").await;
    assert_eq!(s, StatusCode::ACCEPTED);
    wait_count(&fx, CASES, &other, 1).await;
    let project = agent_api::providers::llm::tenant_level_project_id(fx.f.tenant).to_string();
    let id = |actor: Uuid| {
        [
            Some("agent".to_string()),
            Some(project.clone()),
            Some(actor.to_string()),
        ]
    };
    assert_eq!(*fx.llm.ids.lock().unwrap(), [id(person), id(service)]);
}

#[tokio::test]
async fn accept_customer_message_is_the_send_path() {
    // the route stores nothing itself: its only write is turn::accept_message
    let src = include_str!("../../src/routes/chat.rs");
    let send = &src[src.find("async fn send_chat_message").expect("the handler")..];
    assert!(send.contains("turn::accept_message("), "the send path");
    assert!(
        !send.contains("store::append") && !send.contains("INSERT"),
        "sendChatMessage writes only through turn::accept_message"
    );
    // and the library door does exactly what the route does: store, queue, drain
    let fx = setup("t9-door", Duration::ZERO).await;
    let conv_id = start(&fx, "test", "C-2").await;
    let st = &fx.e.state;
    let mut tx = fx.f.job.begin(&st.db).await.unwrap();
    let conv = agent_api::conversations::store::conversation_or_404(&mut tx, &conv_id)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let scope = agent_api::interaction::Scope::from(&fx.f.job);
    let a = agent_api::turn::accept_message(
        st,
        agent_api::turn::Inbound {
            scope: &scope,
            conv: &conv,
            author: "Bo Jansen",
            actor: agent_api::audit::Actor::system("service:t9-test"),
            text: "I want to return the rug, it is too big",
            external_id: Some("ext-1"),
        },
        agent_api::turn::AuthorRole::User,
    )
    .await
    .unwrap();
    assert_eq!(wait_count(&fx, CASES, &conv_id, 1).await, 1);
    let linked: Option<String> =
        sqlx::query_scalar("SELECT case_id FROM conversation_message WHERE message_id = $1")
            .bind(a.message_id)
            .fetch_one(&fx.e.owner)
            .await
            .unwrap();
    assert!(linked.is_some(), "the drained message joins its case");
}

#[tokio::test]
async fn card_number_is_redacted_before_persist_and_publish() {
    let fx = setup("t9-card", Duration::ZERO).await;
    let conv = start(&fx, "test", "C-1").await;
    let bus = root(&fx).await.scoped(&test_scope(&fx));
    let mut ps = bus.client().get_async_pubsub().await.unwrap();
    ps.subscribe(bus.streams.chat()).await.unwrap();
    let mut frames = ps.into_on_message();
    let pan = "4111 1111 1111 1111";
    let order = "1234 5678 9012 3456";
    let text = format!("return ORD-10001 too big, refund to {pan} exp 12/27 cvv 123, ref {order}");
    let (s, b) = say(&fx, &conv, &text).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    // the stored row
    let stored: String = sqlx::query_scalar(
        "SELECT text FROM conversation_message WHERE conversation_id = $1 AND role = 'customer'",
    )
    .bind(&conv)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert!(
        !stored.contains(pan) && !stored.contains("4111"),
        "{stored}"
    );
    assert!(
        !stored.contains("12/27") && !stored.contains("cvv 123"),
        "{stored}"
    );
    assert!(
        stored.contains("[card removed]") && stored.contains(order),
        "{stored}"
    );
    // the published echo
    let frame = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(m) = frames.next().await {
            let v: Value = serde_json::from_str(&m.get_payload::<String>().unwrap()).unwrap();
            if v["data"]["role"] == "customer" {
                return v;
            }
        }
        Value::Null
    })
    .await
    .expect("the customer echo is published");
    assert_eq!(frame["event"], "message");
    assert_eq!(frame["data"]["text"], stored.as_str());
    // the qualifier request
    wait_count(&fx, CASES, &conv, 1).await;
    let sent = fx.llm.bodies.lock().unwrap()[0].to_string();
    assert!(!sent.contains("4111"), "the qualifier saw the card");
    assert!(sent.contains(order), "a Luhn-invalid number is kept");
    // the bus entry and the case events carry no card either
    let events: Vec<Value> = sqlx::query_scalar(
        "SELECT e.body FROM case_event e JOIN agent_case c USING (tenant_id, job_id, case_id) \
          WHERE c.conversation_id = $1",
    )
    .bind(&conv)
    .fetch_all(&fx.e.owner)
    .await
    .unwrap();
    assert!(events.iter().all(|e| !e.to_string().contains("4111")));
    let entries = r::entries(&bus, &bus.streams.customer("order-support")).await;
    assert_eq!(entries.len(), 1);
    assert!(
        entries
            .iter()
            .all(|e| !e.payload.to_string().contains("4111"))
    );
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE tenant_id = $1 AND action = 'chat.card_redacted'",
    )
    .bind(fx.f.tenant)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(audited, 1);
}

const USAGE: &str = "SELECT provider_id, funding FROM usage_event WHERE conversation_id = $1";

/// A provisioned tenant with no BYO provider runs on the router under ITS OWN licence, never the platform key.
#[tokio::test]
async fn unbound_qualify_of_a_provisioned_tenant_calls_the_router_with_its_own_token() {
    let fx = setup_tenant("b4live-provisioned", true).await;
    let conv = start(&fx, "test", "C-1").await;
    let (s, b) = say(&fx, &conv, "I want to return the rug, it is too big").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(
        wait_count(&fx, CASES, &conv, 1).await,
        1,
        "the provisioned tenant's turn qualified on the router"
    );
    assert_eq!(fx.llm.calls(), 1);
    assert_eq!(
        fx.llm.auths(),
        [Some(format!("Bearer {TENANT_ROUTER_TOKEN}"))],
        "the tenant's own token, not INTERNAL_LLM_KEY"
    );
    let usage: Vec<(String, String)> = sqlx::query_as(USAGE)
        .bind(&conv)
        .fetch_all(&fx.e.owner)
        .await
        .unwrap();
    assert_eq!(
        usage,
        [("internal".to_string(), "local_gpu".to_string())],
        "metered as the platform's own model, platform-funded"
    );
}

/// The platform-owned tenant keeps calling the router with `INTERNAL_LLM_KEY`.
#[tokio::test]
async fn platform_owned_tenant_still_uses_the_platform_key() {
    let fx = setup("b4live-platform-key", Duration::ZERO).await;
    let conv = start(&fx, "test", "C-1").await;
    let (s, b) = say(&fx, &conv, "I want to return the rug, it is too big").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(wait_count(&fx, CASES, &conv, 1).await, 1);
    assert_eq!(fx.llm.auths(), [Some(format!("Bearer {PLATFORM_KEY}"))]);
}

/// Fail closed (owner): a tenant with no inference-router licence is not answered and the platform key is NEVER
/// sent in its place; a test chat says what is happening.
#[tokio::test]
async fn unbound_qualify_of_an_unprovisioned_tenant_fails_closed_without_the_platform_key() {
    let fx = setup_tenant("b4live-unprovisioned", false).await;
    let conv = start(&fx, "test", "C-1").await;
    let (s, b) = say(&fx, &conv, "I want to return the rug, it is too big").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(wait_count(&fx, AGENT_MSGS, &conv, 1).await, 1);
    let text: String = sqlx::query_scalar(
        "SELECT text FROM conversation_message WHERE conversation_id = $1 AND role = 'agent'",
    )
    .bind(&conv)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    assert_eq!(
        text,
        agent_api::turn::INFERENCE_NOT_PROVISIONED_TEXT,
        "test env: the specific text, not TURN_FAILED"
    );
    assert_eq!(fx.llm.calls(), 0, "the router saw no request at all");
    assert!(
        !fx.llm
            .auths()
            .contains(&Some(format!("Bearer {PLATFORM_KEY}"))),
        "the platform key never stands in for a tenant"
    );
    let cases: i64 = sqlx::query_scalar(CASES)
        .bind(&conv)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap();
    assert_eq!(cases, 0);
    let usage: Vec<(String, String)> = sqlx::query_as(USAGE)
        .bind(&conv)
        .fetch_all(&fx.e.owner)
        .await
        .unwrap();
    assert!(usage.is_empty(), "{usage:?}");
}

#[tokio::test]
async fn start_without_the_runtime_is_503_and_stores_nothing() {
    // no REDIS_URL: the runtime is dark, so a conversation could never be sent to
    let llm = fake_llm(RETURN_ANSWER, Duration::ZERO).await;
    let e = common::env().await;
    assert!(e.state.redis.is_none());
    let f = crate::b4::it_desk_identity::own_job(&e, "returns_desk", &RETURNS, false).await;
    let fx = Fx {
        e,
        f,
        llm,
        redis: (String::new(), String::new()),
    };
    let (s, b) = call(
        &fx,
        "POST",
        "conversations",
        json!({"env": "test", "customer_email": "c-1@test.example"}),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert_eq!(b["error"]["code"], "unavailable");
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM conversation WHERE job_id = $1")
        .bind(fx.f.job.job_id)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap();
    assert_eq!(stored, 0, "no conversation that can never be sent to");
    assert_eq!(fx.llm.calls(), 0);
}

// ---- test before publish: test_readiness, qualify_unbound ----------------------------------------

#[tokio::test]
async fn chat_profile_tells_the_tester_what_a_test_needs() {
    let fx = setup_tenant("tbp-profile", false).await;
    let (s, b) = call(&fx, "GET", "chat/profile", Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["test_readiness"]["qualify_bound"], false, "{b}");
    let items = b["test_readiness"]["items"].as_array().unwrap();
    let item = items
        .iter()
        .find(|i| i["code"] == "llm_inference_not_provisioned")
        .unwrap_or_else(|| panic!("llm_inference_not_provisioned item: {b}"));
    assert_eq!(item["title"], "Language model access is still being set up");
    assert_eq!(item["fix_label"], "View setup status");
    assert_eq!(item["href"], "/admin");
    assert!(b["test_readiness"]["template_errors"].is_array(), "{b}");
    // a provisioned tenant is served by the router under its own licence: bound, no item
    let ok = setup_tenant("tbp-profile-provisioned", true).await;
    let (_, b) = call(&ok, "GET", "chat/profile", Value::Null).await;
    assert_eq!(b["test_readiness"]["qualify_bound"], true, "{b}");
    assert!(
        b["test_readiness"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["code"] != "llm_inference_not_provisioned"),
        "{b}"
    );
}

#[tokio::test]
async fn a_test_turn_of_an_unprovisioned_tenant_replies_the_setup_text() {
    let fx = setup_tenant("tbp-unbound-turn", false).await;
    let conv = start(&fx, "test", "C-1").await;
    let (s, b) = say(&fx, &conv, "Where is my order?").await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(wait_count(&fx, AGENT_MSGS, &conv, 1).await, 1);
    let lines = agent_lines(&fx, &conv).await;
    assert_eq!(
        lines,
        [agent_api::turn::INFERENCE_NOT_PROVISIONED_TEXT.to_string()],
        "never the generic TURN_FAILED, never a question for an id"
    );
    assert_eq!(fx.llm.calls(), 0);
}
