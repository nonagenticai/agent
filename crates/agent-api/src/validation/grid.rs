//! The validation grid per release on approve (F-06, B8-9; B8 T12a).
//!
//! - [`OnApprove`] is B2's `release::hooks::ReleaseApproved`: after a person's approve COMMITs, it inserts one
//!   `validation_grid_run {trigger: approve}` (0 live questions → `skipped_no_questions`, no task; an active run of
//!   that release → nothing), audits `validation.grid_queued` and spawns one task. The approve never waits for it and
//!   a failure never reaches it (B2 logs `release_hook_failed`).
//! - The task ([`run_grid`]) grades the cut in chunks of [`CHUNK`] questions (concurrency 8, 20 s per question, no
//!   total budget: no request waits), storing each chunk and `done`, then ends `done` | `failed` and audits
//!   `validation.grid_finished`. Every write is its own job-scoped transaction; it takes no lock apply takes.
//! - Every run holds a lease (0190, [`crate::lease`]): it claims the row first, renews after every chunk and releases
//!   at the end, so a second replica (or a sweep) never runs the same grid.
//! - Boot ([`resume`]) continues the `queued` / `running` rows without a live lease only. **No CronJob, no timer, no re-run on a
//!   schedule**: nothing here creates a grid run except a person's approve or `runValidationCut`.
//!
//! [`install`] registers the hook and resumes at boot (main.rs).
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::cuts::{self, RunCtx, begin_job};
use super::judge::Limits;
use crate::audit::{self, Actor, Event};
use crate::design::JobRef;
use crate::error::{ApiError, ApiResult};
use crate::generator::run::iso;
use crate::release::hooks::{Approved, ReleaseApproved};
use crate::state::AppState;

/// Questions graded (and stored) per step of a grid run.
pub const CHUNK: usize = 8;

/// The hook B8 registers on `AppState.release_approved`. Holds the state (strata, Laya, metering config); the hook
/// list lives as long as the state, so the cycle is the process lifetime.
pub struct OnApprove {
    pub st: AppState,
}

/// Register [`OnApprove`] and resume the active grid runs (boot). Returns the resume task.
pub fn install(st: &AppState) -> JoinHandle<()> {
    st.release_approved
        .register_approved(Arc::new(OnApprove { st: st.clone() }));
    let st = st.clone();
    tokio::spawn(async move {
        if let Err(e) = resume(&st).await {
            tracing::error!(error = ?e, "validation_grid_resume_failed");
        }
    })
}

impl ReleaseApproved for OnApprove {
    fn name(&self) -> &'static str {
        "validation_grid"
    }

    fn on_release_approved<'a>(
        &'a self,
        _db: &'a PgPool,
        approved: &'a Approved,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            if let Some(id) = queue(&self.st, approved).await? {
                spawn(self.st.clone(), approved.tenant, approved.job.id, id);
            }
            Ok(())
        })
    }
}

/// Insert the approve's grid run (see the module doc); `Some(id)` = queued, a task should run it.
pub async fn queue(st: &AppState, a: &Approved) -> ApiResult<Option<Uuid>> {
    let mut tx = begin_job(&st.db, a.tenant, a.job.id).await?;
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM validation_question WHERE job_id = $1 AND deleted_at IS NULL",
    )
    .bind(a.job.id)
    .fetch_one(&mut *tx)
    .await?;
    let (status, finished) = if live == 0 {
        ("skipped_no_questions", true)
    } else {
        ("queued", false)
    };
    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO validation_grid_run (job_id, release_id, trigger, status, total, by, finished_at) \
         VALUES ($1, $2, 'approve', $3, $4, $5, CASE WHEN $6 THEN now() END) \
         ON CONFLICT (tenant_id, job_id, release_id) WHERE status IN ('queued', 'running') DO NOTHING \
         RETURNING id",
    )
    .bind(a.job.id)
    .bind(&a.release_id)
    .bind(status)
    .bind(i32::try_from(live).unwrap_or(i32::MAX))
    .bind(&a.by.id)
    .bind(finished)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(id) = id else {
        // a run of this release is already active: it grades the same questions
        tx.commit().await?;
        return Ok(None);
    };
    audit::record(
        &mut tx,
        &a.by,
        Event::new(
            "validation.grid_queued",
            "validation_grid_run",
            id.to_string(),
        )
        .job(&a.job.key)
        .detail("release", a.release_id.clone())
        .detail("status", status)
        .detail("questions", live),
    )
    .await?;
    tx.commit().await?;
    Ok((!finished).then_some(id))
}

