//! `/api/knowledge/model-changes`: model changes per job (B3 T4a; D83, D92, D93; plan §5.8). The logic lives in
//! crate::knowledge::model_changes; generations and pins in crate::knowledge::{generations, targets}.
//!
//! - `GET /api/knowledge/model-changes?state=` (member) → the tenant's changes, newest first, each with its jobs'
//!   states; 422 `invalid_state`.
//! - `GET /api/knowledge/model-changes/{id}` (member) → the change with every affected job's part (`stores` with
//!   their from / to generations, chunks and `missing_originals`, `live_release`, `draft_rev`); 404
//!   `model_change_not_found`. Rebuild states are refreshed from strata first (D92).
//! - `POST /api/knowledge/model-changes/{id}/jobs/{job}/reembed` (knowledge_manager; a person's click, the only way a
//!   rebuild starts) → the job's part. 404; 409 `already_reembedding`, `already_accepted`, `change_superseded`,
//!   `provision_pending`; 503 `unavailable` (strata down, or a rebuild route strata does not serve: the change stays
//!   `pending`).
//! - `POST /api/knowledge/model-changes/{id}/jobs/{job}/accept[?acknowledge_missing=true]` (knowledge_manager) → the
//!   job's part, its `embedding_pin` drafts written (D54: `design::track`). 404; 409 `generation_not_ready` +
//!   `stores`, `originals_not_retained` + `documents` (unless acknowledged), `already_accepted`, `change_superseded`
//!   (a newer save of the slot dismissed the change).
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::error::{ApiError, ApiPath, ApiQuery, ApiResult};
use crate::knowledge::model_changes::{self, STATES};
use crate::state::AppState;
use crate::tenancy::{JobRequires, Member, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/knowledge/model-changes", get(list))
        .route("/api/knowledge/model-changes/{id}", get(one))
        .route(
            "/api/knowledge/model-changes/{id}/jobs/{job}/reembed",
            post(reembed),
        )
        .route(
            "/api/knowledge/model-changes/{id}/jobs/{job}/accept",
            post(accept),
        )
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    state: Option<String>,
}

#[derive(Deserialize)]
struct AcceptQuery {
    #[serde(default)]
    acknowledge_missing: bool,
}

async fn list(
    Member(t): Member,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Value>> {
    let state = q.state.as_deref().filter(|s| !s.is_empty());
    if let Some(s) = state
        && !STATES.contains(&s)
    {
        return Err(ApiError::Unprocessable(
            "invalid_state",
            format!("A model change state is one of {}.", STATES.join(", ")),
        ));
    }
    model_changes::refresh(&st, t.tenant).await?;
    let mut tx = t.begin(&st.db).await?;
    let out = model_changes::list(&mut tx, state).await?;
    tx.commit().await?;
    Ok(Json(Value::Array(out)))
}

async fn one(
    Member(t): Member,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<Value>> {
    model_changes::refresh(&st, t.tenant).await?;
    let mut tx = t.begin(&st.db).await?;
    let v = model_changes::view(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Json(v))
}

async fn reembed(
    JobRequires(j, _): JobRequires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiPath((id, _job)): ApiPath<(Uuid, String)>,
) -> ApiResult<Json<Value>> {
    Ok(Json(model_changes::reembed(&st, &j, id).await?))
}

async fn accept(
    JobRequires(j, _): JobRequires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiPath((id, _job)): ApiPath<(Uuid, String)>,
    ApiQuery(q): ApiQuery<AcceptQuery>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        model_changes::accept(&st, &st.design, &j, id, q.acknowledge_missing).await?,
    ))
}
