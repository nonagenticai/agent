//! B3 T4d (D101/D102): a voice turn's speech and an interaction's model resolve per JOB from the tenant's own
//! providers — the uses `asr` / `tts` (crate::voice::ProviderSpeech) and `qualify` (crate::interaction::ProviderBackend)
//! read from the job's LIVE design (a binding is a draft until that job publishes it) — while the platform gateways
//! (`VOICE_PLATFORM_TENANTS`, `INTERACTION_LLM_TENANTS`) are tried first for the owner test tenants only. A fake
//! gateway on 127.0.0.1 (w2v + OpenAI-compatible audio + chat completions) records every call; the API is rebuilt
//! with the test-only loopback netguard policy. Real Postgres 18 (one container for this binary, a fresh database per
//! test).
use crate::common;

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use crate::common::design_contract::{
    self, ContractJob, contract_job, cuttable_selection, new_job,
};
use crate::common::{TestEnv, get, insert_tenant, insert_user, json, person, send};
use agent_api::connectors::transport::Transport;
use agent_api::design::DraftRef;
use agent_api::interaction::{
    BackendError, BackendEvent, InteractionBackend, InteractionRequest, Message, ProviderBackend,
    ProviderDeps, Role, Scope, Usage, collect,
};
use agent_api::netguard::Policy;
use agent_api::state::AppState;
use agent_api::voice::provider_clients;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use serde_json::{Value, json};
use uuid::Uuid;

const TRANSCRIPT: &str = "where is my order";
const FAKE_WAV: &[u8] = b"RIFF\x24\x00\x00\x00WAVEfmt fake";
const SSE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
    data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n\
    data: [DONE]\n\n";

fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
    vars.insert(
        "PROVIDER_SECRET_KEYS".to_string(),
        format!(r#"{{"p1":"{key}"}}"#),
    );
    vars.insert("PROVIDER_SECRET_KEY_CURRENT".to_string(), "p1".to_string());
}

// ---- the fake gateway -----------------------------------------------------------------------------------------------

/// One call the fake saw.
#[derive(Clone, Debug)]
struct Hit {
    path: String,
    auth: Option<String>,
    /// `X-Effective-Tenant-Id` (agent#71): the platform model only, never a tenant's own provider.
    effective_tenant: Option<String>,
    body: Vec<u8>,
}

#[derive(Clone, Default)]
struct Hits(Arc<Mutex<Vec<Hit>>>);

impl Hits {
    fn all(&self) -> Vec<Hit> {
        self.0.lock().unwrap().clone()
    }
    fn at(&self, suffix: &str) -> Vec<Hit> {
        self.all()
            .into_iter()
            .filter(|h| h.path.ends_with(suffix))
            .collect()
    }
}

/// Answers by path suffix, under any prefix: ASR (`/api/transcribe`, `/audio/transcriptions`) → `{text}`, TTS
/// (`/api/tts/synthesize`, `/audio/speech`) → WAV bytes (a path under `/broken` → 500 echoing the bearer), chat
/// completions → a "Hello" stream.
async fn gateway(State(h): State<Hits>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let path = uri.path().to_string();
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let effective_tenant = headers
        .get("x-effective-tenant-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    h.0.lock().unwrap().push(Hit {
        path: path.clone(),
        auth: auth.clone(),
        effective_tenant,
        body: body.to_vec(),
    });
    if path.ends_with("/api/transcribe") || path.ends_with("/audio/transcriptions") {
        return axum::Json(json!({ "text": TRANSCRIPT })).into_response();
    }
    if path.ends_with("/api/tts/synthesize") || path.ends_with("/audio/speech") {
        if path.contains("/broken") {
            let echo = format!("bad request from {}", auth.unwrap_or_default());
            return (StatusCode::INTERNAL_SERVER_ERROR, echo).into_response();
        }
        return ([("content-type", "audio/wav")], FAKE_WAV.to_vec()).into_response();
    }
    if path.ends_with("/chat/completions") {
        return ([("content-type", "text/event-stream")], SSE).into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

struct Fake {
    /// `http://127.0.0.1:<port>`.
    base: String,
    hits: Hits,
}

async fn fake() -> Fake {
    let hits = Hits::default();
    let app = Router::new().fallback(gateway).with_state(hits.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Fake {
        base: format!("http://{addr}"),
        hits,
    }
}

/// "Hel", "lo", done: the interaction backend of a voice turn when the test is about speech.
struct Hello;

impl InteractionBackend for Hello {
    fn respond(
        &self,
        _req: InteractionRequest,
    ) -> BoxStream<'static, Result<BackendEvent, BackendError>> {
        stream::iter(vec![
            Ok(BackendEvent::Delta("Hel".into())),
            Ok(BackendEvent::Delta("lo".into())),
            Ok(BackendEvent::Done {
                usage: Some(Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                }),
            }),
        ])
        .boxed()
    }
}

