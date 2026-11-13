//! `GET /api/templates?kind=job` (B1 T11): the job template tags a super_admin creates jobs from (P6 tags; K11), newest
//! first. Answers the superset of `JobTemplateTag` (platform) and `JobTemplateOption` (tenant):
//! `{layer, version, key, label, industry, pins, status}`. Drafts are listed (the UI greys them); creating from one is
//! 409 `template_not_tagged`. A retired tag is not listed (422 `template_retired` at creation).
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{ApiError, ApiQuery, ApiResult};
use crate::state::AppState;
use crate::tenancy::SuperAdmin;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/templates", get(list))
}

#[derive(Deserialize)]
struct Query {
    kind: Option<String>,
}

type TagRow = (String, i32, String, String, Option<String>, Value, String);

async fn list(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<Query>,
) -> ApiResult<Json<Value>> {
    if q.kind.as_deref() != Some("job") {
        return Err(ApiError::Unprocessable(
            "invalid_kind",
            "kind must be job".into(),
        ));
    }
    // job_template_tag is readable by agent_app (platform data every tenant reads).
    let rows: Vec<TagRow> = sqlx::query_as(
        "SELECT layer, version, key, label, industry, pins, status FROM job_template_tag WHERE retired_at IS NULL \
          ORDER BY created_at DESC, layer, version DESC",
    )
    .fetch_all(&st.db)
    .await?;
    Ok(Json(Value::Array(
        rows.into_iter()
            .map(|(layer, version, key, label, industry, pins, status)| {
                json!({"layer": layer, "version": version, "key": key, "label": label,
                       "industry": industry, "pins": pins, "status": status})
            })
            .collect(),
    )))
}
