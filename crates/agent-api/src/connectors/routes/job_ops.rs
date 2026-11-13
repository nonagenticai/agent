//! A job's own operations (0246, design kind `job_operation`): a tenant adds the operations of its own connector
//! templates (an MCP server, an ERP, a CRM, any backend) to its own job, with no platform step (owner 2026-10-31: the
//! platform is general purpose; a tenant's tools are never platform catalogue).
//!
//! | Route | Role | Notes |
//! |---|---|---|
//! | `GET /api/jobs/{job}/own-operations` | job member | `{ops: [{op, value}], available: [..]}` (draft) |
//! | `PUT /api/jobs/{job}/own-operations/{op}` `{template, version, identity_level}` | job_designer | design write |
//! | `DELETE /api/jobs/{job}/own-operations/{op}` | job_designer | 404 `job_operation_not_found` |
//!
//! `available` lists every op of the tenant's templates that are not retired: `{template, version, op, own,
//! side_effect, input_fields, output_fields, description, added}` (`own`: the template defines it; else the platform
//! catalogue does). A PUT copies the op's definition from `template@version` into the job's draft at the chosen
//! identity level: a later template edit never changes the job until it is added again. 404 `template_not_found`,
//! 404 `operation_not_in_template`, 409 `template_retired`, 422 `invalid_body`. The job op is then routed, declared
//! and released like any operation of the job (`/api/jobs/{job}/operations…`).
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{self, Event};
use crate::connectors::model::{OpDef, TenantTemplateOp};
use crate::connectors::opkey::{op_key, parse_op_key, same_op};
use crate::connectors::tenant_templates::{self as db, def_of, op_defs};
use crate::design::{self, DraftWrite, Label};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::job_design::targets::job_ops::{self, JobOp, KIND};
use crate::state::AppState;
use crate::tenancy::{JobMember, JobRequires, role};
use agent_model::kinds::catalog::IdentityLevel;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/own-operations", get(list))
        .route(
            "/api/jobs/{job}/own-operations/{op}",
            axum::routing::put(add).delete(remove),
        )
}

const AREA: &str = "tools";
const PAGE: &str = "J6";

