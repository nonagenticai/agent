//! B2 T4: the draft ledger core (spec B2 §2.3–§2.4, D54/D62/D69) — `design::track` / `track_tenant`, the stored
//! baseline, the live read (release snapshot, else baseline), projection reads through the parent place, and the
//! draft-vs-live diff. Real Postgres 18 (one container for this binary, a fresh database per test), through the RLS
//! runtime role `agent_app`.
//!
//! The engine is exercised with test-only targets (`Registry::register_scratch`, kinds `scratch*`, never in
//! `design::KINDS`/`PENDING`) over scratch tables each test creates as the database owner, with RLS + grants to
//! `agent_app` exactly like a real tenant table:
//! - `scratch` (job scope, `scratch_item`), `scratch_parent` (job scope, `scratch_parent_item`);
//! - `scratch_tenant` (tenant scope, D69, `scratch_tenant_item`);
//! - `scratch_proj` (a projection: the `hidden` field of a `scratch_parent` place).
use crate::common;

use std::sync::Arc;

use agent_api::auth::{Caller, RoleSet, TenantStatus};
use agent_api::design::{
    self, CaseDesign, DesignOf, DesignRow, DesignTarget, DraftRef, DraftWrite, Env, JobRef, Label,
    Place, Registry, Scope, Snap, Tracked,
};
use agent_api::error::{ApiError, ApiResult};
use agent_api::tenancy::{JobCtx, TenantCtx};
use common::{TestEnv, insert_tenant};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, PgConnection, PgPool};
use uuid::Uuid;

const DESIGNER: &str = "u-designer";
const R1: &str = "cfg-2026-10-20.1";

const SCRATCH_DDL: &str = r#"
CREATE TABLE scratch_item (
    tenant_id uuid NOT NULL DEFAULT app_current_tenant(),
    job_id    uuid NOT NULL,
    name      text NOT NULL,
    val       jsonb NOT NULL,
    PRIMARY KEY (tenant_id, job_id, name),
    FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);
CREATE TABLE scratch_parent_item (
    tenant_id uuid NOT NULL DEFAULT app_current_tenant(),
    job_id    uuid NOT NULL,
    name      text NOT NULL,
    val       jsonb NOT NULL,
    PRIMARY KEY (tenant_id, job_id, name),
    FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);
CREATE TABLE scratch_tenant_item (
    tenant_id uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    name      text NOT NULL,
    val       jsonb NOT NULL,
    PRIMARY KEY (tenant_id, name)
);
ALTER TABLE scratch_item ENABLE ROW LEVEL SECURITY;
ALTER TABLE scratch_parent_item ENABLE ROW LEVEL SECURITY;
ALTER TABLE scratch_tenant_item ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON scratch_item FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
CREATE POLICY tenant_isolation ON scratch_parent_item FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
CREATE POLICY tenant_isolation ON scratch_tenant_item FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE, DELETE ON scratch_item, scratch_parent_item, scratch_tenant_item TO agent_app;
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

/// Write (or with `None` remove) the row `name` of a job-scoped scratch table.
async fn put_row(
    c: &mut PgConnection,
    table: &'static str,
    job: Uuid,
    name: String,
    v: Snap,
) -> ApiResult<()> {
    match v {
        None => {
            sqlx::query(AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE job_id = $1 AND name = $2"
            )))
            .bind(job)
            .bind(name)
            .execute(&mut *c)
            .await?;
        }
        Some(val) => {
            sqlx::query(AssertSqlSafe(format!(
                "INSERT INTO {table} (job_id, name, val) VALUES ($1, $2, $3) \
                 ON CONFLICT (tenant_id, job_id, name) DO UPDATE SET val = EXCLUDED.val"
            )))
            .bind(job)
            .bind(name)
            .bind(val)
            .execute(&mut *c)
            .await?;
        }
    }
    Ok(())
}

/// Write (or remove) a tenant-wide scratch row.
async fn put_tenant_row(
    c: &mut PgConnection,
    tenant: Uuid,
    name: String,
    v: Snap,
) -> ApiResult<()> {
    match v {
        None => {
            sqlx::query("DELETE FROM scratch_tenant_item WHERE tenant_id = $1 AND name = $2")
                .bind(tenant)
                .bind(name)
                .execute(&mut *c)
                .await?;
        }
        Some(val) => {
            sqlx::query(
                "INSERT INTO scratch_tenant_item (tenant_id, name, val) VALUES ($1, $2, $3) \
                 ON CONFLICT (tenant_id, name) DO UPDATE SET val = EXCLUDED.val",
            )
            .bind(tenant)
            .bind(name)
            .bind(val)
            .execute(&mut *c)
            .await?;
        }
    }
    Ok(())
}

