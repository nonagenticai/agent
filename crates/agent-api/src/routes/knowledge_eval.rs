//! `/api/knowledge/eval`: golden questions and async retrieval eval runs (B3 T9, D72; plan §5.6, §0 R7/R10). Every
//! method proxies strata_rag's tenant eval (rag #21, pinned `7c2f1ad`: `src/api/eval_api.rs`, `handlers::eval`);
//! shapes = `web/src/lib/api/types/retrieval.ts`, refusals = `web/src/lib/api/areas/retrieval.ts`. Golden questions
//! are tenant data written straight (never through the D54 draft ledger); eval runs are operational records.
//!
//! - `GET /api/knowledge/eval/questions` (member) → strata's `{items, next}` as is.
//! - `POST /api/knowledge/eval/questions` (knowledge_manager) → 201 `GoldenQuestion`. 422 `invalid_input`,
//!   `too_many_questions`; 409 `question_exists` (strata's, passed through).
//! - `PATCH /api/knowledge/eval/questions/{id}` (knowledge_manager) → `GoldenQuestion`; + 404 `not_found`.
//! - `DELETE /api/knowledge/eval/questions/{id}` (knowledge_manager) → 204; 404 `not_found`.
//! - `POST /api/knowledge/eval/questions/import` (knowledge_manager) `{questions:[…]}` → 201 `{added, total}`, all or
//!   nothing (strata decides). JSON only: the UI parses CSV client-side and sends the rows.
//! - `POST /api/knowledge/eval/runs` (knowledge_manager) → **202** with the `running` run summary and
//!   `Location: /api/knowledge/eval/runs/{id}` (strata's `/api/eval/runs/{id}` rewritten). The scoring runs in
//!   strata_rag; agent answers with strata's 202 and never waits for it (default 10 s bound on the proxy call).
//!   D91 / §0 R7: the root is resolved here, before strata is called: `root_id` (an agent store id) names a store
//!   whose root is evaluated; without it the tenant's only root. Several roots and none named → 422
//!   `root_required` (+ `roots`); an unknown store → 404 `not_found`; no store yet, or the root not yet created in
//!   strata_rag → 409 `eval_not_ready`. strata always gets `root_id`. T4a: with `job` (a job key) the root is the one
//!   of that job's pinned DRAFT generation (what its Test environment searches; 404 `job_not_found`), without it the
//!   store's own binding (the `initial` generation, what a job with no pin searches). §0 R10: eval stays on the
//!   tenant tree; no platform packs (strata spec §9).
//! - `GET /api/knowledge/eval/runs?limit=&cursor=` (member) → strata's `{items, next}` (cursor opaque, passed back).
//! - `GET /api/knowledge/eval/runs/{id}` (member) → `EvalRunDetail`; 404 `not_found`.
//!
//! Every run row's `root_id` is translated strata store id → agent store id, generation roots included (null when
//! the store is no longer in the tenant's tree). Writes send `X-Actor-Id` and are audited (`eval_question.create|update|delete|import`,
//! `eval_run.start`) after strata accepted them.
use std::collections::HashMap;

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::audit::{self, Event};
use crate::design::DesignOf;
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::knowledge::generations;
use crate::knowledge::stores::{self, StoreRow};
use crate::state::AppState;
use crate::strata::rag::{self, EvalReq};
use crate::strata::{self, Plane, Service, StrataStoreId};
use crate::tenancy::{Member, Requires, TenantCtx, role};
use crate::upstream;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/knowledge/eval/questions",
            get(list_questions).post(create_question),
        )
        // static, so it never reaches `/api/knowledge/eval/questions/{id}` (which has no POST)
        .route(
            "/api/knowledge/eval/questions/import",
            post(import_questions),
        )
        .route(
            "/api/knowledge/eval/questions/{id}",
            patch(update_question).delete(delete_question),
        )
        .route("/api/knowledge/eval/runs", get(list_runs).post(run_eval))
        .route("/api/knowledge/eval/runs/{id}", get(get_run))
}

