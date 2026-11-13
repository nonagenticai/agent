//! B1 + B4 "Done when" (real stack): the tools of an EXTERNAL MCP system go through the declared-input model, and a
//! tenant IAM procedure can be built from MCP tools.
//!
//! The ops of the world are served by a published platform template with the `mcp` transport and an `api_key_header`
//! credential, behind a real tenant instance whose `base_url` is a fake MCP server on loopback (the only fake besides
//! the model). Each op names its MCP tool in `path`; `request` renders the tool's arguments from the op's declared
//! inputs, under the tool's OWN argument names (`code`, `token`, `customer_id`), so the assertions can see exactly
//! where the state-filled values land. The instance's key is the platform's credential, sent as a header on every
//! request of the session.
use std::sync::{Arc, Mutex};

use agent_api::runtime::Env;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::IntoResponse;
use serde_json::{Value, json};

use super::world::*;
use crate::common::conv::code;
use crate::common::{json as req, send, super_admin};

const OPDEFS: &str = include_str!("../../contracts/opdefs.json");
const TEMPLATE: &str = "iam_mcp";
const INSTANCE: &str = "mcp-test";
/// The instance's key and the header it travels in.
const MCP_KEY: &str = "mcp-key-7731-secret";
const KEY_HEADER: &str = "x-service-key";

/// `(op, MCP tool, [(tool argument, canonical input rendered into it)])`.
const TOOLS: [(&str, &str, &[(&str, &str)]); 5] = [
    (VERIFY, "verify_code", &[("code", "msisdn")]),
    (
        LOOKUP,
        "find_customer",
        &[("email", "email"), ("token", "order_id")],
    ),
    (READ, "list_orders", &[("customer_id", "party_id")]),
    (
        BOARD,
        "add_comment",
        &[("case", "case_id"), ("text", "comment")],
    ),
    (
        CANCEL,
        "cancel_order",
        &[("order", "order_id"), ("why", "reason")],
    ),
];

/// The chain in the order the runtime must call it.
const CHAIN: [&str; 3] = ["verify_code", "find_customer", "list_orders"];

// ---- the fake external MCP server ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct ToolCall {
    tool: String,
    args: Value,
    key: Option<String>,
}

#[derive(Clone, Default)]
struct Seen {
    calls: Arc<Mutex<Vec<ToolCall>>>,
    /// the key header of every POST (initialize, notifications, tools/call)
    posts: Arc<Mutex<Vec<Option<String>>>>,
}

struct ExtMcp {
    url: String,
    seen: Seen,
}

impl ExtMcp {
    fn calls(&self) -> Vec<ToolCall> {
        self.seen.calls.lock().unwrap().clone()
    }

    fn names(&self) -> Vec<String> {
        self.calls().into_iter().map(|c| c.tool).collect()
    }

    fn posts(&self) -> Vec<Option<String>> {
        self.seen.posts.lock().unwrap().clone()
    }
}

/// The server's tools, with the same business rules as the REST `System`: the code is the credential of the verifier,
/// the token plus the email identify the customer, only the customer id opens the orders.
fn serve_tool(tool: &str, a: &Value) -> Result<Value, String> {
    let is = |k: &str, v: &str| a[k].as_str() == Some(v);
    match tool {
        "verify_code" if is("code", CODE) => Ok(json!({"porting_id": TOKEN, "direction": "in",
            "status": "verified", "porting_date": "2026-11-01", "rejection_reason": ""})),
        "verify_code" => Err("the code is wrong".into()),
        "find_customer" if is("token", TOKEN) && is("email", CUSTOMER_EMAIL) => {
            Ok(json!({"party_id": CUSTOMER, "roles": "customer"}))
        }
        "find_customer" => Err("unknown token".into()),
        "list_orders" if is("customer_id", CUSTOMER) => Ok(json!({"order_id": "ORD-1",
            "status": "shipped", "placed_at": "2026-10-01", "total": "19.90", "items": "kettle"})),
        "list_orders" => Err("not found".into()),
        "add_comment" => Ok(json!({"status": "commented"})),
        "cancel_order" => Ok(json!({"status": "cancelled"})),
        other => Err(format!("no tool {other}")),
    }
}

