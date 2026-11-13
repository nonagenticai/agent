//! Release readiness (B2 T10, D67, J3), the HTTP side of crate::readiness:
//!
//! - `GET /api/jobs/{job}/release-readiness?of=release|draft` — any member: `ReleaseReadiness`.
//! - `GET /api/jobs/{job}/readiness/board?of=release|draft` — any member: `ReadinessBoard`.
//!
//! `of` defaults to `release` (the newest approved release, else the current one); anything else is 422
//! `invalid_of`. Both read only: job-scoped transaction → `design::ensure_job` → readiness → commit (`ensure_job` may
//! insert the job's target row and baseline).
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use crate::design::{self, JobRef};
use crate::error::{ApiQuery, ApiResult};
use crate::readiness::{self, ReadOf, ReadinessBoard, ReleaseReadiness};
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/release-readiness", get(release_readiness))
        .route("/api/jobs/{job}/readiness/board", get(readiness_board))
}

fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}

#[derive(Deserialize)]
struct OfQuery {
    of: Option<String>,
}

async fn release_readiness(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<OfQuery>,
) -> ApiResult<Json<ReleaseReadiness>> {
    let of = ReadOf::parse(q.of.as_deref())?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let checked = readiness::checked(&mut tx, st.compiler.as_ref(), &jr, of).await?;
    let out = st
        .readiness
        .release_readiness(&mut tx, &st.design, st.compiler.as_ref(), &checked)
        .await?;
    tx.commit().await?;
    Ok(Json(out))
}

async fn readiness_board(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<OfQuery>,
) -> ApiResult<Json<ReadinessBoard>> {
    let of = ReadOf::parse(q.of.as_deref())?;
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let checked = readiness::checked(&mut tx, st.compiler.as_ref(), &jr, of).await?;
    let out = st
        .readiness
        .board(&mut tx, &st.design, st.compiler.as_ref(), &checked)
        .await?;
    tx.commit().await?;
    Ok(Json(out))
}