// ---- the world ------------------------------------------------------------------------------------------------------

fn loopback() -> Policy {
    Policy {
        allow_loopback: true,
        private_targets: vec![],
    }
}

/// `st` with the loopback netguard policy (the fake is on 127.0.0.1) and the API over it.
fn loopback_state(st: &AppState) -> AppState {
    let mut st = st.clone();
    st.connectors = Transport::new(loopback());
    st
}

/// A member of `tenant` holding `role` (`role_keys` syntax); its token.
async fn member(e: &TestEnv, tenant: Uuid, role: &str) -> String {
    let sub = format!("u-{}-{}", role.replace(',', "-"), Uuid::new_v4().simple());
    insert_user(&e.owner, &sub, tenant, role).await;
    person(&sub, &[])
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

async fn call(
    app: &Router,
    tok: &str,
    method: &str,
    uri: &str,
    body: Value,
) -> (StatusCode, Value) {
    send(app, json(method, uri, Some(tok), body)).await
}

/// A provider (as an integrations manager); its id.
async fn create(
    app: &Router,
    im: &str,
    name: &str,
    kind: &str,
    base_url: &str,
    models: &[&str],
    key: &str,
) -> Uuid {
    let body =
        json!({"name": name, "kind": kind, "base_url": base_url, "models": models, "secret": key});
    let (s, b) = call(app, im, "POST", "/api/providers", body).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    b["id"].as_str().unwrap().parse().unwrap()
}

async fn bind(app: &Router, im: &str, use_name: &str, body: Value) {
    let (s, b) = call(
        app,
        im,
        "PUT",
        &format!("/api/providers/uses/{use_name}"),
        body,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{use_name}: {b}");
}

/// Approve and apply the job's draft (`POST …/releases {apply: true}`) as a release manager.
async fn publish(app: &Router, job_key: &str, tok: &str) {
    let (s, b) = send(app, get(&format!("/api/jobs/{job_key}/target"), Some(tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let body = json!({"apply": true, "selection_seen_at": b["selection_updated_at"].clone()});
    let (s, b) = call(
        app,
        tok,
        "POST",
        &format!("/api/jobs/{job_key}/releases"),
        body,
    )
    .await;
    assert!(
        s == StatusCode::CREATED || s == StatusCode::OK,
        "approve: {s}: {b}"
    );
    assert_eq!(b["applied"], true, "{b}");
}

/// The job gets a complete selection (so it cuts) and a release manager; its token.
async fn cuttable(e: &TestEnv, f: &ContractJob) -> String {
    design_contract::write(
        e,
        f,
        "selection",
        &DraftRef::new(),
        Some(cuttable_selection()),
    )
    .await;
    design_contract::publishable(e, f).await;
    member(e, f.tenant, "tenant_admin").await
}

/// A second job `key` of `tenant` through the platform route (its creation hook stores a baseline); its scope.
async fn second_job(e: &TestEnv, tenant: Uuid, key: &str) -> Scope {
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/tenants/{tenant}/jobs"),
            Some(&common::super_admin()),
            json!({"template": "job/customer_care_agent@v4", "key": key, "label": "Second desk"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let id: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
        .bind(tenant)
        .bind(key)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    Scope {
        tenant,
        job_id: id,
        job_key: key.to_string(),
    }
}

/// A job inserted directly (no baseline yet, never released: a tenant-wide draft write — `track_tenant` — stores its
/// baseline first, so a binding is still not live until it publishes); its scope.
async fn raw_job(e: &TestEnv, tenant: Uuid, key: &str) -> Scope {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, $2, 'J', '[]', 'test') RETURNING id",
    )
    .bind(tenant)
    .bind(key)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    Scope {
        tenant,
        job_id: id,
        job_key: key.to_string(),
    }
}

fn scope_of(f: &ContractJob) -> Scope {
    Scope {
        tenant: f.tenant,
        job_id: f.job.job_id,
        job_key: f.job.job_key.clone(),
    }
}

const BOUNDARY: &str = "t4d-boundary-91c2";

/// A voice turn on `job_key` with a small recording.
fn turn(job_key: &str, tok: &str) -> Request<Body> {
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"audio\"; filename=\"rec\"\r\n\
             Content-Type: audio/webm\r\n\r\nwebm-bytes\r\n--{BOUNDARY}--\r\n"
        )
        .as_bytes(),
    );
    Request::post(format!("/api/jobs/{job_key}/voice/turns"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header("authorization", format!("Bearer {tok}"))
        .body(Body::from(body))
        .unwrap()
}

// ---- speech ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn asr_tts_bindings_resolve_per_job() {
    let fake = fake().await;
    let e = common::env_with(with_keys).await;
    let f = new_job(&e).await;
    let other = second_job(&e, f.tenant, "second_desk").await;
    let mut st = loopback_state(&e.state);
    st.interaction = Arc::new(Hello);
    let app = agent_api::router(st.clone());
    let im = member(&e, f.tenant, "integrations_manager").await;
    let user = member(&e, f.tenant, "tenant_user").await;
    let (asr_key, tts_key) = (
        format!("sk-asr-{}", Uuid::new_v4().simple()),
        format!("sk-tts-{}", Uuid::new_v4().simple()),
    );
    let asr = create(
        &app,
        &im,
        "Ears",
        "w2v",
        &format!("{}/w2v", fake.base),
        &["whisper"],
        &asr_key,
    )
    .await;
    let tts = create(
        &app,
        &im,
        "Mouth",
        "openai",
        &format!("{}/oa/v1", fake.base),
        &["tts-1"],
        &tts_key,
    )
    .await;
    bind(
        &app,
        &im,
        "asr",
        json!({"provider_id": asr, "model": "whisper"}),
    )
    .await;
    bind(
        &app,
        &im,
        "tts",
        json!({"provider_id": tts, "model": "tts-1", "voice": "nova"}),
    )
    .await;
    let mine = scope_of(&f);

    // D69: bound in the draft only, so no job is served yet
    assert!(provider_clients(&st, &mine).await.unwrap().is_none());
    let (s, b) = send(&app, turn(&mine.job_key, &user)).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "provider_not_configured"),
        "{b}"
    );
    assert!(fake.hits.all().is_empty(), "no gateway call before publish");

    // this job publishes: its live design binds both; the other job's live design still binds neither
    let rm = cuttable(&e, &f).await;
    publish(&app, &mine.job_key, &rm).await;
    let (a, t) = provider_clients(&st, &mine)
        .await
        .unwrap()
        .expect("the published job resolves both");
    assert_eq!(
        (a.kind, a.asr_url.as_str(), a.token.as_str()),
        (
            agent_api::config::SpeechProviderKind::W2v,
            format!("{}/w2v", fake.base).as_str(),
            asr_key.as_str()
        )
    );
    assert_eq!(
        (
            t.kind,
            t.tts_url.as_str(),
            t.tts_model.as_str(),
            t.tts_voice.as_str()
        ),
        (
            agent_api::config::SpeechProviderKind::OpenaiCompatible,
            format!("{}/oa/v1", fake.base).as_str(),
            "tts-1",
            "nova"
        )
    );
    assert!(provider_clients(&st, &other).await.unwrap().is_none());

    let (s, b) = send(&app, turn(&mine.job_key, &user)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["transcript"], TRANSCRIPT);
    assert_eq!(b["reply_text"], "Hello");
    let wav = base64::engine::general_purpose::STANDARD
        .decode(b["audio_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(wav, FAKE_WAV);
    let heard = fake.hits.at("/w2v/api/transcribe");
    assert_eq!(heard.len(), 1, "{:?}", fake.hits.all());
    assert_eq!(heard[0].auth, Some(format!("Bearer {asr_key}")));
    let spoke = fake.hits.at("/oa/v1/audio/speech");
    assert_eq!(spoke.len(), 1, "{:?}", fake.hits.all());
    assert_eq!(spoke[0].auth, Some(format!("Bearer {tts_key}")));
    let sent: Value = serde_json::from_slice(&spoke[0].body).unwrap();
    assert_eq!(
        (&sent["model"], &sent["voice"], &sent["input"]),
        (&json!("tts-1"), &json!("nova"), &json!("Hello"))
    );

    // the other job of the same tenant is not served
    let (s, b) = send(&app, turn(&other.job_key, &user)).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "provider_not_configured"),
        "{b}"
    );
}

#[tokio::test]
async fn unbound_tenant_voice_409_provider_not_configured() {
    let fake = fake().await;
    let e = common::env_with(with_keys).await;
    let f = new_job(&e).await;
    let mut st = loopback_state(&e.state);
    st.interaction = Arc::new(Hello);
    let app = agent_api::router(st.clone());
    let im = member(&e, f.tenant, "integrations_manager").await;
    let user = member(&e, f.tenant, "tenant_user").await;
    let mine = scope_of(&f);

    // nothing bound
    let (s, b) = send(&app, turn(&mine.job_key, &user)).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "provider_not_configured"),
        "{b}"
    );

    // asr bound and published, tts not: still not configured, and no gateway is called
    let p = create(
        &app,
        &im,
        "Ears only",
        "w2v",
        &format!("{}/w2v", fake.base),
        &["whisper"],
        "sk-ears-only-1",
    )
    .await;
    bind(
        &app,
        &im,
        "asr",
        json!({"provider_id": p, "model": "whisper"}),
    )
    .await;
    let rm = cuttable(&e, &f).await;
    publish(&app, &mine.job_key, &rm).await;
    assert!(provider_clients(&st, &mine).await.unwrap().is_none());
    let (s, b) = send(&app, turn(&mine.job_key, &user)).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "provider_not_configured"),
        "{b}"
    );
    assert!(fake.hits.all().is_empty(), "{:?}", fake.hits.all());
}

