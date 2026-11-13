//! One in-process upstream (axum on 127.0.0.1:0) standing in for Keycloak (token + admin API), licences,
//! Turnstile and Telegram, recording every call. After strata_configurator `tests/signups.rs` mocks.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{Map, Value, json};
use uuid::Uuid;

pub const BOT_TOKEN: &str = "test-bot";

#[derive(Default)]
pub struct Mock {
    /// Portal-realm users: id → (email, attributes).
    pub users: Vec<(String, String, Map<String, Value>)>,
    /// Ids this mock created.
    pub created: Vec<String>,
    /// The representation of every accepted create, in order.
    pub created_reps: Vec<Value>,
    /// Ids holding a password credential (`GET users/{id}/credentials`).
    pub with_password: Vec<String>,
    /// Ids whose email is verified.
    pub email_verified: Vec<String>,
    /// Ids a `DELETE users/{id}` removed.
    pub deleted: Vec<String>,
    /// Ids a `PUT users/{id}` disabled (`enabled: false`).
    pub disabled: Vec<String>,
    /// Ids with an identity-provider link (`GET users/{id}/federated-identity`).
    pub idp_linked: Vec<String>,
    /// `GET users/{id}/credentials` answers 403 while set (the SA may not read credentials).
    pub credentials_forbidden: bool,
    /// `GET users/{id}/credentials` answers 500 while set.
    pub credentials_down: bool,
    /// execute-actions-email calls: (user id, query string, body).
    pub emails: Vec<(String, String, Value)>,
    /// execute-actions-email answers 500 while set.
    pub email_down: bool,
    /// Tenants registered in licences.
    pub registered: Vec<String>,
    /// (tenant header, holder sub) of every licence created.
    pub issued: Vec<(String, String)>,
    pub minted: usize,
    /// Every licences call answers 503 while set.
    pub licences_down: bool,
    pub verified: Vec<String>,
    /// Every accepted sendMessage payload.
    pub telegram: Vec<Value>,
    /// Every Keycloak admin read (users, clients, role mappings) answers 500 while set.
    pub keycloak_down: bool,
    /// (user id, client `clientId`, role): the effective client-role mappings.
    pub client_roles: Vec<(String, String, String)>,
    /// The role-mapping read alone answers 500 while set (user reads still work).
    pub role_mappings_down: bool,
    /// `POST /license/revoke` answers 200 `{"revoked": false}` (a no-op) while set.
    pub revoke_noop: bool,
    /// Licence ids `POST /license/revoke` revoked.
    pub revoked: Vec<String>,
    /// Licence ids a mint was asked for, in order.
    pub mint_ids: Vec<String>,
    /// Mints answer 503 while set (other licences calls still work).
    pub mint_down: bool,
    /// Every mint waits this long before answering (to make two runners race).
    pub mint_delay_ms: u64,
    /// Every `POST /license/revoke` waits this long before answering (a hung licences service, B1b T8).
    pub revoke_delay_ms: u64,
    /// The `region_id` of every accepted licence create (`None` when the body had none), in order (B1b T10).
    pub issued_regions: Vec<Option<String>>,
    /// `GET /regions` answers an empty list while set (licences has no region for any residency).
    pub no_regions: bool,
    /// `GET /regions` calls.
    pub region_lookups: usize,
    /// `GET /regions` answers 503 while set (other licences calls still work).
    pub regions_down: bool,
    /// `POST /license/revoke` answers the 200 no-op for these licence ids only.
    pub revoke_noop_ids: Vec<String>,
    /// `GET clients?clientId=` calls (the API must make none: it would need `view-clients`).
    pub client_lookups: usize,
    /// Licences calls in order: `mint:<licence id>`, `revoke:<licence id>` (successful ones).
    pub calls: Vec<String>,
    /// While set, every `POST /license/revoke` first records whether its id is already parked in some
    /// `tenants.licence_revoke_pending` (committed state, read on this pool) into `parked_at_revoke`.
    pub revoke_probe: Option<sqlx::PgPool>,
    pub parked_at_revoke: Vec<(String, bool)>,
    /// licences `find_active_license`: holder sub → its licence id. A create for that holder answers this id again
    /// while it is not revoked (licences' create is idempotent per tenant, app and subject).
    pub active_by_holder: HashMap<String, String>,
    /// id → (firstName, lastName) as Keycloak holds them (set on create, changed by a `PUT users/{id}` carrying them).
    pub names: HashMap<String, (Value, Value)>,
    /// A one-shot pause on the `POST /license/revoke` of this licence id: the handler notifies `.1` (reached) and
    /// waits on `.2` (release) before it answers, so a test can commit a racing transaction while the caller holds
    /// the licence lock across the call.
    pub revoke_gate: Option<(String, Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    /// `POST /internal/tenants/{id}/bundles/{key}/ensure` calls: (tenant id, bundle key, body), in order.
    pub ensure_calls: Vec<(String, String, Value)>,
    /// The ensure status (0 = 200). 403 models a caller without the `bundle-provisioner` role.
    pub ensure_status: u16,
    /// Apps the ensure answers as `refused` (app, reason).
    pub ensure_refused: Vec<(String, String)>,
}

pub type Shared = Arc<Mutex<Mock>>;

pub fn lock(m: &Shared) -> MutexGuard<'_, Mock> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn authed(h: &HeaderMap) -> bool {
    h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) == Some("Bearer sa-token")
}

