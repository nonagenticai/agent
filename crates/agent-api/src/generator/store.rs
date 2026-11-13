//! Generator run rows (0171, B8 T9a). Every run carries a tenant (R15: a platform run the platform-owned tenant):
//! a tenant transaction (`begin_scoped`) sees that tenant's runs (RLS `tenant_isolation`), a platform transaction
//! (`begin_platform`, super_admin routes) every run (RLS `platform_read`).
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::run::{CostLineRow, ItemRow, RunDetail, RunRow, StageRow, StepRow};
use crate::error::{ApiError, ApiResult};

const RUN_COLUMNS: &str = "r.id, r.tenant_id, r.scope, r.industry, r.department, r.job_key, r.job_id, r.status, \
     r.current_step, r.description, r.objects, r.output, r.tagged, r.funding, r.started_by, r.started_at, \
     r.updated_at, \
     (SELECT coalesce(sum(l.tokens), 0)::bigint FROM generator_cost_line l WHERE l.run_id = r.id) AS tokens";

/// 404 `run_not_found` (fixture `runOf` text).
pub fn run_not_found(id: &str) -> ApiError {
    ApiError::Missing("run_not_found", format!("No generator run #{id}."))
}

/// Every run the transaction sees, newest first.
pub async fn list(c: &mut PgConnection) -> ApiResult<Vec<RunRow>> {
    Ok(sqlx::query_as::<_, RunRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {RUN_COLUMNS} FROM generator_run r ORDER BY r.id DESC"
    )))
    .fetch_all(&mut *c)
    .await?)
}

/// One run the transaction sees; 404 `run_not_found`.
pub async fn run(c: &mut PgConnection, id: i64) -> ApiResult<RunRow> {
    sqlx::query_as::<_, RunRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {RUN_COLUMNS} FROM generator_run r WHERE r.id = $1"
    )))
    .bind(id)
    .fetch_optional(&mut *c)
    .await?
    .ok_or_else(|| run_not_found(&id.to_string()))
}

/// A run with its steps, stages, items and cost lines (in pipeline / start / draft / call order).
pub async fn detail(c: &mut PgConnection, id: i64) -> ApiResult<RunDetail> {
    let run = run(&mut *c, id).await?;
    let steps = sqlx::query_as::<_, StepRow>(
        "SELECT step, status, summary FROM generator_step WHERE run_id = $1 ORDER BY ord, step",
    )
    .bind(id)
    .fetch_all(&mut *c)
    .await?;
    let stages = sqlx::query_as::<_, StageRow>(
        "SELECT stage, label, out_of_scope FROM generator_stage WHERE run_id = $1 ORDER BY ord, stage",
    )
    .bind(id)
    .fetch_all(&mut *c)
    .await?;
    let items = sqlx::query_as::<_, ItemRow>(
        "SELECT item_id, step, kind, title, detail, example, decision, decision_note, payload->>'key' AS payload_key, new_object, new_operation, \
                proposal, llm_confidence \
           FROM generator_item WHERE run_id = $1 ORDER BY ord, item_id",
    )
    .bind(id)
    .fetch_all(&mut *c)
    .await?;
    let cost_lines = sqlx::query_as::<_, CostLineRow>(
        "SELECT step, model, tokens, funding, at FROM generator_cost_line WHERE run_id = $1 ORDER BY at, id",
    )
    .bind(id)
    .fetch_all(&mut *c)
    .await?;
    Ok(RunDetail {
        run,
        steps,
        stages,
        items,
        cost_lines,
    })
}

/// The tokens of the packaged runs of `scope` with usage (the estimate's basis), oldest id first, with the newest
/// one's funding lane.
pub async fn packaged_usage(
    c: &mut PgConnection,
    scope: &str,
) -> ApiResult<(Vec<(i64, i64)>, Option<String>)> {
    let rows: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT r.id, sum(l.tokens)::bigint, r.funding FROM generator_run r \
           JOIN generator_cost_line l ON l.run_id = r.id \
          WHERE r.scope = $1 AND r.status = 'packaged' \
          GROUP BY r.id, r.funding HAVING sum(l.tokens) > 0 ORDER BY r.id",
    )
    .bind(scope)
    .fetch_all(&mut *c)
    .await?;
    let funding = rows.last().map(|(_, _, f)| f.clone());
    Ok((
        rows.into_iter().map(|(id, t, _)| (id, t)).collect(),
        funding,
    ))
}

