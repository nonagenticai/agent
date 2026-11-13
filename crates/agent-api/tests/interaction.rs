//! The interaction API (crate::interaction): the native, OpenAI Responses and OpenAI chat-completions shapes, sync and
//! streamed, over a fake backend; the job guard; the input limits; the unconfigured backend. Real Postgres 18 (one
//! container for this binary).
use crate::common;

use std::sync::{Arc, Mutex};

use agent_api::interaction::{
    BackendError, BackendEvent, InteractionBackend, InteractionRequest, Usage,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{TestEnv, insert_tenant, insert_user, json, person, send};
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
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

/// One delta, then the upstream fails.
struct Breaks;

impl InteractionBackend for Breaks {
    fn respond(
        &self,
        _req: InteractionRequest,
    ) -> BoxStream<'static, Result<BackendEvent, BackendError>> {
        stream::iter(vec![
            Ok(BackendEvent::Delta("Hel".into())),
            Err(BackendError::Upstream("status 500".into())),
        ])
        .boxed()
    }
}

/// An env whose interaction backend is `backend` (None = as configured: unconfigured), a tenant with an observer
/// `u-member` and a job `faq_agent`, and another tenant with a job `other_job`.
async fn setup(backend: Option<Arc<dyn InteractionBackend>>) -> TestEnv {
    let mut e = common::env().await;
    if let Some(b) = backend {
        e.state.interaction = b;
        e.app = agent_api::router(e.state.clone());
    }
    let t = Uuid::new_v4();
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

fn member() -> String {
    person("u-member", &[])
}

fn post(uri: &str, body: Value) -> Request<Body> {
    json("POST", uri, Some(&member()), body)
}

/// Status, content type and the raw body.
async fn raw(e: &TestEnv, req: Request<Body>) -> (StatusCode, String, String) {
    let resp = e.app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, ct, String::from_utf8(bytes.to_vec()).unwrap())
}

/// Server-sent events as (event name or "", data).
fn events(body: &str) -> Vec<(String, String)> {
    body.split("\n\n")
        .filter(|b| !b.trim().is_empty())
        .filter_map(|block| {
            let mut name = String::new();
            let mut data = Vec::new();
            for line in block.lines() {
                if let Some(n) = line.strip_prefix("event:") {
                    name = n.trim().to_string();
                } else if let Some(d) = line.strip_prefix("data:") {
                    data.push(d.trim_start().to_string());
                }
            }
            (!data.is_empty()).then(|| (name, data.join("\n")))
        })
        .collect()
}

async fn stream_of(e: &TestEnv, uri: &str, body: Value) -> Vec<(String, String)> {
    let (s, ct, text) = raw(e, post(uri, body)).await;
    assert_eq!(s, StatusCode::OK, "{text}");
    assert!(ct.starts_with("text/event-stream"), "{ct}");
    events(&text)
}

fn parse(d: &str) -> Value {
    serde_json::from_str(d).unwrap()
}

const NATIVE: &str = "/api/jobs/faq_agent/interactions";
const RESPONSES: &str = "/api/jobs/faq_agent/responses";
const CHAT: &str = "/api/jobs/faq_agent/chat/completions";

#[tokio::test]
async fn native_sync_and_stream() {
    let hello = Arc::new(Hello::default());
    let e = setup(Some(hello.clone())).await;
    let (s, b) = send(
        &e.app,
        post(
            NATIVE,
            json!({"input": "hi", "channel": "telegram", "metadata": {"k": "v"}}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["id"].as_str().unwrap().starts_with("int_"), "{b}");
    assert_eq!(b["object"], "interaction");
    assert_eq!(b["status"], "completed");
    assert_eq!(b["job"], "faq_agent");
    assert_eq!(b["channel"], "telegram");
    assert!(b["created_at"].as_str().unwrap().contains('T'));
    assert_eq!(
        b["output"],
        json!([{"role": "assistant", "content": [{"type": "text", "text": "Hello"}]}])
    );
    assert_eq!(b["output_text"], "Hello");
    assert_eq!(b["usage"], json!({"input_tokens": 3, "output_tokens": 2}));
    let seen = hello.last.lock().unwrap().clone().unwrap();
    assert_eq!(seen.job_key, "faq_agent");
    assert!(
        seen.party.is_none(),
        "a member without a party: the staff playground"
    );
    assert_eq!(seen.metadata["k"], "v");
    assert_eq!(seen.id.get_version_num(), 7);
    // D109: a body `party` is a customer — a runtime turn, never the playground (this job has no live release)
    let (s, b) = send(
        &e.app,
        post(NATIVE, json!({"input": "hi", "party": {"id": "tg:42"}})),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("job_not_live")),
        "{b}"
    );
    assert_eq!(
        hello.last.lock().unwrap().clone().unwrap().id,
        seen.id,
        "the playground did not see the customer"
    );

    let ev = stream_of(&e, NATIVE, json!({"input": "hi", "stream": true})).await;
    let names: Vec<&str> = ev.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "interaction.created",
            "output_text.delta",
            "output_text.delta",
            "interaction.completed"
        ]
    );
    assert_eq!(parse(&ev[0].1)["status"], "in_progress");
    assert_eq!(parse(&ev[1].1), json!({"delta": "Hel"}));
    assert_eq!(parse(&ev[3].1)["output_text"], "Hello");
    assert_eq!(parse(&ev[3].1)["id"], parse(&ev[0].1)["id"]);
}