/// Where agent serves one run (the rewritten `Location`).
pub fn run_location(id: Uuid) -> String {
    format!("/api/knowledge/eval/runs/{id}")
}

// ---------------------------------------------------------------------------------------------------------------
// Inputs

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportInput {
    questions: Vec<Value>,
}

/// `EvalRunOptions` (`types/retrieval.ts`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvalInput {
    #[serde(default)]
    question_ids: Option<Vec<Uuid>>,
    #[serde(default)]
    questions: Option<Vec<Value>>,
    /// An agent store id; its tree's root is evaluated.
    #[serde(default)]
    root_id: Option<Uuid>,
    /// A job key: the root of that job's pinned draft generation is evaluated (T4a).
    #[serde(default)]
    job: Option<String>,
    #[serde(default)]
    modes: Option<Vec<String>>,
    #[serde(default)]
    beam: Option<usize>,
    #[serde(default)]
    min_score: Option<f32>,
}

#[derive(Deserialize)]
struct RunsQuery {
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    cursor: Option<String>,
}

fn not_an_object() -> ApiError {
    ApiError::Unprocessable(
        "invalid_input",
        "A golden question is a JSON object.".into(),
    )
}

// ---------------------------------------------------------------------------------------------------------------
// The data plane and the root translation

/// The rag plane for `t` (the data-plane gate; nothing stays locked) acting as the caller.
async fn rag_plane<'a>(st: &'a AppState, t: &TenantCtx) -> ApiResult<Plane<'a>> {
    let mut tx = t.begin(&st.db).await?;
    let p = strata::gate(st, &mut tx, t.tenant, Service::Rag)
        .await?
        .with_actor(&t.caller.sub);
    tx.commit().await?;
    Ok(p)
}

/// One `store_generation` row as [`agent_ids`] reads it: `(root store, strata root, members)`.
type GenIds = (Uuid, Option<Uuid>, Value);
/// The rag plane, the tenant's store tree and its strata → agent id map.
type PlaneTree<'a> = (Plane<'a>, Vec<StoreRow>, HashMap<Uuid, Uuid>);

/// The rag plane, the tenant's store tree and its strata → agent id map ([`agent_ids`]), read in one scoped
/// transaction.
async fn rag_plane_and_tree<'a>(st: &'a AppState, t: &TenantCtx) -> ApiResult<PlaneTree<'a>> {
    let mut tx = t.begin(&st.db).await?;
    let p = strata::gate(st, &mut tx, t.tenant, Service::Rag)
        .await?
        .with_actor(&t.caller.sub);
    let all = stores::all(&mut tx).await?;
    let gens: Vec<GenIds> =
        sqlx::query_as("SELECT store_id, strata_store_id, members FROM store_generation")
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;
    let ids = agent_ids(&all, &gens);
    Ok((p, all, ids))
}

/// The strata root `job` searches for `root` in its pinned draft generation (T4a); 409 `eval_not_ready` when that
/// generation has no strata root yet.
async fn job_root(
    st: &AppState,
    t: &TenantCtx,
    job_key: &str,
    root: &StoreRow,
) -> ApiResult<StrataStoreId> {
    let mut tx = t.begin(&st.db).await?;
    let job = generations::job_ref(&mut tx, job_key).await?;
    let sid = generations::root_for(&mut tx, &job, root, &DesignOf::Draft).await?;
    tx.commit().await?;
    sid.ok_or_else(|| {
        ApiError::Conflict(
            "eval_not_ready",
            format!(
                "{}'s store generation for job {job_key} is not ready in strata_rag yet.",
                root.key
            ),
        )
    })
}

