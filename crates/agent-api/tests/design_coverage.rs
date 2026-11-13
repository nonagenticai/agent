//! B2 T15: coverage backfill for late-registered ledger kinds (spec B2 §2.5 "late registration"). Real Postgres 18
//! (one container for this binary, a fresh database per test), through the RLS runtime role `agent_app`.
//!
//! A release cut (and a baseline taken) while only `scratch` was registered covers `scratch` alone. A later boot
//! registers `scratch_new`; `design::coverage::backfill` must copy `scratch_new`'s CURRENT working rows into the
//! release snapshot and the baseline, append the kind to `snapshot_kinds` / `jobs.baseline_kinds`, never touch the
//! already covered kinds, do nothing on a second run, and serialise concurrent replicas on its advisory lock.
//!
//! Test-only targets (`Registry::register_scratch`, kinds `scratch*`) over scratch tables created as the database
//! owner with RLS + grants exactly like a real tenant table. Releases are inserted by direct SQL (§3 shape), as
//! `tests/design_engine.rs` does.
use crate::common;

use std::sync::Arc;
use std::time::Duration;

use agent_api::design::coverage::{self, BackfillOptions, Backfilled, COVERAGE_LOCK};
use agent_api::design::{
    self, DesignOf, DesignTarget, DraftRef, JobRef, Place, Registry, Scope, Snap,
};
use agent_api::error::{ApiError, ApiResult};
use agent_api::tenancy::begin_scoped;
use common::{TestEnv, insert_tenant};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, PgConnection, PgPool};
use uuid::Uuid;

const R1: &str = "cfg-2026-10-20.1";
const R2: &str = "cfg-2026-10-20.2";
const R3: &str = "cfg-2026-10-20.3";

const SCRATCH_DDL: &str = r#"
CREATE TABLE scratch_item (
    tenant_id uuid NOT NULL DEFAULT app_current_tenant(),
    job_id    uuid NOT NULL,
    name      text NOT NULL,
    val       jsonb NOT NULL,
    PRIMARY KEY (tenant_id, job_id, name),
    FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);
CREATE TABLE scratch_new_item (
    tenant_id uuid NOT NULL DEFAULT app_current_tenant(),
    job_id    uuid NOT NULL,
    name      text NOT NULL,
    val       jsonb NOT NULL,
    PRIMARY KEY (tenant_id, job_id, name),
    FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);
ALTER TABLE scratch_item ENABLE ROW LEVEL SECURITY;
ALTER TABLE scratch_new_item ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON scratch_item FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
CREATE POLICY tenant_isolation ON scratch_new_item FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE, DELETE ON scratch_item, scratch_new_item TO agent_app;
"#;

// ---- scratch targets ------------------------------------------------------------------------------------------------

fn name(r: &DraftRef) -> String {
    r.get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn name_ref(n: &str) -> DraftRef {
    let mut m = DraftRef::new();
    m.insert("name".to_string(), Value::String(n.to_string()));
    m
}

fn job_of(at: Place<'_>) -> ApiResult<Uuid> {
    at.job
        .map(|j| j.id)
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("a job place is needed")))
}

/// A job-scoped kind over one scratch table; key `<prefix>:<name>`.
struct JobRows {
    kind: &'static str,
    prefix: &'static str,
    table: &'static str,
}

impl DesignTarget for JobRows {
    fn kind(&self) -> &'static str {
        self.kind
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("{}:{}", self.prefix, name(r))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let v: Option<Value> = sqlx::query_scalar(AssertSqlSafe(format!(
                "SELECT val FROM {} WHERE job_id = $1 AND name = $2",
                self.table
            )))
            .bind(job)
            .bind(name(r))
            .fetch_optional(&mut *c)
            .await?;
            Ok(v)
        })
    }
    fn set<'a>(
        &'a self,
        _c: &'a mut PgConnection,
        _at: Place<'a>,
        _r: &'a DraftRef,
        _v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let names: Vec<String> = sqlx::query_scalar(AssertSqlSafe(format!(
                "SELECT name FROM {} WHERE job_id = $1 ORDER BY name",
                self.table
            )))
            .bind(job)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(names.into_iter().map(|n| name_ref(&n)).collect()))
        })
    }
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        v.as_ref().map(Value::to_string)
    }
}

const SCRATCH: JobRows = JobRows {
    kind: "scratch",
    prefix: "scratch",
    table: "scratch_item",
};
const SCRATCH_NEW: JobRows = JobRows {
    kind: "scratch_new",
    prefix: "scratch-new",
    table: "scratch_new_item",
};