#[tokio::test]
async fn native_stream_failure_after_start_is_an_error_event() {
    let e = setup(Some(Arc::new(Breaks))).await;
    let ev = stream_of(&e, NATIVE, json!({"input": "hi", "stream": true})).await;
    let names: Vec<&str> = ev.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["interaction.created", "output_text.delta", "error"]);
    assert_eq!(
        parse(&ev[2].1)["error"]["code"],
        "interaction_upstream_error"
    );
    // sync: the whole answer fails as JSON
    let (s, b) = send(&e.app, post(NATIVE, json!({"input": "hi"}))).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::BAD_GATEWAY, Some("interaction_upstream_error"))
    );
}

#[tokio::test]
async fn responses_sync_and_stream() {
    let hello = Arc::new(Hello::default());
    let e = setup(Some(hello.clone())).await;
    let mut req = post(
        RESPONSES,
        json!({"model": "gpt-4.1", "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
               "instructions": "be brief", "store": true, "metadata": {"a": "b"}}),
    );
    req.headers_mut()
        .insert("x-interaction-channel", "voice".parse().unwrap());
    let (s, b) = send(&e.app, req).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["id"].as_str().unwrap().starts_with("resp_"));
    assert_eq!(b["object"], "response");
    assert_eq!(b["status"], "completed");
    assert_eq!(b["model"], "gpt-4.1");
    assert!(b["created_at"].is_i64());
    assert_eq!(b["output"][0]["type"], "message");
    assert_eq!(b["output"][0]["role"], "assistant");
    assert_eq!(
        b["output"][0]["content"],
        json!([{"type": "output_text", "text": "Hello", "annotations": []}])
    );
    assert_eq!(b["output_text"], "Hello");
    assert_eq!(
        b["usage"],
        json!({"input_tokens": 3, "output_tokens": 2, "total_tokens": 5})
    );
    assert_eq!(b["metadata"], json!({"a": "b"}));
    let seen = hello.last.lock().unwrap().clone().unwrap();
    assert_eq!(seen.channel, "voice");
    assert_eq!(seen.instructions.as_deref(), Some("be brief"));

    let ev = stream_of(&e, RESPONSES, json!({"input": "hi", "stream": true})).await;
    let types: Vec<&str> = ev.iter().map(|(n, _)| n.as_str()).collect();
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
    let mut last = -1i64;
    for (name, data) in &ev {
        let v = parse(data);
        assert_eq!(v["type"], name.as_str());
        let n = v["sequence_number"].as_i64().unwrap();
        assert!(n > last, "sequence_number increases: {n} after {last}");
        last = n;
    }
    assert_eq!(parse(&ev[6].1)["text"], "Hello");
    let done = parse(&ev[9].1);
    assert_eq!(done["response"]["output_text"], "Hello");
    assert_eq!(
        done["response"]["model"], "faq_agent",
        "model defaults to the job key"
    );
}

#[tokio::test]
async fn responses_refuses_state_and_tools() {
    let e = setup(Some(Arc::new(Hello::default()))).await;
    for body in [
        // the staff playground stores no conversation to continue
        json!({"input": "hi", "previous_response_id": format!("resp_{}", Uuid::now_v7().simple())}),
        json!({"input": "hi", "tools": [{"type": "function", "name": "f"}]}),
    ] {
        let (s, b) = send(&e.app, post(RESPONSES, body)).await;
        assert_eq!(
            (s, b["error"]["code"].as_str()),
            (StatusCode::BAD_REQUEST, Some("unsupported_parameter")),
            "{b}"
        );
    }
    let mut req = post(RESPONSES, json!({"input": "hi"}));
    req.headers_mut()
        .insert("x-interaction-channel", "Not-A-Channel".parse().unwrap());
    let (s, b) = send(&e.app, req).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid_channel"))
    );
}

