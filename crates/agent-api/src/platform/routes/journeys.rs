//! B8 phase 2 T4 / T4a / T4b: the platform journey designer routes (plan B8 route table, Revision 2026-10-25 P11).
//! Every route is `SuperAdmin` (403 `super_admin_required`), takes no `X-Active-Tenant-Id` and runs on the platform
//! plane. A journey is addressed by industry AND department (B8-6 amendment): every route takes
//! `?department=<dept>`, optional while the industry has one department layer, else 422 `department_required`.
//!
//! - reads: `journeyIndustries` (one row per department), `industryJourney` (the grid with findings), in one
//!   [`read_tx`];
//! - writes: [`write_tx`], one `journey::designer` function (revisions through `write_authored`, one `journey.*`
//!   audit row), commit; the answer is the refreshed `IndustryJourney` (creates answer 201). `addJourneyStage` with
//!   `{industry}` = `base` writes the base layer and answers the stage written.
//!
//! `{op}` of the operation routes is `object.verb{variant}`; axum percent-decodes the segment, so a client may send
//! it encoded (`case.create%7Breturn%7D`).
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Who, read_tx, write_tx};
use crate::error::{ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::journey::designer::{
    self, ActivityEdit, BindBody, ChannelBody, KindBody, MessagesBody, MoveBody, NewActivity,
    NewStage, NewTheme, OperationBody, OrderBody, PriorBody, StageEdit, ThemeEdit,
};
use crate::journey::grid;
use crate::journey::model::IndustryJourney;
use crate::page::paged;
use crate::state::AppState;
use crate::tenancy::SuperAdmin;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/platform/journeys/{industry}/activities/{key}/card",
            put(update_use_case_card),
        )
        .route("/api/platform/journeys", get(journey_industries))
        .route("/api/platform/journeys/{industry}", get(industry_journey))
        .route(
            "/api/platform/journeys/{industry}/stages",
            post(add_journey_stage),
        )
        .route(
            "/api/platform/journeys/{industry}/stages/order",
            put(reorder_journey_stages),
        )
        .route(
            "/api/platform/journeys/{industry}/stages/{stage}",
            patch(update_journey_stage),
        )
        .route(
            "/api/platform/journeys/{industry}/stages/{stage}/knowledge-kinds/{kind}",
            put(set_stage_knowledge_kind).delete(remove_stage_knowledge_kind),
        )
        .route(
            "/api/platform/journeys/{industry}/stages/{stage}/messages/{channel}",
            put(set_stage_messages),
        )
        .route(
            "/api/platform/journeys/{industry}/priors/{theme}/{stage}",
            put(set_theme_stage_prior),
        )
        .route(
            "/api/platform/journeys/{industry}/themes",
            post(add_journey_theme),
        )
        .route(
            "/api/platform/journeys/{industry}/themes/{theme}",
            patch(update_journey_theme),
        )
        .route(
            "/api/platform/journeys/{industry}/channels/{channel}",
            put(set_journey_channel_status),
        )
        .route(
            "/api/platform/journeys/{industry}/activities",
            post(add_journey_activity),
        )
        .route(
            "/api/platform/journeys/{industry}/activities/{key}",
            patch(update_journey_activity).delete(retire_journey_activity),
        )
        .route(
            "/api/platform/journeys/{industry}/activities/{key}/move",
            post(move_journey_activity),
        )
        .route(
            "/api/platform/journeys/{industry}/activities/{key}/objects/{object}",
            put(bind_activity_object).delete(unbind_activity_object),
        )
        .route(
            "/api/platform/journeys/{industry}/activities/{key}/operations/{op}",
            put(set_activity_operation).delete(remove_activity_operation),
        )
}

/// `?department=`.
#[derive(Debug, Default, Deserialize)]
struct Dept {
    department: Option<String>,
}

/// `?department=` and, on `removeStageKnowledgeKind`, `?layer=` (the layer to remove the kind at).
#[derive(Debug, Default, Deserialize)]
struct DeptLayer {
    department: Option<String>,
    layer: Option<String>,
}

// --- reads -----------------------------------------------------------------------------------------------------------

/// `GET /api/platform/journeys` — `{items, next}`, one row per industry × department (an industry without a
/// department layer is one row with `department: null`), not-started ones included.
async fn journey_industries(_sa: SuperAdmin, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = read_tx(&st).await?;
    let industries = grid::industries(&mut tx).await?;
    let mut items: Vec<Value> = Vec::new();
    for i in &industries {
        if i.departments.is_empty() {
            items.push(json!({
                "industry": i.industry,
                "label": i.label,
                "department": null,
                "department_label": null,
                "status": i.status,
                "tag": i.tag,
                "dept_tag": null,
                "activities": 0,
                "red": 0,
            }));
        }
        for d in &i.departments {
            items.push(json!({
                "industry": i.industry,
                "label": i.label,
                "department": d.department,
                "department_label": d.label,
                "status": d.status,
                "tag": i.tag,
                "dept_tag": d.dept_tag,
                "activities": d.activities,
                "red": d.red,
            }));
        }
    }
    Ok(Json(paged(items, None)))
}