/// The registry of the old boot: `scratch` only.
fn old_registry() -> Registry {
    let mut r = Registry::default();
    r.register_scratch(Arc::new(SCRATCH));
    r
}

/// The registry after `scratch_new`'s plan landed.
fn new_registry() -> Registry {
    let mut r = old_registry();
    r.register_scratch(Arc::new(SCRATCH_NEW));
    r
}

// ---- fixture --------------------------------------------------------------------------------------------------------

struct Fx {
    e: TestEnv,
    tenant: Uuid,
    /// Released (R1, current) and with an older approved cut R2 lacking `scratch_new` too.
    released: Uuid,
    /// Never released: a baseline taken under the old registry.
    baselined: Uuid,
    /// Neither released nor baselined: nothing to backfill.
    untouched: Uuid,
}

async fn new_job(db: &PgPool, tenant: Uuid, key: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, $2, $2, '[]', 'test') RETURNING id",
    )
    .bind(tenant)
    .bind(key)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn seed(db: &PgPool, table: &str, tenant: Uuid, job: Uuid, n: &str, val: Value) {
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {table} (tenant_id, job_id, name, val) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (tenant_id, job_id, name) DO UPDATE SET val = EXCLUDED.val"
    )))
    .bind(tenant)
    .bind(job)
    .bind(n)
    .bind(val)
    .execute(db)
    .await
    .unwrap();
}

/// A release row (owner, §3 shape) storing `kinds` (`[]`: no snapshot stored, unappliable).
async fn insert_release_kinds(
    db: &PgPool,
    tenant: Uuid,
    job: Uuid,
    release: &str,
    state: &str,
    kinds: &[&str],
) {
    let sha = |c: char| format!("sha256:{}", c.to_string().repeat(64));
    sqlx::query(
        r#"INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins,
               provenance, requires, causes, state, approved_by, approved_at, released_by, released_at,
               snapshot_kinds)
           VALUES ($1, $2, $3, $4, $5, '{"sections": []}', '{}', '[]', '{}', '{}', '[]', $6, 'u-rm', now(),
               CASE WHEN $6 = 'approved' THEN NULL ELSE 'u-rm' END,
               CASE WHEN $6 = 'approved' THEN NULL ELSE now() END,
               $7::text[])"#,
    )
    .bind(tenant)
    .bind(job)
    .bind(release)
    .bind(sha('a'))
    .bind(sha('b'))
    .bind(state)
    .bind(kinds.iter().map(|k| k.to_string()).collect::<Vec<String>>())
    .execute(db)
    .await
    .unwrap();
}

/// A release row covering `scratch` only.
async fn insert_release(db: &PgPool, tenant: Uuid, job: Uuid, release: &str, state: &str) {
    insert_release_kinds(db, tenant, job, release, state, &["scratch"]).await;
}

async fn snapshot_row(db: &PgPool, tenant: Uuid, job: Uuid, release: &str, n: &str, v: Value) {
    sqlx::query(
        "INSERT INTO release_snapshot (tenant_id, job_id, release_id, key, kind, ref, snap) \
         VALUES ($1, $2, $3, $4, 'scratch', $5, $6)",
    )
    .bind(tenant)
    .bind(job)
    .bind(release)
    .bind(format!("scratch:{n}"))
    .bind(json!({ "name": n }))
    .bind(json!({ "v": v }))
    .execute(db)
    .await
    .unwrap();
}

/// A baseline taken under the old registry: `scratch` rows only, `baseline_kinds = {scratch}`.
async fn old_baseline(db: &PgPool, tenant: Uuid, job: Uuid, n: &str, v: Value) {
    sqlx::query(
        "INSERT INTO release_baseline (tenant_id, job_id, key, kind, ref, snap) \
         VALUES ($1, $2, $3, 'scratch', $4, $5)",
    )
    .bind(tenant)
    .bind(job)
    .bind(format!("scratch:{n}"))
    .bind(json!({ "name": n }))
    .bind(json!({ "v": v }))
    .execute(db)
    .await
    .unwrap();
    sqlx::query("UPDATE jobs SET baseline_kinds = '{scratch}' WHERE id = $1")
        .bind(job)
        .execute(db)
        .await
        .unwrap();
}

