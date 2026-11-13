//! Generator start and drive routes. Platform mutations require SuperAdmin. Tenant mutations require the
//! job's effective JobDesigner role and load runs under tenant and job RLS before reading their contents.
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::auth::{Caller, Role};
use crate::design::{self, DesignOf, JobRef};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::generator::{
    drive, evidence, llm, operation, package, proposal,
    run::{self, RunRow, Scope},
    store,
};
use crate::journey::model::is_slug;
use crate::journey::{designer, grid};
use crate::platform::db::{authored, layers};
use crate::platform::routes::Who;
use crate::platform_db::{self, begin_platform};
use crate::state::AppState;
use crate::tenancy::{self, JobCtx, JobMember, JobRequires, SuperAdmin, TenantCtx, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/generator/runs", post(start))
        .route(
            "/api/platform/generator/runs/{id}/draft",
            post(platform_draft),
        )
        .route(
            "/api/platform/generator/runs/{id}/items/{item}/decision",
            post(platform_decide),
        )
        .route(
            "/api/platform/generator/runs/{id}/items/{item}/proposal",
            post(platform_propose_object),
        )
        .route(
            "/api/platform/generator/runs/{id}/items/{item}/operation-proposal",
            post(platform_propose_operation),
        )
        .route(
            "/api/platform/object-proposals/{proposal}",
            post(platform_resolve),
        )
        .route(
            "/api/platform/generator/runs/{id}/stages/{stage}",
            put(platform_scope),
        )
        .route(
            "/api/platform/generator/runs/{id}/advance",
            post(platform_advance),
        )
        .route("/api/platform/generator/runs/{id}/tag", post(platform_tag))
        .route(
            "/api/platform/generator/runs/{id}/evidence",
            post(platform_evidence).layer(DefaultBodyLimit::max(evidence::UPLOAD_BODY_LIMIT)),
        )
        .route("/api/jobs/{job}/generator/runs/{id}", get(tenant_detail))
        .route(
            "/api/jobs/{job}/generator/runs/{id}/draft",
            post(tenant_draft),
        )
        .route(
            "/api/jobs/{job}/generator/runs/{id}/items/{item}/decision",
            post(tenant_decide),
        )
        .route(
            "/api/jobs/{job}/generator/runs/{id}/stages/{stage}",
            put(tenant_scope),
        )
        .route(
            "/api/jobs/{job}/generator/runs/{id}/advance",
            post(tenant_advance),
        )
        .route(
            "/api/jobs/{job}/generator/runs/{id}/package",
            post(tenant_package),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartInput {
    scope: String,
    #[serde(default)]
    industry: String,
    #[serde(default)]
    department: String,
    job: String,
    #[serde(default)]
    description: String,
    tenant_id: Option<Uuid>,
}

async fn start(
    caller: Caller,
    tenant: Result<TenantCtx, ApiError>,
    State(st): State<AppState>,
    ApiJson(input): ApiJson<StartInput>,
) -> ApiResult<Json<Value>> {
    let scope = Scope::parse(&input.scope).ok_or_else(|| {
        ApiError::Unprocessable("invalid_input", "scope is platform or tenant.".into())
    })?;
    if input.description.chars().count() > 4000 || !is_slug(input.job.trim()) {
        return Err(ApiError::Unprocessable(
            "invalid_input",
            "Use a job slug and a description of at most 4000 characters.".into(),
        ));
    }
    if scope == Scope::Platform {
        if !caller.super_admin {
            return Err(ApiError::Forbidden(
                "super_admin_required",
                "Only a super_admin starts a platform run.".into(),
            ));
        }
        start_platform(&st, &SuperAdmin(caller), input).await
    } else {
        let t = tenant?;
        t.require(Role::JobDesigner)?;
        if input.tenant_id.is_some_and(|id| id != t.tenant) {
            return Err(ApiError::Forbidden("forbidden", "Not your tenant.".into()));
        }
        start_tenant(&st, t, input).await
    }
}

async fn start_platform(
    st: &AppState,
    sa: &SuperAdmin,
    input: StartInput,
) -> ApiResult<Json<Value>> {
    if !is_slug(input.industry.trim()) || !is_slug(input.department.trim()) {
        return Err(ApiError::Unprocessable(
            "hierarchy_incomplete",
            "Name an industry and department in lower_snake_case.".into(),
        ));
    }
    let tenant = st
        .cfg
        .platform_tenant_id
        .ok_or_else(crate::knowledge::packs::curation::platform_tenant_unset)?;
    let mut tx = begin_platform(&st.db).await?;
    tenancy::set_tenant(&mut tx, tenant).await?;
    let who = Who::of(sa);
    let industry = input.industry.trim();
    let department = input.department.trim();
    let job = input.job.trim();
    // A new department can extend an existing archetype. A new one without an archetype is still a valid layer.
    let dept_key = agent_model::layer::LayerKey::industry_dept(industry, department);
    if layers::find_layer_by_key(&mut tx, &dept_key.label())
        .await?
        .is_none()
    {
        let archetype = agent_model::layer::LayerKey::base_dept(department).label();
        let extends = layers::find_layer_by_key(&mut tx, &archetype)
            .await?
            .map(|_| department.to_owned());
        authored::insert_layer(
            &mut tx,
            &dept_key,
            None,
            &authored::LayerMeta {
                extends,
                ..Default::default()
            },
        )
        .await?;
    }
    let target = designer::resolve(&mut tx, industry, Some(department), None).await?;
    let grid = grid::load(&mut tx, industry, department).await?;
    let job_layer = agent_model::layer::LayerKey::job(job);
    if layers::find_layer_by_key(&mut tx, &job_layer.label())
        .await?
        .is_none()
    {
        authored::insert_layer(
            &mut tx,
            &job_layer,
            None,
            &authored::LayerMeta {
                label: Some(job.to_owned()),
                template_selection: Some(
                    json!({"industry":industry,"departments":[target.dept_layer]}),
                ),
                ..Default::default()
            },
        )
        .await?;
    }
    let mut output = Vec::new();
    for family in [target.dept_layer, job_layer.label()] {
        let next: i32 = sqlx::query_scalar(
            "SELECT coalesce(max(version),0)+1 FROM layer_tag WHERE family=$1 AND NOT provisional",
        )
        .bind(&family)
        .fetch_one(&mut *tx)
        .await?;
        output.push(format!("{family}@v{next}"));
    }
    let steps = run::steps_for(Scope::Platform);
    let steps: Vec<_> = steps.iter().map(|s| s.as_str()).collect();
    let stages: Vec<_> = grid
        .stages
        .iter()
        .map(|s| (s.key.as_str().to_owned(), s.label.clone(), s.out_of_scope))
        .collect();
    let id = store::insert_run(
        &mut tx,
        &store::NewRun {
            tenant_id: tenant,
            scope: "platform",
            industry,
            department,
            job_key: job,
            job_id: None,
            first_step: steps[1],
            description: input.description.trim(),
            output: &output,
            funding: "platform_external",
            provider: None,
            started_by: &who.by,
        },
        &steps,
        &format!("{industry} / {department} / {job}"),
        &stages,
    )
    .await?;
    let run = store::run(&mut tx, id).await?;
    drive::event(
        &mut tx,
        &run,
        &who.actor,
        "generator.start",
        json!({"scope":"platform","industry":industry,"department":department}),
    )
    .await?;
    let view = drive::detail(&mut tx, &run).await?;
    tx.commit().await?;
    Ok(Json(view))
}

async fn start_tenant(st: &AppState, t: TenantCtx, input: StartInput) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let job_id: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE key=$1")
        .bind(input.job.trim())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::Missing("job_not_found", "No such job.".into()))?;
    let j = JobCtx {
        t,
        job_id,
        job_key: input.job.trim().to_owned(),
    };
    design::ensure_job(&mut tx, &st.design, &job_ref(&j)).await?;
    let bases = crate::job_design::base::at(
        &mut tx,
        &st.design,
        st.compiler.as_ref(),
        &job_ref(&j),
        &DesignOf::Draft,
    )
    .await?;
    let departments: Vec<_> = bases
        .selection
        .departments
        .iter()
        .filter_map(|s| agent_model::compile::parse_department(s))
        .collect();
    let (industry, department) = departments
        .iter()
        .find(|(_, d)| input.department.is_empty() || d == &input.department)
        .cloned()
        .ok_or_else(|| {
            ApiError::Unprocessable(
                "hierarchy_incomplete",
                "Choose a department of this job.".into(),
            )
        })?;
    let provider =
        llm::OpenAiDrafter::tenant(&mut tx, st, &st.cfg.generator, j.t.tenant, &job_ref(&j))
            .await?;
    let snapshot =
        serde_json::to_value(provider.snapshot()).map_err(|e| ApiError::Internal(e.into()))?;
    let journey =
        crate::journey::job::job_journey(&mut tx, st, &job_ref(&j), &DesignOf::Draft).await?;
    let steps = run::steps_for(Scope::Tenant);
    let steps: Vec<_> = steps.iter().map(|s| s.as_str()).collect();
    let stages: Vec<_> = journey
        .stages
        .iter()
        .map(|s| (s.key.as_str().to_owned(), s.label.clone(), s.out_of_scope))
        .collect();
    let output = vec![format!(
        "refinements for {} / {} (D8)",
        j.t.tenant, j.job_key
    )];
    let id = store::insert_run(
        &mut tx,
        &store::NewRun {
            tenant_id: j.t.tenant,
            scope: "tenant",
            industry: &industry,
            department: &department,
            job_key: &j.job_key,
            job_id: Some(job_id),
            first_step: steps[1],
            description: input.description.trim(),
            output: &output,
            funding: "tenant_byo",
            provider: Some(&snapshot),
            started_by: &j.t.caller.sub,
        },
        &steps,
        &format!("{industry} / {department} / {}", j.job_key),
        &stages,
    )
    .await?;
    let run = store::run(&mut tx, id).await?;
    drive::event(
        &mut tx,
        &run,
        &j.t.actor(),
        "generator.start",
        json!({"scope":"tenant","department":department}),
    )
    .await?;
    let view = drive::detail(&mut tx, &run).await?;
    tx.commit().await?;
    Ok(Json(view))
}

fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}

async fn platform_begin(
    st: &AppState,
    id: i64,
) -> ApiResult<(Transaction<'static, Postgres>, RunRow)> {
    let mut tx = begin_platform(&st.db).await?;
    let run = drive::platform_run(&mut tx, id).await?;
    tenancy::set_tenant(&mut tx, run.tenant_id).await?;
    Ok((tx, run))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hint {
    hint: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StageScope {
    out_of_scope: bool,
}

async fn platform_draft(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<i64>,
    ApiJson(body): ApiJson<Hint>,
) -> ApiResult<Json<Value>> {
    let hint = drive::checked_hint(body.hint)?;
    let (mut tx, run) = platform_begin(&st, id).await?;
    drive::assert_open(&run)?;
    platform_db::leave(&mut tx).await?;
    let drafter = llm::OpenAiDrafter::platform(&mut tx, &st, &st.cfg.generator).await?;
    platform_db::enter(&mut tx).await?;
    let who = Who::of(&sa);
    let view = drive::draft(&mut tx, &st, &run, &who.actor, &who.by, hint, &drafter).await?;
    tx.commit().await?;
    Ok(Json(view))
}
async fn platform_decide(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((id, item)): ApiPath<(i64, String)>,
    ApiJson(body): ApiJson<drive::Decision>,
) -> ApiResult<Json<Value>> {
    let (mut tx, run) = platform_begin(&st, id).await?;
    let view = drive::decide(&mut tx, &run, &Who::of(&sa).actor, &item, body).await?;
    tx.commit().await?;
    Ok(Json(view))
}
async fn platform_scope(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((id, stage)): ApiPath<(i64, String)>,
    ApiJson(body): ApiJson<StageScope>,
) -> ApiResult<Json<Value>> {
    let (mut tx, run) = platform_begin(&st, id).await?;
    let view = drive::stage_scope(
        &mut tx,
        &run,
        &Who::of(&sa).actor,
        &stage,
        body.out_of_scope,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(view))
}
async fn platform_advance(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<i64>,
) -> ApiResult<Json<Value>> {
    let (mut tx, run) = platform_begin(&st, id).await?;
    let view = drive::advance(&mut tx, &run, &Who::of(&sa).actor).await?;
    tx.commit().await?;
    Ok(Json(view))
}
async fn tenant_detail(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_job, id)): ApiPath<(String, i64)>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let run = drive::tenant_run(&mut tx, &j, id).await?;
    let view = drive::detail(&mut tx, &run).await?;
    tx.commit().await?;
    Ok(Json(view))
}
async fn tenant_draft(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, id)): ApiPath<(String, i64)>,
    ApiJson(body): ApiJson<Hint>,
) -> ApiResult<Json<Value>> {
    let hint = drive::checked_hint(body.hint)?;
    let mut tx = j.begin(&st.db).await?;
    let run = drive::tenant_run(&mut tx, &j, id).await?;
    let drafter =
        llm::OpenAiDrafter::tenant(&mut tx, &st, &st.cfg.generator, j.t.tenant, &job_ref(&j))
            .await?;
    let view = drive::draft(
        &mut tx,
        &st,
        &run,
        &j.t.actor(),
        &j.t.caller.sub,
        hint,
        &drafter,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(view))
}
async fn tenant_decide(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, id, item)): ApiPath<(String, i64, String)>,
    ApiJson(body): ApiJson<drive::Decision>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let run = drive::tenant_run(&mut tx, &j, id).await?;
    let view = drive::decide(&mut tx, &run, &j.t.actor(), &item, body).await?;
    tx.commit().await?;
    Ok(Json(view))
}
async fn tenant_scope(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, id, stage)): ApiPath<(String, i64, String)>,
    ApiJson(body): ApiJson<StageScope>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let run = drive::tenant_run(&mut tx, &j, id).await?;
    let view = drive::stage_scope(&mut tx, &run, &j.t.actor(), &stage, body.out_of_scope).await?;
    tx.commit().await?;
    Ok(Json(view))
}
async fn tenant_advance(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, id)): ApiPath<(String, i64)>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let run = drive::tenant_run(&mut tx, &j, id).await?;
    let view = drive::advance(&mut tx, &run, &j.t.actor()).await?;
    tx.commit().await?;
    Ok(Json(view))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resolve {
    approve: bool,
}

