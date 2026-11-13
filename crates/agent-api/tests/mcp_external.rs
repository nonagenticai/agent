//! B1: an MCP-transport template with a credential calls an EXTERNAL MCP server at the instance's base URL, with the
//! instance's own headers, over the netguard-pinned client; and `mcp-import` turns the server's tools into template
//! ops. The only fake is the external MCP server (an axum app on loopback speaking streamable-HTTP JSON-RPC);
//! the import test runs on the real Postgres harness through the API router.
use crate::common;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_api::connectors::contract::call_op;
use agent_api::connectors::instances::InstanceRow;
use agent_api::connectors::mapping;
use agent_api::connectors::model::{ConnectorTemplate, ConnectorTemplateOp};
use agent_api::connectors::runtime::{ConnectorExecutor, Env, OpRequest, OpState, ResolvedTarget};
use agent_api::connectors::transport::Transport;
use agent_api::mcp_client::store::{AnswerInput, NewTask, TaskPoller, record_started};
use agent_api::mcp_client::tasks::{StaffActor, TaskHandle};
use agent_api::netguard::Policy;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::IntoResponse;
use base64::Engine as _;
use common::platform::generic_rest;
use std::time::Duration;

use agent_api::connectors::transport::CallTarget;
use agent_api::connectors::transport::auth::AuthSpec;
use agent_api::connectors::wire::{AuthKind, CallSecrets, RenderedRequest};
use agent_api::mcp_client::external::ExtTaskStart;
use common::{TestEnv, insert_tenant, insert_user, json as json_req, person, send};
use serde_json::{Map, Value, json};
use uuid::Uuid;

const KEY_HEADER: &str = "x-planka-api-key";
const API_KEY: &str = "planka-key-5150-secret";
/// The one question a waiting external task asks (`tasks/get` `inputRequests`).
const ASK: &str = "q-1";

/// One POST the stub saw: its JSON-RPC method and the credential header.
type Post = (String, Option<String>);

/// What the fake external server saw.
#[derive(Clone, Default)]
struct Seen {
    /// (rpc method, value of the key header)
    posts: Arc<Mutex<Vec<Post>>>,
    /// `Authorization` of every /mcp POST, in order: `Bearer <licence token>` for a licence-bearer system.
    auth: Arc<Mutex<Vec<Option<String>>>>,
    /// arguments of every tools/call
    calls: Arc<Mutex<Vec<Value>>>,
    /// answer 401 to everything
    refuse: Arc<Mutex<bool>>,
    /// the tenants a bundle-token mint was asked for
    mints: Arc<Mutex<Vec<String>>>,
    /// the next task id of a `tools/call` (`<prefix>-<n>`)
    started: Arc<Mutex<Vec<String>>>,
    /// the task ids a `tasks/update` reached
    answered: Arc<Mutex<HashSet<String>>>,
    /// the task id of every `tasks/get`
    gets: Arc<Mutex<Vec<String>>>,
    /// the params of every `tasks/update`
    updates: Arc<Mutex<Vec<Value>>>,
    /// a started task asks one question before it completes (`tasks/get` answers `input_required`, then `completed`)
    asks: Arc<AtomicBool>,
}

impl Seen {
    /// The `Authorization` of the last POST of `method`.
    fn auth_of(&self, method: &str) -> Option<String> {
        let posts = self.posts.lock().unwrap();
        let auth = self.auth.lock().unwrap();
        posts
            .iter()
            .zip(auth.iter())
            .rfind(|(p, _)| p.0 == method)
            .and_then(|(_, a)| a.clone())
    }
}

fn tools() -> Value {
    json!([
        {"name": "order", "description": "Orders.",
         "inputSchema": {"type": "object", "properties": {
             "action": {"type": "string"},
             "order_id": {"type": "string", "description": "The order."}},
             "required": ["order_id"]},
         "outputSchema": {"type": "object", "properties": {"status": {"type": "string"}, "total": {"type": "number"}}},
         "annotations": {"readOnlyHint": true},
         "_meta": {"operations": {"order.read": {"action": "read"}}}},
        {"name": "card", "description": "Cards.",
         "inputSchema": {"type": "object", "properties": {
             "action": {"type": "string"},
             "card_id": {"type": "string", "description": "The card."},
             "title": {"type": "string", "description": "New title."}},
             "required": ["card_id"]},
         "_meta": {"operations": {"card.rename": {"action": "rename"}}}}
    ])
}