/// Set the `hidden` field of a `scratch_parent_item` row (the projection's working row).
async fn set_hidden(c: &mut PgConnection, job: Uuid, name: String, hidden: Value) -> ApiResult<()> {
    sqlx::query(
        "UPDATE scratch_parent_item SET val = jsonb_set(val, '{hidden}', $3) WHERE job_id = $1 AND name = $2",
    )
    .bind(job)
    .bind(name)
    .bind(hidden)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// A write that changes the working row and then refuses (a handler's validation failing after a write).
async fn write_then_refuse(c: &mut PgConnection, job: Uuid) -> ApiResult<()> {
    put_row(c, "scratch_item", job, "a".to_string(), Some(json!(2))).await?;
    Err(ApiError::Conflict(
        "scratch_refused",
        "the scratch write refuses".to_string(),
    ))
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
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            put_row(c, self.table, job, name(r), v.clone()).await
        })
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

/// A tenant-wide kind (D69): one row per tenant, the same for every job.
struct TenantRows;

impl DesignTarget for TenantRows {
    fn kind(&self) -> &'static str {
        "scratch_tenant"
    }
    fn scope(&self) -> Scope {
        Scope::Tenant
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("scratch-tenant:{}", name(r))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let v: Option<Value> = sqlx::query_scalar(
                "SELECT val FROM scratch_tenant_item WHERE tenant_id = $1 AND name = $2",
            )
            .bind(at.tenant)
            .bind(name(r))
            .fetch_optional(&mut *c)
            .await?;
            Ok(v)
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { put_tenant_row(c, at.tenant, name(r), v.clone()).await })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let names: Vec<String> = sqlx::query_scalar(
                "SELECT name FROM scratch_tenant_item WHERE tenant_id = $1 ORDER BY name",
            )
            .bind(at.tenant)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(names.into_iter().map(|n| name_ref(&n)).collect()))
        })
    }
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        v.as_ref().map(Value::to_string)
    }
}

/// A projection kind: the `hidden` field of a `scratch_parent` place (the fixture's `entity_hidden` shape).
struct Hidden;

impl DesignTarget for Hidden {
    fn kind(&self) -> &'static str {
        "scratch_proj"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("scratch-proj:{}", name(r))
    }
    fn is_projection(&self) -> bool {
        true
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let v: Option<Option<Value>> = sqlx::query_scalar(
                "SELECT val -> 'hidden' FROM scratch_parent_item WHERE job_id = $1 AND name = $2",
            )
            .bind(job)
            .bind(name(r))
            .fetch_optional(&mut *c)
            .await?;
            Ok(v.map(|h| h.unwrap_or(Value::Null)))
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            match v {
                Some(h) => set_hidden(c, job, name(r), h.clone()).await,
                None => Ok(()),
            }
        })
    }
    fn places<'a>(
        &'a self,
        _c: &'a mut PgConnection,
        _at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async { Ok(None) })
    }
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        v.as_ref().map(Value::to_string)
    }
    fn parent(&self, r: &DraftRef) -> Option<(&'static str, DraftRef)> {
        Some(("scratch_parent", name_ref(&name(r))))
    }
    fn project(&self, parent: &Snap, _r: &DraftRef) -> Snap {
        parent
            .as_ref()
            .map(|p| p.get("hidden").cloned().unwrap_or(Value::Null))
    }
}

fn registry() -> Registry {
    let mut r = Registry::default();
    r.register_scratch(Arc::new(JobRows {
        kind: "scratch",
        prefix: "scratch",
        table: "scratch_item",
    }));
    r.register_scratch(Arc::new(JobRows {
        kind: "scratch_parent",
        prefix: "scratch-parent",
        table: "scratch_parent_item",
    }));
    r.register_scratch(Arc::new(TenantRows));
    r.register_scratch(Arc::new(Hidden));
    r
}

// ---- fixture --------------------------------------------------------------------------------------------------------

struct Fx {
    e: TestEnv,
    reg: Registry,
    tenant: Uuid,
    j1: JobCtx,
}

fn tenant_ctx(tenant: Uuid) -> TenantCtx {
    let caller = Caller {
        sub: DESIGNER.to_string(),
        actor: DESIGNER.to_string(),
        home: None,
        super_admin: false,
        session: "s-test".to_string(),
        verified_email: None,
    };
    TenantCtx::new(
        tenant,
        TenantStatus::Active,
        RoleSet::ALL,
        None,
        caller,
        false,
    )
}

fn job_ctx(tenant: Uuid, id: Uuid, key: &str) -> JobCtx {
    JobCtx {
        t: tenant_ctx(tenant),
        job_id: id,
        job_key: key.to_string(),
    }
}