/// `GET /api/platform/journeys/{industry}` — the grid with its findings. 404 `journey_not_found`; 422
/// `department_required`.
async fn industry_journey(
    _sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(industry): ApiPath<String>,
    ApiQuery(q): ApiQuery<Dept>,
) -> ApiResult<Json<IndustryJourney>> {
    let mut tx = read_tx(&st).await?;
    let t = designer::resolve(&mut tx, &industry, q.department.as_deref(), None).await?;
    Ok(Json(grid::load(&mut tx, &t.industry, &t.department).await?))
}

// --- stages, priors ----------------------------------------------------------------------------------------------------

/// `PATCH …/stages/{stage}` — label, description, synonyms, out_of_scope.
async fn update_journey_stage(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((industry, stage)): ApiPath<(String, String)>,
    ApiQuery(q): ApiQuery<Dept>,
    ApiJson(b): ApiJson<StageEdit>,
) -> ApiResult<Json<IndustryJourney>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out = designer::update_stage(&mut tx, &who, &industry, q.department.as_deref(), &stage, b)
        .await?;
    tx.commit().await?;
    Ok(Json(out))
}

/// `PUT …/priors/{theme}/{stage}` `{weight}`.
async fn set_theme_stage_prior(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((industry, theme, stage)): ApiPath<(String, String, String)>,
    ApiQuery(q): ApiQuery<Dept>,
    ApiJson(b): ApiJson<PriorBody>,
) -> ApiResult<Json<IndustryJourney>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out = designer::set_prior(
        &mut tx,
        &who,
        &industry,
        q.department.as_deref(),
        &theme,
        &stage,
        b.weight,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(out))
}

/// `POST …/stages` `{key, label, description, after?}` — `{industry}` = `base` adds at base.
async fn add_journey_stage(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(industry): ApiPath<String>,
    ApiQuery(q): ApiQuery<Dept>,
    ApiJson(b): ApiJson<NewStage>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out = if industry == "base" {
        designer::add_base_stage(&mut tx, &who, b).await?
    } else {
        let j = designer::add_stage(&mut tx, &who, &industry, q.department.as_deref(), b).await?;
        serde_json::to_value(j).map_err(|e| anyhow::anyhow!("journey to JSON: {e}"))?
    };
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

/// `PUT …/stages/order` `{stages: key[]}`.
async fn reorder_journey_stages(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(industry): ApiPath<String>,
    ApiQuery(q): ApiQuery<Dept>,
    ApiJson(b): ApiJson<OrderBody>,
) -> ApiResult<Json<IndustryJourney>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out =
        designer::reorder_stages(&mut tx, &who, &industry, q.department.as_deref(), b).await?;
    tx.commit().await?;
    Ok(Json(out))
}

// --- themes ------------------------------------------------------------------------------------------------------------

/// `POST …/themes` `{key, label, escalation_weight}`.
async fn add_journey_theme(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(industry): ApiPath<String>,
    ApiQuery(q): ApiQuery<Dept>,
    ApiJson(b): ApiJson<NewTheme>,
) -> ApiResult<(StatusCode, Json<IndustryJourney>)> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out = designer::add_theme(&mut tx, &who, &industry, q.department.as_deref(), b).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

/// `PATCH …/themes/{theme}` `{label?, order?, escalation_weight?}`.
async fn update_journey_theme(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((industry, theme)): ApiPath<(String, String)>,
    ApiQuery(q): ApiQuery<Dept>,
    ApiJson(b): ApiJson<ThemeEdit>,
) -> ApiResult<Json<IndustryJourney>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out = designer::update_theme(&mut tx, &who, &industry, q.department.as_deref(), &theme, b)
        .await?;
    tx.commit().await?;
    Ok(Json(out))
}

// --- knowledge kinds, messages, channels --------------------------------------------------------------------------------

/// `PUT …/stages/{stage}/knowledge-kinds/{kind}` `{store_key, required, tenant_data?, layer?}`.
async fn set_stage_knowledge_kind(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((industry, stage, kind)): ApiPath<(String, String, String)>,
    ApiQuery(q): ApiQuery<Dept>,
    ApiJson(b): ApiJson<KindBody>,
) -> ApiResult<Json<IndustryJourney>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out = designer::set_kind(
        &mut tx,
        &who,
        &industry,
        q.department.as_deref(),
        &stage,
        &kind,
        b,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(out))
}

/// `DELETE …/stages/{stage}/knowledge-kinds/{kind}[?layer=]`.
async fn remove_stage_knowledge_kind(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((industry, stage, kind)): ApiPath<(String, String, String)>,
    ApiQuery(q): ApiQuery<DeptLayer>,
) -> ApiResult<Json<IndustryJourney>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out = designer::remove_kind(
        &mut tx,
        &who,
        &industry,
        q.department.as_deref(),
        &stage,
        &kind,
        q.layer.as_deref(),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(out))
}

