//! Per-tenant platform-app licences, MINTED ON DEMAND (migration 0216, crate::tenants::app_licence): the bundle
//! minter caches a token until `expires_at` minus 30 s, drops its Keycloak client token on a 401 and retries once,
//! rejects an echo for another tenant / app / bundle, and fails closed unconfigured; provisioning ensures the bundle,
//! mints, smokes as the tenant and records STATUS only (no token column); nothing is ever revoked.
//! Real Postgres 18 plus in-process fakes: licences admin (fakes::upstream: ensure), the Keycloak token + bundle-token
//! minter (common::minter) and the router / strata smoke targets (here), which accept exactly one token.
use crate::common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_api::audit::Actor;
use agent_api::tenancy::begin_scoped;
use agent_api::tenants::app_licence::{
    ALL, App, AppToken, MintError, Provisioned, invalidate, provision_app, provision_tenant, token,
};
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use common::fakes::{self, Upstream, lock};
use common::minter::{self, MinterFake};
use common::{TestEnv, insert_tenant};
use uuid::Uuid;

const GOOD: &str = "tenant-token-good";

#[derive(Default)]
struct Smoke {
    /// Refuse even the good token (the smoke must fail).
    refuse_good: bool,
    /// Accept ANY token (an upstream that does not enforce licences).
    accept_all: bool,
    /// `(path, bearer)` of every request, in order.
    seen: Vec<(String, String)>,
}

type SmokeState = Arc<Mutex<Smoke>>;

async fn answer(s: &SmokeState, path: &str, h: &HeaderMap) -> StatusCode {
    let bearer = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    let mut g = s.lock().unwrap();
    g.seen.push((path.to_string(), bearer.clone()));
    if g.accept_all || (bearer == GOOD && !g.refuse_good) {
        StatusCode::OK
    } else {
        StatusCode::UNAUTHORIZED
    }
}

async fn router(State(s): State<SmokeState>, h: HeaderMap) -> StatusCode {
    answer(&s, "/chat/completions", &h).await
}
async fn stores(State(s): State<SmokeState>, h: HeaderMap) -> StatusCode {
    answer(&s, "/api/stores", &h).await
}
async fn jobs(State(s): State<SmokeState>, h: HeaderMap) -> StatusCode {
    answer(&s, "/api/ingest/jobs", &h).await
}

async fn smoke_target() -> (String, SmokeState) {
    let state: SmokeState = Arc::default();
    let app = Router::new()
        .route("/chat/completions", post(router))
        .route("/api/stores", get(stores))
        .route("/api/ingest/jobs", get(jobs))
        .with_state(state.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, state)
}

struct Fx {
    e: TestEnv,
    up: Upstream,
    minter: MinterFake,
    smoke: SmokeState,
    tenant: Uuid,
}

async fn fx(configure_strata: bool) -> Fx {
    fx_with(configure_strata, true).await
}

/// `minter_configured` false = no `BUNDLE_MINTER_*` in the process.
async fn fx_with(configure_strata: bool, minter_configured: bool) -> Fx {
    let up = fakes::upstream().await;
    let (url, smoke) = smoke_target().await;
    let tenant = Uuid::now_v7();
    let bm = minter::start();
    for app in ALL {
        bm.grant(tenant, app.key(), GOOD);
    }
    let e = common::env_with(|v: &mut HashMap<String, String>| {
        fakes::identity_env(&up, v);
        if minter_configured {
            minter::env(&bm, v);
        } else {
            v.retain(|k, _| !k.starts_with("BUNDLE_MINTER_"));
        }
        v.insert("INTERNAL_LLM_URL".into(), url.clone());
        if configure_strata {
            for (u, t) in [
                ("STRATA_RAG_URL", "STRATA_RAG_TOKENS"),
                ("STRATA_INGEST_URL", "STRATA_INGEST_TOKENS"),
            ] {
                v.insert(u.into(), url.clone());
                v.insert(t.into(), format!("{}:static-{u}", Uuid::now_v7()));
            }
        }
    })
    .await;
    insert_tenant(&e.owner, tenant, "Globex").await;
    Fx {
        e,
        up,
        minter: bm,
        smoke,
        tenant,
    }
}

fn actor() -> Actor {
    Actor::system("service:app-licences-test")
}

async fn read(f: &Fx, app: App) -> Option<AppToken> {
    let mut tx = begin_scoped(&f.e.state.db, f.tenant).await.unwrap();
    let t = token(&f.e.state, &mut tx, f.tenant, app).await.unwrap();
    tx.commit().await.unwrap();
    t
}

