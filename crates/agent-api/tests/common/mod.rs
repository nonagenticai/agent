//! Integration test harness, after care-bus-agents@45ee128 `crates/carebus-api/tests/common/mod.rs`: ONE
//! Postgres 18 container per test binary (memory: rust-tests-one-container-per-binary), a fresh database per
//! test, migrated by `state::build` exactly as production does, served through the RLS runtime role `agent_app`.
//! No seed data exists in the migrations: tests create the tenants and users they need through the owner pool.
#![allow(dead_code)]
pub mod artifacts_fake;
pub mod classes;
pub mod conv;
pub mod design_contract;
pub mod fakes;
pub mod minter;
pub mod platform;
pub mod router_walk;
pub mod state;
pub mod strata_fake;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_api::config::Config;
use agent_api::state::AppState;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::sync::OnceCell;
use tower::ServiceExt;

pub struct TestEnv {
    pub state: AppState,
    pub app: Router,
    /// Superuser pool on the test database (RLS does not apply): fixtures and assertions only.
    pub owner: PgPool,
    // Drop last, after the router and pools. Reboots of the same database share this guard.
    _database: Option<std::sync::Arc<TestDatabase>>,
}

struct TestDatabase {
    admin: sqlx::postgres::PgConnectOptions,
    name: String,
}

type DatabaseGuards = std::sync::Mutex<HashMap<String, std::sync::Weak<TestDatabase>>>;
static DATABASE_GUARDS: std::sync::LazyLock<DatabaseGuards> =
    std::sync::LazyLock::new(Default::default);

type CleanupRequest = (sqlx::postgres::PgConnectOptions, String);
struct DatabaseCleanup {
    sender: std::sync::Mutex<Option<std::sync::mpsc::Sender<CleanupRequest>>>,
    worker: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

static DATABASE_CLEANUP: std::sync::LazyLock<DatabaseCleanup> = std::sync::LazyLock::new(|| {
    let (sender, receiver) = std::sync::mpsc::channel::<CleanupRequest>();
    let worker = std::thread::spawn(move || {
        use sqlx::Connection;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("cleanup runtime");
        for (admin, name) in receiver {
            let result = rt.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(15), async move {
                    let mut conn = sqlx::PgConnection::connect_with(&admin).await?;
                    sqlx::query(sqlx::AssertSqlSafe(format!(
                        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
                    )))
                    .execute(&mut conn)
                    .await?;
                    Ok::<(), sqlx::Error>(())
                })
                .await
            });
            if !matches!(result, Ok(Ok(()))) {
                eprintln!("test database cleanup failed (no connection details printed)");
            }
        }
    });
    // SAFETY: this callback uses only this process's cleanup queue, and cannot unwind across the C boundary.
    unsafe { libc::atexit(finish_database_cleanup) };
    DatabaseCleanup {
        sender: std::sync::Mutex::new(Some(sender)),
        worker: std::sync::Mutex::new(Some(worker)),
    }
});

extern "C" fn finish_database_cleanup() {
    let queue = &*DATABASE_CLEANUP;
    queue
        .sender
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(worker) = queue
        .worker
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        let _ = worker.join();
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        // Never join here: the test runtime must keep polling to finish or cancel its pending connection
        // handshakes. Blocking it while DROP DATABASE waits for those backends deadlocks PostgreSQL's barrier.
        if let Some(sender) = DATABASE_CLEANUP
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ = sender.send((self.admin.clone(), self.name.clone()));
        }
    }
}

impl TestEnv {
    /// A reboot of an existing database shares its owner's guard; fresh_database() callers retain their database
    /// between reboots. Only env_with() owns a database's lifetime.
    pub fn new(state: AppState, app: Router, owner: PgPool) -> Self {
        let options = owner.connect_options();
        let name = options.get_database().expect("test database name");
        let database = DATABASE_GUARDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .and_then(std::sync::Weak::upgrade);
        Self {
            state,
            app,
            owner,
            _database: database,
        }
    }

