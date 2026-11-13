//! B2 T14: the draft operations and design reads (spec B2 §4, §2.7; D54/D62/D69) — `GET /draft`, `/draft/diff`,
//! `DELETE /draft[/{id}]`, `GET /draft/published`, `POST /draft/restore`, `GET /design/{live,draft,diff}`,
//! `GET /api/design/tenant-diff`, and the cross-plan helpers `design::lifecycle` / `ledger::keys`. Real Postgres 18
//! (one container for this binary, a fresh database per test), through the RLS runtime role `agent_app` and the real
//! router.
//!
//! The router runs over a registry of test-only targets (`Registry::register_scratch`): `scratch` (job scope,
//! `scratch_item`) and `scratch_tenant` (tenant-wide, D69, `scratch_tenant_item`) — no production kind is tenant-wide
//! yet. Releases, cuts, published changes and conflicts are inserted as rows (§3) so this file does not depend on the
//! cut/apply code.
use crate::common;

use std::sync::Arc;

use agent_api::auth::{Caller, RoleSet, TenantStatus};
use agent_api::design::{
    self, DesignOf, DesignTarget, DraftRef, DraftWrite, JobRef, KeysLifecycle, Label, Place,
    Registry, Scope, Snap,
};
use agent_api::error::{ApiError, ApiResult};
use agent_api::tenancy::{JobCtx, TenantCtx};
use axum::Router;
use axum::http::StatusCode;
use common::{TestEnv, get, insert_tenant, insert_user, json as req, person, send};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

const DESIGNER: &str = "u-designer";
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
CREATE TABLE scratch_tenant_item (
    tenant_id uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    name      text NOT NULL,
    val       jsonb NOT NULL,
    PRIMARY KEY (tenant_id, name)
);
ALTER TABLE scratch_item ENABLE ROW LEVEL SECURITY;
ALTER TABLE scratch_tenant_item ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON scratch_item FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
CREATE POLICY tenant_isolation ON scratch_tenant_item FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE, DELETE ON scratch_item, scratch_tenant_item TO agent_app;
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

async fn put_row(c: &mut PgConnection, job: Uuid, name: String, v: Snap) -> ApiResult<()> {
    match v {
        None => {
            sqlx::query("DELETE FROM scratch_item WHERE job_id = $1 AND name = $2")
                .bind(job)
                .bind(name)
                .execute(&mut *c)
                .await?;
        }
        Some(val) => {
            sqlx::query(
                "INSERT INTO scratch_item (job_id, name, val) VALUES ($1, $2, $3) \
                 ON CONFLICT (tenant_id, job_id, name) DO UPDATE SET val = EXCLUDED.val",
            )
            .bind(job)
            .bind(name)
            .bind(val)
            .execute(&mut *c)
            .await?;
        }
    }
    Ok(())
}

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

/// Job scope, key `scratch:<name>`.
struct JobRows;

impl DesignTarget for JobRows {
    fn kind(&self) -> &'static str {
        "scratch"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("scratch:{}", name(r))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let v: Option<Value> =
                sqlx::query_scalar("SELECT val FROM scratch_item WHERE job_id = $1 AND name = $2")
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
            put_row(c, job, name(r), v.clone()).await
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let names: Vec<String> =
                sqlx::query_scalar("SELECT name FROM scratch_item WHERE job_id = $1 ORDER BY name")
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

/// Tenant-wide (D69), key `scratch-tenant:<name>`.
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

fn registry() -> Registry {
    let mut r = Registry::default();
    r.register_scratch(Arc::new(JobRows));
    r.register_scratch(Arc::new(TenantRows));
    r
}

// ---- fixture --------------------------------------------------------------------------------------------------------

struct Fx {
    e: TestEnv,
    /// The router over the scratch registry.
    app: Router,
    reg: Arc<Registry>,
    tenant: Uuid,
    j1: JobCtx,
    tok: String,
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
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) \
         VALUES ($1, $2, upper($2), '[]', 'test') RETURNING id",
    )
    .bind(tenant)
    .bind(key)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn setup() -> Fx {
    let mut e = common::env().await;
    sqlx::raw_sql(SCRATCH_DDL).execute(&e.owner).await.unwrap();
    let reg = Arc::new(registry());
    e.state.design = reg.clone();
    let app = agent_api::router(e.state.clone());
    let tenant = Uuid::new_v4();
    insert_tenant(&e.owner, tenant, "Alpha").await;
    insert_user(&e.owner, DESIGNER, tenant, "job_designer").await;
    let id = new_job(&e.owner, tenant, "j1").await;
    Fx {
        e,
        app,
        reg,
        tenant,
        j1: job_ctx(tenant, id, "j1"),
        tok: person(DESIGNER, &[]),
    }
}

/// A working row written as the owner, outside the ledger (as provisioned).
async fn seed(f: &Fx, job: Uuid, n: &str, val: Value) {
    sqlx::query("INSERT INTO scratch_item (tenant_id, job_id, name, val) VALUES ($1, $2, $3, $4)")
        .bind(f.tenant)
        .bind(job)
        .bind(n)
        .bind(val)
        .execute(&f.e.owner)
        .await
        .unwrap();
}