#[tokio::test]
async fn platform_tenant_keeps_platform_speech() {
    let fake = fake().await;
    let listed = Uuid::new_v4();
    let base = fake.base.clone();
    let e = common::env_with(|v| {
        with_keys(v);
        for (k, val) in [
            ("VOICE_ASR_URL", format!("{base}/plat")),
            ("VOICE_TTS_URL", format!("{base}/plat")),
            ("VOICE_TOKEN", "platform-voice-token".to_string()),
            ("VOICE_PLATFORM_TENANTS", listed.to_string()),
            ("INTERACTION_LLM_URL", format!("{base}/platllm")),
            ("INTERACTION_LLM_TOKEN", "platform-llm-token".to_string()),
            ("INTERACTION_LLM_TENANTS", listed.to_string()),
        ] {
            v.insert(k.to_string(), val);
        }
    })
    .await;
    insert_tenant(&e.owner, listed, &format!("Listed {listed}")).await;
    let job = second_job(&e, listed, "plat_job").await;
    // the boot backends (ProviderSpeech, ProviderBackend), with the loopback policy for the tenant's own providers
    let st = loopback_state(&e.state);
    let app = agent_api::router(st.clone());
    let im = member(&e, listed, "integrations_manager").await;
    let user = member(&e, listed, "tenant_user").await;
    // the listed tenant also binds gateways of its own, and publishes them (D69: a binding is a draft until then;
    // even a never-released job's live design is the baseline `track_tenant` stores before the first write)
    let own = create(
        &app,
        &im,
        "Own speech",
        "w2v",
        &format!("{}/own", fake.base),
        &["whisper"],
        "sk-own-speech-1",
    )
    .await;
    bind(
        &app,
        &im,
        "asr",
        json!({"provider_id": own, "model": "whisper"}),
    )
    .await;
    bind(
        &app,
        &im,
        "tts",
        json!({"provider_id": own, "model": "whisper"}),
    )
    .await;
    let rm = cuttable(&e, &contract_job(listed, job.job_id, &job.job_key)).await;
    publish(&app, &job.job_key, &rm).await;
    assert!(
        provider_clients(&st, &job).await.unwrap().is_some(),
        "its own providers resolve"
    );

    // ...but the platform gateways are tried first (D102)
    let (s, b) = send(&app, turn(&job.job_key, &user)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["reply_text"], "Hello");
    let heard = fake.hits.at("/plat/api/transcribe");
    assert_eq!(heard.len(), 1, "{:?}", fake.hits.all());
    assert_eq!(
        heard[0].auth.as_deref(),
        Some("Bearer platform-voice-token")
    );
    assert_eq!(fake.hits.at("/plat/api/tts/synthesize").len(), 1);
    let chat = fake.hits.at("/platllm/chat/completions");
    assert_eq!(chat.len(), 1, "{:?}", fake.hits.all());
    assert_eq!(chat[0].auth.as_deref(), Some("Bearer platform-llm-token"));
    // the platform passthrough (`LlmPassthrough::from_config`: `effective_tenant: true`) bills the listed tenant
    assert_eq!(chat[0].effective_tenant, Some(listed.to_string()));
    assert_ne!(listed, Uuid::nil());
    assert!(
        fake.hits.all().iter().all(|h| !h.path.starts_with("/own")),
        "{:?}",
        fake.hits.all()
    );

    // a tenant that is not listed gets neither platform gateway
    let stranger = Uuid::new_v4();
    insert_tenant(&e.owner, stranger, &format!("Stranger {stranger}")).await;
    let sjob = raw_job(&e, stranger, "stranger_job").await;
    let suser = member(&e, stranger, "tenant_user").await;
    let before = fake.hits.all().len();
    let (s, b) = send(&app, turn(&sjob.job_key, &suser)).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "provider_not_configured"),
        "{b}"
    );
    let r = collect(st.interaction.respond(InteractionRequest::new(
        &sjob,
        vec![Message::text(Role::User, "hi")],
    )))
    .await;
    assert!(matches!(r, Err(BackendError::NotConfigured(_))), "{r:?}");
    assert_eq!(
        fake.hits.all().len(),
        before,
        "no platform call for a stranger"
    );
}

