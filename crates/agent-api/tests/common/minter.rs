//! A fake Keycloak token endpoint + licences bundle-token minter (the contract of licences main.py:10960-11230):
//! `POST /token` (client credentials) and `POST /internal/tenants/{id}/bundle-token?app=&bundle=&ttl_minutes=`.
//! [`shared`] is ONE process-wide instance every test environment points at (its own thread and runtime, so it
//! outlives any single test); a tenant has a token for an app only after [`MinterFake::grant`]. [`start`] makes an
//! independent instance for tests that count calls or inject faults.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;
use uuid::Uuid;

pub const CLIENT_ID: &str = "agent-bundle-minter";
pub const CLIENT_SECRET: &str = "minter-secret";

#[derive(Default)]
pub struct MinterState {
    /// (tenant, app) -> the bundle token the tenant holds.
    pub tokens: HashMap<(Uuid, String), String>,
    /// Apps outside the bundle's closure: 404 `app_not_in_bundle`.
    pub not_in_bundle: HashSet<String>,
    /// Every mint answers 404 `no_bundle` while set.
    pub no_bundle: bool,
    /// `POST /token` calls.
    pub client_token_calls: u32,
    /// The access tokens a mint accepts (every one `/token` issued and nobody revoked).
    pub valid_client_tokens: HashSet<String>,
    /// The next N mints answer 401 (a rotated realm key / revoked client token).
    pub reject_mints_401: u32,
    /// (tenant, app, bundle, ttl_minutes) of every mint request that reached the handler.
    pub mints: Vec<(String, String, String, String)>,
    /// Lifetime of every minted token, seconds (0 = 900).
    pub expires_in_secs: i64,
    /// Answer the echo with another app / tenant / bundle (the verification must reject it).
    pub echo_app: Option<String>,
    pub echo_tenant: Option<Uuid>,
    pub echo_bundle: Option<String>,
    /// The Keycloak token endpoint answers 500 while set.
    pub token_down: bool,
}

pub type Shared = Arc<Mutex<MinterState>>;

#[derive(Clone)]
pub struct MinterFake {
    pub url: String,
    pub state: Shared,
}

impl MinterFake {
    pub fn lock(&self) -> MutexGuard<'_, MinterState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `tenant` holds `token` for `app`.
    pub fn grant(&self, tenant: Uuid, app: &str, token: &str) {
        self.lock()
            .tokens
            .insert((tenant, app.to_string()), token.to_string());
    }

    pub fn mint_count(&self) -> usize {
        self.lock().mints.len()
    }
}

/// Point `vars` at `fake` (the `BUNDLE_MINTER_*` of every binary).
pub fn env(fake: &MinterFake, vars: &mut HashMap<String, String>) {
    for (k, v) in [
        ("BUNDLE_MINTER_LICENSES_URL", fake.url.clone()),
        ("BUNDLE_MINTER_TOKEN_URL", format!("{}/token", fake.url)),
        ("BUNDLE_MINTER_CLIENT_ID", CLIENT_ID.to_string()),
        ("BUNDLE_MINTER_CLIENT_SECRET", CLIENT_SECRET.to_string()),
    ] {
        vars.insert(k.to_string(), v);
    }
}

fn router(state: Shared) -> Router {
    Router::new()
        .route("/token", post(grant))
        .route("/internal/tenants/{id}/bundle-token", post(bundle_token))
        .with_state(state)
}

/// Start an independent instance on a thread of its own.
pub fn start() -> MinterFake {
    let state: Shared = Arc::default();
    let app = router(state.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("minter runtime");
        rt.block_on(async move {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(format!("http://{}", l.local_addr().unwrap()))
                .unwrap();
            axum::serve(l, app).await.unwrap();
        });
    });
    MinterFake {
        url: rx.recv().expect("minter url"),
        state,
    }
}

/// The process-wide instance.
pub fn shared() -> &'static MinterFake {
    static FAKE: OnceLock<MinterFake> = OnceLock::new();
    FAKE.get_or_init(start)
}

/// `tenant` holds `token` for `app` on the shared fake.
pub fn grant_shared(tenant: Uuid, app: &str, token: &str) {
    shared().grant(tenant, app, token);
}

async fn grant(State(s): State<Shared>, body: String) -> Response {
    let mut g = s.lock().unwrap_or_else(PoisonError::into_inner);
    if g.token_down {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if !body.contains("grant_type=client_credentials")
        || !body.contains(&format!("client_id={CLIENT_ID}"))
        || !body.contains(&format!("client_secret={CLIENT_SECRET}"))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    g.client_token_calls += 1;
    let token = format!("client-token-{}", g.client_token_calls);
    g.valid_client_tokens.insert(token.clone());
    Json(json!({"access_token": token, "expires_in": 300})).into_response()
}

fn detail(status: StatusCode, error: &str) -> Response {
    (
        status,
        Json(json!({"detail": {"error": error, "reason": error}})),
    )
        .into_response()
}

async fn bundle_token(
    State(s): State<Shared>,
    Path(id): Path<Uuid>,
    Query(q): Query<HashMap<String, String>>,
    h: HeaderMap,
) -> Response {
    let mut g = s.lock().unwrap_or_else(PoisonError::into_inner);
    let bearer = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if g.reject_mints_401 > 0 {
        g.reject_mints_401 -= 1;
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !g.valid_client_tokens.contains(bearer) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let app = q.get("app").cloned().unwrap_or_default();
    let bundle = q.get("bundle").cloned().unwrap_or_default();
    let ttl = q.get("ttl_minutes").cloned().unwrap_or_default();
    g.mints
        .push((id.to_string(), app.clone(), bundle.clone(), ttl));
    if g.no_bundle {
        return detail(StatusCode::NOT_FOUND, "no_bundle");
    }
    if g.not_in_bundle.contains(&app) {
        return detail(StatusCode::NOT_FOUND, "app_not_in_bundle");
    }
    let Some(token) = g.tokens.get(&(id, app.clone())).cloned() else {
        return detail(StatusCode::NOT_FOUND, "no_app_licence");
    };
    let secs = if g.expires_in_secs == 0 {
        900
    } else {
        g.expires_in_secs
    };
    let expires = chrono::Utc::now() + chrono::Duration::seconds(secs);
    (
        StatusCode::CREATED,
        Json(json!({
            "token": token,
            "expires_at": expires.to_rfc3339(),
            "tenant_id": g.echo_tenant.unwrap_or(id).to_string(),
            "bundle": g.echo_bundle.clone().unwrap_or(bundle),
            "app": g.echo_app.clone().unwrap_or(app.clone()),
            "license_id": format!("lic-{app}"),
            "token_id": format!("tok-{app}"),
        })),
    )
        .into_response()
}
