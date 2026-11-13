//! `POST /api/jobs/{job}/voice/turns` — one spoken turn (`JobCaller`: a job API key or any member of the job's
//! tenant, like the interaction routes). `multipart/form-data`:
//!
//! - `audio` (required): the recording, at most 10 MiB (413 `audio_too_large`); empty is 422 `empty_audio`, absent
//!   422 `missing_audio`. Its content type picks the extension the ASR service is told (`audio/webm` → `.webm`,
//!   `audio/ogg` → `.ogg`, `audio/wav` → `.wav`, `audio/mp4` → `.m4a`, else `.webm`).
//! - `history` (optional): JSON `[{role: "user"|"assistant", content: string}]`, the conversation so far (the API keeps
//!   none); anything else is 422 `invalid_history`. For a customer party (an API key; D109) it seeds the turn's new
//!   `voice` conversation, so the runtime sees it; voice turns carry no instructions (D108: the release's layers
//!   only).
//! - `language` (optional): a language hint for the ASR, at most 8 characters of `[A-Za-z0-9-]` (422
//!   `invalid_language`).
//! - `voice` (optional): the TTS voice, at most 64 characters of `[A-Za-z0-9_-]` (422 `invalid_voice`).
//!
//! Flow: ASR → a blank transcript is 422 `no_speech` → the interaction API (channel `voice`, the history then the
//! transcript as the last user message; its limits and its 409/503/502 apply) → TTS of the answer as plain text
//! (crate::voice::speakable: markdown is not spoken; at most 10 000 characters of it). 200 `{transcript, reply_text,
//! audio_b64, audio_mime: "audio/wav", usage}`, `reply_text` the answer as written; `audio_b64` is empty when the
//! answer has nothing to speak. 409 `provider_not_configured` when the job has no speech provider (the platform gateway for
//! `VOICE_PLATFORM_TENANTS`, else the job's live `asr` + `tts` bindings, crate::voice::ProviderSpeech; checked before
//! the body is read); 503 `voice_unavailable` when a speech service cannot be reached or times out; 502 `voice_upstream_error`
//! when one answers badly.
use axum::Router;
use axum::extract::multipart::{MultipartError, MultipartRejection};
use axum::extract::{DefaultBodyLimit, Multipart, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::post;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{CHANNEL, Clip};
use crate::error::{ApiError, ApiResult};
use crate::interaction::{InteractionRequest, Message, Role, Scope, apikeys::JobCaller, collect};
use crate::state::AppState;

/// The largest recording (bytes).
pub const MAX_AUDIO_BYTES: usize = 10 * 1024 * 1024;
/// The whole multipart body: the recording plus room for the history and the small fields.
pub const MAX_BODY_BYTES: usize = MAX_AUDIO_BYTES + 1024 * 1024;
/// The longest answer that is spoken (characters); the text answer is never cut.
pub const MAX_SPOKEN_CHARS: usize = 10_000;
pub const MAX_LANGUAGE: usize = 8;
pub const MAX_VOICE: usize = 64;

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/jobs/{job}/voice/turns",
        post(voice_turn).layer(DefaultBodyLimit::max(MAX_BODY_BYTES)),
    )
}

/// The parsed form.
struct Turn {
    clip: Clip,
    history: Vec<Message>,
    voice: Option<String>,
}

async fn voice_turn(
    j: JobCaller,
    State(st): State<AppState>,
    form: Result<Multipart, MultipartRejection>,
) -> ApiResult<Json<Value>> {
    let scope = Scope::from(&j);
    let Some(speech) = st.speech.resolve(&st, &scope).await? else {
        return Err(ApiError::Conflict(
            "provider_not_configured",
            "this tenant has no speech provider configured".into(),
        ));
    };
    let form = form.map_err(|_| {
        ApiError::BadRequest(
            "invalid_body",
            "the body must be multipart/form-data".into(),
        )
    })?;
    let turn = read_form(form).await?;
    let transcript = speech.transcribe(turn.clip).await?;
    let transcript = transcript.trim().to_string();
    if transcript.is_empty() {
        return Err(ApiError::Unprocessable(
            "no_speech",
            "no speech was recognised in the recording".into(),
        ));
    }
    let mut input = turn.history;
    input.push(Message::text(Role::User, transcript.clone()));
    let mut req = InteractionRequest::new(&scope, input);
    req.channel = CHANNEL.into();
    j.stamp(&mut req);
    // D109: this connector names its conversation channel (one conversation per turn; `history` seeds it)
    req.conversation = Some(crate::interaction::ConversationRef {
        channel: CHANNEL.into(),
        id: req.id.to_string(),
        route: Default::default(),
    });
    req.validate()?;
    let r = collect(crate::interaction::respond(&st, req)).await?;
    // markdown is not spoken; the cut applies to what is spoken, and `reply_text` stays the answer as written
    let spoken: String = super::speakable(&r.text)
        .chars()
        .take(MAX_SPOKEN_CHARS)
        .collect();
    let audio_b64 = if spoken.is_empty() {
        String::new()
    } else {
        let wav = speech.synthesize(spoken, turn.voice).await?;
        base64::engine::general_purpose::STANDARD.encode(&wav)
    };
    let usage = r
        .usage
        .map(|u| json!({"input_tokens": u.input_tokens, "output_tokens": u.output_tokens}));
    Ok(Json(json!({
        "transcript": transcript,
        "reply_text": r.text,
        "audio_b64": audio_b64,
        "audio_mime": "audio/wav",
        "usage": usage,
    })))
}

