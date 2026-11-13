//! The release list and the cut (B2 T11, spec §4/§5, D54/D62):
//!
//! - `GET /api/jobs/{job}/releases` — any member: `Paged<ReleaseRow>`, newest first.
//! - `POST /api/jobs/{job}/releases` `{apply, selection_seen_at, adopt_pins?}` — `release_manager`: approve (cut);
//!   201 with a new release, 200 when the newest release already had the same hash (`ApproveResult`).
//! - `GET /api/jobs/{job}/releases/status` — any member: `ReleaseStatus` (static, so it wins over `{id}`).
//! - `GET /api/jobs/{job}/releases/{id}` — any member: `ReleaseDetail`; 404 `release_not_found`.
//! - `GET /api/jobs/{job}/releases/{id}/compare?with=current` — any member: `ReleaseCompare`.
//! - `GET /api/jobs/{job}/releases/{id}/design` — any member: the release's `DesignState`; 422 `bundle_invalid`.
//!
//! Every handler runs in a job-scoped transaction and calls `design::ensure_job` first. Approve commits, then runs
//! the `ReleaseApproved` hooks on a spawned task (crate::release::hooks): they can neither undo nor block it.
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::design::{self, JobRef};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::page::paged;
use crate::release::approve::{self, ApproveInput};
use crate::release::hooks::Approved;
use crate::release::summary;
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, JobRequires, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/jobs/{job}/releases",
            get(release_list).post(approve_release),
        )
        .route("/api/jobs/{job}/releases/status", get(release_status))
        .route("/api/jobs/{job}/releases/{id}", get(release_detail))
        .route(
            "/api/jobs/{job}/releases/{id}/compare",
            get(release_compare),
        )
        .route("/api/jobs/{job}/releases/{id}/design", get(release_design))
}

fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}

async fn release_list(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let items = summary::rows(&mut tx, &jr).await?;
    tx.commit().await?;
    Ok(Json(paged(items, None)))
}

async fn approve_release(
    JobRequires(j, _): JobRequires<role::ReleaseManager>,
    State(st): State<AppState>,
    ApiJson(input): ApiJson<ApproveInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let jr = job_ref(&j);
    crate::knowledge::packs::resolve::prepare_cut(
        &st,
        j.t.tenant,
        &jr,
        input.adopt_pins.as_deref(),
    )
    .await?;
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let cut = approve::approve(&mut tx, &st, &j, &input).await?;
    let release = summary::row(&mut tx, &jr, &cut.release_id).await?;
    tx.commit().await?;
    if cut.created {
        // after the COMMIT: a hook sees the committed release and cannot undo or block the approve
        st.release_approved.spawn_after_commit(
            st.db.clone(),
            Approved {
                tenant: j.t.tenant,
                job: jr,
                release_id: cut.release_id.clone(),
                by: j.t.actor(),
            },
        );
    }
    let status = if cut.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(json!({"release": release, "created": cut.created, "applied": cut.applied})),
    ))
}

async fn release_status(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let v = summary::status(
        &mut tx,
        &st.design,
        st.compiler.as_ref(),
        &st.readiness,
        &jr,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(v))
}

async fn release_detail(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_job, id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let v = summary::detail(&mut tx, &st.design, st.compiler.as_ref(), &jr, &id).await?;
    tx.commit().await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
struct CompareQuery {
    /// Only `current` (the fixture's one comparison); absent = `current`.
    #[serde(default)]
    with: Option<String>,
}

async fn release_compare(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_job, id)): ApiPath<(String, String)>,
    ApiQuery(q): ApiQuery<CompareQuery>,
) -> ApiResult<Json<Value>> {
    if q.with.as_deref().is_some_and(|w| w != "current") {
        return Err(ApiError::BadRequest(
            "invalid_query",
            "with must be current.".into(),
        ));
    }
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let v = summary::compare(&mut tx, &jr, &id).await?;
    tx.commit().await?;
    Ok(Json(v))
}

async fn release_design(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_job, id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let v = summary::release_design(&mut tx, &st.design, &jr, &id).await?;
    tx.commit().await?;
    Ok(Json(v))
}