    fn owned(state: AppState, app: Router, owner: PgPool) -> Self {
        let options = owner.connect_options();
        let name = options
            .get_database()
            .expect("test database name")
            .to_owned();
        assert!(
            name.starts_with(&format!("agent_t_{}_", std::process::id()))
                && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        );
        let database = std::sync::Arc::new(TestDatabase {
            admin: options.as_ref().clone().database("postgres"),
            name: name.clone(),
        });
        let mut guards = DATABASE_GUARDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guards.retain(|_, guard| guard.strong_count() > 0);
        guards.insert(name, std::sync::Arc::downgrade(&database));
        Self {
            state,
            app,
            owner,
            _database: Some(database),
        }
    }
}

static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Swap the database name (path segment) in a postgres URL, keeping any query string.
fn with_db_name(url: &str, name: &str) -> String {
    let (head, query) = match url.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (url, None),
    };
    let scheme_end = head.find("://").map(|i| i + 3).unwrap_or(0);
    let base = match head[scheme_end..].find('/') {
        Some(i) => &head[..scheme_end + i],
        None => head,
    };
    match query {
        Some(q) => format!("{base}/{name}?{q}"),
        None => format!("{base}/{name}"),
    }
}

/// One server per test binary: `TEST_DATABASE_URL` when set, otherwise a single throwaway Postgres 18 from the
/// registry (`TEST_PG_REGISTRY`, default `library`). The container
/// lives in a static, so it is removed by an `atexit` hook when the test process exits.
struct Server {
    base: String,
    _container: Option<ContainerAsync<Postgres>>,
}