// ---- the interaction backend ----------------------------------------------------------------------------------------

#[tokio::test]
async fn interaction_backend_resolves_qualify_per_job() {
    let fake = fake().await;
    let e = common::env_with(with_keys).await;
    let f = new_job(&e).await;
    let other = second_job(&e, f.tenant, "other_desk").await;
    let st = loopback_state(&e.state);
    let app = agent_api::router(st.clone());
    let im = member(&e, f.tenant, "integrations_manager").await;
    let key = format!("sk-llm-{}", Uuid::new_v4().simple());
    let p = create(
        &app,
        &im,
        "Brain",
        "openai",
        &format!("{}/llm/v1", fake.base),
        &["gpt-x"],
        &key,
    )
    .await;
    bind(
        &app,
        &im,
        "qualify",
        json!({"provider_id": p, "model": "gpt-x"}),
    )
    .await;
    let backend = ProviderBackend::new(
        &e.state.cfg,
        ProviderDeps {
            db: e.state.db.clone(),
            design: e.state.design.clone(),
            keyring: e.state.provider_keyring.clone(),
            policy: loopback(),
        },
        None,
    );
    let mine = scope_of(&f);
    let ask = |s: &Scope| InteractionRequest::new(s, vec![Message::text(Role::User, "hi")]);

    // a draft binding serves nobody
    let r = collect(backend.respond(ask(&mine))).await;
    assert!(matches!(r, Err(BackendError::NotConfigured(_))), "{r:?}");
    assert!(fake.hits.all().is_empty());

    let rm = cuttable(&e, &f).await;
    publish(&app, &mine.job_key, &rm).await;
    let r = collect(backend.respond(ask(&mine))).await.unwrap();
    assert_eq!(r.text, "Hello");
    let chat = fake.hits.at("/llm/v1/chat/completions");
    assert_eq!(chat.len(), 1, "{:?}", fake.hits.all());
    assert_eq!(chat[0].auth, Some(format!("Bearer {key}")));
    // `client_for` on a tenant provider (`provider_id: Some`): our tenant id never leaves for a tenant's own host
    assert_eq!(chat[0].effective_tenant, None, "{:?}", chat[0]);
    let body: Value = serde_json::from_slice(&chat[0].body).unwrap();
    assert_eq!(body["model"], "gpt-x");
    assert_eq!(body["stream"], true);

    // the other job has not published the binding
    let r = collect(backend.respond(ask(&other))).await;
    assert!(matches!(r, Err(BackendError::NotConfigured(_))), "{r:?}");
    assert_eq!(fake.hits.all().len(), 1);
}

