//! Business rules (B2 T9, J9; spec B2 §4), the HTTP side of crate::release::rules:
//!
//! - `GET /api/jobs/{job}/rules` — any member: `Paged<BusinessRule>`.
//! - `GET /api/jobs/{job}/rules/passages` — any member: `PolicyPassage[]` (`[]` until B3).
//! - `PUT /api/jobs/{job}/rules/{id}` `{values}` — job_designer: the rule (a draft `rule_values` write).
//! - `PUT /api/jobs/{job}/rules/{id}/passage` `{passage_id}` — job_designer: the rule (a draft `rule_passage` write).
//! - `POST /api/jobs/{job}/rules/{id}/evaluate` `{facts, values?, as_of?}` — any member: a `RuleEvaluation` dry run
//!   that stores nothing.
//!
//! Each handler: job-scoped transaction → `design::ensure_job` → work → commit (reads commit too: `ensure_job` may
//! insert the job's target row and baseline).
use std::collections::BTreeMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::design::{self, JobRef};
use crate::error::{ApiJson, ApiPath, ApiResult};
use crate::page::paged;
use crate::release::rule_authoring::{self, CreateRule};
use crate::release::rules::{self, DryRun};
use crate::release::rules_eval::RuleEvaluation;
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, JobRequires, role};
use agent_model::kinds::rule_slot::RuleSlotFields;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/jobs/{job}/rules",
            get(business_rules).post(create_rule),
        )
        .route("/api/jobs/{job}/rules/passages", get(rule_passages))
        .route(
            "/api/jobs/{job}/rules/{id}",
            put(save_business_rule).delete(delete_rule),
        )
        .route(
            "/api/jobs/{job}/rules/{id}/definition",
            put(update_rule_definition),
        )
        .route("/api/jobs/{job}/rules/{id}/passage", put(link_rule_passage))
        .route("/api/jobs/{job}/rules/{id}/evaluate", post(evaluate_rule))
}

async fn create_rule(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiJson(body): ApiJson<CreateRule>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let saved = rule_authoring::save(
        &mut tx,
        &st.design,
        st.compiler.as_ref(),
        &j,
        &body.id,
        body.definition,
        true,
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(saved)))
}

async fn update_rule_definition(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, id)): ApiPath<(String, String)>,
    ApiJson(body): ApiJson<RuleSlotFields>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let saved = rule_authoring::save(
        &mut tx,
        &st.design,
        st.compiler.as_ref(),
        &j,
        &id,
        body,
        false,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(saved))
}

async fn delete_rule(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, id)): ApiPath<(String, String)>,
) -> ApiResult<StatusCode> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    rule_authoring::remove(&mut tx, &st.design, &j, &id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}

async fn business_rules(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let items = rules::list(&mut tx, &st.design, st.compiler.as_ref(), &jr).await?;
    tx.commit().await?;
    Ok(Json(paged(items, None)))
}

async fn rule_passages(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let items = rules::passages(&mut tx, &jr).await?;
    tx.commit().await?;
    Ok(Json(Value::Array(items)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveBody {
    values: BTreeMap<String, Option<String>>,
}

async fn save_business_rule(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, id)): ApiPath<(String, String)>,
    ApiJson(body): ApiJson<SaveBody>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let rule = rules::save(
        &mut tx,
        &st.design,
        st.compiler.as_ref(),
        &j,
        &id,
        &body.values,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(rule))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PassageBody {
    passage_id: Option<String>,
}

async fn link_rule_passage(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, id)): ApiPath<(String, String)>,
    ApiJson(body): ApiJson<PassageBody>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let rule = rules::link_passage(
        &mut tx,
        &st.design,
        st.compiler.as_ref(),
        &j,
        &id,
        body.passage_id.as_deref(),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(rule))
}

async fn evaluate_rule(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_job, id)): ApiPath<(String, String)>,
    ApiJson(body): ApiJson<DryRun>,
) -> ApiResult<Json<RuleEvaluation>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let out = rules::evaluate(&mut tx, &st.design, st.compiler.as_ref(), &jr, &id, &body).await?;
    tx.commit().await?;
    Ok(Json(out))
}