async fn seed_tenant(f: &Fx, n: &str, val: Value) {
    sqlx::query("INSERT INTO scratch_tenant_item (tenant_id, name, val) VALUES ($1, $2, $3)")
        .bind(f.tenant)
        .bind(n)
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

/// One tracked write of `scratch_item.<n>` = `v`, committed.
async fn write(f: &Fx, j: &JobCtx, n: &str, v: Snap) {
    let mut tx = j.begin(&f.e.state.db).await.unwrap();
    let (job, row) = (j.job_id, n.to_string());
    let w = scratch_write("scratch", n, &format!("Set {n}"));
    design::track::<(), _>(&mut tx, &f.reg, j, w, move |c| {
        Box::pin(put_row(c, job, row, v))
    })
    .await
    .expect("tracked write");
    tx.commit().await.unwrap();
}

/// One tracked tenant-wide write of `scratch_tenant_item.<n>` = `v`, committed.
async fn write_tenant(f: &Fx, n: &str, v: Snap) {
    let t = tenant_ctx(f.tenant);
    let mut tx = t.begin(&f.e.state.db).await.unwrap();
    let (tenant, row) = (f.tenant, n.to_string());
    let w = scratch_write("scratch_tenant", n, &format!("Tenant {n}"));
    design::track_tenant::<(), _>(&mut tx, &f.reg, &t, w, move |c| {
        Box::pin(put_tenant_row(c, tenant, row, v))
    })
    .await
    .expect("tracked tenant write");
    tx.commit().await.unwrap();
}

async fn working(f: &Fx, job: Uuid, n: &str) -> Option<Value> {
    sqlx::query_scalar("SELECT val FROM scratch_item WHERE job_id = $1 AND name = $2")
        .bind(job)
        .bind(n)
        .fetch_optional(&f.e.owner)
        .await
        .unwrap()
}

async fn working_tenant(f: &Fx, n: &str) -> Option<Value> {
    sqlx::query_scalar("SELECT val FROM scratch_tenant_item WHERE tenant_id = $1 AND name = $2")
        .bind(f.tenant)
        .bind(n)
        .fetch_optional(&f.e.owner)
        .await
        .unwrap()
}

#[derive(Debug, sqlx::FromRow)]
struct Ledger {
    id: i64,
    key: String,
    label: String,
    after: Option<Value>,
}

async fn ledger(f: &Fx, job: Uuid) -> Vec<Ledger> {
    sqlx::query_as::<_, Ledger>(
        "SELECT id, key, label, after FROM draft_ledger WHERE job_id = $1 ORDER BY key",
    )
    .bind(job)
    .fetch_all(&f.e.owner)
    .await
    .unwrap()
}

async fn rev(f: &Fx, job: Uuid) -> i64 {
    sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
        .bind(job)
        .fetch_one(&f.e.owner)
        .await
        .unwrap()
}

async fn audit_count(f: &Fx, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE tenant_id = $1 AND action = $2")
        .bind(f.tenant)
        .bind(action)
        .fetch_one(&f.e.owner)
        .await
        .unwrap()
}

/// A release row (§3 `job_release`), `approved_hours_ago` orders the cuts. A non-`approved` row is released.
async fn release(f: &Fx, job: Uuid, id: &str, state: &str, approved_hours_ago: i32) {
    let hash = format!("sha256:{:0>64}", id.rsplit('.').next().unwrap());
    sqlx::query(
        "INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins, \
                                  provenance, requires, causes, state, approved_by, approved_at, released_by, \
                                  released_at, snapshot_kinds) \
         VALUES ($1, $2, $3, $4, $4, '{}', '{}', '[]', '{}', '{}', '{}', $5::text, 'test', \
                 now() - make_interval(hours => $6), \
                 CASE WHEN $5::text = 'approved' THEN NULL ELSE 'test' END, \
                 CASE WHEN $5::text = 'approved' THEN NULL ELSE now() - make_interval(hours => $6) END, \
                 ARRAY['scratch', 'scratch_tenant'])",
    )
    .bind(f.tenant)
    .bind(job)
    .bind(id)
    .bind(hash)
    .bind(state)
    .bind(approved_hours_ago)
    .execute(&f.e.owner)
    .await
    .unwrap();
}

/// One stored snapshot place of a release.
async fn snapshot(f: &Fx, job: Uuid, release: &str, kind: &str, n: &str, val: Value) {
    let key = if kind == "scratch" {
        format!("scratch:{n}")
    } else {
        format!("scratch-tenant:{n}")
    };
    sqlx::query(
        "INSERT INTO release_snapshot (tenant_id, job_id, release_id, key, kind, ref, snap) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(f.tenant)
    .bind(job)
    .bind(release)
    .bind(key)
    .bind(kind)
    .bind(json!({"name": n}))
    .bind(json!({"v": val}))
    .execute(&f.e.owner)
    .await
    .unwrap();
}

async fn make_current(f: &Fx, job: Uuid, release: &str) {
    sqlx::query("UPDATE jobs SET current_release_id = $2 WHERE id = $1")
        .bind(job)
        .bind(release)
        .execute(&f.e.owner)
        .await
        .unwrap();
}

/// A ledger row bundled by a cut (`release_bundled_draft`).
async fn bundle(f: &Fx, job: Uuid, release: &str, ledger_id: i64, n: &str, val: Value) {
    sqlx::query(
        "INSERT INTO release_bundled_draft (tenant_id, job_id, release_id, key, ledger_id, kind, ref, area, page, \
                                            label, after, at, by_sub) \
         VALUES ($1, $2, $3, $4, $5, 'scratch', $6, 'scratch', 'J7', $7, $8, now(), $9)",
    )
    .bind(f.tenant)
    .bind(job)
    .bind(release)
    .bind(format!("scratch:{n}"))
    .bind(ledger_id)
    .bind(json!({"name": n}))
    .bind(format!("Set {n}"))
    .bind(json!({"v": val}))
    .bind(DESIGNER)
    .execute(&f.e.owner)
    .await
    .unwrap();
}

/// A published change (`published_change`), rolled back to `rolled_back_to` when given.
async fn published(
    f: &Fx,
    job: Uuid,
    n: &str,
    release: &str,
    after: Value,
    rolled_back_to: Option<&str>,
) -> i64 {
    let rolled = rolled_back_to.map(
        |to| json!({"to": to, "at": "2026-10-20T10:00:00Z", "by": DESIGNER, "rollback": "rb-1"}),
    );
    sqlx::query_scalar(
        "INSERT INTO published_change (tenant_id, job_id, key, kind, ref, area, page, label, before, after, at, \
                                       by_sub, published_in, published_by, rolled_back) \
         VALUES ($1, $2, $3, 'scratch', $4, 'scratch', 'J7', $5, NULL, $6, now(), $7, $8, $7, $9) RETURNING id",
    )
    .bind(f.tenant)
    .bind(job)
    .bind(format!("scratch:{n}"))
    .bind(json!({"name": n}))
    .bind(format!("Set {n} in {release}"))
    .bind(json!({"v": after}))
    .bind(DESIGNER)
    .bind(release)
    .bind(rolled)
    .fetch_one(&f.e.owner)
    .await
    .unwrap()
}

/// An open conflict carrying a decision for the draft at `draft_key`.
async fn decided_conflict(f: &Fx, job: Uuid, release: &str, draft_key: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO release_conflict (tenant_id, job_id, release_id, kind, object, key, tenant_row, detail, decision) \
         VALUES ($1, $2, $3, 'job_setting', 'job_setting', 'actor', 'override', 'x', $4) RETURNING id",
    )
    .bind(f.tenant)
    .bind(job)
    .bind(release)
    .bind(json!({"choice": "reverted", "draft_key": draft_key, "draft_id": "d-1", "by": DESIGNER,
                 "at": "2026-10-20T10:00:00Z", "published_in": null}))
    .fetch_one(&f.e.owner)
    .await
    .unwrap()
}

async fn decision(f: &Fx, id: i64) -> Option<Value> {
    sqlx::query_scalar("SELECT decision FROM release_conflict WHERE id = $1")
        .bind(id)
        .fetch_one(&f.e.owner)
        .await
        .unwrap()
}

fn uri(job: &str, tail: &str) -> String {
    format!("/api/jobs/{job}/{tail}")
}

async fn http_get(f: &Fx, u: &str) -> (StatusCode, Value) {
    send(&f.app, get(u, Some(&f.tok))).await
}

async fn http_delete(f: &Fx, u: &str) -> (StatusCode, Value) {
    send(&f.app, req("DELETE", u, Some(&f.tok), json!({}))).await
}

async fn http_restore(f: &Fx, ids: Value) -> (StatusCode, Value) {
    let u = uri("j1", "draft/restore");
    send(&f.app, req("POST", &u, Some(&f.tok), json!({"ids": ids}))).await
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

// ---- discard --------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn discard_puts_live_value_back() {
    let f = setup().await;
    let j1 = f.j1.job_id;
    seed(&f, j1, "a", json!(1)).await;
    write(&f, &f.j1, "a", Some(json!(2))).await;
    let id = ledger(&f, j1).await[0].id;

    let (st, b) = http_get(&f, &uri("j1", "draft")).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["count"], 1);
    let ch = &b["changes"][0];
    assert_eq!(ch["id"], format!("d-{id}"));
    assert_eq!(ch["key"], "scratch:a");
    assert_eq!(ch["kind"], "scratch");
    assert_eq!(ch["label"], "Set a");
    assert_eq!(ch["by"], DESIGNER);
    assert_eq!(ch["live"], "1");
    assert_eq!(ch["draft"], "2");
    assert_eq!(ch["in_release"], Value::Null);
    assert_eq!(ch["live_moved"], Value::Null);
    assert_eq!(b["pending_kinds"], json!(design::PENDING));
    let (st, diff) = http_get(&f, &uri("j1", "draft/diff")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(diff, b["changes"]);
    assert_eq!(rev(&f, j1).await, 1);

    let (st, b) = http_delete(&f, &uri("j1", &format!("draft/d-{id}"))).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["count"], 0);
    assert_eq!(
        working(&f, j1, "a").await,
        Some(json!(1)),
        "the live value is back"
    );
    assert!(ledger(&f, j1).await.is_empty());
    assert_eq!(rev(&f, j1).await, 2, "a discard changes the draft");
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE tenant_id = $1 AND action = 'job.draft_discarded' AND job = 'j1'",
    )
    .bind(f.tenant)
    .fetch_one(&f.e.owner)
    .await
    .unwrap();
    assert_eq!(details["ledger_id"], id);
    assert_eq!(details["kind"], "scratch");
    assert!(details.get("label").is_none(), "labels are never audited");

    // DELETE /draft discards every row: a place the live design does not have is removed again
    write(&f, &f.j1, "a", Some(json!(3))).await;
    write(&f, &f.j1, "b", Some(json!(5))).await;
    let (st, b) = http_delete(&f, &uri("j1", "draft")).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["count"], 0);
    assert_eq!(working(&f, j1, "a").await, Some(json!(1)));
    assert_eq!(working(&f, j1, "b").await, None);
    assert_eq!(audit_count(&f, "job.draft_discarded").await, 3);
}

