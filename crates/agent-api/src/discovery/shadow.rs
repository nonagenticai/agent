//! Shadow mode (T15, the half that needs no T5/T14b; D63, Review Focus 5). A runtime FLAG, never a route row: while
//! it is on for an env, an operation that has no route on that env is handed over to `agent_cases` instead of being
//! `not_offered` ([`ShadowRoutes`], the B4 router's hook), and every turn of the job is observed ([`observe`]: the
//! stage / activity the turn's GA already computed, a scrubbed excerpt; no model call).
//!
//! - [`start`] (409 `shadow_active`) / [`stop`] (409 `shadow_inactive`) toggle `shadow_mode`; [`stop`] also purges
//!   the job's observations older than [`KEEP`]. [`purge_all`] does that for every tenant on api boot.
//! - [`view`] = the fixture's `ShadowMode`: `conversations` / `labelled` count observations since `started_at`.
//! - [`active`] / [`observe`] run on the runtime's (`agent_runtime`) tenant-scoped connection: 0173 grants it SELECT
//!   on `shadow_mode` and INSERT on `discovery_observation`, nothing else.
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::Value;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::scrub;
use crate::error::{ApiError, ApiResult};
use crate::runtime::routes::{ResolvedTarget, RouteQuery, RouteResolver};
use crate::tenancy::JobCtx;

/// How long observations are kept (plan: purged after 30 days).
pub const KEEP_DAYS: i32 = 30;
/// [`KEEP_DAYS`] as the SQL interval text.
pub const KEEP: &str = "30 days";

/// `test` | `prod`, else 422 `invalid_env`.
pub fn env_of(v: &str) -> ApiResult<&'static str> {
    match v {
        "test" => Ok("test"),
        "prod" => Ok("prod"),
        _ => Err(ApiError::Unprocessable(
            "invalid_env",
            "Shadow mode runs on env test or prod.".into(),
        )),
    }
}

/// `startShadowMode`: 409 `shadow_active` while it is on (on either env).
pub async fn start(c: &mut PgConnection, job: &JobCtx, env: &str) -> ApiResult<()> {
    let started: Option<bool> = sqlx::query_scalar(
        "INSERT INTO shadow_mode AS s (tenant_id, job_id, active, env, started_at, started_by, stopped_at) \
         VALUES ($1, $2, true, $3, clock_timestamp(), $4, NULL) \
         ON CONFLICT (tenant_id, job_id) DO UPDATE \
            SET active = true, env = EXCLUDED.env, started_at = EXCLUDED.started_at, \
                started_by = EXCLUDED.started_by, stopped_at = NULL \
          WHERE NOT s.active \
         RETURNING true",
    )
    .bind(job.t.tenant)
    .bind(job.job_id)
    .bind(env)
    .bind(&job.t.caller.sub)
    .fetch_optional(&mut *c)
    .await?;
    if started.is_none() {
        return Err(ApiError::Conflict(
            "shadow_active",
            "Shadow mode is already on for this job; stop it first.".into(),
        ));
    }
    Ok(())
}

/// `stopShadowMode`: 409 `shadow_inactive` when it is off; then the job's observations older than [`KEEP`] go.
/// Returns the env it ran on.
pub async fn stop(c: &mut PgConnection, job: &JobCtx) -> ApiResult<String> {
    let env: Option<String> = sqlx::query_scalar(
        "UPDATE shadow_mode SET active = false, stopped_at = clock_timestamp() \
          WHERE tenant_id = $1 AND job_id = $2 AND active RETURNING env",
    )
    .bind(job.t.tenant)
    .bind(job.job_id)
    .fetch_optional(&mut *c)
    .await?;
    let Some(env) = env else {
        return Err(ApiError::Conflict(
            "shadow_inactive",
            "Shadow mode is not on for this job.".into(),
        ));
    };
    sqlx::query(
        "DELETE FROM discovery_observation \
          WHERE tenant_id = $1 AND job_id = $2 AND at < now() - make_interval(days => $3)",
    )
    .bind(job.t.tenant)
    .bind(job.job_id)
    .bind(KEEP_DAYS)
    .execute(&mut *c)
    .await?;
    Ok(env)
}

