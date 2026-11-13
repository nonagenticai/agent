//! The voice connector (crate::voice): `POST /api/jobs/{job}/voice/turns` over a fake ASR/TTS and a fake interaction
//! backend; the form limits; the unconfigured connector and backend; the job guard. Real Postgres 18 (one container
//! for this binary).
use crate::common;

use std::sync::{Arc, Mutex};

use agent_api::interaction::{
    BackendError, BackendEvent, InteractionBackend, InteractionRequest, Role, Usage,
};
use agent_api::voice::{Clip, PlatformSpeech, SpeechServices, Stage, VoiceError};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use common::{TestEnv, insert_tenant, insert_user, person, send};
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream::{self, BoxStream};
use serde_json::{Value, json};
use uuid::Uuid;

/// "Hel", "lo", done (3 in, 2 out); remembers the last request.
#[derive(Default)]
struct Hello {
    last: Mutex<Option<InteractionRequest>>,
}

impl InteractionBackend for Hello {
    fn respond(
        &self,
        req: InteractionRequest,
    ) -> BoxStream<'static, Result<BackendEvent, BackendError>> {
        *self.last.lock().unwrap() = Some(req);
        stream::iter(vec![
            Ok(BackendEvent::Delta("Hel".into())),
            Ok(BackendEvent::Delta("lo".into())),
            Ok(BackendEvent::Done {
                usage: Some(Usage {
                    input_tokens: 3,
                    output_tokens: 2,
                }),
            }),
        ])
        .boxed()
    }
}

const FAKE_WAV: &[u8] = b"RIFF\x24\x00\x00\x00WAVEfmt fake";

/// Hears `transcript`, speaks `FAKE_WAV`; remembers what it was given.
struct Speech {
    transcript: String,
    clip: Mutex<Option<Clip>>,
    spoken: Mutex<Option<(String, Option<String>)>>,
}

impl Speech {
    fn hearing(transcript: &str) -> Arc<Self> {
        Arc::new(Self {
            transcript: transcript.into(),
            clip: Mutex::new(None),
            spoken: Mutex::new(None),
        })
    }
}

impl SpeechServices for Speech {
    fn transcribe(&self, clip: Clip) -> BoxFuture<'static, Result<String, VoiceError>> {
        *self.clip.lock().unwrap() = Some(clip);
        let t = self.transcript.clone();
        Box::pin(async move { Ok(t) })
    }

    fn synthesize(
        &self,
        text: String,
        voice: Option<String>,
    ) -> BoxFuture<'static, Result<Vec<u8>, VoiceError>> {
        *self.spoken.lock().unwrap() = Some((text, voice));
        Box::pin(async { Ok(FAKE_WAV.to_vec()) })
    }
}

/// The ASR cannot be reached.
struct Down;

impl SpeechServices for Down {
    fn transcribe(&self, _clip: Clip) -> BoxFuture<'static, Result<String, VoiceError>> {
        Box::pin(async { Err(VoiceError::Unavailable(Stage::Asr, "unreachable".into())) })
    }

    fn synthesize(
        &self,
        _text: String,
        _voice: Option<String>,
    ) -> BoxFuture<'static, Result<Vec<u8>, VoiceError>> {
        Box::pin(async { Err(VoiceError::Unavailable(Stage::Tts, "unreachable".into())) })
    }
}

/// [`setup_listed`] with the tenant allow-listed for `speech`.
async fn setup(
    backend: Option<Arc<dyn InteractionBackend>>,
    speech: Option<Arc<dyn SpeechServices>>,
) -> TestEnv {
    setup_listed(backend, speech, true).await
}

