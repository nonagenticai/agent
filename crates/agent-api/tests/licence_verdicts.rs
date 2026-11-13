//! B7 T2 licence verdicts (crate::licensing::verdict, crate::licensing::introspect) + addendum R3 (re-issue crash
//! reconcile) and R4 (`agent_licence_pending_seconds`). Real Postgres 18 (one container for this binary); licences'
//! `POST /license/introspect` and the service-account token endpoint are an in-process stub.
use crate::common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_api::config::Config;
use agent_api::licensing::verdict::{self, Verdict};
use agent_api::state::AppState;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use common::TestEnv;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Default)]
struct Stub {
    /// Answer by raw token; unknown tokens answer `token_not_found`.
    answers: HashMap<String, Value>,
    hits: usize,
    down: bool,
    auth: Vec<Option<String>>,
}

type Shared = Arc<Mutex<Stub>>;

async fn introspect(State(s): State<Shared>, h: HeaderMap, Json(b): Json<Value>) -> Response {
    let mut m = s.lock().unwrap();
    m.hits += 1;
    let auth = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    m.auth.push(auth);
    if m.down {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let token = b["token"].as_str().unwrap_or_default();
    let answer = m
        .answers
        .get(token)
        .cloned()
        .unwrap_or_else(|| json!({"active": false, "reason": "token_not_found"}));
    Json(answer).into_response()
}

async fn grant() -> Response {
    Json(json!({"access_token": "sa-token", "expires_in": 300})).into_response()
}

struct Licences {
    url: String,
    s: Shared,
    // Each test has its own database, but pending-licence gauges are process-global. A refresh against a second
    // database clears the first database's series. Keep a test's refresh and metric assertions in one scope.
    _gauge_guard: tokio::sync::MutexGuard<'static, ()>,
}

impl Licences {
    fn answer(&self, token: &str, v: Value) {
        self.s.lock().unwrap().answers.insert(token.into(), v);
    }
    fn hits(&self) -> usize {
        self.s.lock().unwrap().hits
    }
}

async fn licences() -> Licences {
    static GAUGES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let gauge_guard = GAUGES.lock().await;
    let s: Shared = Arc::default();
    let app = Router::new()
        .route("/license/introspect", post(introspect))
        .route("/token", post(grant))
        .with_state(s.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Licences {
        url,
        s,
        _gauge_guard: gauge_guard,
    }
}

fn stub_env(up: &Licences, vars: &mut HashMap<String, String>) {
    for (k, v) in common::identity_vars(&up.url) {
        vars.insert(k.to_string(), v);
    }
    vars.insert(
        "LICENSE_INTROSPECT_URL".into(),
        format!("{}/license/introspect", up.url),
    );
}

async fn setup() -> (TestEnv, Licences) {
    let up = licences().await;
    let e = common::env_with(|v| stub_env(&up, v)).await;
    (e, up)
}

fn active(t: Uuid, lic: &str) -> Value {
    json!({
        "active": true, "tenant_id": t.to_string(), "license_id": lic, "status": "active",
        "app": {"key": "agent"}, "plan": {"code": "byo"}, "roles": []
    })
}

fn revoked() -> Value {
    json!({"active": false, "reason": "token_invalid"})
}

/// A tenant holding licence `licence` with `token` sealed as production stores it (or no licence at all).
async fn tenant(st: &AppState, owner: &PgPool, licence: Option<(&str, &str)>) -> Uuid {
    let t = Uuid::new_v4();
    let (id, sealed) = match licence {
        Some((id, token)) => (Some(id), Some(st.identity.cipher.seal(t, token).unwrap())),
        None => (None, None),
    };
    sqlx::query(
        "INSERT INTO tenants (id, name, residency, updated_by, licence_id, licence_token_encrypted) \
         VALUES ($1, $2, 'eu', 'test', $3, $4)",
    )
    .bind(t)
    .bind(format!("T {t}"))
    .bind(id)
    .bind(sealed)
    .execute(owner)
    .await
    .unwrap();
    t
}

async fn stored(owner: &PgPool, t: Uuid) -> Option<(String, Option<String>, Option<String>)> {
    sqlx::query_as(
        "SELECT status, reason, licence_id FROM tenant_licence_verdict WHERE tenant_id = $1",
    )
    .bind(t)
    .fetch_optional(owner)
    .await
    .unwrap()
}

async fn write(e: &TestEnv, t: Uuid) -> Verdict {
    verdict::write(&e.state, t)
        .await
        .unwrap()
        .expect("a verdict is written")
}

#[tokio::test]
async fn refresher_writes_one_row_per_tenant() {
    let (e, up) = setup().await;
    let a = tenant(&e.state, &e.owner, Some(("lic-a", "tok-a"))).await;
    let b = tenant(&e.state, &e.owner, Some(("lic-b", "tok-b"))).await;
    let none = tenant(&e.state, &e.owner, None).await;
    up.answer("tok-a", active(a, "lic-a"));
    up.answer("tok-b", active(b, "lic-b"));
    for _ in 0..2 {
        let r = verdict::refresh_all(&e.state).await.unwrap();
        assert_eq!((r.tenants, r.written, r.failed), (3, 3, 0), "{r:?}");
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM tenant_licence_verdict")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 3, "one row per tenant, rewritten in place");
    assert_eq!(
        stored(&e.owner, a).await,
        Some(("active".into(), None, Some("lic-a".into())))
    );
    assert_eq!(
        stored(&e.owner, none).await,
        Some(("none".into(), Some("no_licence".into()), None))
    );
    let v = write(&e, b).await;
    assert_eq!(v.plan.as_deref(), Some("byo"));
    assert!(!v.fails_closed(), "{v:?}");
}

#[tokio::test]
async fn revoked_licence_writes_revoked() {
    let (e, up) = setup().await;
    let t = tenant(&e.state, &e.owner, Some(("lic-r", "tok-r"))).await;
    up.answer("tok-r", revoked());
    let v = write(&e, t).await;
    assert_eq!(v.status, "revoked");
    assert_eq!(v.reason.as_deref(), Some("token_invalid"));
    assert!(v.fails_closed());
}

#[tokio::test]
async fn unreadable_token_is_inactive_unreadable() {
    let (e, up) = setup().await;
    let t = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO tenants (id, name, residency, updated_by, licence_id, licence_token_encrypted) \
         VALUES ($1, $2, 'eu', 'test', 'lic-u', 'v2:gone:not-a-sealed-token')",
    )
    .bind(t)
    .bind(format!("T {t}"))
    .execute(&e.owner)
    .await
    .unwrap();
    let v = write(&e, t).await;
    assert_eq!(
        (v.status.as_str(), v.reason.as_deref()),
        ("inactive", Some("license_unreadable"))
    );
    assert_eq!(up.hits(), 0, "nothing to introspect");
}

#[tokio::test]
async fn introspect_outage_never_keeps_active() {
    let (e, up) = setup().await;
    let t = tenant(&e.state, &e.owner, Some(("lic-o", "tok-o"))).await;
    up.answer("tok-o", active(t, "lic-o"));
    assert_eq!(write(&e, t).await.status, "active");
    up.s.lock().unwrap().down = true;
    let v = write(&e, t).await;
    assert_eq!(
        (v.status.as_str(), v.reason.as_deref()),
        ("inactive", Some("license_unavailable"))
    );
    let r = verdict::refresh_all(&e.state).await.unwrap();
    assert_eq!(r.unavailable, 1, "{r:?}");
    assert_eq!(stored(&e.owner, t).await.unwrap().0, "inactive");
}

#[tokio::test]
async fn suspended_tenant_skips_introspect() {
    let (e, up) = setup().await;
    let t = tenant(&e.state, &e.owner, Some(("lic-s", "tok-s"))).await;
    up.answer("tok-s", active(t, "lic-s"));
    sqlx::query("UPDATE tenants SET status = 'suspended', suspended_at = now() WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    let v = write(&e, t).await;
    assert_eq!(
        (v.status.as_str(), v.reason.as_deref()),
        ("inactive", Some("tenant_suspended"))
    );
    assert_eq!(
        up.hits(),
        0,
        "licences is not called for a suspended tenant"
    );
}

/// Two replicas on one database: a nudge sent through replica A reaches replica B's refresher (Postgres
/// LISTEN/NOTIFY), which rewrites the row long before its 1 h timer would.
#[tokio::test]
async fn a_nudge_from_another_replica_rewrites_the_row() {
    let up = licences().await;
    let url = common::fresh_database().await;
    let mut vars = common::base_env(&url);
    stub_env(&up, &mut vars);
    let cfg = Config::from_lookup(|k| vars.get(k).cloned()).expect("test config");
    let a = agent_api::state::build_for_tests(cfg.clone(), common::TEST_SECRET)
        .await
        .expect("replica a");
    let b = agent_api::state::build_for_tests(cfg, common::TEST_SECRET)
        .await
        .expect("replica b");
    let owner = PgPool::connect(&url).await.unwrap();
    let t = tenant(&a, &owner, Some(("lic-n", "tok-n"))).await;
    up.answer("tok-n", active(t, "lic-n"));
    let refresher = verdict::spawn_refresher_every(b.clone(), Duration::from_secs(3600));
    let mut status = None;
    for _ in 0..50 {
        status = stored(&owner, t).await.map(|s| s.0);
        if status.as_deref() == Some("active") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(status.as_deref(), Some("active"), "B's first pass");
    up.answer("tok-n", revoked());
    for _ in 0..50 {
        verdict::nudge(&a.db, Some(t)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        status = stored(&owner, t).await.map(|s| s.0);
        if status.as_deref() == Some("revoked") {
            break;
        }
    }
    refresher.abort();
    assert_eq!(
        status.as_deref(),
        Some("revoked"),
        "B rewrote the row on A's nudge"
    );
}

#[tokio::test]
async fn verdict_rls_isolates_tenants() {
    let (e, up) = setup().await;
    let a = tenant(&e.state, &e.owner, Some(("lic-a", "tok-a"))).await;
    let b = tenant(&e.state, &e.owner, Some(("lic-b", "tok-b"))).await;
    up.answer("tok-a", active(a, "lic-a"));
    up.answer("tok-b", active(b, "lic-b"));
    write(&e, a).await;
    write(&e, b).await;
    let mut tx = agent_api::tenancy::begin_scoped(&e.state.db, a)
        .await
        .unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM tenant_licence_verdict")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(n, 1, "tenant a sees its own row only");
    assert!(verdict::read(&mut tx, b).await.unwrap().is_none());
    assert_eq!(
        verdict::read(&mut tx, a).await.unwrap().unwrap().status,
        "active"
    );
    let cross = sqlx::query(
        "INSERT INTO tenant_licence_verdict (tenant_id, status, licence_id) VALUES ($1, 'active', 'x')",
    )
    .bind(b)
    .execute(&mut *tx)
    .await;
    assert!(cross.is_err(), "a cannot write b's verdict");
    tx.rollback().await.unwrap();
    // The platform overview sees both, through the definer function only.
    let mut tx = agent_api::platform_db::begin_platform(&e.state.db)
        .await
        .unwrap();
    let all = verdict::overview(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(all.len(), 2, "{all:?}");
}

#[tokio::test]
async fn introspect_sends_bearer() {
    let (e, up) = setup().await;
    let t = tenant(&e.state, &e.owner, Some(("lic-x", "tok-x"))).await;
    up.answer("tok-x", active(t, "lic-x"));
    write(&e, t).await;
    let auth = up.s.lock().unwrap().auth.clone();
    assert_eq!(auth, vec![Some("Bearer sa-token".to_string())]);
}

/// A platform `license.reissue_request` row `minutes` old for `t`'s tenant holder and old licence `old`.
async fn open_reissue(owner: &PgPool, t: Uuid, old: &str, minutes: i32) {
    sqlx::query(
        "INSERT INTO platform_audit (at, actor_id, actor_type, action, subject_tenant, subject_label, status, details) \
         VALUES (now() - make_interval(mins => $3), 'sa-1', 'user', 'license.reissue_request', $1, 'Demo', 'ok', \
                 jsonb_build_object('holder', 'tenant', 'old_licence_id', $2::text, 'residency', 'eu'))",
    )
    .bind(t)
    .bind(old)
    .bind(minutes)
    .execute(owner)
    .await
    .unwrap();
}

async fn closes(owner: &PgPool, t: Uuid) -> Vec<(String, Value)> {
    sqlx::query_as(
        "SELECT status, details FROM platform_audit WHERE subject_tenant = $1 AND action = 'license.reissue' ORDER BY id",
    )
    .bind(t)
    .fetch_all(owner)
    .await
    .unwrap()
}

/// R3: licences revoked the old licence but the re-issue crashed before its COMMIT. The sweep clears the dead id,
/// tombstones it (`license.revoke`, via reconcile), closes the request, and does all of that once.
#[tokio::test]
async fn reissue_crash_window_is_repaired_once() {
    let (e, up) = setup().await;
    let t = tenant(&e.state, &e.owner, Some(("lic-old", "tok-old"))).await;
    up.answer("tok-old", revoked());
    open_reissue(&e.owner, t, "lic-old", 20).await;
    let r = verdict::refresh_all(&e.state).await.unwrap();
    assert_eq!(r.repaired, 1, "{r:?}");
    let (id, token): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT licence_id, licence_token_encrypted FROM tenants WHERE id = $1")
            .bind(t)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(
        (id, token),
        (None, None),
        "the dead licence is no longer held"
    );
    let tomb: Vec<Value> = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE tenant_id = $1 AND action = 'license.revoke' AND status = 'success'",
    )
    .bind(t)
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(tomb.len(), 1);
    assert_eq!(tomb[0]["license_id"], "lic-old");
    assert_eq!(tomb[0]["via"], "reconcile");
    let c = closes(&e.owner, t).await;
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].0, "failed");
    assert_eq!(c[0].1["code"], "crash_repaired");
    assert_eq!(
        stored(&e.owner, t).await.unwrap().0,
        "none",
        "the verdict follows the repair; the B1 tick mints the replacement"
    );
    let r = verdict::refresh_all(&e.state).await.unwrap();
    assert_eq!(r.repaired, 0, "idempotent: {r:?}");
    assert_eq!(closes(&e.owner, t).await.len(), 1);
}

/// R3: a request whose licence licences never revoked (the re-issue died before its revoke) is closed without
/// touching the holder; a request younger than the grace period is left alone.
#[tokio::test]
async fn reissue_without_drift_is_closed_untouched() {
    let (e, up) = setup().await;
    let t = tenant(&e.state, &e.owner, Some(("lic-live", "tok-live"))).await;
    up.answer("tok-live", active(t, "lic-live"));
    open_reissue(&e.owner, t, "lic-live", 1).await;
    verdict::refresh_all(&e.state).await.unwrap();
    assert!(
        closes(&e.owner, t).await.is_empty(),
        "inside the grace period"
    );
    open_reissue(&e.owner, t, "lic-live", 20).await;
    let r = verdict::refresh_all(&e.state).await.unwrap();
    assert_eq!(r.repaired, 0);
    let c = closes(&e.owner, t).await;
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0].1["code"], "crash_no_drift");
    let id: Option<String> = sqlx::query_scalar("SELECT licence_id FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(id.as_deref(), Some("lic-live"));
    assert_eq!(stored(&e.owner, t).await.unwrap().0, "active");
}

/// R4: an active tenant waiting for its licence carries `agent_licence_pending_seconds{tenant}` > 0; a licensed one 0.
#[tokio::test]
async fn pending_licence_age_is_a_gauge() {
    let handle = agent_api::observability::metrics_handle();
    let (e, _up) = setup().await;
    let waiting = tenant(&e.state, &e.owner, None).await;
    sqlx::query("UPDATE tenants SET updated_at = now() - interval '2 hours' WHERE id = $1")
        .bind(waiting)
        .execute(&e.owner)
        .await
        .unwrap();
    let licensed = tenant(&e.state, &e.owner, Some(("lic-g", "tok-g"))).await;
    verdict::refresh_pending_gauge(&e.state.db).await.unwrap();
    let text = handle.render();
    let value = |t: Uuid| -> f64 {
        let prefix = format!("agent_licence_pending_seconds{{tenant=\"{t}\"}} ");
        text.lines()
            .find_map(|l| l.strip_prefix(prefix.as_str()))
            .unwrap_or_else(|| panic!("no series for {t}:\n{text}"))
            .trim()
            .parse()
            .unwrap()
    };
    assert!(value(waiting) >= 7_000.0, "{}", value(waiting));
    assert_eq!(value(licensed), 0.0);
}
