//! KH T6 (D109): one answer path. Every customer party — an API key on `/interactions` and `/chat/completions`, MCP,
//! voice, an async webhook interaction, the edge's channels (R6 accept-then-answer included) — lands in the B4 runtime:
//! its conversation is created first, the message goes through `turn::accept_customer_message`, and the reply is the
//! line the runtime stored (job `faq_agent` / `j1` binds no qualifier, so the drain answers `TURN_FAILED`). Without a
//! live release it is 409 `job_not_live`; live but served by no runtime process it is 503 `unavailable`. The staff
//! draft playground (a member calling without a party) keeps `AppState.interaction` — the counting [`Playground`]
//! here — and no customer path ever reaches it or constructs `LlmPassthrough` (the source guard at the end). Real
//! Postgres 18 and the shared test Redis (a fresh prefix per test).
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_api::interaction::webhook::{self, Webhooks};
use agent_api::interaction::{BackendError, BackendEvent, InteractionBackend, InteractionRequest};
use agent_api::runtime::Env;
use agent_api::turn::TURN_FAILED;
use agent_api::voice::{Clip, PlatformSpeech, SpeechServices, VoiceError};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream::{self, BoxStream};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::conv::{self, Ctx};
use crate::common::{TestEnv, insert_tenant, insert_user, json as req, person, send};
use crate::redis_env;

/// The staff playground's answer.
const PLAYGROUND: &str = "playground answer";

/// The staff playground backend (`AppState.interaction`): counts its calls.
#[derive(Default)]
struct Playground {
    calls: AtomicUsize,
}

impl Playground {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl InteractionBackend for Playground {
    fn respond(
        &self,
        _req: InteractionRequest,
    ) -> BoxStream<'static, Result<BackendEvent, BackendError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        stream::iter(vec![
            Ok(BackendEvent::Delta(PLAYGROUND.into())),
            Ok(BackendEvent::Done { usage: None }),
        ])
        .boxed()
    }
}

/// Hears [`HEARD`], speaks a fixed WAV.
struct Speech;

const HEARD: &str = "Where is my order?";

impl SpeechServices for Speech {
    fn transcribe(&self, _clip: Clip) -> BoxFuture<'static, Result<String, VoiceError>> {
        Box::pin(async { Ok(HEARD.to_string()) })
    }

    fn synthesize(
        &self,
        _text: String,
        _voice: Option<String>,
    ) -> BoxFuture<'static, Result<Vec<u8>, VoiceError>> {
        Box::pin(async { Ok(b"RIFF\x24\x00\x00\x00WAVEfmt fake".to_vec()) })
    }
}

/// Built at runtime, so no key literal is committed.
fn webhook_key() -> String {
    "k".repeat(48)
}

struct Fx {
    e: TestEnv,
    t: Uuid,
    job: Uuid,
    play: Arc<Playground>,
    webhooks: Arc<Webhooks>,
    /// The `faq_agent` API key: its id (the customer party `api:<id>`) and secret.
    key_id: String,
    secret: String,
}

