//! The validation grid per release cut (J17; B8 T12): the read model, a cut run and its stored results.
//!
//! - **Cuts** = the job's releases, newest first (`job_release`, the B2 list order), each with its state
//!   (`current` / `approved` / `applied_earlier`, B2's release state). A current or approved cut is graded on every
//!   live question (`deleted_at IS NULL`) ⋈ its stored result; an `applied_earlier` cut keeps only its stored
//!   results (a deleted question's text is kept for them, F-50).
//! - **Run** ([`run_cut`]): grade the cut's questions ([`super::judge::grade`]), then in one job-scoped transaction
//!   upsert every graded result (a person's verdict is never overwritten by the judge), meter each judge call
//!   (`usage_event` source `validation`, use `judge`, attributed to the person who ran or approved) and record the
//!   cut run. A question whose answer failed or that was not reached keeps its row (a re-run fills it).
use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::judge::{
    self, Answerer, Judge, KnowledgeAnswerer, LayaJudge, Limits, NoJudge, Outcome, Question,
};
use crate::design::{DesignOf, JobRef};
use crate::error::{ApiError, ApiResult};
use crate::providers::llm::{self, Use};
use crate::release::summary::release_not_found;
use crate::state::AppState;
use crate::usage::{self, UsageEvent};

/// `SELECT … FROM job_release` of one job: id, hash, state.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct CutRelease {
    pub release_id: String,
    pub hash: String,
    pub state: String,
}

/// One grid row (a question, with its stored result on this cut if any).
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct RowRec {
    pub id: Uuid,
    pub question: String,
    pub expected: String,
    pub reference: String,
    pub activity: String,
    pub response: Option<String>,
    pub cited: Vec<String>,
    pub verdict: String,
    pub reason: Option<String>,
    pub judged_by: Option<String>,
}

impl RowRec {
    pub fn question(&self) -> Question {
        Question {
            id: self.id,
            question: self.question.clone(),
            expected: self.expected.clone(),
            reference: self.reference.clone(),
            activity: self.activity.clone(),
        }
    }

    pub fn json(&self) -> Value {
        json!({
            "id": self.id.to_string(),
            "question": self.question,
            "expected": self.expected,
            "reference": self.reference,
            "activity": self.activity,
            "response": self.response,
            "cited": self.cited,
            "verdict": self.verdict,
            "reason": self.reason,
            "judged_by": self.judged_by,
        })
    }
}

/// A job-scoped transaction (both GUCs), for paths that have no request context (the grid task).
pub async fn begin_job(
    db: &sqlx::PgPool,
    tenant: Uuid,
    job: Uuid,
) -> ApiResult<sqlx::Transaction<'static, sqlx::Postgres>> {
    let mut tx = crate::tenancy::begin_scoped(db, tenant).await?;
    sqlx::query("SELECT set_config('app.current_job', $1, true)")
        .bind(job.to_string())
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// The job's releases, newest first.
pub async fn releases(c: &mut PgConnection, job: Uuid) -> ApiResult<Vec<CutRelease>> {
    Ok(sqlx::query_as::<_, CutRelease>(
        "SELECT release_id, hash, state FROM job_release WHERE job_id = $1 \
          ORDER BY approved_at DESC, release_id DESC",
    )
    .bind(job)
    .fetch_all(&mut *c)
    .await?)
}