/// The licences `/bundle-token` mint (the licence service the minter reads `BUNDLE_MINTER_LICENSES_URL` from).
async fn bundle_token(
    State(seen): State<Seen>,
    Path(tenant): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> (StatusCode, axum::Json<Value>) {
    seen.mints.lock().unwrap().push(tenant.clone());
    let token = licence_of(&tenant);
    (
        StatusCode::CREATED,
        axum::Json(json!({
            "tenant_id": tenant,
            "app": q.get("app").cloned().unwrap_or_default(),
            "bundle": q.get("bundle").cloned().unwrap_or_default(),
            "token": token,
            "license_id": "lic-1",
            "token_id": "tok-1",
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
        })),
    )
}

/// The Keycloak token endpoint the minter authenticates its client at (`BUNDLE_MINTER_TOKEN_URL`).
async fn token_endpoint() -> axum::Json<Value> {
    axum::Json(json!({"access_token": "minter-client-token", "expires_in": 3600}))
}

/// The bearer the fake mints for `tenant`.
fn licence_of(tenant: &str) -> String {
    format!("licence-token-{tenant}")
}

async fn fake_mcp(seen: Seen) -> String {
    fake_mcp_named(seen, "t").await
}

/// The fake external server on loopback: `/mcp` (streamable HTTP JSON-RPC with the tasks extension), the licences
/// mint and the Keycloak token endpoint the bundle minter calls. Task ids are `<prefix>-<n>`.
async fn fake_mcp_named(seen: Seen, prefix: &str) -> String {
    let prefix = prefix.to_owned();
    let s = seen.clone();
    let app = axum::Router::new()
        .route(
            "/mcp",
            axum::routing::any(move |method: Method, headers: HeaderMap, body: String| {
                let seen = s.clone();
                let prefix = prefix.clone();
                async move {
                    if method != Method::POST {
                        return StatusCode::METHOD_NOT_ALLOWED.into_response();
                    }
                    let req: Value = serde_json::from_str(&body).unwrap();
                    let m = req["method"].as_str().unwrap_or("").to_owned();
                    let key = headers
                        .get(KEY_HEADER)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    let auth = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    seen.posts.lock().unwrap().push((m.clone(), key.clone()));
                    seen.auth.lock().unwrap().push(auth.clone());
                    // the key of an api-key system, the licence bearer of a licence-bearer system
                    let accepted = key.as_deref() == Some(API_KEY)
                        || auth
                            .as_deref()
                            .is_some_and(|a| a.starts_with("Bearer ") && a.len() > "Bearer ".len());
                    if *seen.refuse.lock().unwrap() || !accepted {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    let id = req["id"].clone();
                    let tid = req["params"]["taskId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    let task = |status: &str, extra: Value| {
                        let mut t = json!({"resultType": "complete", "taskId": tid, "status": status,
                            "createdAt": "2026-11-08T00:00:00Z",
                            "lastUpdatedAt": "2026-11-08T00:00:00Z",
                            "ttlMs": 604800000, "pollIntervalMs": 5000});
                        if let Value::Object(extra) = extra
                            && let Some(t) = t.as_object_mut()
                        {
                            t.extend(extra);
                        }
                        t
                    };
                    if m == "tasks/get" {
                        seen.gets.lock().unwrap().push(tid.clone());
                    }
                    let reply = match m.as_str() {
                        "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": {
                            "protocolVersion": req["params"]["protocolVersion"],
                            "capabilities": {"tools": {},
                                             "extensions": {"io.modelcontextprotocol/tasks": {}}},
                            "serverInfo": {"name": "fake", "version": "0"}}}),
                        "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools()}}),
                        // SEP-2663: a long-running tool answers with a task instead of a result.
                        "tools/call" if req["params"]["name"] == "project_execute" => {
                            seen.calls.lock().unwrap().push(req["params"].clone());
                            let mut started = seen.started.lock().unwrap();
                            let task_id = format!("{prefix}-{}", started.len() + 1);
                            started.push(task_id.clone());
                            // rmcp flattens the seed task at the top level (CreateTaskResult.task is
                            // #[serde(flatten)]), so the task fields sit beside resultType, not under "task".
                            json!({"jsonrpc": "2.0", "id": id, "result": {
                                "resultType": "task",
                                "taskId": task_id, "status": "working",
                                "createdAt": "2026-11-08T00:00:00Z",
                                "lastUpdatedAt": "2026-11-08T00:00:00Z",
                                "ttlMs": 604800000, "pollIntervalMs": 5000}})
                        }
                        "tools/call" => {
                            seen.calls.lock().unwrap().push(req["params"].clone());
                            json!({"jsonrpc": "2.0", "id": id, "result": {
                                "content": [{"type": "text", "text": "{\"card\":{\"title\":\"Ship it\"}}"}],
                                "structuredContent": {"card": {"title": "Ship it"}},
                                "isError": false}})
                        }
                        // A task waits for one human answer when `asks` is set: `input_required` until a
                        // `tasks/update` reaches it, then the terminal result.
                        "tasks/get" if seen.asks.load(Ordering::SeqCst)
                            && !seen.answered.lock().unwrap().contains(&tid) =>
                        {
                            json!({"jsonrpc": "2.0", "id": id, "result": task(
                                "input_required",
                                json!({"statusMessage": "waiting for approval",
                                       "inputRequests": {ASK: {"method": "elicitation/create", "params": {
                                           "message": "Deliver the project?",
                                           "requestedSchema": {"type": "object", "required": ["decision"],
                                               "properties": {"decision": {"type": "string",
                                                                           "enum": ["approved", "rejected"]}}}}}}}))})
                        }
                        "tasks/get" if seen.asks.load(Ordering::SeqCst) => {
                            json!({"jsonrpc": "2.0", "id": id, "result": task("completed", json!({
                                "result": {"content": [{"type": "text", "text": "delivered"}],
                                           "structuredContent": {"run_id": "r-1", "project_id": "p-1",
                                                                 "status": "succeeded", "result": "delivered"},
                                           "isError": false}}))})
                        }
                        "tasks/get" => json!({"jsonrpc": "2.0", "id": id, "result": task("working", json!({}))}),
                        "tasks/update" => {
                            seen.answered.lock().unwrap().insert(tid.clone());
                            seen.updates.lock().unwrap().push(req["params"].clone());
                            json!({"jsonrpc": "2.0", "id": id, "result": {"resultType": "complete"}})
                        }
                        "tasks/cancel" => json!({"jsonrpc": "2.0", "id": id,
                            "result": {"resultType": "complete"}}),
                        _ => return StatusCode::ACCEPTED.into_response(),
                    };
                    let mut resp = axum::Json(reply).into_response();
                    if m == "initialize" {
                        resp.headers_mut()
                            .insert("mcp-session-id", "sess-1".parse().unwrap());
                    }
                    resp
                }
            }),
        )
        .route("/token", axum::routing::post(token_endpoint))
        .route(
            "/internal/tenants/{tenant}/bundle-token",
            axum::routing::post(bundle_token),
        )
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://127.0.0.1:{port}/mcp")
}

fn transport() -> Transport {
    Transport::new(Policy {
        allow_loopback: true,
        private_targets: vec![],
    })
}

