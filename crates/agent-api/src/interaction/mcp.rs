//! The MCP connector: every job is a stateless MCP server (Streamable HTTP transport, JSON responses only: no SSE
//! stream, no `Mcp-Session-Id`) with one tool, `ask`, which hands its `input` to the job through the interaction API
//! (channel `mcp`) and returns the job's answer as text. Other agents (Claude Code / Desktop, n8n's MCP client, any MCP
//! host) hand work to a job this way.
//!
//! - `POST /api/jobs/{job}/mcp` — `JobCaller`, like crate::interaction::routes (401 without a token or key, 404
//!   `job_not_found` for another tenant's job, as the normal error JSON). One JSON-RPC 2.0 message per POST.
//!
//! A request is answered 200 `application/json`. A notification or a client response is 202 with no body. A body
//! that is not JSON is 200 with a -32700 error (`id` null); a batch, or a message that is not a JSON-RPC 2.0 request,
//! is -32600; an unknown method -32601; an unknown tool or bad arguments -32602 (an input-limit refusal carries its
//! code in `error.data.code`). A backend that cannot answer is a tool result with `isError: true` (`<code>: <public
//! message>`). An `MCP-Protocol-Version` header naming an unsupported version is 400 with a -32600 body. Other verbs
//! fall to the router's 405 (no SSE stream is offered on GET).
//!
//! The input and the answer are customer text: never logged.
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::post;
use serde_json::{Value, json};

use super::apikeys::JobCaller;
use super::backend::collect;
use super::model::{BackendError, InteractionRequest, InteractionResult, Message, Role, Scope};
use crate::state::AppState;

/// The protocol versions served, newest (the default) first.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26"];
/// The header a client names its negotiated protocol version in.
pub const VERSION_HEADER: &str = "mcp-protocol-version";
/// The interaction channel of an MCP call.
pub const CHANNEL: &str = "mcp";
/// The one tool.
pub const TOOL: &str = "ask";

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/jobs/{job}/mcp", post(mcp))
}

async fn mcp(
    j: JobCaller,
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(v) = headers.get(VERSION_HEADER) {
        let supported = v
            .to_str()
            .is_ok_and(|s| PROTOCOL_VERSIONS.contains(&s.trim()));
        if !supported {
            let e = error(
                Value::Null,
                INVALID_REQUEST,
                "unsupported MCP-Protocol-Version",
                None,
            );
            return (StatusCode::BAD_REQUEST, Json(e)).into_response();
        }
    }
    match dispatch(&body, &Scope::from(&j)) {
        Step::Accepted => StatusCode::ACCEPTED.into_response(),
        Step::Reply(v) => Json(v).into_response(),
        Step::Ask { id, mut req } => {
            j.stamp(&mut req);
            let r = collect(super::respond(&st, *req)).await;
            Json(result(id, tool_result(r))).into_response()
        }
    }
}

/// What one incoming message calls for.
#[derive(Debug, PartialEq)]
pub enum Step {
    /// A notification or a client response: 202, no body.
    Accepted,
    /// A JSON-RPC response, ready to send.
    Reply(Value),
    /// `tools/call` of `ask`: a validated request for the backend; answer `id` with [`tool_result`].
    Ask {
        id: Value,
        req: Box<InteractionRequest>,
    },
}

/// One JSON-RPC message (the raw body) addressed to the job of `scope`, everything but the backend call.
pub fn dispatch(body: &[u8], scope: &Scope) -> Step {
    let Ok(msg) = serde_json::from_slice::<Value>(body) else {
        return Step::Reply(error(
            Value::Null,
            PARSE_ERROR,
            "parse error: the body is not JSON",
            None,
        ));
    };
    let m = match msg {
        Value::Object(m) => m,
        Value::Array(_) => {
            return Step::Reply(error(
                Value::Null,
                INVALID_REQUEST,
                "batches are not supported",
                None,
            ));
        }
        _ => {
            return Step::Reply(error(
                Value::Null,
                INVALID_REQUEST,
                "a JSON-RPC message is an object",
                None,
            ));
        }
    };
    let id = match m.get("id") {
        Some(v) if v.is_string() || v.is_number() => Some(v.clone()),
        Some(_) => {
            return Step::Reply(error(
                Value::Null,
                INVALID_REQUEST,
                "id must be a string or a number",
                None,
            ));
        }
        None => None,
    };
    let echo = id.clone().unwrap_or(Value::Null);
    if m.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Step::Reply(error(
            echo,
            INVALID_REQUEST,
            "jsonrpc must be \"2.0\"",
            None,
        ));
    }
    let Some(method) = m.get("method") else {
        if id.is_some() && (m.contains_key("result") || m.contains_key("error")) {
            return Step::Accepted;
        }
        return Step::Reply(error(
            echo,
            INVALID_REQUEST,
            "a request names a method",
            None,
        ));
    };
    let Some(method) = method.as_str() else {
        return Step::Reply(error(
            echo,
            INVALID_REQUEST,
            "method must be a string",
            None,
        ));
    };
    let Some(id) = id else {
        return Step::Accepted;
    };
    let params = m.get("params").unwrap_or(&Value::Null);
    match method {
        "initialize" => Step::Reply(result(id, initialize(params, scope))),
        "ping" => Step::Reply(result(id, json!({}))),
        "tools/list" => Step::Reply(result(id, json!({"tools": [tool(scope)]}))),
        "tools/call" => call(id, params, scope),
        _ => Step::Reply(error(id, METHOD_NOT_FOUND, "method not found", None)),
    }
}