/// Tenant names by id, as a platform transaction reads them (`agent_list_tenants()`, granted to `agent_platform`).
pub async fn tenant_names(c: &mut PgConnection) -> ApiResult<Vec<(Uuid, String)>> {
    Ok(
        sqlx::query_as::<_, (Uuid, String)>("SELECT id, name FROM agent_list_tenants()")
            .fetch_all(&mut *c)
            .await?,
    )
}

// ---- writes (B8 T9b / T10) -----------------------------------------------------------------------------------------

/// The run row locked for the rest of the transaction (`FOR UPDATE`); 404 `run_not_found`.
pub async fn lock_run(c: &mut PgConnection, id: i64) -> ApiResult<RunRow> {
    sqlx::query_as::<_, RunRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {RUN_COLUMNS} FROM generator_run r WHERE r.id = $1 FOR UPDATE OF r"
    )))
    .bind(id)
    .fetch_optional(&mut *c)
    .await?
    .ok_or_else(|| run_not_found(&id.to_string()))
}

/// The ids of the open platform runs whose output names the layer `family` (`family@vN`).
pub async fn open_runs_on(c: &mut PgConnection, family: &str) -> ApiResult<Vec<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT r.id FROM generator_run r \
          WHERE r.scope = 'platform' AND r.status = 'running' \
            AND EXISTS (SELECT 1 FROM unnest(r.output) AS o WHERE split_part(o, '@', 1) = $1) \
          ORDER BY r.id",
    )
    .bind(family)
    .fetch_all(&mut *c)
    .await?)
}

/// What start stores of a run.
#[derive(Debug)]
pub struct NewRun<'a> {
    pub tenant_id: Uuid,
    pub scope: &'a str,
    pub industry: &'a str,
    pub department: &'a str,
    pub job_key: &'a str,
    pub job_id: Option<Uuid>,
    pub first_step: &'a str,
    pub description: &'a str,
    pub output: &'a [String],
    pub funding: &'a str,
    pub provider: Option<&'a Value>,
    pub started_by: &'a str,
}

/// Insert the run, its steps (`input` done, the first drafting step current, the rest todo) and its stages
/// (`(key, label, out_of_scope)` in order). Returns the run id.
pub async fn insert_run(
    c: &mut PgConnection,
    r: &NewRun<'_>,
    steps: &[&'static str],
    input_summary: &str,
    stages: &[(String, String, bool)],
) -> ApiResult<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO generator_run (tenant_id, scope, industry, department, job_key, job_id, current_step, \
                                    description, output, funding, provider, started_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) RETURNING id",
    )
    .bind(r.tenant_id)
    .bind(r.scope)
    .bind(r.industry)
    .bind(r.department)
    .bind(r.job_key)
    .bind(r.job_id)
    .bind(r.first_step)
    .bind(r.description)
    .bind(r.output)
    .bind(r.funding)
    .bind(r.provider)
    .bind(r.started_by)
    .fetch_one(&mut *c)
    .await?;
    for (i, step) in steps.iter().enumerate() {
        let (status, summary) = if *step == "input" {
            ("done", input_summary)
        } else if *step == r.first_step {
            ("current", "")
        } else {
            ("todo", "")
        };
        sqlx::query(
            "INSERT INTO generator_step (run_id, step, ord, status, summary) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(*step)
        .bind(i16::try_from(i).unwrap_or(i16::MAX))
        .bind(status)
        .bind(summary)
        .execute(&mut *c)
        .await?;
    }
    for (i, (stage, label, out)) in stages.iter().enumerate() {
        sqlx::query(
            "INSERT INTO generator_stage (run_id, stage, ord, label, out_of_scope) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(stage)
        .bind(i16::try_from(i).unwrap_or(i16::MAX))
        .bind(label)
        .bind(*out)
        .execute(&mut *c)
        .await?;
    }
    Ok(id)
}

/// A drafted item to store.
#[derive(Debug)]
pub struct NewItem<'a> {
    pub kind: &'a str,
    pub title: &'a str,
    pub detail: &'a str,
    pub example: Option<&'a Value>,
    pub payload: &'a Value,
    pub new_object: Option<&'a str>,
    pub new_operation: Option<&'a Value>,
    pub confidence: f32,
}

