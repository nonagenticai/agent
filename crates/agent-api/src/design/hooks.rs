//! The job-creation hook (spec B2 §2.7) and its lazy fallback for jobs created before B2.
//!
//! - [`on_job_created`] runs in the creating transaction (called by B1 `readmodel::jobs::on_job_created`, which both
//!   create handlers of `routes/jobs.rs` call): it inserts the job's `job_target` row with the template's default
//!   selection, then takes the stored baseline ([`ensure_baseline`]).
//! - [`ensure_job`] does the same for a job that has no `job_target` row or no baseline yet (a job created before
//!   B2). **Every B2 job route (T7+) calls `ensure_job` first**, in its own job-scoped transaction, before it reads
//!   or writes the job's design. B1's job extractor does not call it.
//!
//! Both are idempotent: a job that already has its `job_target` row and a baseline (or a release) is left alone.
use agent_model::compile::Selection;
use serde_json::Value;
use sqlx::PgConnection;

use super::snapshot::{ensure_baseline, internal, job_not_found};
use super::{JobRef, Registry};
use crate::error::{ApiError, ApiResult};
use crate::platform::templates::{self, TemplatePin};

/// The default selection of the job template `layer@vversion` (B6a `resolve_template(..).default_selection`).
/// `None` when the platform store refuses that template ([`is_template_refusal`]: 404 `template_not_found`, 409
/// `template_not_tagged`): the job then starts with no selection chosen. Any other error propagates.
pub async fn default_selection(
    c: &mut PgConnection,
    layer: &str,
    version: i32,
) -> ApiResult<Option<Selection>> {
    match templates::resolve_template(c, layer, version).await {
        Ok(t) => Ok(Some(t.default_selection)),
        Err(e) if is_template_refusal(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// A template refusal that leaves the transaction usable: a 404 (`template_not_found`, `tag_not_found`,
/// `profile_not_found`), 409 `template_not_tagged`, or a detailed 404/409/422. Anything else (a database error,
/// including one mapped to a 409 `conflict`) has aborted the transaction and must propagate.
pub(crate) fn is_template_refusal(e: &ApiError) -> bool {
    match e {
        ApiError::Missing(..) | ApiError::Conflict("template_not_tagged", _) => true,
        ApiError::Detailed { status, .. } => matches!(status.as_u16(), 404 | 409 | 422),
        _ => false,
    }
}

/// B2's part of job creation (§2.7), in the creating transaction `tx` (scoped to the job's tenant): insert the
/// `job_target` row with `selection` (the template's default selection; `None` for a job without a template), then
/// take the baseline. `pins` are the job's template pins (B2 reads them from `jobs.pins` when it compiles; the
/// parameter is the §2.7 hook signature, kept for the plans that hook in after B2). Idempotent.
pub async fn on_job_created(
    tx: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    selection: Option<Selection>,
    _pins: &[TemplatePin],
) -> ApiResult<()> {
    let selection: Option<Value> = selection
        .map(|s| serde_json::to_value(&s))
        .transpose()
        .map_err(|e| internal(format!("a selection does not serialise: {e}")))?;
    sqlx::query(
        "INSERT INTO job_target (job_id, selection) VALUES ($1, $2) \
         ON CONFLICT (tenant_id, job_id) DO NOTHING",
    )
    .bind(job.id)
    .bind(selection)
    .execute(&mut *tx)
    .await?;
    ensure_baseline(&mut *tx, reg, job).await
}

#[derive(sqlx::FromRow)]
struct JobDesignState {
    template_layer: Option<String>,
    template_version: Option<i32>,
    pins: Value,
    has_target: bool,
    settled: bool,
}

/// The lazy fallback of [`on_job_created`] for a job created before B2 (controller ruling R4): every B2 job route
/// calls this first, in its job-scoped transaction `tx`. A job that has its `job_target` row and a baseline (or a
/// release) costs one read. Otherwise the job row is locked (`FOR UPDATE`, as `design::track` does) and the hook
/// runs. A job that already has a stored design but no `job_target` row gets one with NO selection: inserting the
/// template default then would change its design outside the ledger (D54). 404 `job_not_found`.
pub async fn ensure_job(tx: &mut PgConnection, reg: &Registry, job: &JobRef) -> ApiResult<()> {
    let st = sqlx::query_as::<_, JobDesignState>(
        "SELECT j.template_layer, j.template_version, j.pins, \
                EXISTS (SELECT 1 FROM job_target t WHERE t.job_id = j.id) AS has_target, \
                (j.current_release_id IS NOT NULL OR j.baseline_kinds IS NOT NULL) AS settled \
           FROM jobs j WHERE j.id = $1",
    )
    .bind(job.id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(job_not_found)?;
    if st.has_target && st.settled {
        return Ok(());
    }
    sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(job.id)
        .execute(&mut *tx)
        .await?;
    let selection = match (&st.template_layer, st.template_version, st.settled) {
        (Some(layer), Some(version), false) => default_selection(&mut *tx, layer, version).await?,
        _ => None,
    };
    let pins: Vec<TemplatePin> = serde_json::from_value(st.pins)
        .map_err(|e| internal(format!("jobs.pins is unreadable: {e}")))?;
    on_job_created(tx, reg, job, selection, &pins).await
}
