//! B12 fixtures (crate::conversations): a fake agent-edge (`/internal/deliver`, `/internal/notify-workers`) on
//! 127.0.0.1:0, a fixed-answer interaction backend, and a tenant with one job and members holding the support roles.
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_api::interaction::{BackendError, BackendEvent, InteractionBackend, InteractionRequest};
use agent_api::runtime::heartbeat::{self, Serving};
use agent_api::runtime::{Env, Scope, interaction as runtime_interaction};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use serde_json::{Value, json};
use uuid::Uuid;

use super::artifacts_fake::{self, Artifacts};
use super::{TestEnv, insert_tenant, insert_user, json as req_json, person, send};

/// The licence token of tenant Alpha in every B12 test (the fake artifact_service is pinned to it).
pub const ALPHA_LICENCE: &str = "lic-token-alpha-0001";

/// The edge key of every B12 test (at least 32 characters).
pub const EDGE_KEY: &str = "edge-internal-key-for-the-b12-tests-0001";

/// What the fake edge received.
#[derive(Clone, Default)]
pub struct Edge {
    pub url: String,
    pub delivered: Arc<Mutex<Vec<Value>>>,
    pub notified: Arc<Mutex<Vec<Value>>>,
    /// `/internal/worker-note` bodies (SW-11: an internal note posted to a helpdesk ticket).
    pub notes: Arc<Mutex<Vec<Value>>>,
    /// The status of each next `/internal/worker-note` call, popped in order (204 when empty); a non-2xx one answers
    /// `{"error": {"code": ...}}`. Every call is recorded in `notes` whatever it answers.
    pub note_status: Arc<Mutex<VecDeque<u16>>>,
    /// Answer `/internal/deliver` with 502.
    pub fail: Arc<AtomicBool>,
    /// Requests that did not carry the edge key.
    pub unauthorized: Arc<AtomicUsize>,
}

impl Edge {
    pub fn delivered(&self) -> Vec<Value> {
        self.delivered.lock().unwrap().clone()
    }
    pub fn notified(&self) -> Vec<Value> {
        self.notified.lock().unwrap().clone()
    }
    pub fn notes(&self) -> Vec<Value> {
        self.notes.lock().unwrap().clone()
    }
    /// The next `/internal/worker-note` calls answer `statuses` in order, then 204.
    pub fn note_answers(&self, statuses: &[u16]) {
        self.note_status
            .lock()
            .unwrap()
            .extend(statuses.iter().copied());
    }
}

fn authorized(e: &Edge, h: &HeaderMap) -> bool {
    let ok = h.get("authorization").and_then(|v| v.to_str().ok())
        == Some(format!("Bearer {EDGE_KEY}").as_str());
    if !ok {
        e.unauthorized.fetch_add(1, Ordering::SeqCst);
    }
    ok
}

async fn deliver(State(e): State<Edge>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    if !authorized(&e, &h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    e.delivered.lock().unwrap().push(b);
    if e.fail.load(Ordering::SeqCst) {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": {"code": "delivery_failed"}})),
        )
            .into_response();
    }
    Json(json!({"external_id": format!("out-{}", Uuid::new_v4())})).into_response()
}

async fn worker_note(State(e): State<Edge>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    if !authorized(&e, &h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    e.notes.lock().unwrap().push(b);
    let status = e.note_status.lock().unwrap().pop_front().unwrap_or(204);
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::NO_CONTENT);
    if status.is_success() {
        return status.into_response();
    }
    (status, Json(json!({"error": {"code": "note_failed"}}))).into_response()
}

