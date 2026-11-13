//! The qualifier's tenant-provider arm, end to end (`turn::qualify::model` + `extract`): a tenant that binds and
//! publishes its own `qualify` provider is called at ITS base URL with ITS key and bound model — and nothing of the
//! platform's identity rides along (no `X-Effective-Tenant-Id`, no app/project/actor headers, never the internal or
//! router key), and the platform's model / interaction router is never called.
//!
//! Both `INTERNAL_LLM_URL` and `INTERACTION_LLM_URL` point at a
//! recording "router" fake, so every wrong turn (fallback wins, arm forwards platform headers, arm calls the config
//! URL) is visible to one recorder. A control call on the platform arm first proves the recorder sees the router
//! call with `X-Effective-Tenant-Id`, `X-App-Id`, `X-Project-Id` and `X-Actor-Id` — both calls go through
//! `ResolvedLlm::acting_for` as the production call sites do — so the absence assertions below are not vacuous.
//!
//! Mutations caught: the `(Some(id), Some(key))` arm stamping `Some(tenant)` instead of `None` (adds
//! `X-Effective-Tenant-Id`; proven red in CI 2026-10-25), or building its client/URL from
//! `internal_providers.llm_url` (router gets the call). Returning `r.effective_tenant` there is an equivalent mutant:
//! `providers::llm::resolved` already sets it to `None` for a tenant provider.
//!
//! The other two tenant arms have a test each below: a binding without a key builds no model, and a URL netguard
//! refuses gets 503 `llm_unavailable` with no call.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get as route_get, post as route_post};
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::design_contract::{self, ContractJob, cuttable_selection, new_job};
use crate::common::{self, TestEnv, get, insert_user, json, person, send};
use agent_api::connectors::transport::Transport;
use agent_api::design::{DesignOf, DraftRef};
use agent_api::error::ApiError;
use agent_api::netguard::Policy;
use agent_api::providers::llm::{self, Use};
use agent_api::runtime::design::RuntimeDesign;
use agent_api::runtime::prompt;
use agent_api::tenancy::begin_scoped;
use agent_api::turn::qualify;

const TENANT_KEY: &str = "sk-tenant-own-7";
const ROUTER_KEY: &str = "sk-router-interaction-3";
const INTERNAL_KEY: &str = "sk-internal-vllm-5";
const MODEL: &str = "gpt-4.1-mini";
/// The acting person's Keycloak `sub`: production call sites pass it through `ResolvedLlm::acting_for`.
const PERSON: &str = "0195f48b-d000-7000-8000-00000000c0de";

/// One recorded request: its path, headers and JSON body.
type Calls = Arc<Mutex<Vec<(String, HeaderMap, Value)>>>;

async fn models() -> Json<Value> {
    Json(json!({"data": [{"id": MODEL}]}))
}

