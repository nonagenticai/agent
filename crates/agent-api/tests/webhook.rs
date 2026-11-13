//! Async interactions with signed webhook callbacks (crate::interaction::webhook): 202 `queued`, then a Standard
//! Webhooks-signed POST to a local receiver; retries keep the webhook-id; a backend failure is a `failed` callback;
//! the refusals (stream + callback, unconfigured, private URL, observer reading the secret, the pending cap). Durable
//! (0114): a queued row survives the worker that claimed it, a stored answer is not recomputed, retries end `failed`
//! with `last_error`, the per-tenant pending cap, the delivery-state route. Real Postgres 18 (one container for this
//! binary).
use crate::common;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_api::interaction::webhook::{self, Webhooks};
use agent_api::interaction::{BackendError, BackendEvent, InteractionBackend, InteractionRequest};
use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use base64::Engine;
use common::{TestEnv, get, insert_tenant, insert_user, json, person, send};
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use uuid::Uuid;

/// "Hel", "lo", done.
struct Hello;

impl InteractionBackend for Hello {
    fn respond(
        &self,
        _req: InteractionRequest,
    ) -> BoxStream<'static, Result<BackendEvent, BackendError>> {
        stream::iter(vec![
            Ok(BackendEvent::Delta("Hel".into())),
            Ok(BackendEvent::Delta("lo".into())),
            Ok(BackendEvent::Done { usage: None }),
        ])
        .boxed()
    }
}

/// One delta, then the upstream fails.
struct Breaks;

impl InteractionBackend for Breaks {
    fn respond(
        &self,
        _req: InteractionRequest,
    ) -> BoxStream<'static, Result<BackendEvent, BackendError>> {
        stream::iter(vec![
            Ok(BackendEvent::Delta("Hel".into())),
            Err(BackendError::Upstream("status 500".into())),
        ])
        .boxed()
    }
}

const NATIVE: &str = "/api/jobs/faq_agent/interactions";
const SECRET: &str = "/api/jobs/faq_agent/interactions/webhook-secret";

/// Built at runtime, so no key literal is committed.
fn master_key() -> String {
    "k".repeat(48)
}

/// A tenant with an observer `u-member`, an integrations manager `u-integ` and a job `faq_agent`. With `key`, the
/// master key is configured, the sender is replaced by a test one (local http receivers allowed, short retries) and
/// the delivery worker runs; without, async is off.
async fn setup(backend: Arc<dyn InteractionBackend>, key: bool) -> TestEnv {
    let e = setup_idle(backend, key).await;
    if key {
        webhook::spawn_worker(e.state.clone()).expect("webhooks configured");
    }
    e
}

/// [`setup`] without the delivery worker: rows stay where the route (or the test) left them until a test starts one.
async fn setup_idle(backend: Arc<dyn InteractionBackend>, key: bool) -> TestEnv {
    let mut e = common::env_with(|v| {
        if key {
            v.insert("INTERACTION_WEBHOOK_KEY".into(), master_key());
        }
    })
    .await;
    assert_eq!(e.state.webhooks.is_some(), key, "configured from the env");
    e.state.interaction = backend;
    if key {
        let mut w = Webhooks::new(&master_key());
        w.allow_private = true;
        w.retry = vec![
            Duration::ZERO,
            Duration::from_millis(50),
            Duration::from_millis(50),
        ];
        e.state.webhooks = Some(Arc::new(w));
    }
    e.app = agent_api::router(e.state.clone());
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    insert_user(&e.owner, "u-member", t, "tenant_user").await;
    insert_user(&e.owner, "u-integ", t, "integrations_manager").await;
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'faq_agent', 'J', '[]', 'test')",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    e
}

fn member() -> String {
    person("u-member", &[])
}

fn integ() -> String {
    person("u-integ", &[])
}

/// A local receiver: records (headers, body) and answers the scripted statuses in order, then 200.
struct Receiver {
    url: String,
    rx: mpsc::UnboundedReceiver<(HeaderMap, String)>,
}

impl Receiver {
    async fn next(&mut self) -> (HeaderMap, String) {
        tokio::time::timeout(Duration::from_secs(10), self.rx.recv())
            .await
            .expect("a callback within 10 s")
            .expect("receiver open")
    }