/// One tenant with a released job, a baselined job and an untouched job, cut/taken under [`old_registry`]; the
/// working rows have since moved on (`scratch:a` = 2, released as 1) and `scratch_new` has rows nobody stored.
async fn tenant_fixture(db: &PgPool, name: &str) -> (Uuid, Uuid, Uuid, Uuid) {
    let tenant = Uuid::new_v4();
    insert_tenant(db, tenant, name).await;
    let released = new_job(db, tenant, "released").await;
    let baselined = new_job(db, tenant, "baselined").await;
    let untouched = new_job(db, tenant, "untouched").await;

    insert_release(db, tenant, released, R1, "current").await;
    snapshot_row(db, tenant, released, R1, "a", json!(1)).await;
    insert_release(db, tenant, released, R2, "approved").await;
    snapshot_row(db, tenant, released, R2, "a", json!(1)).await;
    sqlx::query("UPDATE jobs SET current_release_id = $2 WHERE id = $1")
        .bind(released)
        .bind(R1)
        .execute(db)
        .await
        .unwrap();
    old_baseline(db, tenant, baselined, "a", json!(1)).await;

    for job in [released, baselined, untouched] {
        seed(db, "scratch_item", tenant, job, "a", json!(2)).await;
        seed(
            db,
            "scratch_new_item",
            tenant,
            job,
            "x",
            json!({ "n": name }),
        )
        .await;
        seed(db, "scratch_new_item", tenant, job, "y", Value::Null).await;
    }
    (tenant, released, baselined, untouched)
}

async fn setup() -> Fx {
    let e = common::env().await;
    sqlx::raw_sql(SCRATCH_DDL).execute(&e.owner).await.unwrap();
    let (tenant, released, baselined, untouched) = tenant_fixture(&e.owner, "Alpha").await;
    Fx {
        e,
        tenant,
        released,
        baselined,
        untouched,
    }
}