async fn start_mcp() -> ExtMcp {
    let seen = Seen::default();
    let state = seen.clone();
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::any(move |method: Method, headers: HeaderMap, body: String| {
            let seen = state.clone();
            async move {
                if method != Method::POST {
                    return StatusCode::METHOD_NOT_ALLOWED.into_response();
                }
                let rpc: Value = serde_json::from_str(&body).unwrap();
                let m = rpc["method"].as_str().unwrap_or("").to_owned();
                let key = headers
                    .get(KEY_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                seen.posts.lock().unwrap().push(key.clone());
                if key.as_deref() != Some(MCP_KEY) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                let id = rpc["id"].clone();
                let reply = match m.as_str() {
                    "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": {
                        "protocolVersion": rpc["params"]["protocolVersion"],
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "fake-iam-mcp", "version": "0"}}}),
                    "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": []}}),
                    "tools/call" => {
                        let tool = rpc["params"]["name"].as_str().unwrap_or("").to_owned();
                        let args = rpc["params"]["arguments"].clone();
                        seen.calls.lock().unwrap().push(ToolCall {
                            tool: tool.clone(),
                            args: args.clone(),
                            key,
                        });
                        match serve_tool(&tool, &args) {
                            Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": {
                                "content": [{"type": "text", "text": v.to_string()}],
                                "structuredContent": v, "isError": false}}),
                            Err(e) => json!({"jsonrpc": "2.0", "id": id, "result": {
                                "content": [{"type": "text", "text": e}], "isError": true}}),
                        }
                    }
                    _ => return StatusCode::ACCEPTED.into_response(),
                };
                let mut resp = axum::Json(reply).into_response();
                if m == "initialize" {
                    resp.headers_mut()
                        .insert("mcp-session-id", "sess-iam".parse().unwrap());
                }
                resp
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    ExtMcp {
        url: format!("http://127.0.0.1:{port}/mcp"),
        seen,
    }
}

// ---- the MCP template -------------------------------------------------------------------------------------------------------------

