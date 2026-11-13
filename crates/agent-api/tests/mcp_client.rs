//! `HttpMcp` calls nonagenticai_mcp_server as ONE tenant per call: `X-Tenant-Id: Bearer <tenant's mcp licence>` and
//! `X-MCP-Binding: <tenant's binding token>`, no `Authorization`, one session per tenant, and no request at all when
//! the tenant's credentials cannot be fetched. Then the MCP tasks side (`mcp_client::tasks`) against a stub server.
//! The fake servers are external-boundary fakes over real HTTP on loopback.
//!
//! Dropped from care-bus: the `ApiCredentialSource` / `from_vars` tests (those pieces are not ported; the agent
//! builds its own `CredentialSource`) and the strata-token assertions (the binding header replaces it).
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_api::mcp_client::McpClient;
use agent_api::mcp_client::client::{
    AUTH_REJECTED, BINDING_HEADER, FnSource, HttpMcp, McpCredentials, TENANT_HEADER,
    TenantCredentials,
};
use serde_json::json;
use uuid::Uuid;

const A: Uuid = Uuid::from_u128(0xa);
const B: Uuid = Uuid::from_u128(0xb);

fn short(t: Uuid) -> String {
    format!("{:x}", t.as_u128())
}

/// The n-th licence (1-based) the fake issuer hands `t`.
fn lic(t: Uuid, n: usize) -> String {
    format!("lic-{}-{n}", short(t))
}

fn binding(t: Uuid, n: usize) -> String {
    format!("bnd-{}-{n}", short(t))
}

/// A fake credential source: the n-th fetch for a tenant issues `lic(t, n)` / `binding(t, n)`, valid for `life(n)`.
/// Records (tenant, fresh) per fetch; a tenant in `fail` gets an error.
#[derive(Clone, Default)]
struct Issuer {
    fetches: Arc<Mutex<Vec<(Uuid, bool)>>>,
    fail: Arc<Mutex<Vec<Uuid>>>,
}

impl Issuer {
    fn hits(&self) -> usize {
        self.fetches.lock().unwrap().len()
    }

    fn hits_for(&self, t: Uuid) -> usize {
        self.fetches
            .lock()
            .unwrap()
            .iter()
            .filter(|(x, _)| *x == t)
            .count()
    }

    fn issue(
        &self,
        tenant: Uuid,
        fresh: bool,
        life: &dyn Fn(usize) -> Duration,
    ) -> anyhow::Result<McpCredentials> {
        if self.fail.lock().unwrap().contains(&tenant) {
            anyhow::bail!("credential source refused the MCP credentials: 409");
        }
        let n = {
            let mut f = self.fetches.lock().unwrap();
            f.push((tenant, fresh));
            f.iter().filter(|(x, _)| *x == tenant).count()
        };
        McpCredentials::new(
            &lic(tenant, n),
            &binding(tenant, n),
            Instant::now() + life(n),
        )
    }

    fn creds(&self, life: impl Fn(usize) -> Duration + Send + Sync + 'static) -> TenantCredentials {
        let me = self.clone();
        TenantCredentials::new(FnSource(move |tenant: Uuid, fresh: bool| {
            let out = me.issue(tenant, fresh, &life);
            async move { out }
        }))
    }
}

fn long(_: usize) -> Duration {
    Duration::from_secs(300)
}

#[test]
fn tools_list_result_is_parsed_into_mcp_tools() {
    use agent_api::mcp_client::client::{McpTool, mcp_tools_from_list_result};
    let result = json!({"tools": [
        {"name": "order_get_order", "description": "Get one order.", "inputSchema": {"type": "object", "required": ["order_id"]}, "_meta": {"operation": "orders.get"}},
        {"name": "faq_ask", "inputSchema": {"type": "object"}},
        {"description": "no name: skipped"}
    ]});
    let tools = mcp_tools_from_list_result(&result);
    assert_eq!(tools.len(), 2);
    assert_eq!(
        tools[0],
        McpTool {
            name: "order_get_order".into(),
            description: Some("Get one order.".into()),
            input_schema: json!({"type": "object", "required": ["order_id"]}),
            operation: Some("orders.get".into()),
            operations: Default::default(),
            output_schema: None,
            read_only: false,
        }
    );
    assert_eq!(tools[1].description, None);
    assert_eq!(tools[1].operation, None);
    assert!(mcp_tools_from_list_result(&json!({})).is_empty());
}

#[tokio::test]
async fn credentials_are_cached_per_tenant_until_close_to_expiry() {
    let issuer = Issuer::default();
    let creds = issuer.creds(long);
    assert_eq!(creds.get(A).await.unwrap().licence_token(), lic(A, 1));
    assert_eq!(creds.get(A).await.unwrap().licence_token(), lic(A, 1));
    assert_eq!(creds.get(B).await.unwrap().licence_token(), lic(B, 1));
    assert_eq!(issuer.hits_for(A), 1);
    assert_eq!(issuer.hits_for(B), 1);
}

#[tokio::test]
async fn expired_credentials_are_refetched() {
    let issuer = Issuer::default();
    let creds = issuer.creds(|_| Duration::ZERO);
    assert_eq!(creds.get(A).await.unwrap().licence_token(), lic(A, 1));
    assert_eq!(creds.get(A).await.unwrap().licence_token(), lic(A, 2));
    assert_eq!(issuer.hits(), 2);
}

#[tokio::test]
async fn renew_fetches_fresh_once_and_keeps_a_licence_another_request_already_renewed() {
    let issuer = Issuer::default();
    let creds = issuer.creds(long);
    let first = creds.get(A).await.unwrap();
    let renewed = creds.renew(A, first.licence_token()).await.unwrap();
    assert_eq!(renewed.licence_token(), lic(A, 2));
    // a second request refused with the same stale licence gets the renewed one, no third fetch
    let again = creds.renew(A, first.licence_token()).await.unwrap();
    assert_eq!(again.licence_token(), lic(A, 2));
    assert_eq!(*issuer.fetches.lock().unwrap(), [(A, false), (A, true)]);
}