fn template() -> ConnectorTemplate {
    serde_json::from_value(json!({
        "key": "kanban", "version": "1", "transport": "mcp", "auth_kind": "api_key_header",
        "auth_params": {"header": "X-Planka-Api-Key", "value_template": "{{secret.api_key}}"},
        "credential_schema": [{"name": "api_key", "label": "Key", "secret": true}],
        "status": "active", "ops": []
    }))
    .unwrap()
}

fn read_op() -> ConnectorTemplateOp {
    serde_json::from_value(json!({
        "object": "card", "verb": "read", "side_effect": "R", "method": "POST", "path": "/card",
        "request": r#"{"action":"read","card_id":"{{canonical.card_id}}"}"#,
        "field_map": [{"path": "$.card.title", "field": "title", "transform": "none"}],
        "idempotency": {"kind": "nat"}, "timeout_ms": 5000
    }))
    .unwrap()
}

fn instance(base: &str, tenant: Uuid) -> InstanceRow {
    InstanceRow {
        tenant_id: tenant,
        id: "kanban-1".into(),
        label: "Kanban".into(),
        template_scope: "tenant".into(),
        template_key: "kanban".into(),
        template_version: "1".into(),
        env: "test".into(),
        base_url: base.into(),
        env_ids: json!({}),
        max_rps: None,
        stub: false,
        platform: false,
        status: "ok".into(),
        last_error: None,
        created_by: "u".into(),
        created_at: chrono::Utc::now(),
        cred_updated_at: None,
        cred_fingerprint: Some("fp".into()),
        cred_version: Some(1),
    }
}

fn secret() -> Map<String, Value> {
    json!({"api_key": API_KEY}).as_object().cloned().unwrap()
}

async fn run(
    t: &Transport,
    inst: &InstanceRow,
) -> Result<agent_api::connectors::wire::RawResponse, agent_api::connectors::wire::OpError> {
    let canonical = json!({"card_id": "C-1"}).as_object().cloned().unwrap();
    call_op(
        t,
        inst,
        &template(),
        &read_op(),
        &canonical,
        Some(&secret()),
        "idem-1",
        "test",
    )
    .await
}

#[tokio::test]
async fn an_external_mcp_call_carries_the_instance_key_and_maps_through_field_map() {
    let seen = Seen::default();
    let base = fake_mcp(seen.clone()).await;
    let t = transport();
    let inst = instance(&base, Uuid::new_v4());
    let resp = run(&t, &inst).await.expect("call");
    assert_eq!(resp.status, 200);
    let mapped = mapping::extract(&read_op(), &BTreeMap::new(), &resp).unwrap();
    assert_eq!(mapped["title"], "Ship it");
    // every request of the session carried the instance's key
    let posts = seen.posts.lock().unwrap().clone();
    assert!(posts.iter().any(|(m, _)| m == "tools/call"));
    assert!(
        posts.iter().all(|(_, k)| k.as_deref() == Some(API_KEY)),
        "{posts:?}"
    );
    // the template's constant argument went out with the mapped input
    let call = seen.calls.lock().unwrap()[0].clone();
    assert_eq!(
        (&call["name"], &call["arguments"]),
        (&json!("card"), &json!({"action": "read", "card_id": "C-1"}))
    );
    // the session is reused
    run(&t, &inst).await.expect("second call");
    let inits = seen
        .posts
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| m == "initialize")
        .count();
    assert_eq!(inits, 1);
}

#[tokio::test]
async fn a_refused_key_is_connector_auth_failed_and_never_echoed() {
    let seen = Seen::default();
    *seen.refuse.lock().unwrap() = true;
    let base = fake_mcp(seen.clone()).await;
    let t = transport();
    let e = run(&t, &instance(&base, Uuid::new_v4()))
        .await
        .expect_err("401");
    assert_eq!(e.code, "connector_auth_failed", "{e:?}");
    assert!(!e.detail.contains(API_KEY), "{}", e.detail);
    // a refused session is not kept: a server that now accepts is reached by the next call
    *seen.refuse.lock().unwrap() = false;
    run(&t, &instance(&base, Uuid::new_v4()))
        .await
        .expect("recovers");
}

#[tokio::test]
async fn an_external_server_that_materializes_a_task_is_polled_answered_and_cancelled() {
    let seen = Seen::default();
    let base = fake_mcp(seen.clone()).await;
    let t = transport();
    let inst = instance(&base, Uuid::new_v4());
    let target = CallTarget {
        tenant: inst.tenant_id,
        instance_id: inst.id.clone(),
        env: "test",
        credential_version: inst.cred_version.unwrap_or(0),
        base_url: inst.base_url.clone(),
        stub: false,
    };
    let spec = AuthSpec {
        kind: AuthKind::ApiKeyHeader,
        params: [
            ("header".to_string(), "X-Planka-Api-Key".to_string()),
            (
                "value_template".to_string(),
                "{{secret.api_key}}".to_string(),
            ),
        ]
        .into_iter()
        .collect(),
    };
    let secrets = CallSecrets {
        config: Default::default(),
        secret: secret(),
    };
    let ext = t.external_mcp();
    let timeout = Duration::from_secs(5);
    // The template names the tool, so no tools/list is needed to resolve it.
    let req = RenderedRequest {
        method: "POST".into(),
        path: "project_execute".into(),
        query: vec![],
        headers: vec![],
        body: Some(json!({"project_id": "p-1"})),
    };

    match ext
        .start_task(
            &t,
            &target,
            &spec,
            &secrets,
            "projects.execute",
            &req,
            timeout,
            None,
        )
        .await
        .expect("the call answers")
    {
        ExtTaskStart::Task(handle) => {
            assert_eq!(handle.task_id, "t-1");
            assert_eq!(handle.status, "working");
        }
        other => panic!("expected a task, got {other:?}"),
    }

    let snapshot = ext
        .get_task(&t, &target, &spec, &secrets, timeout, "t-1")
        .await
        .expect("tasks/get");
    assert_eq!(snapshot.status, "working");
    assert_eq!(snapshot.poll_interval_ms, Some(5000));
    assert_eq!(snapshot.detail["taskId"], "t-1");

    ext.answer_task(
        &t,
        &target,
        &spec,
        &secrets,
        timeout,
        "t-1",
        "approval-1",
        json!({"action": "accept", "content": {"decision": "approved"}}),
        None,
    )
    .await
    .expect("tasks/update");

    ext.cancel_task(&t, &target, &spec, &secrets, timeout, "t-1")
        .await
        .expect("tasks/cancel");

    let methods: Vec<String> = seen
        .posts
        .lock()
        .unwrap()
        .iter()
        .map(|(m, _)| m.clone())
        .collect();
    for want in ["tools/call", "tasks/get", "tasks/update", "tasks/cancel"] {
        assert!(
            methods.iter().any(|m| m == want),
            "{want} missing from {methods:?}"
        );
    }
}