/// Insert drafted items of `step` as `pending` (ids `<run>-<step>-<n>`, numbered after the step's existing ones).
/// Returns the new ids.
pub async fn insert_items(
    c: &mut PgConnection,
    run: i64,
    step: &str,
    items: &[NewItem<'_>],
) -> ApiResult<Vec<String>> {
    let (mut ord, mut n): (i32, i32) = sqlx::query_as(
        "SELECT (SELECT coalesce(max(ord) + 1, 0) FROM generator_item WHERE run_id = $1), \
                (SELECT coalesce(max(substring(item_id from '[0-9]+$')::int), 0) FROM generator_item \
                  WHERE run_id = $1 AND step = $2)",
    )
    .bind(run)
    .bind(step)
    .fetch_one(&mut *c)
    .await?;
    let mut ids = Vec::with_capacity(items.len());
    for it in items {
        n += 1;
        let id = format!("{run}-{step}-{n}");
        sqlx::query(
            "INSERT INTO generator_item (run_id, item_id, ord, step, kind, title, detail, example, payload, \
                                         new_object, new_operation, llm_confidence) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(run)
        .bind(&id)
        .bind(ord)
        .bind(step)
        .bind(it.kind)
        .bind(it.title)
        .bind(it.detail)
        .bind(it.example)
        .bind(it.payload)
        .bind(it.new_object)
        .bind(it.new_operation)
        .bind(it.confidence)
        .execute(&mut *c)
        .await?;
        ord += 1;
        ids.push(id);
    }
    Ok(ids)
}

/// The stored shape of one item for the drive (decision, typed payload, proposal state).
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ItemFull {
    pub item_id: String,
    pub step: String,
    pub kind: String,
    pub title: String,
    pub detail: String,
    pub example: Option<Value>,
    pub payload: Option<Value>,
    pub decision: String,
    pub decision_note: Option<String>,
    pub new_object: Option<String>,
    pub new_operation: Option<Value>,
    pub proposal: String,
}

const ITEM_FULL: &str = "item_id, step, kind, title, detail, example, payload, decision, decision_note, new_object, \
     new_operation, proposal";

/// Every item of the run, in draft order.
pub async fn items_full(c: &mut PgConnection, run: i64) -> ApiResult<Vec<ItemFull>> {
    Ok(sqlx::query_as::<_, ItemFull>(sqlx::AssertSqlSafe(format!(
        "SELECT {ITEM_FULL} FROM generator_item WHERE run_id = $1 ORDER BY ord, item_id"
    )))
    .bind(run)
    .fetch_all(&mut *c)
    .await?)
}

/// One item of the run, locked; 404 `item_not_found`.
pub async fn item_for_update(c: &mut PgConnection, run: i64, item: &str) -> ApiResult<ItemFull> {
    sqlx::query_as::<_, ItemFull>(sqlx::AssertSqlSafe(format!(
        "SELECT {ITEM_FULL} FROM generator_item WHERE run_id = $1 AND item_id = $2 FOR UPDATE"
    )))
    .bind(run)
    .bind(item)
    .fetch_optional(&mut *c)
    .await?
    .ok_or_else(|| ApiError::Missing("item_not_found", "No such draft item.".into()))
}

/// Touch the run (`updated_at`).
pub async fn touch(c: &mut PgConnection, run: i64) -> ApiResult<()> {
    sqlx::query("UPDATE generator_run SET updated_at = now() WHERE id = $1")
        .bind(run)
        .execute(&mut *c)
        .await?;
    Ok(())
}