/// The root store an eval runs on (D91 / §0 R7), as the agent row. `store` = a store of the tree (its root is
/// evaluated); without one, the tenant's only root. Several roots and none named → 422 `root_required`; an
/// unknown store → 404 `not_found`; no store at all, or a root strata_rag does not hold yet → 409
/// `eval_not_ready`.
fn eval_root(all: &[StoreRow], store: Option<Uuid>) -> ApiResult<(&StoreRow, StrataStoreId)> {
    let root_id = match store {
        Some(id) => stores::root_of(all, id).ok_or_else(|| {
            ApiError::Missing(
                "not_found",
                "No knowledge store with that id in this tenant.".into(),
            )
        })?,
        None => {
            let roots: Vec<&StoreRow> = all.iter().filter(|s| s.parent_id.is_none()).collect();
            match roots.as_slice() {
                [] => {
                    return Err(ApiError::Conflict(
                        "eval_not_ready",
                        "This tenant has no knowledge store to evaluate yet.".into(),
                    ));
                }
                [one] => one.id,
                _ => {
                    let keys: Vec<String> = roots.iter().map(|s| s.key.clone()).collect();
                    return Err(ApiError::Unprocessable(
                        "root_required",
                        "This tenant has several store trees; pick the one to evaluate.".into(),
                    )
                    .with("roots", keys));
                }
            }
        }
    };
    let root = all
        .iter()
        .find(|s| s.id == root_id)
        .ok_or_else(stores::store_not_found)?;
    let sid = root.strata().ok_or_else(|| {
        ApiError::Conflict(
            "eval_not_ready",
            format!(
                "{} is still being created in strata_rag; evaluate it once it is ready.",
                root.key
            ),
        )
    })?;
    Ok((root, sid))
}

/// strata store id → agent store id of the tenant's tree: each store's own binding, plus every generation's root
/// and mirrored members.
fn agent_ids(all: &[StoreRow], gens: &[GenIds]) -> HashMap<Uuid, Uuid> {
    let mut out: HashMap<Uuid, Uuid> = all
        .iter()
        .filter_map(|s| s.strata_store_id.map(|sid| (sid, s.id)))
        .collect();
    for (store, root, members) in gens {
        if let Some(sid) = root {
            out.entry(*sid).or_insert(*store);
        }
        for (agent, sid) in members.as_object().into_iter().flatten() {
            if let (Ok(agent), Some(Ok(sid))) =
                (Uuid::parse_str(agent), sid.as_str().map(Uuid::parse_str))
            {
                out.entry(sid).or_insert(agent);
            }
        }
    }
    out
}

