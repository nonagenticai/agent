//! Job journey reads and tracked activity refinements (B8 J4/J5, D54/D62).
//! Reads compose pinned templates and the selected design; writes return the refreshed draft journey.
//!
//! All seven are `job_designer`, run in the job-scoped transaction after `ensure_job`, and are one tracked draft
//! write each (D54: never live on save, kind `activity`, page J4). They answer the refreshed draft journey. The logic and
//! the refusals live in [`crate::journey::refine`]; the row is B2's `job_overlay_row` object `activity` (Q-R3 (a)).
//!
//! | method | route | refusals |
//! |---|---|---|
//! | `hideJobActivity` | `POST /api/jobs/{job}/refinements/journey/hide` `{key}` | 422 `is_extension`, `required_activity`, `required_operation`; 404 `activity_not_found`; 409 `already_hidden` |
//! | `restoreJobActivity` | `POST …/journey/restore` `{key}` | 409 `not_refined` |
//! | `addJobExtensionActivity` | `POST …/journey/extensions` `{stage, slug, label, actor, trigger?}` | 422 `invalid_key`, `label_required`, `stage_out_of_scope`; 409 `activity_exists` |
//! | `removeJobExtensionActivity` | `DELETE …/journey/extensions/{key}` | 404 `extension_not_found` |
//! | `relabelJobActivity` | `PUT …/journey/labels/{key}` `{label}` | 422 `label_required`; 404 `activity_not_found`; 409 `hidden` |
//! | `refineJobUseCaseCard` | `PUT …/journey/cards/{key}` `{summary?, examples?, metric?, sample_dialogue?}` | 404 `activity_not_found`; 422 `invalid_card`, `scenarios_not_refinable`; 409 `hidden` |
//! | `resetJobUseCaseCard` | `DELETE …/journey/cards/{key}` | 409 `not_refined` |
//!
//! J13's `POST /api/jobs/{job}/refinements/{op}` answers 409 `edit_on_journey_page` for the object `activity`.
use axum::extract::State;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::design::{self, DesignOf, JobRef};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::journey::refine::{self, NewExtension};
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, JobRequires, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/journey", get(job_journey))
        .route(
            "/api/jobs/{job}/journey/activities/{key}",
            get(job_activity),
        )
        .route(
            "/api/jobs/{job}/refinements/journey/hide",
            post(hide_job_activity),
        )
        .route(
            "/api/jobs/{job}/refinements/journey/restore",
            post(restore_job_activity),
        )
        .route(
            "/api/jobs/{job}/refinements/journey/extensions",
            post(add_job_extension_activity),
        )
        .route(
            "/api/jobs/{job}/refinements/journey/extensions/{key}",
            delete(remove_job_extension_activity),
        )
        .route(
            "/api/jobs/{job}/refinements/journey/labels/{key}",
            put(relabel_job_activity),
        )
        .route(
            "/api/jobs/{job}/refinements/journey/cards/{key}",
            put(refine_job_use_case_card).delete(reset_job_use_case_card),
        )
}

#[derive(Deserialize)]
struct KeyBody {
    key: String,
}

#[derive(Deserialize)]
struct LabelBody {
    label: String,
}

/// The job-scoped transaction, with the job's design baseline in place (as J13's routes).
async fn begin(st: &AppState, j: &JobCtx) -> ApiResult<sqlx::Transaction<'static, sqlx::Postgres>> {
    let jr = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    Ok(tx)
}

fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}

#[derive(Default, Deserialize)]
struct OfQuery {
    of: Option<String>,
}

impl OfQuery {
    fn design(&self) -> ApiResult<DesignOf> {
        match self.of.as_deref().unwrap_or("draft") {
            "draft" => Ok(DesignOf::Draft),
            "live" => Ok(DesignOf::Live),
            v if !v.trim().is_empty() => Ok(DesignOf::Release(v.to_owned())),
            _ => Err(ApiError::BadRequest(
                "invalid_of",
                "Name draft, live or a release id.".into(),
            )),
        }
    }
}