async fn platform_propose_object(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((id, item)): ApiPath<(i64, String)>,
) -> ApiResult<Json<Value>> {
    let (mut tx, run) = platform_begin(&st, id).await?;
    let view = proposal::propose_object(&mut tx, &run, &Who::of(&sa).actor, &item).await?;
    tx.commit().await?;
    Ok(Json(view))
}
async fn platform_propose_operation(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((id, item)): ApiPath<(i64, String)>,
    ApiJson(body): ApiJson<operation::ProposeBody>,
) -> ApiResult<Json<Value>> {
    let (mut tx, run) = platform_begin(&st, id).await?;
    let view = proposal::propose_operation(&mut tx, &run, &Who::of(&sa).actor, &item, body).await?;
    tx.commit().await?;
    Ok(Json(view))
}
async fn platform_resolve(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<String>,
    ApiJson(body): ApiJson<Resolve>,
) -> ApiResult<Json<Value>> {
    let (id, item) = proposal::split(&p)?;
    let (mut tx, run) = platform_begin(&st, id).await.map_err(|e| {
        if e.code() == "run_not_found" {
            ApiError::Missing("proposal_not_found", format!("No proposal {p}."))
        } else {
            e
        }
    })?;
    let view = proposal::resolve(&mut tx, &run, &Who::of(&sa).actor, item, body.approve).await?;
    tx.commit().await?;
    Ok(Json(view))
}
async fn platform_tag(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<i64>,
) -> ApiResult<Json<Value>> {
    let (mut tx, run) = platform_begin(&st, id).await?;
    let who = Who::of(&sa);
    let view =
        package::tag_platform(&mut tx, &*st.generator_guard, &run, &who.actor, &who.by).await?;
    tx.commit().await?;
    Ok(Json(view))
}
async fn tenant_package(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, id)): ApiPath<(String, i64)>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let run = drive::tenant_run(&mut tx, &j, id).await?;
    let view = package::package_tenant(&mut tx, &st, &j, &run, &j.t.actor()).await?;
    tx.commit().await?;
    Ok(Json(view))
}

async fn platform_evidence(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<i64>,
    req: Request,
) -> ApiResult<Json<Value>> {
    let run = {
        let (tx, run) = platform_begin(&st, id).await?;
        drop(tx);
        run
    };
    evidence::gates_provider(&st, &run).await?;
    let up = evidence::read_upload(req).await?;
    let (file, format, got) = evidence::collect(up).await?;
    let (mut tx, run) = platform_begin(&st, id).await?;
    let who = Who::of(&sa);
    let ev = evidence::insert(&mut tx, &run, &who.actor, &who.by, &file, format, &got).await?;
    let view = drive::detail(&mut tx, &run).await?;
    tx.commit().await?;
    drop(evidence::spawn(st.clone(), run.id, ev));
    Ok(Json(view))
}