/// An env whose interaction backend is `backend` (None = as configured: unconfigured) and whose platform speech
/// gateway is `speech` (None = as configured: off), lent to the tenant when `listed`; a tenant with an observer
/// `u-member` and a job `faq_agent`, and another tenant with a job `other_job`.
async fn setup_listed(
    backend: Option<Arc<dyn InteractionBackend>>,
    speech: Option<Arc<dyn SpeechServices>>,
    listed: bool,
) -> TestEnv {
    let mut e = common::env().await;
    if let Some(b) = backend {
        e.state.interaction = b;
    }
    let t = Uuid::new_v4();
    if speech.is_some() {
        let tenants = if listed { vec![t] } else { Vec::new() };
        e.state.speech = Arc::new(PlatformSpeech::new(speech, tenants));
    }
    e.app = agent_api::router(e.state.clone());
    insert_tenant(&e.owner, t, "Alpha").await;
    insert_user(&e.owner, "u-member", t, "tenant_user").await;
    let other = Uuid::new_v4();
    insert_tenant(&e.owner, other, "Beta").await;
    for (tenant, key) in [(t, "faq_agent"), (other, "other_job")] {
        sqlx::query(
            "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, $2, 'J', '[]', 'test')",
        )
        .bind(tenant)
        .bind(key)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    e
}

const TURNS: &str = "/api/jobs/faq_agent/voice/turns";
const BOUNDARY: &str = "test-boundary-7d1f";

/// One form field: (name, content type of a file field, bytes).
type Field<'a> = (&'a str, Option<&'a str>, Vec<u8>);

fn form(fields: &[Field<'_>]) -> Vec<u8> {
    let mut b = Vec::new();
    for (name, ct, data) in fields {
        b.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match ct {
            Some(ct) => b.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"rec\"\r\nContent-Type: {ct}\r\n\r\n"
                )
                .as_bytes(),
            ),
            None => b.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            ),
        }
        b.extend_from_slice(data);
        b.extend_from_slice(b"\r\n");
    }
    b.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    b
}

fn turn(uri: &str, token: Option<&str>, fields: &[Field<'_>]) -> Request<Body> {
    let mut b = Request::post(uri).header(
        "content-type",
        format!("multipart/form-data; boundary={BOUNDARY}"),
    );
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::from(form(fields))).unwrap()
}

fn member() -> String {
    person("u-member", &[])
}

fn audio() -> Field<'static> {
    (
        "audio",
        Some("audio/webm;codecs=opus"),
        b"webm-bytes".to_vec(),
    )
}

fn code(b: &Value) -> Option<&str> {
    b["error"]["code"].as_str()
}