#[test]
fn credentials_must_be_header_safe_and_never_print() {
    let at = Instant::now();
    assert!(McpCredentials::new("", "s", at).is_err());
    assert!(McpCredentials::new("l", "", at).is_err());
    assert!(McpCredentials::new("l\nX-Evil: 1", "s", at).is_err());
    assert!(McpCredentials::new("l", "b\nX-Evil: 1", at).is_err());
    let c = McpCredentials::new("secret-licence", "secret-binding", at).unwrap();
    let shown = format!("{c:?}");
    assert!(!shown.contains("secret"), "{shown}");
    let [(n1, v1), (n2, v2)] = c.headers();
    assert_eq!(n1.as_str(), TENANT_HEADER);
    assert_eq!(n1.as_str(), "x-tenant-id");
    assert_eq!(v1.to_str().unwrap(), "Bearer secret-licence");
    assert_eq!(n2.as_str(), BINDING_HEADER);
    assert_eq!(n2.as_str(), "x-mcp-binding");
    assert_eq!(v2.to_str().unwrap(), "secret-binding");
    assert!(v1.is_sensitive() && v2.is_sensitive());
    assert_eq!(c.licence_token(), "secret-licence");
    assert_eq!(c.binding_token(), "secret-binding");
}

#[test]
fn tool_error_prefix_sets_retryable_and_is_stripped() {
    use agent_api::mcp_client::client::tool_error_from_text;
    let e = tool_error_from_text("[retryable] demo-backends timed out");
    assert!(e.retryable);
    assert_eq!(e.message, "demo-backends timed out");
    let e = tool_error_from_text("[permanent] order ORD-1 not found");
    assert!(!e.retryable);
    assert_eq!(e.message, "order ORD-1 not found");
}

#[test]
fn tool_error_without_prefix_is_permanent() {
    let e = agent_api::mcp_client::client::tool_error_from_text("something broke");
    assert!(!e.retryable);
    assert_eq!(e.message, "something broke");
    assert_eq!(
        agent_api::mcp_client::client::tool_error_from_text("").message,
        "tool error"
    );
}

#[test]
fn mcp_protocol_error_is_permanent_and_keeps_the_session() {
    use agent_api::mcp_client::client::service_error_to_tool_error;
    use rmcp::service::ServiceError;
    let (e, reset) = service_error_to_tool_error(&ServiceError::McpError(
        rmcp::ErrorData::invalid_params("unknown tool", None),
    ));
    assert!(!e.retryable);
    assert!(!reset);
    let (e, reset) = service_error_to_tool_error(&ServiceError::TransportClosed);
    assert!(e.retryable);
    assert!(reset);
    let (e, reset) = service_error_to_tool_error(&ServiceError::Timeout {
        timeout: Duration::from_secs(1),
    });
    assert!(e.retryable);
    assert!(reset);
}

/// One POST the stub saw.
#[derive(Clone, Debug)]
struct Post {
    method: String,
    tenant: String,
    binding: String,
    authorization: Option<String>,
    strata_token: Option<String>,
    session: Option<String>,
    /// `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name` (the 2026-07-28 wire's per-request headers).
    protocol: Option<String>,
    mcp_method: Option<String>,
    mcp_name: Option<String>,
}

#[derive(Clone, Default)]
struct McpLog {
    posts: Arc<Mutex<Vec<Post>>>,
    /// An `initialize` carrying this `X-Tenant-Id` never gets an answer (a hung server, for that tenant).
    hang_initialize_for: Arc<Mutex<Option<String>>>,
}

impl McpLog {
    fn posts(&self) -> Vec<Post> {
        self.posts.lock().unwrap().clone()
    }

    /// The `X-Tenant-Id` of every POST of JSON-RPC `method`.
    fn tenants_of(&self, method: &str) -> Vec<String> {
        self.posts()
            .into_iter()
            .filter(|p| p.method == method)
            .map(|p| p.tenant)
            .collect()
    }
}