    /// Nothing more arrives within `ms`.
    async fn quiet(&mut self, ms: u64) {
        let more = tokio::time::timeout(Duration::from_millis(ms), self.rx.recv()).await;
        assert!(more.is_err(), "no further callback");
    }
}

async fn receiver(script: Vec<u16>) -> Receiver {
    let (tx, rx) = mpsc::unbounded_channel();
    let script = Arc::new(Mutex::new(VecDeque::from(script)));
    let app = Router::new().route(
        "/hook",
        post(move |h: HeaderMap, body: String| {
            let tx = tx.clone();
            let script = script.clone();
            async move {
                let _ = tx.send((h, body));
                let code = script.lock().unwrap().pop_front().unwrap_or(200);
                StatusCode::from_u16(code).unwrap()
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Receiver {
        url: format!("http://{addr}/hook"),
        rx,
    }
}

fn header<'a>(h: &'a HeaderMap, k: &str) -> &'a str {
    h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("")
}

/// The job's secret from the route, as the raw key bytes.
async fn secret(e: &TestEnv) -> Vec<u8> {
    let (s, b) = send(&e.app, get(SECRET, Some(&integ()))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["scheme"], "standard-webhooks-v1");
    assert_eq!(b["signature_header"], "webhook-signature");
    let shown = b["secret"].as_str().unwrap();
    let raw = base64::engine::general_purpose::STANDARD
        .decode(shown.strip_prefix("whsec_").expect("whsec_ prefix"))
        .unwrap();
    assert_eq!(raw.len(), 32);
    raw
}

/// The Standard Webhooks check a receiver runs: `v1,` + base64(HMAC-SHA256(secret, "{id}.{ts}.{body}")), and a
/// fresh timestamp.
fn verify(secret: &[u8], h: &HeaderMap, body: &str) {
    let id = header(h, "webhook-id");
    let ts = header(h, "webhook-timestamp");
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    let tag = ring::hmac::sign(&key, format!("{id}.{ts}.{body}").as_bytes());
    let want = format!(
        "v1,{}",
        base64::engine::general_purpose::STANDARD.encode(tag)
    );
    assert_eq!(header(h, "webhook-signature"), want);
    let ts: i64 = ts.parse().unwrap();
    assert!((chrono::Utc::now().timestamp() - ts).abs() < 60, "fresh");
}

async fn submit(e: &TestEnv, body: Value) -> (StatusCode, Value) {
    send(&e.app, json("POST", NATIVE, Some(&member()), body)).await
}

#[tokio::test]
async fn async_interaction_delivers_a_signed_callback() {
    let e = setup(Arc::new(Hello), true).await;
    let mut r = receiver(vec![]).await;
    let (s, b) = submit(&e, json!({"input": "hi", "callback_url": r.url})).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["status"], "queued");
    assert_eq!(b["object"], "interaction");
    assert_eq!(b["callback_url"], r.url.as_str());
    assert_eq!(b["output"], json!([]));
    let id = b["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("int_"));
    let (h, body) = r.next().await;
    assert_eq!(header(&h, "webhook-id"), id);
    assert_eq!(header(&h, "content-type"), "application/json");
    assert_eq!(header(&h, "user-agent"), "agent-interaction-webhooks/1");
    verify(&secret(&e).await, &h, &body);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["id"], id.as_str());
    assert_eq!(v["status"], "completed");
    assert_eq!(v["output_text"], "Hello");
    assert_eq!(v["job"], "faq_agent");
    r.quiet(200).await;
}

#[tokio::test]
async fn a_5xx_is_retried_with_the_same_webhook_id() {
    let e = setup(Arc::new(Hello), true).await;
    let mut r = receiver(vec![500]).await;
    let (s, b) = submit(&e, json!({"input": "hi", "callback_url": r.url})).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let key = secret(&e).await;
    let (h1, b1) = r.next().await;
    let (h2, b2) = r.next().await;
    assert_eq!(header(&h1, "webhook-id"), b["id"].as_str().unwrap());
    assert_eq!(header(&h2, "webhook-id"), header(&h1, "webhook-id"));
    assert_eq!(b1, b2, "the same body on attempt 2");
    verify(&key, &h1, &b1);
    verify(&key, &h2, &b2);
    r.quiet(300).await;
}

#[tokio::test]
async fn a_4xx_is_not_retried() {
    let e = setup(Arc::new(Hello), true).await;
    let mut r = receiver(vec![410]).await;
    let (s, _) = submit(&e, json!({"input": "hi", "callback_url": r.url})).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    r.next().await;
    r.quiet(300).await;
}

#[tokio::test]
async fn a_backend_failure_is_a_failed_callback() {
    let e = setup(Arc::new(Breaks), true).await;
    let mut r = receiver(vec![]).await;
    let (s, b) = submit(&e, json!({"input": "hi", "callback_url": r.url})).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let (h, body) = r.next().await;
    verify(&secret(&e).await, &h, &body);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["id"], b["id"]);
    assert_eq!(v["status"], "failed");
    assert_eq!(v["output"], json!([]));
    assert_eq!(v["error"]["code"], "interaction_upstream_error");
}