#[tokio::test]
async fn a_turn_is_transcribed_answered_and_spoken() {
    let hello = Arc::new(Hello::default());
    let speech = Speech::hearing("  where is my order?  ");
    let e = setup(Some(hello.clone()), Some(speech.clone())).await;
    let history =
        json!([{"role": "user", "content": "hi"}, {"role": "assistant", "content": "hello"}]);
    let (s, b) = send(
        &e.app,
        turn(
            TURNS,
            Some(&member()),
            &[
                audio(),
                ("history", None, history.to_string().into_bytes()),
                ("language", None, b"en".to_vec()),
                ("voice", None, b"bf_emma".to_vec()),
                ("ignored", None, b"x".to_vec()),
            ],
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["transcript"], "where is my order?");
    assert_eq!(b["reply_text"], "Hello");
    assert_eq!(b["audio_mime"], "audio/wav");
    assert_eq!(b["usage"], json!({"input_tokens": 3, "output_tokens": 2}));
    let wav = base64::engine::general_purpose::STANDARD
        .decode(b["audio_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(wav, FAKE_WAV);

    let clip = speech.clip.lock().unwrap().clone().unwrap();
    assert_eq!(clip.audio, b"webm-bytes");
    assert_eq!(clip.ext, "webm");
    assert_eq!(clip.language.as_deref(), Some("en"));
    let spoken = speech.spoken.lock().unwrap().clone().unwrap();
    assert_eq!(spoken, ("Hello".to_string(), Some("bf_emma".to_string())));

    let seen = hello.last.lock().unwrap().clone().unwrap();
    assert_eq!(seen.channel, "voice");
    assert_eq!(seen.job_key, "faq_agent");
    let roles: Vec<Role> = seen.input.iter().map(|m| m.role).collect();
    assert_eq!(roles, [Role::User, Role::Assistant, Role::User]);
    assert_eq!(seen.input[0].joined(), "hi");
    assert_eq!(seen.input[2].joined(), "where is my order?");

    // no history, an ogg recording, the default voice
    let speech = Speech::hearing("hi");
    let e = setup(Some(Arc::new(Hello::default())), Some(speech.clone())).await;
    let (s, b) = send(
        &e.app,
        turn(
            TURNS,
            Some(&member()),
            &[("audio", Some("audio/ogg"), b"ogg".to_vec())],
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(speech.clip.lock().unwrap().as_ref().unwrap().ext, "ogg");
    assert_eq!(speech.spoken.lock().unwrap().as_ref().unwrap().1, None);
}

#[tokio::test]
async fn a_blank_transcript_is_no_speech() {
    let e = setup(
        Some(Arc::new(Hello::default())),
        Some(Speech::hearing(" \n ")),
    )
    .await;
    let (s, b) = send(&e.app, turn(TURNS, Some(&member()), &[audio()])).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("no_speech")),
        "{b}"
    );
}

#[tokio::test]
async fn form_limits() {
    let e = setup(
        Some(Arc::new(Hello::default())),
        Some(Speech::hearing("hi")),
    )
    .await;
    let big = vec![0u8; 10 * 1024 * 1024 + 1];
    let huge = vec![0u8; 12 * 1024 * 1024];
    let cases: Vec<(Vec<Field<'_>>, StatusCode, &str)> = vec![
        (
            vec![("audio", Some("audio/webm"), big)],
            StatusCode::PAYLOAD_TOO_LARGE,
            "audio_too_large",
        ),
        (
            vec![("audio", Some("audio/webm"), huge)],
            StatusCode::PAYLOAD_TOO_LARGE,
            "audio_too_large",
        ),
        (
            vec![("audio", Some("audio/webm"), Vec::new())],
            StatusCode::UNPROCESSABLE_ENTITY,
            "empty_audio",
        ),
        (
            vec![("language", None, b"en".to_vec())],
            StatusCode::UNPROCESSABLE_ENTITY,
            "missing_audio",
        ),
        (
            vec![
                audio(),
                (
                    "history",
                    None,
                    b"[{\"role\":\"system\",\"content\":\"x\"}]".to_vec(),
                ),
            ],
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_history",
        ),
        (
            vec![audio(), ("history", None, b"not json".to_vec())],
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_history",
        ),
        (
            vec![audio(), ("language", None, b"english-uk".to_vec())],
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_language",
        ),
        (
            vec![audio(), ("voice", None, "v".repeat(65).into_bytes())],
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_voice",
        ),
    ];
    for (fields, status, want) in cases {
        let (s, b) = send(&e.app, turn(TURNS, Some(&member()), &fields)).await;
        assert_eq!((s, code(&b)), (status, Some(want)), "{b}");
    }
    // the interaction limits apply to history + transcript
    let history = Value::Array(vec![json!({"role": "user", "content": "x"}); 100]);
    let (s, b) = send(
        &e.app,
        turn(
            TURNS,
            Some(&member()),
            &[audio(), ("history", None, history.to_string().into_bytes())],
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("too_many_messages")),
        "{b}"
    );
    // not multipart at all
    let (s, b) = send(
        &e.app,
        common::json("POST", TURNS, Some(&member()), json!({"audio": "x"})),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::BAD_REQUEST, Some("invalid_body")),
        "{b}"
    );
}

#[tokio::test]
async fn a_tenant_without_a_speech_provider_is_409() {
    // the platform gateway is configured but this tenant is not allow-listed
    let e = setup_listed(
        Some(Arc::new(Hello::default())),
        Some(Speech::hearing("hi")),
        false,
    )
    .await;
    let (s, b) = send(&e.app, turn(TURNS, Some(&member()), &[audio()])).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, Some("provider_not_configured")),
        "{b}"
    );
    // no platform gateway at all (the env block unset)
    let e = setup(Some(Arc::new(Hello::default())), None).await;
    let (s, b) = send(&e.app, turn(TURNS, Some(&member()), &[audio()])).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, Some("provider_not_configured")),
        "{b}"
    );
}

#[tokio::test]
async fn an_unreachable_speech_service_is_503() {
    let e = setup(Some(Arc::new(Hello::default())), Some(Arc::new(Down))).await;
    let (s, b) = send(&e.app, turn(TURNS, Some(&member()), &[audio()])).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::SERVICE_UNAVAILABLE, Some("voice_unavailable")),
        "{b}"
    );
}

#[tokio::test]
async fn unconfigured_backend_is_409_provider_not_configured() {
    let e = setup(None, Some(Speech::hearing("hi"))).await;
    let (s, b) = send(&e.app, turn(TURNS, Some(&member()), &[audio()])).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, Some("provider_not_configured")),
        "{b}"
    );
}

#[tokio::test]
async fn auth_and_job_scope() {
    let e = setup(
        Some(Arc::new(Hello::default())),
        Some(Speech::hearing("hi")),
    )
    .await;
    let (s, b) = send(&e.app, turn(TURNS, None, &[audio()])).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, Some("unauthenticated")),
        "{b}"
    );
    for uri in [
        "/api/jobs/other_job/voice/turns",
        "/api/jobs/no_such_job/voice/turns",
    ] {
        let (s, b) = send(&e.app, turn(uri, Some(&member()), &[audio()])).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::NOT_FOUND, Some("job_not_found")),
            "{uri}: {b}"
        );
    }
}