async fn notify(State(e): State<Edge>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    if !authorized(&e, &h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let item = b["item"]["id"].as_str().unwrap_or("").to_string();
    let results: Vec<Value> = b["targets"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|t| {
            json!({
                "binding_id": t["binding_id"], "ok": true,
                "thread": {"channel": t["kind"], "id": format!("thread-{item}"), "route": {}},
            })
        })
        .collect();
    e.notified.lock().unwrap().push(b);
    Json(json!({"results": results})).into_response()
}

/// Start the fake edge.
pub async fn edge() -> Edge {
    let mut e = Edge::default();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    e.url = format!("http://{}", l.local_addr().unwrap());
    let app = Router::new()
        .route("/internal/deliver", post(deliver))
        .route("/internal/notify-workers", post(notify))
        .route("/internal/worker-note", post(worker_note))
        .with_state(e.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    e
}

/// Answers `text`; counts calls and keeps the last request.
pub struct Fixed {
    pub text: String,
    pub calls: AtomicUsize,
    pub last: Mutex<Option<InteractionRequest>>,
}

impl Fixed {
    pub fn new(text: &str) -> Arc<Self> {
        Arc::new(Self {
            text: text.into(),
            calls: AtomicUsize::new(0),
            last: Mutex::new(None),
        })
    }
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl InteractionBackend for Fixed {
    fn respond(
        &self,
        req: InteractionRequest,
    ) -> BoxStream<'static, Result<BackendEvent, BackendError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last.lock().unwrap() = Some(req);
        stream::iter(vec![
            Ok(BackendEvent::Delta(self.text.clone())),
            Ok(BackendEvent::Done { usage: None }),
        ])
        .boxed()
    }
}

/// `TENANT_DATA_KEYS` with one kid `k1` (key bytes built at runtime).
pub fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
    vars.insert("TENANT_DATA_KEYS".into(), format!(r#"{{"k1":"{key}"}}"#));
    vars.insert("TENANT_DATA_KEY_CURRENT".into(), "k1".into());
}

pub struct Ctx {
    pub e: TestEnv,
    pub t: Uuid,
    pub job: Uuid,
    pub edge: Edge,
    /// The fake artifact_service (SW-9 attachment bytes), pinned to Alpha's licence ([`ALPHA_LICENCE`]).
    pub artifacts: Artifacts,
    pub backend: Arc<Fixed>,
    /// The job's verified web chat binding (`wk_test`): every [`Ctx::inbound`] arrives on it.
    pub wc: Uuid,
    /// The web chat session (B13) of each [`Ctx::inbound`] conversation: `(session_id, session_token, ip_hash)`.
    pub sessions: Arc<Mutex<HashMap<String, Session>>>,
}

/// `(session_id, session_token, ip_hash)` of one web chat conversation.
pub type Session = (String, String, String);

impl Ctx {
    /// POST `body` to an internal route with the edge key.
    pub async fn internal(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let mut r = req_json("POST", path, None, body);
        r.headers_mut().insert(
            "authorization",
            format!("Bearer {EDGE_KEY}").parse().unwrap(),
        );
        send(&self.e.app, r).await
    }

    /// GET an internal route with the edge key.
    pub async fn internal_get(&self, path: &str) -> (StatusCode, Value) {
        let r = axum::http::Request::get(path)
            .header("authorization", format!("Bearer {EDGE_KEY}"))
            .body(axum::body::Body::empty())
            .unwrap();
        send(&self.e.app, r).await
    }

    /// A web chat session (B13) on `wk_test` from the edge's own origin and `ip_hash`: `(session_id, token)`, or
    /// `None` when the sessions route refuses.
    pub async fn session(&self, ip_hash: &str) -> Option<(String, String)> {
        let (s, b) = self
            .internal(
                "/api/internal/channel/webchat/sessions",
                json!({"binding_id": self.wc, "widget_key": "wk_test",
                       "origin": "https://chat.nonagentic.ai", "ip_hash": ip_hash}),
            )
            .await;
        (s == StatusCode::CREATED).then(|| {
            (
                b["session_id"].as_str().unwrap_or_default().to_string(),
                b["session_token"].as_str().unwrap_or_default().to_string(),
            )
        })
    }

    /// One customer message on web chat conversation `conv`, in that conversation's own session (opened on its first
    /// message, one client IP hash per conversation). When no session can be opened (e.g. the binding is unverified)
    /// the message goes without one, so the binding refusal is what answers.
    pub async fn inbound(&self, conv: &str, msg: &str, text: &str) -> (StatusCode, Value) {
        let known = self.sessions.lock().unwrap().get(conv).cloned();
        let sess = match known {
            Some(s) => Some(s),
            None => {
                let ip = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(conv.as_bytes()));
                let opened = self.session(&ip).await.map(|(id, tok)| (id, tok, ip));
                if let Some(o) = &opened {
                    self.sessions
                        .lock()
                        .unwrap()
                        .insert(conv.to_string(), o.clone());
                }
                opened
            }
        };
        let mut body = json!({
            "kind": "web_chat", "external_id": "wk_test", "binding_id": self.wc,
            "tenant": self.t, "job_id": self.job,
            "job_key": "j1", "conversation": {"channel": "web_chat", "id": conv, "route": {}},
            "message_id": msg, "party": {"id": "cust-1", "name": "Femke", "verified": false},
            "text": text,
        });
        if let Some((id, token, ip)) = sess {
            body["party"]["id"] = json!(format!("wc:{id}"));
            body["session_token"] = json!(token);
            body["ip_hash"] = json!(ip);
        }
        self.internal("/api/internal/channel/inbound", body).await
    }

    /// `method path` as `sub` with `body`.
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        sub: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        send(
            &self.e.app,
            req_json(method, path, Some(&person(sub, &[])), body),
        )
        .await
    }

    /// SW T2 Take: `POST /api/jobs/j1/conversations/{conversation_id}/take` as `sub`, expecting the owner
    /// `expected` (`agent`, `queue` or `user:<sub>`). The body is an `OwnerView`.
    pub async fn take(
        &self,
        conversation_id: &str,
        sub: &str,
        expected: &str,
    ) -> (StatusCode, Value) {
        self.call(
            "POST",
            &format!("/api/jobs/j1/conversations/{conversation_id}/take"),
            sub,
            json!({ "expected_owner": expected }),
        )
        .await
    }

    /// SW T2 Assign…: `POST …/conversations/{conversation_id}/assign` as `sub` (support_manager), to `to`.
    pub async fn assign(
        &self,
        conversation_id: &str,
        sub: &str,
        to: &str,
        expected: &str,
    ) -> (StatusCode, Value) {
        self.call(
            "POST",
            &format!("/api/jobs/j1/conversations/{conversation_id}/assign"),
            sub,
            json!({ "user_sub": to, "expected_owner": expected }),
        )
        .await
    }

    /// SW T2 Hand back: `POST …/conversations/{conversation_id}/hand-back` as `sub` with `note`.
    pub async fn hand_back(
        &self,
        conversation_id: &str,
        sub: &str,
        note: &str,
    ) -> (StatusCode, Value) {
        self.call(
            "POST",
            &format!("/api/jobs/j1/conversations/{conversation_id}/hand-back"),
            sub,
            json!({ "note": note }),
        )
        .await
    }

    /// SW T4 Resolve: `POST /api/jobs/j1/handover/{item}/resolve` as `sub` with `body` (`{outcome, closing_message?,
    /// client_msg_id?, feedback?}`). The body is an `OwnerView`.
    pub async fn resolve(&self, item: &str, sub: &str, body: Value) -> (StatusCode, Value) {
        self.call(
            "POST",
            &format!("/api/jobs/j1/handover/{item}/resolve"),
            sub,
            body,
        )
        .await
    }

    /// The handover item `item` as the queue shows it (`HandoverItem`), read as the observer.
    pub async fn item(&self, item: &str) -> Value {
        let (s, q) = self.get("/api/jobs/j1/handover", "u-obs").await;
        assert_eq!(s, StatusCode::OK, "{q}");
        q["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["id"] == item)
            .cloned()
            .unwrap_or_else(|| panic!("{item} is not in the queue: {q}"))
    }

    /// SW T3 Send: `POST /api/jobs/j1/conversations/{conversation_id}/messages` as `sub`, `kind` `reply` or `note`,
    /// with an optional `client_msg_id`. The body is a `MessageView` (200, or 202 when the delivery failed).
    pub async fn send_message(
        &self,
        conversation_id: &str,
        sub: &str,
        kind: &str,
        text: &str,
        client_msg_id: Option<Uuid>,
    ) -> (StatusCode, Value) {
        self.call(
            "POST",
            &format!("/api/jobs/j1/conversations/{conversation_id}/messages"),
            sub,
            json!({ "kind": kind, "text": text, "client_msg_id": client_msg_id }),
        )
        .await
    }

    /// The conversation of handover item `item` (any tenant: the owner pool).
    pub async fn conversation_of(&self, item: &str) -> String {
        self.texts(&format!(
            "SELECT conversation_id FROM agent_handover_items WHERE id = '{item}' AND tenant_id = '{}'",
            self.t
        ))
        .await
        .remove(0)
    }

    pub async fn get(&self, path: &str, sub: &str) -> (StatusCode, Value) {
        send(&self.e.app, super::get(path, Some(&person(sub, &[])))).await
    }

    /// Mark every binding verified (as if `verify <code>` had arrived from each account).
    pub async fn verify_all(&self) {
        sqlx::query(
            "UPDATE connector_bindings SET verified_at = now(), verify_code = NULL WHERE verified_at IS NULL",
        )
        .execute(&self.e.owner)
        .await
        .unwrap();
    }

    /// D130: a customer binding created through the API starts in the test env; fixtures whose traffic expects the
    /// live (prod) path move every customer binding there (the publish route is covered in tests/channel_publish.rs).
    pub async fn go_live(&self) {
        sqlx::query("UPDATE connector_bindings SET env = 'prod' WHERE side = 'customer'")
            .execute(&self.e.owner)
            .await
            .unwrap();
    }

    /// A live `interaction` process serving j1's `env` scope (one heartbeat, 60 s): the runtime answers its
    /// customers. Needs `REDIS_URL` ([`setup_with`] over the shared test Redis's vars).
    pub async fn serve(&self, env: Env) {
        serve(&self.e.state, self.t, "j1", env).await;
    }

    /// A customer web chat conversation `wc` (the ref [`Ctx::inbound`] uses) holding customer message `msg` / `text`
    /// on its open case (a new `CS-n` when none is open): what a customer's turn leaves behind, for tests about
    /// conversations, cases and staff — not about answering (D109: only the runtime answers). `(conversation_id,
    /// case_id)`.
    pub async fn customer_case(&self, wc: &str, msg: &str, text: &str) -> (String, String) {
        use agent_api::conversations::store;
        let mut tx = store::begin_job(&self.e.state.db, self.t, self.job)
            .await
            .unwrap();
        let cref = agent_api::interaction::ConversationRef {
            channel: "web_chat".into(),
            id: wc.into(),
            // as the edge's inbound stores it: the binding the conversation arrived on
            route: [("binding_id".to_string(), self.wc.to_string())].into(),
        };
        let conv = store::upsert_customer_conversation(
            &mut tx,
            self.job,
            "web_chat",
            &cref,
            "cust-1",
            Some("Femke"),
            "prod",
        )
        .await
        .unwrap();
        let design = store::design_of(&mut tx, self.job, "j1", &conv)
            .await
            .unwrap();
        let case_id = store::case_for(&mut tx, self.job, &conv, text, &design)
            .await
            .unwrap();
        store::append(
            &mut tx,
            store::NewMessage {
                job_id: self.job,
                conversation_id: &conv.id,
                role: "customer",
                author: "Femke",
                text,
                case_id: Some(&case_id),
                external_id: Some(msg),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        (conv.id, case_id)
    }

    /// Rows of `sql` (one text column) through the owner pool.
    pub async fn texts(&self, sql: &str) -> Vec<String> {
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
            .fetch_all(&self.e.owner)
            .await
            .unwrap()
    }
}

/// The release id [`fake_release`] makes current.
pub const FAKE_RELEASE: &str = "cfg-2026-10-22.1";

/// The job template a live fixture job is created from: `common::platform::offering`'s tag (`Offering::template`).
pub const LIVE_TEMPLATE: &str = "job/customer_care_agent@v4";

/// D109 (KH T6): a LIVE job `key` of `tenant` with a runtime design — created from [`LIVE_TEMPLATE`] through the
/// platform route (`POST /api/tenants/{id}/jobs`, super_admin), its required Production operations routed, the
/// cuttable selection written and the draft approved and applied as its release (crate::common::design_contract
/// `publish`). The job id.
pub async fn live_job(e: &TestEnv, tenant: Uuid, key: &str) -> Uuid {
    let offered = super::platform::offering(e).await;
    debug_assert_eq!(offered.template(), LIVE_TEMPLATE);
    // B7 gate 2 fails closed: a live job's tenant runs turns, so it holds a fresh active licence verdict
    super::licence_active(&e.owner, tenant).await;
    let (s, b) = send(
        &e.app,
        req_json(
            "POST",
            &format!("/api/tenants/{tenant}/jobs"),
            Some(&super::super_admin()),
            json!({"template": LIVE_TEMPLATE, "key": key, "label": key}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let id: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
        .bind(tenant)
        .bind(key)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let f = super::design_contract::contract_job(tenant, id, key);
    super::design_contract::publishable(e, &f).await;
    super::design_contract::publish(e, &f).await;
    id
}

/// A FAKE live release for `job` of `tenant` ([`FAKE_RELEASE`], empty bundle, release pins `pins`) as
/// `jobs.current_release_id`, the job's own pins left as they are: a live release whose runtime design does not
/// compile (a job inserted with no pins) — D109's 409 `job_not_live`.
pub async fn fake_release(db: &sqlx::PgPool, tenant: Uuid, job: Uuid, pins: &str) {
    let sha = |c: char| format!("sha256:{}", c.to_string().repeat(64));
    sqlx::query(
        r#"INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins,
               provenance, requires, causes, state, approved_by, approved_at, released_by, released_at,
               snapshot_kinds)
           VALUES ($1, $2, $3, $4, $5, '{"sections": []}', '{}', $6::jsonb, '{}', '{}', '[]', 'current', 'u-rm',
               now(), 'u-rm', now(), ARRAY['languages', 'setting'])"#,
    )
    .bind(tenant)
    .bind(job)
    .bind(FAKE_RELEASE)
    .bind(sha('a'))
    .bind(sha('b'))
    .bind(pins)
    .execute(db)
    .await
    .unwrap();
    sqlx::query("UPDATE jobs SET current_release_id = $2 WHERE id = $1")
        .bind(job)
        .bind(FAKE_RELEASE)
        .execute(db)
        .await
        .unwrap();
}

/// A live `interaction` process serving `job_key`'s `env` scope of `tenant` (one heartbeat, 60 s, on the state's
/// Redis — `REDIS_URL` must be set).
pub async fn serve(st: &agent_api::state::AppState, tenant: Uuid, job_key: &str, env: Env) {
    let root = st.redis.clone().expect("REDIS_URL set");
    let scope = Scope::new(tenant, job_key, env).unwrap();
    let serving = Serving::default();
    serving.set(&scope, None, None);
    let consumer = format!("kh-t6-{}", Uuid::new_v4().simple());
    heartbeat::beat(
        &root,
        runtime_interaction::ROLE,
        &consumer,
        0,
        &serving,
        std::time::Duration::from_secs(60),
    )
    .await
    .unwrap();
}

/// D109: the B12 fixture on the runtime — `vars` point it at the shared test Redis (`redis_env::api_vars`), job `j1`
/// is live ([`live_job`]) and a live `interaction` process serves its prod scope, so every message the agent would answer is a
/// runtime turn (job `j1` binds no qualifier: the drain answers `turn::TURN_FAILED`).
pub async fn setup_runtime(vars: [(String, String); 2]) -> Ctx {
    let c = setup_live(|v| v.extend(vars)).await;
    c.serve(Env::Prod).await;
    c
}

/// The B12 fixture with job `j1` LIVE ([`live_job`]: a released runtime design), not yet served.
pub async fn setup_live(tweak: impl FnOnce(&mut HashMap<String, String>)) -> Ctx {
    setup_inner(tweak, true).await
}

/// A B12 env: the fake edge wired in (`EDGE_INTERNAL_KEY`, `EDGE_DELIVER_URL`), tenant data keys, a fixed backend,
/// tenant Alpha with job `j1` and members `u-admin` (every role), `u-agent` / `u-agent2` (support_agent), `u-mgr`
/// (support_manager), `u-int` (integrations_manager), `u-obs` (observer).
pub async fn setup() -> Ctx {
    setup_with(|_| {}).await
}

pub async fn setup_with(tweak: impl FnOnce(&mut HashMap<String, String>)) -> Ctx {
    setup_inner(tweak, false).await
}

async fn setup_inner(tweak: impl FnOnce(&mut HashMap<String, String>), live: bool) -> Ctx {
    let edge = edge().await;
    let url = edge.url.clone();
    let artifacts = artifacts_fake::artifacts().await;
    let artifacts_url = artifacts.url.clone();
    let mut e = super::env_with(|v| {
        v.insert("EDGE_INTERNAL_KEY".into(), EDGE_KEY.into());
        v.insert("EDGE_DELIVER_URL".into(), url);
        v.insert("ARTIFACTS_URL".into(), artifacts_url);
        with_keys(v);
        tweak(v);
    })
    .await;
    let backend = Fixed::new("Your order ships tomorrow.");
    e.state.interaction = backend.clone();
    e.app = agent_api::router(e.state.clone());
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    // B7 gate 2 fails closed: Alpha runs turns, so it holds a fresh active licence verdict
    super::licence_active(&e.owner, t).await;
    // SW-9: Alpha holds a sealed licence, the credential of its attachment bytes at artifact_service
    let sealed = e.state.identity.cipher.seal(t, ALPHA_LICENCE).unwrap();
    sqlx::query("UPDATE tenants SET licence_id = $2, licence_token_encrypted = $3 WHERE id = $1")
        .bind(t)
        .bind(format!("lic-test-{t}"))
        .bind(sealed)
        .execute(&e.owner)
        .await
        .unwrap();
    artifacts.pin(t, ALPHA_LICENCE);
    for (sub, role) in [
        ("u-admin", "tenant_admin"),
        ("u-agent", "support_agent"),
        ("u-agent2", "support_agent"),
        ("u-mgr", "support_manager"),
        ("u-int", "integrations_manager"),
        ("u-obs", "tenant_user"),
    ] {
        insert_user(&e.owner, sub, t, role).await;
        sqlx::query("UPDATE users SET email = $2 WHERE sub = $1")
            .bind(sub)
            .bind(format!("{}@alpha.example", sub.trim_start_matches("u-")))
            .execute(&e.owner)
            .await
            .unwrap();
    }
    let job: Uuid = if live {
        live_job(&e, t, "j1").await
    } else {
        sqlx::query_scalar(
            "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'j1', 'J1', '[]', 'test') \
             RETURNING id",
        )
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap()
    };
    let wc: Uuid = sqlx::query_scalar(
        "INSERT INTO connector_bindings (tenant_id, job_id, kind, side, external_id, label, created_by, verified_at) \
         VALUES ($1, $2, 'web_chat', 'customer', 'wk_test', 'Site', 'test', now()) RETURNING id",
    )
    .bind(t)
    .bind(job)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    Ctx {
        e,
        t,
        job,
        edge,
        artifacts,
        backend,
        wc,
        sessions: Arc::default(),
    }
}

pub fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}
