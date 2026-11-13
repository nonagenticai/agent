//! The native shape, `POST /api/jobs/{job}/interactions`.
//!
//! In: `{input: string | [{role: "system"|"user"|"assistant", content: string | [{type: "text", text} |
//! {type: "data", data}]}], instructions?, channel?, party?: {id, display_name?, role?}, conversation?: {id}, metadata?,
//! stream?, callback_url?}`.
//!
//! D109/D108/A4: a request with a party (an API key, or `party`) is a runtime turn; without `conversation` it opens a
//! new conversation seeded with the earlier `input` turns, with `conversation.id` (the caller's thread key, at most
//! 200 characters) it joins the job's `api` conversation of that key, created on first use; `instructions` and `system` messages are NOT applied for it (the system
//! message comes from the release's layers only) — accepted and ignored. The staff playground (a member without a
//! party) still sends them to its model. `channel` labels the request; a customer conversation is always `api`.
//!
//! Out (sync): `{id: "int_<uuid>", object: "interaction", status: "completed", job, channel, conversation?: {id},
//! created_at (RFC 3339),
//! output: [{role: "assistant", content: [{type: "text", text}]}], output_text, usage: {input_tokens, output_tokens}
//! | null}`.
//!
//! Out (stream): `interaction.created` (the object, `in_progress`, empty output), `output_text.delta` `{delta}` per
//! delta, then `interaction.completed` (the whole object), or `error` `{error: {code, message}}`.
//!
//! Out (async, with `callback_url`; crate::interaction::webhook): 202 with the object `queued` (empty output, plus
//! `callback_url`); the callback receives the sync object (`completed`), or the object `failed` with empty output and
//! `error: {code, message}`.
use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Encoder, Frame, error_object};
use crate::interaction::model::{
    BackendError, BackendEvent, ConversationRef, DEFAULT_CHANNEL, InteractionRequest,
    InteractionResult, Message, Part, Party, Role, Scope, Usage,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeBody {
    pub input: NativeInput,
    pub instructions: Option<String>,
    pub channel: Option<String>,
    pub party: Option<Party>,
    /// The caller's own thread key (e.g. a card id): requests with the same `id` of one job are one conversation.
    pub conversation: Option<NativeConversation>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub stream: bool,
    /// Answer asynchronously: 202 now, the result POSTed (signed) to this URL (crate::interaction::webhook).
    pub callback_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeConversation {
    pub id: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum NativeInput {
    Text(String),
    Messages(Vec<NativeMessage>),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeMessage {
    pub role: Role,
    pub content: NativeContent,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum NativeContent {
    Text(String),
    Parts(Vec<NativePart>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum NativePart {
    Text { text: String },
    Data { data: Value },
}

/// The body → the request (validated by the caller) and whether to stream.
pub fn to_request(b: NativeBody, scope: &Scope) -> (InteractionRequest, bool) {
    let input = match b.input {
        NativeInput::Text(t) => vec![Message::text(Role::User, t)],
        NativeInput::Messages(ms) => ms
            .into_iter()
            .map(|m| Message {
                role: m.role,
                parts: match m.content {
                    NativeContent::Text(t) => vec![Part::Text(t)],
                    NativeContent::Parts(ps) => ps
                        .into_iter()
                        .map(|p| match p {
                            NativePart::Text { text } => Part::Text(text),
                            NativePart::Data { data } => Part::Data(data),
                        })
                        .collect(),
                },
            })
            .collect(),
    };
    let mut req = InteractionRequest::new(scope, input);
    req.channel = b.channel.unwrap_or_else(|| DEFAULT_CHANNEL.into());
    req.party = b.party;
    // always the `api` conversation channel: a caller's `channel` labels the request, never the conversation
    req.conversation = b.conversation.map(|c| ConversationRef {
        channel: DEFAULT_CHANNEL.into(),
        id: c.id,
        route: Default::default(),
    });
    req.instructions = b.instructions;
    req.metadata = b.metadata;
    (req, b.stream)
}

/// The interaction object, sync and streamed.
pub struct NativeEncoder {
    id: String,
    job: String,
    channel: String,
    /// The caller's thread key (`conversation.id`), echoed so a delivered answer names its thread.
    conversation: Option<String>,
    created_at: String,
    text: String,
}

impl NativeEncoder {
    pub fn new(req: &InteractionRequest) -> Self {
        Self {
            id: format!("int_{}", req.id),
            job: req.job_key.clone(),
            channel: req.channel.clone(),
            conversation: req.conversation.as_ref().map(|c| c.id.clone()),
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            text: String::new(),
        }
    }

    /// The 202 answer of an async interaction: `queued`, empty output, and where the result will be delivered.
    pub fn queued(&self, callback_url: &str) -> Value {
        let mut v = self.object("queued", None, None);
        v["callback_url"] = json!(callback_url);
        v
    }

    /// The callback body of an async interaction the backend could not answer: `failed`, empty output, `error`.
    pub fn failed(&self, e: &BackendError) -> Value {
        let mut v = self.object("failed", None, None);
        v["error"] = error_object(e);
        v
    }

    fn object(&self, status: &str, text: Option<&str>, usage: Option<Usage>) -> Value {
        let output = match text {
            Some(t) => json!([{"role": "assistant", "content": [{"type": "text", "text": t}]}]),
            None => json!([]),
        };
        let mut v = json!({
            "id": self.id,
            "object": "interaction",
            "status": status,
            "job": self.job,
            "channel": self.channel,
            "created_at": self.created_at,
            "output": output,
            "output_text": text.unwrap_or(""),
            "usage": usage.map(|u| json!({"input_tokens": u.input_tokens, "output_tokens": u.output_tokens})),
        });
        if let Some(c) = &self.conversation {
            v["conversation"] = json!({"id": c});
        }
        v
    }
}

impl Encoder for NativeEncoder {
    fn complete(&self, r: &InteractionResult) -> Value {
        self.object("completed", Some(&r.text), r.usage)
    }

    fn start(&mut self) -> Vec<Frame> {
        vec![Frame::named(
            "interaction.created",
            &self.object("in_progress", None, None),
        )]
    }

    fn on(&mut self, ev: Result<BackendEvent, BackendError>) -> Vec<Frame> {
        match ev {
            Ok(BackendEvent::Delta(d)) => {
                self.text.push_str(&d);
                vec![Frame::named("output_text.delta", &json!({"delta": d}))]
            }
            Ok(BackendEvent::Done { usage }) => vec![Frame::named(
                "interaction.completed",
                &self.object("completed", Some(&self.text), usage),
            )],
            Err(e) => vec![Frame::named("error", &json!({"error": error_object(&e)}))],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interaction::shapes::{hello, run, scope};

    fn body(v: Value) -> NativeBody {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn translate_in() {
        let (r, stream) = to_request(body(json!({"input": "hi"})), &scope());
        assert!(!stream);
        assert_eq!(r.channel, "api");
        assert_eq!(r.job_key, "faq_agent");
        assert_eq!(r.input, vec![Message::text(Role::User, "hi")]);
        let (r, stream) = to_request(
            body(json!({
                "input": [
                    {"role": "system", "content": "be brief"},
                    {"role": "user", "content": [{"type": "text", "text": "order"}, {"type": "data", "data": {"id": 7}}]}
                ],
                "instructions": "x", "channel": "telegram", "party": {"id": "tg:1", "display_name": "Ann"},
                "metadata": {"k": "v"}, "stream": true
            })),
            &scope(),
        );
        assert!(stream);
        assert_eq!(r.channel, "telegram");
        assert_eq!(r.party.as_ref().unwrap().id, "tg:1");
        assert_eq!(r.input[0], Message::text(Role::System, "be brief"));
        assert_eq!(r.input[1].parts[1], Part::Data(json!({"id": 7})));
        assert_eq!(r.metadata["k"], "v");
        assert!(r.validate().is_ok());
        assert!(serde_json::from_value::<NativeBody>(json!({"input": "x", "nope": 1})).is_err());
        assert!(
            serde_json::from_value::<NativeBody>(
                json!({"input": [{"role": "tool", "content": "x"}]})
            )
            .is_err()
        );
    }

    #[test]
    fn conversation_id_is_the_apis_thread_key() {
        let (r, _) = to_request(
            body(json!({"input": "hi", "channel": "voice", "conversation": {"id": "card-17"}})),
            &scope(),
        );
        let c = r.conversation.as_ref().unwrap();
        // the channel of the request never names the conversation's channel
        assert_eq!((c.channel.as_str(), c.id.as_str()), ("api", "card-17"));
        assert!(r.validate().is_ok());
        let (mut r, _) = to_request(
            body(json!({"input": "hi", "conversation": {"id": "x".repeat(201)}})),
            &scope(),
        );
        assert_eq!(r.validate().unwrap_err().code(), "invalid_conversation");
        r.conversation.as_mut().unwrap().id = " ".into();
        assert_eq!(r.validate().unwrap_err().code(), "invalid_conversation");
        assert!(
            serde_json::from_value::<NativeBody>(
                json!({"input": "x", "conversation": {"id": "a", "extra": 1}})
            )
            .is_err()
        );
        let (r, _) = to_request(
            body(json!({"input": "hi", "conversation": {"id": "card-17"}})),
            &scope(),
        );
        assert_eq!(
            NativeEncoder::new(&r).queued("https://x")["conversation"],
            json!({"id": "card-17"})
        );
    }

    #[test]
    fn translate_out() {
        let (r, _) = to_request(body(json!({"input": "hi"})), &scope());
        let mut enc = NativeEncoder::new(&r);
        let sync = enc.complete(&InteractionResult {
            text: "Hello".into(),
            usage: None,
            ..Default::default()
        });
        assert_eq!(sync["id"], format!("int_{}", r.id));
        assert_eq!(sync["object"], "interaction");
        assert_eq!(sync["output_text"], "Hello");
        assert_eq!(
            sync["output"][0]["content"][0],
            json!({"type": "text", "text": "Hello"})
        );
        assert!(sync["usage"].is_null());
        let frames = run(&mut enc, hello());
        let names: Vec<_> = frames.iter().map(|f| f.event.unwrap()).collect();
        assert_eq!(
            names,
            [
                "interaction.created",
                "output_text.delta",
                "output_text.delta",
                "interaction.completed"
            ]
        );
        assert_eq!(frames[0].json()["status"], "in_progress");
        assert_eq!(frames[0].json()["output"], json!([]));
        assert_eq!(frames[1].json(), json!({"delta": "Hel"}));
        let done = frames[3].json();
        assert_eq!(done["output_text"], "Hello");
        assert_eq!(
            done["usage"],
            json!({"input_tokens": 3, "output_tokens": 2})
        );
        let f = enc.on(Err(BackendError::Upstream("status 500".into())));
        assert_eq!(f[0].event, Some("error"));
        assert_eq!(f[0].json()["error"]["code"], "interaction_upstream_error");
    }

    #[test]
    fn async_objects() {
        let b = body(json!({"input": "hi", "callback_url": "https://hooks.test/x"}));
        assert_eq!(b.callback_url.as_deref(), Some("https://hooks.test/x"));
        let (r, _) = to_request(b, &scope());
        let enc = NativeEncoder::new(&r);
        let q = enc.queued("https://hooks.test/x");
        assert_eq!(
            (&q["status"], &q["callback_url"], &q["output"]),
            (&json!("queued"), &json!("https://hooks.test/x"), &json!([]))
        );
        assert_eq!(q["id"], format!("int_{}", r.id));
        let f = enc.failed(&BackendError::Unavailable("timeout".into()));
        assert_eq!(f["status"], "failed");
        assert_eq!(f["id"], q["id"]);
        assert_eq!(f["error"]["code"], "interaction_backend_unavailable");
        assert!(
            !f.to_string().contains("timeout"),
            "operator detail stays out"
        );
    }
}