#[tokio::test]
async fn discard_tenant_kind_rerecords_other_jobs() {
    let f = setup().await;
    let j1 = f.j1.job_id;
    let j2 = new_job(&f.e.owner, f.tenant, "j2").await;
    let j3 = new_job(&f.e.owner, f.tenant, "j3").await;
    seed_tenant(&f, "t", json!(1)).await;
    write_tenant(&f, "t", Some(json!(2))).await;
    for j in [j1, j2, j3] {
        assert_eq!(ledger(&f, j).await.len(), 1, "D69: every job records it");
    }
    // j2's Production already runs 2; j1 and j3 run their baseline 1
    release(&f, j2, R1, "current", 1).await;
    snapshot(&f, j2, R1, "scratch_tenant", "t", json!(2)).await;
    make_current(&f, j2, R1).await;
    let (r2, r3) = (rev(&f, j2).await, rev(&f, j3).await);

    let id = ledger(&f, j1).await[0].id;
    let (st, b) = http_delete(&f, &uri("j1", &format!("draft/d-{id}"))).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["count"], 0);
    assert_eq!(
        working_tenant(&f, "t").await,
        Some(json!(1)),
        "j1's live value is put back for the whole tenant"
    );
    assert!(ledger(&f, j1).await.is_empty());
    let l2 = ledger(&f, j2).await;
    assert_eq!(
        l2.len(),
        1,
        "j2's live differs from the draft now: re-recorded"
    );
    assert_eq!(l2[0].key, "scratch-tenant:t");
    assert_eq!(l2[0].label, "Tenant t — discarded from the draft in j1");
    assert_eq!(l2[0].after, Some(json!({"v": 1})));
    assert!(
        ledger(&f, j3).await.is_empty(),
        "j3's draft equals its live value again: the row is dropped"
    );
    assert_eq!(rev(&f, j2).await, r2 + 1);
    assert_eq!(rev(&f, j3).await, r3 + 1);
}

