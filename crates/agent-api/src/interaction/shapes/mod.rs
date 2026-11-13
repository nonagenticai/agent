//! The wire shapes of the interaction API. Each is a pure translate-in / translate-out layer, with no HTTP in it:
//! a body (plus the `Scope` and the channel) → an `InteractionRequest`; an [`InteractionResult`] → the sync
//! answer; backend events → the frames of the streamed answer ([`Encoder`]).
//!
//! - [`native`]: `POST /api/jobs/{job}/interactions`.
//! - [`responses`]: `POST /api/jobs/{job}/responses` (the OpenAI Responses subset).
//! - [`chat_completions`]: `POST /api/jobs/{job}/chat/completions` (the OpenAI chat-completions subset).
use serde_json::Value;

use super::model::{BackendError, BackendEvent, InteractionResult};

pub mod chat_completions;
pub mod native;
pub mod responses;

/// One server-sent event: an optional `event:` name and its `data:`.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub event: Option<&'static str>,
    pub data: String,
}

impl Frame {
    pub fn named(event: &'static str, data: &Value) -> Self {
        Self {
            event: Some(event),
            data: data.to_string(),
        }
    }

    pub fn data(data: impl Into<String>) -> Self {
        Self {
            event: None,
            data: data.into(),
        }
    }

    /// The data as JSON (tests).
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.data).unwrap_or(Value::Null)
    }
}

/// How a shape answers: the whole object (sync), or frames as the backend streams (stream).
pub trait Encoder: Send + 'static {
    /// The sync answer.
    fn complete(&self, r: &InteractionResult) -> Value;
    /// The frames sent before the first backend event.
    fn start(&mut self) -> Vec<Frame>;
    /// The frames for one backend event (a delta, the end, or a failure mid-stream).
    fn on(&mut self, ev: Result<BackendEvent, BackendError>) -> Vec<Frame>;
}

/// `{"code", "message"}` of a failure mid-stream.
pub fn error_object(e: &BackendError) -> Value {
    serde_json::json!({"code": e.code(), "message": e.public_message()})
}

/// Unix seconds now.
pub fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Run an encoder over a list of events (unit tests of the shapes).
#[cfg(test)]
pub fn run(enc: &mut dyn Encoder, events: Vec<Result<BackendEvent, BackendError>>) -> Vec<Frame> {
    let mut out = enc.start();
    for ev in events {
        out.extend(enc.on(ev));
    }
    out
}

#[cfg(test)]
pub fn hello() -> Vec<Result<BackendEvent, BackendError>> {
    use super::model::Usage;
    vec![
        Ok(BackendEvent::Delta("Hel".into())),
        Ok(BackendEvent::Delta("lo".into())),
        Ok(BackendEvent::Done {
            usage: Some(Usage {
                input_tokens: 3,
                output_tokens: 2,
            }),
        }),
    ]
}

#[cfg(test)]
pub fn scope() -> super::model::Scope {
    super::model::Scope {
        tenant: uuid::Uuid::nil(),
        job_id: uuid::Uuid::nil(),
        job_key: "faq_agent".into(),
    }
}