#[tokio::test]
async fn refusals() {
    let e = setup(Arc::new(Hello), true).await;
    let r = receiver(vec![]).await;
    let code = |b: &Value| b["error"]["code"].as_str().map(String::from);
    let (s, b) = submit(
        &e,
        json!({"input": "hi", "stream": true, "callback_url": r.url}),
    )
    .await;
    assert_eq!(
        (s, code(&b).as_deref()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("stream_with_callback")
        )
    );
    for bad in ["ftp://1.1.1.1/x", "https://user:pw@1.1.1.1/x", "nope"] {
        let (s, b) = submit(&e, json!({"input": "hi", "callback_url": bad})).await;
        assert_eq!(
            (s, code(&b).as_deref()),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Some("invalid_callback_url")
            ),
            "{bad}"
        );
    }
    let (s, b) = submit(&e, json!({"input": "  ", "callback_url": r.url})).await;
    assert_eq!(
        (s, code(&b).as_deref()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("empty_input"))
    );
    // an observer may talk to the job but not read its signing secret
    let (s, b) = send(&e.app, get(SECRET, Some(&member()))).await;
    assert_eq!(
        (s, b["error"]["missing_role"].as_str()),
        (StatusCode::FORBIDDEN, Some("integrations_manager")),
        "{b}"
    );
    // the pending cap: pending_cap_429_per_tenant
}

#[tokio::test]
async fn production_policy_refuses_private_and_plain_http_callbacks() {
    let e = setup(Arc::new(Hello), true).await;
    // the sender as configured from the env: public https only
    let mut st = e.state.clone();
    st.webhooks = Some(Arc::new(Webhooks::new(&master_key())));
    let app = agent_api::router(st);
    for url in [
        "http://127.0.0.1:9/hook",
        "https://127.0.0.1/hook",
        "https://10.0.0.1/hook",
        "https://169.254.169.254/latest",
        "https://[::1]/hook",
        "http://example.com/hook",
    ] {
        let (s, b) = send(
            &app,
            json(
                "POST",
                NATIVE,
                Some(&member()),
                json!({"input": "hi", "callback_url": url}),
            ),
        )
        .await;
        assert_eq!(
            (s, b["error"]["code"].as_str()),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Some("invalid_callback_url")
            ),
            "{url}: {b}"
        );
        assert!(
            !b.to_string().contains(url),
            "the refusal does not echo the URL"
        );
    }
}

#[tokio::test]
async fn unconfigured_is_409_on_both_routes() {
    let e = setup(Arc::new(Hello), false).await;
    let (s, b) = submit(
        &e,
        json!({"input": "hi", "callback_url": "https://1.1.1.1/x"}),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("webhooks_not_configured"))
    );
    let (s, b) = send(&e.app, get(SECRET, Some(&integ()))).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("webhooks_not_configured"))
    );
    // sync interactions are unaffected
    let (s, b) = submit(&e, json!({"input": "hi"})).await;
    assert_eq!(
        (s, b["output_text"].as_str()),
        (StatusCode::OK, Some("Hello"))
    );
}

// ---- durable deliveries (0114) -------------------------------------------------------------------------------------

/// [`Hello`], counting how often it is asked.
struct Counting(Arc<AtomicUsize>);

impl InteractionBackend for Counting {
    fn respond(
        &self,
        req: InteractionRequest,
    ) -> BoxStream<'static, Result<BackendEvent, BackendError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Hello.respond(req)
    }
}