#[tokio::test]
async fn discard_unknown_404() {
    let f = setup().await;
    let j1 = f.j1.job_id;
    for id in ["d-999999", "nope", "d-x", "999"] {
        let (st, b) = http_delete(&f, &uri("j1", &format!("draft/{id}"))).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{id}: {b}");
        assert_eq!(code(&b), "draft_not_found");
    }
    // another job's row is not this job's draft
    let j2 = new_job(&f.e.owner, f.tenant, "j2").await;
    write(&f, &job_ctx(f.tenant, j2, "j2"), "a", Some(json!(2))).await;
    let other = ledger(&f, j2).await[0].id;
    let (st, b) = http_delete(&f, &uri("j1", &format!("draft/d-{other}"))).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(ledger(&f, j2).await.len(), 1);
    // discarding an empty draft changes nothing
    let (st, b) = http_delete(&f, &uri("j1", "draft")).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["count"], 0);
    assert_eq!(rev(&f, j1).await, 0);
    assert_eq!(audit_count(&f, "job.draft_discarded").await, 0);
    // an observer cannot discard
    insert_user(&f.e.owner, "u-obs", f.tenant, "tenant_user").await;
    let obs = person("u-obs", &[]);
    let (st, b) = send(
        &f.app,
        req("DELETE", &uri("j1", "draft"), Some(&obs), json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], "job_designer");
}

#[tokio::test]
async fn discarded_change_still_ships_with_approved_cut() {
    let f = setup().await;
    let j1 = f.j1.job_id;
    seed(&f, j1, "a", json!(1)).await;
    write(&f, &f.j1, "a", Some(json!(2))).await;
    write(&f, &f.j1, "b", Some(json!(7))).await;
    let rows = ledger(&f, j1).await;
    let a_id = rows.iter().find(|r| r.key == "scratch:a").unwrap().id;
    // an approved cut carries a (not b)
    release(&f, j1, R1, "approved", 1).await;
    snapshot(&f, j1, R1, "scratch", "a", json!(2)).await;
    snapshot(&f, j1, R1, "scratch", "b", json!(7)).await;
    bundle(&f, j1, R1, a_id, "a", json!(2)).await;
    let ca = decided_conflict(&f, j1, R1, "scratch:a").await;
    let cb = decided_conflict(&f, j1, R1, "scratch:b").await;

    let (_, b) = http_get(&f, &uri("j1", "draft")).await;
    let in_release = |key: &str| {
        b["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["key"] == key)
            .map(|c| c["in_release"].clone())
            .unwrap()
    };
    assert_eq!(in_release("scratch:a"), json!(R1));
    assert_eq!(in_release("scratch:b"), Value::Null);

    let (st, b) = http_delete(&f, &uri("j1", "draft")).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["count"], 0);
    assert_eq!(
        working(&f, j1, "a").await,
        Some(json!(1)),
        "the draft is live again"
    );
    // the cut still ships the change: its bundled row and stored value are untouched
    let bundled: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM release_bundled_draft WHERE job_id = $1 AND release_id = $2 AND key = 'scratch:a'",
    )
    .bind(j1)
    .bind(R1)
    .fetch_one(&f.e.owner)
    .await
    .unwrap();
    assert_eq!(bundled, 1);
    let mut tx = f.j1.begin(&f.e.state.db).await.unwrap();
    let shipped = design::value(
        &mut tx,
        &f.reg,
        &jref(&f.j1),
        &DesignOf::Release(R1.to_string()),
        "scratch",
        &name_ref("a"),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(shipped, Some(json!(2)));
    // J16: the cut's decision stays (without its ledger row); the discarded-only decision is gone
    let da = decision(&f, ca).await.expect("still decided");
    assert_eq!(da["choice"], "reverted");
    assert_eq!(da["draft_key"], "scratch:a");
    assert_eq!(da["draft_id"], Value::Null);
    assert_eq!(decision(&f, cb).await, None);
}