async fn chat(
    State(calls): State<(Calls, &'static str)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    calls
        .0
        .lock()
        .unwrap()
        .push((calls.1.to_string(), headers, body));
    (
        StatusCode::OK,
        Json(json!({
            "choices": [{"message": {"content": "{\"context_space\": null, \"slots\": {}}"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 4},
        })),
    )
}

/// A recording OpenAI-compatible fake under `/v1`: its base URL and its chat-completions calls.
async fn fake(name: &'static str) -> (String, Calls) {
    let calls: Calls = Arc::default();
    let app = Router::new()
        .route("/v1/models", route_get(models))
        .route("/v1/chat/completions", route_post(chat))
        .with_state((calls.clone(), name));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}/v1"), calls)
}

fn env_vars(router: String) -> impl FnOnce(&mut HashMap<String, String>) {
    move |vars| {
        let key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        vars.insert(
            "PROVIDER_SECRET_KEYS".to_string(),
            format!(r#"{{"p1":"{key}"}}"#),
        );
        vars.insert("PROVIDER_SECRET_KEY_CURRENT".to_string(), "p1".to_string());
        vars.insert("INTERNAL_LLM_URL".to_string(), router.clone());
        vars.insert("INTERNAL_LLM_KEY".to_string(), INTERNAL_KEY.to_string());
        vars.insert("INTERACTION_LLM_URL".to_string(), router);
        vars.insert("INTERACTION_LLM_TOKEN".to_string(), ROUTER_KEY.to_string());
    }
}

async fn member(e: &TestEnv, tenant: Uuid, role: &str) -> String {
    let sub = format!("u-{role}-{}", Uuid::new_v4().simple());
    insert_user(&e.owner, &sub, tenant, role).await;
    person(&sub, &[])
}

/// Approve and apply the job's draft as release manager `tok` (pattern gap_tests.rs `publish`).
async fn publish(app: &Router, f: &ContractJob, tok: &str) {
    let job = &f.job.job_key;
    let (s, b) = send(app, get(&format!("/api/jobs/{job}/target"), Some(tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let body = json!({"apply": true, "selection_seen_at": b["selection_updated_at"].clone()});
    let (s, b) = send(
        app,
        json(
            "POST",
            &format!("/api/jobs/{job}/releases"),
            Some(tok),
            body,
        ),
    )
    .await;
    assert!(
        s == StatusCode::CREATED || s == StatusCode::OK,
        "approve: {s}: {b}"
    );
    assert_eq!(b["applied"], true, "{b}");
}

/// The live `qualify` resolve, through the production resolver (`llm::resolve` with the state's config).
async fn resolve_qualify(st: &agent_api::state::AppState, f: &ContractJob) -> llm::ResolvedLlm {
    let mut tx = begin_scoped(&st.db, f.tenant).await.unwrap();
    let r = llm::resolve(
        &mut tx,
        st,
        f.tenant,
        &f.job_ref(),
        Use::Qualify,
        &DesignOf::Live,
    )
    .await
    .unwrap()
    .expect("qualify resolves");
    drop(tx);
    r
}

/// `qualify::model` + one `extract` for `r`: the usage it reports.
async fn call(
    st: &agent_api::state::AppState,
    f: &ContractJob,
    r: llm::ResolvedLlm,
) -> qualify::LlmUsage {
    let m = qualify::model(st, f.tenant, r)
        .await
        .expect("model builds")
        .expect("openai-compatible with a key");
    let design = RuntimeDesign::blank(f.job_ref(), DesignOf::Live);
    let p = prompt::assemble(&(&design).into(), None, None);
    let (out, usage) = qualify::extract(
        &m,
        "where is my order?",
        None,
        &design.spaces,
        &design.settings,
        &p,
    )
    .await;
    out.expect("the qualifier answered");
    usage
}

/// The test state: loopback targets allowed or not (the fakes listen on 127.0.0.1). `_f`'s tenant needs no router
/// licence here: a wrong turn onto the router lane would fail closed, and the platform arm is the control.
fn armed(e: &TestEnv, _f: &ContractJob, allow_loopback: bool) -> agent_api::state::AppState {
    let mut st = e.state.clone();
    let mut cfg = (*st.cfg).clone();
    cfg.connector_policy.allow_loopback = allow_loopback;
    st.cfg = Arc::new(cfg);
    st.connectors = Transport::new(Policy {
        allow_loopback,
        private_targets: vec![],
    });
    st
}

/// The tenant binds its own `qualify` provider at `base` (with [`TENANT_KEY`]) and publishes it, through the routes
/// on `app` (a loopback-allowing state: the provider routes check `base` too). Returns the provider id and the
/// integrations manager's token.
async fn bind_own(e: &TestEnv, f: &ContractJob, app: &Router, base: &str) -> (Uuid, String) {
    let im = member(e, f.tenant, "integrations_manager").await;
    let rm = member(e, f.tenant, "tenant_admin").await;
    design_contract::write(
        e,
        f,
        "selection",
        &DraftRef::new(),
        Some(cuttable_selection()),
    )
    .await;
    design_contract::route_required_prod_ops(e, f).await;
    let (s, b) = send(
        app,
        json(
            "POST",
            "/api/providers",
            Some(&im),
            json!({"name": "Own", "kind": "openai", "base_url": base, "models": [MODEL], "secret": TENANT_KEY}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let pid: Uuid = b["id"].as_str().unwrap().parse().unwrap();
    let (s, b) = send(
        app,
        json(
            "PUT",
            "/api/providers/uses/qualify",
            Some(&im),
            json!({"provider_id": pid, "model": MODEL}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    publish(app, f, &rm).await;
    (pid, im)
}

const PLATFORM_HEADERS: [&str; 5] = [
    "x-effective-tenant-id",
    "x-app-id",
    "x-project-id",
    "x-actor-id",
    "x-tenant-id",
];

#[tokio::test]
async fn tenant_qualify_provider_gets_its_own_key_and_model_and_no_platform_headers() {
    let (router, router_calls) = fake("router").await;
    let (tenant_base, tenant_calls) = fake("tenant").await;
    let e = common::env_with(env_vars(router)).await;
    let f = new_job(&e).await;
    // the tenant holds its own inference-router licence (the fake bundle minter hands the token out on demand): an
    // unbound qualify calls the router under it, never under the platform key
    common::minter::grant_shared(f.tenant, "inference-router", "tenant-router-token");
    let st = armed(&e, &f, true);
    let app = agent_api::router(st.clone());

    // control: unbound, qualify takes the platform arm — the router fake records the call with the billing header
    let unbound = resolve_qualify(&st, &f).await;
    assert_eq!((unbound.provider_id, unbound.funding), (None, "local_gpu"));
    // as production does (drain.rs / probe.rs): the resolved model acts for the person
    let u = call(&st, &f, unbound.acting_for(Some(PERSON))).await;
    assert_eq!(u.provider_id, qualify::INTERNAL_PROVIDER);
    {
        let rc = router_calls.lock().unwrap();
        assert_eq!(rc.len(), 1, "control: the platform arm calls the router");
        let hdr = |h: &str| {
            rc[0]
                .1
                .get(h)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        // every header the tenant arm must NOT send is seen here, so its absence below is not vacuous
        assert_eq!(
            hdr("x-effective-tenant-id"),
            Some(f.tenant.to_string()),
            "control: the recorder sees the platform arm's billing header"
        );
        assert_eq!(
            hdr("x-app-id"),
            Some(llm::APP_ID.to_string()),
            "control: x-app-id"
        );
        assert_eq!(
            hdr("x-project-id"),
            Some(llm::tenant_level_project_id(f.tenant).to_string()),
            "control: x-project-id"
        );
        assert_eq!(
            hdr("x-actor-id"),
            Some(PERSON.to_string()),
            "control: x-actor-id"
        );
    }
    router_calls.lock().unwrap().clear();

    let (pid, _) = bind_own(&e, &f, &app, &tenant_base).await;

    let bound = resolve_qualify(&st, &f).await;
    assert_eq!(bound.provider_id, Some(pid), "the tenant binding wins");
    let usage = call(&st, &f, bound.acting_for(Some(PERSON))).await;

    assert_eq!(usage.provider_id, pid.to_string());
    assert_eq!(usage.model, MODEL);
    assert_eq!(usage.funding, "tenant_byo");
    assert_eq!((usage.tokens_in, usage.tokens_out), (11, 4));

    let tc = tenant_calls.lock().unwrap();
    assert_eq!(tc.len(), 1, "the tenant provider is called exactly once");
    let (_, headers, body) = &tc[0];
    assert_eq!(
        headers.get("authorization").and_then(|v| v.to_str().ok()),
        Some(format!("Bearer {TENANT_KEY}").as_str()),
        "the tenant's own key"
    );
    assert_eq!(body["model"], MODEL, "the bound model");
    for h in PLATFORM_HEADERS {
        assert!(
            headers.get(h).is_none(),
            "{h} must not reach a tenant provider"
        );
    }
    for (name, v) in headers {
        let v = v.to_str().unwrap_or_default();
        assert!(
            !v.contains(ROUTER_KEY) && !v.contains(INTERNAL_KEY) && !v.contains(PERSON),
            "header {name} carries a platform key"
        );
    }
    assert!(
        router_calls.lock().unwrap().is_empty(),
        "the router / platform model is never called for a bound tenant"
    );
}

/// The `(Some(_), None)` arm: a live tenant binding whose key is gone (cleared after publish, `secret: null`) builds
/// NO model — it never falls back to the platform model (the platform would pay for the tenant's turn) and never calls
/// the tenant URL keyless. Its callers (drain.rs, probe.rs) answer `TURN_FAILED` on `None` (`qualify_unbound`).
///
/// Mutations caught: that arm returning a platform `Model` (router URL, internal key, `X-Effective-Tenant-Id`), or a
/// tenant `Model` with an empty token. The keyed control on the same binding first proves `Some` is reachable, so the
/// `None` below is the missing key, not a binding that never resolved.
#[tokio::test]
async fn tenant_qualify_provider_without_a_key_builds_no_model() {
    let (router, router_calls) = fake("router").await;
    let (tenant_base, tenant_calls) = fake("tenant").await;
    let e = common::env_with(env_vars(router)).await;
    let f = new_job(&e).await;
    let st = armed(&e, &f, true);
    let app = agent_api::router(st.clone());
    let (pid, im) = bind_own(&e, &f, &app, &tenant_base).await;

    // control: with its key, the same live binding builds a model
    let keyed = resolve_qualify(&st, &f).await;
    assert_eq!(keyed.provider_id, Some(pid), "the tenant binding wins");
    assert!(keyed.key.is_some(), "control: the stored key opens");
    let m = qualify::model(&st, f.tenant, keyed.acting_for(Some(PERSON)))
        .await
        .expect("control: model builds");
    assert!(
        m.is_some(),
        "control: a keyed tenant provider builds a model"
    );

    // the tenant clears the key; the binding stays live
    let (s, b) = send(
        &app,
        json(
            "PUT",
            &format!("/api/providers/{pid}"),
            Some(&im),
            json!({"name": "Own", "kind": "openai", "base_url": tenant_base, "models": [MODEL], "secret": null}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["secret"]["set"], false, "{b}");

    let keyless = resolve_qualify(&st, &f).await;
    assert_eq!(
        (keyless.provider_id, keyless.funding),
        (Some(pid), "tenant_byo"),
        "still the tenant's binding, not the platform fallback"
    );
    assert!(keyless.key.is_none(), "the key is gone");
    let m = qualify::model(&st, f.tenant, keyless.acting_for(Some(PERSON)))
        .await
        .expect("a keyless tenant provider is not an error");
    assert!(
        m.is_none(),
        "a tenant provider without a key builds no model: {m:?}"
    );
    assert!(
        tenant_calls.lock().unwrap().is_empty(),
        "the tenant URL is never called"
    );
    assert!(
        router_calls.lock().unwrap().is_empty(),
        "the router / platform model is never called"
    );
}

/// The `(Some(id), Some(key))` arm goes through netguard: a tenant provider whose stored URL the outbound policy
/// refuses (here loopback, with `allow_loopback` off — the URL was accepted under a policy that allowed it) gets 503
/// `llm_unavailable` and no call, although the URL is live: the same binding under the loopback-allowing policy then
/// reaches the tenant fake, so the refusal is the guard and not an unreachable host.
///
/// Mutations caught: the tenant client built without `netguard::check_url` / `netguard::pinned_client` (a plain
/// client, or `platform_client`), or `qualify::model` passing a policy other than `cfg.connector_policy`. netguard
/// itself is tested in tests/transports.rs and tests/b3/foundations.rs; this pins that the qualifier uses it.
#[tokio::test]
async fn tenant_qualify_provider_refused_by_netguard_gets_503_and_no_call() {
    let (router, router_calls) = fake("router").await;
    let (tenant_base, tenant_calls) = fake("tenant").await;
    let e = common::env_with(env_vars(router)).await;
    let f = new_job(&e).await;
    let open = armed(&e, &f, true);
    let app = agent_api::router(open.clone());
    let (pid, _) = bind_own(&e, &f, &app, &tenant_base).await;

    let strict = armed(&e, &f, false);
    let bound = resolve_qualify(&strict, &f).await;
    assert_eq!(bound.provider_id, Some(pid), "the tenant binding wins");
    assert!(
        bound.key.is_some(),
        "keyed: the netguard arm, not the keyless one"
    );
    match qualify::model(&strict, f.tenant, bound.acting_for(Some(PERSON))).await {
        Err(ApiError::Unavailable(code, _, _)) => assert_eq!(code, "llm_unavailable"),
        other => panic!("a loopback tenant URL must be refused by netguard, got {other:?}"),
    }
    assert!(
        tenant_calls.lock().unwrap().is_empty(),
        "the refused tenant URL is never called"
    );
    assert!(
        router_calls.lock().unwrap().is_empty(),
        "a refusal does not fall back to the router"
    );

    // control: the same binding under the loopback-allowing policy reaches the (live) tenant fake
    let bound = resolve_qualify(&open, &f).await;
    let usage = call(&open, &f, bound.acting_for(Some(PERSON))).await;
    assert_eq!(usage.provider_id, pid.to_string());
    assert_eq!(
        tenant_calls.lock().unwrap().len(),
        1,
        "control: the URL is live"
    );
    assert!(router_calls.lock().unwrap().is_empty());
}