/// The (tenant, job id) of `faq_agent` in the tenant [`setup`] made.
async fn faq_job(e: &TestEnv) -> (Uuid, Uuid) {
    sqlx::query_as("SELECT tenant_id, id FROM jobs WHERE key = 'faq_agent' LIMIT 1")
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

/// `int_<uuid>` → the row id.
fn row_id(id: &str) -> Uuid {
    Uuid::parse_str(id.strip_prefix("int_").unwrap()).unwrap()
}

/// The row's (state, attempts, last_error).
async fn row_state(e: &TestEnv, id: Uuid) -> (String, i32, Option<String>) {
    sqlx::query_as("SELECT state, attempts, last_error FROM interaction_deliveries WHERE id = $1")
        .bind(id)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

/// Wait (≤ 10 s) until the row reaches `state`.
async fn until_state(e: &TestEnv, id: Uuid, state: &str) -> (String, i32, Option<String>) {
    for _ in 0..200 {
        let s = row_state(e, id).await;
        if s.0 == state {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "row {id} never reached {state}: {:?}",
        row_state(e, id).await
    );
}

#[tokio::test]
async fn queued_row_survives_and_is_delivered_by_new_worker() {
    // no worker: the route only stores the row (as if the process died right after the 202)
    let e = setup_idle(Arc::new(Hello), true).await;
    let mut r = receiver(vec![]).await;
    let (s, b) = submit(&e, json!({"input": "hi", "callback_url": r.url})).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["status"], "queued");
    let id = b["id"].as_str().unwrap().to_string();
    let rid = row_id(&id);
    assert_eq!(row_state(&e, rid).await, ("queued".into(), 0, None));
    let (caller, request, url): (Value, Value, String) = sqlx::query_as(
        "SELECT caller, request, callback_url FROM interaction_deliveries WHERE id = $1",
    )
    .bind(rid)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(url, r.url);
    assert_eq!(caller["channel"], "api");
    assert_eq!(request["job_key"], "faq_agent");
    assert_eq!(request["input"][0]["parts"][0]["text"], "hi");
    // a worker claims it and dies mid-answer: the lease is taken, then expires
    let claimed: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM agent_interaction_deliveries_due(10)")
            .fetch_all(&e.owner)
            .await
            .unwrap();
    assert_eq!(claimed, vec![rid]);
    let again: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM agent_interaction_deliveries_due(10)")
            .fetch_all(&e.owner)
            .await
            .unwrap();
    assert!(again.is_empty(), "a leased row is not claimed twice");
    sqlx::query(
        "UPDATE interaction_deliveries SET state = 'answering', lease_until = now() - interval '1 second' \
          WHERE id = $1",
    )
    .bind(rid)
    .execute(&e.owner)
    .await
    .unwrap();
    // a new worker on the same database resumes it
    webhook::spawn_worker(e.state.clone()).unwrap();
    let (h, body) = r.next().await;
    assert_eq!(header(&h, "webhook-id"), id);
    verify(&secret(&e).await, &h, &body);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["status"], "completed");
    assert_eq!(v["output_text"], "Hello");
    assert_eq!(v["created_at"], b["created_at"], "the 202's created_at");
    assert_eq!(
        until_state(&e, rid, "delivered").await,
        ("delivered".into(), 1, None)
    );
    r.quiet(200).await;
}

#[tokio::test]
async fn retries_then_failed_records_last_error() {
    let e = setup(Arc::new(Hello), true).await;
    // three attempts (the test policy), all 5xx
    let mut r = receiver(vec![500, 503, 502]).await;
    let (s, b) = submit(&e, json!({"input": "hi", "callback_url": r.url})).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    for _ in 0..3 {
        r.next().await;
    }
    r.quiet(300).await;
    let rid = row_id(b["id"].as_str().unwrap());
    assert_eq!(
        until_state(&e, rid, "failed").await,
        ("failed".into(), 3, Some("status 502".into()))
    );
    let lease: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT lease_until FROM interaction_deliveries WHERE id = $1")
            .bind(rid)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert!(lease.is_none(), "a final row holds no lease");
    // a 4xx gives up at once
    let mut r = receiver(vec![410]).await;
    let (_, b) = submit(&e, json!({"input": "hi", "callback_url": r.url})).await;
    r.next().await;
    assert_eq!(
        until_state(&e, row_id(b["id"].as_str().unwrap()), "failed").await,
        ("failed".into(), 1, Some("status 410".into()))
    );
}

#[tokio::test]
async fn answer_not_recomputed_on_resume() {
    let calls = Arc::new(AtomicUsize::new(0));
    let e = setup_idle(Arc::new(Counting(calls.clone())), true).await;
    let mut r = receiver(vec![]).await;
    let (tenant, job) = faq_job(&e).await;
    let id = Uuid::now_v7();
    let stored = json!({"id": format!("int_{id}"), "object": "interaction", "status": "completed",
                        "output_text": "stored earlier", "output": []});
    // answered and one attempt made (a 5xx) by a worker that then died; its lease expired
    sqlx::query(
        "INSERT INTO interaction_deliveries \
                (id, tenant_id, job_id, request, callback_url, state, answer, attempts, last_error, lease_until) \
         VALUES ($1, $2, $3, $4, $5, 'delivering', $6, 1, 'status 500', now() - interval '1 second')",
    )
    .bind(id)
    .bind(tenant)
    .bind(job)
    .bind(json!({"v": 1, "job_key": "faq_agent", "channel": "api", "input": [
        {"role": "user", "parts": [{"type": "text", "text": "hi"}]}]}))
    .bind(&r.url)
    .bind(&stored)
    .execute(&e.owner)
    .await
    .unwrap();
    webhook::spawn_worker(e.state.clone()).unwrap();
    let (h, body) = r.next().await;
    assert_eq!(header(&h, "webhook-id"), format!("int_{id}"));
    verify(&secret(&e).await, &h, &body);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v, stored, "the stored answer, as stored");
    assert_eq!(
        until_state(&e, id, "delivered").await,
        ("delivered".into(), 2, None),
        "the attempt count carries over"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the backend was not asked again"
    );
    // a row queued without an answer is answered once
    let (s, b) = submit(&e, json!({"input": "hi", "callback_url": r.url})).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    r.next().await;
    until_state(&e, row_id(b["id"].as_str().unwrap()), "delivered").await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn pending_cap_429_per_tenant() {
    let e = setup_idle(Arc::new(Hello), true).await;
    let r = receiver(vec![]).await;
    let (tenant, job) = faq_job(&e).await;
    sqlx::query(
        "INSERT INTO interaction_deliveries (id, tenant_id, job_id, request, callback_url) \
         SELECT gen_random_uuid(), $1, $2, '{}'::jsonb, 'https://1.1.1.1/x' FROM generate_series(1, $3)",
    )
    .bind(tenant)
    .bind(job)
    .bind(i32::try_from(webhook::MAX_PENDING).unwrap())
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = submit(&e, json!({"input": "hi", "callback_url": r.url})).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (
            StatusCode::TOO_MANY_REQUESTS,
            Some("too_many_pending_interactions")
        ),
        "{b}"
    );
    // another tenant is not affected
    let other = Uuid::new_v4();
    insert_tenant(&e.owner, other, "Beta").await;
    insert_user(&e.owner, "u-beta", other, "tenant_user").await;
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'faq_agent', 'J', '[]', 'test')",
    )
    .bind(other)
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            NATIVE,
            Some(&person("u-beta", &[])),
            json!({"input": "hi", "callback_url": r.url}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    // a final row frees its slot
    sqlx::query(
        "UPDATE interaction_deliveries SET state = 'delivered', answer = '{}' \
          WHERE id = (SELECT id FROM interaction_deliveries WHERE tenant_id = $1 LIMIT 1)",
    )
    .bind(tenant)
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = submit(&e, json!({"input": "hi", "callback_url": r.url})).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
}

