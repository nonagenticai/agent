//! The OpenAI chat-completions shape, `POST /api/jobs/{job}/chat/completions`: an OpenAI SDK with `base_url =
//! https://<host>/api/jobs/<job>` calls `client.chat.completions.create(...)` against it unchanged.
//!
//! In: `{model?, messages: [{role, content: string | [{type: "text", text}]}], stream?}`. `system` and `developer`
//! messages become the instructions (joined by blank lines); `user` and `assistant` are the input. Non-empty
//! `tools`, any other role or content type is 400 `unsupported_parameter`; every other parameter is ignored.
//!
//! D109/D108 (KH T6): for a customer party (an API key, or a body `party`) the request is a runtime turn on a new
//! conversation seeded with the earlier `user`/`assistant` messages; `instructions` and system/developer messages are NOT applied
//! (the system message comes from the release's layers only) — they are accepted and ignored. The staff playground
//! (a member without a party) still sends them to its model.
//!
//! Out (sync): a `chat.completion`. Out (stream): `data: <chat.completion.chunk>` lines (the first delta
//! `{role: "assistant"}`, then `{content}`, the last `{}` with `finish_reason: "stop"` and the usage), then
//! `data: [DONE]`; a failure mid-stream is `data: {"error": {code, message}}` and no `[DONE]`.
use serde::Deserialize;
use serde_json::{Value, json};

use super::responses::role;
use super::{Encoder, Frame, error_object, unix_now};
use crate::error::{ApiError, ApiResult};
use crate::interaction::model::{
    BackendError, BackendEvent, InteractionRequest, InteractionResult, Message, Part, Role, Scope,
    Usage,
};

#[derive(Debug, Deserialize)]
pub struct ChatBody {
    pub model: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub stream: Option<bool>,
    pub tools: Option<Vec<Value>>,
}

#[derive(Debug, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Option<ChatContent>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ChatContent {
    Text(String),
    Parts(Vec<ChatPart>),
}

#[derive(Debug, Deserialize)]
pub struct ChatPart {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: Option<String>,
}

/// What a body translates to.
#[derive(Debug)]
pub struct Translated {
    pub req: InteractionRequest,
    pub stream: bool,
    /// Echoed in the answer.
    pub model: String,
}

fn unsupported(msg: impl Into<String>) -> ApiError {
    ApiError::BadRequest("unsupported_parameter", msg.into())
}

/// The body → the request (validated by the caller).
pub fn to_request(b: ChatBody, scope: &Scope, channel: String) -> ApiResult<Translated> {
    if b.tools.as_ref().is_some_and(|t| !t.is_empty()) {
        return Err(unsupported("tools are not supported"));
    }
    let mut system = Vec::new();
    let mut input = Vec::new();
    for m in b.messages {
        let role = role(&m.role)?;
        let parts = match m.content {
            None => Vec::new(),
            Some(ChatContent::Text(t)) => vec![Part::Text(t)],
            Some(ChatContent::Parts(ps)) => ps
                .into_iter()
                .map(|p| match (p.kind.as_str(), p.text) {
                    ("text", Some(t)) => Ok(Part::Text(t)),
                    (k, _) => Err(unsupported(format!(
                        "content of type {k:?} is not supported"
                    ))),
                })
                .collect::<ApiResult<Vec<_>>>()?,
        };
        let msg = Message { role, parts };
        if role == Role::System {
            system.push(msg.joined());
        } else {
            input.push(msg);
        }
    }
    let mut req = InteractionRequest::new(scope, input);
    req.channel = channel;
    req.instructions = (!system.is_empty()).then(|| system.join("\n\n"));
    Ok(Translated {
        model: b.model.unwrap_or_else(|| scope.job_key.clone()),
        stream: b.stream.unwrap_or(false),
        req,
    })
}

fn usage_json(u: Usage) -> Value {
    json!({
        "prompt_tokens": u.input_tokens,
        "completion_tokens": u.output_tokens,
        "total_tokens": u.input_tokens.saturating_add(u.output_tokens),
    })
}

/// The `chat.completion` and its chunks.
pub struct ChatEncoder {
    id: String,
    created: i64,
    model: String,
}

