//! The OpenAI Responses shape, `POST /api/jobs/{job}/responses`: an OpenAI SDK with `base_url =
//! https://<host>/api/jobs/<job>` calls `client.responses.create(...)` against it unchanged.
//!
//! In (the stateless subset): `model?` (echoed, never used to pick a model; default the job key), `input: string |
//! [{role, content} | {type: "message", role, content}]` with content a string or `[{type: "input_text" |
//! "output_text" | "text", text}]`, `instructions?`, `stream?`, `metadata?`, `store?` (accepted, ignored). Roles
//! `system` and `developer` are system messages. Non-empty `tools` are 400 `unsupported_parameter`; any other item or
//! content type too. Every other OpenAI parameter (temperature, …) is accepted and ignored.
//!
//! `previous_response_id` (B4, B10 §3) continues the customer conversation of that response: the new request joins the
//! conversation the earlier turn ran on (its stored history; earlier `input` turns are not seeded again). The id is the
//! `resp_<request id>` this shape answered with; a malformed id, or one that names no turn of the same job and customer
//! party, is 404 `response_not_found` (never another tenant's, job's or party's conversation). The staff playground
//! stores nothing, so there it is 400 `unsupported_parameter`.
//!
//! D109/D108 (KH T6): for a customer party (an API key, or a body `party`) the request is a runtime turn on a new
//! conversation seeded with the earlier `input` turns; `instructions` and system/developer messages are NOT applied
//! (the system message comes from the release's layers only) — they are accepted and ignored. The staff playground
//! (a member without a party) still sends them to its model.
//!
//! Out (sync): the `response` object. Out (stream): `response.created`, `response.in_progress`,
//! `response.output_item.added`, `response.content_part.added`, `response.output_text.delta`…,
//! `response.output_text.done`, `response.content_part.done`, `response.output_item.done`, `response.completed`; a
//! failure mid-stream is `response.failed`. Every frame's data carries its `type` and an increasing
//! `sequence_number` from 0.
use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{Encoder, Frame, error_object, unix_now};
use crate::error::{ApiError, ApiResult};
use crate::interaction::model::{
    BackendError, BackendEvent, InteractionRequest, InteractionResult, Message, Part, Role, Scope,
    Usage,
};

#[derive(Debug, Deserialize)]
pub struct ResponsesBody {
    pub model: Option<String>,
    pub input: ResponsesInput,
    pub instructions: Option<String>,
    pub stream: Option<bool>,
    pub metadata: Option<BTreeMap<String, String>>,
    /// Accepted and ignored: nothing is stored.
    pub store: Option<bool>,
    pub previous_response_id: Option<String>,
    pub tools: Option<Vec<Value>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ResponsesInput {
    Text(String),
    Items(Vec<ResponsesItem>),
}

#[derive(Debug, Deserialize)]
pub struct ResponsesItem {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub role: Option<String>,
    pub content: Option<ResponsesContent>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ResponsesContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: Option<String>,
}

fn unsupported(msg: impl Into<String>) -> ApiError {
    ApiError::BadRequest("unsupported_parameter", msg.into())
}

/// A role of an input message: `developer` is a system message.
pub(super) fn role(raw: &str) -> ApiResult<Role> {
    match raw {
        "system" | "developer" => Ok(Role::System),
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        other => Err(unsupported(format!("role {other:?} is not supported"))),
    }
}

/// What a body translates to.
#[derive(Debug)]
pub struct Translated {
    pub req: InteractionRequest,
    pub stream: bool,
    /// Echoed in the answer.
    pub model: String,
    /// The request id named by `previous_response_id` (resolved to its conversation by the route).
    pub previous: Option<Uuid>,
}

/// The body → the request (validated by the caller). `channel` is `api` or the `X-Interaction-Channel` header.
pub fn to_request(b: ResponsesBody, scope: &Scope, channel: String) -> ApiResult<Translated> {
    let previous = b
        .previous_response_id
        .as_deref()
        .map(|id| response_request_id(id).ok_or_else(response_not_found))
        .transpose()?;
    if b.tools.as_ref().is_some_and(|t| !t.is_empty()) {
        return Err(unsupported("tools are not supported"));
    }
    let input = match b.input {
        ResponsesInput::Text(t) => vec![Message::text(Role::User, t)],
        ResponsesInput::Items(items) => {
            items.into_iter().map(item).collect::<ApiResult<Vec<_>>>()?
        }
    };
    let mut req = InteractionRequest::new(scope, input);
    req.channel = channel;
    req.instructions = b.instructions;
    req.metadata = b.metadata.unwrap_or_default();
    Ok(Translated {
        model: b.model.unwrap_or_else(|| scope.job_key.clone()),
        stream: b.stream.unwrap_or(false),
        req,
        previous,
    })
}

/// The request id inside a `resp_<simple uuid>` this shape answered with.
pub fn response_request_id(id: &str) -> Option<Uuid> {
    id.strip_prefix("resp_")
        .filter(|h| h.len() == 32)
        .and_then(|h| Uuid::try_parse(h).ok())
}

/// 404 for a `previous_response_id` that names no turn the caller may continue (unknown, malformed or foreign alike).
pub fn response_not_found() -> ApiError {
    ApiError::Missing(
        "response_not_found",
        "previous_response_id names no response of this job".into(),
    )
}

fn item(it: ResponsesItem) -> ApiResult<Message> {
    if let Some(k) = it.kind.as_deref().filter(|k| *k != "message") {
        return Err(unsupported(format!(
            "input items of type {k:?} are not supported"
        )));
    }
    let role = role(
        it.role
            .as_deref()
            .ok_or_else(|| unsupported("an input message needs a role"))?,
    )?;
    let parts = match it.content {
        None => Vec::new(),
        Some(ResponsesContent::Text(t)) => vec![Part::Text(t)],
        Some(ResponsesContent::Parts(ps)) => ps
            .into_iter()
            .map(|p| match (p.kind.as_str(), p.text) {
                ("input_text" | "output_text" | "text", Some(t)) => Ok(Part::Text(t)),
                (k, _) => Err(unsupported(format!(
                    "content of type {k:?} is not supported"
                ))),
            })
            .collect::<ApiResult<Vec<_>>>()?,
    };
    Ok(Message { role, parts })
}

/// The `response` object and its event sequence.
pub struct ResponsesEncoder {
    id: String,
    msg_id: String,
    created_at: i64,
    model: String,
    metadata: BTreeMap<String, String>,
    seq: u64,
    text: String,
}

impl ResponsesEncoder {
    pub fn new(t: &Translated) -> Self {
        Self {
            id: format!("resp_{}", t.req.id.simple()),
            msg_id: format!("msg_{}", Uuid::now_v7().simple()),
            created_at: unix_now(),
            model: t.model.clone(),
            metadata: t.req.metadata.clone(),
            seq: 0,
            text: String::new(),
        }
    }