#[tokio::test]
async fn delivery_status_route() {
    let e = setup(Arc::new(Hello), true).await;
    let mut r = receiver(vec![]).await;
    let (s, b) = submit(&e, json!({"input": "hi", "callback_url": r.url})).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let id = b["id"].as_str().unwrap().to_string();
    r.next().await;
    until_state(&e, row_id(&id), "delivered").await;
    let path = format!("/api/jobs/faq_agent/interactions/deliveries/{id}");
    let (s, v) = send(&e.app, get(&path, Some(&member()))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["id"], id.as_str());
    assert_eq!(v["state"], "delivered");
    assert_eq!(v["attempts"], 1);
    assert!(v["last_error"].is_null());
    assert!(v["created_at"].is_string() && v["updated_at"].is_string());
    assert!(
        v.get("request").is_none() && v.get("answer").is_none(),
        "no customer text"
    );
    // the bare uuid too
    let bare = format!(
        "/api/jobs/faq_agent/interactions/deliveries/{}",
        row_id(&id)
    );
    let (s, v2) = send(&e.app, get(&bare, Some(&member()))).await;
    assert_eq!((s, &v2["id"]), (StatusCode::OK, &v["id"]));
    // unknown, malformed, another job's
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) \
         SELECT tenant_id, 'other_job', 'O', '[]', 'test' FROM jobs WHERE key = 'faq_agent'",
    )
    .execute(&e.owner)
    .await
    .unwrap();
    for p in [
        format!(
            "/api/jobs/faq_agent/interactions/deliveries/int_{}",
            Uuid::now_v7()
        ),
        "/api/jobs/faq_agent/interactions/deliveries/nope".to_string(),
        format!("/api/jobs/other_job/interactions/deliveries/{id}"),
    ] {
        let (s, v) = send(&e.app, get(&p, Some(&member()))).await;
        assert_eq!(
            (s, v["error"]["code"].as_str()),
            (StatusCode::NOT_FOUND, Some("delivery_not_found")),
            "{p}: {v}"
        );
    }
    // no token
    let (s, _) = send(&e.app, get(&path, None)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

// ---- the claim: tenant status, per-tenant cap, short leases, the claim counter -----------------------------------

/// A non-final row of `faq_agent` stored directly (no lease), in `state` with `claims` claims so far.
async fn insert_row(e: &TestEnv, url: &str, state: &str, claims: i32) -> Uuid {
    let (tenant, job) = faq_job(e).await;
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO interaction_deliveries (id, tenant_id, job_id, request, callback_url, state, claims) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(tenant)
    .bind(job)
    .bind(json!({"v": 1, "job_key": "faq_agent", "channel": "api", "input": [
        {"role": "user", "parts": [{"type": "text", "text": "hi"}]}]}))
    .bind(url)
    .bind(state)
    .bind(claims)
    .execute(&e.owner)
    .await
    .unwrap();
    id
}

/// The tenants of the rows one claim (`agent_interaction_deliveries_due(64)`) takes.
async fn claim(e: &TestEnv) -> Vec<Uuid> {
    sqlx::query_scalar("SELECT tenant_id FROM agent_interaction_deliveries_due(64)")
        .fetch_all(&e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn suspended_tenant_rows_are_not_answered() {
    let calls = Arc::new(AtomicUsize::new(0));
    let e = setup_idle(Arc::new(Counting(calls.clone())), true).await;
    let mut r = receiver(vec![]).await;
    let (tenant, _) = faq_job(&e).await;
    let id = insert_row(&e, &r.url, "queued", 0).await;
    sqlx::query("UPDATE tenants SET status = 'suspended', suspended_at = now() WHERE id = $1")
        .bind(tenant)
        .execute(&e.owner)
        .await
        .unwrap();
    assert!(
        claim(&e).await.is_empty(),
        "a suspended tenant's row is not claimed"
    );
    webhook::spawn_worker(e.state.clone()).unwrap();
    r.quiet(500).await;
    assert_eq!(
        row_state(&e, id).await,
        ("queued".into(), 0, None),
        "it waits"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the backend was not asked");
    // reactivated: the row is answered and delivered
    sqlx::query("UPDATE tenants SET status = 'active', suspended_at = NULL WHERE id = $1")
        .bind(tenant)
        .execute(&e.owner)
        .await
        .unwrap();
    e.state.webhooks.as_ref().unwrap().kick.notify_one();
    r.next().await;
    until_state(&e, id, "delivered").await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn claim_caps_rows_per_tenant() {
    let e = setup_idle(Arc::new(Hello), true).await;
    let (a, job) = faq_job(&e).await;
    sqlx::query(
        "INSERT INTO interaction_deliveries (id, tenant_id, job_id, request, callback_url) \
         SELECT gen_random_uuid(), $1, $2, '{}'::jsonb, 'https://1.1.1.1/x' FROM generate_series(1, 20)",
    )
    .bind(a)
    .bind(job)
    .execute(&e.owner)
    .await
    .unwrap();
    let b = Uuid::new_v4();
    insert_tenant(&e.owner, b, "Beta").await;
    sqlx::query(
        "WITH j AS (INSERT INTO jobs (tenant_id, key, label, pins, created_by) \
                    VALUES ($1, 'faq_agent', 'J', '[]', 'test') RETURNING id) \
         INSERT INTO interaction_deliveries (id, tenant_id, job_id, request, callback_url) \
         SELECT gen_random_uuid(), $1, j.id, '{}'::jsonb, 'https://1.1.1.1/x' FROM j, generate_series(1, 3)",
    )
    .bind(b)
    .execute(&e.owner)
    .await
    .unwrap();
    let count = |got: &[Uuid], t: Uuid| got.iter().filter(|x| **x == t).count();
    let got = claim(&e).await;
    assert_eq!(
        (count(&got, a), count(&got, b)),
        (8, 3),
        "8 of A's 20, all of B's 3"
    );
    assert!(claim(&e).await.is_empty(), "A holds its 8 leases");
    // three of A's leases expire: three more, the five still held counted
    sqlx::query(
        "UPDATE interaction_deliveries SET lease_until = now() - interval '1 second' \
          WHERE id IN (SELECT id FROM interaction_deliveries \
                        WHERE tenant_id = $1 AND lease_until IS NOT NULL LIMIT 3)",
    )
    .bind(a)
    .execute(&e.owner)
    .await
    .unwrap();
    let got = claim(&e).await;
    assert_eq!((count(&got, a), count(&got, b)), (3, 0));
    let leased: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM interaction_deliveries WHERE tenant_id = $1 AND lease_until > now()",
    )
    .bind(a)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(leased, 8);
}

#[tokio::test]
async fn expired_lease_is_reclaimed_quickly() {
    let e = setup_idle(Arc::new(Hello), true).await;
    let mut r = receiver(vec![]).await;
    let id = insert_row(&e, &r.url, "queued", 0).await;
    assert_eq!(claim(&e).await.len(), 1);
    let (left, claims): (f64, i32) = sqlx::query_as(
        "SELECT extract(epoch FROM lease_until - now())::float8, claims FROM interaction_deliveries WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(
        left > 50.0 && left <= 60.0,
        "a 60 s lease, not the answer deadline: {left}"
    );
    assert_eq!(claims, 1);
    assert!(claim(&e).await.is_empty(), "held");
    // the worker died: no heartbeat renewed it, and a minute later it is claimed again
    sqlx::query(
        "UPDATE interaction_deliveries SET state = 'answering', lease_until = now() - interval '1 second' \
          WHERE id = $1",
    )
    .bind(id)
    .execute(&e.owner)
    .await
    .unwrap();
    webhook::spawn_worker(e.state.clone()).unwrap();
    r.next().await;
    assert_eq!(
        until_state(&e, id, "delivered").await,
        ("delivered".into(), 1, None)
    );
    let claims: i32 = sqlx::query_scalar("SELECT claims FROM interaction_deliveries WHERE id = $1")
        .bind(id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(claims, 2, "the claim counter counts the reclaim");
}

#[tokio::test]
async fn row_failed_after_too_many_claims() {
    let calls = Arc::new(AtomicUsize::new(0));
    let e = setup_idle(Arc::new(Counting(calls.clone())), true).await;
    let mut r = receiver(vec![]).await;
    // claimed 5 times already without an answer (a request that kills its worker): the 6th claim fails it
    let dead = insert_row(&e, &r.url, "answering", webhook::MAX_CLAIMS).await;
    webhook::spawn_worker(e.state.clone()).unwrap();
    assert_eq!(
        until_state(&e, dead, "failed").await,
        ("failed".into(), 0, Some("too_many_claims".into()))
    );
    r.quiet(300).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0, "not answered again");
    // one claim fewer is still answered (its 5th claim)
    let alive = insert_row(&e, &r.url, "answering", webhook::MAX_CLAIMS - 1).await;
    e.state.webhooks.as_ref().unwrap().kick.notify_one();
    r.next().await;
    until_state(&e, alive, "delivered").await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
