//! B7 T6: the netguard and the rest/graphql/stub transports against an axum mock on loopback (the test-only
//! `allow_loopback` policy), plus the pure netguard rules and `checkTarget` parity. No Postgres.
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_api::connectors::model::SideEffect;
use agent_api::connectors::transport::{
    AuthSpec, CallOptions, CallTarget, Transport, TransportKind, execute, scrub_error,
};
use agent_api::connectors::wire::{AuthKind, CallSecrets, OpError, RawResponse, RenderedRequest};
use agent_api::netguard::{self, MAX_BODY_BYTES, Policy, PrivateTarget, SAMPLE_CHARS};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{AppendHeaders, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{Value, json};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0xa1);
const OTHER: Uuid = Uuid::from_u128(0xb2);
const CLIENT_SECRET: &str = "cs-very-secret-0042";
const LIVE_KEY: &str = "k-live-123456";

#[derive(Clone, Default)]
struct Mock {
    /// Every recorded request (all routes but `/other`).
    hits: Arc<AtomicUsize>,
    other_hits: Arc<AtomicUsize>,
    mints: Arc<AtomicUsize>,
    resource_hits: Arc<AtomicUsize>,
    revoked: Arc<Mutex<Vec<String>>>,
    reject_all: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<HeaderMap>>>,
}

struct Up {
    url: String,
    port: u16,
    mock: Mock,
}

fn record(m: &Mock, h: &HeaderMap) {
    m.hits.fetch_add(1, Ordering::SeqCst);
    m.seen.lock().unwrap().push(h.clone());
}

async fn ok(State(m): State<Mock>, h: HeaderMap) -> Json<Value> {
    record(&m, &h);
    Json(json!({"ok": true}))
}

async fn redirect(State(m): State<Mock>, h: HeaderMap) -> impl IntoResponse {
    record(&m, &h);
    (StatusCode::FOUND, [(header::LOCATION, "/other")])
}

async fn other(State(m): State<Mock>) -> &'static str {
    m.other_hits.fetch_add(1, Ordering::SeqCst);
    "other"
}

async fn gql(State(m): State<Mock>, Path(case): Path<String>, h: HeaderMap) -> Json<Value> {
    record(&m, &h);
    Json(match case.as_str() {
        "unauth" => json!({"data": null, "errors": [
            {"message": "Invalid API key or access token", "extensions": {"code": "UNAUTHENTICATED"}}
        ]}),
        "generic" => json!({"data": null, "errors": [{"message": "Internal error"}]}),
        "user" => json!({"data": {"productCreate": {"product": null, "userErrors": [
            {"field": ["title"], "message": "Title can't be blank"}
        ]}}}),
        _ => json!({"data": {"shop": {"name": "acme"}}}),
    })
}

async fn token(State(m): State<Mock>, h: HeaderMap) -> Json<Value> {
    record(&m, &h);
    let n = m.mints.fetch_add(1, Ordering::SeqCst) + 1;
    Json(json!({"access_token": format!("tok-{n}"), "token_type": "Bearer", "expires_in": 3600}))
}

/// A token endpoint that echoes the request (which carries the client secret) in its error body.
async fn token_leak(State(m): State<Mock>, h: HeaderMap, body: String) -> (StatusCode, String) {
    record(&m, &h);
    (
        StatusCode::BAD_REQUEST,
        format!("invalid_client for request {body}"),
    )
}

async fn resource(State(m): State<Mock>, h: HeaderMap) -> (StatusCode, Json<Value>) {
    record(&m, &h);
    m.resource_hits.fetch_add(1, Ordering::SeqCst);
    let token = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    let revoked = m.revoked.lock().unwrap().contains(&token);
    if m.reject_all.load(Ordering::SeqCst) || revoked {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid_token"})),
        );
    }
    (StatusCode::OK, Json(json!({"echo": token})))
}

async fn big(State(m): State<Mock>, h: HeaderMap) -> String {
    record(&m, &h);
    "x".repeat(MAX_BODY_BYTES + 1024)
}

async fn slow() -> &'static str {
    tokio::time::sleep(Duration::from_secs(3)).await;
    "late"
}

async fn text() -> (StatusCode, String) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("<html>{}", "y".repeat(5000)),
    )
}

async fn limited() -> impl IntoResponse {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, "7")],
        Json(json!({"error": "slow down"})),
    )
}

async fn leak(State(m): State<Mock>, h: HeaderMap) -> Json<Value> {
    record(&m, &h);
    Json(json!({"echo": format!("the key is {LIVE_KEY}")}))
}

const CSRF_TOKEN: &str = "csrf-tok-7f3a91";
const SESSION: &str = "SAP_SESSIONID_X=sess-55aa";

