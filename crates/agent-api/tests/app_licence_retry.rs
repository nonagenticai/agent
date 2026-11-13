//! An upstream 401 on a TENANT bundle token drops the cached token, mints a fresh one and retries ONCE (strata via
//! `upstream::Call`, the inference-router via `turn::qualify::extract`). A second 401 is the error (no loop). A static
//! strata pair (the operator's) and the platform `INTERNAL_LLM_KEY` are never re-minted. Real Postgres 18, the fake
//! minter (common::minter) and fake strata / router targets that accept exactly one bearer at a time. The strata
//! providers sender (`ConnectionsPut::send`, driven with a synthetic outbox row) re-mints and resends once too; a
//! MULTIPART upload cannot be resent, so its 401 only drops the cached token and the NEXT call mints fresh.
use crate::common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_api::design::DesignOf;
use agent_api::providers::llm::{self, Use};
use agent_api::providers::slots::PushPayload;
use agent_api::provision::senders::connections::ConnectionsPut;
use agent_api::provision::{OutboxRow, SendResult, Sender};
use agent_api::runtime::design::RuntimeDesign;
use agent_api::runtime::prompt;
use agent_api::strata::{self, Service, ingest, rag};
use agent_api::tenancy::begin_scoped;
use agent_api::tenants::app_licence::App;
use agent_api::turn::qualify;
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use common::design_contract::{self, ContractJob};
use common::minter::{self, MinterFake};
use common::{TestEnv, fakes};
use serde_json::{Value, json};
use uuid::Uuid;

const T1: &str = "bundle-token-one";
const T2: &str = "bundle-token-two";

#[derive(Default)]
struct Target {
    /// The one bearer answered 200; any other 401.
    accept: String,
    /// Every request's bearer, in order.
    seen: Vec<String>,
}

type Shared = Arc<Mutex<Target>>;

fn answer(s: &Shared, h: &HeaderMap) -> bool {
    let bearer = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    let mut g = s.lock().unwrap();
    let ok = bearer == g.accept;
    g.seen.push(bearer);
    ok
}

async fn stores(State(s): State<Shared>, h: HeaderMap) -> (StatusCode, Json<Value>) {
    if answer(&s, &h) {
        (StatusCode::OK, Json(json!({"stores": []})))
    } else {
        (StatusCode::UNAUTHORIZED, Json(json!({"error": "no"})))
    }
}

