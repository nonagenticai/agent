//! Text-to-speech, in the contract of the gateway's kind (crate::config::SpeechProviderKind):
//!
//! - `w2v`: `POST {tts_url}/api/tts/synthesize` with JSON `{text, provider: "kokoro", voice, output_format: "wav"}`.
//! - `openai_compatible`: `POST {tts_url}/audio/speech` with JSON `{model, input, voice, response_format: "wav"}`.
//!
//! The answer is the WAV bytes. `Authorization: Bearer {VOICE_TOKEN}`. Counted as upstream `tts` / `synthesize`
//! (crate::observability::upstream_http). Only statuses are logged.
use serde_json::json;

use super::{Stage, VoiceClient, VoiceError, transport};
use crate::config::SpeechProviderKind;

/// The TTS provider a w2v gateway is asked for.
pub const W2V_PROVIDER: &str = "kokoro";

/// The URL and JSON body of a synthesis call: `voice` when the turn names one, else the configured default.
pub fn request(c: &VoiceClient, text: &str, voice: Option<&str>) -> (String, serde_json::Value) {
    let voice = voice.unwrap_or(c.tts_voice.as_str());
    match c.kind {
        SpeechProviderKind::W2v => (
            format!("{}/api/tts/synthesize", c.tts_url),
            json!({
                "text": text,
                "provider": W2V_PROVIDER,
                "voice": voice,
                "output_format": "wav",
            }),
        ),
        SpeechProviderKind::OpenaiCompatible => (
            format!("{}/audio/speech", c.tts_url),
            json!({
                "model": c.tts_model,
                "input": text,
                "voice": voice,
                "response_format": "wav",
            }),
        ),
    }
}

/// WAV bytes of `text`.
pub async fn synthesize(
    c: &VoiceClient,
    text: &str,
    voice: Option<&str>,
) -> Result<Vec<u8>, VoiceError> {
    let (url, body) = request(c, text, voice);
    let call = c.http.post(url).bearer_auth(&c.token).json(&body).send();
    let r = crate::observability::upstream_http("tts", "synthesize".into(), call)
        .await
        .map_err(|e| transport(Stage::Tts, &e))?;
    let status = r.status();
    if !status.is_success() {
        tracing::warn!(status = status.as_u16(), "voice_tts_status");
        return Err(VoiceError::Upstream(
            Stage::Tts,
            format!("status {}", status.as_u16()),
        ));
    }
    let bytes = r.bytes().await.map_err(|e| transport(Stage::Tts, &e))?;
    if bytes.is_empty() {
        tracing::warn!("voice_tts_empty");
        return Err(VoiceError::Upstream(Stage::Tts, "empty audio".into()));
    }
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::http::{HeaderMap, StatusCode, Uri};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use serde_json::Value;

    use super::*;
    use crate::config::VoiceConfig;

    fn client(kind: SpeechProviderKind, tts: &str) -> VoiceClient {
        VoiceClient::from_config(&VoiceConfig {
            kind,
            asr_url: "http://unused".into(),
            tts_url: tts.into(),
            token: "voice-tok".into(),
            asr_model: "whisper-1".into(),
            tts_model: "tts-hd".into(),
            tts_voice: "af_heart".into(),
            timeout_s: 10,
            platform_tenants: Vec::new(),
        })
        .unwrap()
    }

    #[derive(Default)]
    struct Seen {
        path: String,
        auth: Option<String>,
        body: Option<Value>,
    }

    const WAV: &[u8] = b"RIFF\x24\x00\x00\x00WAVEfmt ";

    /// A fake TTS answering on both contracts' paths (the openai_compatible base URL ends in `/v1`).
    async fn upstream(status: StatusCode, answer: &'static [u8]) -> (String, Arc<Mutex<Seen>>) {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let s2 = seen.clone();
        let handler = move |uri: Uri, h: HeaderMap, body: axum::Json<Value>| {
            let seen = s2.clone();
            async move {
                let mut g = seen.lock().unwrap();
                g.path = uri.path().to_string();
                g.auth = h
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(String::from);
                g.body = Some(body.0);
                (status, [("content-type", "audio/wav")], answer).into_response()
            }
        };
        let app = Router::new()
            .route("/api/tts/synthesize", post(handler.clone()))
            .route("/v1/audio/speech", post(handler));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    #[tokio::test]
    async fn w2v_posts_json_with_the_bearer_and_returns_the_wav() {
        let (url, seen) = upstream(StatusCode::OK, WAV).await;
        let c = client(SpeechProviderKind::W2v, &url);
        let wav = synthesize(&c, "Hello", None).await.unwrap();
        assert_eq!(wav, WAV);
        {
            let g = seen.lock().unwrap();
            assert_eq!(g.path, "/api/tts/synthesize");
            assert_eq!(g.auth.as_deref(), Some("Bearer voice-tok"));
            assert_eq!(
                g.body.as_ref().unwrap(),
                &json!({"text": "Hello", "provider": "kokoro", "voice": "af_heart", "output_format": "wav"})
            );
        }
        synthesize(&c, "Hi", Some("bf_emma")).await.unwrap();
        assert_eq!(
            seen.lock().unwrap().body.as_ref().unwrap()["voice"],
            "bf_emma"
        );
    }

    #[tokio::test]
    async fn openai_compatible_posts_model_input_voice_and_format() {
        let (url, seen) = upstream(StatusCode::OK, WAV).await;
        let c = client(SpeechProviderKind::OpenaiCompatible, &format!("{url}/v1"));
        let wav = synthesize(&c, "Hello", Some("nova")).await.unwrap();
        assert_eq!(wav, WAV);
        let g = seen.lock().unwrap();
        assert_eq!(g.path, "/v1/audio/speech");
        assert_eq!(g.auth.as_deref(), Some("Bearer voice-tok"));
        assert_eq!(
            g.body.as_ref().unwrap(),
            &json!({"model": "tts-hd", "input": "Hello", "voice": "nova", "response_format": "wav"})
        );
    }

    #[tokio::test]
    async fn non_2xx_is_upstream_and_unreachable_is_unavailable() {
        let (url, _) = upstream(StatusCode::BAD_GATEWAY, b"nope").await;
        for c in [
            client(SpeechProviderKind::W2v, &url),
            client(SpeechProviderKind::OpenaiCompatible, &format!("{url}/v1")),
        ] {
            assert_eq!(
                synthesize(&c, "Hello", None).await,
                Err(VoiceError::Upstream(Stage::Tts, "status 502".into()))
            );
        }
        let (url, _) = upstream(StatusCode::OK, b"").await;
        assert!(matches!(
            synthesize(&client(SpeechProviderKind::W2v, &url), "Hello", None).await,
            Err(VoiceError::Upstream(Stage::Tts, _))
        ));
        assert!(matches!(
            synthesize(
                &client(SpeechProviderKind::W2v, "http://127.0.0.1:9"),
                "Hello",
                None
            )
            .await,
            Err(VoiceError::Unavailable(Stage::Tts, _))
        ));
    }
}