/// SAP-style CSRF fetch: `x-csrf-token: Fetch` answers a token and two session cookies; anything else no token.
async fn csrf_fetch(State(m): State<Mock>, h: HeaderMap) -> impl IntoResponse {
    record(&m, &h);
    let fetch = h
        .get("x-csrf-token")
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"fetch"));
    if !fetch {
        return (StatusCode::OK, Json(json!({"d": {}}))).into_response();
    }
    (
        StatusCode::OK,
        AppendHeaders([
            (header::HeaderName::from_static("x-csrf-token"), CSRF_TOKEN),
            (
                header::SET_COOKIE,
                "SAP_SESSIONID_X=sess-55aa; path=/; HttpOnly",
            ),
            (header::SET_COOKIE, "sap-usercontext=sap-client=100; path=/"),
        ]),
        Json(json!({"d": {}})),
    )
        .into_response()
}

async fn csrf_forbidden(State(m): State<Mock>, h: HeaderMap) -> StatusCode {
    record(&m, &h);
    StatusCode::FORBIDDEN
}

async fn write(State(m): State<Mock>, h: HeaderMap) -> (StatusCode, Json<Value>) {
    record(&m, &h);
    m.resource_hits.fetch_add(1, Ordering::SeqCst);
    (StatusCode::CREATED, Json(json!({"id": "so-1"})))
}

