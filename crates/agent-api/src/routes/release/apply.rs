//! Apply / rollback and its preview (B2 T12, spec §4/§5, D62):
//!
//! - `POST /api/jobs/{job}/releases/{id}/apply` — `release_manager`: make release `id` current (a cut, a rollback to
//!   an older release or a re-apply of a newer one); 200 `ApplyResult`. 404 `release_not_found`; 409
//!   `tenant_suspended`, `release_not_ready` (+`blocking`); 422 `bundle_invalid`. No body.
//! - `GET /api/jobs/{job}/releases/{id}/rollback-preview` — any member: `RollbackPreview`. 404; 409
//!   `already_current`; 422 `bundle_invalid`.
//!
//! Both run in one job-scoped transaction after `design::ensure_job`.
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;

use crate::design::{self, JobRef};
use crate::error::{ApiPath, ApiResult};
use crate::release::apply;
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, JobRequires, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/releases/{id}/apply", post(apply_release))
        .route(
            "/api/jobs/{job}/releases/{id}/rollback-preview",
            get(rollback_preview),
        )
}

fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}

async fn apply_release(
    JobRequires(j, _): JobRequires<role::ReleaseManager>,
    State(st): State<AppState>,
    ApiPath((_job, id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let done = apply::apply(&mut tx, &st, &j, &id).await?;
    let v = apply::result(&mut tx, &jr, &done).await?;
    tx.commit().await?;
    Ok(Json(v))
}

async fn rollback_preview(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_job, id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let v = apply::rollback_preview(&mut tx, &st, &jr, &id).await?;
    tx.commit().await?;
    Ok(Json(v))
}