/// A licence bearer is minted per call and a session caches its headers for `SESSION_TTL`, so a session opened with
/// one token must never be reused for another: a new token opens a new session, the same token reuses it.
#[tokio::test]
async fn a_new_licence_token_opens_a_new_session() {
    let seen = Seen::default();
    let base = fake_mcp(seen.clone()).await;
    let t = transport();
    let target = CallTarget {
        tenant: Uuid::new_v4(),
        instance_id: "convoy-session".into(),
        env: "test",
        credential_version: 0,
        base_url: base,
        stub: false,
    };
    let spec = AuthSpec {
        kind: AuthKind::LicenceBearer,
        params: BTreeMap::new(),
    };
    let secrets = |token: &str| CallSecrets {
        config: Default::default(),
        secret: json!({"licence_token": token})
            .as_object()
            .cloned()
            .unwrap(),
    };
    let req = RenderedRequest {
        method: "POST".into(),
        path: "card".into(),
        query: vec![],
        headers: vec![],
        body: Some(json!({"action": "read", "card_id": "C-1"})),
    };
    let timeout = Duration::from_secs(5);
    let ext = t.external_mcp();
    let inits = || {
        seen.posts
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == "initialize")
            .count()
    };
    ext.call(
        &t,
        &target,
        &spec,
        &secrets("token-1"),
        "card.read",
        &req,
        timeout,
    )
    .await
    .expect("call with the first licence");
    assert_eq!(inits(), 1);
    ext.call(
        &t,
        &target,
        &spec,
        &secrets("token-2"),
        "card.read",
        &req,
        timeout,
    )
    .await
    .expect("call with the second licence");
    assert_eq!(inits(), 2, "a new licence token opens a new session");
    ext.call(
        &t,
        &target,
        &spec,
        &secrets("token-2"),
        "card.read",
        &req,
        timeout,
    )
    .await
    .expect("call with the second licence again");
    assert_eq!(inits(), 2, "the same token reuses its session");
}

#[tokio::test]
async fn a_private_target_is_refused_like_rest() {
    let t = Transport::new(Policy::default());
    let e = run(&t, &instance("http://127.0.0.1:9/mcp", Uuid::new_v4()))
        .await
        .expect_err("loopback not allowed");
    assert!(matches!(e.code, "invalid_target" | "invalid_url"), "{e:?}");
}

fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
    vars.insert(
        "TENANT_DATA_KEYS".to_string(),
        format!(r#"{{"k1":"{key}"}}"#),
    );
    vars.insert("TENANT_DATA_KEY_CURRENT".to_string(), "k1".to_string());
}

/// A tenant with an integrations manager, the connector transport allowed to reach loopback, and the router.
async fn world_with_env(
    tweak: impl FnOnce(&mut HashMap<String, String>),
) -> (
    TestEnv,
    agent_api::state::AppState,
    axum::Router,
    String,
    Uuid,
) {
    let e = common::env_with(tweak).await;
    let a = Uuid::new_v4();
    generic_rest(&e).await.unwrap();
    insert_tenant(&e.owner, a, "Tenant A").await;
    insert_user(&e.owner, "u-im-a", a, "integrations_manager").await;
    let mut st = e.state.clone();
    st.connectors = transport();
    let app = agent_api::router(st.clone());
    (e, st, app, person("u-im-a", &[]), a)
}

async fn world() -> (
    TestEnv,
    agent_api::state::AppState,
    axum::Router,
    String,
    Uuid,
) {
    world_with_env(with_keys).await
}

/// [`world`] with the bundle minter pointed at the fake server (`mcp` is its `/mcp` endpoint), so a licence-bearer
/// system is minted for and called without a stored credential.
async fn world_bundle(
    mcp: &str,
) -> (
    TestEnv,
    agent_api::state::AppState,
    axum::Router,
    String,
    Uuid,
) {
    let licences = mcp.trim_end_matches("/mcp").to_owned();
    world_with_env(move |vars| {
        with_keys(vars);
        vars.insert("BUNDLE_MINTER_LICENSES_URL".to_string(), licences.clone());
        vars.insert(
            "BUNDLE_MINTER_TOKEN_URL".to_string(),
            format!("{licences}/token"),
        );
        vars.insert(
            "BUNDLE_MINTER_CLIENT_SECRET".to_string(),
            "test-client-secret".to_string(),
        );
    })
    .await
}

