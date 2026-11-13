//! Speech-to-text, in the contract of the gateway's kind (crate::config::SpeechProviderKind):
//!
//! - `w2v`: `POST {asr_url}/api/transcribe`, `multipart/form-data` with `file`, `language` (only when given) and
//!   `word_timestamps=false`.
//! - `openai_compatible`: `POST {asr_url}/audio/transcriptions`, `multipart/form-data` with `file`, `model`,
//!   `language` (only when given) and `response_format=json`.
//!
//! The file part is named `audio.<ext>` with the recording's content type; the answer is JSON whose `text` is the
//! transcript. `Authorization: Bearer {VOICE_TOKEN}`. The body is built here (a fixed, tiny form) rather than with
//! reqwest's `multipart` feature. Counted as upstream `asr` / `transcribe` (crate::observability::upstream_http). Only
//! statuses are logged.
use super::{Clip, Stage, VoiceClient, VoiceError, transport};
use crate::config::SpeechProviderKind;

/// The content type the ASR service is told the file has.
pub fn mime_for(ext: &str) -> &'static str {
    match ext {
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        _ => "audio/webm",
    }
}

/// The URL and the text fields (after the file) of a transcription call.
pub fn request_parts(c: &VoiceClient, clip: &Clip) -> (String, Vec<(&'static str, String)>) {
    let mut fields = Vec::new();
    match c.kind {
        SpeechProviderKind::W2v => {
            if let Some(l) = &clip.language {
                fields.push(("language", l.clone()));
            }
            fields.push(("word_timestamps", "false".to_string()));
            (format!("{}/api/transcribe", c.asr_url), fields)
        }
        SpeechProviderKind::OpenaiCompatible => {
            fields.push(("model", c.asr_model.clone()));
            if let Some(l) = &clip.language {
                fields.push(("language", l.clone()));
            }
            fields.push(("response_format", "json".to_string()));
            (format!("{}/audio/transcriptions", c.asr_url), fields)
        }
    }
}

/// The `multipart/form-data` body: the `file` part, then `fields`, with `boundary`.
pub fn form(clip: &Clip, fields: &[(&'static str, String)], boundary: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(clip.audio.len() + 512);
    b.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.{}\"\r\n\
             Content-Type: {}\r\n\r\n",
            clip.ext,
            mime_for(clip.ext)
        )
        .as_bytes(),
    );
    b.extend_from_slice(&clip.audio);
    b.extend_from_slice(b"\r\n");
    for (name, value) in fields {
        b.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    b.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    b
}

/// The transcript of `clip` (possibly blank).
pub async fn transcribe(c: &VoiceClient, clip: Clip) -> Result<String, VoiceError> {
    let boundary = format!("voice-{}", uuid::Uuid::new_v4().simple());
    let (url, fields) = request_parts(c, &clip);
    let body = form(&clip, &fields, &boundary);
    let call = c
        .http
        .post(url)
        .bearer_auth(&c.token)
        .header(
            reqwest::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send();
    let r = crate::observability::upstream_http("asr", "transcribe".into(), call)
        .await
        .map_err(|e| transport(Stage::Asr, &e))?;
    let status = r.status();
    if !status.is_success() {
        tracing::warn!(status = status.as_u16(), "voice_asr_status");
        return Err(VoiceError::Upstream(
            Stage::Asr,
            format!("status {}", status.as_u16()),
        ));
    }
    let bytes = r.bytes().await.map_err(|e| transport(Stage::Asr, &e))?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
        tracing::warn!("voice_asr_malformed");
        VoiceError::Upstream(Stage::Asr, "malformed answer".into())
    })?;
    match v.get("text").and_then(|t| t.as_str()) {
        Some(t) => Ok(t.to_string()),
        None => {
            tracing::warn!("voice_asr_no_text");
            Err(VoiceError::Upstream(Stage::Asr, "no text field".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::Multipart;
    use axum::http::{HeaderMap, StatusCode, Uri};
    use axum::response::IntoResponse;
    use axum::routing::post;

    use super::*;
    use crate::config::VoiceConfig;

    fn client(kind: SpeechProviderKind, asr: &str) -> VoiceClient {
        VoiceClient::from_config(&VoiceConfig {
            kind,
            asr_url: asr.into(),
            tts_url: "http://unused".into(),
            token: "voice-tok".into(),
            asr_model: "whisper-large".into(),
            tts_model: "tts-1".into(),
            tts_voice: "af_heart".into(),
            timeout_s: 10,
            platform_tenants: Vec::new(),
        })
        .unwrap()
    }

    fn w2v(asr: &str) -> VoiceClient {
        client(SpeechProviderKind::W2v, asr)
    }

    /// What the fake ASR saw: the path, the bearer header and every form field as (name, filename, content type,
    /// bytes).
    type Field = (String, Option<String>, Option<String>, Vec<u8>);

    #[derive(Default)]
    struct Seen {
        path: String,
        auth: Option<String>,
        fields: Vec<Field>,
    }

    /// A fake ASR answering on both contracts' paths (the openai_compatible base URL ends in `/v1`).
    async fn upstream(status: StatusCode, answer: &'static str) -> (String, Arc<Mutex<Seen>>) {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let s2 = seen.clone();
        let handler = move |uri: Uri, h: HeaderMap, mut mp: Multipart| {
            let seen = s2.clone();
            async move {
                let mut fields = Vec::new();
                while let Some(f) = mp.next_field().await.unwrap() {
                    let name = f.name().unwrap_or("").to_string();
                    let file = f.file_name().map(String::from);
                    let ct = f.content_type().map(String::from);
                    let data = f.bytes().await.unwrap().to_vec();
                    fields.push((name, file, ct, data));
                }
                let mut g = seen.lock().unwrap();
                g.path = uri.path().to_string();
                g.auth = h
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(String::from);
                g.fields = fields;
                (status, [("content-type", "application/json")], answer).into_response()
            }
        };
        let app = Router::new()
            .route("/api/transcribe", post(handler.clone()))
            .route("/v1/audio/transcriptions", post(handler));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    fn clip(language: Option<&str>) -> Clip {
        Clip {
            audio: b"RIFF\x00\x01binary\r\n--not-a-boundary".to_vec(),
            ext: "ogg",
            language: language.map(String::from),
        }
    }

    fn names(seen: &Mutex<Seen>) -> Vec<String> {
        seen.lock()
            .unwrap()
            .fields
            .iter()
            .map(|f| f.0.clone())
            .collect()
    }

    #[tokio::test]
    async fn w2v_sends_its_form_with_the_bearer_and_reads_text() {
        let (url, seen) = upstream(
            StatusCode::OK,
            r#"{"text": " hello there ", "language": "en"}"#,
        )
        .await;
        let t = transcribe(&w2v(&url), clip(Some("en"))).await.unwrap();
        assert_eq!(t, " hello there ");
        assert_eq!(names(&seen), ["file", "language", "word_timestamps"]);
        let g = seen.lock().unwrap();
        assert_eq!(g.path, "/api/transcribe");
        assert_eq!(g.auth.as_deref(), Some("Bearer voice-tok"));
        assert_eq!(g.fields[0].1.as_deref(), Some("audio.ogg"));
        assert_eq!(g.fields[0].2.as_deref(), Some("audio/ogg"));
        assert_eq!(g.fields[0].3, clip(None).audio);
        assert_eq!(g.fields[1].3, b"en");
        assert_eq!(g.fields[2].3, b"false");
    }

    #[tokio::test]
    async fn w2v_sends_language_only_when_given() {
        let (url, seen) = upstream(StatusCode::OK, r#"{"text": ""}"#).await;
        let t = transcribe(&w2v(&url), clip(None)).await.unwrap();
        assert_eq!(t, "");
        assert_eq!(names(&seen), ["file", "word_timestamps"]);
    }

    #[tokio::test]
    async fn openai_compatible_sends_model_and_response_format() {
        let (url, seen) = upstream(StatusCode::OK, r#"{"text": "hi"}"#).await;
        let c = client(SpeechProviderKind::OpenaiCompatible, &format!("{url}/v1"));
        assert_eq!(transcribe(&c, clip(Some("nl"))).await.unwrap(), "hi");
        assert_eq!(
            names(&seen),
            ["file", "model", "language", "response_format"]
        );
        {
            let g = seen.lock().unwrap();
            assert_eq!(g.path, "/v1/audio/transcriptions");
            assert_eq!(g.auth.as_deref(), Some("Bearer voice-tok"));
            assert_eq!(g.fields[0].1.as_deref(), Some("audio.ogg"));
            assert_eq!(g.fields[1].3, b"whisper-large");
            assert_eq!(g.fields[2].3, b"nl");
            assert_eq!(g.fields[3].3, b"json");
        }
        transcribe(&c, clip(None)).await.unwrap();
        assert_eq!(names(&seen), ["file", "model", "response_format"]);
    }

    #[tokio::test]
    async fn non_2xx_and_bad_json_are_upstream_and_unreachable_is_unavailable() {
        let (url, _) = upstream(StatusCode::INTERNAL_SERVER_ERROR, "{}").await;
        assert_eq!(
            transcribe(&w2v(&url), clip(None)).await,
            Err(VoiceError::Upstream(Stage::Asr, "status 500".into()))
        );
        let c = client(SpeechProviderKind::OpenaiCompatible, &format!("{url}/v1"));
        assert_eq!(
            transcribe(&c, clip(None)).await,
            Err(VoiceError::Upstream(Stage::Asr, "status 500".into()))
        );
        let (url, _) = upstream(StatusCode::OK, "not json").await;
        assert!(matches!(
            transcribe(&w2v(&url), clip(None)).await,
            Err(VoiceError::Upstream(Stage::Asr, _))
        ));
        let (url, _) = upstream(StatusCode::OK, r#"{"segments": []}"#).await;
        assert!(matches!(
            transcribe(&w2v(&url), clip(None)).await,
            Err(VoiceError::Upstream(Stage::Asr, _))
        ));
        // the discard port: connection refused
        assert!(matches!(
            transcribe(&w2v("http://127.0.0.1:9"), clip(None)).await,
            Err(VoiceError::Unavailable(Stage::Asr, _))
        ));
    }
}