/// One release of the job; 404 `release_not_found`.
pub async fn release(c: &mut PgConnection, job: Uuid, id: &str) -> ApiResult<CutRelease> {
    sqlx::query_as::<_, CutRelease>(
        "SELECT release_id, hash, state FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(job)
    .bind(id)
    .fetch_optional(&mut *c)
    .await?
    .ok_or_else(|| release_not_found(id))
}

const LIVE_ROWS: &str = "SELECT q.id, q.question, q.expected, q.reference, q.activity, r.response, \
            coalesce(r.cited, '{}') AS cited, coalesce(r.verdict, 'pending') AS verdict, r.reason, r.judged_by \
       FROM validation_question q \
       LEFT JOIN validation_result r \
              ON r.question_id = q.id AND r.job_id = q.job_id AND r.release_id = $2 \
      WHERE q.job_id = $1 AND q.deleted_at IS NULL \
      ORDER BY q.created_at, q.id";

const STORED_ROWS: &str = "SELECT q.id, q.question, q.expected, q.reference, q.activity, r.response, r.cited, \
            r.verdict, r.reason, r.judged_by \
       FROM validation_result r JOIN validation_question q ON q.id = r.question_id \
      WHERE r.job_id = $1 AND r.release_id = $2 \
      ORDER BY q.created_at, q.id";

/// The rows of one cut (see the module doc).
pub async fn rows(c: &mut PgConnection, job: Uuid, rel: &CutRelease) -> ApiResult<Vec<RowRec>> {
    let sql = if rel.state == "applied_earlier" {
        STORED_ROWS
    } else {
        LIVE_ROWS
    };
    Ok(sqlx::query_as::<_, RowRec>(sql)
        .bind(job)
        .bind(&rel.release_id)
        .fetch_all(&mut *c)
        .await?)
}

/// `ValidationCut` of one release.
pub async fn cut_json(c: &mut PgConnection, job: Uuid, rel: &CutRelease) -> ApiResult<Value> {
    let rows = rows(&mut *c, job, rel).await?;
    let ran: Option<(DateTime<Utc>, String)> = sqlx::query_as(
        "SELECT ran_at, ran_by FROM validation_cut_run WHERE job_id = $1 AND release_id = $2",
    )
    .bind(job)
    .bind(&rel.release_id)
    .fetch_optional(&mut *c)
    .await?;
    let (ran_at, ran_by) = match ran {
        Some((at, by)) => (Some(crate::generator::run::iso(at)), Some(by)),
        None => (None, None),
    };
    Ok(json!({
        "release": {"id": rel.release_id, "hash": rel.hash},
        "state": rel.state,
        "ran_at": ran_at,
        "ran_by": ran_by,
        "rows": rows.iter().map(RowRec::json).collect::<Vec<_>>(),
    }))
}

/// Every cut of the job, newest first.
pub async fn cuts_json(c: &mut PgConnection, job: Uuid) -> ApiResult<Vec<Value>> {
    let mut out = Vec::new();
    for rel in releases(&mut *c, job).await? {
        out.push(cut_json(&mut *c, job, &rel).await?);
    }
    Ok(out)
}

/// What a run needs to know besides the questions.
#[derive(Clone, Debug)]
pub struct RunCtx {
    pub tenant: Uuid,
    pub job: JobRef,
    pub release_id: String,
    /// The sub of the person who ran it (manual) or approved the release (grid): usage is attributed to them.
    pub by: String,
}

/// Counts of a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunCounts {
    pub total: usize,
    pub graded: usize,
    pub failed: usize,
    pub not_reached: usize,
}

/// The live answerer and judge of a release (own short transaction; nothing is held across an upstream call).
pub async fn upstreams(
    st: &AppState,
    ctx: &RunCtx,
) -> ApiResult<(Arc<dyn Answerer>, Arc<dyn Judge>)> {
    let mut tx = begin_job(&st.db, ctx.tenant, ctx.job.id).await?;
    let kinds = KnowledgeAnswerer::kinds_of(&mut tx, st, &ctx.job, &ctx.release_id).await?;
    let of = DesignOf::Release(ctx.release_id.clone());
    let resolved = match llm::resolve(&mut tx, st, ctx.tenant, &ctx.job, Use::Gates, &of).await {
        Ok(r) => r,
        // a binding that cannot serve is a judge that cannot answer: pending (B8-2), not a failed run
        Err(ApiError::Conflict(..)) => None,
        Err(e) => return Err(e),
    };
    tx.commit().await?;
    let judge: Arc<dyn Judge> = match resolved {
        Some(r) => match LayaJudge::for_resolved(&r, &st.cfg.connector_policy, ctx.tenant).await {
            Some(j) => Arc::new(j),
            None => Arc::new(NoJudge),
        },
        None => Arc::new(NoJudge),
    };
    Ok((
        Arc::new(KnowledgeAnswerer {
            st: st.clone(),
            tenant: ctx.tenant,
            job: ctx.job.clone(),
            release_id: ctx.release_id.clone(),
            kinds,
        }),
        judge,
    ))
}