async fn status_rows(
    f: &Fx,
) -> Vec<(String, Option<String>, Option<String>, bool, Option<String>)> {
    sqlx::query_as(
        "SELECT app_key, licence_id, token_id, provisioned_at IS NOT NULL, last_error \
           FROM tenant_app_licence WHERE tenant_id = $1 ORDER BY app_key",
    )
    .bind(f.tenant)
    .fetch_all(&f.e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn token_is_none_without_a_licence() {
    let f = fx(true).await;
    let other = Uuid::now_v7();
    insert_tenant(&f.e.owner, other, "Initech").await;
    let mut tx = begin_scoped(&f.e.state.db, other).await.unwrap();
    for app in ALL {
        // licences answers 404 no_app_licence: fail closed, not an error
        let t = token(&f.e.state, &mut tx, other, app).await.unwrap();
        assert!(t.is_none(), "{}", app.key());
    }
}

#[tokio::test]
async fn app_token_debug_is_redacted() {
    let t = AppToken::new("never-print-me");
    assert_eq!(format!("{t:?}"), "AppToken(<redacted>)");
    assert!(!format!("{t:#?}").contains("never-print-me"));
}

#[tokio::test]
async fn an_unconfigured_minter_gives_no_token_and_a_visible_retry() {
    let f = fx_with(true, false).await;
    assert!(!f.e.state.bundle_minter.configured());
    assert!(read(&f, App::InferenceRouter).await.is_none());
    let err = provision_app(&f.e.state, f.tenant, App::InferenceRouter, &actor())
        .await
        .unwrap()
        .expect_err("no minter");
    assert_eq!(
        (err.code, err.retry),
        ("bundle_minter_not_configured", true)
    );
    assert_eq!(f.minter.mint_count(), 0);
}

#[tokio::test]
async fn a_mint_is_cached_until_expiry_minus_the_skew() {
    let f = fx(true).await;
    let m = &f.e.state.bundle_minter;
    let first = m.mint(f.tenant, App::StrataRag).await.unwrap();
    let second = m.mint(f.tenant, App::StrataRag).await.unwrap();
    assert_eq!(first.token.expose(), GOOD);
    assert_eq!(second.licence_id, "lic-strata-rag");
    assert_eq!(
        f.minter.mint_count(),
        1,
        "the second call is served from the cache"
    );
    assert_eq!(f.minter.lock().client_token_calls, 1);
    {
        let g = f.minter.lock();
        let (tenant, app, bundle, ttl) = g.mints[0].clone();
        assert_eq!(
            (tenant, app, bundle, ttl),
            (
                f.tenant.to_string(),
                "strata-rag".into(),
                "agent".into(),
                "15".into()
            )
        );
    }
    // another app of the tenant is its own entry
    m.mint(f.tenant, App::StrataIngest).await.unwrap();
    assert_eq!(f.minter.mint_count(), 2);
    assert_eq!(
        f.minter.lock().client_token_calls,
        1,
        "the client token is cached too"
    );
    // an upstream 401 drops the entry: the next call mints anew
    invalidate(&f.e.state, f.tenant, App::StrataRag);
    m.mint(f.tenant, App::StrataRag).await.unwrap();
    assert_eq!(f.minter.mint_count(), 3);
}

#[tokio::test]
async fn a_token_inside_the_skew_is_not_usable() {
    let f = fx(true).await;
    f.minter.lock().expires_in_secs = 20; // under the 30 s skew
    let err =
        f.e.state
            .bundle_minter
            .mint(f.tenant, App::StrataRag)
            .await
            .unwrap_err();
    assert!(matches!(err, MintError::Unavailable(_)), "{err:?}");
    f.minter.lock().expires_in_secs = 3600;
    f.e.state
        .bundle_minter
        .mint(f.tenant, App::StrataRag)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_401_drops_the_client_token_and_retries_once() {
    let f = fx(true).await;
    let m = &f.e.state.bundle_minter;
    m.mint(f.tenant, App::StrataRag).await.unwrap();
    assert_eq!(f.minter.lock().client_token_calls, 1);
    // Keycloak rotated its key: the cached client token is no longer valid
    f.minter.lock().valid_client_tokens.clear();
    m.mint(f.tenant, App::StrataIngest)
        .await
        .expect("retried with a fresh client token");
    assert_eq!(f.minter.lock().client_token_calls, 2);
    // a second 401 in a row is a refusal, not a loop
    f.minter.lock().reject_mints_401 = 5;
    let err = m.mint(f.tenant, App::InferenceRouter).await.unwrap_err();
    assert!(matches!(err, MintError::Unavailable(_)), "{err:?}");
    assert_eq!(
        f.minter.lock().client_token_calls,
        3,
        "one retry, then stop"
    );
}

#[tokio::test]
async fn a_mint_for_another_tenant_app_or_bundle_is_rejected() {
    let f = fx(true).await;
    let m = &f.e.state.bundle_minter;
    f.minter.lock().echo_app = Some("strata-ingest".into());
    assert!(m.mint(f.tenant, App::StrataRag).await.is_err());
    f.minter.lock().echo_app = None;
    f.minter.lock().echo_tenant = Some(Uuid::now_v7());
    assert!(m.mint(f.tenant, App::StrataRag).await.is_err());
    f.minter.lock().echo_tenant = None;
    f.minter.lock().echo_bundle = Some("other".into());
    assert!(m.mint(f.tenant, App::StrataRag).await.is_err());
    // nothing rejected was cached
    f.minter.lock().echo_bundle = None;
    let before = f.minter.mint_count();
    m.mint(f.tenant, App::StrataRag).await.unwrap();
    assert_eq!(f.minter.mint_count(), before + 1);
}

#[tokio::test]
async fn a_keycloak_outage_is_503_and_a_refusal_is_no_token() {
    let f = fx(true).await;
    f.minter.lock().token_down = true;
    let mut tx = begin_scoped(&f.e.state.db, f.tenant).await.unwrap();
    let err = token(&f.e.state, &mut tx, f.tenant, App::StrataRag)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "app_licence_unavailable");
    f.minter.lock().token_down = false;
    f.minter.lock().not_in_bundle.insert("strata-rag".into());
    assert!(
        token(&f.e.state, &mut tx, f.tenant, App::StrataRag)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn provisioning_ensures_the_bundle_mints_smokes_and_records_status_only() {
    let f = fx(true).await;
    let done = provision_tenant(&f.e.state, f.tenant, &actor())
        .await
        .unwrap()
        .expect("provisioned");
    assert!(
        done.iter().all(|(_, p)| *p == Provisioned::Stored),
        "{done:?}"
    );
    // the bundle was ensured once, by the licences-admin client, before any mint
    {
        let g = lock(&f.up.mock);
        assert_eq!(g.ensure_calls.len(), 1, "{:?}", g.ensure_calls);
        assert_eq!(g.ensure_calls[0].0, f.tenant.to_string());
        assert_eq!(g.ensure_calls[0].1, "agent");
        assert_eq!(
            g.ensure_calls[0].2,
            serde_json::json!({"plans": ["standard"]})
        );
    }
    let rows = status_rows(&f).await;
    assert_eq!(rows.len(), 3);
    for (app, licence, token_id, provisioned, err) in &rows {
        assert_eq!(licence.as_deref(), Some(format!("lic-{app}").as_str()));
        assert_eq!(token_id.as_deref(), Some(format!("tok-{app}").as_str()));
        assert!(*provisioned && err.is_none(), "{app}");
    }
    // no secret column at all
    let cols: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns WHERE table_name = 'tenant_app_licence'",
    )
    .fetch_all(&f.e.owner)
    .await
    .unwrap();
    assert!(
        !cols.iter().any(|c| c.contains("token") && c != "token_id"),
        "{cols:?}"
    );
    // tokens are minted on demand
    for app in ALL {
        assert_eq!(read(&f, app).await.unwrap().expose(), GOOD);
    }
    // the negative control came first for every app: bogus bearer, then the tenant's
    let seen = f.smoke.lock().unwrap().seen.clone();
    assert_eq!(seen.len(), 6, "{seen:?}");
    for pair in seen.chunks(2) {
        assert_ne!(pair[0].1, GOOD);
        assert_eq!(pair[1].1, GOOD);
        assert_eq!(pair[0].0, pair[1].0);
    }
}

#[tokio::test]
async fn a_failed_smoke_records_the_error_revokes_nothing_and_drops_the_cached_token() {
    let f = fx(true).await;
    f.smoke.lock().unwrap().refuse_good = true;
    let err = provision_app(&f.e.state, f.tenant, App::InferenceRouter, &actor())
        .await
        .unwrap()
        .expect_err("smoke refused");
    assert_eq!(err.code, "app_smoke_failed");
    assert!(err.retry);
    assert!(!err.message.contains(GOOD));
    let rows = status_rows(&f).await;
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].3, "not provisioned");
    assert!(
        rows[0]
            .4
            .as_deref()
            .unwrap()
            .starts_with("app_smoke_failed"),
        "{rows:?}"
    );
    // the shared bundle licence is never revoked
    assert!(lock(&f.up.mock).revoked.is_empty());
    assert!(
        lock(&f.up.mock)
            .calls
            .iter()
            .all(|c| !c.starts_with("revoke:"))
    );
    // the bad token was dropped from the cache: the retry mints again
    f.smoke.lock().unwrap().refuse_good = false;
    let again = provision_app(&f.e.state, f.tenant, App::InferenceRouter, &actor())
        .await
        .unwrap()
        .expect("retried");
    assert_eq!(again, Provisioned::Stored);
    assert_eq!(f.minter.mint_count(), 2);
    let rows = status_rows(&f).await;
    assert!(rows[0].3 && rows[0].4.is_none(), "error cleared: {rows:?}");
}

#[tokio::test]
async fn an_upstream_that_accepts_a_bogus_token_proves_nothing() {
    let f = fx(true).await;
    f.smoke.lock().unwrap().accept_all = true;
    let err = provision_app(&f.e.state, f.tenant, App::StrataIngest, &actor())
        .await
        .unwrap()
        .expect_err("not enforcing");
    assert_eq!(err.code, "app_not_enforcing");
    assert!(status_rows(&f).await.iter().all(|r| !r.3));
}

#[tokio::test]
async fn a_rerun_mints_and_ensures_nothing_more() {
    let f = fx(true).await;
    provision_tenant(&f.e.state, f.tenant, &actor())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(f.minter.mint_count(), 3);
    let again = provision_tenant(&f.e.state, f.tenant, &actor())
        .await
        .unwrap()
        .unwrap();
    assert!(
        again.iter().all(|(_, p)| *p == Provisioned::AlreadyStored),
        "{again:?}"
    );
    assert_eq!(f.minter.mint_count(), 3, "no second mint");
    assert_eq!(lock(&f.up.mock).ensure_calls.len(), 1, "no second ensure");
}

#[tokio::test]
async fn an_unconfigured_strata_is_skipped_not_minted() {
    let f = fx(false).await;
    let done = provision_tenant(&f.e.state, f.tenant, &actor())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        done.iter().map(|(_, p)| *p).collect::<Vec<_>>(),
        [
            Provisioned::Stored,
            Provisioned::NotConfigured,
            Provisioned::NotConfigured
        ]
    );
    assert_eq!(f.minter.mint_count(), 1);
    assert_eq!(status_rows(&f).await.len(), 1);
}