async fn completions(State(s): State<Shared>, h: HeaderMap) -> (StatusCode, Json<Value>) {
    if answer(&s, &h) {
        (
            StatusCode::OK,
            Json(json!({
                "choices": [{"message": {"content": "{\"context_space\": null, \"slots\": {}}"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1},
            })),
        )
    } else {
        (StatusCode::UNAUTHORIZED, Json(json!({"error": "no"})))
    }
}

async fn providers(State(s): State<Shared>, h: HeaderMap) -> (StatusCode, Json<Value>) {
    if answer(&s, &h) {
        (StatusCode::OK, Json(json!({})))
    } else {
        (StatusCode::UNAUTHORIZED, Json(json!({"error": "no"})))
    }
}

async fn ingest_jobs(
    State(s): State<Shared>,
    h: HeaderMap,
    _body: Bytes,
) -> (StatusCode, Json<Value>) {
    if answer(&s, &h) {
        (StatusCode::ACCEPTED, Json(json!({"jobs": []})))
    } else {
        (StatusCode::UNAUTHORIZED, Json(json!({"error": "no"})))
    }
}

/// A fake serving strata (`/api/stores`) and the router (`/chat/completions`) that accepts only `accept`.
async fn target(accept: &str) -> (String, Shared) {
    let s: Shared = Arc::new(Mutex::new(Target {
        accept: accept.to_string(),
        seen: vec![],
    }));
    let app = Router::new()
        .route("/api/stores", get(stores))
        .route("/chat/completions", post(completions))
        .route("/api/tenant/providers", get(providers).put(providers))
        .route("/api/ingest/jobs", post(ingest_jobs))
        .with_state(s.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, s)
}

struct Fx {
    e: TestEnv,
    bm: MinterFake,
    seen: Shared,
    f: ContractJob,
}

/// `static_rag`: the tenant has an operator strata pair (`static-pair`), else it relies on its minted licence.
/// `platform`: the tenant is the platform-owned one (`INTERNAL_LLM_KEY` serves it).
async fn fx(accept: &str, static_rag: bool, platform: bool) -> Fx {
    let _up = fakes::upstream().await;
    let (url, seen) = target(accept).await;
    let tenant = Uuid::now_v7();
    let bm = minter::start();
    for app in agent_api::tenants::app_licence::ALL {
        bm.grant(tenant, app.key(), T1);
    }
    let e = common::env_with(|v: &mut HashMap<String, String>| {
        minter::env(&bm, v);
        v.insert("INTERNAL_LLM_URL".into(), url.clone());
        v.insert("INTERNAL_LLM_KEY".into(), "platform-key".into());
        v.insert("STRATA_RAG_URL".into(), url.clone());
        v.insert("STRATA_INGEST_URL".into(), url.clone());
        v.insert(
            "STRATA_INGEST_TOKENS".into(),
            format!("{}:other", Uuid::now_v7()),
        );
        if static_rag {
            v.insert("STRATA_RAG_TOKENS".into(), format!("{tenant}:static-pair"));
        } else {
            v.insert(
                "STRATA_RAG_TOKENS".into(),
                format!("{}:other", Uuid::now_v7()),
            );
        }
        if platform {
            v.insert("AGENT_PLATFORM_TENANT_ID".into(), tenant.to_string());
        }
    })
    .await;
    let f = design_contract::new_job_for(&e, tenant).await;
    Fx { e, bm, seen, f }
}

impl Fx {
    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().seen.clone()
    }
    fn tenant(&self) -> Uuid {
        self.f.tenant
    }

    /// `GET /api/stores` as the tenant's strata-rag plane.
    async fn stores(&self) -> Result<usize, agent_api::error::ApiError> {
        let p = strata::plane(&self.e.state, self.tenant(), Service::Rag)
            .await
            .expect("plane");
        rag::stores(&self.e.state.http, &p).await.map(|s| s.len())
    }

    /// One qualifier call on the live `qualify` resolve; `Ok` when the router answered.
    async fn qualify(&self) -> Result<(), agent_api::error::ApiError> {
        let st = &self.e.state;
        let mut tx = begin_scoped(&st.db, self.tenant()).await.unwrap();
        let r = llm::resolve(
            &mut tx,
            st,
            self.tenant(),
            &self.f.job_ref(),
            Use::Qualify,
            &DesignOf::Live,
        )
        .await
        .unwrap()
        .expect("qualify resolves");
        drop(tx);
        let m = qualify::model(st, self.tenant(), r)
            .await
            .unwrap()
            .expect("openai-compatible with a key");
        let design = RuntimeDesign::blank(self.f.job_ref(), DesignOf::Live);
        let p = prompt::assemble(&(&design).into(), None, None);
        let (out, _) = qualify::extract(
            &m,
            "where is my order?",
            None,
            &design.spaces,
            &design.settings,
            &p,
        )
        .await;
        out.map(|_| ())
    }
}

#[tokio::test]
async fn strata_401_on_a_tenant_token_reminted_and_retried_once() {
    let fx = fx(T2, false, false).await;
    // T1 is minted and cached by the plane; licences then rotates the tenant's token to T2
    let p = strata::plane(&fx.e.state, fx.tenant(), Service::Rag)
        .await
        .unwrap();
    assert_eq!(fx.bm.mint_count(), 1);
    fx.bm.grant(fx.tenant(), App::StrataRag.key(), T2);
    let stores = rag::stores(&fx.e.state.http, &p).await.expect("retried");
    assert!(stores.is_empty(), "the fake strata lists no stores");
    assert_eq!(fx.seen(), [T1, T2], "one 401, one retry");
    assert_eq!(fx.bm.mint_count(), 2, "one re-mint");
}

#[tokio::test]
async fn a_second_strata_401_is_the_error_and_nothing_loops() {
    let fx = fx("nobody", false, false).await;
    let p = strata::plane(&fx.e.state, fx.tenant(), Service::Rag)
        .await
        .unwrap();
    fx.bm.grant(fx.tenant(), App::StrataRag.key(), T2);
    let e = rag::stores(&fx.e.state.http, &p).await.unwrap_err();
    assert_eq!(
        e.code(),
        "unavailable",
        "401 stays our-credentials unavailable"
    );
    assert_eq!(fx.seen(), [T1, T2], "exactly one retry");
    assert_eq!(fx.bm.mint_count(), 2);
}

#[tokio::test]
async fn router_401_on_a_tenant_token_reminted_and_retried_once() {
    let fx = fx(T2, false, false).await;
    // the first resolve mints and caches T1; the router only knows T2 by the time the call is made
    let mut tx = begin_scoped(&fx.e.state.db, fx.tenant()).await.unwrap();
    let t = agent_api::tenants::app_licence::token(
        &fx.e.state,
        &mut tx,
        fx.tenant(),
        App::InferenceRouter,
    )
    .await
    .unwrap();
    drop(tx);
    assert!(t.is_some());
    fx.bm.grant(fx.tenant(), App::InferenceRouter.key(), T2);
    fx.qualify().await.expect("retried with the fresh token");
    assert_eq!(fx.seen(), [T1, T2], "one 401, one retry");
    assert_eq!(fx.bm.mint_count(), 2, "one re-mint");
}

#[tokio::test]
async fn a_second_router_401_is_the_error_and_nothing_loops() {
    let fx = fx("nobody", false, false).await;
    let mut tx = begin_scoped(&fx.e.state.db, fx.tenant()).await.unwrap();
    agent_api::tenants::app_licence::token(&fx.e.state, &mut tx, fx.tenant(), App::InferenceRouter)
        .await
        .unwrap();
    drop(tx);
    fx.bm.grant(fx.tenant(), App::InferenceRouter.key(), T2);
    let e = fx.qualify().await.unwrap_err();
    assert_eq!(e.code(), "llm_unavailable");
    assert_eq!(fx.seen(), [T1, T2], "exactly one retry");
    assert_eq!(fx.bm.mint_count(), 2);
}

#[tokio::test]
async fn a_static_strata_pair_is_never_reminted() {
    let fx = fx("nobody", true, false).await;
    // the operator pair is refused: the error surfaces, licences is never asked
    let e = fx.stores().await.unwrap_err();
    assert_eq!(e.code(), "unavailable");
    assert_eq!(fx.seen(), ["static-pair"], "no retry");
    assert_eq!(fx.bm.mint_count(), 0, "the operator token is untouched");
}

#[tokio::test]
async fn a_platform_key_401_is_never_reminted() {
    let fx = fx("nobody", false, true).await;
    let e = fx.qualify().await.unwrap_err();
    assert_eq!(e.code(), "llm_unavailable");
    assert_eq!(fx.seen(), ["platform-key"], "no retry");
    assert_eq!(fx.bm.mint_count(), 0);
}

/// A `connections_put` row with no slots: the sender still reads the set (GET) and replaces it (PUT).
fn put_row(tenant: Uuid) -> OutboxRow {
    let now = chrono::Utc::now();
    OutboxRow {
        id: Uuid::now_v7(),
        tenant_id: tenant,
        target: "strata_rag".into(),
        op: "connections_put".into(),
        dedupe_key: "strata_rag:connections_put".into(),
        payload: serde_json::to_value(PushPayload::default()).unwrap(),
        status: "running".into(),
        attempts: 1,
        max_attempts: 5,
        next_attempt_at: now,
        lease_until: None,
        lease_token: None,
        last_http_status: None,
        last_error_code: None,
        last_error: None,
        result: None,
        created_by: "test".into(),
        created_at: now,
        updated_at: now,
        completed_at: None,
    }
}

/// Seam: `ConnectionsPut::send` (the registered sender) on a synthetic row; the outbox claim / drain around it is
/// covered by b3/provider_slots.
#[tokio::test]
async fn providers_sender_401_on_a_tenant_token_reminted_and_retried_once() {
    let fx = fx(T2, false, false).await;
    // T1 is minted and cached; licences then rotates the tenant's token to T2
    strata::plane(&fx.e.state, fx.tenant(), Service::Rag)
        .await
        .unwrap();
    assert_eq!(fx.bm.mint_count(), 1);
    fx.bm.grant(fx.tenant(), App::StrataRag.key(), T2);
    let r = ConnectionsPut
        .send(&fx.e.state, &put_row(fx.tenant()))
        .await;
    match r {
        SendResult::Http { status, .. } => assert_eq!(status, 200),
        other => panic!("expected the PUT to land, got {other:?}"),
    }
    // GET: T1 refused, T2 on the resend; PUT: the re-minted T2 is the cached token
    assert_eq!(fx.seen(), [T1, T2, T2]);
    assert_eq!(fx.bm.mint_count(), 2, "one re-mint");
}

#[tokio::test]
async fn multipart_401_is_not_resent_but_drops_the_cached_token() {
    let fx = fx(T2, false, false).await;
    let form = || reqwest::multipart::Form::new().text("store_id", "s");
    let p = strata::plane(&fx.e.state, fx.tenant(), Service::Ingest)
        .await
        .unwrap();
    assert_eq!(fx.bm.mint_count(), 1);
    // licences rotated the token to T2 after T1 was cached: the upload with T1 is refused
    fx.bm.grant(fx.tenant(), App::StrataIngest.key(), T2);
    let e = ingest::create_multipart(&fx.e.state.http, &p, form())
        .await
        .unwrap_err();
    assert_eq!(e.code(), "unavailable");
    assert_eq!(fx.seen(), [T1], "a multipart body is never resent");
    assert_eq!(fx.bm.mint_count(), 1, "the refusal itself mints nothing");
    // the cache was dropped: the next call mints a fresh token and lands
    let p = strata::plane(&fx.e.state, fx.tenant(), Service::Ingest)
        .await
        .unwrap();
    assert_eq!(fx.bm.mint_count(), 2, "the next call minted anew");
    ingest::create_multipart(&fx.e.state.http, &p, form())
        .await
        .expect("fresh token accepted");
    assert_eq!(fx.seen(), [T1, T2]);
}