fn hdr(h: &axum::http::HeaderMap, name: &str) -> Option<String> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Minimal streamable-HTTP MCP server (JSON responses, no SSE) that records the credential headers per request. The
/// session id it hands out names the licence that opened the session (`sess-<licence>`).
async fn mcp_server(log: McpLog) -> String {
    use axum::http::{HeaderMap, Method, StatusCode};
    use axum::response::IntoResponse;
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::any(move |method: Method, headers: HeaderMap, body: String| {
            let log = log.clone();
            async move {
                if method != Method::POST {
                    // recorded too: a GET stream must never be opened on the stateless wire
                    log.posts.lock().unwrap().push(Post {
                        method: method.to_string(),
                        tenant: hdr(&headers, TENANT_HEADER).unwrap_or_default(),
                        binding: String::new(),
                        authorization: None,
                        strata_token: None,
                        session: None,
                        protocol: None,
                        mcp_method: None,
                        mcp_name: None,
                    });
                    return StatusCode::METHOD_NOT_ALLOWED.into_response();
                }
                let req: serde_json::Value = serde_json::from_str(&body).unwrap();
                let m = req["method"].as_str().unwrap_or("").to_string();
                let tenant = hdr(&headers, TENANT_HEADER).unwrap_or_default();
                log.posts.lock().unwrap().push(Post {
                    method: m.clone(),
                    tenant: tenant.clone(),
                    binding: hdr(&headers, BINDING_HEADER).unwrap_or_default(),
                    authorization: hdr(&headers, "authorization"),
                    strata_token: hdr(&headers, "x-strata-token"),
                    session: hdr(&headers, "mcp-session-id"),
                    protocol: hdr(&headers, "mcp-protocol-version"),
                    mcp_method: hdr(&headers, "mcp-method"),
                    mcp_name: hdr(&headers, "mcp-name"),
                });
                let id = req["id"].clone();
                let hang = log.hang_initialize_for.lock().unwrap().clone();
                if m == "server/discover" && hang.as_deref() == Some(tenant.as_str()) {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
                if m == "tools/call" && req["params"]["name"] == "slow" {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                if m == "tools/call" && req["params"]["name"] == "slow-broken" {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
                let reply = match m.as_str() {
                    "server/discover" => json!({"jsonrpc": "2.0", "id": id, "result": {
                        "resultType": "complete", "supportedVersions": ["2026-07-28"], "ttlMs": 0, "cacheScope": "private",
                        "capabilities": {"tools": {}},
                        "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "stub", "version": "0"}}}}),
                    "tools/call" if req["params"]["name"] == "nope" => json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32602, "message": "unknown tool nope"}}),
                    "tools/list" => {
                        tokio::time::sleep(Duration::from_secs(5)).await; // a hung server
                        json!({"jsonrpc": "2.0", "id": id, "result": {"tools": []}})
                    }
                    "tools/call" => json!({"jsonrpc": "2.0", "id": id, "result": {
                        "content": [{"type": "text", "text": "{\"ok\":true}"}], "isError": false}}),
                    _ => return StatusCode::ACCEPTED.into_response(), // notifications
                };
                let mut resp = axum::Json(reply).into_response();
                if m == "server/discover" {
                    let licence = tenant.strip_prefix("Bearer ").unwrap_or("none");
                    resp.headers_mut()
                        .insert("mcp-session-id", format!("sess-{licence}").parse().unwrap());
                }
                resp
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/mcp")
}

/// One shared identity for every tenant would be the hole. Each tenant's calls carry ITS licence and ITS binding
/// token, on its own session; tenant A's requests never carry B's token, and no request carries `Authorization` or
/// `X-Strata-Token`.
#[tokio::test]
async fn each_tenant_calls_with_its_own_licence_and_binding_on_its_own_session() {
    let issuer = Issuer::default();
    let log = McpLog::default();
    let mcp = Arc::new(HttpMcp::new(
        mcp_server(log.clone()).await,
        issuer.creds(long),
    ));
    let (a, b) = (mcp.for_tenant(A), mcp.for_tenant(B));
    for c in [&a, &b, &a, &b] {
        let v = c.call_tool("order_get_order", json!({})).await.unwrap();
        assert_eq!(v, json!({"ok": true}));
    }

    let bearer = |t: Uuid| format!("Bearer {}", lic(t, 1));
    assert_eq!(
        log.tenants_of("server/discover"),
        [bearer(A), bearer(B)],
        "one session per tenant"
    );
    assert_eq!(
        log.tenants_of("tools/call"),
        [bearer(A), bearer(B), bearer(A), bearer(B)]
    );
    for p in log.posts() {
        assert_eq!(p.authorization, None, "no shared identity: {p:?}");
        assert_eq!(p.strata_token, None, "no strata token: {p:?}");
        let t = if p.tenant == bearer(A) { A } else { B };
        assert_eq!(p.tenant, bearer(t), "{p:?}");
        assert_eq!(
            p.binding,
            binding(t, 1),
            "the binding token of the same tenant: {p:?}"
        );
        if let Some(s) = &p.session {
            assert_eq!(
                s,
                &format!("sess-{}", lic(t, 1)),
                "a session is only ever used with the licence that opened it: {p:?}"
            );
        }
    }
    assert_eq!((issuer.hits_for(A), issuer.hits_for(B)), (1, 1));
}

/// No credentials for a tenant: the call errors (retryable, not `auth_rejected`) and nothing is sent for it; other
/// tenants are unaffected and never borrow anyone's credentials.
#[tokio::test]
async fn a_tenant_without_credentials_gets_no_call_and_borrows_none() {
    let issuer = Issuer::default();
    issuer.fail.lock().unwrap().push(B);
    let log = McpLog::default();
    let mcp = Arc::new(HttpMcp::new(
        mcp_server(log.clone()).await,
        issuer.creds(long),
    ));
    mcp.for_tenant(A)
        .call_tool("order_get_order", json!({}))
        .await
        .unwrap();
    let before = log.posts().len();
    let err = mcp
        .for_tenant(B)
        .call_tool("order_get_order", json!({}))
        .await
        .unwrap_err();
    assert!(err.retryable, "{}", err.message);
    assert!(!err.message.contains(AUTH_REJECTED), "{}", err.message);
    assert!(err.message.contains("mcp credentials"), "{}", err.message);
    assert_eq!(log.posts().len(), before, "nothing was sent for B");
    let err = mcp.list_tools(B).await.unwrap_err();
    assert!(format!("{err:#}").contains("mcp credentials"), "{err:#}");
    assert_eq!(log.posts().len(), before, "nothing was sent for B");
}

/// One tenant's hung `initialize` is bounded by `connect_timeout` and never holds up another tenant's calls.
#[tokio::test]
async fn a_hung_session_open_for_one_tenant_does_not_block_another() {
    let issuer = Issuer::default();
    let log = McpLog::default();
    *log.hang_initialize_for.lock().unwrap() = Some(format!("Bearer {}", lic(A, 1)));
    let mut mcp = HttpMcp::new(mcp_server(log.clone()).await, issuer.creds(long));
    mcp.connect_timeout = Duration::from_millis(700);
    let mcp = Arc::new(mcp);
    let hung = {
        let a = mcp.for_tenant(A);
        tokio::spawn(async move { a.call_tool("order_get_order", json!({})).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await; // A is opening its session
    let started = Instant::now();
    mcp.for_tenant(B)
        .call_tool("order_get_order", json!({}))
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "B waited on A: {:?}",
        started.elapsed()
    );
    let err = tokio::time::timeout(Duration::from_secs(3), hung)
        .await
        .expect("the connect timeout bounds A")
        .unwrap()
        .unwrap_err();
    assert!(err.retryable, "{}", err.message);
    assert!(
        err.message.contains("no answer within 700 ms"),
        "{}",
        err.message
    );
}

/// Concurrent cold starts of one tenant share one credentials fetch.
#[tokio::test]
async fn concurrent_cold_fetches_of_one_tenant_are_single_flight() {
    let calls = Arc::new(Mutex::new(0usize));
    let n = calls.clone();
    let creds = Arc::new(TenantCredentials::new(FnSource(
        move |t: Uuid, _fresh: bool| {
            *n.lock().unwrap() += 1;
            async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                McpCredentials::new(&lic(t, 1), &binding(t, 1), Instant::now() + long(1))
            }
        },
    )));
    let mut tasks = vec![];
    for _ in 0..8 {
        let c = creds.clone();
        tasks.push(tokio::spawn(async move { c.get(A).await.unwrap() }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap().licence_token(), lic(A, 1));
    }
    creds.get(B).await.unwrap();
    assert_eq!(*calls.lock().unwrap(), 2, "one fetch for A, one for B");
}

#[tokio::test]
async fn http_mcp_reconnects_with_the_new_licence_and_classifies_protocol_errors() {
    let issuer = Issuer::default();
    let log = McpLog::default();
    // credentials that are already due: every call fetches a new licence
    let mcp = Arc::new(HttpMcp::new(
        mcp_server(log.clone()).await,
        issuer.creds(|_| Duration::ZERO),
    ));
    let a = mcp.for_tenant(A);

    let v = a
        .call_tool("order_get_order", json!({"order_id": "X"}))
        .await
        .unwrap();
    assert_eq!(v, json!({"ok": true}));
    a.call_tool("order_get_order", json!({})).await.unwrap();

    let want = [
        format!("Bearer {}", lic(A, 1)),
        format!("Bearer {}", lic(A, 2)),
    ];
    assert_eq!(
        log.tenants_of("server/discover"),
        want,
        "new initialize per rotated licence: {:?}",
        log.posts()
    );
    assert_eq!(log.tenants_of("tools/call"), want);

    let err = a.call_tool("nope", json!({})).await.unwrap_err();
    assert!(
        !err.retryable,
        "JSON-RPC error must be permanent: {}",
        err.message
    );
}

#[tokio::test]
async fn licence_rotation_keeps_in_flight_calls_and_the_fresh_session() {
    let issuer = Issuer::default();
    let log = McpLog::default();
    // licence 1 is due at once (the next call rotates); later ones live 300 s
    let mcp = Arc::new(HttpMcp::new(
        mcp_server(log.clone()).await,
        issuer.creds(|n| {
            if n == 1 {
                Duration::ZERO
            } else {
                Duration::from_secs(300)
            }
        }),
    ));

    let slow = {
        let a = mcp.for_tenant(A);
        tokio::spawn(async move { a.call_tool("slow", json!({})).await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await; // the slow call is in flight on licence 1
    let a = mcp.for_tenant(A);
    let rotated = a.call_tool("order_get_order", json!({})).await;
    let slow = slow.await.unwrap();
    assert!(
        slow.is_ok(),
        "in-flight call survives rotation: {:?}",
        slow.err().map(|e| e.message)
    );
    assert!(rotated.is_ok(), "{:?}", rotated.err().map(|e| e.message));

    // The fresh session and licence are still in place: no third fetch, no third initialize.
    a.call_tool("order_get_order", json!({})).await.unwrap();
    assert_eq!(
        log.tenants_of("server/discover"),
        [
            format!("Bearer {}", lic(A, 1)),
            format!("Bearer {}", lic(A, 2))
        ],
        "{:?}",
        log.posts()
    );
    assert_eq!(issuer.hits(), 2, "fresh licence kept");
}

#[tokio::test]
async fn late_transport_failure_on_a_rotated_out_session_keeps_the_fresh_one() {
    let issuer = Issuer::default();
    let log = McpLog::default();
    let mcp = Arc::new(HttpMcp::new(
        mcp_server(log.clone()).await,
        issuer.creds(|n| {
            if n == 1 {
                Duration::ZERO
            } else {
                Duration::from_secs(300)
            }
        }),
    ));
    let broken = {
        let a = mcp.for_tenant(A);
        tokio::spawn(async move { a.call_tool("slow-broken", json!({})).await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    let a = mcp.for_tenant(A);
    a.call_tool("order_get_order", json!({})).await.unwrap(); // rotates to licence 2
    let err = broken.await.unwrap().unwrap_err();
    assert!(
        err.retryable,
        "transport failure is retryable: {}",
        err.message
    );

    a.call_tool("order_get_order", json!({})).await.unwrap();
    assert_eq!(
        log.tenants_of("server/discover"),
        [
            format!("Bearer {}", lic(A, 1)),
            format!("Bearer {}", lic(A, 2))
        ],
        "{:?}",
        log.posts()
    );
    assert_eq!(issuer.hits(), 2, "fresh licence not invalidated");
}

#[tokio::test]
async fn list_tools_is_bounded_by_a_timeout() {
    let mut mcp = HttpMcp::new(
        mcp_server(McpLog::default()).await,
        Issuer::default().creds(long),
    );
    mcp.list_timeout = Duration::from_millis(200);
    let started = Instant::now();
    let err = mcp.list_tools(A).await.unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(
        format!("{err:#}").contains("no answer within 200 ms"),
        "{err:#}"
    );
}

/// One request seen by `RotatingMcp`: (HTTP method, JSON-RPC method or "", X-Tenant-Id, status).
type Seen = (String, String, String, u16);

/// A streamable-HTTP MCP server whose accepted licence can be rotated (one licence revoked, another minted). `GET /mcp` is a long-lived SSE stream that ends when `rotate_to` fires, so the client has to reconnect
/// it. A wrong licence is 401 with `WWW-Authenticate`, as nonagenticai_mcp_server answers.
struct RotatingMcp {
    accepted: Arc<Mutex<String>>,
    restart: tokio::sync::watch::Sender<u32>,
    seen: Arc<Mutex<Vec<Seen>>>,
    url: String,
}

impl RotatingMcp {
    async fn start(accepted: &str) -> Self {
        use axum::body::Body;
        use axum::http::{HeaderMap, Method, StatusCode, header};
        use axum::response::IntoResponse;
        let accepted = Arc::new(Mutex::new(accepted.to_string()));
        let (restart, restart_rx) = tokio::sync::watch::channel(0u32);
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let (acc, log) = (accepted.clone(), seen.clone());
        let app = axum::Router::new().route(
            "/mcp",
            axum::routing::any(move |method: Method, headers: HeaderMap, body: String| {
                let (acc, log, mut restart_rx) = (acc.clone(), log.clone(), restart_rx.clone());
                async move {
                    let tenant = hdr(&headers, TENANT_HEADER).unwrap_or_default();
                    let req: serde_json::Value =
                        serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
                    let m = req["method"].as_str().unwrap_or("").to_string();
                    let ok = tenant == format!("Bearer {}", acc.lock().unwrap())
                        && headers.get(header::AUTHORIZATION).is_none();
                    let status = if !ok {
                        401
                    } else if method == Method::GET || req.get("id").is_some() {
                        200
                    } else {
                        202
                    };
                    log.lock()
                        .unwrap()
                        .push((method.to_string(), m.clone(), tenant, status));
                    if !ok {
                        return (
                            StatusCode::UNAUTHORIZED,
                            [(header::WWW_AUTHENTICATE, "Bearer")],
                        )
                            .into_response();
                    }
                    if method == Method::GET {
                        restart_rx.borrow_and_update();
                        let stream = futures_util::stream::unfold(
                            (0u8, restart_rx),
                            |(n, mut rx)| async move {
                                if n == 0 {
                                    let ping: Result<_, std::convert::Infallible> =
                                        Ok(axum::body::Bytes::from_static(b": ping\n\n"));
                                    return Some((ping, (1, rx)));
                                }
                                let _ = rx.changed().await; // the licence rotated: end the stream
                                None
                            },
                        );
                        return (
                            [(header::CONTENT_TYPE, "text/event-stream")],
                            Body::from_stream(stream),
                        )
                            .into_response();
                    }
                    let id = req["id"].clone();
                    let reply = match m.as_str() {
                        "server/discover" => json!({"jsonrpc": "2.0", "id": id, "result": {
                            "resultType": "complete", "supportedVersions": ["2026-07-28"], "ttlMs": 0, "cacheScope": "private",
                            "capabilities": {"tools": {}},
                            "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "rotating", "version": "0"}}}}),
                        "tools/call" => json!({"jsonrpc": "2.0", "id": id, "result": {
                            "content": [{"type": "text", "text": "{\"ok\":true}"}], "isError": false}}),
                        _ => return StatusCode::ACCEPTED.into_response(),
                    };
                    let mut resp = axum::Json(reply).into_response();
                    if m == "server/discover" {
                        resp.headers_mut()
                            .insert("mcp-session-id", "sess-1".parse().unwrap());
                    }
                    resp
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            accepted,
            restart,
            seen,
            url: format!("http://{addr}/mcp"),
        }
    }

    /// Only `licence` is accepted from now on, and open SSE streams end.
    fn rotate_to(&self, licence: &str) {
        *self.accepted.lock().unwrap() = licence.to_string();
        self.restart.send_modify(|g| *g += 1);
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// (X-Tenant-Id, status) of every `tools/call`.
    fn calls(&self) -> Vec<(String, u16)> {
        self.seen()
            .into_iter()
            .filter(|(_, m, _, _)| m == "tools/call")
            .map(|(_, _, t, s)| (t, s))
            .collect()
    }
}

/// The platform server's wire is the stateless 2026-07-28 one (owner 2026-10-31): `server/discover` opens the
/// session (never `initialize`), every request names the protocol version and its method (and a tool call its tool)
/// in headers, and no GET stream is ever opened.
#[tokio::test]
async fn the_platform_wire_is_2026_07_28_discover_without_initialize_or_a_get_stream() {
    let issuer = Issuer::default();
    let log = McpLog::default();
    let mcp = Arc::new(HttpMcp::new(
        mcp_server(log.clone()).await,
        issuer.creds(long),
    ));
    let v = mcp
        .for_tenant(A)
        .call_tool("order_get_order", json!({}))
        .await
        .unwrap();
    assert_eq!(v, json!({"ok": true}));
    let posts = log.posts();
    let methods: Vec<&str> = posts.iter().map(|p| p.method.as_str()).collect();
    assert_eq!(methods, ["server/discover", "tools/call"], "{posts:?}");
    for p in &posts {
        assert_eq!(p.protocol.as_deref(), Some("2026-07-28"), "{p:?}");
        assert_eq!(p.mcp_method.as_deref(), Some(p.method.as_str()), "{p:?}");
    }
    assert_eq!(posts[1].mcp_name.as_deref(), Some("order_get_order"));
}

#[tokio::test]
async fn a_tool_call_answered_401_is_retried_once_with_fresh_credentials() {
    let issuer = Issuer::default();
    let server = RotatingMcp::start(&lic(A, 1)).await;
    let mut mcp = HttpMcp::new(server.url.clone(), issuer.creds(long));
    mcp.sse_retry_base = Duration::from_secs(60); // keep the stream's reconnect out of this test
    let a = Arc::new(mcp).for_tenant(A);

    a.call_tool("order_get_order", json!({})).await.unwrap();
    server.rotate_to(&lic(A, 2));
    let v = a.call_tool("order_get_order", json!({})).await;
    assert!(
        v.is_ok(),
        "first call after rotation: {:?}",
        v.err().map(|e| e.message)
    );
    let bearer = |n| format!("Bearer {}", lic(A, n));
    assert_eq!(
        server.calls(),
        [(bearer(1), 200), (bearer(1), 401), (bearer(2), 200)]
    );
    assert_eq!(issuer.hits(), 2);
}

/// `initialize` refused with the cached and with the fresh licence (the tenant holds no valid tenant-level mcp
/// licence): the call fails at once with the non-retryable `auth_rejected`, after exactly one fresh fetch.
#[tokio::test]
async fn an_initialize_refused_with_fresh_credentials_too_is_a_non_retryable_auth_rejected_error() {
    let issuer = Issuer::default();
    let server = RotatingMcp::start("a-licence-nobody-has").await;
    let mcp = Arc::new(HttpMcp::new(server.url.clone(), issuer.creds(long)));
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        mcp.for_tenant(A).call_tool("order_get_order", json!({})),
    )
    .await
    .expect("an auth rejection fails fast")
    .unwrap_err();
    assert!(!err.retryable, "{}", err.message);
    assert!(
        err.message.contains(AUTH_REJECTED) && err.message.contains("HTTP 401"),
        "{}",
        err.message
    );
    assert_eq!(*issuer.fetches.lock().unwrap(), [(A, false), (A, true)]);
    let inits: Vec<(String, u16)> = server
        .seen()
        .into_iter()
        .filter(|(_, m, _, _)| m == "server/discover")
        .map(|(_, _, t, s)| (t, s))
        .collect();
    assert_eq!(
        inits,
        [
            (format!("Bearer {}", lic(A, 1)), 401),
            (format!("Bearer {}", lic(A, 2)), 401)
        ],
        "refused, then refused again with the fresh licence"
    );
}

/// The same on a `tools/call` of an open session: refused with the cached and the fresh licence ⇒ `auth_rejected`.
#[tokio::test]
async fn a_tool_call_refused_with_fresh_credentials_too_is_non_retryable() {
    let issuer = Issuer::default();
    let server = RotatingMcp::start(&lic(A, 1)).await;
    let mut mcp = HttpMcp::new(server.url.clone(), issuer.creds(long));
    mcp.sse_retry_base = Duration::from_secs(60); // keep the stream's reconnect out of this test
    let a = Arc::new(mcp).for_tenant(A);

    a.call_tool("order_get_order", json!({})).await.unwrap();
    server.rotate_to("a-licence-this-tenant-never-gets");
    let err = a.call_tool("order_get_order", json!({})).await.unwrap_err();
    assert!(!err.retryable, "{}", err.message);
    assert!(err.message.contains(AUTH_REJECTED), "{}", err.message);
    let bearer = |n| format!("Bearer {}", lic(A, n));
    assert_eq!(
        server.calls(),
        [(bearer(1), 200), (bearer(1), 401), (bearer(2), 401)]
    );
    // Not `== 2`: dropping the refused session may DELETE it with a further renewal in the background.
    assert!(issuer.hits() >= 2);
}

/// `health()` is `GET {MCP_URL minus /mcp}/api/health`: Ok on 2xx, an error on anything else or no server.
#[tokio::test]
async fn health_probes_the_servers_public_health_route() {
    use axum::http::StatusCode;
    let hits = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = hits.clone();
    let app = axum::Router::new()
        .route(
            "/api/health",
            axum::routing::get(move || {
                let seen = seen.clone();
                async move {
                    seen.lock().unwrap().push("/api/health".into());
                    "ok"
                }
            }),
        )
        .route(
            "/down/api/health",
            axum::routing::get(|| async { StatusCode::SERVICE_UNAVAILABLE }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let issuer = Issuer::default();
    let up = HttpMcp::new(format!("http://{addr}/mcp"), issuer.creds(long));
    up.health().await.unwrap();
    assert_eq!(*hits.lock().unwrap(), ["/api/health"]);
    assert_eq!(issuer.hits(), 0, "health needs no credentials");

    let down = HttpMcp::new(format!("http://{addr}/down/mcp"), issuer.creds(long));
    let err = down.health().await.unwrap_err();
    assert!(format!("{err:#}").contains("unhealthy"), "{err:#}");

    let gone = HttpMcp::new("http://127.0.0.1:1/mcp".into(), issuer.creds(long));
    assert!(gone.health().await.is_err());
}

/// MCP tasks (`mcp_client::tasks`): the session declares the extension, a long-running call comes back as a task,
/// `tasks/get` / `tasks/update` / `tasks/cancel` reach the server, the answer carries the deciding human, an unknown
/// task is `Lost`, and a server without the extension gets a plain call.
mod tasks {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use agent_api::mcp_client::client::{FnSource, HttpMcp, McpCredentials, TenantCredentials};
    use agent_api::mcp_client::tasks::{
        StaffActor, TaskError, TaskOutcome, TaskStart, input_questions, outcome_of,
    };
    use serde_json::{Value, json};
    use uuid::Uuid;

    const A: Uuid = Uuid::from_u128(0xa);
    const TASKS: &str = "io.modelcontextprotocol/tasks";

    fn creds() -> TenantCredentials {
        TenantCredentials::new(FnSource(|tenant: Uuid, _fresh: bool| {
            let out = McpCredentials::new(
                &format!("lic-{tenant}"),
                &format!("bnd-{tenant}"),
                Instant::now() + Duration::from_secs(300),
            );
            async move { out }
        }))
    }

    #[derive(Clone, Default)]
    struct Log {
        bodies: Arc<Mutex<Vec<Value>>>,
    }

    impl Log {
        fn of(&self, method: &str) -> Vec<Value> {
            self.bodies
                .lock()
                .unwrap()
                .iter()
                .filter(|b| b["method"] == method)
                .cloned()
                .collect()
        }
    }

    fn task_wire(status: &str) -> Value {
        json!({"taskId": "t-1", "status": status, "createdAt": "2026-10-30T10:00:00Z",
               "lastUpdatedAt": "2026-10-30T10:00:01Z", "ttlMs": 86_400_000, "pollIntervalMs": 2000})
    }

    /// A stub MCP server. `with_tasks` decides whether `initialize` advertises the extension. `tools/call` of `long`
    /// returns a task; `tasks/get` of `t-1` walks working → input_required → completed; any other task id is -32602.
    async fn server(log: Log, with_tasks: bool) -> String {
        use axum::http::{Method, StatusCode};
        use axum::response::IntoResponse;
        let polls = Arc::new(Mutex::new(0usize));
        let app = axum::Router::new().route(
            "/mcp",
            axum::routing::any(move |method: Method, body: String| {
                let log = log.clone();
                let polls = polls.clone();
                async move {
                    if method != Method::POST {
                        return StatusCode::METHOD_NOT_ALLOWED.into_response();
                    }
                    let req: Value = serde_json::from_str(&body).unwrap();
                    log.bodies.lock().unwrap().push(req.clone());
                    let id = req["id"].clone();
                    let m = req["method"].as_str().unwrap_or("");
                    let ok = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
                    let reply = match m {
                        "server/discover" => {
                            let mut caps = json!({"tools": {}});
                            if with_tasks {
                                caps["extensions"] = json!({TASKS: {}});
                            }
                            ok(json!({"resultType": "complete", "supportedVersions": ["2026-07-28"], "ttlMs": 0, "cacheScope": "private", "capabilities": caps,
                                      "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "stub", "version": "0"}}}))
                        }
                        "tools/call" if req["params"]["name"] == "long" && with_tasks => {
                            let mut t = task_wire("working");
                            t["resultType"] = json!("task");
                            ok(t)
                        }
                        "tools/call" => ok(json!({"content": [], "isError": false,
                            "structuredContent": {"run_id": "r-1", "status": "queued", "result": null, "error": null}})),
                        "tasks/get" if req["params"]["taskId"] == "t-1" => {
                            let n = {
                                let mut p = polls.lock().unwrap();
                                *p += 1;
                                *p
                            };
                            let mut t = task_wire(match n {
                                1 => "working",
                                2 => "input_required",
                                _ => "completed",
                            });
                            t["resultType"] = json!("complete");
                            if n == 2 {
                                t["inputRequests"] = json!({"t-1:00000000000000aa": {"method": "elicitation/create",
                                    "params": {"message": "Deploy?", "requestedSchema": {"type": "object",
                                        "required": ["decision"], "properties": {"decision": {"type": "string",
                                        "enum": ["approved", "rejected"]}, "reason": {"type": "string"}}}}}});
                            }
                            if n >= 3 {
                                t["result"] = json!({"content": [], "isError": false,
                                    "structuredContent": {"run_id": "r-1", "status": "done", "result": {"ok": 1}, "error": null}});
                            }
                            ok(t)
                        }
                        "tasks/get" | "tasks/update" | "tasks/cancel" if req["params"]["taskId"] != "t-1" => json!({
                            "jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "unknown task"}}),
                        "tasks/update" | "tasks/cancel" => ok(json!({"resultType": "complete"})),
                        _ => return StatusCode::ACCEPTED.into_response(),
                    };
                    axum::Json(reply).into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/mcp")
    }

    /// Whether a request declared the extension: in `initialize`'s capabilities, or in the per-request client
    /// capabilities `_meta` when the protocol has no handshake.
    fn declares_tasks(body: &Value) -> bool {
        body["params"]["capabilities"]["extensions"]
            .get(TASKS)
            .is_some()
            || body["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"]["extensions"]
                .get(TASKS)
                .is_some()
    }

    #[tokio::test]
    async fn a_long_running_call_is_a_task_polled_answered_by_a_human_and_cancelled() {
        let log = Log::default();
        let mcp = HttpMcp::new(server(log.clone(), true).await, creds());

        // C1: the session declares the extension.
        let starter = StaffActor {
            tenant: A,
            staff_user: "staff-1".into(),
        };
        let started = mcp
            .start_task(&starter, "long", json!({"x": 1}))
            .await
            .unwrap();
        // C9: the tools/call names the person the task starts for (fr refuses a project without one).
        assert_eq!(
            log.of("tools/call")[0]["params"]["_meta"]["ai.nonagentic/actor"],
            json!({"channel": "mcp", "address": format!("agent:{A}"), "on_behalf_of": "staff-1"})
        );
        assert!(
            log.of("server/discover")
                .iter()
                .chain(log.of("tools/call").iter())
                .any(declares_tasks),
            "the tasks extension is declared: {:?}",
            log.bodies.lock().unwrap()
        );

        // C2: a task handle.
        let TaskStart::Task(h) = started else {
            panic!("expected a task, got {started:?}")
        };
        assert_eq!(h.task_id, "t-1");
        assert_eq!(h.status, "working");
        assert_eq!(h.poll_interval_ms, Some(2000));
        assert_eq!(h.ttl_ms, Some(86_400_000));

        // C3: polling walks the states.
        let s = mcp.get_task(A, "t-1").await.unwrap();
        assert_eq!(s.status, "working");
        assert!(outcome_of(&s).is_none());
        let s = mcp.get_task(A, "t-1").await.unwrap();
        assert_eq!(s.status, "input_required");
        let q = input_questions(&s.detail);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].key, "t-1:00000000000000aa");

        // C4: the answer carries the deciding human in `_meta`.
        let actor = StaffActor {
            tenant: A,
            staff_user: "staff-7".into(),
        };
        mcp.answer_task(
            &actor,
            "t-1",
            &q[0].key,
            json!({"action": "accept", "content": {"decision": "approved"}}),
        )
        .await
        .unwrap();
        let upd = log.of("tasks/update");
        assert_eq!(upd.len(), 1);
        assert_eq!(upd[0]["params"]["taskId"], "t-1");
        assert_eq!(
            upd[0]["params"]["inputResponses"]["t-1:00000000000000aa"],
            json!({"action": "accept", "content": {"decision": "approved"}})
        );
        assert_eq!(
            upd[0]["params"]["_meta"]["ai.nonagentic/actor"],
            json!({"channel": "mcp", "address": format!("agent:{A}"), "on_behalf_of": "staff-7"})
        );

        // C5: cancel is an ack.
        mcp.cancel_task(A, "t-1").await.unwrap();
        assert_eq!(log.of("tasks/cancel")[0]["params"]["taskId"], "t-1");

        // C6: completed → the run envelope, as call_tool would give it.
        let s = mcp.get_task(A, "t-1").await.unwrap();
        assert_eq!(
            outcome_of(&s),
            Some(TaskOutcome::Completed(Ok(
                json!({"run_id": "r-1", "status": "done", "result": {"ok": 1}, "error": null})
            )))
        );

        // -32602: the task is lost, on every task method.
        assert!(matches!(
            mcp.get_task(A, "gone").await,
            Err(TaskError::Lost(_))
        ));
        assert!(matches!(
            mcp.cancel_task(A, "gone").await,
            Err(TaskError::Lost(_))
        ));
        assert!(matches!(
            mcp.answer_task(&actor, "gone", "k", json!({"action": "decline"}))
                .await,
            Err(TaskError::Lost(_))
        ));

        // A short call through start_task completes at once.
        let done = mcp.start_task(&starter, "quick", json!({})).await.unwrap();
        assert_eq!(
            done,
            TaskStart::Completed(
                json!({"run_id": "r-1", "status": "queued", "result": null, "error": null})
            )
        );
    }

    /// C7: a server that does not advertise the extension gets today's plain call and its queued envelope.
    #[tokio::test]
    async fn without_the_extension_a_long_running_call_is_a_plain_call() {
        let log = Log::default();
        let mcp = HttpMcp::new(server(log.clone(), false).await, creds());
        let starter = StaffActor {
            tenant: A,
            staff_user: "staff-1".into(),
        };
        let started = mcp.start_task(&starter, "long", json!({})).await.unwrap();
        assert_eq!(
            started,
            TaskStart::Completed(
                json!({"run_id": "r-1", "status": "queued", "result": null, "error": null})
            )
        );
        assert_eq!(log.of("tools/call").len(), 1);
    }
}

// ---- a stored-token binding follows the published catalog (AgentMcpSource::credentials) ----------
//
// The MCP server re-pins a binding to the latest published toolkit versions only on `bindings/ensure`,
// always keeping the token when `rotate` is false. These tests drive `credentials()` end to end over a
// real Postgres (the tenant's sealed binding row) and a loopback fake of the MCP internal API, so a
// regression in the call site fails them.
mod binding_refresh {
    use crate::common;

    use std::sync::{Arc, Mutex};

    use agent_api::config::BundleMinterConfig;
    use agent_api::connectors::custody;
    use agent_api::mcp_client::client::CredentialSource;
    use agent_api::mcp_client::creds::{AgentMcpSource, binding_aad};
    use agent_api::tenants::app_licence::BundleMinter;
    use base64::Engine as _;
    use serde_json::{Value, json};
    use uuid::Uuid;

    #[derive(Clone, Copy)]
    enum Reply {
        AlreadyIssued,
        Issued,
        Fail,
    }

    /// A fake MCP internal API: `/token` (the minter client) and `bindings/ensure`, whose answer is `reply`
    /// and whose request bodies are recorded.
    async fn fake_mcp(reply: Reply) -> (String, Arc<Mutex<Vec<Value>>>) {
        use axum::response::IntoResponse;
        let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let state = seen.clone();
        let app = axum::Router::new()
            .route(
                "/token",
                axum::routing::post(|| async {
                    axum::Json(json!({"access_token": "svc", "expires_in": 600}))
                }),
            )
            .route(
                "/mcp/internal/tenants/{t}/bindings/ensure",
                axum::routing::post(move |body: axum::body::Bytes| {
                    let state = state.clone();
                    async move {
                        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                        state.lock().unwrap().push(v);
                        match reply {
                            Reply::AlreadyIssued => (
                                axum::http::StatusCode::OK,
                                axum::Json(json!({"binding_id": "b-refresh", "token_already_issued": true})),
                            )
                                .into_response(),
                            Reply::Issued => (
                                axum::http::StatusCode::CREATED,
                                axum::Json(json!({"binding_id": "b-refresh", "token": "bnd-new"})),
                            )
                                .into_response(),
                            Reply::Fail => {
                                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response()
                            }
                        }
                    }
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    /// A source pointing at `base` and at a fake licences minter that holds `lic-mcp` for the tenant.
    fn source(e: &common::TestEnv, cfg: &BundleMinterConfig, base: &str) -> AgentMcpSource {
        AgentMcpSource::new(
            e.state.db.clone(),
            e.state.tenant_keyring.clone(),
            Arc::new(BundleMinter::new(Some(cfg.clone()), reqwest::Client::new())),
            Some(cfg.clone()),
            reqwest::Client::new(),
            base,
        )
    }

    #[tokio::test]
    async fn a_stored_binding_is_re_ensured_and_keeps_or_stores_the_right_token() {
        use sqlx::Row as _;

        // The tenant DEK the binding token is sealed under.
        let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        let e = common::env_with(|m| {
            m.insert("TENANT_DATA_KEYS".into(), format!(r#"{{"k1":"{key}"}}"#));
            m.insert("TENANT_DATA_KEY_CURRENT".into(), "k1".into());
        })
        .await;
        assert!(e.state.tenant_keyring.is_some(), "the tenant keyring");

        let tenant = Uuid::new_v4();
        common::insert_tenant(&e.owner, tenant, "Refresh").await;
        let lic = common::minter::start();
        lic.grant(tenant, "mcp", "lic-refresh");
        let cfg = BundleMinterConfig {
            licenses_url: lic.url.clone(),
            token_url: format!("{}/token", lic.url),
            client_id: common::minter::CLIENT_ID.into(),
            client_secret: common::minter::CLIENT_SECRET.into(),
            bundle: "agent".into(),
            ttl_minutes: 15,
        };

        // The stored state a tenant has after its first ensure: one sealed binding token.
        let aad = binding_aad(tenant, "b-refresh");
        let mut fields = serde_json::Map::new();
        fields.insert("token".into(), json!("bnd-stored"));
        let mut tx = e.owner.begin().await.unwrap();
        agent_api::tenancy::set_tenant(&mut tx, tenant)
            .await
            .unwrap();
        let blob = custody::seal_fields(
            &mut tx,
            e.state.tenant_keyring.as_deref(),
            tenant,
            &aad,
            &fields,
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tenant_mcp_binding \
                 (tenant_id, binding_id, sealed_token, nonce, dek_version, fingerprint, licence_id, product) \
             VALUES ($1, $2, $3, $4, $5, $6, 'lic-mcp', 'agent')",
        )
        .bind(tenant)
        .bind("b-refresh")
        .bind(&blob.ciphertext)
        .bind(&blob.nonce)
        .bind(blob.dek_version)
        .bind(&blob.fingerprint)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();

        // 1. The server says the token was issued before: it is kept, one ensure ran, and nothing rotates.
        let (base, seen) = fake_mcp(Reply::AlreadyIssued).await;
        let creds = source(&e, &cfg, &base).fetch(tenant, false).await.unwrap();
        assert_eq!(creds.binding_token(), "bnd-stored");
        let bodies = seen.lock().unwrap().clone();
        assert_eq!(bodies.len(), 1, "the stored-token path must call ensure");
        assert_eq!(bodies[0]["product"], json!("agent"));
        assert_ne!(bodies[0].get("rotate").and_then(Value::as_bool), Some(true));

        // 2. The server issues a token on the re-ensure: it is stored sealed and used.
        let (base, seen) = fake_mcp(Reply::Issued).await;
        let creds = source(&e, &cfg, &base).fetch(tenant, false).await.unwrap();
        assert_eq!(creds.binding_token(), "bnd-new");
        assert_eq!(seen.lock().unwrap().len(), 1);
        let row = sqlx::query(
            "SELECT sealed_token, nonce, dek_version FROM tenant_mcp_binding WHERE tenant_id = $1",
        )
        .bind(tenant)
        .fetch_one(&e.owner)
        .await
        .unwrap();
        let sealed: Vec<u8> = row.get("sealed_token");
        let nonce: Vec<u8> = row.get("nonce");
        let dek_version: i32 = row.get("dek_version");
        let mut tx = e.owner.begin().await.unwrap();
        agent_api::tenancy::set_tenant(&mut tx, tenant)
            .await
            .unwrap();
        let opened = custody::open_fields(
            &mut tx,
            e.state.tenant_keyring.as_deref(),
            tenant,
            &aad,
            &sealed,
            &nonce,
            dek_version,
        )
        .await
        .unwrap();
        assert_eq!(opened.get("token").and_then(Value::as_str), Some("bnd-new"));
        tx.commit().await.unwrap();

        // 3. A failed re-ensure is swallowed: the stored token (the one stored in 2) is still used.
        let (base, seen) = fake_mcp(Reply::Fail).await;
        let creds = source(&e, &cfg, &base).fetch(tenant, false).await.unwrap();
        assert_eq!(creds.binding_token(), "bnd-new");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