fn jref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
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

async fn setup() -> Fx {
    let e = common::env().await;
    sqlx::raw_sql(SCRATCH_DDL).execute(&e.owner).await.unwrap();
    let tenant = Uuid::new_v4();
    insert_tenant(&e.owner, tenant, "Alpha").await;
    let id = new_job(&e.owner, tenant, "j1").await;
    Fx {
        e,
        reg: registry(),
        tenant,
        j1: job_ctx(tenant, id, "j1"),
    }
}

/// A working row written as the owner, outside the ledger (as provisioned, or an untracked change).
async fn seed(f: &Fx, table: &'static str, job: Uuid, name: &str, val: Value) {
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {table} (tenant_id, job_id, name, val) VALUES ($1, $2, $3, $4)"
    )))
    .bind(f.tenant)
    .bind(job)
    .bind(name)
    .bind(val)
    .execute(&f.e.owner)
    .await
    .unwrap();
}

async fn seed_tenant(f: &Fx, name: &str, val: Value) {
    sqlx::query("INSERT INTO scratch_tenant_item (tenant_id, name, val) VALUES ($1, $2, $3)")
        .bind(f.tenant)
        .bind(name)
        .bind(val)
        .execute(&f.e.owner)
        .await
        .unwrap();
}

fn scratch_write(kind: &'static str, n: &str, label: &str) -> DraftWrite {
    DraftWrite {
        area: "scratch",
        page: "J7",
        kind,
        r#ref: name_ref(n),
        label: Label::Now(label.to_string()),
    }
}

/// One tracked write of `scratch_item.<n>` = `v` (`None` = remove), committed.
async fn write(f: &Fx, j: &JobCtx, n: &str, v: Snap) -> Tracked<()> {
    let mut tx = j.begin(&f.e.state.db).await.unwrap();
    let (job, row) = (j.job_id, n.to_string());
    let w = scratch_write("scratch", n, &format!("Set {n}"));
    let out = design::track::<(), _>(&mut tx, &f.reg, j, w, move |c| {
        Box::pin(put_row(c, "scratch_item", job, row, v))
    })
    .await
    .expect("tracked write");
    tx.commit().await.unwrap();
    out
}

/// One design read in its own job-scoped transaction.
async fn read(f: &Fx, j: &JobCtx, of: DesignOf, kind: &str, n: &str) -> ApiResult<Snap> {
    let mut tx = j.begin(&f.e.state.db).await?;
    let v = design::value(&mut tx, &f.reg, &jref(j), &of, kind, &name_ref(n)).await;
    tx.commit().await?;
    v
}

/// A ledger row as stored (owner view).
#[derive(Debug, sqlx::FromRow)]
struct Ledger {
    id: i64,
    key: String,
    kind: String,
    page: String,
    label: String,
    by_sub: String,
    before: Option<Value>,
    after: Option<Value>,
}

async fn ledger(db: &PgPool, job: Uuid) -> Vec<Ledger> {
    sqlx::query_as::<_, Ledger>(
        "SELECT id, key, kind, page, label, by_sub, before, after FROM draft_ledger WHERE job_id = $1 ORDER BY id",
    )
    .bind(job)
    .fetch_all(db)
    .await
    .unwrap()
}

async fn stored_rev(db: &PgPool, job: Uuid) -> i64 {
    sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
        .bind(job)
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

/// The job's stored baseline `(key, kind, snap)`, sorted by key (byte order).
async fn baseline_rows(db: &PgPool, job: Uuid) -> Vec<(String, String, Option<Value>)> {
    let mut rows: Vec<(String, String, Option<Value>)> =
        sqlx::query_as("SELECT key, kind, snap FROM release_baseline WHERE job_id = $1")
            .bind(job)
            .fetch_all(db)
            .await
            .unwrap();
    rows.sort_by(|x, y| x.0.cmp(&y.0));
    rows
}

/// A tenant audit row (owner view).
#[derive(Debug, sqlx::FromRow)]
struct AuditRow {
    resource_type: String,
    resource_id: String,
    job: Option<String>,
    actor_id: String,
    details: Value,
}

async fn audit_rows(db: &PgPool, action: &str) -> Vec<AuditRow> {
    sqlx::query_as::<_, AuditRow>(
        "SELECT resource_type, resource_id, job, actor_id, details FROM audit_event WHERE action = $1 ORDER BY id",
    )
    .bind(action)
    .fetch_all(db)
    .await
    .unwrap()
}

/// A release row (owner) covering the three scratch snapshot kinds; any state but `approved` carries `released_*`.
async fn insert_release(db: &PgPool, j: &JobCtx, release: &str, state: &str) {
    let sha = |c: char| format!("sha256:{}", c.to_string().repeat(64));
    sqlx::query(
        r#"INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins,
               provenance, requires, causes, state, approved_by, approved_at, released_by, released_at,
               snapshot_kinds)
           VALUES ($1, $2, $3, $4, $5, '{"sections": []}', '{}', '[]', '{}', '{}', '[]', $6, 'u-rm', now(),
               CASE WHEN $6 = 'approved' THEN NULL ELSE 'u-rm' END,
               CASE WHEN $6 = 'approved' THEN NULL ELSE now() END,
               ARRAY['scratch', 'scratch_parent', 'scratch_tenant'])"#,
    )
    .bind(j.t.tenant)
    .bind(j.job_id)
    .bind(release)
    .bind(sha('a'))
    .bind(sha('b'))
    .bind(state)
    .execute(db)
    .await
    .unwrap();
}