#[tokio::test]
async fn an_app_outside_the_bundle_is_a_visible_retry() {
    let f = fx(true).await;
    f.minter
        .lock()
        .not_in_bundle
        .insert("inference-router".into());
    let err = provision_tenant(&f.e.state, f.tenant, &actor())
        .await
        .unwrap()
        .expect_err("not in the bundle yet");
    assert_eq!((err.code, err.retry), ("app_not_in_bundle", true));
    assert!(err.message.contains("inference-router"), "{}", err.message);
    let rows = status_rows(&f).await;
    assert!(
        rows.iter().any(|r| r.0 == "inference-router"
            && !r.3
            && r.4
                .as_deref()
                .is_some_and(|e| e.starts_with("app_not_in_bundle"))),
        "{rows:?}"
    );
}

#[tokio::test]
async fn a_bundle_the_tenant_does_not_hold_is_a_visible_retry() {
    let f = fx(true).await;
    f.minter.lock().no_bundle = true;
    let err = provision_app(&f.e.state, f.tenant, App::StrataRag, &actor())
        .await
        .unwrap()
        .expect_err("no bundle");
    assert_eq!((err.code, err.retry), ("bundle_not_held", true));
}

#[tokio::test]
async fn a_forbidden_or_refusing_ensure_is_a_visible_retry_before_any_mint() {
    let f = fx(true).await;
    lock(&f.up.mock).ensure_status = 403;
    let err = provision_tenant(&f.e.state, f.tenant, &actor())
        .await
        .unwrap()
        .expect_err("forbidden");
    assert_eq!((err.code, err.retry), ("bundle_ensure_forbidden", true));
    lock(&f.up.mock).ensure_status = 0;
    lock(&f.up.mock).ensure_refused = vec![("strata-rag".into(), "no_plan".into())];
    let err = provision_tenant(&f.e.state, f.tenant, &actor())
        .await
        .unwrap()
        .expect_err("refused");
    assert_eq!(err.code, "bundle_ensure_refused");
    assert!(
        err.message.contains("strata-rag") && err.message.contains("no_plan"),
        "{}",
        err.message
    );
    assert_eq!(
        f.minter.mint_count(),
        0,
        "nothing minted before the ensure lands"
    );
}