/// The fixture's `ShadowMode` for the job (off with env `test` when it never ran).
pub async fn view(c: &mut PgConnection, job: &JobCtx) -> ApiResult<Value> {
    let v: Option<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object( \
                  'active', s.active, 'env', s.env, 'started_at', s.started_at, 'started_by', s.started_by, \
                  'conversations', (SELECT count(DISTINCT o.case_id) FROM discovery_observation o \
                                     WHERE o.tenant_id = s.tenant_id AND o.job_id = s.job_id \
                                       AND s.started_at IS NOT NULL AND o.at >= s.started_at), \
                  'labelled', (SELECT count(*) FROM discovery_observation o \
                                WHERE o.tenant_id = s.tenant_id AND o.job_id = s.job_id AND o.labelled \
                                  AND s.started_at IS NOT NULL AND o.at >= s.started_at)) \
           FROM shadow_mode s WHERE s.tenant_id = $1 AND s.job_id = $2",
    )
    .bind(job.t.tenant)
    .bind(job.job_id)
    .fetch_optional(&mut *c)
    .await?;
    Ok(v.unwrap_or_else(|| {
        serde_json::json!({"active": false, "env": "test", "started_at": null, "started_by": null,
                           "conversations": 0, "labelled": 0})
    }))
}

/// B4's router: is shadow mode on for `job` on `env` (`test` | `prod`)? `c` must be scoped to `tenant`.
pub async fn active(c: &mut PgConnection, tenant: Uuid, job: Uuid, env: &str) -> ApiResult<bool> {
    let on: Option<bool> = sqlx::query_scalar(
        "SELECT true FROM shadow_mode WHERE tenant_id = $1 AND job_id = $2 AND active AND env = $3",
    )
    .bind(tenant)
    .bind(job)
    .bind(env)
    .fetch_optional(&mut *c)
    .await?;
    Ok(on.is_some())
}

/// One turn as the runtime saw it (B4's post-turn hook).
#[derive(Clone, Debug)]
pub struct Observation<'a> {
    pub tenant: Uuid,
    pub job: Uuid,
    pub case_id: &'a str,
    /// `test` | `prod`.
    pub env: &'a str,
    /// The GA answer the turn already computed.
    pub stage: Option<&'a str>,
    pub activity: Option<&'a str>,
    /// The turn was qualified into an activity (GA answered above the margin).
    pub labelled: bool,
    /// The customer text (already card-redacted by the turn); scrubbed here. `None` when B8 answered.
    pub text: Option<&'a str>,
}

/// Record `o` while shadow mode is on for its job and env (else nothing; returns whether a row was written). The
/// excerpt is scrubbed; a text with a card number / password / one-time code stores none. `c` must be scoped to
/// `o.tenant`.
pub async fn observe(c: &mut PgConnection, o: &Observation<'_>) -> ApiResult<bool> {
    let excerpt = o
        .text
        .filter(|t| !scrub::must_drop(t))
        .map(scrub::excerpt)
        .filter(|e| !e.is_empty());
    let n = sqlx::query(
        "INSERT INTO discovery_observation (tenant_id, job_id, case_id, env, stage, activity, labelled, excerpt) \
         SELECT $1, $2, $3, $4, $5, $6, $7, $8 \
          WHERE EXISTS (SELECT 1 FROM shadow_mode \
                         WHERE tenant_id = $1 AND job_id = $2 AND active AND env = $4)",
    )
    .bind(o.tenant)
    .bind(o.job)
    .bind(o.case_id)
    .bind(o.env)
    .bind(o.stage)
    .bind(o.activity)
    .bind(o.labelled)
    .bind(excerpt)
    .execute(&mut *c)
    .await?
    .rows_affected();
    Ok(n > 0)
}

/// Boot: delete observations older than [`KEEP`], every tenant (definer `discovery_purge_observations`).
pub async fn purge_all(db: &PgPool) -> ApiResult<i32> {
    let n: i32 =
        sqlx::query_scalar("SELECT discovery_purge_observations(make_interval(days => $1))")
            .bind(KEEP_DAYS)
            .fetch_one(db)
            .await?;
    Ok(n)
}

/// The B4 router hook (R10): wraps the runner's resolver; an operation the inner resolver leaves `not_offered` goes
/// to `agent_cases` (a handover) while shadow mode is on for the scope's job and env. It writes nothing.
pub struct ShadowRoutes {
    inner: Arc<dyn RouteResolver>,
}

impl ShadowRoutes {
    pub fn new(inner: Arc<dyn RouteResolver>) -> ShadowRoutes {
        ShadowRoutes { inner }
    }
}

impl RouteResolver for ShadowRoutes {
    fn resolve<'a>(
        &'a self,
        c: &'a mut PgConnection,
        q: &'a RouteQuery,
    ) -> BoxFuture<'a, ApiResult<ResolvedTarget>> {
        Box::pin(async move {
            let target = self.inner.resolve(&mut *c, q).await?;
            if matches!(target, ResolvedTarget::NotOffered)
                && active(&mut *c, q.tenant, q.job_id, q.env.as_str()).await?
            {
                return Ok(ResolvedTarget::AgentCases);
            }
            Ok(target)
        })
    }
}