/// `client_for`'s internal arm (agent#71): a platform-owned tenant's `qualify` resolves to the internal model
/// (`INTERNAL_LLM_URL`, D70) with `effective_tenant: Some(tenant)`, so the call names that tenant. Guards
/// interaction/mod.rs `client_for` (`let effective_tenant = r.provider_id.is_none() && r.effective_tenant.is_some()`)
/// and interaction/llm.rs `respond` (`if self.effective_tenant { call = call.header(EFFECTIVE_TENANT_HEADER, ..) }`).
#[tokio::test]
async fn interaction_backend_names_the_tenant_to_the_internal_model() {
    let fake = fake().await;
    let base = fake.base.clone();
    let e = common::env_with(|v| {
        with_keys(v);
        v.insert("INTERNAL_LLM_URL".to_string(), format!("{base}/int/v1"));
    })
    .await;
    let f = new_job(&e).await;
    sqlx::query("UPDATE tenants SET platform_owned = true WHERE id = $1")
        .bind(f.tenant)
        .execute(&e.owner)
        .await
        .unwrap();
    let backend = ProviderBackend::new(
        &e.state.cfg,
        ProviderDeps {
            db: e.state.db.clone(),
            design: e.state.design.clone(),
            keyring: e.state.provider_keyring.clone(),
            policy: loopback(),
        },
        None,
    );
    let r = collect(backend.respond(InteractionRequest::new(
        &scope_of(&f),
        vec![Message::text(Role::User, "hi")],
    )))
    .await
    .unwrap();
    assert_eq!(r.text, "Hello");
    let chat = fake.hits.at("/int/v1/chat/completions");
    assert_eq!(chat.len(), 1, "{:?}", fake.hits.all());
    assert_ne!(f.tenant, Uuid::nil());
    assert_eq!(chat[0].effective_tenant, Some(f.tenant.to_string()));
}