// ---- restore --------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn restore_after_rollback_rerecords_one_row_per_place() {
    let f = setup().await;
    let j1 = f.j1.job_id;
    seed(&f, j1, "a", json!(1)).await;
    // R1 → R2 (a=2) → R3 (a=3, b=5), then a rollback to R1
    release(&f, j1, R1, "current", 3).await;
    snapshot(&f, j1, R1, "scratch", "a", json!(1)).await;
    release(&f, j1, R2, "applied_earlier", 2).await;
    snapshot(&f, j1, R2, "scratch", "a", json!(2)).await;
    release(&f, j1, R3, "applied_earlier", 1).await;
    snapshot(&f, j1, R3, "scratch", "a", json!(3)).await;
    snapshot(&f, j1, R3, "scratch", "b", json!(5)).await;
    make_current(&f, j1, R1).await;
    let p1 = published(&f, j1, "a", R3, json!(30), Some(R1)).await;
    let p2 = published(&f, j1, "a", R2, json!(2), Some(R1)).await;
    let p3 = published(&f, j1, "b", R3, json!(5), Some(R1)).await;
    let live = published(&f, j1, "c", R2, json!(9), None).await;

    let (st, b) = http_restore(&f, json!("all")).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["count"], 2, "one ledger row per place");
    // a: R3 (the newer cut) wins over R2, with R3's stored value (not the row's `after`)
    assert_eq!(working(&f, j1, "a").await, Some(json!(3)));
    assert_eq!(working(&f, j1, "b").await, Some(json!(5)));
    let rows = ledger(&f, j1).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].key, "scratch:a");
    assert_eq!(
        rows[0].label,
        format!("Restored after rollback: Set a in {R3}")
    );
    assert_eq!(rows[0].after, Some(json!({"v": 3})));
    assert_eq!(rows[1].key, "scratch:b");
    let restored: Vec<(i64, bool)> = sqlx::query_as(
        "SELECT id, restored_to_draft IS NOT NULL FROM published_change WHERE job_id = $1 ORDER BY id",
    )
    .bind(j1)
    .fetch_all(&f.e.owner)
    .await
    .unwrap();
    assert_eq!(
        restored,
        vec![(p1, true), (p2, true), (p3, true), (live, false)]
    );
    assert_eq!(audit_count(&f, "job.change_restored_to_draft").await, 3);
    assert_eq!(rev(&f, j1).await, 1);

    let (st, list) = http_get(&f, &uri("j1", "draft/published")).await;
    assert_eq!(st, StatusCode::OK, "{list}");
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 4);
    let one = list
        .iter()
        .find(|p| p["id"] == format!("pub-{p2}"))
        .unwrap();
    assert_eq!(one["published_in"], R2);
    assert_eq!(one["draft"], "2");
    assert_eq!(one["live"], Value::Null);
    assert_eq!(one["in_release"], Value::Null);
    assert_eq!(one["rolled_back"]["to"], R1);
    assert_eq!(one["restored_to_draft"]["by"], DESIGNER);
    let still = list
        .iter()
        .find(|p| p["id"] == format!("pub-{live}"))
        .unwrap();
    assert_eq!(still["rolled_back"], Value::Null);
    assert_eq!(still["restored_to_draft"], Value::Null);

    let (st, b) = http_restore(&f, json!("all")).await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "nothing_to_restore");
}

#[tokio::test]
async fn nothing_to_restore_409() {
    let f = setup().await;
    let (st, b) = http_restore(&f, json!("all")).await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "nothing_to_restore");
    let (st, b) = http_restore(&f, json!([])).await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "nothing_to_restore");
    let (st, b) = http_restore(&f, json!(["pub-1"])).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "change_not_rolled_back");
    let (st, b) = http_restore(&f, json!("some")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(code(&b), "invalid_body");
    // a published change that was never rolled back is not restorable
    let j1 = f.j1.job_id;
    release(&f, j1, R1, "current", 1).await;
    make_current(&f, j1, R1).await;
    let p = published(&f, j1, "a", R1, json!(2), None).await;
    let (st, b) = http_restore(&f, json!([format!("pub-{p}")])).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "change_not_rolled_back");
    assert_eq!(rev(&f, j1).await, 0);
}