/// The `initialize` result: the client's protocol version when served, else the newest.
fn initialize(params: &Value, scope: &Scope) -> Value {
    let version = params["protocolVersion"]
        .as_str()
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {
            "name": "agent",
            "title": format!("Agent job {}", scope.job_key),
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": format!(
            "This server is the agent platform job `{}`. Call the `ask` tool with a request in `input`; the job's \
             answer comes back as text. Every call stands alone: no conversation is kept between calls.",
            scope.job_key
        ),
    })
}

/// The `ask` tool definition.
fn tool(scope: &Scope) -> Value {
    json!({
        "name": TOOL,
        "title": format!("Ask {}", scope.job_key),
        "description": format!(
            "Sends a request to the job `{}` of the agent platform and returns its answer as text.",
            scope.job_key
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "input": {"type": "string", "description": "The request for the job, in plain language."},
                "instructions": {"type": "string", "description": "Optional guidance on how to answer (ignored for an API key caller: the job's own instructions apply)."},
            },
            "required": ["input"],
            "additionalProperties": false,
        },
    })
}

/// `tools/call`: `ask` with a string `input` (and optionally string `instructions`, nothing else), within the input
/// limits; else -32602.
fn call(id: Value, params: &Value, scope: &Scope) -> Step {
    let bad = |id: Value, msg: &str| Step::Reply(error(id, INVALID_PARAMS, msg, None));
    if params["name"].as_str() != Some(TOOL) {
        return bad(id, "unknown tool");
    }
    let args = &params["arguments"];
    let Some(input) = args["input"].as_str() else {
        return bad(id, "arguments.input must be a string");
    };
    let instructions = match &args["instructions"] {
        Value::Null => None,
        Value::String(s) => Some(s.clone()).filter(|s| !s.trim().is_empty()),
        _ => return bad(id, "arguments.instructions must be a string"),
    };
    if let Some(a) = args.as_object()
        && a.keys().any(|k| k != "input" && k != "instructions")
    {
        return bad(id, "arguments carries only input and instructions");
    }
    let mut req = InteractionRequest::new(scope, vec![Message::text(Role::User, input)]);
    req.channel = CHANNEL.into();
    req.instructions = instructions;
    if let Err(e) = req.validate() {
        let data = json!({"code": e.code()});
        return Step::Reply(error(id, INVALID_PARAMS, &e.to_string(), Some(data)));
    }
    Step::Ask {
        id,
        req: Box::new(req),
    }
}