static SERVER: OnceCell<Server> = OnceCell::const_new();
static CONTAINER_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Statics are never dropped and the harness runs no reaper, so remove the shared container when the process
/// exits. Scoped to this process's own container id, never a prefix match.
extern "C" fn remove_container() {
    if let Some(id) = CONTAINER_ID.get() {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", "-v", id])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

async fn start_container() -> (String, ContainerAsync<Postgres>) {
    let reg = match std::env::var("TEST_PG_REGISTRY") {
        Ok(r) if !r.is_empty() => r,
        _ => "docker.io/library".to_string(),
    };
    let image = format!("{}/postgres", reg.trim_end_matches('/'));
    let mut last = String::new();
    for attempt in 1..=3 {
        match Postgres::default()
            .with_name(&image)
            .with_tag("18-alpine")
            // Parallel migrations use PostgreSQL dynamic shared memory; Docker's default 64 MiB is too small
            // for the full B4 suite. This is a limit, allocated only as the server needs it.
            .with_shm_size(1024 * 1024 * 1024)
            .with_cmd(["postgres", "-c", "max_connections=300", "-c", "fsync=off"])
            .start()
            .await
        {
            Ok(pg) => {
                // Docker can report the container started before the host port mapping is published: poll.
                let mut port = None;
                for _ in 0..40 {
                    match pg.get_host_port_ipv4(5432).await {
                        Ok(p) => {
                            port = Some(p);
                            break;
                        }
                        Err(e) => last = format!("{e:?}"),
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                if let Some(port) = port {
                    return (
                        format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres"),
                        pg,
                    );
                }
                eprintln!("postgres container port never published (attempt {attempt}/3): {last}");
            }
            Err(e) => {
                last = format!("{e:?}");
                eprintln!("postgres container start failed (attempt {attempt}/3): {last}");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
    panic!("could not start postgres 18 (is Docker running?): {last}");
}

async fn server() -> &'static Server {
    SERVER
        .get_or_init(|| async {
            if let Ok(base) = std::env::var("TEST_DATABASE_URL")
                && !base.is_empty()
            {
                ensure_app_role(&base).await;
                sweep_orphaned_databases(&base).await;
                return Server {
                    base,
                    _container: None,
                };
            }
            let (base, c) = start_container().await;
            if CONTAINER_ID.set(c.id().to_string()).is_ok() {
                // SAFETY: registering a plain `extern "C" fn()` with atexit.
                unsafe { libc::atexit(remove_container) };
            }
            ensure_app_role(&base).await;
            Server {
                base,
                _container: Some(c),
            }
        })
        .await
}

/// On a persistent server (`TEST_DATABASE_URL`), drop the `agent_t_<pid>_…` databases whose test process is gone. A
/// database held by a static environment is never dropped, and a panicking or killed binary never drains its
/// cleanup queue, so without this every workspace run leaks databases until the server's disk is full. A database
/// whose pid is alive (this process, or another run's) is left alone; a recycled pid only skips one.
async fn sweep_orphaned_databases(base: &str) {
    let Ok(admin) = PgPool::connect(base).await else {
        return;
    };
    let names: Vec<String> =
        sqlx::query_scalar("SELECT datname FROM pg_database WHERE datname LIKE 'agent\\_t\\_%'")
            .fetch_all(&admin)
            .await
            .unwrap_or_default();
    for name in names {
        let Some(pid) = name
            .strip_prefix("agent_t_")
            .and_then(|rest| rest.split('_').next())
            .and_then(|pid| pid.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        // SAFETY: signal 0 only checks that the process exists; nothing is delivered.
        let gone = unsafe { libc::kill(pid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone {
            // another binary may be sweeping the same name: a failure here is not this test's
            let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
            )))
            .execute(&admin)
            .await;
        }
    }
    admin.close().await;
}

/// The runtime role's test-only password. Production creates `agent_app` LOGIN through the gitops provisioning
/// script with its own secret; migration 0001 creates it NOLOGIN when it is missing.
pub const APP_ROLE_PASSWORD: &str = "agent_app";

/// Once per server, before any database is migrated: `agent_app` exists, may log in, and has the test password.
async fn ensure_app_role(base: &str) {
    let admin = PgPool::connect(base)
        .await
        .expect("connect to the test postgres server");
    sqlx::query(
        "DO $$ BEGIN
             IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'agent_app') THEN
                 CREATE ROLE agent_app LOGIN PASSWORD 'agent_app' NOSUPERUSER NOBYPASSRLS;
             ELSE
                 ALTER ROLE agent_app LOGIN PASSWORD 'agent_app' NOSUPERUSER NOBYPASSRLS;
             END IF;
         EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;
         END $$",
    )
    .execute(&admin)
    .await
    .expect("create the agent_app test role");
    // The platform role (migration 0031 verifies, never creates it; production's DBA step creates it).
    sqlx::query(
        "DO $$ BEGIN
             IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'agent_platform') THEN
                 CREATE ROLE agent_platform NOLOGIN NOSUPERUSER NOBYPASSRLS;
             END IF;
         EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;
         END $$",
    )
    .execute(&admin)
    .await
    .expect("create the agent_platform test role");
    grant_platform_membership(&admin).await;
    // The runtime processes' role (B4 0065 creates it NOLOGIN when missing; production's DBA step creates it LOGIN
    // with its own secret). A member of nothing.
    sqlx::query(
        "DO $$ BEGIN
             IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'agent_runtime') THEN
                 CREATE ROLE agent_runtime LOGIN PASSWORD 'agent_runtime' NOSUPERUSER NOBYPASSRLS;
             ELSE
                 ALTER ROLE agent_runtime LOGIN PASSWORD 'agent_runtime' NOSUPERUSER NOBYPASSRLS;
             END IF;
         EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL;
         END $$",
    )
    .execute(&admin)
    .await
    .expect("create the agent_runtime test role");
    admin.close().await;
}

/// The runtime role's test-only password (see [`ensure_app_role`]).
pub const RUNTIME_ROLE_PASSWORD: &str = "agent_runtime";

/// A pool on `owner`'s database as `agent_runtime` (LOGIN, NOSUPERUSER, NOBYPASSRLS): the runtime processes' role.
pub async fn runtime_pool(owner: &PgPool) -> PgPool {
    let opts = owner
        .connect_options()
        .as_ref()
        .clone()
        .username("agent_runtime")
        .password(RUNTIME_ROLE_PASSWORD);
    PgPool::connect_with(opts)
        .await
        .expect("connect as agent_runtime")
}

/// `GRANT agent_platform TO agent_app WITH INHERIT FALSE, SET TRUE` (re-granting resets the options).
pub async fn grant_platform_membership(admin: &PgPool) {
    sqlx::query("GRANT agent_platform TO agent_app WITH INHERIT FALSE, SET TRUE")
        .execute(admin)
        .await
        .expect("grant agent_platform to agent_app");
}

/// A superuser pool on the shared test server (role-level fixtures only; roles are server-wide).
pub async fn server_admin() -> PgPool {
    PgPool::connect(&server().await.base)
        .await
        .expect("connect to the test postgres server")
}

/// A fresh, empty, unmigrated database on the shared server; returns its owner URL.
pub async fn fresh_database() -> String {
    database().await
}

/// Boot the API on an existing database `url` (migrated to head by `state::build`, as in production): for
/// upgrade tests that seed an older schema first.
pub async fn env_for(url: String) -> TestEnv {
    let vars = base_env(&url);
    let cfg = Config::from_lookup(|k| vars.get(k).cloned()).expect("test config");
    let state = agent_api::state::build_for_tests(cfg, TEST_SECRET)
        .await
        .expect("state");
    let owner = PgPool::connect(&url).await.expect("owner pool");
    let app = agent_api::router(state.clone());
    let e = TestEnv::new(state, app, owner);
    platform::install(&e).await;
    e
}

/// The SQLSTATE of a database error.
pub fn sqlstate(e: &sqlx::Error) -> Option<String> {
    e.as_database_error()
        .and_then(|d| d.code())
        .map(|c| c.into_owned())
}

/// [`env_with`] that returns the boot error instead of panicking.
pub async fn try_env() -> anyhow::Result<TestEnv> {
    let url = database().await;
    let vars = base_env(&url);
    let cfg = Config::from_lookup(|k| vars.get(k).cloned())?;
    let state = agent_api::state::build_for_tests(cfg, TEST_SECRET).await?;
    let owner = PgPool::connect(&url).await?;
    let app = agent_api::router(state.clone());
    let e = TestEnv::new(state, app, owner);
    platform::install(&e).await;
    Ok(e)
}

/// The same URL with another user and password.
fn with_user(url: &str, user: &str, password: &str) -> String {
    let scheme_end = url.find("://").map(|i| i + 3).unwrap_or(0);
    let rest = &url[scheme_end..];
    let host_start = match (rest.find('@'), rest.find('/')) {
        (Some(at), Some(slash)) if at < slash => at + 1,
        (Some(at), None) => at + 1,
        _ => 0,
    };
    format!(
        "{}{user}:{password}@{}",
        &url[..scheme_end],
        &rest[host_start..]
    )
}

/// A fresh, empty database on the shared server; returns its URL.
async fn database() -> String {
    let srv = server().await;
    let n = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("agent_t_{}_{nanos}_{n}", std::process::id());
    let admin = PgPool::connect(&srv.base)
        .await
        .expect("connect to the test postgres server");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .expect("create test database");
    admin.close().await;
    with_db_name(&srv.base, &name)
}

pub const TEST_SECRET: &[u8] = b"agent-api-test-secret";
pub const TEST_ISSUER: &str = "https://issuer.test/realms/portal";
pub const TEST_AUDIENCE: &str = "agent";

/// Where the identity clients point when a test does not start the fake upstream: the discard port, so a call
/// that should not happen fails fast (connection refused) instead of hanging.
pub const NO_UPSTREAM: &str = "http://127.0.0.1:9";

/// The licence token key of every test (bytes 0..32, built at runtime: no key literal is committed).
pub fn test_token_key() -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode((0u8..32).collect::<Vec<u8>>())
}

/// The identity variables (always required, B1 T4) pointed at `upstream` (fakes::upstream's URL, or
/// [`NO_UPSTREAM`]).
pub fn identity_vars(upstream: &str) -> Vec<(&'static str, String)> {
    vec![
        ("AGENT_PUBLIC_URL", "https://agent.test".to_string()),
        ("LICENSE_TOKEN_ENC_KEY", test_token_key()),
        ("KEYCLOAK_ADMIN_TOKEN_URL", format!("{upstream}/token")),
        ("KEYCLOAK_ADMIN_CLIENT_ID", "agent-admin".to_string()),
        ("KEYCLOAK_ADMIN_CLIENT_SECRET", "kc-secret".to_string()),
        (
            "KEYCLOAK_ADMIN_API_URL",
            format!("{upstream}/admin/realms/portal"),
        ),
        ("AGENT_KC_CLIENT_UUID", "cid-agent".to_string()),
        ("LICENSES_ADMIN_TOKEN_URL", format!("{upstream}/token")),
        (
            "LICENSES_ADMIN_CLIENT_ID",
            "agent-licenses-admin".to_string(),
        ),
        ("LICENSES_ADMIN_CLIENT_SECRET", "lic-secret".to_string()),
        ("LICENSES_ADMIN_URL", upstream.to_string()),
    ]
}

/// The environment the API is configured from (parsed by `Config::from_lookup`, as in production).
pub fn base_env(database_url: &str) -> HashMap<String, String> {
    let mut m = HashMap::from([
        (
            "DATABASE_URL".to_string(),
            with_user(database_url, "agent_app", APP_ROLE_PASSWORD),
        ),
        ("MIGRATE_DATABASE_URL".to_string(), database_url.to_string()),
        ("KEYCLOAK_ISSUER".to_string(), TEST_ISSUER.to_string()),
        ("KEYCLOAK_AUDIENCE".to_string(), TEST_AUDIENCE.to_string()),
    ]);
    for (k, v) in identity_vars(NO_UPSTREAM) {
        m.insert(k.to_string(), v);
    }
    // the process-wide fake bundle minter: a tenant without a registered token gets 404 no_app_licence (fail closed)
    minter::env(minter::shared(), &mut m);
    m
}

/// A fresh database; signup dark.
pub async fn env() -> TestEnv {
    env_with(|_| {}).await
}

/// A fresh database, the environment adjusted by `tweak` before the config is parsed.
pub async fn env_with(tweak: impl FnOnce(&mut HashMap<String, String>)) -> TestEnv {
    let e = env_bare_with(tweak).await;
    platform::install(&e).await;
    e
}

/// [`env`] without the platform offering: an empty platform (no layers, tags or vocabulary).
pub async fn env_bare() -> TestEnv {
    env_bare_with(|_| {}).await
}

/// [`env_with`] without the platform offering.
pub async fn env_bare_with(tweak: impl FnOnce(&mut HashMap<String, String>)) -> TestEnv {
    let url = database().await;
    let mut vars = base_env(&url);
    tweak(&mut vars);
    let cfg = Config::from_lookup(|k| vars.get(k).cloned()).expect("test config");
    let state = agent_api::state::build_for_tests(cfg, TEST_SECRET)
        .await
        .expect("state");
    let owner = PgPool::connect(&url).await.expect("owner pool");
    let app = agent_api::router(state.clone());
    TestEnv::owned(state, app, owner)
}

/// Inserts a tenant through the owner pool (RLS does not apply to it).
pub async fn insert_tenant(db: &PgPool, id: uuid::Uuid, name: &str) {
    sqlx::query(
        "INSERT INTO tenants (id, name, residency, updated_by) VALUES ($1, $2, 'eu', 'test') \
         ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name",
    )
    .bind(id)
    .bind(name)
    .execute(db)
    .await
    .unwrap();
}

/// Upserts a fresh `active` licence verdict for `tenant` through the owner pool (B7 gate 2, crate::licensing::gate):
/// the owner ruled fail closed, so a tenant that is not platform-owned gets no agent turn without a verdict row that
/// is `active` and at most `verdict::MAX_AGE_SECONDS` old. Fixtures whose tenants run turns call this; a test that
/// wants the gate to refuse deletes or overwrites the row.
/// `tenant` holds all three of its app licences (inference-router, strata-rag, strata-ingest; 0216), so the
/// app-licence backfill skips it.
pub async fn app_licences_held(db: &PgPool, tenant: uuid::Uuid) {
    sqlx::query(
        "INSERT INTO tenant_app_licence (tenant_id, app_key, licence_id, token_id, provisioned_at) \
         SELECT $1, k, 'lic-' || k, 'tok-' || k, now() \
           FROM unnest(ARRAY['inference-router', 'strata-rag', 'strata-ingest']) AS k \
         ON CONFLICT (tenant_id, app_key) DO UPDATE SET provisioned_at = now()",
    )
    .bind(tenant)
    .execute(db)
    .await
    .unwrap();
}

pub async fn licence_active(db: &PgPool, tenant: uuid::Uuid) {
    sqlx::query(
        "INSERT INTO tenant_licence_verdict (tenant_id, status, reason, licence_id, checked_at) \
         VALUES ($1, 'active', NULL, $2, now()) \
         ON CONFLICT (tenant_id) DO UPDATE \
         SET status = 'active', reason = NULL, licence_id = EXCLUDED.licence_id, checked_at = now()",
    )
    .bind(tenant)
    .bind(format!("lic-test-{tenant}"))
    .execute(db)
    .await
    .unwrap();
}

/// The §R.1 role keys in order (D88): what a first member and a super_admin in a tenant hold.
pub const ALL_ROLES: [&str; 8] = [
    "tenant_admin",
    "job_designer",
    "release_manager",
    "knowledge_manager",
    "integrations_manager",
    "support_manager",
    "support_agent",
    "operator",
];

/// The role keys `role` names: `tenant_admin` = every role (a pre-D88 admin), `tenant_user` or `""` = none (an
/// observer), else a comma-separated list of keys.
pub fn role_keys(role: &str) -> Vec<String> {
    match role {
        "tenant_admin" => ALL_ROLES.iter().map(|r| r.to_string()).collect(),
        "tenant_user" | "" => Vec::new(),
        list => list.split(',').map(|r| r.trim().to_string()).collect(),
    }
}

/// Inserts a member through the owner pool; `role` as [`role_keys`].
pub async fn insert_user(db: &PgPool, sub: &str, tenant: uuid::Uuid, role: &str) {
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, roles, display_name, updated_by) VALUES ($1, $2, $3, $1, 'test')",
    )
    .bind(sub)
    .bind(tenant)
    .bind(role_keys(role))
    .execute(db)
    .await
    .unwrap();
}

