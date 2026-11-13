//! Tenant IAM procedures (A3) and per-job tool declarations (A2). Unversioned `/api` routes.
//!
//! | Route | Role | Notes |
//! |---|---|---|
//! | `GET /api/iam-procedures` | member | `[{id, name, description, steps, jobs, ..}]` |
//! | `POST /api/iam-procedures` `{name, description?, steps}` | tenant_admin | 201; 409 `name_taken` |
//! | `GET /api/iam-procedures/{id}` | member | 404 `procedure_not_found` |
//! | `PUT /api/iam-procedures/{id}` | tenant_admin | replaces name/description/steps |
//! | `DELETE /api/iam-procedures/{id}` | tenant_admin | 409 `procedure_attached` while a job attaches it |
//! | `GET /api/jobs/{job}/tools/{op}/policy` | job member | `{op, policy}` (draft; `policy` null when none) |
//! | `PUT /api/jobs/{job}/tools/{op}/policy` `ToolPolicy` | integrations_manager | design write, kind `tool_policy` |
//! | `DELETE /api/jobs/{job}/tools/{op}/policy` | integrations_manager | 404 `policy_not_found` |
//! | `GET /api/jobs/{job}/iam-procedures` | job member | `[procedure id]` attached in the draft |
//! | `PUT /api/jobs/{job}/iam-procedures/{id}` | integrations_manager | design write, kind `iam_attachment` |
//! | `DELETE /api/jobs/{job}/iam-procedures/{id}` | integrations_manager | 404 `attachment_not_found` |
//!
//! 422 codes of the validators (`runtime::declared`): `unknown_operation`, `invalid_key`, `reserved_key`,
//! `ask_user_lookup_key`, `empty_prompt`, `invalid_retries`, `invalid_ttl`, `duplicate_output_key`,
//! `empty_input_name`, `invalid_name`, plus `cycle` / `depth_exceeded` from the producer chains and `invalid_body`.
//! A job's op must be in its catalogue (`operation_not_expected` 404 for a malformed or unoffered op key). A policy
//! write and an attachment are job design writes (D54): the working row changes inside `design::track`, one audit row
//! in the same transaction. `draft_rev` is the revision after the write, `previous_rev` the one before.
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{
    attached, canonical_op, check_chains, get as get_procedure, job_index, known_shape, refusal,
};
use crate::audit::{self, Event};
use crate::connectors::opkey::parse_op_key;
use crate::connectors::routes_store::{self, DesignOf, ExpectedCatalogue};
use crate::design::{self, DraftWrite, Label};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::job_design::targets::declared::{
    attachment_ref, delete_attachment, delete_policy, policy_ref, write_attachment, write_policy,
};
use crate::runtime::declared::{
    IamProcedure, IamStep, IdentityStrength, ProducerIndex, ToolPolicy, validate_policy,
    validate_procedure,
};
use crate::state::AppState;
use crate::tenancy::{JobMember, JobRequires, Member, Requires, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/iam-procedures",
            get(list_procedures).post(create_procedure),
        )
        .route(
            "/api/iam-procedures/{id}",
            get(read_procedure)
                .put(update_procedure)
                .delete(delete_procedure),
        )
        .route(
            "/api/jobs/{job}/tools/{op}/policy",
            get(read_policy).put(put_policy).delete(clear_policy),
        )
        .route("/api/jobs/{job}/iam-procedures", get(job_attachments))
        .route(
            "/api/jobs/{job}/iam-procedures/{id}",
            put(attach).delete(detach),
        )
}

const AREA: &str = "tools";
const PAGE: &str = "J6";

fn body<T: serde::de::DeserializeOwned>(v: Value) -> ApiResult<T> {
    serde_json::from_value(v).map_err(|e| {
        ApiError::Unprocessable(
            "invalid_body",
            format!("The request body is not valid: {e}"),
        )
    })
}

// ---- tenant procedures ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcedureBody {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    strength: IdentityStrength,
    #[serde(default)]
    steps: Vec<IamStep>,
}

/// Validate a body into a canonical procedure (ops canonicalised, name trimmed) and check its own chains.
fn checked(id: Uuid, b: ProcedureBody) -> ApiResult<IamProcedure> {
    let mut p = IamProcedure {
        id,
        name: b.name.trim().to_owned(),
        description: b.description.trim().to_owned(),
        strength: b.strength,
        steps: b.steps,
    };
    validate_procedure(&p, &known_shape).map_err(refusal)?;
    for s in &mut p.steps {
        s.op = canonical_op(&s.op).unwrap_or_else(|| s.op.clone());
    }
    let idx = ProducerIndex::build(&[], std::slice::from_ref(&p));
    let keys: Vec<String> = p
        .steps
        .iter()
        .flat_map(|s| s.outputs.values().map(|o| o.key.clone()))
        .collect();
    check_chains(&idx, &keys)?;
    Ok(p)
}

async fn list_procedures(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let rows = super::list(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(Value::Array(
        rows.iter().map(|(r, n)| r.json(Some(*n))).collect(),
    )))
}