/// Tenant Alpha with a member `u-member`, an integrations manager `u-im` and job `faq_agent` (one API key); the
/// counting playground; the platform speech gateway lent to Alpha; async interactions on (local receivers allowed,
/// one attempt). `live`: the job has a live release; `served`: a live `interaction` process serves its prod scope.
async fn fx(test: &str, live: bool, served: bool) -> Fx {
    let vars = redis_env::api_vars(test).await;
    let mut e = crate::common::env_with(|v| {
        v.extend(vars);
        v.insert("INTERACTION_WEBHOOK_KEY".into(), webhook_key());
    })
    .await;
    let play = Arc::new(Playground::default());
    e.state.interaction = play.clone();
    let t = Uuid::new_v4();
    let speech: Arc<dyn SpeechServices> = Arc::new(Speech);
    e.state.speech = Arc::new(PlatformSpeech::new(Some(speech), [t]));
    let mut w = Webhooks::new(&webhook_key());
    w.allow_private = true;
    w.retry = vec![Duration::ZERO];
    let webhooks = Arc::new(w);
    e.state.webhooks = Some(webhooks.clone());
    e.app = agent_api::router(e.state.clone());
    insert_tenant(&e.owner, t, "Alpha").await;
    // B7 gate 2 fails closed: Alpha runs turns, so it holds a fresh active licence verdict
    crate::common::licence_active(&e.owner, t).await;
    insert_user(&e.owner, "u-member", t, "tenant_user").await;
    insert_user(&e.owner, "u-im", t, "integrations_manager").await;
    let job: Uuid = if live {
        // a released runtime design (D109)
        conv::live_job(&e, t, "faq_agent").await
    } else {
        sqlx::query_scalar(
            "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'faq_agent', 'J', '[]', 'test') \
             RETURNING id",
        )
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap()
    };
    if served {
        conv::serve(&e.state, t, "faq_agent", Env::Prod).await;
    }
    let (s, b) = send(
        &e.app,
        req(
            "POST",
            "/api/jobs/faq_agent/api-keys",
            Some(&person("u-im", &[])),
            json!({"label": "bot"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    Fx {
        e,
        t,
        job,
        play,
        webhooks,
        key_id: b["key"]["id"].as_str().unwrap().to_string(),
        secret: b["secret"].as_str().unwrap().to_string(),
    }
}

impl Fx {
    /// `POST uri` with the API key.
    async fn keyed(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        let r = Request::post(uri)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", self.secret))
            .body(Body::from(body.to_string()))
            .unwrap();
        send(&self.e.app, r).await
    }

    /// `(channel, customer_id)` of every conversation, sorted.
    async fn conversations(&self) -> Vec<(String, String)> {
        let mut rows: Vec<(String, String)> =
            sqlx::query_as("SELECT channel, customer_id FROM conversation")
                .fetch_all(&self.e.owner)
                .await
                .unwrap();
        rows.sort();
        rows
    }

    /// `(role, text)` of every message, in order.
    async fn messages(&self) -> Vec<(String, String)> {
        sqlx::query_as("SELECT role, text FROM conversation_message ORDER BY at_ms, message_id")
            .fetch_all(&self.e.owner)
            .await
            .unwrap()
    }

    fn party(&self) -> String {
        format!("api:{}", self.key_id)
    }
}

fn code(b: &Value) -> Option<&str> {
    b["error"]["code"].as_str()
}

/// `(caller text, agent line)` as the runtime stores one turn of an API key caller (role `caller`, 0234).
fn turn(caller: &str) -> Vec<(String, String)> {
    vec![
        ("caller".to_string(), caller.to_string()),
        ("agent".to_string(), TURN_FAILED.to_string()),
    ]
}

#[tokio::test]
async fn api_key_interactions_land_in_the_runtime() {
    let f = fx("kh-t6-api", true, true).await;
    let (s, b) = f
        .keyed(
            "/api/jobs/faq_agent/interactions",
            json!({"input": "Where is my order?"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["output_text"], TURN_FAILED, "{b}");
    assert_eq!(f.conversations().await, [("api".to_string(), f.party())]);
    assert_eq!(f.messages().await, turn("Where is my order?"));
    // the OpenAI chat-completions shape: its own stateless request, its own conversation
    let (s, b) = f
        .keyed(
            "/api/jobs/faq_agent/chat/completions",
            json!({"messages": [{"role": "user", "content": "And my invoice?"}]}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["choices"][0]["message"]["content"], TURN_FAILED, "{b}");
    assert_eq!(f.conversations().await.len(), 2);
    assert_eq!(f.play.calls(), 0, "a customer never reaches the playground");
}

#[tokio::test]
async fn the_staff_playground_keeps_its_backend() {
    let f = fx("kh-t6-playground", false, false).await;
    let (s, b) = send(
        &f.e.app,
        req(
            "POST",
            "/api/jobs/faq_agent/interactions",
            Some(&person("u-member", &[])),
            json!({"input": "Draft check"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["output_text"], PLAYGROUND, "{b}");
    assert_eq!(f.play.calls(), 1);
    assert!(
        f.conversations().await.is_empty(),
        "the playground stores nothing"
    );
    // the same member naming a customer party is a customer: not live here
    let (s, b) = send(
        &f.e.app,
        req(
            "POST",
            "/api/jobs/faq_agent/interactions",
            Some(&person("u-member", &[])),
            json!({"input": "hi", "party": {"id": "crm:7"}}),
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, Some("job_not_live")),
        "{b}"
    );
    assert_eq!(f.play.calls(), 1);
}

#[tokio::test]
async fn no_live_release_is_409_and_unserved_is_503() {
    let f = fx("kh-t6-not-live", false, true).await;
    let (s, b) = f
        .keyed("/api/jobs/faq_agent/interactions", json!({"input": "hi"}))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, Some("job_not_live")),
        "{b}"
    );
    // streamed: the refusal is the normal error JSON, before the stream starts
    let (s, b) = f
        .keyed(
            "/api/jobs/faq_agent/interactions",
            json!({"input": "hi", "stream": true}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, Some("job_not_live")),
        "{b}"
    );
    assert!(f.conversations().await.is_empty(), "nothing stored");
    assert_eq!(f.play.calls(), 0);

    let f = fx("kh-t6-unserved", true, false).await;
    let (s, b) = f
        .keyed("/api/jobs/faq_agent/interactions", json!({"input": "hi"}))
        .await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Some("interaction_backend_unavailable")
        ),
        "{b}"
    );
    assert!(f.conversations().await.is_empty(), "nothing stored");
    assert_eq!(f.play.calls(), 0);
}

#[tokio::test]
async fn mcp_ask_lands_in_the_runtime() {
    let f = fx("kh-t6-mcp", true, true).await;
    let (s, b) = f
        .keyed(
            "/api/jobs/faq_agent/mcp",
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                   "params": {"name": "ask", "arguments": {"input": "Do you deliver?"}}}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["result"],
        json!({"content": [{"type": "text", "text": TURN_FAILED}], "isError": false}),
        "{b}"
    );
    // `mcp` is no conversation channel: the conversation is an `api` one
    assert_eq!(f.conversations().await, [("api".to_string(), f.party())]);
    assert_eq!(f.messages().await, turn("Do you deliver?"));
    assert_eq!(f.play.calls(), 0);
}

/// A multipart voice turn: the audio field and the `history` field.
fn voice_turn(secret: &str, history: &Value) -> Request<Body> {
    let boundary = "kh-t6-boundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"audio\"; filename=\"rec\"\r\n\
             Content-Type: audio/webm;codecs=opus\r\n\r\nwebm-bytes\r\n\
             --{boundary}\r\nContent-Disposition: form-data; name=\"history\"\r\n\r\n{history}\r\n\
             --{boundary}--\r\n"
        )
        .as_bytes(),
    );
    Request::post("/api/jobs/faq_agent/voice/turns")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("authorization", format!("Bearer {secret}"))
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn voice_turn_lands_in_the_runtime() {
    let f = fx("kh-t6-voice", true, true).await;
    let history = json!([{"role": "user", "content": "I ordered shoes"}, {"role": "assistant", "content": "Which order?"}]);
    let (s, b) = send(&f.e.app, voice_turn(&f.secret, &history)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["transcript"], HEARD);
    assert_eq!(b["reply_text"], TURN_FAILED, "{b}");
    assert!(!b["audio_b64"].as_str().unwrap().is_empty(), "{b}");
    assert_eq!(f.conversations().await, [("voice".to_string(), f.party())]);
    // the history seeds the turn's conversation: the runtime runs on it
    let mut want = vec![
        ("customer".to_string(), "I ordered shoes".to_string()),
        ("agent".to_string(), "Which order?".to_string()),
    ];
    want.extend(turn(HEARD));
    assert_eq!(f.messages().await, want);
    assert_eq!(f.play.calls(), 0);
}

#[tokio::test]
async fn async_webhook_interaction_lands_in_the_runtime() {
    let f = fx("kh-t6-webhook", true, true).await;
    // nothing listens there: the answer is stored before the (failing) delivery
    let dead = redis_env::dead_addr().await;
    let (s, b) = f
        .keyed(
            "/api/jobs/faq_agent/interactions",
            json!({"input": "Where is my refund?", "callback_url": format!("http://{dead}/hook")}),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(webhook::run_due(&f.e.state, &f.webhooks).await, 1);
    let mut answer = Value::Null;
    for _ in 0..150 {
        let a: Option<Option<Value>> =
            sqlx::query_scalar("SELECT answer FROM interaction_deliveries")
                .fetch_optional(&f.e.owner)
                .await
                .unwrap();
        if let Some(Some(a)) = a {
            answer = a;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(answer["status"], "completed", "{answer}");
    assert_eq!(answer["output_text"], TURN_FAILED, "{answer}");
    assert_eq!(f.conversations().await, [("api".to_string(), f.party())]);
    assert_eq!(f.messages().await, turn("Where is my refund?"));
    assert_eq!(f.play.calls(), 0);
}

/// A customer-side Telegram binding of `j1` (core-only kind, R6's accept-then-answer).
async fn telegram(c: &Ctx) -> Value {
    let (s, b) = c
        .call(
            "POST",
            "/api/jobs/j1/channel-bindings",
            "u-int",
            json!({"kind": "telegram", "side": "customer", "label": "Test bot",
                   "settings": {"allowed_user_ids": [42]},
                   "credential": {"bot_token": "123456789:test-not-a-real-bot-token-000000",
                                  "webhook_secret": "test-webhook-secret-0001"}}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    c.verify_all().await;
    // D130: a customer binding starts in the test env; this answer path is the live one
    c.go_live().await;
    b
}

/// `role:text` of Telegram message 9001 and of every line that is not a customer's, in order.
async fn stored(c: &Ctx) -> Vec<String> {
    c.texts(
        "SELECT role || ':' || text FROM conversation_message \
          WHERE external_id = '9001' OR role <> 'customer' ORDER BY at_ms, message_id",
    )
    .await
}

/// B7 R6 persist-before-200 on the runtime: `accept_only` stores the (redacted) message without starting its turn and
/// answers `accepted`; a resent accept is `accepted` again (not re-stored); the call without the flag runs the turn on
/// the stored message once; any call after that is `duplicate`.
#[tokio::test]
async fn edge_accept_then_answer_runs_one_runtime_turn() {
    let c = conv::setup_runtime(redis_env::api_vars("kh-t6-accept").await).await;
    let tg = telegram(&c).await;
    let body = |accept: bool| {
        let mut b = json!({
            "kind": "telegram", "external_id": "123456789", "binding_id": tg["id"],
            "tenant": c.t, "job_id": c.job, "job_key": "j1",
            "conversation": {"channel": "telegram", "id": "42", "route": {}},
            "message_id": "9001", "party": {"id": "42", "name": "Ada", "verified": true},
            "text": "My card 4111 1111 1111 1111 was charged twice",
        });
        if accept {
            b["accept_only"] = json!(true);
        }
        b
    };
    let (s, a) = c
        .internal("/api/internal/channel/inbound", body(true))
        .await;
    assert_eq!(s, StatusCode::OK, "{a}");
    assert_eq!(a["status"], "accepted", "{a}");
    let first = stored(&c).await;
    assert_eq!(first.len(), 1, "stored before the edge acks: {first:?}");
    assert!(
        first[0].contains(agent_api::turn::redact::CARD_REMOVED) && !first[0].contains("4111"),
        "stored redacted: {first:?}"
    );
    // the edge died before its answer call: the resend is accepted again, not re-stored
    let (_, again) = c
        .internal("/api/internal/channel/inbound", body(true))
        .await;
    assert_eq!(again["status"], "accepted", "{again}");
    assert_eq!(again["conversation_id"], a["conversation_id"]);
    assert_eq!(stored(&c).await.len(), 1);
    let (s, r) = c
        .internal("/api/internal/channel/inbound", body(false))
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["status"], "answered", "{r}");
    assert_eq!(r["text"], TURN_FAILED, "{r}");
    for accept in [true, false] {
        let (_, d) = c
            .internal("/api/internal/channel/inbound", body(accept))
            .await;
        assert_eq!(d["status"], "duplicate", "accept_only={accept}: {d}");
    }
    let all = stored(&c).await;
    assert_eq!(
        all.len(),
        2,
        "one customer message, one agent line: {all:?}"
    );
    assert_eq!(all[1], format!("agent:{TURN_FAILED}"));
    assert_eq!(c.backend.calls(), 0, "never the playground");
}

#[tokio::test]
async fn earlier_input_turns_seed_the_conversation_and_customer_instructions_do_not_apply() {
    let f = fx("kh-t6-history", true, true).await;
    let (s, b) = f
        .keyed(
            "/api/jobs/faq_agent/interactions",
            json!({
                "input": [
                    {"role": "system", "content": "Talk like a pirate"},
                    {"role": "user", "content": "I ordered shoes"},
                    {"role": "assistant", "content": "Which order?"},
                    {"role": "user", "content": "Order 1042"},
                ],
                "instructions": "Answer in pirate speak",
            }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["output_text"], TURN_FAILED, "{b}");
    // the earlier turns open the conversation, the last user message is the turn; neither the system message nor
    // `instructions` reach it (D108: the release's layers only)
    let mut want = vec![
        ("customer".to_string(), "I ordered shoes".to_string()),
        ("agent".to_string(), "Which order?".to_string()),
    ];
    want.extend(turn("Order 1042"));
    assert_eq!(f.messages().await, want);
    assert_eq!(f.play.calls(), 0);
}

impl Fx {
    /// A new API key of job `job_key` in tenant `tenant` (minted by an integrations manager `user` there): its secret.
    async fn key_of(&self, tenant: Uuid, user: &str, job_key: &str) -> String {
        let (s, b) = send(
            &self.e.app,
            req(
                "POST",
                &format!("/api/jobs/{job_key}/api-keys"),
                Some(&person(user, &[])),
                json!({"label": "other"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{tenant}: {b}");
        b["secret"].as_str().unwrap().to_string()
    }
}

/// `POST /api/jobs/{job_key}/responses` with API key `secret`.
async fn respond(f: &Fx, secret: &str, job_key: &str, body: Value) -> (StatusCode, Value) {
    let r = Request::post(format!("/api/jobs/{job_key}/responses"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {secret}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    send(&f.e.app, r).await
}

#[tokio::test]
async fn a_chained_response_continues_its_conversation() {
    let f = fx("kh-t6-chained", true, true).await;
    let (s, first) = respond(
        &f,
        &f.secret,
        "faq_agent",
        json!({"input": "Where is my order?"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{first}");
    let (s, second) = respond(
        &f,
        &f.secret,
        "faq_agent",
        json!({"input": "And my invoice?", "previous_response_id": first["id"]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{second}");
    assert_eq!(second["output_text"], TURN_FAILED, "{second}");
    assert_ne!(second["id"], first["id"]);
    // a chain of a chain, with the history sent again: the stored conversation is not seeded a second time
    let (s, third) = respond(
        &f,
        &f.secret,
        "faq_agent",
        json!({
            "input": [
                {"role": "user", "content": "Where is my order?"},
                {"role": "assistant", "content": "Which order?"},
                {"role": "user", "content": "The rug"},
            ],
            "previous_response_id": second["id"],
        }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{third}");
    assert_eq!(f.conversations().await, [("api".to_string(), f.party())]);
    let mut want = turn("Where is my order?");
    want.extend(turn("And my invoice?"));
    want.extend(turn("The rug"));
    assert_eq!(f.messages().await, want);
    // the stream form continues it too
    let (s, sse) = respond(
        &f,
        &f.secret,
        "faq_agent",
        json!({"input": "Thanks", "stream": true, "previous_response_id": third["id"]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{sse}");
    assert!(
        sse.as_str()
            .is_some_and(|t| t.contains("response.completed")),
        "{sse}"
    );
    assert_eq!(f.conversations().await.len(), 1);
    assert_eq!(f.play.calls(), 0);
}

#[tokio::test]
async fn a_previous_response_of_another_party_job_or_tenant_is_404() {
    let f = fx("kh-t6-chained-404", true, true).await;
    let (s, first) = respond(
        &f,
        &f.secret,
        "faq_agent",
        json!({"input": "Where is my order?"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{first}");
    let prev = first["id"].clone();
    let chained = |id: &Value| json!({"input": "And my invoice?", "previous_response_id": id});
    // another API key of the same job is another customer party
    let other_party = f.key_of(f.t, "u-im", "faq_agent").await;
    // another job of the same tenant
    let other_job_id = conv::live_job(&f.e, f.t, "other_agent").await;
    let other_job = f.key_of(f.t, "u-im", "other_agent").await;
    // another tenant, whose job has the same key
    let beta = Uuid::new_v4();
    insert_tenant(&f.e.owner, beta, "Beta").await;
    insert_user(&f.e.owner, "u-im-beta", beta, "integrations_manager").await;
    let beta_job: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'faq_agent', 'J', '[]', 'test') \
         RETURNING id",
    )
    .bind(beta)
    .fetch_one(&f.e.owner)
    .await
    .unwrap();
    let other_tenant = f.key_of(beta, "u-im-beta", "faq_agent").await;
    let unknown = json!(format!("resp_{}", Uuid::now_v7().simple()));
    for (what, secret, job, id) in [
        ("unknown", &f.secret, "faq_agent", &unknown),
        ("malformed", &f.secret, "faq_agent", &json!("resp_1")),
        ("party", &other_party, "faq_agent", &prev),
        ("job", &other_job, "other_agent", &prev),
        ("tenant", &other_tenant, "faq_agent", &prev),
    ] {
        let (s, b) = respond(&f, secret, job, chained(id)).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::NOT_FOUND, Some("response_not_found")),
            "{what}: {b}"
        );
    }
    // nothing was written by the refusals
    assert_eq!(f.conversations().await, [("api".to_string(), f.party())]);
    assert_eq!(f.messages().await, turn("Where is my order?"));
    // the staff playground stores no conversation to continue
    let (s, b) = send(
        &f.e.app,
        req(
            "POST",
            "/api/jobs/faq_agent/responses",
            Some(&person("u-member", &[])),
            chained(&prev),
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::BAD_REQUEST, Some("unsupported_parameter")),
        "{b}"
    );
    assert_eq!(f.play.calls(), 0);
    // the same customer party's own turn, but stored under another job or another tenant: the job and tenant
    // filters refuse it on their own (the party matches)
    for (what, tenant, job) in [("job", f.t, other_job_id), ("tenant", beta, beta_job)] {
        let request = Uuid::now_v7();
        plant_api_turn(&f, tenant, job, &f.party(), request).await;
        let id = json!(format!("resp_{}", request.simple()));
        let (s, b) = respond(&f, &f.secret, "faq_agent", chained(&id)).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::NOT_FOUND, Some("response_not_found")),
            "planted in another {what}: {b}"
        );
    }
}

/// An `api` conversation of `customer` in `tenant`'s job `job`, with its customer ref and one customer message of
/// request `request` (as `customer_turn` stores one), written directly.
async fn plant_api_turn(f: &Fx, tenant: Uuid, job: Uuid, customer: &str, request: Uuid) {
    let conv = format!("conv-planted-{}", request.simple());
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id, started_at) \
         VALUES ($1, $2, $3, 'prod', 'api', $4, now())",
    )
    .bind(tenant)
    .bind(job)
    .bind(&conv)
    .bind(customer)
    .execute(&f.e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO channel_conversation_refs \
            (tenant_id, job_id, conversation_id, side, item_id, channel, external_id, route, updated_at) \
         VALUES ($1, $2, $3, 'customer', NULL, 'api', $4, '{}', now())",
    )
    .bind(tenant)
    .bind(job)
    .bind(&conv)
    .bind(request.to_string())
    .execute(&f.e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_message (tenant_id, job_id, conversation_id, role, text, at_ms, external_id) \
         VALUES ($1, $2, $3, 'customer', 'hi', 1, $4)",
    )
    .bind(tenant)
    .bind(job)
    .bind(&conv)
    .bind(request.to_string())
    .execute(&f.e.owner)
    .await
    .unwrap();
}

#[tokio::test]
async fn an_api_caller_cannot_label_its_conversation_a_connector_channel() {
    let f = fx("kh-t6-channel", true, true).await;
    for (header, uri, body) in [
        (
            "helpdesk",
            "/api/jobs/faq_agent/chat/completions",
            json!({"messages": [{"role": "user", "content": "hi"}]}),
        ),
        (
            "telegram",
            "/api/jobs/faq_agent/responses",
            json!({"input": "hi"}),
        ),
    ] {
        let r = Request::post(uri)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", f.secret))
            .header("x-interaction-channel", header)
            .body(Body::from(body.to_string()))
            .unwrap();
        let (s, b) = send(&f.e.app, r).await;
        assert_eq!(s, StatusCode::OK, "{header}: {b}");
    }
    let (s, b) = f
        .keyed(
            "/api/jobs/faq_agent/interactions",
            json!({"input": "hi", "channel": "web_chat"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let channels: Vec<String> = f.conversations().await.into_iter().map(|c| c.0).collect();
    assert_eq!(channels, ["api", "api", "api"]);
}

#[tokio::test]
async fn a_live_release_whose_design_does_not_compile_is_409_job_not_live() {
    let f = fx("kh-t6-no-design", false, true).await;
    // a live release, but the job pins no template that compiles: the runtime would run without a design
    conv::fake_release(
        &f.e.owner,
        f.t,
        f.job,
        r#"[{"layer": "job/customer_care_agent", "version": 4}]"#,
    )
    .await;
    let (s, b) = f
        .keyed("/api/jobs/faq_agent/interactions", json!({"input": "hi"}))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, Some("job_not_live")),
        "{b}"
    );
    let why = b["error"]["message"].as_str().unwrap_or_default();
    assert!(
        why.contains("no runtime design ("),
        "the first template error is named for operators: {b}"
    );
    assert!(f.conversations().await.is_empty());
    assert_eq!(f.play.calls(), 0);
}

// ---- R15 (KH-F3): a job live on a release cut before KH ---------------------------------------------------------

/// `sel` (the hierarchy shape) as a release cut before migration 0132 stored it: bare `industry_depts`, the region
/// under `dimensions.region[]`. [`selection_v2`](agent_api::platform::selection::selection_v2) round-trips it.
fn pre_kh(sel: &Value) -> Value {
    let industry = sel["industry"].as_str().unwrap().to_string();
    let prefix = format!("industry_dept/{industry}/");
    let depts: Vec<Value> = sel["departments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| json!(d.as_str().unwrap().strip_prefix(&prefix).unwrap()))
        .collect();
    let mut dims = sel["dimensions"].clone();
    dims["region"] = match &sel["region"] {
        Value::String(r) => json!([r]),
        _ => json!([]),
    };
    // every other field (profile, profile_values, areas, …) is kept as it was cut
    let mut legacy = sel.clone();
    let o = legacy.as_object_mut().unwrap();
    o.remove("departments");
    o.remove("region");
    o.insert("industry_depts".into(), json!(depts));
    o.insert("dimensions".into(), dims);
    assert_eq!(
        agent_api::platform::selection::selection_v2(&legacy),
        *sel,
        "the legacy shape upgrades back to the cut selection"
    );
    legacy
}

/// Rewrite release `release` of `job` as a pre-KH cut: `job_release.selection` and its `release_snapshot`
/// `selection` row hold [`pre_kh`] of what was cut. Both are immutable (D62), so the triggers are disabled inside one
/// owner transaction; the release `hash` moves too, so no snapshot LRU entry of the new shape survives.
async fn make_pre_kh(e: &TestEnv, job: Uuid, release: &str) {
    let mut tx = e.owner.begin().await.unwrap();
    let cut: Value = sqlx::query_scalar(
        "SELECT selection FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(job)
    .bind(release)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let legacy = pre_kh(&cut);
    sqlx::raw_sql(
        "ALTER TABLE job_release DISABLE TRIGGER job_release_immutable; \
         ALTER TABLE release_snapshot DISABLE TRIGGER release_snapshot_immutable",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE job_release SET selection = $3, \
                hash = 'sha256:' || encode(sha256(convert_to(hash || ':r15', 'UTF8')), 'hex') \
          WHERE job_id = $1 AND release_id = $2",
    )
    .bind(job)
    .bind(release)
    .bind(&legacy)
    .execute(&mut *tx)
    .await
    .unwrap();
    let n = sqlx::query(
        "UPDATE release_snapshot SET snap = jsonb_build_object('v', $3::jsonb) \
          WHERE job_id = $1 AND release_id = $2 AND key = 'selection'",
    )
    .bind(job)
    .bind(release)
    .bind(&legacy)
    .execute(&mut *tx)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(n, 1, "the release snapshot holds the selection");
    sqlx::raw_sql(
        "ALTER TABLE job_release ENABLE TRIGGER job_release_immutable; \
         ALTER TABLE release_snapshot ENABLE TRIGGER release_snapshot_immutable",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

async fn current_release(e: &TestEnv, job: Uuid) -> String {
    sqlx::query_scalar("SELECT current_release_id FROM jobs WHERE id = $1")
        .bind(job)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

impl Fx {
    /// One API-key customer turn: answered by the runtime (200, its stored line), never 409 `job_not_live`.
    async fn answers(&self, text: &str) {
        // the runtime design cache may hold the scope's design from before the rewrite
        let scope = agent_api::runtime::Scope::new(self.t, "faq_agent", Env::Prod).unwrap();
        self.e.state.designs.invalidate(&scope);
        let (s, b) = self
            .keyed("/api/jobs/faq_agent/interactions", json!({"input": text}))
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["output_text"], TURN_FAILED, "{b}");
    }

    /// The runtime design of the Live scope, loaded fresh.
    async fn live_template_errors(&self) -> Vec<String> {
        let f = crate::common::design_contract::contract_job(self.t, self.job, "faq_agent");
        let mut tx = f.job.begin(&self.e.state.db).await.unwrap();
        let d = agent_api::runtime::design::RuntimeDesign::load(
            &mut tx,
            &self.e.state.design,
            &f.job_ref(),
            &agent_api::design::DesignOf::Live,
        )
        .await
        .expect("the live design loads");
        tx.commit().await.unwrap();
        d.template_errors
    }
}

/// Ruling R15: migration 0132 leaves the release identity (`job_release.selection`, `release_snapshot`) in the shape
/// it was cut with (D62). A job live on such a release still compiles — every reader upgrades the selection in
/// memory (`design::read::selection`) — so customers reach it, it is not 409 `job_not_live`.
#[tokio::test]
async fn a_job_live_on_a_pre_kh_release_answers_customers() {
    let f = fx("kh-t6-r15-live", true, true).await;
    let release = current_release(&f.e, f.job).await;
    make_pre_kh(&f.e, f.job, &release).await;
    assert!(
        f.live_template_errors().await.is_empty(),
        "the legacy live selection compiles"
    );
    // the job design's bases of that release read its selection upgraded too
    let cj = crate::common::design_contract::contract_job(f.t, f.job, "faq_agent");
    let mut tx = cj.job.begin(&f.e.state.db).await.unwrap();
    let bases = agent_api::job_design::base::of_release(&mut tx, &cj.job_ref(), &release)
        .await
        .unwrap()
        .expect("the release exists");
    tx.commit().await.unwrap();
    assert!(
        bases.template_errors.is_empty(),
        "{:?}",
        bases.template_errors
    );
    assert_eq!(
        bases.selection.departments,
        ["industry_dept/retail/customer_service"]
    );
    f.answers("Where is my order?").await;
}

/// R15: rolling back to a pre-KH release restores its selection rewritten into the hierarchy shape, and the job stays
/// live on it.
#[tokio::test]
async fn rolling_back_to_a_pre_kh_release_keeps_the_job_live() {
    use crate::common::design_contract;
    use agent_api::design::DraftRef;
    let f = fx("kh-t6-r15-rollback", true, true).await;
    let old = current_release(&f.e, f.job).await;
    // what the old release cut, in the hierarchy shape, before it is rewritten as a pre-KH snapshot
    let cut: Value = sqlx::query_scalar(
        "SELECT selection FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(f.job)
    .bind(&old)
    .fetch_one(&f.e.owner)
    .await
    .unwrap();
    make_pre_kh(&f.e, f.job, &old).await;
    // a later release (another area set), applied
    let c = design_contract::contract_job(f.t, f.job, "faq_agent");
    let mut next = design_contract::cuttable_selection();
    // a retail selection needs at least one area: add one rather than empty them
    next["areas"] = json!(["order-support", "faq"]);
    design_contract::write(&f.e, &c, "selection", &DraftRef::new(), Some(next)).await;
    let tok = design_contract::release_manager(&f.e, &c).await;
    let newer = design_contract::cut(&f.e, &c, &tok, true).await;
    assert_ne!(newer, old);
    // roll back to the pre-KH release
    let (s, b) = send(
        &f.e.app,
        req(
            "POST",
            &format!("/api/jobs/faq_agent/releases/{old}/apply"),
            Some(&tok),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["applied"], true, "{b}");
    assert_eq!(current_release(&f.e, f.job).await, old);
    let stored: Value = sqlx::query_scalar("SELECT selection FROM job_target WHERE job_id = $1")
        .bind(f.job)
        .fetch_one(&f.e.owner)
        .await
        .unwrap();
    assert_eq!(
        stored, cut,
        "the restored draft selection is the old cut, in the hierarchy shape"
    );
    assert!(f.live_template_errors().await.is_empty());
    f.answers("Back on the old release?").await;
}

#[tokio::test]
async fn a_streamed_customer_turn_opens_before_the_reply() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let f = fx("kh-t6-stream", true, true).await;
    // the turn cannot store its conversation while the job row is held (the conversation's foreign key)
    let mut hold = f.e.owner.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(f.job)
        .execute(&mut *hold)
        .await
        .unwrap();
    let r = Request::post("/api/jobs/faq_agent/interactions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", f.secret))
        .body(Body::from(
            json!({"input": "hi", "stream": true}).to_string(),
        ))
        .unwrap();
    let resp = tokio::time::timeout(Duration::from_secs(10), f.e.app.clone().oneshot(r))
        .await
        .expect("the stream opens before the turn ran")
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body();
    let first = tokio::time::timeout(Duration::from_secs(10), body.frame())
        .await
        .expect("the first event arrives before the reply")
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
    let first = String::from_utf8(first.to_vec()).unwrap();
    assert!(first.contains("interaction.created"), "{first}");
    assert!(
        f.conversations().await.is_empty(),
        "the turn has not run yet"
    );
    hold.rollback().await.unwrap();
    let rest = tokio::time::timeout(Duration::from_secs(60), body.collect())
        .await
        .expect("the reply arrives")
        .unwrap()
        .to_bytes();
    let rest = String::from_utf8(rest.to_vec()).unwrap();
    assert!(rest.contains("interaction.completed"), "{rest}");
    assert!(rest.contains(TURN_FAILED), "{rest}");
}

/// B7 R6 back to back (D109 review): turn N−1 is still running (its drain holds the lock) when the edge accepts N and
/// N+1, and N−1's reply lands after both were stored. The answer calls queue N and N+1 behind the running turn —
/// neither is dropped as a duplicate, neither takes N−1's late reply — and the drain takes both in its next pass:
/// that one turn's reply is delivered once, on the later message's call.
#[tokio::test]
async fn back_to_back_accepts_behind_a_running_turn_are_both_answered() {
    let c = conv::setup_runtime(redis_env::api_vars("kh-t6-back-to-back").await).await;
    let tg = telegram(&c).await;
    let msg = |id: &str, accept: bool| {
        let mut b = json!({
            "kind": "telegram", "external_id": "123456789", "binding_id": tg["id"],
            "tenant": c.t, "job_id": c.job, "job_key": "j1",
            "conversation": {"channel": "telegram", "id": "42", "route": {}},
            "message_id": id, "party": {"id": "42", "name": "Ada", "verified": true},
            "text": format!("question {id}"),
        });
        if accept {
            b["accept_only"] = json!(true);
        }
        b
    };
    const INBOUND: &str = "/api/internal/channel/inbound";
    let (s, b) = c.internal(INBOUND, msg("9001", false)).await;
    assert_eq!(
        (s, b["status"].as_str()),
        (StatusCode::OK, Some("answered")),
        "{b}"
    );
    let conv_id = b["conversation_id"].as_str().unwrap().to_string();
    // turn N−1 is running: its drain holds the conversation lock
    let root = c.e.state.redis.clone().unwrap();
    let bus = root.scoped(&agent_api::runtime::Scope::new(c.t, "j1", Env::Prod).unwrap());
    // (N−1's own drain lets go of the lock right after storing that reply)
    let mut held = false;
    for _ in 0..50 {
        if agent_api::turn::acquire(&bus, &conv_id, "turn-n-1")
            .await
            .unwrap()
        {
            held = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(held, "the lock of turn N-1");
    for id in ["9002", "9003"] {
        let (s, b) = c.internal(INBOUND, msg(id, true)).await;
        assert_eq!(
            (s, b["status"].as_str()),
            (StatusCode::OK, Some("accepted")),
            "{id}: {b}"
        );
    }
    // N−1's reply lands after N and N+1 were stored
    {
        use agent_api::conversations::store;
        let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
        store::append(
            &mut tx,
            store::NewMessage {
                job_id: c.job,
                conversation_id: &conv_id,
                role: "agent",
                author: "agent",
                text: "the late reply of turn N-1",
                case_id: None,
                external_id: None,
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    let drain = async {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let ctx = agent_api::turn::drain::TurnCtx {
            tenant: c.t,
            job_id: c.job,
            job_key: "j1".into(),
            env: Env::Prod,
            conversation_id: conv_id.clone(),
        };
        // turn N−1's drain finishes its pass and finds N and N+1 on the inbox
        agent_api::turn::drain::run(c.e.state.clone(), ctx, "turn-n-1".into()).await;
    };
    let (n, n1, ()) = tokio::join!(
        c.internal(INBOUND, msg("9002", false)),
        c.internal(INBOUND, msg("9003", false)),
        drain
    );
    assert_eq!(n.0, StatusCode::OK, "{}", n.1);
    assert_eq!(n1.0, StatusCode::OK, "{}", n1.1);
    assert_eq!(
        n.1["status"], "duplicate",
        "the batch's reply goes out once: {}",
        n.1
    );
    assert_eq!(n1.1["status"], "answered", "{}", n1.1);
    assert_eq!(n1.1["text"], TURN_FAILED, "not N−1's late reply: {}", n1.1);
    let inbox: i64 = redis::cmd("LLEN")
        .arg(agent_api::turn::chat_key(&bus, &conv_id, ":inbox"))
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert_eq!(inbox, 0, "nothing stranded");
    let agent_lines = c
        .texts(
            "SELECT text FROM conversation_message WHERE role = 'agent' ORDER BY at_ms, message_id",
        )
        .await;
    assert_eq!(
        agent_lines,
        [TURN_FAILED, "the late reply of turn N-1", TURN_FAILED],
        "one reply per turn"
    );
    // a provider retry after that is a duplicate, for both
    for id in ["9002", "9003"] {
        let (_, d) = c.internal(INBOUND, msg(id, false)).await;
        assert_eq!(d["status"], "duplicate", "{id}: {d}");
    }
    assert_eq!(c.backend.calls(), 0);
}

/// Two answer calls for one R6-stored message at once (a provider retry racing the edge): the stored → queued step is
/// one check-and-set, so the message is queued once, one turn runs, and its reply goes out once.
#[tokio::test]
async fn concurrent_answer_calls_for_one_stored_message_run_one_turn() {
    let c = conv::setup_runtime(redis_env::api_vars("kh-t6-concurrent").await).await;
    let tg = telegram(&c).await;
    let msg = |accept: bool| {
        let mut b = json!({
            "kind": "telegram", "external_id": "123456789", "binding_id": tg["id"],
            "tenant": c.t, "job_id": c.job, "job_key": "j1",
            "conversation": {"channel": "telegram", "id": "42", "route": {}},
            "message_id": "9101", "party": {"id": "42", "name": "Ada", "verified": true},
            "text": "Where is my parcel?",
        });
        if accept {
            b["accept_only"] = json!(true);
        }
        b
    };
    const INBOUND: &str = "/api/internal/channel/inbound";
    let (s, a) = c.internal(INBOUND, msg(true)).await;
    assert_eq!(
        (s, a["status"].as_str()),
        (StatusCode::OK, Some("accepted")),
        "{a}"
    );
    let (x, y) = tokio::join!(
        c.internal(INBOUND, msg(false)),
        c.internal(INBOUND, msg(false))
    );
    assert_eq!(x.0, StatusCode::OK, "{}", x.1);
    assert_eq!(y.0, StatusCode::OK, "{}", y.1);
    let mut statuses = [
        x.1["status"].as_str().unwrap_or_default().to_string(),
        y.1["status"].as_str().unwrap_or_default().to_string(),
    ];
    statuses.sort();
    assert_eq!(statuses, ["answered", "duplicate"], "{} / {}", x.1, y.1);
    assert_eq!(
        c.texts("SELECT text FROM conversation_message WHERE role = 'agent'")
            .await,
        [TURN_FAILED],
        "one turn ran"
    );
    assert_eq!(c.backend.calls(), 0);
}

/// The race behind [`concurrent_answer_calls_for_one_stored_message_run_one_turn`], made deterministic: an edge
/// answer call that takes an armed message holds it (a claim token of its own), so a second call for the same message
/// while the first is still answering is told it is held — never a second turn or a second delivery of its reply.
#[tokio::test]
async fn an_edge_call_that_takes_an_armed_message_holds_it_against_a_resend() {
    let c = conv::setup_runtime(redis_env::api_vars("kh-t6-take-holds").await).await;
    let tg = telegram(&c).await;
    let (s, a) = c
        .internal(
            "/api/internal/channel/inbound",
            json!({
                "kind": "telegram", "external_id": "123456789", "binding_id": tg["id"],
                "tenant": c.t, "job_id": c.job, "job_key": "j1",
                "conversation": {"channel": "telegram", "id": "43", "route": {}},
                "message_id": "9102", "party": {"id": "43", "name": "Ada", "verified": true},
                "text": "Where is my parcel?", "accept_only": true,
            }),
        )
        .await;
    assert_eq!(
        (s, a["status"].as_str()),
        (StatusCode::OK, Some("accepted")),
        "{a}"
    );
    let conv_id = a["conversation_id"].as_str().unwrap().to_string();
    let take = agent_api::conversations::answer_sweeper::take;
    let mut first = c.e.owner.begin().await.unwrap();
    assert!(
        !take(&mut first, &conv_id, "9102").await.unwrap(),
        "the first call takes it"
    );
    first.commit().await.unwrap();
    let mut second = c.e.owner.begin().await.unwrap();
    assert!(
        take(&mut second, &conv_id, "9102").await.unwrap(),
        "a resend finds it held"
    );
    second.commit().await.unwrap();
}

// ---- guard: no customer path constructs or calls the passthrough ---------------------------------------------------

/// Every `.rs` file under `dir`.
fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if p.is_dir() {
            sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The source of `p` without its `#[cfg(test)] mod tests` tail (unit tests may build a passthrough of their own).
fn production_source(p: &Path) -> String {
    let s = std::fs::read_to_string(p).unwrap();
    match s.find("#[cfg(test)]\nmod tests") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

#[test]
fn no_customer_path_constructs_or_calls_the_passthrough() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);
    assert!(files.len() > 50, "the crate's sources were found");
    let rel = |p: &Path| {
        p.strip_prefix(&src)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    };
    let mut built = Vec::new();
    let mut direct = Vec::new();
    for f in &files {
        let s = production_source(f);
        let r = rel(f);
        if s.contains("LlmPassthrough {") && !s.contains("pub struct LlmPassthrough {")
            || s.contains("LlmPassthrough::from_config(")
            || s.contains("LlmPassthrough::new(")
        {
            built.push(r.clone());
        }
        for call in [".interaction.respond(", ".interaction.answer("] {
            if s.contains(call) {
                direct.push(format!("{r}: {call}"));
            }
        }
    }
    built.sort();
    // the staff playground's backend (`from_config`, `client_for`) is the only place a passthrough is built
    assert_eq!(built, ["interaction/mod.rs"], "{built:?}");
    // and the one dispatch (`interaction::respond`) is the only caller of `AppState.interaction`
    assert_eq!(
        direct,
        ["interaction/mod.rs: .interaction.respond("],
        "{direct:?}"
    );
    // every customer entry point goes through that dispatch
    for (file, call) in [
        ("interaction/routes.rs", "super::respond("),
        ("interaction/webhook.rs", "super::respond("),
        ("interaction/mcp.rs", "super::respond("),
        ("voice/routes.rs", "crate::interaction::respond("),
    ] {
        let s = production_source(&src.join(file));
        assert!(s.contains(call), "{file} answers through the dispatch");
    }
    let mod_rs = production_source(&src.join("interaction/mod.rs"));
    assert!(
        mod_rs.contains("crate::runtime::backend::runtime_stream("),
        "customers go to the runtime"
    );
    let edge = production_source(&src.join("channels/config.rs"));
    assert!(
        !edge.contains("LlmPassthrough"),
        "agent-edge has no model of its own"
    );
}
