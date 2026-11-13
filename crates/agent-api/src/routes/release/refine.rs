//! Overlay refinements (B2 T8, spec §4, D8/D54): `refinementLayers`, `refine` and `saveContextSpaceOverride`. The
//! logic lives in [`crate::release::overlay`]; these handlers open the job-scoped transaction, run `ensure_job`, call
//! it and commit.
//!
//! - `GET /api/jobs/{job}/{object}[?layer=base|tenant]` — any member: the fixture `OverlayLayers`
//!   `{object, base, tenant, draft_rev, entities}`. Both sides are always returned in full; `layer` is accepted and
//!   ignored (the web client asks once per layer and merges the two answers). 404 `unknown_object` for anything but
//!   the five overlay objects (static routes such as `/settings` win over `{object}`).
//! - `POST /api/jobs/{job}/refinements/{op}` `{object, key}` — `job_designer`: `{draft_rev}`. 422 `invalid_op`; the
//!   refusals of `overlay::refine`. The same route with `{conflict}` is `resolveConflict` (B2 T13,
//!   [`super::conflicts::resolve`]); a body with neither (or both) shape is 400 `invalid_body`.
//! - `PUT /api/jobs/{job}/context-spaces/{key}` `{label?, required?, entities?}` — `job_designer`: `{draft_rev}`. The
//!   refusals of `overlay::save_context_space`. It is also B5's J12 `saveContextSpaceRequired` (plan B5 R12):
//!   `{required: string[] | null}` alone is recorded under kind `space_required`, and `null` puts the template's
//!   slots back.
//! - `PUT /api/jobs/{job}/instructions/{slot}` `{text, language?}` — `job_designer`: `{draft_rev}` (KH T4, D108). The
//!   refusals of `overlay::save_instruction` (422 `instruction_not_refinable` for any slot but `tone`, `escalation`,
//!   `format`).
use axum::extract::State;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::conflicts;
use crate::design::{self, JobRef};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::release::overlay::{self, ContextSpacePatch, InstructionPatch, Op};
use crate::state::AppState;
use crate::tenancy::{JobMember, JobRequires, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/refinements/{op}", post(refine_place))
        .route(
            "/api/jobs/{job}/context-spaces/{key}",
            put(save_context_space_override),
        )
        .route(
            "/api/jobs/{job}/instructions/{slot}",
            put(save_instruction_override),
        )
        .route("/api/jobs/{job}/{object}", get(refinement_layers))
}

/// `refine` sends `{object, key}`; `resolveConflict` sends `{conflict}` (fixture `ReleaseConflict.id`).
#[derive(Deserialize)]
struct RefineBody {
    object: Option<String>,
    key: Option<String>,
    conflict: Option<i64>,
}

async fn refinement_layers(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_job, object)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let jr = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let out = overlay::layers(&mut tx, &st.design, st.compiler.as_ref(), &jr, &object).await?;
    tx.commit().await?;
    Ok(Json(out))
}

async fn refine_place(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, op)): ApiPath<(String, String)>,
    ApiJson(b): ApiJson<RefineBody>,
) -> ApiResult<Json<Value>> {
    let op = Op::parse(&op)?;
    let jr = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let draft_rev = match (b.conflict, b.object, b.key) {
        (Some(id), None, None) => {
            conflicts::resolve(&mut tx, &st.design, st.compiler.as_ref(), &j, op, id).await?
        }
        (None, Some(object), Some(key)) => {
            overlay::refine(
                &mut tx,
                &st.design,
                st.compiler.as_ref(),
                &j,
                op,
                &object,
                &key,
            )
            .await?
        }
        _ => {
            return Err(ApiError::BadRequest(
                "invalid_body",
                "Send {object, key} to refine, or {conflict} to resolve a conflict.".to_string(),
            ));
        }
    };
    tx.commit().await?;
    Ok(Json(json!({ "draft_rev": draft_rev })))
}

async fn save_context_space_override(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, key)): ApiPath<(String, String)>,
    ApiJson(p): ApiJson<ContextSpacePatch>,
) -> ApiResult<Json<Value>> {
    let jr = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let draft_rev =
        overlay::save_context_space(&mut tx, &st.design, st.compiler.as_ref(), &j, &key, p).await?;
    tx.commit().await?;
    Ok(Json(json!({ "draft_rev": draft_rev })))
}

async fn save_instruction_override(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, slot)): ApiPath<(String, String)>,
    ApiJson(p): ApiJson<InstructionPatch>,
) -> ApiResult<Json<Value>> {
    let jr = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let draft_rev =
        overlay::save_instruction(&mut tx, &st.design, st.compiler.as_ref(), &j, &slot, p).await?;
    tx.commit().await?;
    Ok(Json(json!({ "draft_rev": draft_rev })))
}