async fn read_procedure(
    Member(t): Member,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let r = get_procedure(&mut tx, id).await?;
    let n = super::attached_jobs(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Json(r.json(Some(n))))
}

async fn create_procedure(
    Requires(t, _): Requires<role::TenantAdmin>,
    State(st): State<AppState>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let p = checked(Uuid::nil(), body(v)?)?;
    let mut tx = t.begin(&st.db).await?;
    let r = super::create(
        &mut tx,
        &p.name,
        &p.description,
        p.strength,
        &p.steps,
        &t.caller.sub,
    )
    .await?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new("iam_procedure.create", "iam_procedure", r.id.to_string())
            .detail("name", r.name.clone())
            .detail("steps", p.steps.len() as i64),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(r.json(Some(0)))))
}

async fn update_procedure(
    Requires(t, _): Requires<role::TenantAdmin>,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    let p = checked(id, body(v)?)?;
    let mut tx = t.begin(&st.db).await?;
    let r = super::update(
        &mut tx,
        id,
        &p.name,
        &p.description,
        p.strength,
        &p.steps,
        &t.caller.sub,
    )
    .await?;
    let n = super::attached_jobs(&mut tx, id).await?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new("iam_procedure.update", "iam_procedure", id.to_string())
            .detail("name", r.name.clone())
            .detail("steps", p.steps.len() as i64)
            .detail("jobs", n),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(r.json(Some(n))))
}

async fn delete_procedure(
    Requires(t, _): Requires<role::TenantAdmin>,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<StatusCode> {
    let mut tx = t.begin(&st.db).await?;
    super::delete(&mut tx, id).await?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new("iam_procedure.delete", "iam_procedure", id.to_string()),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- job declarations ------------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct OpPath {
    op: String,
}

#[derive(Deserialize)]
struct AttachPath {
    id: Uuid,
}

fn not_expected(key: &str) -> ApiError {
    ApiError::Missing(
        "operation_not_expected",
        format!("{key} is not an operation of this job."),
    )
}

/// Does the job's catalogue offer `op` (expected, or any variant of its object.verb in the compiled catalogue)?
fn offered(ex: &ExpectedCatalogue, key: &str) -> bool {
    let op = parse_op_key(key);
    !op.object.is_empty()
        && (ex.find(&op).is_some()
            || ex
                .catalogue
                .iter()
                .any(|r| r.object == op.object && r.verb == op.verb))
}

/// Does the procedure establish the conversation's subject (level 1 or 2)?
fn produces_subject(p: &IamProcedure) -> bool {
    p.steps
        .iter()
        .any(|s| s.outputs.values().any(|o| o.key == "subject"))
}

async fn lock_job(c: &mut PgConnection, job: Uuid) -> ApiResult<i64> {
    sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(job)
        .fetch_optional(&mut *c)
        .await?
        .ok_or_else(|| ApiError::Missing("job_not_found", format!("no job {job}")))
}

async fn catalogue(
    c: &mut PgConnection,
    reg: &design::Registry,
    job: Uuid,
) -> ApiResult<ExpectedCatalogue> {
    let row = routes_store::job_row(c, job).await?;
    routes_store::expected(c, reg, &row, DesignOf::Draft).await
}

fn write(kind: &'static str, r: design::DraftRef, label: String) -> DraftWrite {
    DraftWrite {
        area: AREA,
        page: PAGE,
        kind,
        r#ref: r,
        label: Label::Now(label.chars().take(400).collect()),
    }
}

async fn read_policy(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<OpPath>,
) -> ApiResult<Json<Value>> {
    let key = canonical_op(&p.op).ok_or_else(|| not_expected(p.op.trim()))?;
    let mut tx = j.begin(&st.db).await?;
    let policy: Option<Value> =
        sqlx::query_scalar("SELECT policy FROM job_tool_policy WHERE job_id = $1 AND op = $2")
            .bind(j.job_id)
            .bind(&key)
            .fetch_optional(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(Json(json!({"op": key, "policy": policy})))
}

async fn put_policy(
    JobRequires(j, _): JobRequires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<OpPath>,
    ApiJson(v): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    let key = canonical_op(&p.op).ok_or_else(|| not_expected(p.op.trim()))?;
    let policy: ToolPolicy = body(v)?;
    let mut tx = j.begin(&st.db).await?;
    let previous_rev = lock_job(&mut tx, j.job_id).await?;
    let ex = catalogue(&mut tx, &st.design, j.job_id).await?;
    if !offered(&ex, &key) {
        return Err(not_expected(&key));
    }
    validate_policy(&key, &policy, &|o| offered(&ex, o)).map_err(refusal)?;
    let keys: Vec<String> = policy.outputs.values().map(|o| o.key.clone()).collect();
    let idx = job_index(&mut tx, j.job_id, Some((&key, &policy)), None).await?;
    check_chains(&idx, &keys)?;
    let label = format!(
        "{key}: {} state inputs, {} outputs",
        policy
            .inputs
            .values()
            .filter(|i| !matches!(i, crate::runtime::declared::InputDecl::Model))
            .count(),
        policy.outputs.len()
    );
    let (job, by, row_key, row_policy) = (
        j.job_id,
        j.t.caller.sub.clone(),
        key.clone(),
        policy.clone(),
    );
    let out = design::track::<(), _>(
        &mut tx,
        &st.design,
        &j,
        write("tool_policy", policy_ref(&key), label),
        move |c| Box::pin(async move { write_policy(c, job, &row_key, &row_policy, &by).await }),
    )
    .await?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("tool_policy.update", "tool_policy", key.clone())
            .job(&j.job_key)
            .detail("change", "set")
            .detail("inputs", policy.inputs.len() as i64)
            .detail("outputs", policy.outputs.len() as i64),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "op": key,
        "policy": policy,
        "draft_rev": out.draft_rev,
        "previous_rev": previous_rev,
    })))
}