async fn job_journey(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<OfQuery>,
) -> ApiResult<Json<Value>> {
    let of = q.design()?;
    let mut tx = begin(&st, &j).await?;
    let view = crate::journey::job::job_journey(&mut tx, &st, &job_ref(&j), &of).await?;
    tx.commit().await?;
    Ok(Json(
        serde_json::to_value(view).map_err(|e| ApiError::Internal(e.into()))?,
    ))
}

async fn job_activity(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_job, key)): ApiPath<(String, String)>,
    ApiQuery(q): ApiQuery<OfQuery>,
) -> ApiResult<Json<Value>> {
    let of = q.design()?;
    let mut tx = begin(&st, &j).await?;
    let view = crate::journey::job::job_activity(&mut tx, &st, &job_ref(&j), &key, &of).await?;
    tx.commit().await?;
    Ok(Json(
        serde_json::to_value(view).map_err(|e| ApiError::Internal(e.into()))?,
    ))
}

async fn answered(
    tx: &mut sqlx::PgConnection,
    st: &AppState,
    j: &JobCtx,
) -> ApiResult<Json<Value>> {
    let view = crate::journey::job::job_journey(tx, st, &job_ref(j), &DesignOf::Draft).await?;
    Ok(Json(
        serde_json::to_value(view).map_err(|e| ApiError::Internal(e.into()))?,
    ))
}

async fn hide_job_activity(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath(_job): ApiPath<String>,
    ApiJson(b): ApiJson<KeyBody>,
) -> ApiResult<Json<Value>> {
    let mut tx = begin(&st, &j).await?;
    refine::hide(&mut tx, &st.design, st.compiler.as_ref(), &j, &b.key).await?;
    let out = answered(&mut tx, &st, &j).await?;
    tx.commit().await?;
    Ok(out)
}

async fn restore_job_activity(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath(_job): ApiPath<String>,
    ApiJson(b): ApiJson<KeyBody>,
) -> ApiResult<Json<Value>> {
    let mut tx = begin(&st, &j).await?;
    refine::restore(&mut tx, &st.design, st.compiler.as_ref(), &j, &b.key).await?;
    let out = answered(&mut tx, &st, &j).await?;
    tx.commit().await?;
    Ok(out)
}

async fn add_job_extension_activity(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath(_job): ApiPath<String>,
    ApiJson(b): ApiJson<NewExtension>,
) -> ApiResult<Json<Value>> {
    let mut tx = begin(&st, &j).await?;
    refine::add_extension(&mut tx, &st.design, st.compiler.as_ref(), &j, b).await?;
    let out = answered(&mut tx, &st, &j).await?;
    tx.commit().await?;
    Ok(out)
}

async fn remove_job_extension_activity(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, key)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let mut tx = begin(&st, &j).await?;
    refine::remove_extension(&mut tx, &st.design, st.compiler.as_ref(), &j, &key).await?;
    let out = answered(&mut tx, &st, &j).await?;
    tx.commit().await?;
    Ok(out)
}

async fn relabel_job_activity(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, key)): ApiPath<(String, String)>,
    ApiJson(b): ApiJson<LabelBody>,
) -> ApiResult<Json<Value>> {
    let mut tx = begin(&st, &j).await?;
    refine::relabel(
        &mut tx,
        &st.design,
        st.compiler.as_ref(),
        &j,
        &key,
        &b.label,
    )
    .await?;
    let out = answered(&mut tx, &st, &j).await?;
    tx.commit().await?;
    Ok(out)
}

async fn refine_job_use_case_card(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, key)): ApiPath<(String, String)>,
    ApiJson(b): ApiJson<Map<String, Value>>,
) -> ApiResult<Json<Value>> {
    let mut tx = begin(&st, &j).await?;
    refine::refine_card(&mut tx, &st.design, st.compiler.as_ref(), &j, &key, &b).await?;
    let out = answered(&mut tx, &st, &j).await?;
    tx.commit().await?;
    Ok(out)
}

async fn reset_job_use_case_card(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, key)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let mut tx = begin(&st, &j).await?;
    refine::reset_card(&mut tx, &st.design, st.compiler.as_ref(), &j, &key).await?;
    let out = answered(&mut tx, &st, &j).await?;
    tx.commit().await?;
    Ok(out)
}