/// A run row's `root_id` in agent ids: the store whose strata id it is, or null when the tree no longer holds one.
fn translate_run(run: &mut Value, ids: &HashMap<Uuid, Uuid>) {
    if let Some(obj) = run.as_object_mut()
        && let Some(v) = obj.get_mut("root_id")
        && !v.is_null()
    {
        let agent = v
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .and_then(|sid| ids.get(&sid).copied());
        *v = agent.map_or(Value::Null, |id| Value::String(id.to_string()));
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Golden questions

/// `goldenQuestions`.
async fn list_questions(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let p = rag_plane(&st, &t).await?;
    Ok(Json(rag::eval_questions(&st.http, &p).await?))
}

/// The audit row of a write strata accepted (its own short transaction).
async fn record(st: &AppState, t: &TenantCtx, ev: Event) -> ApiResult<()> {
    let mut tx = t.begin(&st.db).await?;
    audit::record(&mut tx, &t.actor(), ev).await?;
    tx.commit().await?;
    Ok(())
}

/// The question id strata answered with (for the audit row).
fn id_of(v: &Value) -> String {
    v.get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// `createGoldenQuestion`.
async fn create_question(
    Requires(t, _): Requires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiJson(input): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if !input.is_object() {
        return Err(not_an_object());
    }
    let p = rag_plane(&st, &t).await?;
    let row = rag::eval_question_create(&st.http, &p, input).await?;
    record(
        &st,
        &t,
        Event::new("eval_question.create", "eval_question", id_of(&row)),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(row)))
}

/// `updateGoldenQuestion`.
async fn update_question(
    Requires(t, _): Requires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(patch): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    if !patch.is_object() {
        return Err(not_an_object());
    }
    let fields: Vec<String> = patch
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let p = rag_plane(&st, &t).await?;
    let row = rag::eval_question_update(&st.http, &p, id, patch).await?;
    record(
        &st,
        &t,
        Event::new("eval_question.update", "eval_question", id.to_string())
            .detail("fields", fields),
    )
    .await?;
    Ok(Json(row))
}

/// `deleteGoldenQuestion`: stored runs keep their snapshot.
async fn delete_question(
    Requires(t, _): Requires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<StatusCode> {
    let p = rag_plane(&st, &t).await?;
    rag::eval_question_delete(&st.http, &p, id).await?;
    record(
        &st,
        &t,
        Event::new("eval_question.delete", "eval_question", id.to_string()),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `importGoldenQuestions`: all or nothing (strata's rule).
async fn import_questions(
    Requires(t, _): Requires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiJson(input): ApiJson<ImportInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let p = rag_plane(&st, &t).await?;
    let res = rag::eval_questions_import(&st.http, &p, input.questions).await?;
    let added = res.get("added").cloned().unwrap_or(Value::Null);
    let total = res.get("total").cloned().unwrap_or(Value::Null);
    record(
        &st,
        &t,
        Event::new("eval_question.import", "eval_question", "import")
            .detail("added", added)
            .detail("total", total),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(res)))
}

// ---------------------------------------------------------------------------------------------------------------
// Eval runs

/// `runEval`: 202 + `Location`, the run scores in strata_rag.
async fn run_eval(
    Requires(t, _): Requires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiJson(input): ApiJson<EvalInput>,
) -> ApiResult<Response> {
    let (p, all, ids) = rag_plane_and_tree(&st, &t).await?;
    let (root, sid) = eval_root(&all, input.root_id)?;
    let sid = match input.job.as_deref() {
        Some(job) => job_root(&st, &t, job, root).await?,
        None => sid,
    };
    let root_key = root.key.clone();
    let req = EvalReq {
        modes: input.modes,
        beam: input.beam,
        min_score: input.min_score,
        question_ids: input.question_ids,
        questions: input.questions,
    };
    let mut run = rag::eval_run(&st.http, &p, sid, &req).await?;
    let Some(id) = run
        .get("id")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
    else {
        tracing::warn!(op = "eval_run", "strata_rag answered 202 without a run id");
        return Err(upstream::unavailable(Service::Rag.name()));
    };
    translate_run(&mut run, &ids);
    let source = run.get("source").cloned().unwrap_or(Value::Null);
    let count = run.get("question_count").cloned().unwrap_or(Value::Null);
    record(
        &st,
        &t,
        Event::new("eval_run.start", "eval_run", id.to_string())
            .detail("root", root_key)
            .detail("source", source)
            .detail("question_count", count),
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        [(header::LOCATION, run_location(id))],
        Json(run),
    )
        .into_response())
}

/// `evalRuns`: newest first; the cursor is strata's, passed both ways untouched.
async fn list_runs(
    Member(t): Member,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<RunsQuery>,
) -> ApiResult<Json<Value>> {
    let (p, _, ids) = rag_plane_and_tree(&st, &t).await?;
    let mut page = rag::eval_runs(&st.http, &p, q.limit, q.cursor.as_deref()).await?;
    if let Some(items) = page.get_mut("items").and_then(Value::as_array_mut) {
        for run in items {
            translate_run(run, &ids);
        }
    }
    Ok(Json(page))
}

/// `evalRun`: one run with its question snapshot and results (polled until `completed` / `failed`).
async fn get_run(
    Member(t): Member,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<Value>> {
    let (p, _, ids) = rag_plane_and_tree(&st, &t).await?;
    let mut run = rag::eval_run_get(&st.http, &p, id).await?;
    translate_run(&mut run, &ids);
    Ok(Json(run))
}