#[tokio::test]
async fn chat_completions_sync_and_stream() {
    let hello = Arc::new(Hello::default());
    let e = setup(Some(hello.clone())).await;
    let (s, b) = send(
        &e.app,
        post(
            CHAT,
            json!({"model": "m", "messages": [{"role": "system", "content": "be brief"}, {"role": "user", "content": "hi"}]}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["id"].as_str().unwrap().starts_with("chatcmpl-"));
    assert_eq!(b["object"], "chat.completion");
    assert_eq!(b["model"], "m");
    assert_eq!(
        b["choices"],
        json!([{"index": 0, "message": {"role": "assistant", "content": "Hello"}, "finish_reason": "stop"}])
    );
    assert_eq!(
        b["usage"],
        json!({"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5})
    );
    let seen = hello.last.lock().unwrap().clone().unwrap();
    assert_eq!(seen.instructions.as_deref(), Some("be brief"));
    assert_eq!(seen.input.len(), 1);

    let ev = stream_of(
        &e,
        CHAT,
        json!({"messages": [{"role": "user", "content": "hi"}], "stream": true}),
    )
    .await;
    assert!(ev.iter().all(|(n, _)| n.is_empty()), "data-only lines");
    assert_eq!(ev.last().unwrap().1, "[DONE]");
    let chunks: Vec<Value> = ev[..ev.len() - 1].iter().map(|(_, d)| parse(d)).collect();
    assert_eq!(
        chunks[0]["choices"][0]["delta"],
        json!({"role": "assistant"})
    );
    let text: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "Hello");
    let last = chunks.last().unwrap();
    assert_eq!(last["choices"][0]["finish_reason"], "stop");
    assert!(
        chunks
            .iter()
            .all(|c| c["object"] == "chat.completion.chunk")
    );
}

#[tokio::test]
async fn auth_and_job_scope() {
    let e = setup(Some(Arc::new(Hello::default()))).await;
    for uri in [NATIVE, RESPONSES, CHAT] {
        let (s, b) = send(&e.app, json("POST", uri, None, json!({"input": "hi"}))).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{uri}: {b}");
    }
    for uri in [
        "/api/jobs/other_job/interactions",
        "/api/jobs/no_such_job/responses",
        "/api/jobs/other_job/chat/completions",
    ] {
        let (s, b) = send(&e.app, post(uri, json!({"input": "hi", "messages": []}))).await;
        assert_eq!(
            (s, b["error"]["code"].as_str()),
            (StatusCode::NOT_FOUND, Some("job_not_found")),
            "{uri}: {b}"
        );
    }
}

#[tokio::test]
async fn input_limits() {
    let e = setup(Some(Arc::new(Hello::default()))).await;
    let cases = [
        (
            json!({"input": "x".repeat(32_001)}),
            StatusCode::PAYLOAD_TOO_LARGE,
            "input_too_large",
        ),
        (
            json!({"input": []}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "empty_input",
        ),
        (
            json!({"input": "   "}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "empty_input",
        ),
        (
            json!({"input": vec![json!({"role": "user", "content": "x"}); 101]}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "too_many_messages",
        ),
        (
            json!({"input": "hi", "channel": "Bad Channel"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_channel",
        ),
        (
            json!({"input": "hi", "metadata": {"k": "v".repeat(513)}}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_metadata",
        ),
    ];
    for (body, status, code) in cases {
        let (s, b) = send(&e.app, post(NATIVE, body)).await;
        assert_eq!(
            (s, b["error"]["code"].as_str()),
            (status, Some(code)),
            "{b}"
        );
    }
    let (s, b) = send(
        &e.app,
        post(
            CHAT,
            json!({"messages": [{"role": "user", "content": "x".repeat(32_001)}]}),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::PAYLOAD_TOO_LARGE, Some("input_too_large"))
    );
    let (s, b) = send(&e.app, post(RESPONSES, json!({"input": ""}))).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("empty_input"))
    );
}

#[tokio::test]
async fn unconfigured_backend_is_409_provider_not_configured_sync_and_before_the_stream() {
    let e = setup(None).await;
    for (uri, body) in [
        (NATIVE, json!({"input": "hi"})),
        (NATIVE, json!({"input": "hi", "stream": true})),
        (RESPONSES, json!({"input": "hi", "stream": true})),
        (
            CHAT,
            json!({"messages": [{"role": "user", "content": "hi"}]}),
        ),
    ] {
        let (s, b) = send(&e.app, post(uri, body)).await;
        assert_eq!(
            (s, b["error"]["code"].as_str()),
            (StatusCode::CONFLICT, Some("provider_not_configured")),
            "{uri}: {b}"
        );
    }
}