async fn release_kinds(db: &PgPool, job: Uuid, release: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT snapshot_kinds FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(job)
    .bind(release)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn baseline_kinds(db: &PgPool, job: Uuid) -> Option<Vec<String>> {
    sqlx::query_scalar("SELECT baseline_kinds FROM jobs WHERE id = $1")
        .bind(job)
        .fetch_one(db)
        .await
        .unwrap()
}

type Stored = Vec<(String, String, Option<Value>)>;

/// R1 rows, R2 rows, baseline rows, R1 kinds, R2 kinds, baseline kinds.
type State = (
    Stored,
    Stored,
    Stored,
    Vec<String>,
    Vec<String>,
    Option<Vec<String>>,
);

/// `(key, kind, snap)` of a release snapshot, by key.
async fn snapshot_rows(db: &PgPool, job: Uuid, release: &str) -> Stored {
    sqlx::query_as(
        "SELECT key, kind, snap FROM release_snapshot WHERE job_id = $1 AND release_id = $2 ORDER BY key COLLATE \"C\"",
    )
    .bind(job)
    .bind(release)
    .fetch_all(db)
    .await
    .unwrap()
}

/// `(key, kind, snap)` of a job's baseline, by key.
async fn baseline_rows(db: &PgPool, job: Uuid) -> Stored {
    sqlx::query_as(
        "SELECT key, kind, snap FROM release_baseline WHERE job_id = $1 ORDER BY key COLLATE \"C\"",
    )
    .bind(job)
    .fetch_all(db)
    .await
    .unwrap()
}

/// Everything the backfill could have written, for before/after comparisons.
async fn stored_state(f: &Fx) -> State {
    let db = &f.e.owner;
    (
        snapshot_rows(db, f.released, R1).await,
        snapshot_rows(db, f.released, R2).await,
        baseline_rows(db, f.baselined).await,
        release_kinds(db, f.released, R1).await,
        release_kinds(db, f.released, R2).await,
        baseline_kinds(db, f.baselined).await,
    )
}

/// The design `of` at the `scratch_new` places, read as `agent_app` through the read API.
async fn new_kind_design(
    f: &Fx,
    reg: &Registry,
    job: Uuid,
    key: &str,
    of: DesignOf,
) -> Vec<(String, Snap)> {
    let mut tx = begin_scoped(&f.e.state.db, f.tenant).await.unwrap();
    let j = JobRef {
        id: job,
        key: key.to_string(),
    };
    let rows = design::take_snapshot(&mut tx, reg, &j, &of).await.unwrap();
    tx.commit().await.unwrap();
    let mut out: Vec<(String, Snap)> = rows
        .into_iter()
        .filter(|r| r.kind == "scratch_new")
        .map(|r| (r.key, r.snap))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn expected_new_rows(tenant_name: &str) -> Stored {
    vec![
        (
            "scratch-new:x".to_string(),
            "scratch_new".to_string(),
            Some(json!({ "v": { "n": tenant_name } })),
        ),
        (
            "scratch-new:y".to_string(),
            "scratch_new".to_string(),
            Some(json!({ "v": null })),
        ),
    ]
}

fn with_new(mut old: Stored, tenant_name: &str) -> Stored {
    old.extend(expected_new_rows(tenant_name));
    old.sort_by(|a, b| a.0.cmp(&b.0));
    old
}

fn scratch_a(v: i64) -> (String, String, Option<Value>) {
    (
        "scratch:a".to_string(),
        "scratch".to_string(),
        Some(json!({ "v": v })),
    )
}

// ---- tests ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn new_kind_backfills_existing_releases_and_baselines() {
    let f = setup().await;
    // A second tenant with the same shape: its rows come from ITS working rows (RLS-scoped, per tenant).
    let (_, beta_released, beta_baselined, _) = tenant_fixture(&f.e.owner, "Beta").await;

    // The boot that cut the releases: nothing to do.
    let old = old_registry();
    assert_eq!(
        coverage::backfill(&f.e.state.db, &old).await.unwrap(),
        Backfilled::default(),
        "the old registry's kinds are all covered"
    );

    // Before the backfill, the release does not cover `scratch_new`: its reads fall through to the working rows.
    let reg = new_registry();
    let draft_now = new_kind_design(&f, &reg, f.released, "released", DesignOf::Draft).await;
    assert_eq!(
        new_kind_design(&f, &reg, f.released, "released", DesignOf::Live).await,
        draft_now
    );

    // "Restart" with `scratch_new` registered.
    let done = coverage::backfill(&f.e.state.db, &reg).await.unwrap();
    assert_eq!(
        done,
        Backfilled {
            releases: 4,
            baselines: 2,
            rows: 12,
            failed_tenants: 0,
        },
        "2 tenants × (R1 + R2 + one baseline) × 2 places"
    );

    // Releases: the kind appended, its places = the working rows at backfill time, `scratch` untouched (released
    // as 1 although the working row is now 2).
    for release in [R1, R2] {
        assert_eq!(
            release_kinds(&f.e.owner, f.released, release).await,
            vec!["scratch", "scratch_new"]
        );
        assert_eq!(
            snapshot_rows(&f.e.owner, f.released, release).await,
            with_new(vec![scratch_a(1)], "Alpha"),
            "{release}"
        );
    }
    // Baseline: the same.
    assert_eq!(
        baseline_kinds(&f.e.owner, f.baselined).await,
        Some(vec!["scratch".to_string(), "scratch_new".to_string()])
    );
    assert_eq!(
        baseline_rows(&f.e.owner, f.baselined).await,
        with_new(vec![scratch_a(1)], "Alpha")
    );
    // A job with neither a release nor a baseline stays without one (its baseline is taken at its first write).
    assert_eq!(baseline_kinds(&f.e.owner, f.untouched).await, None);
    assert!(baseline_rows(&f.e.owner, f.untouched).await.is_empty());
    // The other tenant got its own working rows.
    assert_eq!(
        snapshot_rows(&f.e.owner, beta_released, R1).await,
        with_new(vec![scratch_a(1)], "Beta")
    );
    assert_eq!(
        baseline_rows(&f.e.owner, beta_baselined).await,
        with_new(vec![scratch_a(1)], "Beta")
    );
    assert_eq!(
        release_kinds(&f.e.owner, beta_released, R2).await,
        vec!["scratch", "scratch_new"]
    );

    // Stored now: moving the working rows moves the draft, not live, the release or the baseline.
    seed(
        &f.e.owner,
        "scratch_new_item",
        f.tenant,
        f.released,
        "x",
        json!(99),
    )
    .await;
    seed(
        &f.e.owner,
        "scratch_new_item",
        f.tenant,
        f.baselined,
        "x",
        json!(99),
    )
    .await;
    let stored_x = Some(json!({ "n": "Alpha" }));
    for of in [DesignOf::Live, DesignOf::Release(R2.to_string())] {
        assert_eq!(
            new_kind_design(&f, &reg, f.released, "released", of.clone()).await,
            vec![
                ("scratch-new:x".to_string(), stored_x.clone()),
                ("scratch-new:y".to_string(), Some(Value::Null)),
            ],
            "{of:?}"
        );
    }
    assert_eq!(
        new_kind_design(&f, &reg, f.baselined, "baselined", DesignOf::Live).await,
        vec![
            ("scratch-new:x".to_string(), stored_x),
            ("scratch-new:y".to_string(), Some(Value::Null)),
        ]
    );
    assert_eq!(
        new_kind_design(&f, &reg, f.released, "released", DesignOf::Draft).await[0],
        ("scratch-new:x".to_string(), Some(json!(99)))
    );
}

#[tokio::test]
async fn backfill_idempotent() {
    let f = setup().await;
    let reg = new_registry();
    let first = coverage::backfill(&f.e.state.db, &reg).await.unwrap();
    assert_eq!(
        first,
        Backfilled {
            releases: 2,
            baselines: 1,
            rows: 6,
            failed_tenants: 0,
        }
    );
    let after_first = stored_state(&f).await;

    // The working rows move: a second run must not pick that up (everything is covered already).
    seed(
        &f.e.owner,
        "scratch_new_item",
        f.tenant,
        f.released,
        "z",
        json!(7),
    )
    .await;
    assert_eq!(
        coverage::backfill(&f.e.state.db, &reg).await.unwrap(),
        Backfilled::default(),
        "a second run finds nothing to do"
    );
    // An older binary (fewer kinds) booting next does nothing either.
    assert_eq!(
        coverage::backfill(&f.e.state.db, &old_registry())
            .await
            .unwrap(),
        Backfilled::default()
    );
    assert_eq!(stored_state(&f).await, after_first);
    let (_, _, _, k1, _, kb) = after_first;
    assert_eq!(
        k1,
        vec!["scratch", "scratch_new"],
        "the kind is appended once"
    );
    assert_eq!(
        kb,
        Some(vec!["scratch".to_string(), "scratch_new".to_string()])
    );
}

/// The advisory lock waiters on the test database.
async fn lock_waiters(db: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks l JOIN pg_database d ON d.oid = l.database \
          WHERE l.locktype = 'advisory' AND NOT l.granted AND d.datname = current_database()",
    )
    .fetch_one(db)
    .await
    .unwrap()
}