/// Run grid `id` on its own task (it claims the row first; a row another replica holds is left alone).
pub fn spawn(st: AppState, tenant: Uuid, job: Uuid, id: Uuid) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run_grid(&st, tenant, job, id).await {
            tracing::warn!(grid = %id, code = e.code(), "validation_grid_failed");
        }
    })
}

#[derive(sqlx::FromRow)]
struct GridRow {
    release_id: String,
    status: String,
    by: String,
    job_key: String,
}

/// The grid's tenant-log actor: the person whose approve caused it.
fn actor(by: &str) -> Actor {
    Actor::member(by, false)
}

/// How a leased grid run ended for this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridOutcome {
    /// This process held the lease and ran the grid (or found it no longer active).
    Ran,
    /// Another owner holds the lease (the claim was refused), or this one lost it mid-run.
    HeldElsewhere,
}

/// Grade one grid run to its end (see the module doc) as this process ([`crate::lease::owner`]).
pub async fn run_grid(st: &AppState, tenant: Uuid, job: Uuid, id: Uuid) -> ApiResult<()> {
    run_grid_as(st, tenant, job, id, crate::lease::owner())
        .await
        .map(|_| ())
}

/// [`run_grid`] as `owner`: claim the row (refused → [`GridOutcome::HeldElsewhere`], nothing done), run it, and
/// always release the lease afterwards (a no-op once another owner has it).
pub async fn run_grid_as(
    st: &AppState,
    tenant: Uuid,
    job: Uuid,
    id: Uuid,
    owner: &str,
) -> ApiResult<GridOutcome> {
    let granted: bool = sqlx::query_scalar("SELECT validation_grid_claim($1, $2, $3)")
        .bind(id)
        .bind(owner)
        .bind(crate::lease::TTL_SECS)
        .fetch_one(&st.db)
        .await?;
    if !granted {
        return Ok(GridOutcome::HeldElsewhere);
    }
    let out = run_leased(st, tenant, job, id, owner).await;
    if let Err(e) = sqlx::query("SELECT validation_grid_release($1, $2)")
        .bind(id)
        .bind(owner)
        .execute(&st.db)
        .await
    {
        tracing::warn!(grid = %id, error = ?e, "validation_grid_release_failed");
    }
    out
}