#[tokio::test]
async fn import_creates_one_op_per_tool_and_per_consolidated_operation() {
    let seen = Seen::default();
    let base = fake_mcp(seen.clone()).await;
    let (_e, _st, app, tok, _t) = world().await;
    let call = |m: &'static str, uri: String, body: Value| {
        let (app, tok) = (app.clone(), tok.clone());
        async move { send(&app, json_req(m, &uri, Some(&tok), body)).await }
    };
    let (s, b) = call(
        "POST",
        "/api/connections/templates".into(),
        json!({"key": "kanban_mcp", "label": "Kanban", "kind": "custom", "transport": "mcp",
               "auth_kind": "api_key_header", "description": "", "auth_params": {"header": "X-Planka-Api-Key"}}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["transport"], "mcp");
    let (s, b) = call(
        "POST",
        "/api/connections/systems".into(),
        json!({"id": "kanban-1", "label": "Kanban", "template_key": "kanban_mcp", "template_version": "1",
               "env": "test", "base_url": base, "env_ids": {}, "max_rps": null, "stub": false}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = call(
        "PUT",
        "/api/connections/systems/kanban-1/credential".into(),
        json!({"api_key": API_KEY}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");

    let import = || {
        call(
            "POST",
            "/api/connections/templates/kanban_mcp/1/mcp-import".into(),
            json!({"system": "kanban-1"}),
        )
    };
    let (s, b) = import().await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // the catalogue op is written with the platform's definition; the one the catalogue lacks is the tenant's own op
    let written = b["ops"].as_array().unwrap();
    assert_eq!(written.len(), 2, "{b}");
    let read = &written[0];
    assert_eq!(
        (read["op"].as_str(), read["tool"].as_str()),
        (Some("order.read"), Some("order"))
    );
    assert_eq!(read["action"], "created");
    assert_eq!(read["own"], false);
    // the fixed `action` is a constant, not an input
    assert_eq!(read["inputs"].as_array().unwrap().len(), 1, "{b}");
    assert_eq!(read["inputs"][0]["name"], "order_id");
    assert_eq!(read["inputs"][0]["required"], true);
    assert_eq!(read["inputs"][0]["description"], "The order.");
    let rename = &written[1];
    assert_eq!(
        (rename["op"].as_str(), rename["own"].as_bool()),
        (Some("card.rename"), Some(true))
    );
    assert!(b.get("missing_operations").is_none(), "{b}");
    assert_eq!(b["invalid_operations"], json!([]));

    // stored on the draft (the own op with the tool's definition), and the template's own /validate accepts both
    let (s, b) = send(&app, common::get("/api/connections/templates", Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let stored = b["items"][0]["ops"].as_array().unwrap();
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0]["path"], "/order");
    assert_eq!(
        stored[0]["request"],
        r#"{"action":"read","order_id":"{{canonical.order_id}}"}"#
    );
    assert_eq!(stored[0]["field_map"][0]["path"], "$.status");
    assert!(
        stored[0].get("def").is_none(),
        "a platform op keeps the platform definition"
    );
    assert_eq!(
        stored[1]["def"],
        json!({"side_effect": "W", "input_fields": ["card_id", "title"], "output_fields": [],
               "description": "Cards."})
    );
    let (s, b) = call(
        "POST",
        "/api/connections/templates/kanban_mcp/1/validate".into(),
        json!({}),
    )
    .await;
    assert_eq!((s, b.clone()), (StatusCode::OK, json!([])), "{b}");

    // importing again replaces the same op, it does not duplicate it
    let (s, b) = import().await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["ops"][0]["action"], "updated");
    // a system that is not there is refused
    let (s, b) = call(
        "POST",
        "/api/connections/templates/kanban_mcp/1/mcp-import".into(),
        json!({"system": "nope"}),
    )
    .await;
    assert_ne!(s, StatusCode::OK, "{b}");
}

// ---- the conveyor as an external task server (0250) ------------------------------------------------------------------

/// A tenant template (`mcp`, `licence_bearer`, no credential) and a system pinned to `base`; `task_tools` names the
/// ops it runs as tasks.
async fn licence_system(
    app: &axum::Router,
    tok: &str,
    key: &str,
    id: &str,
    base: &str,
    task_tools: Value,
) {
    let (s, b) = send(
        app,
        json_req(
            "POST",
            "/api/connections/templates",
            Some(tok),
            json!({"key": key, "label": "Conveyor", "kind": "custom", "transport": "mcp",
                   "auth_kind": "licence_bearer", "description": "", "task_tools": task_tools}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = send(
        app,
        json_req(
            "PUT",
            &format!("/api/connections/templates/{key}/1"),
            Some(tok),
            json!({
                "label": "Conveyor", "description": "", "credential_schema": [],
                "ops": [
                    {"object": "projects", "verb": "execute", "method": "POST", "path": "/project_execute",
                     "request": "{\"project_id\":\"{{canonical.project_id}}\"}",
                     "field_map": [{"path": "$.run_id", "field": "run_id", "transform": "none"}],
                     "idempotency": {"kind": "nat"}},
                    {"object": "card", "verb": "read", "method": "POST", "path": "/card",
                     "request": "{\"action\":\"read\",\"card_id\":\"{{canonical.card_id}}\"}",
                     "field_map": [{"path": "$.card.title", "field": "title", "transform": "none"}],
                     "idempotency": {"kind": "nat"}}
                ],
            }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(
        app,
        json_req(
            "POST",
            "/api/connections/systems",
            Some(tok),
            json!({"id": id, "label": id, "template_key": key, "template_version": "1",
                   "env": "test", "base_url": base, "env_ids": {}, "max_rps": null, "stub": false}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
}

/// A job of the tenant (owner pool: RLS does not apply).
async fn job(e: &TestEnv, tenant: Uuid, key: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, $2, $2, '[]', 'test') RETURNING id",
    )
    .bind(tenant)
    .bind(key)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

fn handle(id: &str) -> TaskHandle {
    TaskHandle {
        task_id: id.into(),
        status: "working".into(),
        status_message: None,
        poll_interval_ms: Some(2000),
        ttl_ms: Some(86_400_000),
    }
}

/// Records a task `origin = connector` on `instance` and makes it due now. Returns the row's key
/// (`tenant:instance:server id`), which is what every later SQL statement and staff route uses.
async fn start_connector_task(
    e: &TestEnv,
    tenant: Uuid,
    job: Uuid,
    id: &str,
    instance: &str,
) -> String {
    record_started(
        &e.state.db,
        &NewTask {
            tenant,
            job,
            operation: "projects.execute",
            idempotency_key: "idem-connector",
            case_id: None,
            created_by: "job",
            origin: "connector",
            instance: Some(instance),
        },
        "projects.execute",
        &handle(id),
    )
    .await
    .unwrap();
    let key: String = sqlx::query_scalar(
        "SELECT task_id FROM mcp_tasks WHERE server_task_id = $1 AND origin_instance = $2",
    )
    .bind(id)
    .bind(instance)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    sqlx::query("UPDATE mcp_tasks SET poll_after = now() - interval '1 second' WHERE task_id = $1")
        .bind(&key)
        .execute(&e.owner)
        .await
        .unwrap();
    key
}

fn op_request(
    tenant: Uuid,
    job: Uuid,
    operation: &str,
    inputs: Value,
    outputs: &[&str],
) -> OpRequest {
    OpRequest {
        tenant_id: tenant,
        job_id: job,
        operation: operation.to_string(),
        env: Env::Test,
        inputs: inputs.as_object().cloned().unwrap_or_default(),
        outputs: outputs.iter().map(|s| s.to_string()).collect(),
        idempotency_key: "idem-conveyor".to_string(),
        actor: None,
        case_id: None,
    }
}

#[tokio::test]
async fn a_licence_bearer_system_needs_no_stored_credential() {
    let seen = Seen::default();
    let base = fake_mcp(seen.clone()).await;
    let (e, st, app, tok, tenant) = world_bundle(&base).await;
    licence_system(&app, &tok, "convoy", "convoy-read", &base, json!([])).await;
    let j = job(&e, tenant, "convoy").await;
    let x = ConnectorExecutor::new(st.clone());
    let req = op_request(
        tenant,
        j,
        "card.read",
        json!({"card_id": "C-1"}),
        &["title"],
    );
    let out = x
        .execute(
            &req,
            &ResolvedTarget::Instance {
                instance_id: "convoy-read".into(),
            },
        )
        .await;
    assert_eq!(out.state, OpState::Ok, "{out:?}");
    assert_eq!(out.output["title"], json!("Ship it"));
    // No credential is stored for the system; the licence was minted (once) for this tenant and carried the call.
    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM connector_credential WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(stored, 0, "a licence-bearer system stores no credential");
    assert_eq!(
        seen.mints.lock().unwrap().clone(),
        [tenant.to_string()].to_vec()
    );
    assert_eq!(
        seen.auth_of("tools/call"),
        Some(format!("Bearer {}", licence_of(&tenant.to_string())))
    );
}

#[tokio::test]
async fn an_external_task_starts_from_a_job_and_records_its_origin() {
    let seen = Seen::default();
    let base = fake_mcp(seen.clone()).await;
    let (e, st, app, tok, tenant) = world_bundle(&base).await;
    licence_system(
        &app,
        &tok,
        "convoy",
        "convoy-task",
        &base,
        json!(["projects.execute"]),
    )
    .await;
    let j = job(&e, tenant, "convoy").await;
    let x = ConnectorExecutor::new(st.clone());
    let req = op_request(
        tenant,
        j,
        "projects.execute",
        json!({"project_id": "p-1"}),
        &["run_id"],
    );
    let out = x
        .execute(
            &req,
            &ResolvedTarget::Instance {
                instance_id: "convoy-task".into(),
            },
        )
        .await;
    assert_eq!(out.state, OpState::Ok, "{out:?}");
    assert_eq!(out.outcome_code.as_deref(), Some("mcp_task_started"));
    assert_eq!(out.output["mcp_task_id"], json!("t-1"));
    // A connector task's row key is namespaced on its server; the server's own id is kept beside it.
    let (task_id, server_task_id, origin, instance, operation, tool, status): (
        String,
        String,
        String,
        Option<String>,
        String,
        String,
        String,
    ) = sqlx::query_as(
        "SELECT task_id, server_task_id, origin, origin_instance, operation, tool, status
           FROM mcp_tasks WHERE server_task_id = 't-1'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(task_id, format!("{tenant}:convoy-task:t-1"));
    assert_eq!(server_task_id, "t-1");
    assert_eq!(
        (
            origin.as_str(),
            instance.as_deref(),
            operation.as_str(),
            tool.as_str(),
            status.as_str()
        ),
        (
            "connector",
            Some("convoy-task"),
            "projects.execute",
            "projects.execute",
            "working"
        )
    );
    assert_eq!(
        seen.auth_of("tools/call"),
        Some(format!("Bearer {}", licence_of(&tenant.to_string())))
    );
    // The task starts as the job's own: its `tools/call` carries the job actor in `_meta`, so the conveyor knows
    // whose run it is (`StaffActor::for_job`: the tenant's address, the job as `job`).
    let calls = seen.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(
        calls[0]["_meta"]["ai.nonagentic/actor"],
        json!({"channel": "mcp", "address": format!("agent:{tenant}"), "job": j.to_string()})
    );
}

#[tokio::test]
async fn the_poller_polls_an_external_task_through_its_own_instance() {
    let seen = Seen::default();
    let base = fake_mcp(seen.clone()).await;
    let (e, st, app, tok, tenant) = world_bundle(&base).await;
    licence_system(
        &app,
        &tok,
        "convoy",
        "convoy-poll",
        &base,
        json!(["projects.execute"]),
    )
    .await;
    let j = job(&e, tenant, "convoy").await;
    let key = start_connector_task(&e, tenant, j, "t-1", "convoy-poll").await;
    let p = TaskPoller {
        db: common::runtime_pool(&e.owner).await,
        clients: st.task_clients(),
        lease_s: 30,
        batch: 20,
        answer_timeout: Duration::from_secs(3600),
    };
    assert_eq!(p.tick().await.unwrap(), 1);
    let methods: Vec<String> = seen
        .posts
        .lock()
        .unwrap()
        .iter()
        .map(|(m, _)| m.clone())
        .collect();
    assert!(methods.iter().any(|m| m == "tasks/get"), "{methods:?}");
    // the wire carries the server's own id, never the namespaced row key
    assert_eq!(seen.gets.lock().unwrap().clone(), ["t-1".to_string()]);
    assert_eq!(
        seen.auth_of("tasks/get"),
        Some(format!("Bearer {}", licence_of(&tenant.to_string())))
    );
    let status: String = sqlx::query_scalar("SELECT status FROM mcp_tasks WHERE task_id = $1")
        .bind(&key)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(status, "working");
}

/// A system deleted while its task is in flight can never answer that task again: the poller ends the row `lost`
/// instead of retrying a permanent failure forever, and never calls `tasks/get` on a server it cannot reach.
#[tokio::test]
async fn a_deleted_instance_marks_the_task_lost() {
    let seen = Seen::default();
    let base = fake_mcp(seen.clone()).await;
    let (e, st, app, tok, tenant) = world_bundle(&base).await;
    licence_system(
        &app,
        &tok,
        "convoy",
        "convoy-gone",
        &base,
        json!(["projects.execute"]),
    )
    .await;
    let j = job(&e, tenant, "convoy").await;
    // the tenant deletes the system after its task started: the row still names it
    sqlx::query("DELETE FROM connector_instance WHERE tenant_id = $1 AND id = 'convoy-gone'")
        .bind(tenant)
        .execute(&e.owner)
        .await
        .unwrap();
    let key = start_connector_task(&e, tenant, j, "t-1", "convoy-gone").await;
    let p = TaskPoller {
        db: common::runtime_pool(&e.owner).await,
        clients: st.task_clients(),
        lease_s: 30,
        batch: 20,
        answer_timeout: Duration::from_secs(3600),
    };
    assert_eq!(p.tick().await.unwrap(), 1);
    let status: String = sqlx::query_scalar("SELECT status FROM mcp_tasks WHERE task_id = $1")
        .bind(&key)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(status, "lost");
    assert!(
        seen.gets.lock().unwrap().is_empty(),
        "no poll reaches a system that no longer exists"
    );
}

/// A licence the server has refused is dropped from the minter's cache, so the next poll mints a fresh token instead
/// of replaying the revoked one until its TTL. The fake's `/bundle-token` call count (`Seen.mints`) is the counter:
/// it goes up only when the minter actually asks for a new licence.
#[tokio::test]
async fn a_refused_licence_is_minted_again_on_the_next_poll() {
    let seen = Seen::default();
    let base = fake_mcp(seen.clone()).await;
    let (e, st, app, tok, tenant) = world_bundle(&base).await;
    licence_system(
        &app,
        &tok,
        "convoy",
        "convoy-licence",
        &base,
        json!(["projects.execute"]),
    )
    .await;
    let j = job(&e, tenant, "convoy").await;
    let key = start_connector_task(&e, tenant, j, "t-1", "convoy-licence").await;
    let p = TaskPoller {
        db: common::runtime_pool(&e.owner).await,
        clients: st.task_clients(),
        lease_s: 30,
        batch: 20,
        answer_timeout: Duration::from_secs(3600),
    };
    *seen.refuse.lock().unwrap() = true;
    assert_eq!(p.tick().await.unwrap(), 1);
    assert_eq!(
        seen.mints.lock().unwrap().len(),
        1,
        "the first poll minted a licence"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM mcp_tasks WHERE task_id = $1")
        .bind(&key)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(
        status, "working",
        "a refused credential is retried, not terminal"
    );
    // a failed get waits RETRY_AFTER; make the row due again for the next tick
    sqlx::query("UPDATE mcp_tasks SET poll_after = now() - interval '1 second' WHERE task_id = $1")
        .bind(&key)
        .execute(&e.owner)
        .await
        .unwrap();
    *seen.refuse.lock().unwrap() = false;
    assert_eq!(p.tick().await.unwrap(), 1);
    assert_eq!(
        seen.mints.lock().unwrap().len(),
        2,
        "the refused licence was dropped from the minter's cache (mint-endpoint count)"
    );
    assert_eq!(
        seen.gets.lock().unwrap().clone(),
        ["t-1".to_string()],
        "the second poll's tasks/get succeeded"
    );
}

#[tokio::test]
async fn a_saved_answer_goes_out_as_tasks_update_with_the_actor() {
    let seen = Seen::default();
    seen.asks.store(true, Ordering::SeqCst);
    let base = fake_mcp(seen.clone()).await;
    let (e, st, app, tok, tenant) = world_bundle(&base).await;
    licence_system(
        &app,
        &tok,
        "convoy",
        "convoy-answer",
        &base,
        json!(["projects.execute"]),
    )
    .await;
    let j = job(&e, tenant, "convoy").await;
    let key = start_connector_task(&e, tenant, j, "t-1", "convoy-answer").await;
    let p = TaskPoller {
        db: common::runtime_pool(&e.owner).await,
        clients: st.task_clients(),
        lease_s: 30,
        batch: 20,
        answer_timeout: Duration::from_secs(3600),
    };
    assert_eq!(p.tick().await.unwrap(), 1);
    let v = agent_api::mcp_client::store::one(&e.state.db, tenant, j, &key)
        .await
        .unwrap();
    assert_eq!(v.status, "input_required");
    assert_eq!(v.server_task_id, "t-1");
    assert_eq!(v.questions.len(), 1);

    let actor = StaffActor {
        tenant,
        staff_user: "u-staff".into(),
    };
    let r = agent_api::mcp_client::store::answer(
        &e.state.db,
        &st.task_clients(),
        &actor,
        j,
        &key,
        &AnswerInput {
            key: ASK.into(),
            action: "accept".into(),
            content: Some(json!({"decision": "approved"})),
        },
        &[],
    )
    .await
    .unwrap();
    assert!(r.sent && !r.already_answered);
    let updates = seen.updates.lock().unwrap().clone();
    assert_eq!(updates.len(), 1, "{updates:?}");
    // the server is answered with its own task id
    assert_eq!(
        updates[0]["taskId"],
        json!("t-1"),
        "the namespaced row key never goes on the wire"
    );
    assert_eq!(
        updates[0]["inputResponses"][ASK],
        json!({"action": "accept", "content": {"decision": "approved"}})
    );
    assert_eq!(
        updates[0]["_meta"]["ai.nonagentic/actor"],
        json!({"channel": "mcp", "address": format!("agent:{tenant}"), "on_behalf_of": "u-staff"})
    );
}

#[tokio::test]
async fn each_task_is_polled_through_its_own_instance() {
    let seen_a = Seen::default();
    let base_a = fake_mcp_named(seen_a.clone(), "a").await;
    let seen_b = Seen::default();
    let base_b = fake_mcp_named(seen_b.clone(), "b").await;
    let (e, st, app, tok, tenant) = world_bundle(&base_a).await;
    licence_system(
        &app,
        &tok,
        "convoy",
        "convoy-a",
        &base_a,
        json!(["projects.execute"]),
    )
    .await;
    let (s, b) = send(
        &app,
        json_req(
            "POST",
            "/api/connections/systems",
            Some(&tok),
            json!({"id": "convoy-b", "label": "B", "template_key": "convoy", "template_version": "1",
                   "env": "test", "base_url": base_b, "env_ids": {}, "max_rps": null, "stub": false}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let j = job(&e, tenant, "convoy").await;
    start_connector_task(&e, tenant, j, "a-1", "convoy-a").await;
    start_connector_task(&e, tenant, j, "b-1", "convoy-b").await;
    let p = TaskPoller {
        db: common::runtime_pool(&e.owner).await,
        clients: st.task_clients(),
        lease_s: 30,
        batch: 20,
        answer_timeout: Duration::from_secs(3600),
    };
    assert_eq!(p.tick().await.unwrap(), 2);
    assert_eq!(
        seen_a.gets.lock().unwrap().clone(),
        ["a-1".to_string()].to_vec()
    );
    assert_eq!(
        seen_b.gets.lock().unwrap().clone(),
        ["b-1".to_string()].to_vec()
    );
    for seen in [&seen_a, &seen_b] {
        assert_eq!(
            seen.auth_of("tasks/get"),
            Some(format!("Bearer {}", licence_of(&tenant.to_string())))
        );
    }
}

/// A task id is unique only on its own server (SEP-2663): two connector instances answering `t-1` are two rows, and
/// each poll reaches the server that returned that id, carrying the server's own id rather than the row key.
#[tokio::test]
async fn two_instances_may_return_the_same_task_id() {
    let seen_a = Seen::default();
    let base_a = fake_mcp(seen_a.clone()).await;
    let seen_b = Seen::default();
    let base_b = fake_mcp(seen_b.clone()).await;
    let (e, st, app, tok, tenant) = world_bundle(&base_a).await;
    licence_system(
        &app,
        &tok,
        "convoy",
        "convoy-a",
        &base_a,
        json!(["projects.execute"]),
    )
    .await;
    let (s, b) = send(
        &app,
        json_req(
            "POST",
            "/api/connections/systems",
            Some(&tok),
            json!({"id": "convoy-b", "label": "B", "template_key": "convoy", "template_version": "1",
                   "env": "test", "base_url": base_b, "env_ids": {}, "max_rps": null, "stub": false}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let j = job(&e, tenant, "convoy").await;
    let x = ConnectorExecutor::new(st.clone());
    for instance in ["convoy-a", "convoy-b"] {
        let out = x
            .execute(
                &op_request(
                    tenant,
                    j,
                    "projects.execute",
                    json!({"project_id": "p-1"}),
                    &["run_id"],
                ),
                &ResolvedTarget::Instance {
                    instance_id: instance.into(),
                },
            )
            .await;
        assert_eq!(out.state, OpState::Ok, "{instance}: {out:?}");
        assert_eq!(
            out.outcome_code.as_deref(),
            Some("mcp_task_started"),
            "{instance}"
        );
        assert_eq!(out.output["mcp_task_id"], json!("t-1"), "{instance}");
    }
    // both servers' `t-1` are stored: neither insert collided with the other
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT task_id, server_task_id FROM mcp_tasks WHERE server_task_id = 't-1' ORDER BY task_id",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            (format!("{tenant}:convoy-a:t-1"), "t-1".to_string()),
            (format!("{tenant}:convoy-b:t-1"), "t-1".to_string()),
        ]
    );
    sqlx::query(
        "UPDATE mcp_tasks SET poll_after = now() - interval '1 second' WHERE server_task_id = 't-1'",
    )
    .execute(&e.owner)
    .await
    .unwrap();
    // each poll goes to its own server, with that server's own task id
    let p = TaskPoller {
        db: common::runtime_pool(&e.owner).await,
        clients: st.task_clients(),
        lease_s: 30,
        batch: 20,
        answer_timeout: Duration::from_secs(3600),
    };
    assert_eq!(p.tick().await.unwrap(), 2);
    assert_eq!(
        seen_a.gets.lock().unwrap().clone(),
        ["t-1".to_string()].to_vec()
    );
    assert_eq!(
        seen_b.gets.lock().unwrap().clone(),
        ["t-1".to_string()].to_vec()
    );
}