async fn grant(body: String) -> Response {
    if !body.contains("grant_type=client_credentials") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    Json(json!({"access_token": "sa-token", "expires_in": 300})).into_response()
}

async fn find_users(
    State(m): State<Shared>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if lock(&m).keycloak_down {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let wanted = q.get("email").or_else(|| q.get("username")).cloned();
    let hits: Vec<Value> = lock(&m)
        .users
        .iter()
        .filter(|(_, e, _)| wanted.as_deref().is_some_and(|w| w.eq_ignore_ascii_case(e)))
        .map(|(id, e, _)| json!({"id": id, "username": e, "email": e}))
        .collect();
    Json(hits).into_response()
}

async fn create_user(State(m): State<Shared>, h: HeaderMap, Json(rep): Json<Value>) -> Response {
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let email = rep["email"].as_str().unwrap_or_default().to_string();
    let mut g = lock(&m);
    if g.keycloak_down {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if g.users
        .iter()
        .any(|(_, e, _)| e.eq_ignore_ascii_case(&email))
    {
        return StatusCode::CONFLICT.into_response();
    }
    let id = Uuid::new_v4().to_string();
    let attrs = rep["attributes"].as_object().cloned().unwrap_or_default();
    g.users.push((id.clone(), email, attrs));
    g.created.push(id.clone());
    g.names.insert(
        id.clone(),
        (rep["firstName"].clone(), rep["lastName"].clone()),
    );
    g.created_reps.push(rep);
    (
        StatusCode::CREATED,
        [(
            header::LOCATION,
            format!("http://kc.test/admin/realms/portal/users/{id}"),
        )],
    )
        .into_response()
}

async fn read_user(State(m): State<Shared>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let g = lock(&m);
    if g.keycloak_down {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match g.users.iter().find(|(i, _, _)| *i == id) {
        Some((i, e, attrs)) => {
            let mut rep = json!({
                "id": i, "username": e, "email": e, "enabled": !g.disabled.contains(i), "attributes": attrs,
                "emailVerified": g.email_verified.contains(i), "requiredActions": [],
            });
            if let Some((f, l)) = g.names.get(i) {
                if !f.is_null() {
                    rep["firstName"] = f.clone();
                }
                if !l.is_null() {
                    rep["lastName"] = l.clone();
                }
            }
            Json(rep).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn credentials(State(m): State<Shared>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let g = lock(&m);
    if g.credentials_down {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if g.credentials_forbidden {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !g.users.iter().any(|(i, _, _)| *i == id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let creds: Vec<Value> = if g.with_password.contains(&id) {
        vec![json!({"id": "c1", "type": "password"})]
    } else {
        vec![]
    };
    Json(creds).into_response()
}

async fn update_user(
    State(m): State<Shared>,
    h: HeaderMap,
    Path(id): Path<String>,
    Json(rep): Json<Value>,
) -> Response {
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut g = lock(&m);
    let Some(u) = g.users.iter_mut().find(|(i, _, _)| *i == id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    u.2 = rep["attributes"].as_object().cloned().unwrap_or_default();
    g.names.insert(
        id.clone(),
        (rep["firstName"].clone(), rep["lastName"].clone()),
    );
    if rep["enabled"] == json!(false) && !g.disabled.contains(&id) {
        g.disabled.push(id);
    } else if rep["enabled"] == json!(true) {
        g.disabled.retain(|d| *d != id);
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn delete_user(State(m): State<Shared>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut g = lock(&m);
    let before = g.users.len();
    g.users.retain(|(i, _, _)| *i != id);
    if g.users.len() == before {
        return StatusCode::NOT_FOUND.into_response();
    }
    g.deleted.push(id);
    StatusCode::NO_CONTENT.into_response()
}

async fn federated_identity(
    State(m): State<Shared>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let g = lock(&m);
    if !g.users.iter().any(|(i, _, _)| *i == id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let links: Vec<Value> = if g.idp_linked.contains(&id) {
        vec![json!({"identityProvider": "google", "userId": "g-1"})]
    } else {
        vec![]
    };
    Json(links).into_response()
}

async fn action_email(
    State(m): State<Shared>,
    h: HeaderMap,
    Path(id): Path<String>,
    axum::extract::RawQuery(q): axum::extract::RawQuery,
    Json(body): Json<Value>,
) -> StatusCode {
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED;
    }
    let mut g = lock(&m);
    if g.email_down {
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    g.emails.push((id, q.unwrap_or_default(), body));
    StatusCode::NO_CONTENT
}

async fn register(State(m): State<Shared>, Path(id): Path<String>) -> Response {
    let mut g = lock(&m);
    if g.licences_down {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    g.registered.push(id.clone());
    Json(json!({"id": id})).into_response()
}

async fn get_app(State(m): State<Shared>) -> Response {
    if lock(&m).licences_down {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    Json(json!({"id": "app-1", "key": "agent"})).into_response()
}

async fn plans(State(m): State<Shared>) -> Response {
    if lock(&m).licences_down {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    Json(json!([{"id": "plan-1", "code": "byo"}])).into_response()
}

fn tenant_header(h: &HeaderMap) -> String {
    h.get("x-tenant-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// The licences region ids the fake answers on `GET /regions`.
pub const REGION_EU: &str = "0195f48b-0000-7000-8000-0000000000e0";
pub const REGION_US: &str = "0195f48b-0000-7000-8000-0000000000e1";

async fn regions(State(m): State<Shared>) -> Response {
    let mut g = lock(&m);
    g.region_lookups += 1;
    if g.licences_down || g.regions_down {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if g.no_regions {
        return Json(json!([])).into_response();
    }
    Json(json!([
        {"id": REGION_EU, "code": "eu", "name": "Europe", "active": true},
        {"id": REGION_US, "code": "us", "name": "United States", "active": true},
    ]))
    .into_response()
}

async fn create_licence(
    State(m): State<Shared>,
    h: HeaderMap,
    Path(sub): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let mut g = lock(&m);
    if g.licences_down {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    g.issued_regions
        .push(body["region_id"].as_str().map(str::to_string));
    if let Some(id) = g.active_by_holder.get(&sub).cloned()
        && !g.revoked.contains(&id)
    {
        return Json(json!({"license_id": id, "subject_id": "subject-1"})).into_response();
    }
    g.issued.push((tenant_header(&h), sub));
    Json(json!({"license_id": format!("lic-{}", g.issued.len()), "subject_id": "subject-1"}))
        .into_response()
}

async fn mint(State(m): State<Shared>, h: HeaderMap, Path(id): Path<String>) -> Response {
    let delay = lock(&m).mint_delay_ms;
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    let mut g = lock(&m);
    if g.licences_down || g.mint_down {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    g.minted += 1;
    g.calls.push(format!("mint:{id}"));
    g.mint_ids.push(id);
    Json(json!({"token": format!("tok-{}", tenant_header(&h))})).into_response()
}

/// `GET clients?clientId=` → `[{id}]` (the client's internal id is `cid-<clientId>`).
async fn find_client(
    State(m): State<Shared>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut g = lock(&m);
    g.client_lookups += 1;
    if g.keycloak_down {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match q.get("clientId") {
        Some(c) => Json(json!([{"id": format!("cid-{c}"), "clientId": c}])).into_response(),
        None => Json(json!([])).into_response(),
    }
}

/// `GET users/{id}/role-mappings/clients/{cid}/composite` → the user's effective roles on that client.
async fn client_role_mappings(
    State(m): State<Shared>,
    h: HeaderMap,
    Path((id, cid)): Path<(String, String)>,
) -> Response {
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let g = lock(&m);
    if g.keycloak_down || g.role_mappings_down {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let roles: Vec<Value> = g
        .client_roles
        .iter()
        .filter(|(u, c, _)| *u == id && format!("cid-{c}") == cid)
        .map(|(_, _, r)| json!({"name": r}))
        .collect();
    Json(roles).into_response()
}

/// `POST /license/revoke {license_id}`, as licences `revoke_token_or_license` answers it (licenses main.py, origin/main
/// 2026-10-20): `{"revoked": true}` for a licence it holds — ALSO when that licence is already revoked (its
/// `revoke_license` is idempotent and answers true again); `{"revoked": false, "message": "License not found"}` only
/// for an unknown id or another tenant's (`revoke_noop` / `revoke_noop_ids` stand for that).
async fn ensure_bundle(
    State(m): State<Shared>,
    Path((id, key)): Path<(String, String)>,
    Json(b): Json<Value>,
) -> Response {
    let mut g = lock(&m);
    g.ensure_calls.push((id.clone(), key.clone(), b));
    match g.ensure_status {
        0 | 200 => Json(json!({
            "tenant_id": id,
            "bundle": key,
            "budget_currency": "EUR",
            "apps": {},
            "refused": g.ensure_refused.iter().map(|(a, r)| json!({"app": a, "reason": r})).collect::<Vec<_>>(),
        }))
        .into_response(),
        s => (
            StatusCode::from_u16(s).unwrap(),
            Json(json!({"detail": {"error": "bundle_not_ensurable_by_caller"}})),
        )
            .into_response(),
    }
}

async fn revoke(State(m): State<Shared>, Json(b): Json<Value>) -> Response {
    let delay = lock(&m).revoke_delay_ms;
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    let id = b["license_id"].as_str().unwrap_or_default().to_string();
    let gate = {
        let mut g = lock(&m);
        if g.revoke_gate.as_ref().is_some_and(|(gid, _, _)| *gid == id) {
            g.revoke_gate.take()
        } else {
            None
        }
    };
    if let Some((_, reached, release)) = gate {
        reached.notify_one();
        release.notified().await;
    }
    let probe = lock(&m).revoke_probe.clone();
    if let Some(db) = probe {
        let parked: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM tenants WHERE $1 = ANY (licence_revoke_pending))",
        )
        .bind(&id)
        .fetch_one(&db)
        .await
        .unwrap_or(false);
        lock(&m).parked_at_revoke.push((id.clone(), parked));
    }
    let mut g = lock(&m);
    if g.licences_down {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if g.revoke_noop || g.revoke_noop_ids.contains(&id) {
        return Json(json!({"revoked": false, "message": "License not found"})).into_response();
    }
    if g.revoked.contains(&id) {
        // Already revoked: licences answers true again (idempotent), and nothing changes there.
        g.calls.push(format!("rerevoke:{id}"));
        return Json(
            json!({"revoked": true, "message": "License and all tokens revoked successfully"}),
        )
        .into_response();
    }
    g.calls.push(format!("revoke:{id}"));
    g.revoked.push(id);
    Json(json!({"revoked": true})).into_response()
}

async fn siteverify(State(m): State<Shared>, body: String) -> Response {
    let token = body
        .split('&')
        .find_map(|kv| kv.strip_prefix("response="))
        .unwrap_or_default()
        .to_string();
    let ok = token == "good-token";
    lock(&m).verified.push(token);
    Json(json!({"success": ok})).into_response()
}

async fn send_message(State(m): State<Shared>, Json(b): Json<Value>) -> Response {
    lock(&m).telegram.push(b);
    Json(json!({"ok": true})).into_response()
}

pub struct Upstream {
    pub url: String,
    pub mock: Shared,
}

pub async fn upstream() -> Upstream {
    let mock: Shared = Arc::default();
    let app = Router::new()
        .route("/token", post(grant))
        .route(
            "/admin/realms/portal/users",
            get(find_users).post(create_user),
        )
        .route(
            "/admin/realms/portal/users/{id}",
            get(read_user).put(update_user).delete(delete_user),
        )
        .route(
            "/admin/realms/portal/users/{id}/federated-identity",
            get(federated_identity),
        )
        .route(
            "/admin/realms/portal/users/{id}/credentials",
            get(credentials),
        )
        .route(
            "/admin/realms/portal/users/{id}/execute-actions-email",
            put(action_email),
        )
        .route("/tenants/{id}", put(register))
        .route("/apps/{key}", get(get_app))
        .route("/apps/{key}/plans", get(plans))
        .route("/licenses/user/{sub}", post(create_licence))
        .route("/regions", get(regions))
        .route("/licenses/{id}/tokens", post(mint))
        .route("/license/revoke", post(revoke))
        .route(
            "/internal/tenants/{id}/bundles/{key}/ensure",
            post(ensure_bundle),
        )
        .route("/admin/realms/portal/clients", get(find_client))
        .route(
            "/admin/realms/portal/users/{id}/role-mappings/clients/{cid}/composite",
            get(client_role_mappings),
        )
        .route("/siteverify", post(siteverify))
        .route(&format!("/bot{BOT_TOKEN}/sendMessage"), post(send_message))
        .with_state(mock.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Upstream { url, mock }
}

/// The identity environment (Keycloak admin + licences) pointed at `up`; signup stays dark.
pub fn identity_env(up: &Upstream, vars: &mut HashMap<String, String>) {
    for (k, v) in super::identity_vars(&up.url) {
        vars.insert(k.to_string(), v);
    }
}

/// The signup environment pointed at `up` (Telegram on, Turnstile off).
pub fn signup_env(up: &Upstream, vars: &mut HashMap<String, String>) {
    identity_env(up, vars);
    let u = &up.url;
    for (k, v) in [
        ("EARLY_ACCESS_SIGNUP_ENABLED", "true".to_string()),
        ("TELEGRAM_BOT_TOKEN", BOT_TOKEN.to_string()),
        ("TELEGRAM_SIGNUP_CHAT_ID", "-1003934173351".to_string()),
        ("TELEGRAM_SIGNUP_THREAD_ID", "4242".to_string()),
        ("SIGNUP_TELEGRAM_RESIDENCY", "eu".to_string()),
        ("TELEGRAM_API_BASE", u.clone()),
        ("TURNSTILE_VERIFY_URL", format!("{u}/siteverify")),
    ] {
        vars.insert(k.to_string(), v);
    }
}