/// The run itself, once `owner` holds the lease. A run that is no longer active is left alone.
async fn run_leased(
    st: &AppState,
    tenant: Uuid,
    job: Uuid,
    id: Uuid,
    owner: &str,
) -> ApiResult<GridOutcome> {
    let mut tx = begin_job(&st.db, tenant, job).await?;
    let row: Option<GridRow> = sqlx::query_as(
        "SELECT g.release_id, g.status, g.by, j.key AS job_key FROM validation_grid_run g \
           JOIN jobs j ON j.tenant_id = g.tenant_id AND j.id = g.job_id \
          WHERE g.id = $1 FOR UPDATE OF g",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(g) = row.filter(|g| g.status == "queued" || g.status == "running") else {
        tx.commit().await?;
        return Ok(GridOutcome::Ran);
    };
    let rel = cuts::release(&mut tx, job, &g.release_id).await?;
    let rows = cuts::rows(&mut tx, job, &rel).await?;
    let questions = cuts::questions_of(&rows);
    sqlx::query(
        "UPDATE validation_grid_run SET status = 'running', started_at = coalesce(started_at, now()), \
                done = 0, total = $2 WHERE id = $1",
    )
    .bind(id)
    .bind(i32::try_from(questions.len()).unwrap_or(i32::MAX))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let ctx = RunCtx {
        tenant,
        job: JobRef {
            id: job,
            key: g.job_key.clone(),
        },
        release_id: g.release_id.clone(),
        by: g.by.clone(),
    };
    let result = match grade_in_chunks(st, &ctx, id, owner, &questions).await {
        // the lease was lost mid-run: its new holder owns the row, write nothing
        Ok(None) => return Ok(GridOutcome::HeldElsewhere),
        Ok(Some(graded)) => Ok(graded),
        Err(e) => Err(e),
    };
    let (status, code): (&str, Option<String>) = match &result {
        Ok(_) => ("done", None),
        Err(e) => ("failed", Some(e.code().to_string())),
    };
    let mut tx = begin_job(&st.db, tenant, job).await?;
    sqlx::query(
        "UPDATE validation_grid_run SET status = $2, error_code = $3, finished_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(&code)
    .execute(&mut *tx)
    .await?;
    let graded = result.as_ref().copied().unwrap_or(0);
    audit::record(
        &mut tx,
        &actor(&g.by),
        Event::new(
            "validation.grid_finished",
            "validation_grid_run",
            id.to_string(),
        )
        .job(&g.job_key)
        .detail("release", g.release_id.clone())
        .detail("status", status)
        .detail("graded", graded)
        .detail("total", questions.len()),
    )
    .await?;
    tx.commit().await?;
    result.map(|_| GridOutcome::Ran)
}

/// Grade and store [`CHUNK`] questions at a time, bumping `done` and renewing the lease; returns the graded count,
/// or `None` when the lease was lost (another owner holds the run: stop).
async fn grade_in_chunks(
    st: &AppState,
    ctx: &RunCtx,
    id: Uuid,
    owner: &str,
    questions: &[super::judge::Question],
) -> ApiResult<Option<usize>> {
    let (answerer, judge) = cuts::upstreams(st, ctx).await?;
    let mut graded = 0usize;
    let mut done = 0usize;
    for chunk in questions.chunks(CHUNK) {
        let counts = cuts::run_cut(
            st,
            ctx,
            chunk,
            answerer.clone(),
            judge.clone(),
            Limits::grid(),
        )
        .await?;
        graded += counts.graded;
        done += chunk.len();
        let mut tx = begin_job(&st.db, ctx.tenant, ctx.job.id).await?;
        sqlx::query("UPDATE validation_grid_run SET done = $2 WHERE id = $1")
            .bind(id)
            .bind(i32::try_from(done).unwrap_or(i32::MAX))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        let held: bool = sqlx::query_scalar("SELECT validation_grid_renew($1, $2, $3)")
            .bind(id)
            .bind(owner)
            .bind(crate::lease::TTL_SECS)
            .fetch_one(&st.db)
            .await?;
        if !held {
            return Ok(None);
        }
    }
    Ok(Some(graded))
}

/// Boot and sweep: continue every `queued` / `running` grid run no live lease holds (never creates one), as this
/// process.
pub async fn resume(st: &AppState) -> ApiResult<usize> {
    resume_as(st, crate::lease::owner()).await
}

/// [`resume`] as `owner`: each spawned run claims its row under that owner.
pub async fn resume_as(st: &AppState, owner: &str) -> ApiResult<usize> {
    let active: Vec<(Uuid, Uuid, Uuid)> =
        sqlx::query_as("SELECT id, tenant_id, job_id FROM validation_grid_active()")
            .fetch_all(&st.db)
            .await?;
    let n = active.len();
    for (id, tenant, job) in active {
        let (st, owner) = (st.clone(), owner.to_string());
        tokio::spawn(async move {
            if let Err(e) = run_grid_as(&st, tenant, job, id, &owner).await {
                tracing::warn!(grid = %id, code = e.code(), "validation_grid_failed");
            }
        });
    }
    if n > 0 {
        tracing::info!(runs = n, "validation_grid_resumed");
    }
    Ok(n)
}

/// 409 `grid_running`: a grid run of this release is queued or running.
pub fn grid_running() -> ApiError {
    ApiError::Conflict(
        "grid_running",
        "This release is being graded already; wait for it to finish.".into(),
    )
}

/// The latest grid run of a release (`releaseValidation`), or null.
pub async fn latest_json(
    c: &mut sqlx::PgConnection,
    job: Uuid,
    release_id: &str,
) -> ApiResult<Value> {
    #[derive(sqlx::FromRow)]
    struct Latest {
        id: Uuid,
        trigger: String,
        status: String,
        done: i32,
        total: i32,
        queued_at: chrono::DateTime<chrono::Utc>,
        started_at: Option<chrono::DateTime<chrono::Utc>>,
        finished_at: Option<chrono::DateTime<chrono::Utc>>,
        error_code: Option<String>,
    }
    let row: Option<Latest> = sqlx::query_as(
        "SELECT id, trigger, status, done, total, queued_at, started_at, finished_at, error_code \
           FROM validation_grid_run WHERE job_id = $1 AND release_id = $2 \
          ORDER BY queued_at DESC, id DESC LIMIT 1",
    )
    .bind(job)
    .bind(release_id)
    .fetch_optional(&mut *c)
    .await?;
    Ok(match row {
        None => Value::Null,
        Some(g) => json!({
            "id": g.id.to_string(),
            "trigger": g.trigger,
            "status": g.status,
            "done": g.done,
            "total": g.total,
            "queued_at": iso(g.queued_at),
            "started_at": g.started_at.map(iso),
            "finished_at": g.finished_at.map(iso),
            "error_code": g.error_code,
        }),
    })
}