/// The `tools/call` result of a backend answer: the text, or `<code>: <public message>` with `isError: true`.
pub fn tool_result(r: Result<InteractionResult, BackendError>) -> Value {
    let (text, is_error) = match r {
        Ok(r) => (r.text, false),
        Err(e) => (format!("{}: {}", e.code(), e.public_message()), true),
    };
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut e = json!({"code": code, "message": message});
    if let Some(d) = data {
        e["data"] = d;
    }
    json!({"jsonrpc": "2.0", "id": id, "error": e})
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn scope() -> Scope {
        Scope {
            tenant: Uuid::nil(),
            job_id: Uuid::nil(),
            job_key: "faq".into(),
        }
    }

    fn run(v: Value) -> Step {
        dispatch(v.to_string().as_bytes(), &scope())
    }

    fn reply(v: Value) -> Value {
        match run(v) {
            Step::Reply(r) => r,
            other => panic!("{other:?}"),
        }
    }

    fn code(r: &Value) -> i64 {
        r["error"]["code"].as_i64().unwrap()
    }

    fn ask(args: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "ask", "arguments": args}})
    }

    #[test]
    fn malformed_messages() {
        let r = match dispatch(b"{not json", &scope()) {
            Step::Reply(r) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!((code(&r), &r["id"]), (PARSE_ERROR, &Value::Null));
        assert_eq!(r["jsonrpc"], "2.0");
        let batch = reply(json!([{"jsonrpc": "2.0", "id": 1, "method": "ping"}]));
        assert_eq!(code(&batch), INVALID_REQUEST);
        assert_eq!(code(&reply(json!("ping"))), INVALID_REQUEST);
        let r = reply(json!({"id": 1, "method": "ping"}));
        assert_eq!((code(&r), &r["id"]), (INVALID_REQUEST, &json!(1)));
        let r = reply(json!({"jsonrpc": "2.0", "id": null, "method": "ping"}));
        assert_eq!((code(&r), &r["id"]), (INVALID_REQUEST, &Value::Null));
        let r = reply(json!({"jsonrpc": "2.0", "id": "a", "method": 3}));
        assert_eq!((code(&r), &r["id"]), (INVALID_REQUEST, &json!("a")));
        assert_eq!(
            code(&reply(json!({"jsonrpc": "2.0", "id": 1}))),
            INVALID_REQUEST
        );
    }

    #[test]
    fn notifications_and_responses_are_accepted() {
        assert_eq!(
            run(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
            Step::Accepted
        );
        assert_eq!(
            run(json!({"jsonrpc": "2.0", "method": "notifications/unknown", "params": {}})),
            Step::Accepted
        );
        assert_eq!(
            run(json!({"jsonrpc": "2.0", "id": 3, "result": {}})),
            Step::Accepted
        );
        assert_eq!(
            run(json!({"jsonrpc": "2.0", "id": 3, "error": {"code": -1, "message": "x"}})),
            Step::Accepted
        );
    }

    #[test]
    fn initialize_negotiates_the_version() {
        let init = |v: &str| {
            reply(json!({"jsonrpc": "2.0", "id": "i", "method": "initialize",
                         "params": {"protocolVersion": v, "capabilities": {}, "clientInfo": {"name": "c", "version": "1"}}}))
        };
        let r = init("2025-03-26");
        assert_eq!(r["id"], "i");
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(
            r["result"]["capabilities"],
            json!({"tools": {"listChanged": false}})
        );
        assert_eq!(r["result"]["serverInfo"]["name"], "agent");
        assert_eq!(r["result"]["serverInfo"]["title"], "Agent job faq");
        assert_eq!(
            r["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert!(
            r["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("ask")
        );
        assert_eq!(
            init("1999-01-01")["result"]["protocolVersion"],
            "2025-06-18"
        );
        let r = reply(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}));
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
    }

    #[test]
    fn ping_list_and_unknown_methods() {
        let r = reply(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}));
        assert_eq!(r, json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
        let r = reply(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
        let tools = r["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "ask");
        assert!(tools[0]["description"].as_str().unwrap().contains("`faq`"));
        assert_eq!(tools[0]["inputSchema"]["required"], json!(["input"]));
        assert_eq!(tools[0]["inputSchema"]["additionalProperties"], false);
        let r = reply(json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"}));
        assert_eq!((code(&r), &r["id"]), (METHOD_NOT_FOUND, &json!(3)));
    }

    #[test]
    fn ask_builds_a_validated_request() {
        let Step::Ask { id, req } = run(ask(json!({"input": "hi", "instructions": "be brief"})))
        else {
            panic!("not an ask")
        };
        assert_eq!(id, json!(7));
        assert_eq!(req.channel, "mcp");
        assert_eq!(req.job_key, "faq");
        assert_eq!(req.instructions.as_deref(), Some("be brief"));
        assert_eq!(req.input, vec![Message::text(Role::User, "hi")]);
        assert!(req.party.is_none());
        let Step::Ask { req, .. } = run(ask(json!({"input": "hi", "instructions": " "}))) else {
            panic!("not an ask")
        };
        assert!(req.instructions.is_none(), "blank instructions are none");
    }

    #[test]
    fn ask_refusals_are_invalid_params() {
        for args in [
            json!({}),
            json!({"input": 3}),
            json!({"input": "hi", "instructions": 1}),
            json!({"input": "hi", "extra": true}),
        ] {
            let r = reply(ask(args.clone()));
            assert_eq!((code(&r), &r["id"]), (INVALID_PARAMS, &json!(7)), "{args}");
        }
        let r = reply(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                             "params": {"name": "other", "arguments": {"input": "hi"}}}));
        assert_eq!(code(&r), INVALID_PARAMS);
        let r = reply(ask(json!({"input": "   "})));
        assert_eq!(code(&r), INVALID_PARAMS);
        assert_eq!(r["error"]["data"]["code"], "empty_input");
        let r = reply(ask(json!({"input": "x".repeat(32_001)})));
        assert_eq!(r["error"]["data"]["code"], "input_too_large");
    }

    #[test]
    fn tool_results() {
        let ok = tool_result(Ok(InteractionResult {
            text: "Hello".into(),
            usage: None,
            ..Default::default()
        }));
        assert_eq!(
            ok,
            json!({"content": [{"type": "text", "text": "Hello"}], "isError": false})
        );
        let err = tool_result(Err(BackendError::NotConfigured("operator detail".into())));
        assert_eq!(err["isError"], true);
        let text = err["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("provider_not_configured: "), "{text}");
        assert!(!text.contains("operator detail"));
    }
}