/// The template op of `key` served by `tool`: POST `/<tool>`, the arguments rendered from the op's declared inputs,
/// every output field mapped one to one from the tool's structured content.
fn template_op(key: &str, tool: &str, args: &[(&str, &str)]) -> Value {
    let (object, verb) = key.split_once('.').unwrap();
    let defs: Vec<Value> = serde_json::from_str(OPDEFS).unwrap();
    let def = defs
        .into_iter()
        .find(|d| d["object"] == object && d["verb"] == verb && d["variant"] == "*")
        .unwrap_or_else(|| panic!("{key} is not in contracts/opdefs.json"));
    let side = def["side_effect"].as_str().unwrap().to_owned();
    let outputs: Vec<String> = def["output_fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f.as_str().map(str::to_owned))
        .collect();
    let request = format!(
        "{{{}}}",
        args.iter()
            .map(|(a, c)| format!("\"{a}\":\"{{{{canonical.{c}}}}}\""))
            .collect::<Vec<_>>()
            .join(",")
    );
    let body: serde_json::Map<String, Value> =
        outputs.iter().map(|f| (f.clone(), json!("test"))).collect();
    let idempotency = if side == "R" {
        json!({"kind": "nat"})
    } else {
        json!({"kind": "dup_search", "on": "case ref"})
    };
    json!({
        "object": object, "verb": verb, "variant": "*", "side_effect": side, "op_kind": "standard",
        "method": "POST", "path": format!("/{tool}"), "request": request,
        "field_map": outputs.iter().map(|f| json!({"path": format!("$.{f}"), "field": f})).collect::<Vec<_>>(),
        "status_map": [],
        "fault_map": [
            {"code": "timeout", "system": "fault=timeout"},
            {"code": "upstream_unavailable", "system": "fault=5xx"},
            {"code": "not_found", "system": "HTTP 404"},
            {"code": "connector_auth_failed", "system": "HTTP 401"}
        ],
        "pagination": null, "idempotency": idempotency, "verify_read": null, "timeout_ms": 3000,
        // what the MCP tool's `inputSchema` declares of each input (0243): the model's tool spec carries it
        "input_types": args.iter()
            .map(|(_, c)| ((*c).to_owned(), json!({"type": "string", "description": format!("typed {c}")})))
            .collect::<serde_json::Map<String, Value>>(),
        "fixtures": [
            {"name": format!("{key} ok"), "outcome": "ok", "status": 200, "body": body},
            {"name": "not found", "outcome": "not_found", "status": 404,
             "body": {"errors": [{"message": "Not Found"}]}},
            {"name": "expired credential", "outcome": "401 -> connector_auth_failed", "status": 401,
             "body": {"errors": "Unauthorized"}},
            {"name": "upstream 5xx", "outcome": "upstream_unavailable", "status": 503,
             "body": {"errors": "Unavailable"}},
            {"name": "upstream timeout", "outcome": "timeout", "status": 0, "body": null}
        ],
    })
}

/// Create and publish the platform template `iam_mcp@1` (transport `mcp`, credential header `X-Service-Key`).
async fn publish_mcp_template(w: &World) {
    let su = super_admin();
    let (s, b) = send(
        &w.e.app,
        req(
            "POST",
            "/api/platform/connector-templates",
            Some(&su),
            json!({
                "key": TEMPLATE, "version": "1", "label": "IAM MCP desk", "vendor": "Test",
                "system_class": "commerce", "wave": "v1", "transport": "mcp", "auth_kind": "api_key_header",
                "auth_params": {"header": "X-Service-Key"}, "vendor_api_pin": "test pin",
                "credential_schema": [{"name": "api_key", "label": "Service key", "secret": true}],
                "ops": TOOLS.iter().map(|(op, tool, args)| template_op(op, tool, args)).collect::<Vec<_>>(),
            }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "mcp template: {b}");
    let (s, b) = send(
        &w.e.app,
        req(
            "POST",
            &format!("/api/platform/connector-templates/{TEMPLATE}/1/publish"),
            Some(&su),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "publish mcp template: {b}");
}

/// The world, the fake MCP server, the published MCP template and the tenant's instance on it; `f`'s test routes point
/// at the MCP instance (the REST `System` of the world is left unused: any hit on it is a routing bug).
async fn mcp_world(name: &str, script: Script) -> (World, ExtMcp) {
    let w = World::build(name, script, false).await;
    let mcp = start_mcp().await;
    publish_mcp_template(&w).await;
    w.external_instance(
        INSTANCE,
        TEMPLATE,
        "test",
        &mcp.url,
        json!({"api_key": MCP_KEY}),
    )
    .await;
    w.route_ops_to(&w.f, "test", INSTANCE).await;
    (w, mcp)
}

// ---- B1 ---------------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_mcp_tool_gets_its_identity_inputs_from_state_and_the_model_never_sees_them() {
    // the model tries to name who the call is for and to override the state-filled id: none of it may travel
    let (w, mcp) = mcp_world(
        "iam-b1-chain",
        tool_then_say(
            "order_list",
            json!({"on_behalf_of": "evil-sub", "actor": "evil-sub", "_meta": {"ai.nonagentic/actor":
                {"address": "evil-sub"}}, "party_id": "evil-sub"}),
            ANSWER,
        ),
    )
    .await;
    let f = &w.f;
    w.chain_policies(f).await;
    w.daemons(f, Env::Test, &[READ, CANCEL], false).await;

    let conv = w.signin_flow(f).await;

    // the routed MCP op's declared input types reach the model's tool spec (the state-filled ones stay out)
    let body = &w.llm.assistant_bodies()[0];
    let cancel = body["tools"]
        .as_array()
        .and_then(|t| t.iter().find(|t| t["function"]["name"] == "order_cancel"))
        .unwrap_or_else(|| panic!("order_cancel offered: {body}"));
    let props = cancel["function"]["parameters"]["properties"]
        .as_object()
        .unwrap();
    assert!(props.contains_key("reason"), "{props:?}");
    for (name, p) in props {
        assert_eq!(
            p,
            &json!({"type": "string", "description": format!("typed {name}")}),
            "{name}"
        );
    }

    // the REST system was never involved; the MCP tools ran in chain order
    assert!(w.sys.hits().is_empty(), "{:?}", w.sys.hits());
    assert_eq!(mcp.names(), CHAIN);

    // every argument is the state-filled value, under the tool's own argument name, and nothing else
    let calls = mcp.calls();
    assert_eq!(calls[0].args, json!({"code": CODE}), "{calls:?}");
    assert_eq!(
        calls[1].args,
        json!({"email": CUSTOMER_EMAIL, "token": TOKEN}),
        "{calls:?}"
    );
    assert_eq!(calls[2].args, json!({"customer_id": CUSTOMER}), "{calls:?}");

    // no model-supplied actor or id reached the MCP server (on_behalf_of carries a person's full fr rights)
    assert!(
        !format!("{calls:?}").contains("evil-sub"),
        "model output reached the MCP call: {calls:?}"
    );

    // the credential is the instance's key (platform auth), on every request of the session
    assert!(calls.iter().all(|c| c.key.as_deref() == Some(MCP_KEY)));
    let posts = mcp.posts();
    assert!(!posts.is_empty());
    assert!(
        posts.iter().all(|k| k.as_deref() == Some(MCP_KEY)),
        "{posts:?}"
    );

    // the transcript stores the code redacted, never the code
    let msgs = w.messages(f, &conv).await;
    assert!(
        msgs.iter()
            .any(|m| m.0 == "customer" && m.1 == "[redacted]"),
        "{msgs:?}"
    );
    assert!(msgs.iter().all(|m| !m.1.contains(CODE)), "{msgs:?}");

    // state: the token and the code are sealed secrets, the customer id a value
    let state = w.state(f, &conv).await;
    let get = |k: &str| {
        state
            .iter()
            .find(|s| s.0 == k)
            .unwrap_or_else(|| panic!("{k} in {state:?}"))
    };
    let token = get("auth_token");
    assert_eq!(token.1, "secret");
    assert!(token.2.is_none(), "no plain value for a secret");
    assert!(
        token
            .3
            .as_deref()
            .is_some_and(|s| !s.is_empty() && !s.contains(TOKEN)),
        "sealed"
    );
    let id = get("customer_id");
    assert_eq!((id.1.as_str(), id.2.as_deref()), ("value", Some(CUSTOMER)));
    let otp = get("one_time_code");
    assert_eq!(otp.1, "secret");
    assert!(otp.2.is_none());

    // audit trail: the producers ran as `resolve` sub-calls, the tool as the assistant's own call
    let audit = w.calls(f).await;
    for op in [VERIFY, LOOKUP] {
        assert!(
            audit
                .iter()
                .any(|c| c.0 == op && c.1 == "ok" && c.3 == "resolve"),
            "{op} resolved: {audit:?}"
        );
    }
    assert!(
        audit
            .iter()
            .any(|c| c.0 == READ && c.1 == "ok" && c.3 != "resolve"),
        "{audit:?}"
    );

    // the model never saw the code, the token or the id (nor the instance's key)
    let seen = w.llm.everything();
    for secret in [CODE, TOKEN, CUSTOMER, MCP_KEY] {
        assert!(!seen.contains(secret), "{secret} reached a model request");
    }
    assert!(!w.llm.assistant_bodies().is_empty());
}

// ---- B4 ---------------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_iam_procedure_built_from_mcp_tools_serves_two_jobs() {
    let (w, mcp) = mcp_world("iam-b4", tool_then_say("order_list", json!({}), ANSWER)).await;
    let (job_b, job_c) = (w.new_job().await, w.new_job().await);
    let (a, b, c) = (&w.f, &job_b, &job_c);
    // new jobs are routed to the REST instance of the world: send them to the MCP system too
    for f in [b, c] {
        w.route_ops_to(f, "test", INSTANCE).await;
    }

    // the tenant admin builds the procedure from MCP-served ops
    let (s, p) = w
        .tenant_call(
            "POST",
            "/api/iam-procedures",
            json!({"name": "Acme MCP sign-in", "description": "one-time code, then CRM", "steps": signin_steps()}),
        )
        .await;
    assert_eq!(s, 201, "{p}");
    let id = p["id"].as_str().unwrap().to_owned();
    let (s, dup) = w
        .tenant_call(
            "POST",
            "/api/iam-procedures",
            json!({"name": "Acme MCP sign-in", "steps": []}),
        )
        .await;
    assert_eq!((s.as_u16(), code(&dup)), (409, "name_taken"), "{dup}");

    // every job declares only the consumer; a and b attach the procedure, c does not
    for f in [a, b, c] {
        w.policy_ok(f, READ, read_policy(false)).await;
    }
    for f in [a, b] {
        let (s, r) = w
            .call(f, "PUT", &format!("iam-procedures/{id}"), json!({}))
            .await;
        assert!(s.is_success(), "{s} {r}");
    }

    // both attached jobs resolve through the one procedure, whose producers are MCP tools
    for f in [a, b] {
        w.daemons(f, Env::Test, &[READ], false).await;
        let before = mcp.calls().len();
        w.signin_flow(f).await;
        let ran: Vec<String> = mcp.names().split_off(before);
        assert_eq!(ran, CHAIN, "job {}", f.job.job_key);
        let fresh = &mcp.calls()[before..];
        assert_eq!(fresh[0].args, json!({"code": CODE}));
        assert_eq!(
            fresh[1].args,
            json!({"email": CUSTOMER_EMAIL, "token": TOKEN})
        );
        assert_eq!(fresh[2].args, json!({"customer_id": CUSTOMER}));
        assert!(fresh.iter().all(|x| x.key.as_deref() == Some(MCP_KEY)));
    }
    assert!(w.sys.hits().is_empty(), "{:?}", w.sys.hits());

    // the detached job has no producer for customer_id: the MCP tool is not called and a person takes over
    w.daemons(c, Env::Test, &[READ], false).await;
    let before = mcp.calls().len();
    let conv = w.start_chat(c).await;
    w.say(c, &conv, ASK).await;
    let (reason, _) = w.handover(c).await;
    assert_eq!(reason, "tool_failed");
    assert_eq!(mcp.calls().len(), before, "nothing reached the MCP system");
    let audit = w.calls(c).await;
    assert!(
        audit
            .iter()
            .any(|x| x.0 == READ && x.2.as_deref() == Some("input_unresolved")),
        "{audit:?}"
    );

    // no model request ever carried the code, the token or the id
    let seen = w.llm.everything();
    for secret in [CODE, TOKEN, CUSTOMER, MCP_KEY] {
        assert!(!seen.contains(secret), "{secret} reached a model request");
    }
}
