//! B4 T10a — milestone M1 (§0 R9), the part that needs no system of record: the staff test chat against the real
//! router, drain and qualifier. A super_admin acting in the tenant starts a test-env chat as a test customer (an email
//! the staff member types, `identity_source` `staff_test`) on a job layer the test authors itself, sends a message
//! (202), and the drain qualifies it (the job's `qualify` binding: the tenant is platform-owned, so that is the
//! internal model, an OpenAI-compatible fake on 127.0.0.1 here) into a case stamped with the job's `draft_rev` and no
//! release. A draft edit (`design::track`, no publish) reaches the next turn. The orchestrator / runner /
//! interaction chain (an `order.read` against a connector, the reply on SSE) needs a system of record and an
//! operation catalogue the platform no longer ships: those tests went with the demo dataset.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::b4::it_desk_identity::{OwnSpace, own_job};
use crate::common::design_contract::{self, ContractJob};
use crate::common::{self, TestEnv, json as req, send, super_admin};
use crate::redis_env as r;

/// How long one turn may take.
const WAIT: Duration = Duration::from_secs(30);

/// The fake model's answer: an order-status request (one message completes it).
const ORDER_STATUS: &str =
    r#"{"context_space": "retail:order-support:order_status", "slots": {"order_id": "RET-01"}}"#;
const CUSTOMER: &str = "ann@shop.example";
const SPACES: [OwnSpace; 1] = [OwnSpace {
    key: "retail:order-support:order_status",
    slots: &["order_id"],
    kinds: &[],
}];

/// An OpenAI-compatible `POST /v1/chat/completions` that records every request body.
struct Llm {
    url: String,
    bodies: Arc<Mutex<Vec<Value>>>,
}

impl Llm {
    /// The job context (the part of the one system message from "About this job") of qualifier call `i`; `None`
    /// when the call had none.
    fn job_context(&self, i: usize) -> Option<String> {
        let bodies = self.bodies.lock().unwrap();
        let system = bodies.get(i)?["messages"][0]["content"]
            .as_str()?
            .to_owned();
        system
            .find("About this job")
            .map(|at| system[at..].to_owned())
    }
}