/// R1 → R2 (a=2) → R3 (a=3, b=5), rolled back to R1: `(a in R3, a in R2, b in R3)`.
async fn rolled_back_history(f: &Fx) -> (i64, i64, i64) {
    let j1 = f.j1.job_id;
    seed(f, j1, "a", json!(1)).await;
    release(f, j1, R1, "current", 3).await;
    snapshot(f, j1, R1, "scratch", "a", json!(1)).await;
    release(f, j1, R2, "applied_earlier", 2).await;
    snapshot(f, j1, R2, "scratch", "a", json!(2)).await;
    release(f, j1, R3, "applied_earlier", 1).await;
    snapshot(f, j1, R3, "scratch", "a", json!(3)).await;
    snapshot(f, j1, R3, "scratch", "b", json!(5)).await;
    make_current(f, j1, R1).await;
    let p1 = published(f, j1, "a", R3, json!(3), Some(R1)).await;
    let p2 = published(f, j1, "a", R2, json!(2), Some(R1)).await;
    let p3 = published(f, j1, "b", R3, json!(5), Some(R1)).await;
    (p1, p2, p3)
}

async fn restored_ids(f: &Fx, job: Uuid) -> Vec<i64> {
    sqlx::query_scalar(
        "SELECT id FROM published_change WHERE job_id = $1 AND restored_to_draft IS NOT NULL ORDER BY id",
    )
    .bind(job)
    .fetch_all(&f.e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn restore_by_ids_restores_only_the_named_changes_and_cuts_long_labels() {
    let f = setup().await;
    let j1 = f.j1.job_id;
    let (p1, p2, p3) = rolled_back_history(&f).await;

    // only R2's change of a, with R2's stored value (R3's newer change of a is not named)
    let (st, b) = http_restore(&f, json!([format!("pub-{p2}"), format!("pub-{p2}")])).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["count"], 1);
    assert_eq!(working(&f, j1, "a").await, Some(json!(2)));
    assert_eq!(working(&f, j1, "b").await, None, "b is not named");
    let rows = ledger(&f, j1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key, "scratch:a");
    assert_eq!(rows[0].after, Some(json!({"v": 2})));
    assert_eq!(restored_ids(&f, j1).await, vec![p2]);
    assert_eq!(audit_count(&f, "job.change_restored_to_draft").await, 1);
    assert_eq!(rev(&f, j1).await, 1);
    // a restored change is not restorable again
    let (st, b) = http_restore(&f, json!([format!("pub-{p2}")])).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "change_not_rolled_back");

    // the ledger label (prefix + published label) is cut at 500 characters
    sqlx::query("UPDATE published_change SET label = repeat('é', 490) WHERE id = $1")
        .bind(p3)
        .execute(&f.e.owner)
        .await
        .unwrap();
    let (st, b) = http_restore(&f, json!([format!("pub-{p3}")])).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let len: i32 = sqlx::query_scalar(
        "SELECT char_length(label) FROM draft_ledger WHERE job_id = $1 AND key = 'scratch:b'",
    )
    .bind(j1)
    .fetch_one(&f.e.owner)
    .await
    .unwrap();
    assert_eq!(len, 500);
    assert_eq!(working(&f, j1, "b").await, Some(json!(5)));
    assert_eq!(restored_ids(&f, j1).await, vec![p2, p3]);
    assert!(!restored_ids(&f, j1).await.contains(&p1));
}

#[tokio::test]
async fn restore_tenant_kind_rerecords_every_job() {
    let f = setup().await;
    let j1 = f.j1.job_id;
    let j2 = new_job(&f.e.owner, f.tenant, "j2").await;
    seed_tenant(&f, "t", json!(1)).await;
    // j1: R1 (t=1) current after rolling back R2 (t=2); j2 never released (baseline t=1)
    release(&f, j1, R1, "current", 2).await;
    snapshot(&f, j1, R1, "scratch_tenant", "t", json!(1)).await;
    release(&f, j1, R2, "applied_earlier", 1).await;
    snapshot(&f, j1, R2, "scratch_tenant", "t", json!(2)).await;
    make_current(&f, j1, R1).await;
    let p = published(&f, j1, "t", R2, json!(20), Some(R1)).await;
    sqlx::query(
        "UPDATE published_change SET kind = 'scratch_tenant', key = 'scratch-tenant:t' WHERE id = $1",
    )
    .bind(p)
    .execute(&f.e.owner)
    .await
    .unwrap();
    let (r1, r2) = (rev(&f, j1).await, rev(&f, j2).await);

    let (st, b) = http_restore(&f, json!("all")).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["count"], 1);
    assert_eq!(
        working_tenant(&f, "t").await,
        Some(json!(2)),
        "R2's stored value, for the whole tenant"
    );
    for j in [j1, j2] {
        let l = ledger(&f, j).await;
        assert_eq!(l.len(), 1, "D69: every job records it");
        assert_eq!(l[0].key, "scratch-tenant:t");
        assert_eq!(l[0].after, Some(json!({"v": 2})));
        assert!(l[0].label.starts_with("Restored after rollback: "));
    }
    assert_eq!(rev(&f, j1).await, r1 + 1);
    assert_eq!(rev(&f, j2).await, r2 + 1);
    assert_eq!(restored_ids(&f, j1).await, vec![p]);
}

// ---- lock order -----------------------------------------------------------------------------------------------------

/// Backends of this database (not this one) waiting on a lock.
async fn lock_waiters(db: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity \
          WHERE datname = current_database() AND wait_event_type = 'Lock' AND pid <> pg_backend_pid()",
    )
    .fetch_one(db)
    .await
    .unwrap()
}