/// `PUT …/stages/{stage}/messages/{channel}` `{utterances: string[]}`.
async fn set_stage_messages(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((industry, stage, channel)): ApiPath<(String, String, String)>,
    ApiQuery(q): ApiQuery<Dept>,
    ApiJson(b): ApiJson<MessagesBody>,
) -> ApiResult<Json<IndustryJourney>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out = designer::set_messages(
        &mut tx,
        &who,
        &industry,
        q.department.as_deref(),
        &stage,
        &channel,
        b,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(out))
}

/// `PUT …/channels/{channel}` `{status}`.
async fn set_journey_channel_status(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((industry, channel)): ApiPath<(String, String)>,
    ApiQuery(q): ApiQuery<Dept>,
    ApiJson(b): ApiJson<ChannelBody>,
) -> ApiResult<Json<IndustryJourney>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out = designer::set_channel_status(
        &mut tx,
        &who,
        &industry,
        q.department.as_deref(),
        &channel,
        b,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(out))
}

// --- activities ---------------------------------------------------------------------------------------------------------

/// `POST …/activities` `{stage, slug, label, actor, trigger}`.
async fn add_journey_activity(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(industry): ApiPath<String>,
    ApiQuery(q): ApiQuery<Dept>,
    ApiJson(b): ApiJson<NewActivity>,
) -> ApiResult<(StatusCode, Json<IndustryJourney>)> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out = designer::add_activity(&mut tx, &who, &industry, q.department.as_deref(), b).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

/// `PATCH …/activities/{key}` `{label?, trigger?, actor?, also_in_stages?}`.
async fn update_journey_activity(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((industry, key)): ApiPath<(String, String)>,
    ApiQuery(q): ApiQuery<Dept>,
    ApiJson(b): ApiJson<ActivityEdit>,
) -> ApiResult<Json<IndustryJourney>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out = designer::update_activity(&mut tx, &who, &industry, q.department.as_deref(), &key, b)
        .await?;
    tx.commit().await?;
    Ok(Json(out))
}

/// `DELETE …/activities/{key}`.
async fn retire_journey_activity(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((industry, key)): ApiPath<(String, String)>,
    ApiQuery(q): ApiQuery<Dept>,
) -> ApiResult<Json<IndustryJourney>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out =
        designer::retire_activity(&mut tx, &who, &industry, q.department.as_deref(), &key).await?;
    tx.commit().await?;
    Ok(Json(out))
}

/// `POST …/activities/{key}/move` `{stage}`.
async fn move_journey_activity(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((industry, key)): ApiPath<(String, String)>,
    ApiQuery(q): ApiQuery<Dept>,
    ApiJson(b): ApiJson<MoveBody>,
) -> ApiResult<Json<IndustryJourney>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out =
        designer::move_activity(&mut tx, &who, &industry, q.department.as_deref(), &key, b).await?;
    tx.commit().await?;
    Ok(Json(out))
}

/// `PUT …/activities/{key}/objects/{object}` `{role, fields, required_inputs}`.
async fn bind_activity_object(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((industry, key, object)): ApiPath<(String, String, String)>,
    ApiQuery(q): ApiQuery<Dept>,
    ApiJson(b): ApiJson<BindBody>,
) -> ApiResult<Json<IndustryJourney>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out = designer::bind_object(
        &mut tx,
        &who,
        &industry,
        q.department.as_deref(),
        &key,
        &object,
        b,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(out))
}

/// `DELETE …/activities/{key}/objects/{object}`.
async fn unbind_activity_object(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((industry, key, object)): ApiPath<(String, String, String)>,
    ApiQuery(q): ApiQuery<Dept>,
) -> ApiResult<Json<IndustryJourney>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out = designer::unbind_object(
        &mut tx,
        &who,
        &industry,
        q.department.as_deref(),
        &key,
        &object,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(out))
}

// --- operation expectations (T4b) ----------------------------------------------------------------------------------------

/// `PUT …/activities/{key}/operations/{op}` `{expectation: R|O}`.
async fn set_activity_operation(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((industry, key, op)): ApiPath<(String, String, String)>,
    ApiQuery(q): ApiQuery<Dept>,
    ApiJson(b): ApiJson<OperationBody>,
) -> ApiResult<Json<IndustryJourney>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out = designer::set_operation(
        &mut tx,
        &who,
        &industry,
        q.department.as_deref(),
        &key,
        &op,
        b,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(out))
}

/// `DELETE …/activities/{key}/operations/{op}`.
async fn remove_activity_operation(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((industry, key, op)): ApiPath<(String, String, String)>,
    ApiQuery(q): ApiQuery<Dept>,
) -> ApiResult<Json<IndustryJourney>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out =
        designer::remove_operation(&mut tx, &who, &industry, q.department.as_deref(), &key, &op)
            .await?;
    tx.commit().await?;
    Ok(Json(out))
}

async fn update_use_case_card(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((industry, key)): ApiPath<(String, String)>,
    ApiQuery(q): ApiQuery<Dept>,
    ApiJson(b): ApiJson<serde_json::Map<String, Value>>,
) -> ApiResult<Json<IndustryJourney>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let out =
        designer::update_card(&mut tx, &who, &industry, q.department.as_deref(), &key, b).await?;
    tx.commit().await?;
    Ok(Json(out))
}
