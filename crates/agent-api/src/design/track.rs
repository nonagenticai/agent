//! The draft-write API (spec B2 §2.3, D54): every tenant design write goes through [`track`] (job kinds) or
//! [`track_tenant`] (the D69 tenant-wide kinds). Nothing a tenant changes is live on save: the write lands in the
//! working rows (the draft) and the draft ledger records it. A port of `areas/drafts.ts` `track` / `trackTenant`
//! with `fixtures/ledger.ts` `recordLedger` / `recordTenantLedger`.
//!
//! # The `mutate` closure (controller ruling R1)
//!
//! `mutate` is NOT an `AsyncFnOnce`: the Send-ness of an async closure's future across a higher-ranked borrow is a
//! known compile trap. It is a plain closure returning a boxed future, the shape of sqlx's own
//! `Connection::transaction`:
//!
//! ```ignore
//! let out = design::track(&mut tx, &st.design, &job, write, move |c| {
//!     Box::pin(async move {
//!         sqlx::query("UPDATE … WHERE job_id = $1").bind(job_id).execute(&mut *c).await?;
//!         Ok(())
//!     })
//! })
//! .await?;
//! ```
//!
//! The future borrows only the connection it is handed (`for<'c> … BoxFuture<'c, _>`), so the closure must OWN what
//! it uses: `move` copies/clones of ids and values, never a borrow of a local. It runs inside the caller's
//! transaction; an `Err` from it returns before anything is recorded (the handler's transaction then rolls back).
use futures_util::future::BoxFuture;
use sqlx::PgConnection;
use uuid::Uuid;

use super::hooks::ensure_job;
use super::ledger::{self, Entry, LedgerRow};
use super::snapshot::{internal, job_not_found};
use super::{DraftRef, JobRef, Place, Registry, Scope, Snap};
use crate::audit::{self, Event};
use crate::error::ApiResult;
use crate::runtime::design::nudge_on_commit;
use crate::runtime::scope::Env as NudgeEnv;
use crate::tenancy::{JobCtx, TenantCtx};

/// One design write: where it is and how a reader names it.
pub struct DraftWrite {
    /// Design area: `"gates"`, `"rules"`, `"routes"`, … (the audit resource type).
    pub area: &'static str,
    /// Owning page id: `"J7"`.
    pub page: &'static str,
    /// A registered kind.
    pub kind: &'static str,
    pub r#ref: DraftRef,
    /// One line a reader understands. Never audited (it can carry text a person typed).
    pub label: Label,
}

/// A ledger label: given, or computed from the new value after `mutate` ran (fixture `label: () => string`).
pub enum Label {
    Now(String),
    After(Box<dyn FnOnce(&Snap) -> String + Send>),
}

impl Label {
    fn render(self, after: &Snap) -> String {
        match self {
            Label::Now(s) => s,
            Label::After(f) => f(after),
        }
    }
}

/// What a tracked write returns: `mutate`'s value, the ledger row (`None` = the write put the live value back and
/// the row was dropped) and the job's `draft_rev` after the write.
pub struct Tracked<T> {
    pub value: T,
    pub change: Option<LedgerRow>,
    pub draft_rev: i64,
}

/// The job's draft identity (D63, spec §2.7): bumped by every tracked write that changes a value.
pub async fn draft_rev(c: &mut PgConnection, job: &JobRef) -> ApiResult<i64> {
    sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
        .bind(job.id)
        .fetch_optional(&mut *c)
        .await?
        .ok_or_else(job_not_found)
}

async fn bump_job(c: &mut PgConnection, job: Uuid) -> ApiResult<i64> {
    Ok(sqlx::query_scalar(
        "UPDATE jobs SET draft_rev = draft_rev + 1 WHERE id = $1 RETURNING draft_rev",
    )
    .bind(job)
    .fetch_one(&mut *c)
    .await?)
}