async fn fake_llm() -> Llm {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = bodies.clone();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |axum::Json(v): axum::Json<Value>| {
            let seen = seen.clone();
            async move {
                // as the internal model's chat template (Qwen3 on the pgx vLLM): a system message after index 0
                // is a 400, the request never reaches the model
                let late_system = v["messages"]
                    .as_array()
                    .is_some_and(|ms| ms.iter().skip(1).any(|m| m["role"] == "system"));
                seen.lock().unwrap().push(v);
                if late_system {
                    return (
                        StatusCode::BAD_REQUEST,
                        axum::Json(
                            json!({"error": {"message": "System message must be at the beginning.",
                                                    "type": "BadRequestError", "code": 400}}),
                        ),
                    );
                }
                (
                    StatusCode::OK,
                    axum::Json(json!({
                        "choices": [{"message": {"role": "assistant", "content": ORDER_STATUS}}],
                        "usage": {"prompt_tokens": 9, "completion_tokens": 5},
                    })),
                )
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
        bodies,
    }
}

struct M1 {
    e: TestEnv,
    f: ContractJob,
    llm: Llm,
}

/// A job of its own platform-owned tenant (`qualify` = the fake model) on a job layer of the test's own, and the
/// job's target selection saved.
async fn setup(name: &str) -> M1 {
    let llm = fake_llm().await;
    let vars = r::api_vars(name).await;
    let url = llm.url.clone();
    let e = common::env_with(|v| {
        v.extend(vars.clone());
        v.insert("INTERNAL_LLM_URL".into(), url);
    })
    .await;
    let f = own_job(&e, "order_desk", &SPACES, false).await;
    sqlx::query("UPDATE tenants SET platform_owned = true WHERE id = $1")
        .bind(f.tenant)
        .execute(&e.owner)
        .await
        .unwrap();
    let m = M1 { e, f, llm };
    let body =
        json!({"selection": design_contract::cuttable_selection(), "expected_updated_at": null});
    let (s, b) = call(&m, "PUT", "target", body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    m
}

fn uri(m: &M1, tail: &str) -> String {
    format!("/api/jobs/{}/{tail}", m.f.job.job_key)
}

/// A super_admin acting in the fixture's tenant .
async fn call(m: &M1, method: &str, tail: &str, body: Value) -> (StatusCode, Value) {
    let mut q = if method == "GET" {
        common::get(&uri(m, tail), Some(&super_admin()))
    } else {
        req(method, &uri(m, tail), Some(&super_admin()), body)
    };
    q.headers_mut().insert(
        "x-active-tenant-id",
        m.f.tenant.to_string().parse().unwrap(),
    );
    send(&m.e.app, q).await
}

/// `startChat` as the test customer [`CUSTOMER`]: the conversation id.
async fn start(m: &M1, env: &str) -> String {
    let (s, b) = call(
        m,
        "POST",
        "conversations",
        json!({"env": env, "customer_email": CUSTOMER}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["env"], env);
    assert_eq!(b["customer_id"], CUSTOMER);
    b["conversation_id"].as_str().unwrap().to_string()
}

/// `sendChatMessage`: answered 202 once the message is stored and queued.
async fn say(m: &M1, conv: &str, text: &str) {
    let (s, b) = call(
        m,
        "POST",
        &format!("conversations/{conv}/messages"),
        json!({"text": text}),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["conversation_id"], conv);
}

async fn draft_rev(m: &M1) -> i64 {
    sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
        .bind(m.f.job.job_id)
        .fetch_one(&m.e.owner)
        .await
        .unwrap()
}

/// `(case_id, env, release_id, draft_rev, context_space, customer_id, identity_level)` of the conversation's case,
/// once the drain made it.
async fn case_of(
    m: &M1,
    conv: &str,
) -> (
    String,
    String,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<String>,
    i32,
) {
    let until = Instant::now() + WAIT;
    loop {
        let row = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT case_id, env, release_id, draft_rev, context_space, customer_id, {level} \
               FROM agent_case WHERE tenant_id = $1 AND job_id = $2 AND conversation_id = $3",
            level = agent_api::conversations::state::level_sql("agent_case"),
        )))
        .bind(m.f.tenant)
        .bind(m.f.job.job_id)
        .bind(conv)
        .fetch_optional(&m.e.owner)
        .await
        .unwrap();
        if let Some(row) = row {
            return row;
        }
        assert!(Instant::now() < until, "the turn never qualified");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The `qualified` event body of a case.
async fn qualified_of(m: &M1, case: &str) -> Value {
    sqlx::query_scalar(
        "SELECT body FROM case_event WHERE tenant_id = $1 AND case_id = $2 AND kind = 'qualified' \
          ORDER BY seq LIMIT 1",
    )
    .bind(m.f.tenant)
    .bind(case)
    .fetch_one(&m.e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn test_chat_qualifies_on_the_draft_revision() {
    let m = setup("m1-reply").await;
    let conv = start(&m, "test").await;
    let rev = draft_rev(&m).await;
    say(&m, &conv, "Hi, where is my order?").await;
    let (case, env, release, draft, space, customer, level) = case_of(&m, &conv).await;
    // the case: the test env on the draft at its revision, no release, as the known test customer
    assert_eq!(
        (
            env.as_str(),
            release,
            draft,
            space.as_deref(),
            customer.as_deref(),
            level
        ),
        (
            "test",
            None,
            Some(rev),
            Some("retail:order-support:order_status"),
            Some(CUSTOMER),
            2
        )
    );
    // the qualifier was told the job's context
    let job = m.llm.job_context(0).expect("the job context of the call");
    assert!(job.contains("About this job"), "{job}");
    // D108 (KH T5): the case's `qualified` event lists the layer markers of the turn's one system message (the
    // qualifier's, which starts with them); the job's identity is its own layer's section
    let layers: Vec<String> =
        serde_json::from_value(qualified_of(&m, &case).await["layers"].clone())
            .expect("qualified.layers");
    assert!(
        layers
            .iter()
            .any(|l| l.starts_with("[layer job/order_desk@v")),
        "{layers:?}"
    );
    let system = m.llm.bodies.lock().unwrap()[0]["messages"][0]["content"]
        .as_str()
        .expect("the qualifier's system message")
        .to_owned();
    for marker in &layers {
        assert!(
            system.contains(&format!("{marker}\n")),
            "{marker} in {system}"
        );
    }
    assert!(system.starts_with(&layers[0]), "{system}");
}

#[tokio::test]
async fn test_chat_follows_the_draft_without_publish() {
    let m = setup("m1-draft").await;
    let conv = start(&m, "test").await;
    let rev = draft_rev(&m).await;
    say(&m, &conv, "Where is my order?").await;
    let (_, _, _, draft, _, _, _) = case_of(&m, &conv).await;
    assert_eq!(draft, Some(rev));

    // a draft edit (B2 design::track): the stance. Nothing is published.
    const EDITED: &str = "warm and brief (M1 draft edit)";
    let (s, b) = call(&m, "PUT", "settings", json!({"stance": EDITED})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let rev2 = draft_rev(&m).await;
    assert!(rev2 > rev, "the edit bumps draft_rev");

    // the next turn runs on the new draft revision, and its qualifier call was told the edited stance
    let conv2 = start(&m, "test").await;
    say(&m, &conv2, "Where is my order?").await;
    let (_, env, release, draft, _, _, _) = case_of(&m, &conv2).await;
    assert_eq!((env.as_str(), release, draft), ("test", None, Some(rev2)));
    let before = m.llm.job_context(0).expect("call 1's job context");
    let after = m.llm.job_context(1).expect("call 2's job context");
    assert!(!before.contains(EDITED), "{before}");
    assert!(after.contains(&format!("stance: {EDITED}")), "{after}");
    // no publish: the job has no release
    let (current, releases): (Option<String>, i64) = sqlx::query_as(
        "SELECT current_release_id, (SELECT count(*) FROM job_release WHERE job_id = $1) FROM jobs WHERE id = $1",
    )
    .bind(m.f.job.job_id)
    .fetch_one(&m.e.owner)
    .await
    .unwrap();
    assert_eq!((current, releases), (None, 0));
}

#[tokio::test]
async fn test_chat_prod_without_release_is_409() {
    let m = setup("m1-prod").await;
    let (s, b) = call(
        &m,
        "POST",
        "conversations",
        json!({"env": "prod", "customer_email": CUSTOMER}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "no_release");
    let opened: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM conversation WHERE tenant_id = $1 AND env = 'prod'",
    )
    .bind(m.f.tenant)
    .fetch_one(&m.e.owner)
    .await
    .unwrap();
    assert_eq!(opened, 0, "a refused start opens nothing");
    let (s, b) = call(&m, "GET", "chat/profile", Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["has_release"], false);
    assert_eq!(b["live_systems"], Value::Null);
    // the draft is always there to talk to
    start(&m, "test").await;
}
