//! [`extract`] names the billed tenant (`X-Effective-Tenant-Id`) to the platform model only (agent#71): the
//! inference-router bills the internal call to it, and a tenant's own provider never learns our tenant id.

use std::sync::{Arc, Mutex};

use axum::http::HeaderMap;

use super::*;

/// A fake chat-completions upstream recording each request's `X-Effective-Tenant-Id` (`None` when absent).
async fn upstream() -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |h: HeaderMap| {
            let seen = s2.clone();
            async move {
                seen.lock().unwrap().push(
                    h.get("x-effective-tenant-id")
                        .and_then(|v| v.to_str().ok())
                        .map(String::from),
                );
                axum::Json(json!({
                    "choices": [{"message": {"content": "{\"context_space\": null, \"slots\": {}}"}}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                }))
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}/v1"), seen)
}

/// A [`Model`] as [`model`] builds it: the internal arm carries `ResolvedLlm.effective_tenant`, a tenant provider's
/// arm `None`.
fn model_at(url: &str, provider_id: &str, funding: &str, effective_tenant: Option<Uuid>) -> Model {
    Model {
        http: reqwest::Client::new(),
        url: url.into(),
        token: "tok".into(),
        model: "m".into(),
        provider_id: provider_id.into(),
        funding: funding.into(),
        effective_tenant,
        renew: None,
    }
}

async fn call(m: &Model) -> LlmUsage {
    let (out, usage) = extract(
        m,
        "hi",
        None,
        &SpaceCatalog::new(vec![]),
        &BTreeMap::new(),
        &SystemMessage::default(),
    )
    .await;
    assert!(out.is_ok(), "the fake answers every call");
    usage
}

// Guards qualify.rs `extract`: `if let Some(t) = m.effective_tenant { call = call.header(EFFECTIVE_TENANT_HEADER,
// t.to_string()) }`. Delete it and the platform call carries no tenant (first element `None`); send it
// unconditionally (or the wrong value) and the tenant-provider call or the equality fails.
#[tokio::test]
async fn extract_names_the_tenant_to_the_platform_model_and_never_to_a_tenant_provider() {
    let (url, seen) = upstream().await;
    let tenant = Uuid::new_v4();
    assert_ne!(tenant, Uuid::nil());

    let internal = model_at(&url, INTERNAL_PROVIDER, "local_gpu", Some(tenant));
    let usage = call(&internal).await;
    assert_eq!(usage.provider_id, INTERNAL_PROVIDER);

    let byo = model_at(&url, &Uuid::new_v4().to_string(), "tenant_byo", None);
    let usage = call(&byo).await;
    assert_eq!(usage.funding, "tenant_byo");

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen, vec![Some(tenant.to_string()), None]);
    assert!(
        !seen.contains(&Some(Uuid::nil().to_string())),
        "never the nil tenant: {seen:?}"
    );
}

/// A fake chat-completions upstream recording each request's X-App-Id, X-Project-Id and X-Actor-Id.
async fn identity_upstream() -> (String, Arc<Mutex<Vec<[Option<String>; 3]>>>) {
    let seen: Arc<Mutex<Vec<[Option<String>; 3]>>> = Arc::default();
    let s2 = seen.clone();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |h: HeaderMap| {
            let seen = s2.clone();
            async move {
                let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(String::from);
                seen.lock()
                    .unwrap()
                    .push([get("x-app-id"), get("x-project-id"), get("x-actor-id")]);
                axum::Json(json!({
                    "choices": [{"message": {"content": "{\"context_space\": null, \"slots\": {}}"}}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                }))
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}/v1"), seen)
}

// Guards `model_in`'s internal arm (`platform_client(LLM_TIMEOUT, r.effective_tenant, r.actor)`): a plain client
// there sends the router no X-App-Id / X-Project-Id / X-Actor-Id, and the router 400s it once it enforces.
#[tokio::test]
async fn model_gives_the_platform_model_the_router_identity() {
    let (url, seen) = identity_upstream().await;
    let tenant = Uuid::new_v4();
    let actor = Uuid::new_v4();
    let resolved = ResolvedLlm {
        provider_id: None,
        base_url: url,
        model: "local-llm".into(),
        kind: Kind::Openai,
        funding: "local_gpu",
        key: None,
        voice: None,
        effective_tenant: Some(tenant),
        actor: Some(actor),
        tenant_licence: None,
    };
    let m = model_in(&netguard::Policy::default(), tenant, resolved)
        .await
        .unwrap()
        .expect("the internal model is callable");
    let usage = call(&m).await;
    assert_eq!(usage.provider_id, INTERNAL_PROVIDER);
    assert_eq!(
        *seen.lock().unwrap(),
        [[
            Some("agent".to_string()),
            Some(crate::providers::llm::tenant_level_project_id(tenant).to_string()),
            Some(actor.to_string()),
        ]]
    );
}