/// The form's fields; unknown fields are skipped.
async fn read_form(mut form: Multipart) -> ApiResult<Turn> {
    let mut audio: Option<(Vec<u8>, &'static str)> = None;
    let mut history = Vec::new();
    let mut language = None;
    let mut voice = None;
    while let Some(mut field) = form.next_field().await.map_err(multipart_error)? {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "audio" => {
                let ext = ext_for(field.content_type());
                let mut buf = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
                    if buf.len() + chunk.len() > MAX_AUDIO_BYTES {
                        return Err(audio_too_large());
                    }
                    buf.extend_from_slice(&chunk);
                }
                audio = Some((buf, ext));
            }
            "history" => {
                let text = field.text().await.map_err(multipart_error)?;
                history = parse_history(&text)?;
            }
            "language" => {
                let text = field.text().await.map_err(multipart_error)?;
                language = small_field(&text, MAX_LANGUAGE, |c| {
                    c.is_ascii_alphanumeric() || c == '-'
                })
                .map_err(|_| {
                    ApiError::Unprocessable(
                        "invalid_language",
                        format!("language is at most {MAX_LANGUAGE} characters of [A-Za-z0-9-]"),
                    )
                })?;
            }
            "voice" => {
                let text = field.text().await.map_err(multipart_error)?;
                voice = small_field(&text, MAX_VOICE, |c| {
                    c.is_ascii_alphanumeric() || c == '-' || c == '_'
                })
                .map_err(|_| {
                    ApiError::Unprocessable(
                        "invalid_voice",
                        format!("voice is at most {MAX_VOICE} characters of [A-Za-z0-9_-]"),
                    )
                })?;
            }
            _ => {}
        }
    }
    let Some((audio, ext)) = audio else {
        return Err(ApiError::Unprocessable(
            "missing_audio",
            "the form carries no audio field".into(),
        ));
    };
    if audio.is_empty() {
        return Err(ApiError::Unprocessable(
            "empty_audio",
            "the recording is empty".into(),
        ));
    }
    Ok(Turn {
        clip: Clip {
            audio,
            ext,
            language,
        },
        history,
        voice,
    })
}

fn audio_too_large() -> ApiError {
    ApiError::TooLarge(
        "audio_too_large",
        format!("the recording is at most {MAX_AUDIO_BYTES} bytes"),
    )
}

/// A body over the route's limit is the recording's fault; any other parse failure is a malformed body.
fn multipart_error(e: MultipartError) -> ApiError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        audio_too_large()
    } else {
        ApiError::BadRequest("invalid_body", "the multipart body is not valid".into())
    }
}

/// The file extension the ASR service is told, from the recording's content type (parameters ignored).
pub fn ext_for(content_type: Option<&str>) -> &'static str {
    let essence = content_type
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match essence.as_str() {
        "audio/ogg" => "ogg",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/mp4" => "m4a",
        _ => "webm",
    }
}

/// A trimmed optional text field: blank is `None`; over `max` characters or a character outside `ok` is an error.
fn small_field(text: &str, max: usize, ok: impl Fn(char) -> bool) -> Result<Option<String>, ()> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    if t.chars().count() > max || !t.chars().all(ok) {
        return Err(());
    }
    Ok(Some(t.to_string()))
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum HistoryRole {
    User,
    Assistant,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryItem {
    role: HistoryRole,
    content: String,
}

/// The `history` field: blank is no history.
pub fn parse_history(text: &str) -> ApiResult<Vec<Message>> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let items: Vec<HistoryItem> = serde_json::from_str(text).map_err(|_| {
        ApiError::Unprocessable(
            "invalid_history",
            "history must be a JSON array of {role: \"user\"|\"assistant\", content: string}"
                .into(),
        )
    })?;
    Ok(items
        .into_iter()
        .map(|i| {
            let role = match i.role {
                HistoryRole::User => Role::User,
                HistoryRole::Assistant => Role::Assistant,
            };
            Message::text(role, i.content)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_follow_the_content_type() {
        for (ct, ext) in [
            (Some("audio/webm"), "webm"),
            (Some("audio/webm;codecs=opus"), "webm"),
            (Some("audio/ogg; codecs=opus"), "ogg"),
            (Some("AUDIO/WAV"), "wav"),
            (Some("audio/mp4"), "m4a"),
            (Some("application/octet-stream"), "webm"),
            (None, "webm"),
        ] {
            assert_eq!(ext_for(ct), ext, "{ct:?}");
        }
    }

    #[test]
    fn history_parses_user_and_assistant_only() {
        let h = parse_history(
            r#"[{"role":"user","content":"hi"},{"role":"assistant","content":"hello"}]"#,
        )
        .unwrap();
        assert_eq!(
            h,
            vec![
                Message::text(Role::User, "hi"),
                Message::text(Role::Assistant, "hello")
            ]
        );
        assert!(parse_history("  ").unwrap().is_empty());
        for bad in [
            "nope",
            "{}",
            r#"[{"role":"system","content":"x"}]"#,
            r#"[{"role":"user"}]"#,
            r#"[{"role":"user","content":7}]"#,
            r#"[{"role":"user","content":"x","extra":1}]"#,
        ] {
            assert_eq!(
                parse_history(bad).unwrap_err().code(),
                "invalid_history",
                "{bad}"
            );
        }
    }

    #[test]
    fn small_fields() {
        let lang =
            |s: &str| small_field(s, MAX_LANGUAGE, |c| c.is_ascii_alphanumeric() || c == '-');
        assert_eq!(lang(" en "), Ok(Some("en".into())));
        assert_eq!(lang(""), Ok(None));
        assert_eq!(lang("zh-Hant1"), Ok(Some("zh-Hant1".into())));
        assert!(lang("zh-Hant-TW").is_err());
        assert!(lang("e\"n").is_err());
    }
}
