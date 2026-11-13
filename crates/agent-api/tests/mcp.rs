//! The MCP connector (crate::interaction::mcp): a job as a stateless MCP server over Streamable HTTP, JSON responses
//! only. Initialize and version negotiation, notifications, the `ask` tool over a fake backend and the unconfigured one,
//! the JSON-RPC errors, the protocol-version header, the job guard. Real Postgres 18 (one container for this binary).
use crate::common;

use std::sync::{Arc, Mutex};

use agent_api::interaction::{
    BackendError, BackendEvent, InteractionBackend, InteractionRequest, Usage,
};
use axum::http::{HeaderMap, StatusCode};
use common::{TestEnv, insert_tenant, insert_user, json, person};
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// "Hel", "lo", done; remembers the last request.
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

const MCP: &str = "/api/jobs/faq_agent/mcp";

/// POST `body` to `uri` as `token`, with `MCP-Protocol-Version: version` when given: status, headers, the body as
/// JSON (Null when empty).
async fn rpc(
    e: &TestEnv,
    uri: &str,
    token: Option<&str>,
    version: Option<&str>,
    body: Value,
) -> (StatusCode, HeaderMap, Value) {
    let mut req = json("POST", uri, token, body);
    if let Some(v) = version {
        req.headers_mut()
            .insert("mcp-protocol-version", v.parse().unwrap());
    }
    let resp = e.app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, headers, v)
}

/// A member's call on `faq_agent`, expected 200 JSON.
async fn ok(e: &TestEnv, body: Value) -> Value {
    let (s, h, b) = rpc(e, MCP, Some(&person("u-member", &[])), None, body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(
        h["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    assert!(h.get("mcp-session-id").is_none(), "stateless");
    assert_eq!(b["jsonrpc"], "2.0");
    b
}

fn ask(input: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
           "params": {"name": "ask", "arguments": {"input": input, "instructions": "be brief"}}})
}

#[tokio::test]
async fn initialize_negotiates_and_notifications_are_accepted() {
    let e = setup(Some(Arc::new(Hello::default()))).await;
    let b = ok(
        &e,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}),
    )
    .await;
    assert_eq!(b["id"], 1);
    assert_eq!(b["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(
        b["result"]["capabilities"],
        json!({"tools": {"listChanged": false}})
    );
    assert_eq!(b["result"]["serverInfo"]["title"], "Agent job faq_agent");
    let b = ok(
        &e,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05"}}),
    )
    .await;
    assert_eq!(b["result"]["protocolVersion"], "2025-06-18");

    let (s, _, b) = rpc(
        &e,
        MCP,
        Some(&person("u-member", &[])),
        Some("2025-06-18"),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert_eq!((s, b), (StatusCode::ACCEPTED, Value::Null));
    let b = ok(&e, json!({"jsonrpc": "2.0", "id": "p", "method": "ping"})).await;
    assert_eq!(b["result"], json!({}));
}

#[tokio::test]
async fn tools_list_and_call() {
    let hello = Arc::new(Hello::default());
    let e = setup(Some(hello.clone())).await;
    let b = ok(
        &e,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;
    let tools = b["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "ask");
    assert!(
        tools[0]["description"]
            .as_str()
            .unwrap()
            .contains("faq_agent")
    );

    let b = ok(&e, ask("hi")).await;
    assert_eq!(b["id"], 2);
    assert_eq!(
        b["result"],
        json!({"content": [{"type": "text", "text": "Hello"}], "isError": false})
    );
    let seen = hello.last.lock().unwrap().clone().unwrap();
    assert_eq!(seen.channel, "mcp");
    assert_eq!(seen.job_key, "faq_agent");
    assert_eq!(seen.instructions.as_deref(), Some("be brief"));
    assert_eq!(seen.input.len(), 1);
    assert_eq!(seen.input[0].joined(), "hi");
}

#[tokio::test]
async fn unconfigured_backend_is_a_tool_error() {
    let e = setup(None).await;
    let b = ok(&e, ask("hi")).await;
    assert_eq!(b["result"]["isError"], true, "{b}");
    let text = b["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("provider_not_configured: "), "{text}");
}

#[tokio::test]
async fn json_rpc_errors() {
    let e = setup(Some(Arc::new(Hello::default()))).await;
    let b = ok(
        &e,
        json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"}),
    )
    .await;
    assert_eq!(
        (b["id"].clone(), b["error"]["code"].clone()),
        (json!(3), json!(-32601))
    );
    let b = ok(
        &e,
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "nope", "arguments": {"input": "hi"}}}),
    )
    .await;
    assert_eq!(b["error"]["code"], -32602);
    let b = ok(
        &e,
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "ask", "arguments": {"input": "  "}}}),
    )
    .await;
    assert_eq!(b["error"]["code"], -32602);
    assert_eq!(b["error"]["data"]["code"], "empty_input");
    let b = ok(&e, json!([{"jsonrpc": "2.0", "id": 6, "method": "ping"}])).await;
    assert_eq!(
        (b["id"].clone(), b["error"]["code"].clone()),
        (Value::Null, json!(-32600))
    );

    let (s, _, b) = rpc(
        &e,
        MCP,
        Some(&person("u-member", &[])),
        Some("1999-01-01"),
        json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], -32600);
}

#[tokio::test]
async fn auth_and_job_scope() {
    let e = setup(Some(Arc::new(Hello::default()))).await;
    let ping = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
    let (s, _, b) = rpc(&e, MCP, None, None, ping.clone()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{b}");
    for uri in ["/api/jobs/other_job/mcp", "/api/jobs/no_such_job/mcp"] {
        let (s, _, b) = rpc(&e, uri, Some(&person("u-member", &[])), None, ping.clone()).await;
        assert_eq!(
            (s, b["error"]["code"].as_str()),
            (StatusCode::NOT_FOUND, Some("job_not_found")),
            "{uri}: {b}"
        );
    }
}