/// One stored snapshot row of `release` (owner): kind `scratch` (key `scratch:<n>`) or `scratch_parent`.
async fn snapshot_row(db: &PgPool, j: &JobCtx, release: &str, kind: &str, n: &str, v: Value) {
    let key = match kind {
        "scratch" => format!("scratch:{n}"),
        _ => format!("scratch-parent:{n}"),
    };
    sqlx::query(
        "INSERT INTO release_snapshot (tenant_id, job_id, release_id, key, kind, ref, snap) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(j.t.tenant)
    .bind(j.job_id)
    .bind(release)
    .bind(key)
    .bind(kind)
    .bind(json!({ "name": n }))
    .bind(json!({ "v": v }))
    .execute(db)
    .await
    .unwrap();
}

async fn make_current(db: &PgPool, j: &JobCtx, release: &str) {
    sqlx::query("UPDATE jobs SET current_release_id = $2 WHERE id = $1")
        .bind(j.job_id)
        .bind(release)
        .execute(db)
        .await
        .unwrap();
}

// ---- tests ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn write_records_ledger_row_with_before_after() {
    let f = setup().await;
    seed(&f, "scratch_item", f.j1.job_id, "a", json!(1)).await;

    let out = write(&f, &f.j1, "a", Some(json!(2))).await;
    let change = out.change.expect("a change away from live is recorded");
    assert_eq!(change.job_id, f.j1.job_id);
    assert_eq!(change.key, "scratch:a");
    assert_eq!(change.kind, "scratch");
    assert_eq!(change.r#ref, name_ref("a"));
    assert_eq!(change.area, "scratch");
    assert_eq!(change.page, "J7");
    assert_eq!(change.label, "Set a");
    assert_eq!(change.by_sub, DESIGNER);
    assert_eq!(change.before, Some(json!(1)), "before = the value replaced");
    assert_eq!(change.after, Some(json!(2)), "after = the draft value");
    assert_eq!(change.live_moved, None);

    let rows = ledger(&f.e.owner, f.j1.job_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let r = &rows[0];
    assert_eq!(r.id, change.id);
    assert_eq!((r.key.as_str(), r.kind.as_str()), ("scratch:a", "scratch"));
    assert_eq!(
        (r.page.as_str(), r.label.as_str(), r.by_sub.as_str()),
        ("J7", "Set a", DESIGNER)
    );
    // Stored exactly as design::snap_to_json.
    assert_eq!(r.before, Some(json!({"v": 1})));
    assert_eq!(r.after, Some(json!({"v": 2})));
}

#[tokio::test]
async fn second_write_keeps_first_before_and_id() {
    let f = setup().await;
    seed(&f, "scratch_item", f.j1.job_id, "a", json!(1)).await;

    let first = write(&f, &f.j1, "a", Some(json!(2))).await.change.unwrap();
    let second = write(&f, &f.j1, "a", Some(json!(3))).await.change.unwrap();
    assert_eq!(
        second.id, first.id,
        "repeated saves of one key collapse into one row"
    );
    assert_eq!(
        second.before,
        Some(json!(1)),
        "before stays the value the FIRST save replaced"
    );
    assert_eq!(second.after, Some(json!(3)));

    let rows = ledger(&f.e.owner, f.j1.job_id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].id, first.id);
    assert_eq!(rows[0].before, Some(json!({"v": 1})));
    assert_eq!(rows[0].after, Some(json!({"v": 3})));
}

#[tokio::test]
async fn write_back_to_live_drops_row() {
    let f = setup().await;
    seed(&f, "scratch_item", f.j1.job_id, "a", json!(1)).await;

    assert!(write(&f, &f.j1, "a", Some(json!(2))).await.change.is_some());
    let back = write(&f, &f.j1, "a", Some(json!(1))).await;
    assert!(
        back.change.is_none(),
        "a write that puts the live value back drops the row"
    );
    assert!(ledger(&f.e.owner, f.j1.job_id).await.is_empty());

    // Unless an approved cut still carries the change: then the row stays (draft = live), so applying that cut does
    // not silently undo the revert.
    let again = write(&f, &f.j1, "a", Some(json!(2))).await.change.unwrap();
    insert_release(&f.e.owner, &f.j1, R1, "approved").await;
    sqlx::query(
        "INSERT INTO release_bundled_draft (tenant_id, job_id, release_id, key, ledger_id, kind, ref, area, page, \
             label, after, at, by_sub) \
         VALUES ($1, $2, $3, 'scratch:a', $4, 'scratch', '{\"name\": \"a\"}', 'scratch', 'J7', 'Set a', \
             '{\"v\": 2}', now(), 'u-designer')",
    )
    .bind(f.tenant)
    .bind(f.j1.job_id)
    .bind(R1)
    .bind(again.id)
    .execute(&f.e.owner)
    .await
    .unwrap();
    let kept = write(&f, &f.j1, "a", Some(json!(1)))
        .await
        .change
        .expect("an approved cut bundles scratch:a: the revert is kept");
    assert_eq!(kept.id, again.id);
    assert_eq!(kept.after, Some(json!(1)));
    assert_eq!(ledger(&f.e.owner, f.j1.job_id).await.len(), 1);
}

#[tokio::test]
async fn refused_write_records_nothing() {
    let f = setup().await;
    seed(&f, "scratch_item", f.j1.job_id, "a", json!(1)).await;

    let mut tx = f.j1.begin(&f.e.state.db).await.unwrap();
    let job = f.j1.job_id;
    let res = design::track::<(), _>(
        &mut tx,
        &f.reg,
        &f.j1,
        scratch_write("scratch", "a", "Set a"),
        move |c| Box::pin(write_then_refuse(c, job)),
    )
    .await;
    let Err(err) = res else {
        panic!("a refusing mutate must fail the tracked write");
    };
    assert_eq!(
        err.code(),
        "scratch_refused",
        "mutate's own error comes back"
    );
    // Committed on purpose: even then track itself wrote no ledger row, no draft_rev bump and no audit row.
    tx.commit().await.unwrap();

    assert!(ledger(&f.e.owner, job).await.is_empty());
    assert_eq!(stored_rev(&f.e.owner, job).await, 0);
    assert!(audit_rows(&f.e.owner, "job.draft_change").await.is_empty());
}

#[tokio::test]
async fn baseline_taken_before_first_write() {
    let f = setup().await;
    let job = f.j1.job_id;
    seed(&f, "scratch_item", job, "a", json!(1)).await;
    seed(
        &f,
        "scratch_parent_item",
        job,
        "p",
        json!({"hidden": false}),
    )
    .await;
    seed_tenant(&f, "llm", json!("small")).await;

    assert_eq!(
        baseline_kinds(&f.e.owner, job).await,
        None,
        "no baseline before any write"
    );
    assert!(baseline_rows(&f.e.owner, job).await.is_empty());

    write(&f, &f.j1, "a", Some(json!(2))).await;
    // Sorted by key (byte order: '-' < ':').
    let want = vec![
        (
            "scratch-parent:p".to_string(),
            "scratch_parent".to_string(),
            Some(json!({"v": {"hidden": false}})),
        ),
        (
            "scratch-tenant:llm".to_string(),
            "scratch_tenant".to_string(),
            Some(json!({"v": "small"})),
        ),
        // The value BEFORE the write: the baseline is taken first.
        (
            "scratch:a".to_string(),
            "scratch".to_string(),
            Some(json!({"v": 1})),
        ),
    ];
    assert_eq!(baseline_rows(&f.e.owner, job).await, want);
    assert_eq!(
        baseline_kinds(&f.e.owner, job).await,
        Some(vec![
            "scratch".to_string(),
            "scratch_parent".to_string(),
            "scratch_tenant".to_string()
        ]),
        "every registered snapshot kind, projections excluded"
    );

    // Idempotent: a later write neither retakes nor extends it.
    write(&f, &f.j1, "a", Some(json!(3))).await;
    assert_eq!(baseline_rows(&f.e.owner, job).await, want);
    assert_eq!(
        read(&f, &f.j1, DesignOf::Live, "scratch", "a")
            .await
            .unwrap(),
        Some(json!(1))
    );
}

#[tokio::test]
async fn draft_rev_bumps_only_on_change() {
    let f = setup().await;
    let job = f.j1.job_id;
    seed(&f, "scratch_item", job, "a", json!(1)).await;
    assert_eq!(stored_rev(&f.e.owner, job).await, 0);

    assert_eq!(write(&f, &f.j1, "a", Some(json!(2))).await.draft_rev, 1);
    assert_eq!(
        write(&f, &f.j1, "a", Some(json!(2))).await.draft_rev,
        1,
        "the same value again is no change"
    );
    assert_eq!(
        write(&f, &f.j1, "a", Some(json!(1))).await.draft_rev,
        2,
        "back to live is still a change of the draft"
    );
    assert_eq!(stored_rev(&f.e.owner, job).await, 2);

    let mut tx = f.j1.begin(&f.e.state.db).await.unwrap();
    assert_eq!(design::draft_rev(&mut tx, &jref(&f.j1)).await.unwrap(), 2);
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn live_reads_snapshot_not_working_rows() {
    let f = setup().await;
    seed(&f, "scratch_item", f.j1.job_id, "a", json!(1)).await;
    write(&f, &f.j1, "a", Some(json!(2))).await;

    // Never released: live = the stored baseline, draft = the working rows.
    assert_eq!(
        read(&f, &f.j1, DesignOf::Live, "scratch", "a")
            .await
            .unwrap(),
        Some(json!(1))
    );
    assert_eq!(
        read(&f, &f.j1, DesignOf::Draft, "scratch", "a")
            .await
            .unwrap(),
        Some(json!(2))
    );

    let mut tx = f.j1.begin(&f.e.state.db).await.unwrap();
    let jr = jref(&f.j1);
    assert_eq!(
        design::case_design(&mut tx, &jr, Env::Production)
            .await
            .unwrap(),
        CaseDesign::Live { release_id: None }
    );
    assert_eq!(
        design::case_design(&mut tx, &jr, Env::Test).await.unwrap(),
        CaseDesign::Draft { draft_rev: 1 }
    );
    tx.commit().await.unwrap();

    // Released: live = the current release's snapshot — neither the working rows (2) nor the baseline (1).
    insert_release(&f.e.owner, &f.j1, R1, "current").await;
    snapshot_row(&f.e.owner, &f.j1, R1, "scratch", "a", json!(5)).await;
    make_current(&f.e.owner, &f.j1, R1).await;
    for _ in 0..2 {
        // The second read is served by the snapshot LRU.
        assert_eq!(
            read(&f, &f.j1, DesignOf::Live, "scratch", "a")
                .await
                .unwrap(),
            Some(json!(5))
        );
    }
    assert_eq!(
        read(&f, &f.j1, DesignOf::Draft, "scratch", "a")
            .await
            .unwrap(),
        Some(json!(2))
    );
    assert_eq!(
        read(&f, &f.j1, DesignOf::Release(R1.to_string()), "scratch", "a")
            .await
            .unwrap(),
        Some(json!(5))
    );
    let missing = read(
        &f,
        &f.j1,
        DesignOf::Release("cfg-2026-10-20.9".to_string()),
        "scratch",
        "a",
    )
    .await
    .unwrap_err();
    assert_eq!(missing.code(), "release_not_found");

    let mut tx = f.j1.begin(&f.e.state.db).await.unwrap();
    let want = vec![DesignRow {
        key: "scratch:a".to_string(),
        kind: "scratch".to_string(),
        r#ref: name_ref("a"),
        snap: Some(json!(5)),
    }];
    assert_eq!(
        design::values_of_kind(&mut tx, &f.reg, &jr, &DesignOf::Live, "scratch")
            .await
            .unwrap(),
        want
    );
    assert_eq!(
        design::take_snapshot(&mut tx, &f.reg, &jr, &DesignOf::Live)
            .await
            .unwrap(),
        want
    );
    assert_eq!(
        design::case_design(&mut tx, &jr, Env::Production)
            .await
            .unwrap(),
        CaseDesign::Live {
            release_id: Some(R1.to_string())
        }
    );
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn diff_is_working_rows_vs_live_snapshot() {
    let f = setup().await;
    let job = f.j1.job_id;
    seed(&f, "scratch_item", job, "a", json!(1)).await;
    seed(&f, "scratch_item", job, "b", json!(1)).await;
    seed(&f, "scratch_item", job, "same", json!(7)).await;

    write(&f, &f.j1, "a", Some(json!(2))).await; // changed
    write(&f, &f.j1, "c", Some(json!(3))).await; // added
    write(&f, &f.j1, "b", None).await; // removed
    // Changed outside the ledger: the diff still sees it (it compares rows, it does not replay the ledger).
    seed(&f, "scratch_item", job, "d", json!(4)).await;

    let mut tx = f.j1.begin(&f.e.state.db).await.unwrap();
    let mut deltas = design::diff(&mut tx, &f.reg, &jref(&f.j1)).await.unwrap();
    tx.commit().await.unwrap();
    deltas.sort_by(|x, y| x.place.key.cmp(&y.place.key));

    let keys: Vec<&str> = deltas.iter().map(|d| d.place.key.as_str()).collect();
    assert_eq!(keys, ["scratch:a", "scratch:b", "scratch:c", "scratch:d"]);
    let lv: Vec<(Snap, Snap)> = deltas
        .iter()
        .map(|d| (d.live.clone(), d.draft.clone()))
        .collect();
    assert_eq!(
        lv,
        [
            (Some(json!(1)), Some(json!(2))),
            (Some(json!(1)), None),
            (None, Some(json!(3))),
            (None, Some(json!(4))),
        ]
    );
    let changes: Vec<Vec<&str>> = deltas
        .iter()
        .map(|d| d.changes.iter().map(|c| c.key.as_str()).collect())
        .collect();
    assert_eq!(
        changes,
        [
            vec!["scratch:a"],
            vec!["scratch:b"],
            vec!["scratch:c"],
            Vec::<&str>::new(),
        ]
    );
    assert!(deltas.iter().all(|d| d.place.kind == "scratch"));
}

#[tokio::test]
async fn track_tenant_records_every_job_and_bumps_all() {
    let f = setup().await;
    let j2 = job_ctx(f.tenant, new_job(&f.e.owner, f.tenant, "j2").await, "j2");
    seed_tenant(&f, "llm", json!("small")).await;

    let t = tenant_ctx(f.tenant);
    let mut tx = t.begin(&f.e.state.db).await.unwrap();
    let tenant = f.tenant;
    let w = DraftWrite {
        area: "scratch",
        page: "J7",
        kind: "scratch_tenant",
        r#ref: name_ref("llm"),
        label: Label::After(Box::new(|s: &Snap| {
            format!(
                "Model set to {}",
                s.as_ref().map_or_else(String::new, Value::to_string)
            )
        })),
    };
    let out = design::track_tenant::<(), _>(&mut tx, &f.reg, &t, w, move |c| {
        Box::pin(put_tenant_row(
            c,
            tenant,
            "llm".to_string(),
            Some(json!("large")),
        ))
    })
    .await
    .expect("tenant-wide write");
    tx.commit().await.unwrap();

    assert_eq!(out.draft_rev, 1);
    let change = out.change.expect("recorded");
    assert_eq!(change.key, "scratch-tenant:llm");
    assert_eq!(
        change.label, "Model set to \"large\"",
        "Label::After sees the value mutate wrote"
    );
    assert!(change.job_id == f.j1.job_id || change.job_id == j2.job_id);
    for j in [&f.j1, &j2] {
        let rows = ledger(&f.e.owner, j.job_id).await;
        assert_eq!(rows.len(), 1, "every job's ledger has the row: {rows:?}");
        assert_eq!(rows[0].key, "scratch-tenant:llm");
        assert_eq!(rows[0].kind, "scratch_tenant");
        assert_eq!(rows[0].before, Some(json!({"v": "small"})));
        assert_eq!(rows[0].after, Some(json!({"v": "large"})));
        assert_eq!(
            stored_rev(&f.e.owner, j.job_id).await,
            1,
            "every job's draft_rev bumps"
        );
        // Each job's baseline was taken BEFORE the write.
        let base: Option<Value> = sqlx::query_scalar(
            "SELECT snap FROM release_baseline WHERE job_id = $1 AND key = 'scratch-tenant:llm'",
        )
        .bind(j.job_id)
        .fetch_one(&f.e.owner)
        .await
        .unwrap();
        assert_eq!(base, Some(json!({"v": "small"})));
    }
    let audit = audit_rows(&f.e.owner, "tenant.draft_change").await;
    assert_eq!(audit.len(), 1, "one tenant-wide audit row");
    assert_eq!(audit[0].resource_id, "scratch-tenant:llm");
    assert_eq!(audit[0].job, None);
    assert_eq!(
        audit[0].details,
        json!({"kind": "scratch_tenant", "page": "J7", "ledger_id": change.id})
    );
    assert!(audit_rows(&f.e.owner, "job.draft_change").await.is_empty());

    // A job-scoped kind cannot be written tenant-wide (a programming error).
    let mut tx = t.begin(&f.e.state.db).await.unwrap();
    let res = design::track_tenant::<(), _>(
        &mut tx,
        &f.reg,
        &t,
        scratch_write("scratch", "a", "Set a"),
        |_c| Box::pin(async { Ok(()) }),
    )
    .await;
    let Err(err) = res else {
        panic!("track_tenant must refuse a job-scoped kind");
    };
    assert_eq!(err.code(), "internal");
}

#[tokio::test]
async fn projection_kind_reads_through_parent() {
    let f = setup().await;
    let job = f.j1.job_id;
    seed(
        &f,
        "scratch_parent_item",
        job,
        "p",
        json!({"hidden": false, "label": "Plan"}),
    )
    .await;

    let mut tx = f.j1.begin(&f.e.state.db).await.unwrap();
    let out = design::track::<(), _>(
        &mut tx,
        &f.reg,
        &f.j1,
        scratch_write("scratch_proj", "p", "Hide Plan"),
        move |c| Box::pin(set_hidden(c, job, "p".to_string(), json!(true))),
    )
    .await
    .expect("projection write");
    tx.commit().await.unwrap();
    let change = out.change.expect("recorded");
    assert_eq!(
        (change.kind.as_str(), change.key.as_str()),
        ("scratch_proj", "scratch-proj:p")
    );
    assert_eq!(change.before, Some(json!(false)));
    assert_eq!(change.after, Some(json!(true)));

    // Live: the projection is read out of the parent's stored (baseline) place, not looked up under its own key.
    assert_eq!(
        read(&f, &f.j1, DesignOf::Live, "scratch_proj", "p")
            .await
            .unwrap(),
        Some(json!(false))
    );
    assert_eq!(
        read(&f, &f.j1, DesignOf::Draft, "scratch_proj", "p")
            .await
            .unwrap(),
        Some(json!(true))
    );
    assert_eq!(
        read(&f, &f.j1, DesignOf::Live, "scratch_parent", "p")
            .await
            .unwrap(),
        Some(json!({"hidden": false, "label": "Plan"}))
    );

    // The diff shows the parent place, carrying the projection's ledger row.
    let mut tx = f.j1.begin(&f.e.state.db).await.unwrap();
    let deltas = design::diff(&mut tx, &f.reg, &jref(&f.j1)).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(deltas.len(), 1, "{deltas:?}");
    assert_eq!(deltas[0].place.key, "scratch-parent:p");
    assert_eq!(deltas[0].place.kind, "scratch_parent");
    assert_eq!(
        deltas[0].live,
        Some(json!({"hidden": false, "label": "Plan"}))
    );
    assert_eq!(
        deltas[0].draft,
        Some(json!({"hidden": true, "label": "Plan"}))
    );
    assert_eq!(deltas[0].changes.len(), 1);
    assert_eq!(deltas[0].changes[0].key, "scratch-proj:p");

    // Released with the parent hidden: live follows the release snapshot's parent place.
    insert_release(&f.e.owner, &f.j1, R1, "current").await;
    snapshot_row(
        &f.e.owner,
        &f.j1,
        R1,
        "scratch_parent",
        "p",
        json!({"hidden": true, "label": "Plan"}),
    )
    .await;
    make_current(&f.e.owner, &f.j1, R1).await;
    assert_eq!(
        read(&f, &f.j1, DesignOf::Live, "scratch_proj", "p")
            .await
            .unwrap(),
        Some(json!(true))
    );
}

#[tokio::test]
async fn track_emits_audit_job_draft_change() {
    let f = setup().await;
    seed(&f, "scratch_item", f.j1.job_id, "a", json!(1)).await;

    let id = write(&f, &f.j1, "a", Some(json!(2)))
        .await
        .change
        .unwrap()
        .id;
    let rows = audit_rows(&f.e.owner, "job.draft_change").await;
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(
        r.resource_type, "scratch",
        "resource type = the design area"
    );
    assert_eq!(r.resource_id, "scratch:a", "resource id = the place key");
    assert_eq!(r.job.as_deref(), Some("j1"), "the job KEY");
    assert_eq!(r.actor_id, DESIGNER);
    // Exactly kind, page and ledger id: never the label (free text a person typed).
    assert_eq!(
        r.details,
        json!({"kind": "scratch", "page": "J7", "ledger_id": id})
    );

    // A write back to live is audited too; its row was dropped, so no ledger id.
    write(&f, &f.j1, "a", Some(json!(1))).await;
    let rows = audit_rows(&f.e.owner, "job.draft_change").await;
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[1].details,
        json!({"kind": "scratch", "page": "J7", "ledger_id": null})
    );
}

#[test]
#[should_panic(expected = "register_scratch takes only test kinds")]
fn register_scratch_refuses_a_real_kind() {
    let mut r = Registry::default();
    r.register_scratch(Arc::new(JobRows {
        kind: "gate",
        prefix: "gate",
        table: "scratch_item",
    }));
}