    fn part(text: &str) -> Value {
        json!({"type": "output_text", "text": text, "annotations": []})
    }

    fn item(&self, status: &str, text: Option<&str>) -> Value {
        json!({
            "type": "message",
            "id": self.msg_id,
            "status": status,
            "role": "assistant",
            "content": text.map_or_else(|| json!([]), |t| json!([Self::part(t)])),
        })
    }

    fn response(&self, status: &str, text: Option<&str>, usage: Option<Usage>) -> Value {
        json!({
            "id": self.id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "model": self.model,
            "output": text.map_or_else(|| json!([]), |t| json!([self.item("completed", Some(t))])),
            "output_text": text.unwrap_or(""),
            "usage": usage.map(|u| json!({
                "input_tokens": u.input_tokens,
                "output_tokens": u.output_tokens,
                "total_tokens": u.input_tokens.saturating_add(u.output_tokens),
            })),
            "metadata": self.metadata,
            "error": null,
            "incomplete_details": null,
        })
    }

    /// A frame of `kind` with `type` and the next `sequence_number` merged into `payload`.
    fn frame(&mut self, kind: &'static str, mut payload: Value) -> Frame {
        payload["type"] = json!(kind);
        payload["sequence_number"] = json!(self.seq);
        self.seq += 1;
        Frame::named(kind, &payload)
    }
}

impl Encoder for ResponsesEncoder {
    fn complete(&self, r: &InteractionResult) -> Value {
        self.response("completed", Some(&r.text), r.usage)
    }

    fn start(&mut self) -> Vec<Frame> {
        let r = self.response("in_progress", None, None);
        let item = self.item("in_progress", None);
        let item_id = self.msg_id.clone();
        vec![
            self.frame("response.created", json!({"response": r})),
            self.frame("response.in_progress", json!({"response": r})),
            self.frame(
                "response.output_item.added",
                json!({"output_index": 0, "item": item}),
            ),
            self.frame(
                "response.content_part.added",
                json!({"item_id": item_id, "output_index": 0, "content_index": 0, "part": Self::part("")}),
            ),
        ]
    }