/// Wait (bounded, never a fixed sleep) until at least `n` backends wait on a lock.
async fn until_waiting(db: &PgPool, n: i64) {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while lock_waiters(db).await < n {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never saw {n} lock waiter(s)"));
}

/// T14 I1: discard and restore lock every job of the tenant in id order BEFORE `ensure_job` takes their own job's row
/// on first touch (a job never touched by B2). The old order (own job first, then the tenant's jobs) held the later
/// job while waiting on an earlier one: against a concurrent `track_tenant` that is a 40P01. Deterministic: a held
/// lock on the tenant's first job (j1, uuidv7) pauses the call; its own job row must still be free (NOWAIT).
#[tokio::test]
async fn discard_and_restore_lock_tenant_jobs_before_their_own_job() {
    let f = setup().await;
    let j1 = f.j1.job_id;
    for (key, restore) in [("j2", false), ("j3", true)] {
        let own = new_job(&f.e.owner, f.tenant, key).await;
        assert!(own > j1, "uuidv7: a later job sorts after j1");
        let mut hold = f.e.owner.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
            .bind(j1)
            .execute(&mut *hold)
            .await
            .unwrap();
        let (app, tok) = (f.app.clone(), f.tok.clone());
        let call = tokio::spawn(async move {
            let r = if restore {
                req(
                    "POST",
                    &uri(key, "draft/restore"),
                    Some(&tok),
                    json!({"ids": "all"}),
                )
            } else {
                req("DELETE", &uri(key, "draft"), Some(&tok), json!({}))
            };
            send(&app, r).await
        });
        // the call waits on j1, the first job of the tenant ...
        until_waiting(&f.e.owner, 1).await;
        // ... without holding its own job row
        let free = sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE NOWAIT")
            .bind(own)
            .execute(&f.e.owner)
            .await;
        assert!(
            free.is_ok(),
            "{key}: waits on j1 while holding its own job row: {free:?}"
        );
        hold.commit().await.unwrap();
        let (st, b) = tokio::time::timeout(std::time::Duration::from_secs(20), call)
            .await
            .expect("call timed out")
            .unwrap();
        if restore {
            assert_eq!(st, StatusCode::CONFLICT, "{b}");
            assert_eq!(code(&b), "nothing_to_restore");
        } else {
            assert_eq!(st, StatusCode::OK, "{b}");
            assert_eq!(b["count"], 0);
            let settled: bool = sqlx::query_scalar(
                "SELECT baseline_kinds IS NOT NULL OR current_release_id IS NOT NULL FROM jobs WHERE id = $1",
            )
            .bind(own)
            .fetch_one(&f.e.owner)
            .await
            .unwrap();
            assert!(settled, "discard still settles a job B2 never touched");
        }
    }
}

// ---- design reads ---------------------------------------------------------------------------------------------------

#[tokio::test]
async fn tenant_design_diff_lists_only_jobs_whose_live_differs() {
    let f = setup().await;
    let j1 = f.j1.job_id;
    new_job(&f.e.owner, f.tenant, "j2").await;
    seed_tenant(&f, "t", json!(1)).await;
    write_tenant(&f, "t", Some(json!(2))).await;
    // j1 published it; j2 still runs its baseline
    release(&f, j1, R1, "current", 1).await;
    snapshot(&f, j1, R1, "scratch_tenant", "t", json!(2)).await;
    make_current(&f, j1, R1).await;

    let (st, b) = http_get(&f, "/api/design/tenant-diff").await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let rows = b["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{b}");
    assert_eq!(rows[0]["key"], "scratch-tenant:t");
    assert_eq!(rows[0]["kind"], "scratch_tenant");
    assert_eq!(rows[0]["ref"], json!({"name": "t"}));
    assert_eq!(rows[0]["draft"], "2");
    let jobs = rows[0]["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1, "only the job whose live value differs");
    assert_eq!(jobs[0]["job"], "j2");
    assert_eq!(jobs[0]["job_label"], "J2");
    assert_eq!(jobs[0]["live"], "1");
    assert_eq!(jobs[0]["changes"].as_array().unwrap().len(), 1);
    assert_eq!(jobs[0]["changes"][0]["by"], DESIGNER);

    let (_, same) = http_get(&f, "/api/design/tenant-diff?kinds=scratch_tenant,bogus").await;
    assert_eq!(same, b);
    let (_, none) = http_get(&f, "/api/design/tenant-diff?kinds=scratch").await;
    assert_eq!(none, json!({"rows": []}), "a job kind is not tenant-wide");
}

#[tokio::test]
async fn design_diff_never_reads_ledger() {
    let f = setup().await;
    let j1 = f.j1.job_id;
    seed(&f, j1, "a", json!(1)).await;
    // z: draft == live (in the baseline the first write takes)
    seed(&f, j1, "z", json!(4)).await;
    write(&f, &f.j1, "a", Some(json!(2))).await;

    let (st, d1) = http_get(&f, &uri("j1", "design/diff")).await;
    assert_eq!(st, StatusCode::OK, "{d1}");
    assert_eq!(d1["release_id"], Value::Null);
    assert_eq!(d1["draft_rev"], 1);
    let rows = d1["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["key"], "scratch:a");
    assert_eq!(rows[0]["live"], "1");
    assert_eq!(rows[0]["draft"], "2");
    assert_eq!(rows[0]["changes"][0]["label"], "Set a");

    sqlx::query(
        "UPDATE draft_ledger SET before = '{\"v\": 98}', after = '{\"v\": 99}' WHERE job_id = $1",
    )
    .bind(j1)
    .execute(&f.e.owner)
    .await
    .unwrap();
    let (_, d2) = http_get(&f, &uri("j1", "design/diff")).await;
    assert_eq!(
        d2, d1,
        "the diff is working rows vs the snapshot, never the ledger"
    );
    // a ledger row at a place whose draft equals live does not make the place a diff row
    sqlx::query(
        "INSERT INTO draft_ledger (tenant_id, job_id, key, kind, ref, area, page, label, before, after, by_sub) \
         VALUES ($1, $2, 'scratch:z', 'scratch', '{\"name\": \"z\"}', 'scratch', 'J7', 'Set z', \
                 '{\"v\": 97}', '{\"v\": 96}', $3)",
    )
    .bind(f.tenant)
    .bind(j1)
    .bind(DESIGNER)
    .execute(&f.e.owner)
    .await
    .unwrap();
    let (_, d3) = http_get(&f, &uri("j1", "design/diff")).await;
    let keys: Vec<&Value> = d3["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["key"])
        .collect();
    assert_eq!(
        keys,
        vec![&json!("scratch:a")],
        "places come from the design, not the ledger: {d3}"
    );
    // no ledger at all: the place still differs, it just has no changes
    sqlx::query("DELETE FROM draft_ledger WHERE job_id = $1")
        .bind(j1)
        .execute(&f.e.owner)
        .await
        .unwrap();
    let (st, d4) = http_get(&f, &uri("j1", "design/diff")).await;
    assert_eq!(st, StatusCode::OK, "{d4}");
    let rows = d4["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{d4}");
    assert_eq!(rows[0]["key"], "scratch:a");
    assert_eq!(rows[0]["live"], "1");
    assert_eq!(rows[0]["draft"], "2");
    assert_eq!(rows[0]["changes"], json!([]));

    let (st, live) = http_get(&f, &uri("j1", "design/live")).await;
    assert_eq!(st, StatusCode::OK, "{live}");
    assert_eq!(live["of"], "live");
    assert_eq!(live["release_id"], Value::Null);
    assert_eq!(live["draft_rev"], Value::Null);
    let text_of = |state: &Value, key: &str| {
        state["values"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["key"] == key)
            .map(|v| v["text"].clone())
    };
    assert_eq!(live["values"].as_array().unwrap().len(), 2, "{live}");
    assert_eq!(
        live["values"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["key"] == "scratch:a")
            .cloned(),
        Some(json!({"key": "scratch:a", "kind": "scratch", "ref": {"name": "a"}, "text": "1"}))
    );
    assert_eq!(text_of(&live, "scratch:z"), Some(json!("4")));
    let (st, draft) = http_get(&f, &uri("j1", "design/draft")).await;
    assert_eq!(st, StatusCode::OK, "{draft}");
    assert_eq!(draft["of"], "draft");
    assert_eq!(draft["draft_rev"], 1);
    assert_eq!(text_of(&draft, "scratch:a"), Some(json!("2")));
    assert_eq!(text_of(&draft, "scratch:z"), Some(json!("4")));
}

// ---- cross-plan helpers (§2.7) --------------------------------------------------------------------------------------

/// `design::lifecycle` of `keys` and `ledger::keys`, read in one job-scoped transaction.
async fn lifecycle_of(f: &Fx, keys: &[String]) -> (KeysLifecycle, Vec<String>) {
    let mut tx = f.j1.begin(&f.e.state.db).await.unwrap();
    let s = design::lifecycle(&mut tx, &jref(&f.j1), keys)
        .await
        .unwrap();
    let k = design::ledger::keys(&mut tx, &jref(&f.j1)).await.unwrap();
    tx.commit().await.unwrap();
    (s, k)
}

#[tokio::test]
async fn lifecycle_draft_in_release_live_discarded() {
    let f = setup().await;
    let j1 = f.j1.job_id;
    let keys = ["scratch:a".to_string()];
    assert_eq!(
        lifecycle_of(&f, &keys).await,
        (KeysLifecycle::Discarded, vec![])
    );

    seed(&f, j1, "a", json!(1)).await;
    write(&f, &f.j1, "a", Some(json!(2))).await;
    let (s, k) = lifecycle_of(&f, &keys).await;
    assert_eq!(s, KeysLifecycle::Draft);
    assert_eq!(k, vec![r#"scratch:a@{"v":2}"#.to_string()]);
    assert_eq!(design::fingerprint(&k).len(), 7);

    let id = ledger(&f, j1).await[0].id;
    release(&f, j1, R1, "approved", 1).await;
    bundle(&f, j1, R1, id, "a", json!(2)).await;
    assert_eq!(
        lifecycle_of(&f, &keys).await.0,
        KeysLifecycle::InRelease(R1.to_string())
    );

    // published: no ledger row left, the newest published change names the release
    sqlx::query("DELETE FROM draft_ledger WHERE job_id = $1")
        .bind(j1)
        .execute(&f.e.owner)
        .await
        .unwrap();
    published(&f, j1, "a", R1, json!(2), None).await;
    assert_eq!(
        lifecycle_of(&f, &keys).await,
        (KeysLifecycle::Live(R1.to_string()), vec![])
    );
}