#[tokio::test]
async fn backfill_concurrent_replicas_one_writer() {
    let f = setup().await;
    let reg = Arc::new(new_registry());

    // Hold the coverage lock, as a replica mid-backfill would.
    let mut holder = f.e.owner.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(COVERAGE_LOCK)
        .execute(&mut *holder)
        .await
        .unwrap();

    // Two replicas boot at once.
    let spawn = || {
        let db = f.e.state.db.clone();
        let reg = reg.clone();
        tokio::spawn(async move { coverage::backfill(&db, &reg).await })
    };
    let a = spawn();
    let b = spawn();

    // Both wait on the lock: neither has written anything.
    let mut waiting = 0;
    for _ in 0..100 {
        waiting = lock_waiters(&f.e.owner).await;
        if waiting >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(waiting, 2, "both backfills block on {COVERAGE_LOCK}");
    assert!(!a.is_finished() && !b.is_finished());
    assert_eq!(
        release_kinds(&f.e.owner, f.released, R1).await,
        vec!["scratch"],
        "nothing written while the lock is held"
    );

    holder.commit().await.unwrap();
    let ra = a.await.unwrap().expect("replica a");
    let rb = b.await.unwrap().expect("replica b");

    // One writer: one run did all the work, the other found it done.
    let all = Backfilled {
        releases: 2,
        baselines: 1,
        rows: 6,
        failed_tenants: 0,
    };
    assert!(
        (ra == all && rb == Backfilled::default()) || (rb == all && ra == Backfilled::default()),
        "one writer, one no-op: {ra:?} / {rb:?}"
    );
    for release in [R1, R2] {
        assert_eq!(
            release_kinds(&f.e.owner, f.released, release).await,
            vec!["scratch", "scratch_new"],
            "appended once"
        );
        assert_eq!(
            snapshot_rows(&f.e.owner, f.released, release).await,
            with_new(vec![scratch_a(1)], "Alpha")
        );
    }
    assert_eq!(
        baseline_kinds(&f.e.owner, f.baselined).await,
        Some(vec!["scratch".to_string(), "scratch_new".to_string()])
    );
}

/// Alpha + Beta's full work (one tenant fixture each): R1 + R2 + one baseline, 2 places each.
const ONE_TENANT: Backfilled = Backfilled {
    releases: 2,
    baselines: 1,
    rows: 6,
    failed_tenants: 0,
};

#[tokio::test]
async fn failing_tenant_is_skipped_and_the_others_backfilled() {
    let f = setup().await;
    let (_, beta_released, beta_baselined, _) = tenant_fixture(&f.e.owner, "Beta").await;
    let reg = new_registry();

    // A serving replica holds one of Alpha's jobs (a long approve / an open apply).
    let mut holder = f.e.owner.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(f.released)
        .execute(&mut *holder)
        .await
        .unwrap();

    let opts = BackfillOptions {
        lock_wait: Duration::from_secs(5),
        lock_timeout: Duration::from_millis(50),
        attempts: 3,
        backoff: Duration::from_millis(10),
    };
    let done = coverage::backfill_with(&f.e.state.db, &reg, &opts)
        .await
        .expect("a failing tenant never fails the backfill");
    assert_eq!(
        done,
        Backfilled {
            failed_tenants: 1,
            ..ONE_TENANT
        },
        "Beta done, Alpha failed after its retries"
    );

    // Alpha's transaction rolled back whole: nothing of it written, not even its unlocked baselined job.
    for release in [R1, R2] {
        assert_eq!(
            release_kinds(&f.e.owner, f.released, release).await,
            vec!["scratch"]
        );
        assert_eq!(
            snapshot_rows(&f.e.owner, f.released, release).await,
            vec![scratch_a(1)]
        );
    }
    assert_eq!(
        baseline_kinds(&f.e.owner, f.baselined).await,
        Some(vec!["scratch".to_string()])
    );
    assert_eq!(
        baseline_rows(&f.e.owner, f.baselined).await,
        vec![scratch_a(1)]
    );
    // Beta was backfilled.
    for release in [R1, R2] {
        assert_eq!(
            release_kinds(&f.e.owner, beta_released, release).await,
            vec!["scratch", "scratch_new"]
        );
        assert_eq!(
            snapshot_rows(&f.e.owner, beta_released, release).await,
            with_new(vec![scratch_a(1)], "Beta")
        );
    }
    assert_eq!(
        baseline_rows(&f.e.owner, beta_baselined).await,
        with_new(vec![scratch_a(1)], "Beta")
    );

    // The lock goes away: the next boot covers Alpha, and only Alpha.
    holder.rollback().await.unwrap();
    assert_eq!(
        coverage::backfill(&f.e.state.db, &reg).await.unwrap(),
        ONE_TENANT
    );
    assert_eq!(
        snapshot_rows(&f.e.owner, f.released, R1).await,
        with_new(vec![scratch_a(1)], "Alpha")
    );
}

#[tokio::test]
async fn releases_without_a_snapshot_stay_without_one() {
    let f = setup().await;
    // A cut that stored no snapshot (unappliable) next to the covered ones, and a job whose only release is one.
    insert_release_kinds(&f.e.owner, f.tenant, f.released, R3, "approved", &[]).await;
    let bare = new_job(&f.e.owner, f.tenant, "bare").await;
    insert_release_kinds(&f.e.owner, f.tenant, bare, R1, "approved", &[]).await;
    seed(&f.e.owner, "scratch_item", f.tenant, bare, "a", json!(2)).await;
    seed(
        &f.e.owner,
        "scratch_new_item",
        f.tenant,
        bare,
        "x",
        json!(3),
    )
    .await;

    assert_eq!(
        coverage::backfill(&f.e.state.db, &new_registry())
            .await
            .unwrap(),
        ONE_TENANT,
        "only R1, R2 and the baseline of the fixture"
    );
    for (job, release) in [(f.released, R3), (bare, R1)] {
        assert!(
            release_kinds(&f.e.owner, job, release).await.is_empty(),
            "{release}: still no snapshot kinds"
        );
        assert!(
            snapshot_rows(&f.e.owner, job, release).await.is_empty(),
            "{release}: still no snapshot rows"
        );
    }
    assert_eq!(baseline_kinds(&f.e.owner, bare).await, None);
    // The covered releases of the same job were still backfilled.
    assert_eq!(
        release_kinds(&f.e.owner, f.released, R1).await,
        vec!["scratch", "scratch_new"]
    );
}