// ---- secrets --------------------------------------------------------------------------------------------------------

/// A writer the log capture appends to.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// Every `agent_api` log line of this binary, at every level (one global subscriber, installed once; see
/// tests/observability.rs for why it is global).
fn capture() -> Captured {
    static CAPTURE: OnceLock<Captured> = OnceLock::new();
    CAPTURE
        .get_or_init(|| {
            let buf = Captured::default();
            let w = buf.clone();
            let sub = tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::new("agent_api=trace"))
                .with_ansi(false)
                .with_writer(move || w.clone())
                .finish();
            tracing::subscriber::set_global_default(sub)
                .expect("the only global subscriber of this binary");
            buf
        })
        .clone()
}

#[tokio::test]
async fn speech_key_never_in_logs() {
    let logs = capture();
    let fake = fake().await;
    let e = common::env_with(with_keys).await;
    let f = new_job(&e).await;
    let mut st = loopback_state(&e.state);
    st.interaction = Arc::new(Hello);
    let app = agent_api::router(st.clone());
    let im = member(&e, f.tenant, "integrations_manager").await;
    let user = member(&e, f.tenant, "tenant_user").await;
    let key = format!("sk-voice-secret-{}", Uuid::new_v4().simple());
    // TTS under /broken answers 500 echoing the bearer
    let p = create(
        &app,
        &im,
        "Leaky",
        "w2v",
        &format!("{}/broken", fake.base),
        &["kokoro"],
        &key,
    )
    .await;
    bind(
        &app,
        &im,
        "asr",
        json!({"provider_id": p, "model": "kokoro"}),
    )
    .await;
    bind(
        &app,
        &im,
        "tts",
        json!({"provider_id": p, "model": "kokoro", "voice": "af_heart"}),
    )
    .await;
    let rm = cuttable(&e, &f).await;
    publish(&app, &f.job.job_key, &rm).await;

    let (s, b) = send(&app, turn(&f.job.job_key, &user)).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::BAD_GATEWAY, "voice_upstream_error"),
        "{b}"
    );
    assert!(!b.to_string().contains(&key), "{b}");
    // the key did go out, to the gateway only
    let spoke = fake.hits.at("/broken/api/tts/synthesize");
    assert_eq!(spoke.len(), 1, "{:?}", fake.hits.all());
    assert_eq!(spoke[0].auth, Some(format!("Bearer {key}")));

    // Debug of the clients, the provider view, audit, ledger and snapshots never carry it
    let (a, t) = provider_clients(&st, &scope_of(&f)).await.unwrap().unwrap();
    for d in [format!("{a:?}"), format!("{t:?}")] {
        assert!(!d.contains(&key), "{d}");
        assert!(d.contains("[redacted]"), "{d}");
    }
    let (s, v) = send(&app, get("/api/providers", Some(&user))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(!v.to_string().contains(&key));
    for sql in [
        "SELECT coalesce(string_agg(details::text, ' '), '') FROM audit_event WHERE tenant_id = $1",
        "SELECT coalesce(string_agg(after::text, ' '), '') FROM draft_ledger WHERE tenant_id = $1",
        "SELECT coalesce(string_agg(snap::text, ' '), '') FROM release_snapshot WHERE tenant_id = $1",
    ] {
        let all: String = sqlx::query_scalar(sql)
            .bind(f.tenant)
            .fetch_one(&e.owner)
            .await
            .unwrap();
        assert!(!all.contains(&key), "{sql}");
    }

    // the logs saw the failure (the capture works), never the key
    let text = logs.text();
    assert!(text.contains("voice_tts_status"), "the capture is live");
    assert!(!text.contains(&key), "a speech key reached a log line");
}