#[derive(Deserialize)]
struct OpPath {
    op: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddBody {
    template: String,
    version: String,
    identity_level: IdentityLevel,
}

fn canonical(raw: &str) -> ApiResult<String> {
    let o = parse_op_key(raw);
    if o.object.is_empty() {
        return Err(ApiError::Unprocessable(
            "invalid_body",
            format!("{} is not an operation key (object.verb).", raw.trim()),
        ));
    }
    Ok(op_key(&o))
}

/// The definition of template op `o`: the platform's for a platform op, else the template's own.
fn definition(o: &TenantTemplateOp) -> Option<(OpDef, bool)> {
    match def_of(op_defs(), &o.op_ref()) {
        Some(d) => Some((d.clone(), false)),
        None => o.def.as_ref().map(|d| (db::own_op_def(o, d), true)),
    }
}

fn description(o: &TenantTemplateOp) -> String {
    o.def
        .as_ref()
        .map(|d| d.description.clone())
        .unwrap_or_default()
}

/// The job's draft own ops, op key → value.
async fn draft_ops(c: &mut PgConnection, job: Uuid) -> ApiResult<Vec<(String, Value)>> {
    Ok(
        sqlx::query_as("SELECT op, value FROM job_operation WHERE job_id = $1 ORDER BY op")
            .bind(job)
            .fetch_all(&mut *c)
            .await?,
    )
}

async fn list(JobMember(j): JobMember, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let ops = draft_ops(&mut tx, j.job_id).await?;
    let templates = db::list(&mut tx).await?;
    tx.commit().await?;
    let mut available = Vec::new();
    for t in templates.iter().filter(|t| t.status != "retired") {
        let m = t.model()?;
        for o in &m.ops {
            let Some((d, own)) = definition(o) else {
                continue;
            };
            let key = op_key(&o.op_ref());
            available.push(json!({
                "template": t.key,
                "version": t.version,
                "status": t.status,
                "op": key,
                "own": own,
                "side_effect": d.side_effect,
                "input_fields": d.input_fields,
                "output_fields": d.output_fields,
                "description": description(o),
                "added": ops.iter().any(|(k, _)| *k == key),
            }));
        }
    }
    Ok(Json(json!({
        "ops": ops.into_iter().map(|(op, value)| json!({"op": op, "value": value})).collect::<Vec<_>>(),
        "available": available,
    })))
}

fn write(r: design::DraftRef, label: String) -> DraftWrite {
    DraftWrite {
        area: AREA,
        page: PAGE,
        kind: KIND,
        r#ref: r,
        label: Label::Now(label.chars().take(400).collect()),
    }
}

async fn lock_job(c: &mut PgConnection, job: Uuid) -> ApiResult<i64> {
    sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(job)
        .fetch_optional(&mut *c)
        .await?
        .ok_or_else(|| ApiError::Missing("job_not_found", format!("no job {job}")))
}

async fn add(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<OpPath>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    let key = canonical(&p.op)?;
    let b: AddBody = serde_json::from_value(v).map_err(|e| {
        ApiError::Unprocessable(
            "invalid_body",
            format!("The request body is not valid: {e}"),
        )
    })?;
    let mut tx = j.begin(&st.db).await?;
    let previous_rev = lock_job(&mut tx, j.job_id).await?;
    let t = db::require(&mut tx, b.template.trim(), b.version.trim()).await?;
    if t.status == "retired" {
        return Err(ApiError::Conflict(
            "template_retired",
            format!(
                "{}@{} is retired: add the op from a current version.",
                t.key, t.version
            ),
        ));
    }
    let m = t.model()?;
    let wanted = parse_op_key(&key);
    let (o, (d, _)) = m
        .ops
        .iter()
        .find(|o| same_op(&o.op_ref(), &wanted))
        .and_then(|o| definition(o).map(|d| (o, d)))
        .ok_or_else(|| {
            ApiError::Missing(
                "operation_not_in_template",
                format!("{}@{} has no operation {key}.", t.key, t.version),
            )
        })?;
    let value = JobOp {
        identity_level: b.identity_level,
        side_effect: d.side_effect,
        input_fields: d.input_fields,
        output_fields: d.output_fields,
        description: description(o),
        template: format!("{}@{}", t.key, t.version),
        input_types: o.input_types.clone(),
    };
    let label = format!(
        "{key}: added from {} at level {}",
        value.template,
        crate::runtime::catalog::level_wire(value.identity_level)
    );
    let (job, by, row_key, row) = (j.job_id, j.t.caller.sub.clone(), key.clone(), value.clone());
    let out = design::track::<(), _>(
        &mut tx,
        &st.design,
        &j,
        write(job_ops::op_ref(&key), label),
        move |c| Box::pin(async move { job_ops::write(c, job, &row_key, &row, &by).await }),
    )
    .await?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("job_operation.update", KIND, key.clone())
            .job(&j.job_key)
            .detail("change", "set")
            .detail("template", value.template.clone()),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "op": key,
        "value": value,
        "draft_rev": out.draft_rev,
        "previous_rev": previous_rev,
    })))
}

async fn remove(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<OpPath>,
) -> ApiResult<Json<Value>> {
    let key = canonical(&p.op)?;
    let err = ApiError::Missing(
        "job_operation_not_found",
        format!("{key} is not one of the job's own operations."),
    );
    let mut tx = j.begin(&st.db).await?;
    let previous_rev = lock_job(&mut tx, j.job_id).await?;
    let (job, row_key) = (j.job_id, key.clone());
    let out = design::track::<(), _>(
        &mut tx,
        &st.design,
        &j,
        write(
            job_ops::op_ref(&key),
            format!("{key}: removed from the job"),
        ),
        move |c| {
            Box::pin(async move {
                if job_ops::delete(c, job, &row_key).await? {
                    Ok(())
                } else {
                    Err(err)
                }
            })
        },
    )
    .await?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("job_operation.update", KIND, key.clone())
            .job(&j.job_key)
            .detail("change", "cleared"),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "op": key,
        "draft_rev": out.draft_rev,
        "previous_rev": previous_rev,
    })))
}