/// Run one job-design write and record it (§2.3, in order, all in the caller's transaction `tx`, which must be
/// scoped to the job's tenant):
///
/// 1. lock the job row (`FOR UPDATE`: serialises with cut/apply/discard);
/// 2. [`ensure_job`] — a job never released gets its `job_target` row and its baseline BEFORE the write (a job
///    created before B2 included, so its template default selection is never lost);
/// 3. `before` = the target's value at the place;
/// 4. `mutate(tx)`; an `Err` returns at once and nothing is recorded;
/// 5. record the ledger row (dropped when the draft is the live value again and no approved cut bundles it);
/// 6. bump `jobs.draft_rev` when `proj(before) != proj(after)` (saved tests go stale, D63) and queue the job's
///    `test` config nudge, sent when the caller commits (B4: crate::runtime::design::nudge_on_commit);
/// 7. audit `job.draft_change` (details: kind, page, ledger_id — never the label).
pub async fn track<T, F>(
    tx: &mut PgConnection,
    reg: &Registry,
    job: &JobCtx,
    w: DraftWrite,
    mutate: F,
) -> ApiResult<Tracked<T>>
where
    T: Send,
    F: for<'c> FnOnce(&'c mut PgConnection) -> BoxFuture<'c, ApiResult<T>> + Send,
{
    let t = reg.target(w.kind)?;
    if t.scope() != Scope::Job {
        return Err(internal(format!(
            "design kind {} is tenant-wide: write it with track_tenant",
            w.kind
        )));
    }
    let jr = JobRef {
        id: job.job_id,
        key: job.job_key.clone(),
    };
    let tenant = job.t.tenant;
    let rev: i64 = sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(jr.id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(job_not_found)?;
    ensure_job(&mut *tx, reg, &jr).await?;
    let at = Place {
        tenant,
        job: Some(&jr),
    };
    let before = t.get(&mut *tx, at, &w.r#ref).await?;
    let value = mutate(&mut *tx).await?;
    let after = t.get(&mut *tx, at, &w.r#ref).await?;
    let label = w.label.render(&after);
    let actor = job.t.actor();
    let entry = Entry {
        area: w.area,
        page: w.page,
        kind: w.kind,
        r#ref: &w.r#ref,
        label: &label,
        by_sub: &actor.id,
    };
    let change = ledger::record(&mut *tx, reg, tenant, &jr, &entry, &before).await?;
    let draft_rev = if t.proj(&before) != t.proj(&after) {
        let bumped = bump_job(&mut *tx, jr.id).await?;
        // B4 §0 R7: the job's test scope re-reads its draft once this transaction commits
        nudge_on_commit(&mut *tx, &[jr.id], NudgeEnv::Test).await?;
        bumped
    } else {
        rev
    };
    let key = t.key(&w.r#ref);
    audit::record(
        &mut *tx,
        &actor,
        Event::new("job.draft_change", w.area, key)
            .job(&jr.key)
            .detail("kind", w.kind)
            .detail("page", w.page)
            .detail("ledger_id", change.as_ref().map(|r| r.id)),
    )
    .await?;
    Ok(Tracked {
        value,
        change,
        draft_rev,
    })
}

#[derive(sqlx::FromRow)]
struct LockedJob {
    id: Uuid,
    key: String,
    draft_rev: i64,
}

/// Run one TENANT-WIDE design write (D69 kinds, `scope() == Tenant`, else 500) and record it in the ledger of every
/// job of the tenant: each job's Production gets it when that job publishes.
///
/// 1. lock every job of the tenant `ORDER BY id` (fixed lock order), then [`ensure_job`] each (target row + baseline);
/// 2. `before` at `Place { job: None }`, then `mutate` once;
/// 3. record the ledger row of each job (each against its own live value);
/// 4. bump `draft_rev` on every job locked in step 1 when the value changed, queueing each one's `test` nudge;
/// 5. audit one `tenant.draft_change`.
///
/// `change` = the first job's (by id) recorded row; `draft_rev` = the highest over the tenant's jobs (0 without
/// jobs).
pub async fn track_tenant<T, F>(
    tx: &mut PgConnection,
    reg: &Registry,
    tenant: &TenantCtx,
    w: DraftWrite,
    mutate: F,
) -> ApiResult<Tracked<T>>
where
    T: Send,
    F: for<'c> FnOnce(&'c mut PgConnection) -> BoxFuture<'c, ApiResult<T>> + Send,
{
    let t = reg.target(w.kind)?;
    if t.scope() != Scope::Tenant {
        return Err(internal(format!(
            "design kind {} is job-scoped: write it with track",
            w.kind
        )));
    }
    let jobs = sqlx::query_as::<_, LockedJob>(
        "SELECT id, key, draft_rev FROM jobs WHERE tenant_id = $1 ORDER BY id FOR UPDATE",
    )
    .bind(tenant.tenant)
    .fetch_all(&mut *tx)
    .await?;
    let refs: Vec<JobRef> = jobs
        .iter()
        .map(|j| JobRef {
            id: j.id,
            key: j.key.clone(),
        })
        .collect();
    for jr in &refs {
        ensure_job(&mut *tx, reg, jr).await?;
    }
    let at = Place {
        tenant: tenant.tenant,
        job: None,
    };
    let before = t.get(&mut *tx, at, &w.r#ref).await?;
    let value = mutate(&mut *tx).await?;
    let after = t.get(&mut *tx, at, &w.r#ref).await?;
    let label = w.label.render(&after);
    let actor = tenant.actor();
    let entry = Entry {
        area: w.area,
        page: w.page,
        kind: w.kind,
        r#ref: &w.r#ref,
        label: &label,
        by_sub: &actor.id,
    };
    let mut change = None;
    for jr in &refs {
        let row = ledger::record(&mut *tx, reg, tenant.tenant, jr, &entry, &before).await?;
        change = change.or(row);
    }
    let draft_rev = if t.proj(&before) != t.proj(&after) {
        // only the jobs locked above: a job committed after the lock has no baseline and no ledger row here
        let locked: Vec<Uuid> = refs.iter().map(|j| j.id).collect();
        let bumped: Option<i64> = sqlx::query_scalar(
            "WITH b AS (UPDATE jobs SET draft_rev = draft_rev + 1 WHERE id = ANY($1) RETURNING draft_rev) \
             SELECT max(draft_rev) FROM b",
        )
        .bind(&locked)
        .fetch_one(&mut *tx)
        .await?;
        // B4 §0 R7: every bumped job's test scope re-reads its draft once this transaction commits
        nudge_on_commit(&mut *tx, &locked, NudgeEnv::Test).await?;
        bumped.unwrap_or(0)
    } else {
        jobs.iter().map(|j| j.draft_rev).max().unwrap_or(0)
    };
    let key = t.key(&w.r#ref);
    audit::record(
        &mut *tx,
        &actor,
        Event::new("tenant.draft_change", w.area, key)
            .detail("kind", w.kind)
            .detail("page", w.page)
            .detail("ledger_id", change.as_ref().map(|r| r.id)),
    )
    .await?;
    Ok(Tracked {
        value,
        change,
        draft_rev,
    })
}