pub async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(req)
        .await
        .expect("router is infallible");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, v)
}

pub fn get(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut b = Request::get(uri);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

pub fn json(method: &str, uri: &str, token: Option<&str>, body: Value) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

pub fn sign(claims: &Value) -> String {
    use jsonwebtoken::{EncodingKey, Header, encode};
    encode(
        &Header::default(),
        claims,
        &EncodingKey::from_secret(TEST_SECRET).expect("TEST_SECRET is a non-empty literal"),
    )
    .unwrap()
}

pub fn in_five_minutes() -> i64 {
    (chrono::Utc::now() + chrono::Duration::minutes(5)).timestamp()
}

/// A person token from the web client: `aud` = `agent`, `client_roles` on that client.
pub fn person(sub: &str, client_roles: &[&str]) -> String {
    sign(&json!({
        "sub": sub, "preferred_username": sub, "exp": in_five_minutes(),
        "iss": TEST_ISSUER, "aud": TEST_AUDIENCE, "azp": TEST_AUDIENCE,
        "resource_access": {TEST_AUDIENCE: {"roles": client_roles}}
    }))
}

/// The owner / any super_admin.
pub fn super_admin() -> String {
    person("u-super", &["super_admin"])
}

/// A client-credentials service-account token: `azp`, `aud`, realm roles.
pub fn service(azp: &str, aud: &[&str], realm_roles: &[&str]) -> String {
    sign(&json!({
        "sub": format!("service-account-{azp}"), "preferred_username": format!("service-account-{azp}"),
        "exp": in_five_minutes(), "iss": TEST_ISSUER, "aud": aud, "azp": azp,
        "realm_access": {"roles": realm_roles}
    }))
}