impl ChatEncoder {
    pub fn new(t: &Translated) -> Self {
        Self {
            id: format!("chatcmpl-{}", t.req.id.simple()),
            created: unix_now(),
            model: t.model.clone(),
        }
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        })
    }
}

impl Encoder for ChatEncoder {
    fn complete(&self, r: &InteractionResult) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": r.text},
                "finish_reason": "stop",
            }],
            "usage": r.usage.map(usage_json),
        })
    }

    fn start(&mut self) -> Vec<Frame> {
        vec![Frame::data(
            self.chunk(json!({"role": "assistant"}), None).to_string(),
        )]
    }

    fn on(&mut self, ev: Result<BackendEvent, BackendError>) -> Vec<Frame> {
        match ev {
            Ok(BackendEvent::Delta(d)) => {
                vec![Frame::data(
                    self.chunk(json!({"content": d}), None).to_string(),
                )]
            }
            Ok(BackendEvent::Done { usage }) => {
                let mut last = self.chunk(json!({}), Some("stop"));
                if let Some(u) = usage {
                    last["usage"] = usage_json(u);
                }
                vec![Frame::data(last.to_string()), Frame::data("[DONE]")]
            }
            Err(e) => vec![Frame::data(json!({"error": error_object(&e)}).to_string())],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interaction::shapes::{hello, run, scope};

    fn tr(v: Value) -> ApiResult<Translated> {
        to_request(serde_json::from_value(v).unwrap(), &scope(), "api".into())
    }

    #[test]
    fn translate_in() {
        let t = tr(json!({
            "model": "m", "stream": true, "temperature": 0,
            "messages": [
                {"role": "system", "content": "a"},
                {"role": "developer", "content": [{"type": "text", "text": "b"}]},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": null}
            ]
        }))
        .unwrap();
        assert_eq!(t.model, "m");
        assert!(t.stream);
        assert_eq!(t.req.instructions.as_deref(), Some("a\n\nb"));
        assert_eq!(t.req.input[0], Message::text(Role::User, "hi"));
        assert_eq!(t.req.input[1].parts, vec![]);
        let t = tr(json!({"messages": [{"role": "system", "content": "only"}]})).unwrap();
        assert_eq!(t.model, "faq_agent");
        assert_eq!(t.req.validate().unwrap_err().code(), "empty_input");
        for bad in [
            json!({"messages": [{"role": "tool", "content": "x"}]}),
            json!({"messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {}}]}]}),
            json!({"messages": [{"role": "user", "content": "x"}], "tools": [{"type": "function"}]}),
        ] {
            assert_eq!(
                tr(bad.clone()).unwrap_err().code(),
                "unsupported_parameter",
                "{bad}"
            );
        }
    }

    #[test]
    fn translate_out() {
        let t = tr(json!({"messages": [{"role": "user", "content": "hi"}]})).unwrap();
        let mut enc = ChatEncoder::new(&t);
        let sync = enc.complete(&InteractionResult {
            text: "Hello".into(),
            usage: Some(Usage {
                input_tokens: 3,
                output_tokens: 2,
            }),
            ..Default::default()
        });
        assert_eq!(sync["id"], format!("chatcmpl-{}", t.req.id.simple()));
        assert_eq!(sync["object"], "chat.completion");
        assert_eq!(
            sync["choices"][0],
            json!({"index": 0, "message": {"role": "assistant", "content": "Hello"}, "finish_reason": "stop"})
        );
        assert_eq!(
            sync["usage"],
            json!({"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5})
        );
        let frames = run(&mut enc, hello());
        assert!(frames.iter().all(|f| f.event.is_none()));
        assert_eq!(frames.len(), 5);
        assert_eq!(
            frames[0].json()["choices"][0]["delta"],
            json!({"role": "assistant"})
        );
        assert_eq!(
            frames[1].json()["choices"][0]["delta"],
            json!({"content": "Hel"})
        );
        assert_eq!(frames[3].json()["choices"][0]["finish_reason"], "stop");
        assert_eq!(frames[3].json()["usage"]["total_tokens"], 5);
        assert_eq!(frames[4].data, "[DONE]");
        let f = enc.on(Err(BackendError::Upstream("x".into())));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].json()["error"]["code"], "interaction_upstream_error");
    }
}