async fn start() -> Up {
    let mock = Mock::default();
    let app = Router::new()
        .route("/ok", get(ok))
        .route("/redirect", get(redirect))
        .route("/other", get(other))
        .route("/gql/{case}", post(gql))
        .route("/token", post(token))
        .route("/token-leak", post(token_leak))
        .route("/resource", get(resource))
        .route("/big", get(big))
        .route("/slow", get(slow))
        .route("/text", get(text))
        .route("/limited", get(limited))
        .route("/leak", get(leak))
        .route("/csrf/fetch", get(csrf_fetch))
        .route("/csrf/forbidden", get(csrf_forbidden))
        .route("/write", post(write).put(write))
        .with_state(mock.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Up {
        url: format!("http://127.0.0.1:{port}"),
        port,
        mock,
    }
}

fn loopback() -> Transport {
    Transport::new(Policy {
        allow_loopback: true,
        private_targets: vec![],
    })
}

fn target(base: &str) -> CallTarget {
    CallTarget {
        tenant: TENANT,
        instance_id: "shop".into(),
        env: "prod",
        credential_version: 1,
        base_url: base.into(),
        stub: false,
    }
}

fn get_req(path: &str) -> RenderedRequest {
    RenderedRequest {
        method: "GET".into(),
        path: path.into(),
        ..Default::default()
    }
}

fn secrets(config: Value, secret: Value) -> CallSecrets {
    CallSecrets {
        config: config.as_object().cloned().unwrap_or_default(),
        secret: secret.as_object().cloned().unwrap_or_default(),
    }
}

fn none() -> AuthSpec {
    AuthSpec::new(AuthKind::None)
}

async fn run_with(
    t: &Transport,
    target: &CallTarget,
    auth: &AuthSpec,
    s: &CallSecrets,
    req: RenderedRequest,
    kind: TransportKind,
    timeout: Duration,
) -> Result<RawResponse, OpError> {
    let opts = CallOptions {
        kind,
        timeout,
        stub_fixtures: &[],
        side_effect: SideEffect::R,
    };
    execute(t, target, auth, s, req, opts).await
}

async fn run(
    t: &Transport,
    target: &CallTarget,
    auth: &AuthSpec,
    s: &CallSecrets,
    req: RenderedRequest,
) -> Result<RawResponse, OpError> {
    run_with(
        t,
        target,
        auth,
        s,
        req,
        TransportKind::Rest,
        Duration::from_secs(5),
    )
    .await
}

fn last_seen(m: &Mock) -> HeaderMap {
    m.seen.lock().unwrap().last().cloned().unwrap()
}

fn seen_header(m: &Mock, name: &str) -> String {
    last_seen(m)
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn private_target_refused_before_connect() {
    let up = start().await;
    let t = loopback();
    let p = up.port;
    for base in [
        format!("https://10.0.0.1:{p}"),
        format!("https://100.64.0.1:{p}"),
        format!("https://100.64.0.9:{p}"),
        format!("https://mock.shop.svc:{p}"),
        format!("https://mock.shop.svc.cluster.local:{p}"),
        format!("https://registry.internal:{p}"),
        format!("https://localhost:{p}"),
        format!("https://minio:{p}"),
        "https://169.254.169.254".to_string(),
        format!("https://[fd00::1]:{p}"),
        format!("https://[::ffff:10.0.0.1]:{p}"),
    ] {
        let e = run(
            &t,
            &target(&base),
            &none(),
            &CallSecrets::default(),
            get_req("/ok"),
        )
        .await
        .unwrap_err();
        assert_eq!(e.code, "invalid_target", "{base}");
    }
    // without the test-only allowance the loopback mock itself is a private target
    let strict = Transport::new(Policy::default());
    let e = run(
        &strict,
        &target(&format!("https://127.0.0.1:{p}")),
        &none(),
        &CallSecrets::default(),
        get_req("/ok"),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "invalid_target");
    assert_eq!(up.mock.hits.load(Ordering::SeqCst), 0, "nothing connected");
}

#[tokio::test]
async fn allow_list_admits_only_exact_host_port_scheme_and_tenant() {
    let up = start().await;
    let p = up.port;
    let policy = Policy::from_env_json(
        &json!([{"host": "127.0.0.1", "port": p, "scheme": "http", "tenants": [TENANT]}])
            .to_string(),
    )
    .unwrap();
    assert!(!policy.allow_loopback);
    let t = Transport::new(policy);
    let s = CallSecrets::default();

    let r = run(&t, &target(&up.url), &none(), &s, get_req("/ok"))
        .await
        .unwrap();
    assert_eq!(r.status, 200);

    let mut other_tenant = target(&up.url);
    other_tenant.tenant = OTHER;
    let e = run(&t, &other_tenant, &none(), &s, get_req("/ok"))
        .await
        .unwrap_err();
    assert_eq!(e.code, "invalid_url", "another tenant gets the full guard");
    for (base, code) in [
        (format!("http://127.0.0.1:{}", p ^ 1), "invalid_url"),
        (format!("https://127.0.0.1:{p}"), "invalid_target"),
        (format!("http://localhost:{p}"), "invalid_url"),
    ] {
        let e = run(&t, &target(&base), &none(), &s, get_req("/ok"))
            .await
            .unwrap_err();
        assert_eq!(e.code, code, "{base}");
    }
    assert_eq!(up.mock.hits.load(Ordering::SeqCst), 1);

    // the env parser (Frappe's in-cluster target)
    let frappe = Policy::from_env_json(
        r#"[{"host":"Frappe.FRAPPE.svc.cluster.local","port":8000,"scheme":"HTTP",
             "tenants":["00000000-0000-0000-0000-0000000000a1"]}]"#,
    )
    .unwrap();
    assert_eq!(
        frappe.private_targets,
        vec![PrivateTarget {
            host: "frappe.frappe.svc.cluster.local".into(),
            port: 8000,
            scheme: "http".into(),
            tenants: vec![TENANT],
        }]
    );
    let f = "frappe.frappe.svc.cluster.local";
    assert!(netguard::check_url(&format!("http://{f}:8000"), &frappe, TENANT).is_ok());
    assert!(netguard::check_url(&format!("http://{f}:8000/api"), &frappe, TENANT).is_ok());
    for (raw, tenant) in [
        (format!("http://{f}:8000"), OTHER),
        (format!("http://{f}:8001"), TENANT),
        (format!("https://{f}:8000"), TENANT),
        (format!("http://x.{f}:8000"), TENANT),
        (format!("http://{f}:8000/?a=1"), TENANT),
        (format!("http://u:p@{f}:8000"), TENANT),
    ] {
        assert!(netguard::check_url(&raw, &frappe, tenant).is_err(), "{raw}");
    }
    assert_eq!(Policy::from_env_json("  ").unwrap(), Policy::default());
    assert!(Policy::from_env_json("{not json").is_err());
    assert!(
        Policy::from_env_json(r#"[{"host":"x","port":1,"scheme":"ftp","tenants":["00000000-0000-0000-0000-0000000000a1"]}]"#)
            .is_err()
    );
    assert!(
        Policy::from_env_json(r#"[{"host":"x","port":1,"scheme":"http","tenants":[]}]"#).is_err()
    );
    assert!(
        Policy::from_env_json(r#"[{"host":"x","port":1,"scheme":"http","tenants":[],"extra":1}]"#)
            .is_err()
    );
}

#[tokio::test]
async fn redirect_not_followed() {
    let up = start().await;
    let r = run(
        &loopback(),
        &target(&up.url),
        &none(),
        &CallSecrets::default(),
        get_req("/redirect"),
    )
    .await
    .unwrap();
    assert_eq!(r.status, 302);
    assert_eq!(up.mock.other_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn graphql_200_with_errors_is_an_error() {
    let up = start().await;
    let t = loopback();
    let s = CallSecrets::default();
    let q = |case: &str| RenderedRequest {
        method: "GET".into(), // the transport always POSTs
        path: format!("/gql/{case}"),
        body: Some(json!({"query": "{ shop { name } }"})),
        ..Default::default()
    };
    let tg = target(&up.url);
    let auth = none();
    let gql = |req: RenderedRequest| {
        run_with(
            &t,
            &tg,
            &auth,
            &s,
            req,
            TransportKind::Graphql,
            Duration::from_secs(5),
        )
    };
    let e = gql(q("unauth")).await.unwrap_err();
    assert_eq!(e.code, "connector_auth_failed");
    assert!(!e.detail.contains("Invalid API key"), "no upstream text");
    assert_eq!(
        gql(q("generic")).await.unwrap_err().code,
        "upstream_unavailable"
    );
    assert_eq!(gql(q("user")).await.unwrap_err().code, "invalid_input");
    let r = gql(q("ok")).await.unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.body["data"]["shop"]["name"], "acme");
    let mut bare = q("ok");
    bare.body = Some(json!("{ shop { name } }"));
    assert_eq!(gql(bare).await.unwrap().status, 200);
    let mut empty = q("ok");
    empty.body = None;
    assert_eq!(
        gql(empty).await.unwrap_err().code,
        "connector_misconfigured"
    );
}

fn oauth_secrets(up: &Up, token_path: &str) -> CallSecrets {
    secrets(
        json!({"token_url": format!("{}{token_path}", up.url), "client_id": "cid"}),
        json!({"client_secret": CLIENT_SECRET}),
    )
}

#[tokio::test]
async fn oauth_token_cached_and_401_remints_once() {
    let up = start().await;
    let t = loopback();
    let auth = AuthSpec::new(AuthKind::Oauth2ClientCredentials);
    let s = oauth_secrets(&up, "/token");
    let tg = target(&up.url);
    let m = &up.mock;

    let r = run(&t, &tg, &auth, &s, get_req("/resource")).await.unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.body["echo"], "[redacted]", "the minted token is scrubbed");
    assert_eq!(seen_header(m, "authorization"), "Bearer tok-1");
    assert_eq!(m.mints.load(Ordering::SeqCst), 1);

    run(&t, &tg, &auth, &s, get_req("/resource")).await.unwrap();
    assert_eq!(m.mints.load(Ordering::SeqCst), 1, "cached");

    m.revoked.lock().unwrap().push("tok-1".into());
    let r = run(&t, &tg, &auth, &s, get_req("/resource")).await.unwrap();
    assert_eq!(r.status, 200, "401 → re-mint → retry");
    assert_eq!(m.mints.load(Ordering::SeqCst), 2);
    assert_eq!(seen_header(m, "authorization"), "Bearer tok-2");
    assert_eq!(m.resource_hits.load(Ordering::SeqCst), 4);

    m.reject_all.store(true, Ordering::SeqCst);
    let r = run(&t, &tg, &auth, &s, get_req("/resource")).await.unwrap();
    assert_eq!(r.status, 401, "a second 401 is the mapping engine's");
    assert_eq!(m.mints.load(Ordering::SeqCst), 3, "exactly one re-mint");
    assert_eq!(
        m.resource_hits.load(Ordering::SeqCst),
        6,
        "exactly one retry"
    );
}

#[tokio::test]
async fn credential_version_change_invalidates_token() {
    let up = start().await;
    let t = loopback();
    let auth = AuthSpec::new(AuthKind::Oauth2ClientCredentials);
    let s = oauth_secrets(&up, "/token");
    let m = &up.mock;
    let mut tg = target(&up.url);

    run(&t, &tg, &auth, &s, get_req("/resource")).await.unwrap();
    run(&t, &tg, &auth, &s, get_req("/resource")).await.unwrap();
    assert_eq!(m.mints.load(Ordering::SeqCst), 1);

    tg.credential_version = 2;
    run(&t, &tg, &auth, &s, get_req("/resource")).await.unwrap();
    assert_eq!(
        m.mints.load(Ordering::SeqCst),
        2,
        "a new credential mints anew"
    );
    assert_eq!(seen_header(m, "authorization"), "Bearer tok-2");

    tg.instance_id = "shop2".into();
    run(&t, &tg, &auth, &s, get_req("/resource")).await.unwrap();
    assert_eq!(m.mints.load(Ordering::SeqCst), 3, "per instance");

    // the token URL itself is netguarded
    let bad = secrets(
        json!({"token_url": "https://auth.internal/token", "client_id": "cid"}),
        json!({"client_secret": CLIENT_SECRET}),
    );
    tg.credential_version = 3;
    let e = run(&t, &tg, &auth, &bad, get_req("/resource"))
        .await
        .unwrap_err();
    assert_eq!(e.code, "invalid_target");
}

#[tokio::test]
async fn body_over_1mib_refused() {
    let up = start().await;
    let e = run(
        &loopback(),
        &target(&up.url),
        &none(),
        &CallSecrets::default(),
        get_req("/big"),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "upstream_unavailable");
    assert!(e.detail.contains("1 MiB"), "{}", e.detail);
}

#[tokio::test]
async fn timeout_maps_to_upstream_unavailable() {
    let up = start().await;
    let started = Instant::now();
    let e = run_with(
        &loopback(),
        &target(&up.url),
        &none(),
        &CallSecrets::default(),
        get_req("/slow"),
        TransportKind::Rest,
        Duration::from_millis(300),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "upstream_unavailable");
    assert!(e.detail.contains("timed out"), "{}", e.detail);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn stub_serves_fixtures_and_faults() {
    let fixtures = vec![
        json!({"name": "order.read ok", "status": 200, "outcome": "ok", "body": {"id": "o-1"}}),
        json!({"name": "not found", "status": 404, "outcome": "not_found", "body": {"errors": [{"message": "Not Found"}]}}),
        json!({"name": "expired credential", "status": 401, "outcome": "401 → connector_auth_failed", "body": {"errors": "Unauthorized"}}),
        json!({"name": "upstream timeout", "status": 0, "outcome": "timeout (fault injection)", "body": null}),
        json!({"name": "maintenance", "status": "503", "outcome": "upstream_unavailable", "body": null}),
    ];
    let t = Transport::new(Policy::default());
    let mut tg = target("stub://test_shop");
    tg.stub = true;
    let pick = |sel: Option<&str>| {
        let mut req = get_req("/orders/o-1");
        if let Some(s) = sel {
            req.headers.push(("X-Stub-Fixture".into(), s.into()));
        }
        req
    };
    let auth = none();
    let no_secrets = CallSecrets::default();
    let replay = |req: RenderedRequest| {
        execute(
            &t,
            &tg,
            &auth,
            &no_secrets,
            req,
            CallOptions {
                kind: TransportKind::Rest,
                timeout: Duration::from_secs(1),
                stub_fixtures: &fixtures,
                side_effect: SideEffect::R,
            },
        )
    };
    let r = replay(pick(None)).await.unwrap();
    assert_eq!((r.status, r.body.clone()), (200, json!({"id": "o-1"})));
    assert_eq!(replay(pick(Some("not_found"))).await.unwrap().status, 404);
    assert_eq!(replay(pick(Some("5xx"))).await.unwrap().status, 503);
    assert_eq!(
        replay(pick(Some("expired credential")))
            .await
            .unwrap()
            .status,
        401
    );
    assert_eq!(
        replay(pick(Some("timeout"))).await.unwrap_err().code,
        "upstream_unavailable"
    );
    assert_eq!(
        replay(pick(Some("nope"))).await.unwrap_err().code,
        "connector_misconfigured"
    );

    // the stub kind on a non-stub instance replays too, without touching the network
    let live = target("https://shop.example.com");
    let r = execute(
        &t,
        &live,
        &none(),
        &CallSecrets::default(),
        pick(None),
        CallOptions {
            kind: TransportKind::Stub,
            timeout: Duration::from_secs(1),
            stub_fixtures: &fixtures,
            side_effect: SideEffect::R,
        },
    )
    .await
    .unwrap();
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn secret_scrubbed_from_error_text() {
    let up = start().await;
    let t = loopback();

    // a token endpoint echoing the client secret in its error body
    let e = run(
        &t,
        &target(&up.url),
        &AuthSpec::new(AuthKind::Oauth2ClientCredentials),
        &oauth_secrets(&up, "/token-leak"),
        get_req("/resource"),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "connector_auth_failed");
    assert!(!e.to_string().contains(CLIENT_SECRET), "{e}");
    assert!(
        !e.to_string().contains("invalid_client"),
        "no upstream body: {e}"
    );

    // an upstream echoing the api key in a response body
    let s = secrets(json!({}), json!({"api_key": LIVE_KEY}));
    let r = run(
        &t,
        &target(&up.url),
        &AuthSpec::new(AuthKind::ApiKeyHeader),
        &s,
        get_req("/leak"),
    )
    .await
    .unwrap();
    assert!(!r.body.to_string().contains(LIVE_KEY), "{}", r.body);
    assert_eq!(r.body["echo"], "the key is [redacted]");

    // a missing secret names the field, never a value
    let mut auth = AuthSpec::new(AuthKind::ApiKeyHeader);
    auth.params.insert(
        "value_template".into(),
        "{{secret.nope}} {{secret.api_key}}".into(),
    );
    let e = run(&t, &target(&up.url), &auth, &s, get_req("/ok"))
        .await
        .unwrap_err();
    assert_eq!(e.code, "connector_auth_failed");
    assert!(e.detail.contains("secret.nope") && !e.detail.contains(LIVE_KEY));

    let scrubbed = scrub_error(
        OpError::new(
            "upstream_unavailable",
            format!("saw {LIVE_KEY} twice: {LIVE_KEY}"),
        ),
        &[LIVE_KEY.to_string()],
    );
    assert_eq!(scrubbed.detail, "saw [redacted] twice: [redacted]");
}

#[tokio::test]
async fn basic_and_api_key_header_auth_applied() {
    let up = start().await;
    let t = loopback();
    let tg = target(&up.url);
    let m = &up.mock;
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);

    let basic = AuthSpec::new(AuthKind::Basic);
    let s = secrets(json!({"username": "ann"}), json!({"password": "pw-1"}));
    run(&t, &tg, &basic, &s, get_req("/ok")).await.unwrap();
    assert_eq!(
        seen_header(m, "authorization"),
        format!("Basic {}", b64("ann:pw-1"))
    );

    let frappe = secrets(json!({}), json!({"api_key": "fk", "api_secret": "fs"}));
    run(&t, &tg, &basic, &frappe, get_req("/ok")).await.unwrap();
    assert_eq!(
        seen_header(m, "authorization"),
        format!("Basic {}", b64("fk:fs"))
    );

    let missing = secrets(json!({"username": "ann"}), json!({}));
    assert_eq!(
        run(&t, &tg, &basic, &missing, get_req("/ok"))
            .await
            .unwrap_err()
            .code,
        "connector_auth_failed"
    );

    let key = secrets(json!({"shop": "acme"}), json!({"api_key": "k1"}));
    run(
        &t,
        &tg,
        &AuthSpec::new(AuthKind::ApiKeyHeader),
        &key,
        get_req("/ok"),
    )
    .await
    .unwrap();
    assert_eq!(seen_header(m, "authorization"), "k1");

    let mut custom = AuthSpec::new(AuthKind::ApiKeyHeader);
    custom.params.insert("header".into(), "X-Api-Key".into());
    custom.params.insert(
        "value_template".into(),
        "Key {{secret.api_key}}@{{config.shop}}".into(),
    );
    run(&t, &tg, &custom, &key, get_req("/ok")).await.unwrap();
    assert_eq!(seen_header(m, "x-api-key"), "Key k1@acme");
    assert_eq!(seen_header(m, "authorization"), "");

    let bearer = secrets(json!({}), json!({"token": "bt-9"}));
    run(
        &t,
        &tg,
        &AuthSpec::new(AuthKind::BearerStatic),
        &bearer,
        get_req("/ok"),
    )
    .await
    .unwrap();
    assert_eq!(seen_header(m, "authorization"), "Bearer bt-9");

    // seed-template params: basic over username/password templates + static_headers (config placeholders only)
    let mut jira = AuthSpec::new(AuthKind::Basic);
    jira.params
        .insert("username_template".into(), "{{config.email}}".into());
    jira.params
        .insert("password_template".into(), "{{secret.api_token}}".into());
    jira.params.insert(
        "static_headers".into(),
        r#"{"Prefer":"return=representation","X-Org":"{{config.email}}"}"#.into(),
    );
    let js = secrets(
        json!({"email": "agent@northwind.example"}),
        json!({"api_token": "at-1"}),
    );
    run(&t, &tg, &jira, &js, get_req("/ok")).await.unwrap();
    assert_eq!(
        seen_header(m, "authorization"),
        format!("Basic {}", b64("agent@northwind.example:at-1"))
    );
    assert_eq!(seen_header(m, "prefer"), "return=representation");
    assert_eq!(seen_header(m, "x-org"), "agent@northwind.example");
    jira.params.insert(
        "static_headers".into(),
        r#"{"X-Leak":"{{secret.api_token}}"}"#.into(),
    );
    assert_eq!(
        run(&t, &tg, &jira, &js, get_req("/ok"))
            .await
            .unwrap_err()
            .code,
        "connector_misconfigured"
    );

    let before = m.hits.load(Ordering::SeqCst);
    let e = run(
        &t,
        &tg,
        &AuthSpec::new(AuthKind::BundleToken),
        &CallSecrets::default(),
        get_req("/ok"),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "not_applicable");
    assert_eq!(m.hits.load(Ordering::SeqCst), before);
}

#[tokio::test]
async fn http_scheme_refused() {
    let up = start().await;
    let strict = Transport::new(Policy::default());
    for (t, base) in [
        (&strict, "http://api.example.com".to_string()),
        (&strict, up.url.clone()),
        (&loopback(), "http://api.example.com".to_string()),
    ] {
        let e = run(
            t,
            &target(&base),
            &none(),
            &CallSecrets::default(),
            get_req("/ok"),
        )
        .await
        .unwrap_err();
        assert_eq!(e.code, "invalid_url", "{base}");
        assert_eq!(
            e.detail, "Only https:// base URLs are allowed for connector instances.",
            "{base}"
        );
    }
    assert_eq!(up.mock.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rate_limit_retry_after_and_non_json_body() {
    let up = start().await;
    let t = loopback();
    let r = run(
        &t,
        &target(&up.url),
        &none(),
        &CallSecrets::default(),
        get_req("/limited"),
    )
    .await
    .unwrap();
    assert_eq!((r.status, r.retry_after_secs), (429, Some(7)));

    let r = run(
        &t,
        &target(&up.url),
        &none(),
        &CallSecrets::default(),
        get_req("/text"),
    )
    .await
    .unwrap();
    assert_eq!(r.status, 500);
    let s = r.body.as_str().unwrap();
    assert!(s.starts_with("<html>"));
    assert_eq!(s.chars().count(), SAMPLE_CHARS);
}

/// Golden parity with `checkTarget` in `web/src/lib/connectors/ops.ts`: same code, same message.
#[test]
fn check_target_matches_ui_shape_rules() {
    let private = |h: &str| {
        Some((
            "invalid_target",
            format!(
                "{h} resolves to a non-public address (private, loopback, link-local or CGNAT). The runner only calls public hosts."
            ),
        ))
    };
    let url = |m: &str| Some(("invalid_url", m.to_string()));
    let cases: Vec<(&str, Option<(&str, String)>)> = vec![
        (
            "not a url",
            url("Enter a full URL such as https://shop.example.com."),
        ),
        (
            "",
            url("Enter a full URL such as https://shop.example.com."),
        ),
        (
            "http://shop.example.com",
            url("Only https:// base URLs are allowed for connector instances."),
        ),
        (
            "ftp://shop.example.com",
            url("Only https:// base URLs are allowed for connector instances."),
        ),
        (
            "https://u:p@shop.example.com",
            url("Put credentials in the credential fields, never in the URL."),
        ),
        (
            "https://shop.example.com/?a=1",
            url("A base URL has no query or fragment."),
        ),
        (
            "https://shop.example.com/#x",
            url("A base URL has no query or fragment."),
        ),
        ("https://shop.example.com/?", None),
        ("  https://shop.example.com/api/v2  ", None),
        ("https://100.128.0.1", None),
        ("https://172.32.0.1", None),
        ("https://minio", None),
        ("https://localhost", private("localhost")),
        ("https://app.localhost", private("app.localhost")),
        ("https://API.Internal", private("api.internal")),
        ("https://printer.local", private("printer.local")),
        ("https://x.svc", private("x.svc")),
        (
            "https://a.b.svc.cluster.local",
            private("a.b.svc.cluster.local"),
        ),
        ("https://10.1.2.3", private("10.1.2.3")),
        ("https://172.20.0.1", private("172.20.0.1")),
        ("https://192.168.0.1:8443", private("192.168.0.1")),
        ("https://169.254.169.254", private("169.254.169.254")),
        ("https://100.100.1.1", private("100.100.1.1")),
        ("https://127.0.0.1", private("127.0.0.1")),
        ("https://2130706433", private("127.0.0.1")),
        ("https://0.0.0.0", private("0.0.0.0")),
        ("https://[::1]", private("::1")),
        ("https://[fd00::1]", private("fd00::1")),
        ("https://[fe80::1]", private("fe80::1")),
        ("https://[::ffff:127.0.0.1]", private("::ffff:7f00:1")),
        ("https://[::ffff:10.0.0.1]", private("::ffff:a00:1")),
        ("https://[2001:db8::1]", private("2001:db8::1")),
        ("https://[2606:4700::1111]", None),
        ("https://fcbarcelona.example.com", None),
        ("https://fdm.example.com", None),
    ];
    for (raw, want) in cases {
        let got = netguard::check_target(raw).map(|e| (e.code, e.detail));
        assert_eq!(got, want, "{raw:?}");
    }
    // the server is stricter than the UI on what the UI lets through, never looser
    let strict = Policy::default();
    assert_eq!(
        netguard::check_url("https://minio", &strict, TENANT)
            .unwrap_err()
            .code,
        "invalid_target"
    );
    assert!(netguard::check_url("https://shop.example.com/api/v2", &strict, TENANT).is_ok());
}

fn op_call(side_effect: SideEffect) -> CallOptions<'static> {
    CallOptions {
        kind: TransportKind::Rest,
        timeout: Duration::from_secs(5),
        stub_fixtures: &[],
        side_effect,
    }
}

fn post_req(path: &str) -> RenderedRequest {
    RenderedRequest {
        method: "POST".into(),
        path: path.into(),
        body: Some(json!({"SalesOrderType": "OR"})),
        ..Default::default()
    }
}

fn csrf_auth(fetch_path: &str) -> AuthSpec {
    let mut a = AuthSpec::new(AuthKind::ApiKeyHeader);
    a.params.insert("header".into(), "APIKey".into());
    a.params.insert("csrf_fetch_path".into(), fetch_path.into());
    a
}

fn header_of(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned)
}

#[tokio::test]
async fn csrf_prefetch_sends_token_and_cookies_on_writes_only() {
    let up = start().await;
    let t = loopback();
    let tg = target(&up.url);
    let m = &up.mock;
    let s = secrets(json!({}), json!({"api_key": LIVE_KEY}));
    let auth = csrf_auth("/csrf/fetch");

    let r = execute(
        &t,
        &tg,
        &auth,
        &s,
        post_req("/write"),
        op_call(SideEffect::W),
    )
    .await
    .unwrap();
    assert_eq!(r.status, 201);
    let seen = m.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "one prefetch, one write");
    let (fetch, write) = (&seen[0], &seen[1]);
    assert_eq!(header_of(fetch, "x-csrf-token").as_deref(), Some("Fetch"));
    assert_eq!(
        header_of(fetch, "apikey").as_deref(),
        Some(LIVE_KEY),
        "prefetch is authenticated"
    );
    assert_eq!(header_of(fetch, "cookie"), None);
    assert_eq!(
        header_of(write, "x-csrf-token").as_deref(),
        Some(CSRF_TOKEN)
    );
    assert_eq!(
        header_of(write, "cookie").as_deref(),
        Some(format!("{SESSION}; sap-usercontext=sap-client=100").as_str())
    );
    assert_eq!(header_of(write, "apikey").as_deref(), Some(LIVE_KEY));

    // X (irreversible) and P count as writes too; a PUT is prefetched the same way
    let mut put = post_req("/write");
    put.method = "PUT".into();
    execute(&t, &tg, &auth, &s, put, op_call(SideEffect::X))
        .await
        .unwrap();
    assert_eq!(m.hits.load(Ordering::SeqCst), 4);
    assert_eq!(seen_header(m, "x-csrf-token"), CSRF_TOKEN);

    // a read is never prefetched and never carries the token or the session cookies
    let before = m.hits.load(Ordering::SeqCst);
    execute(&t, &tg, &auth, &s, get_req("/ok"), op_call(SideEffect::R))
        .await
        .unwrap();
    assert_eq!(
        m.hits.load(Ordering::SeqCst),
        before + 1,
        "no prefetch for R"
    );
    let read = last_seen(m);
    assert_eq!(header_of(&read, "x-csrf-token"), None);
    assert_eq!(header_of(&read, "cookie"), None);

    // no cookie jar: a later write prefetches again and sends only what that prefetch answered
    execute(
        &t,
        &tg,
        &auth,
        &s,
        post_req("/write"),
        op_call(SideEffect::W),
    )
    .await
    .unwrap();
    assert_eq!(m.hits.load(Ordering::SeqCst), before + 3);

    // a custom header name is used for both the fetch and the write
    let mut custom = csrf_auth("/csrf/fetch");
    custom
        .params
        .insert("csrf_header".into(), "X-CSRF-Token".into());
    execute(
        &t,
        &tg,
        &custom,
        &s,
        post_req("/write"),
        op_call(SideEffect::W),
    )
    .await
    .unwrap();
    assert_eq!(seen_header(m, "x-csrf-token"), CSRF_TOKEN);
}

#[tokio::test]
async fn csrf_prefetch_failures_are_connector_errors() {
    let up = start().await;
    let t = loopback();
    let tg = target(&up.url);
    let m = &up.mock;
    let s = secrets(json!({}), json!({"api_key": LIVE_KEY}));

    // the fetch path answers 200 but no token (the header name is wrong for this server)
    let mut wrong = csrf_auth("/csrf/fetch");
    wrong
        .params
        .insert("csrf_header".into(), "x-xsrf-token".into());
    let e = execute(
        &t,
        &tg,
        &wrong,
        &s,
        post_req("/write"),
        op_call(SideEffect::W),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "connector_misconfigured");
    assert!(e.detail.contains("x-xsrf-token"), "{}", e.detail);
    assert_eq!(
        m.resource_hits.load(Ordering::SeqCst),
        0,
        "the write was not sent"
    );

    // 404 on the fetch path → misconfigured; 403 → auth failed; nothing is written
    let e = execute(
        &t,
        &tg,
        &csrf_auth("/no/such/path"),
        &s,
        post_req("/write"),
        op_call(SideEffect::W),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "connector_misconfigured");
    let e = execute(
        &t,
        &tg,
        &csrf_auth("/csrf/forbidden"),
        &s,
        post_req("/write"),
        op_call(SideEffect::W),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, "connector_auth_failed");
    assert_eq!(m.resource_hits.load(Ordering::SeqCst), 0);
    assert!(!e.detail.contains(LIVE_KEY));
}

#[tokio::test]
async fn mtls_bad_pem_is_auth_failed_without_echoing_it() {
    let up = start().await;
    let t = loopback();
    let tg = target(&up.url);
    let m = &up.mock;
    let mtls = AuthSpec::new(AuthKind::Mtls);
    // built at runtime: no PEM-looking literal in the source
    let fake_cert = format!(
        "-----BEGIN {0}-----\nAAAA-not-base64-zz9q\n-----END {0}-----",
        "CERTIFICATE"
    );
    let cases = [
        json!({"client_cert_pem": "garbage-cert-zz9q", "client_key_pem": "garbage-key-zz9q"}),
        json!({"client_cert_pem": fake_cert, "client_key_pem": "garbage-key-zz9q"}),
    ];
    for secret in cases {
        let s = secrets(json!({}), secret);
        let e = run(&t, &tg, &mtls, &s, get_req("/ok")).await.unwrap_err();
        assert_eq!(e.code, "connector_auth_failed", "{}", e.detail);
        assert!(
            !e.detail.contains("zz9q"),
            "PEM content echoed: {}",
            e.detail
        );
        assert!(
            !e.detail.contains("-----"),
            "PEM content echoed: {}",
            e.detail
        );
    }

    // a missing key names the field
    let s = secrets(json!({}), json!({"client_cert_pem": "garbage-cert-zz9q"}));
    let e = run(&t, &tg, &mtls, &s, get_req("/ok")).await.unwrap_err();
    assert_eq!(e.code, "connector_auth_failed");
    assert!(e.detail.contains("client_key_pem"), "{}", e.detail);

    // there is no way to turn certificate verification off
    let mut insecure = AuthSpec::new(AuthKind::None);
    insecure
        .params
        .insert("insecure_skip_verify".into(), "true".into());
    let e = run(&t, &tg, &insecure, &CallSecrets::default(), get_req("/ok"))
        .await
        .unwrap_err();
    assert_eq!(e.code, "connector_misconfigured");
    assert_eq!(m.hits.load(Ordering::SeqCst), 0, "nothing connected");
}