/// Grade `questions` and store the outcomes (see the module doc). Returns the counts.
pub async fn run_cut(
    st: &AppState,
    ctx: &RunCtx,
    questions: &[Question],
    answerer: Arc<dyn Answerer>,
    judge: Arc<dyn Judge>,
    limits: Limits,
) -> ApiResult<RunCounts> {
    let outcomes = judge::grade(questions, answerer, judge, limits).await;
    store(st, ctx, questions, &outcomes).await
}

/// Store graded outcomes, meter the judge calls and record the cut run, in one job-scoped transaction.
pub async fn store(
    st: &AppState,
    ctx: &RunCtx,
    questions: &[Question],
    outcomes: &[Outcome],
) -> ApiResult<RunCounts> {
    let mut counts = RunCounts {
        total: questions.len(),
        ..RunCounts::default()
    };
    let mut tx = begin_job(&st.db, ctx.tenant, ctx.job.id).await?;
    for (q, o) in questions.iter().zip(outcomes) {
        let g = match o {
            Outcome::Graded(g) => g,
            Outcome::Failed(_) => {
                counts.failed += 1;
                continue;
            }
            Outcome::NotReached => {
                counts.not_reached += 1;
                continue;
            }
        };
        counts.graded += 1;
        sqlx::query(
            "INSERT INTO validation_result \
                 (job_id, release_id, question_id, response, cited, verdict, reason, judged_by, judged_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'judge', now()) \
             ON CONFLICT (tenant_id, job_id, release_id, question_id) DO UPDATE \
                SET response = EXCLUDED.response, cited = EXCLUDED.cited, verdict = EXCLUDED.verdict, \
                    reason = EXCLUDED.reason, judged_by = 'judge', judged_at = now() \
              WHERE validation_result.judged_by IS NULL OR validation_result.judged_by = 'judge'",
        )
        .bind(ctx.job.id)
        .bind(&ctx.release_id)
        .bind(q.id)
        .bind(&g.response)
        .bind(&g.cited)
        .bind(g.verdict.as_str())
        .bind(g.reason)
        .execute(&mut *tx)
        .await?;
        if let Some(u) = &g.usage {
            let activity = (!q.activity.is_empty()).then_some(q.activity.as_str());
            usage::emit(
                &mut tx,
                &st.cfg.metering,
                &UsageEvent {
                    job_id: Some(ctx.job.id),
                    source: "validation",
                    use_: "judge",
                    activity,
                    conversation_id: None,
                    case_id: None,
                    actor: &ctx.by,
                    usage: u,
                },
            )
            .await?;
        }
    }
    sqlx::query(
        "INSERT INTO validation_cut_run (job_id, release_id, ran_at, ran_by) VALUES ($1, $2, now(), $3) \
         ON CONFLICT (tenant_id, job_id, release_id) DO UPDATE SET ran_at = now(), ran_by = EXCLUDED.ran_by",
    )
    .bind(ctx.job.id)
    .bind(&ctx.release_id)
    .bind(&ctx.by)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(counts)
}

/// Questions of a cut, by id (the order of [`rows`]).
pub fn questions_of(rows: &[RowRec]) -> Vec<Question> {
    rows.iter().map(RowRec::question).collect()
}

/// Rows by question id.
pub fn by_id(rows: &[RowRec]) -> BTreeMap<Uuid, &RowRec> {
    rows.iter().map(|r| (r.id, r)).collect()
}