async fn clear_policy(
    JobRequires(j, _): JobRequires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<OpPath>,
) -> ApiResult<Json<Value>> {
    let key = canonical_op(&p.op).ok_or_else(|| not_expected(p.op.trim()))?;
    let missing = || ApiError::Missing("policy_not_found", format!("{key} has no declaration."));
    let mut tx = j.begin(&st.db).await?;
    let previous_rev = lock_job(&mut tx, j.job_id).await?;
    let (job, row_key, err) = (j.job_id, key.clone(), missing());
    let out = design::track::<(), _>(
        &mut tx,
        &st.design,
        &j,
        write(
            "tool_policy",
            policy_ref(&key),
            format!("{key}: declaration removed"),
        ),
        move |c| {
            Box::pin(async move {
                if delete_policy(c, job, &row_key).await? {
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
        Event::new("tool_policy.update", "tool_policy", key.clone())
            .job(&j.job_key)
            .detail("change", "cleared"),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "op": key,
        "policy": null,
        "draft_rev": out.draft_rev,
        "previous_rev": previous_rev,
    })))
}

async fn job_attachments(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let ids: Vec<Uuid> = attached(&mut tx, j.job_id)
        .await?
        .into_iter()
        .map(|r| r.id)
        .collect();
    tx.commit().await?;
    Ok(Json(json!(ids)))
}

async fn attach(
    JobRequires(j, _): JobRequires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<AttachPath>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let previous_rev = lock_job(&mut tx, j.job_id).await?;
    let proc_row = get_procedure(&mut tx, p.id).await?;
    let proc = proc_row.procedure()?;
    let ex = catalogue(&mut tx, &st.design, j.job_id).await?;
    validate_procedure(&proc, &|o| offered(&ex, o)).map_err(refusal)?;
    // One conversation is one person (owner 2026-11-01): a job has at most one attached procedure that establishes
    // the subject, so the design knows which strength the identity is written with.
    if produces_subject(&proc) {
        for r in attached(&mut tx, j.job_id).await? {
            if r.id != p.id && r.procedure().is_ok_and(|o| produces_subject(&o)) {
                return Err(ApiError::Unprocessable(
                    "identity_already_produced",
                    format!(
                        "`{}` also establishes the person's identity; a job has one identity procedure. Detach it first.",
                        r.name
                    ),
                ));
            }
        }
    }
    let idx = job_index(&mut tx, j.job_id, None, Some(&proc)).await?;
    let keys: Vec<String> = proc
        .steps
        .iter()
        .flat_map(|s| s.outputs.values().map(|o| o.key.clone()))
        .collect();
    check_chains(&idx, &keys)?;
    let (job, id, by) = (j.job_id, p.id, j.t.caller.sub.clone());
    let out = design::track::<(), _>(
        &mut tx,
        &st.design,
        &j,
        write(
            "iam_attachment",
            attachment_ref(id),
            format!("Identity procedure {} attached", proc.name),
        ),
        move |c| Box::pin(async move { write_attachment(c, job, id, &by).await }),
    )
    .await?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("iam_attachment.update", "iam_attachment", p.id.to_string())
            .job(&j.job_key)
            .detail("change", "attached"),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "procedure": p.id,
        "attached": true,
        "draft_rev": out.draft_rev,
        "previous_rev": previous_rev,
    })))
}

async fn detach(
    JobRequires(j, _): JobRequires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<AttachPath>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let previous_rev = lock_job(&mut tx, j.job_id).await?;
    let (job, id) = (j.job_id, p.id);
    let err = ApiError::Missing(
        "attachment_not_found",
        "That identity procedure is not attached to this job.".into(),
    );
    let out = design::track::<(), _>(
        &mut tx,
        &st.design,
        &j,
        write(
            "iam_attachment",
            attachment_ref(id),
            "Identity procedure detached".to_owned(),
        ),
        move |c| {
            Box::pin(async move {
                if delete_attachment(c, job, id).await? {
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
        Event::new("iam_attachment.update", "iam_attachment", p.id.to_string())
            .job(&j.job_key)
            .detail("change", "detached"),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "procedure": p.id,
        "attached": false,
        "draft_rev": out.draft_rev,
        "previous_rev": previous_rev,
    })))
}