    fn on(&mut self, ev: Result<BackendEvent, BackendError>) -> Vec<Frame> {
        let item_id = self.msg_id.clone();
        match ev {
            Ok(BackendEvent::Delta(d)) => {
                self.text.push_str(&d);
                vec![self.frame(
                    "response.output_text.delta",
                    json!({"item_id": item_id, "output_index": 0, "content_index": 0, "delta": d, "logprobs": []}),
                )]
            }
            Ok(BackendEvent::Done { usage }) => {
                let text = self.text.clone();
                let item = self.item("completed", Some(&text));
                let r = self.response("completed", Some(&text), usage);
                vec![
                    self.frame(
                        "response.output_text.done",
                        json!({"item_id": item_id, "output_index": 0, "content_index": 0, "text": text, "logprobs": []}),
                    ),
                    self.frame(
                        "response.content_part.done",
                        json!({"item_id": item_id, "output_index": 0, "content_index": 0, "part": Self::part(&text)}),
                    ),
                    self.frame(
                        "response.output_item.done",
                        json!({"output_index": 0, "item": item}),
                    ),
                    self.frame("response.completed", json!({"response": r})),
                ]
            }
            Err(e) => {
                let mut r = self.response("failed", None, None);
                r["error"] = error_object(&e);
                vec![self.frame("response.failed", json!({"response": r}))]
            }
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
        let t = tr(json!({"input": "hi", "store": false, "temperature": 0.2})).unwrap();
        assert_eq!(t.model, "faq_agent");
        assert!(!t.stream);
        assert_eq!(t.req.input, vec![Message::text(Role::User, "hi")]);
        let t = tr(json!({
            "model": "gpt-x", "stream": true, "instructions": "be brief", "metadata": {"a": "b"}, "tools": [],
            "input": [
                {"role": "developer", "content": "sys"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "q"}]},
                {"role": "assistant", "content": [{"type": "output_text", "text": "a"}]}
            ]
        }))
        .unwrap();
        assert_eq!(t.model, "gpt-x");
        assert!(t.stream);
        assert_eq!(
            t.req.input,
            vec![
                Message::text(Role::System, "sys"),
                Message::text(Role::User, "q"),
                Message::text(Role::Assistant, "a"),
            ]
        );
        assert_eq!(t.req.instructions.as_deref(), Some("be brief"));
        assert_eq!(t.req.metadata["a"], "b");
        assert_eq!(t.previous, None);
        let id = Uuid::now_v7();
        let t = tr(json!({"input": "x", "previous_response_id": format!("resp_{}", id.simple())}))
            .unwrap();
        assert_eq!(t.previous, Some(id));
        let malformed = [
            "resp_1".to_string(),
            String::new(),
            id.to_string(),
            format!("msg_{}", id.simple()),
            format!("resp_{}", id),
        ];
        for bad in malformed {
            let e = tr(json!({"input": "x", "previous_response_id": &bad})).unwrap_err();
            assert_eq!(e.code(), "response_not_found", "{bad}");
        }
        for bad in [
            json!({"input": "x", "tools": [{"type": "web_search"}]}),
            json!({"input": [{"type": "function_call_output", "call_id": "c", "output": "o"}]}),
            json!({"input": [{"role": "user", "content": [{"type": "input_image", "image_url": "u"}]}]}),
            json!({"input": [{"role": "tool", "content": "x"}]}),
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
        let t = tr(json!({"input": "hi", "metadata": {"a": "b"}})).unwrap();
        let mut enc = ResponsesEncoder::new(&t);
        let sync = enc.complete(&InteractionResult {
            text: "Hello".into(),
            usage: Some(Usage {
                input_tokens: 3,
                output_tokens: 2,
            }),
            ..Default::default()
        });
        assert_eq!(sync["id"], format!("resp_{}", t.req.id.simple()));
        assert_eq!(sync["object"], "response");
        assert_eq!(sync["status"], "completed");
        assert_eq!(sync["model"], "faq_agent");
        assert_eq!(sync["output"][0]["type"], "message");
        assert!(
            sync["output"][0]["id"]
                .as_str()
                .unwrap()
                .starts_with("msg_")
        );
        assert_eq!(
            sync["output"][0]["content"][0],
            json!({"type": "output_text", "text": "Hello", "annotations": []})
        );
        assert_eq!(sync["output_text"], "Hello");
        assert_eq!(
            sync["usage"],
            json!({"input_tokens": 3, "output_tokens": 2, "total_tokens": 5})
        );
        assert_eq!(sync["metadata"], json!({"a": "b"}));
        let frames = run(&mut enc, hello());
        let types: Vec<_> = frames.iter().map(|f| f.event.unwrap()).collect();
        assert_eq!(
            types,
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.json()["type"], f.event.unwrap());
            assert_eq!(f.json()["sequence_number"], i);
        }
        assert_eq!(frames[4].json()["delta"], "Hel");
        assert_eq!(frames[6].json()["text"], "Hello");
        assert_eq!(frames[9].json()["response"]["output_text"], "Hello");
        let f = enc.on(Err(BackendError::Unavailable("x".into())));
        assert_eq!(f[0].event, Some("response.failed"));
        assert_eq!(f[0].json()["sequence_number"], 10);
        assert_eq!(
            f[0].json()["response"]["error"]["code"],
            "interaction_backend_unavailable"
        );
    }
}
