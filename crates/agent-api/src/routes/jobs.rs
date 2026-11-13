//! Jobs (B1 T11, P1b T4): the job list and board, one job, creating a job from a template tag, and the tenant admin's
//! request to move a job onto a newer tag.
//!
//! - `GET /api/jobs[?view=board]` — any member: `Paged<JobSummary>`, or with `view=board` `Paged<JobBoardRow>`.
//! - `GET /api/jobs/{job}` — any member: `JobSummary` (404 `job_not_found`).
//! - `POST /api/jobs` `{template: "layer@vN", key?, label?, departments?}` — a tenant_admin (D123): 201 `JobSummary`;
//!   the key defaults to the tag's key, or the first free `<key>_2`, `_3`… (an explicit taken key is 409 `job_exists`);
//!   `departments` narrow the template's (422 `departments_not_in_template`); the job's departments/region default
//!   from the company profile (D124.3); a template of another industry than the company's is 422 `industry_mismatch`.
//!   D128: or a blank job `{key, label, industry, departments[], region?}` (no template): pins = base + `industry/<i>`
//!   + each department family at its newest tagged version in the DB (422 `layer_not_published`),
//!     and every family those compose on that no pin names (D116 `base_dept/<d>`, `templates::complete_pins`).
//! - `GET /api/job-templates` — any member: every tagged template `{template, key, label, description, industry,
//!   departments, recommended}`, recommended (company industry + a company department) first.
//! - `PATCH /api/jobs/{job}` `{label}` — a tenant_admin: renames the job and/or sets who it serves (`audience`, D129; audit `job.update`).
//! - `POST /api/tenants/{id}/jobs` `{template: "layer@vN", key, label}` — super_admin (platform): 201 `JobSummary`.
//! - `POST /api/jobs/{job}/upgrade-request` — tenant admin: the `JobBoardRow` with the open request.
//!
//! D106/D115: a template whose selection or pins lack one industry and one or more of its departments creates no job
//! (422 `hierarchy_incomplete` / `department_industry_mismatch` / `duplicate_department`).
//!
//! A create is one transaction: the job (pins from the tag), the tenant's industry from the tag while it is
//! `unassigned`, the creation hook (crate::readmodel::jobs::on_job_created: audit `job.create`, then B2's `job_target` row
//! and baseline).
use agent_model::compile::Selection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{self, Actor, Event};
use crate::design::Registry;
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::page::paged;
use crate::platform::templates;
use crate::readmodel::agent_status::bus_up;
use crate::readmodel::jobs::{self, JOB_COLUMNS, JobRow};
use crate::state::AppState;
use crate::tenancy::{
    JobMember, JobRequires, Member, Requires, SuperAdmin, TenantAdmin, role, tenant_id,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs", get(list).post(create_in_tenant))
        .route("/api/job-templates", get(catalog))
        .route("/api/jobs/{job}", get(one).patch(update))
        .route("/api/jobs/{job}/upgrade-request", post(upgrade_request))
}

#[derive(Deserialize)]
struct ListQuery {
    view: Option<String>,
}

async fn list(
    Member(t): Member,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Value>> {
    let board = match q.view.as_deref().map(str::trim) {
        None | Some("") => false,
        Some("board") => true,
        Some(_) => {
            return Err(ApiError::Unprocessable(
                "invalid_view",
                "view must be board or absent".into(),
            ));
        }
    };
    let on_bus = bus_up(&st).await;
    let mut tx = t.begin(&st.db).await?;
    let rows = jobs::list(&mut tx).await?;
    let mut items = Vec::with_capacity(rows.len());
    for j in &rows {
        items.push(if board {
            jobs::board_row(&mut tx, &st, j, on_bus).await?
        } else {
            jobs::summary(&mut tx, j, on_bus).await?
        });
    }
    tx.commit().await?;
    Ok(Json(paged(items, None)))
}

fn job_not_found(key: &str) -> ApiError {
    ApiError::Missing("job_not_found", format!("no job {key:?}"))
}

async fn one(JobMember(j): JobMember, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let on_bus = bus_up(&st).await;
    let mut tx = j.begin(&st.db).await?;
    let row = jobs::by_key(&mut tx, &j.job_key)
        .await?
        .ok_or_else(|| job_not_found(&j.job_key))?;
    let v = jobs::summary(&mut tx, &row, on_bus).await?;
    tx.commit().await?;
    Ok(Json(v))
}

/// A job template tag as read.
#[derive(Clone, Debug, sqlx::FromRow)]
struct Tag {
    layer: String,
    version: i32,
    key: String,
    label: String,
    industry: Option<String>,
    pins: Value,
    status: String,
    retired: bool,
}

/// The tag `layer@vversion`: 404 `template_not_found`, 409 `template_not_tagged` for a draft, 422 `template_retired`
/// for a version a super_admin took back (jobs that already pin it are unaffected).
async fn tagged(conn: &mut PgConnection, layer: &str, version: i32) -> ApiResult<Tag> {
    let tag: Option<Tag> = sqlx::query_as(
        "SELECT layer, version, key, label, industry, pins, status, retired_at IS NOT NULL AS retired \
           FROM job_template_tag WHERE layer = $1 AND version = $2",
    )
    .bind(layer)
    .bind(version)
    .fetch_optional(&mut *conn)
    .await?;
    let tag = tag.ok_or_else(|| {
        ApiError::Missing(
            "template_not_found",
            format!("no job template {layer}@v{version}"),
        )
    })?;
    if tag.status != "tagged" {
        return Err(ApiError::Conflict(
            "template_not_tagged",
            format!("{layer}@v{version} is a draft; only a tagged template creates jobs"),
        ));
    }
    if tag.retired {
        return Err(ApiError::Unprocessable(
            "template_retired",
            format!("{layer}@v{version} is retired; choose another template"),
        ));
    }
    Ok(tag)
}

/// D129: who a job serves, else 422 `invalid_audience`. `None` is the default, `customers`.
fn audience(raw: Option<&str>) -> ApiResult<&'static str> {
    match raw {
        None | Some("customers") => Ok("customers"),
        Some("employees") => Ok("employees"),
        Some(_) => Err(ApiError::Unprocessable(
            "invalid_audience",
            "audience must be customers or employees".into(),
        )),
    }
}

/// D132.1: the profile a new job starts with. An explicit key must be a platform profile (422 `unknown_profile`);
/// absent, the only profile of the job's `industry`, else (none of that industry) the only industry-less profile;
/// none or several is 422 `profile_required` listing the choices.
async fn resolve_profile(
    conn: &mut PgConnection,
    industry: &str,
    explicit: Option<&str>,
) -> ApiResult<String> {
    let profiles = crate::release::compile::db_options(&mut *conn)
        .await?
        .profiles;
    if let Some(k) = explicit.map(str::trim).filter(|k| !k.is_empty()) {
        return if profiles.iter().any(|p| p.key == k) {
            Ok(k.to_string())
        } else {
            Err(ApiError::Unprocessable(
                "unknown_profile",
                format!(
                    "no profile {k:?}; the platform's profiles are {}",
                    profiles
                        .iter()
                        .map(|p| p.key.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ))
        };
    }
    let of_industry: Vec<&str> = profiles
        .iter()
        .filter(|p| p.industry.as_deref() == Some(industry))
        .map(|p| p.key.as_str())
        .collect();
    let choices = if of_industry.is_empty() {
        profiles
            .iter()
            .filter(|p| p.industry.is_none())
            .map(|p| p.key.as_str())
            .collect()
    } else {
        of_industry
    };
    match choices.as_slice() {
        [one] => Ok((*one).to_string()),
        _ => Err(ApiError::Unprocessable(
            "profile_required",
            if choices.is_empty() {
                format!("choose a profile: the platform has none for {industry}")
            } else {
                format!("choose a profile: {}", choices.join(", "))
            },
        )),
    }
}

/// Job languages (D132.4): trimmed, lower-cased, deduplicated, non-empty, each from the job-language vocabulary
/// (`routes::release::settings::LANGUAGES`).
pub(crate) fn valid_languages(raw: Vec<String>) -> ApiResult<Vec<String>> {
    let mut next: Vec<String> = Vec::with_capacity(raw.len());
    for l in raw {
        let l = l.trim().to_lowercase();
        if !next.contains(&l) {
            next.push(l);
        }
    }
    if next.is_empty() {
        return Err(ApiError::Unprocessable(
            "invalid_body",
            "At least one language is required.".into(),
        ));
    }
    let bad: Vec<&str> = next
        .iter()
        .map(String::as_str)
        .filter(|l| {
            !crate::routes::release::settings::LANGUAGES
                .iter()
                .any(|(k, _)| k == l)
        })
        .collect();
    if !bad.is_empty() {
        return Err(ApiError::Unprocessable(
            "unknown_language",
            format!("No platform language “{}”.", bad.join(", ")),
        ));
    }
    Ok(next)
}

/// The languages a new job serves: the validated request, else the company's.
async fn job_languages(
    conn: &mut PgConnection,
    tenant: Uuid,
    want: Option<Vec<String>>,
) -> ApiResult<Vec<String>> {
    match want {
        Some(w) => valid_languages(w),
        None => Ok(
            sqlx::query_scalar("SELECT languages FROM tenants WHERE id = $1")
                .bind(tenant)
                .fetch_one(&mut *conn)
                .await?,
        ),
    }
}

/// A job label: one line, 1 to 120 characters after trimming.
fn label(raw: &str) -> ApiResult<String> {
    let l = raw.trim();
    if l.is_empty() || l.chars().count() > 120 || l.chars().any(char::is_control) {
        return Err(ApiError::Unprocessable(
            "invalid_label",
            "label must be one line of 1 to 120 characters".into(),
        ));
    }
    Ok(l.to_string())
}

/// `^[a-z][a-z0-9_]{1,62}$` (the `jobs.key` CHECK).
pub fn valid_key(k: &str) -> bool {
    let mut c = k.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase())
        && (2..=63).contains(&k.len())
        && c.all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '_')
}

/// `layer@vN` → (layer, N), else 422 `invalid_template`.
pub fn parse_template(raw: &str) -> ApiResult<(String, i32)> {
    let bad = || {
        ApiError::Unprocessable(
            "invalid_template",
            "template must be <layer>@v<version>, e.g. job/customer_care_agent@v4".into(),
        )
    };
    let (layer, version) = raw.trim().rsplit_once("@v").ok_or_else(bad)?;
    let version: i32 = version.parse().map_err(|_| bad())?;
    if layer.is_empty() || version < 0 {
        return Err(bad());
    }
    Ok((layer.to_string(), version))
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|c| c == "23505")
}

/// How the job key is chosen.
enum KeyChoice<'a> {
    /// This key exactly: 409 `job_exists` when the tenant has it.
    Exact(&'a str),
    /// The tag's key, or the first free `<key>_2`, `<key>_3`… (D123.1).
    FirstFree(&'a str),
}

/// The tenant-admin create (D123/D124.3): the requested departments (`None` = the company's), the company profile
/// is read from `tenants`. Absent (`None`) on the platform create, which starts from the template defaults.
struct TenantCreate {
    departments: Option<Vec<String>>,
    profile: Option<String>,
    languages: Option<Vec<String>>,
    region: Option<String>,
}

/// The most `<key>_N` suffixes tried before 409 `job_exists`.
const MAX_KEY_SUFFIX: usize = 99;

/// The first free key of `base`, `base_2`, … in the scoped tenant (409 `job_exists` after [`MAX_KEY_SUFFIX`]).
async fn first_free_key(conn: &mut PgConnection, base: &str) -> ApiResult<String> {
    let candidates: Vec<String> = std::iter::once(base.to_string())
        .chain((2..=MAX_KEY_SUFFIX).map(|n| format!("{base}_{n}")))
        .filter(|k| valid_key(k))
        .collect();
    let taken: Vec<String> = sqlx::query_scalar("SELECT key FROM jobs WHERE key = ANY($1)")
        .bind(&candidates)
        .fetch_all(&mut *conn)
        .await?;
    candidates
        .into_iter()
        .find(|k| !taken.contains(k))
        .ok_or_else(|| {
            ApiError::Conflict(
                "job_exists",
                format!("this tenant already has {MAX_KEY_SUFFIX} jobs from {base:?}"),
            )
        })
}

/// The selection a tenant-admin create starts the job with: the template's default, its departments narrowed
/// (D123.1: requested ⊆ the template's, ≥1, else 422 `departments_not_in_template`; none requested = the template's
/// ∩ the company's when non-empty, else the template's), the company region when the platform knows it (D124.3).
async fn tenant_selection(
    conn: &mut PgConnection,
    tenant: Uuid,
    tag: &Tag,
    default: &Selection,
    want: Option<Vec<String>>,
    profile: Option<&str>,
    explicit_region: Option<String>,
) -> ApiResult<Selection> {
    let (industry, company_departments, region): (String, Vec<String>, Option<String>) =
        sqlx::query_as("SELECT industry, departments, region FROM tenants WHERE id = $1")
            .bind(tenant)
            .fetch_one(&mut *conn)
            .await?;
    if industry != "unassigned" && tag.industry.as_deref().is_some_and(|i| i != industry) {
        return Err(ApiError::Unprocessable(
            "industry_mismatch",
            format!(
                "this company is {industry}; {}@v{} is a {} template",
                tag.layer,
                tag.version,
                tag.industry.as_deref().unwrap_or("")
            ),
        ));
    }
    let mut sel = default.clone();
    match want {
        Some(want) => {
            if want.is_empty() || want.iter().any(|d| !default.departments.contains(d)) {
                return Err(ApiError::Unprocessable(
                    "departments_not_in_template",
                    format!(
                        "departments must be one or more of the template's: {}",
                        default.departments.join(", ")
                    ),
                ));
            }
            sel.departments = want;
        }
        None => {
            let both: Vec<String> = default
                .departments
                .iter()
                .filter(|d| company_departments.contains(d))
                .cloned()
                .collect();
            if !both.is_empty() {
                sel.departments = both;
            }
        }
    }
    if let Some(r) = explicit_region {
        // an explicit region must be known (insert's region_refusal answers 422 otherwise)
        sel.region = Some(r);
    } else if let Some(region) = region {
        let mut with = sel.clone();
        with.region = Some(region);
        match templates::region_refusal(&mut *conn, &with).await {
            Ok(()) => sel = with,
            Err(ApiError::Detailed { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    // D132.1: an explicit profile wins; otherwise only fill when the template's selection has none
    if profile.is_some() || sel.profile.is_none() {
        sel.profile = Some(resolve_profile(&mut *conn, &sel.industry, profile).await?);
    }
    Ok(sel)
}

/// Insert the job from `tag` in the scoped transaction, adopt the tag's industry while the tenant has none, run the
/// creation hook. 422 `hierarchy_incomplete` / `department_industry_mismatch` / `duplicate_department` (D106/D115)
/// when the template's selection or pins lack one industry and one or more of its departments; 409 `job_exists`
/// when the tenant has that key. With `tenant_create`: 422 `industry_mismatch` / `departments_not_in_template` and
/// the job's selection from the company profile (D123/D124).
#[allow(clippy::too_many_arguments)]
async fn insert(
    conn: &mut PgConnection,
    tenant: Uuid,
    key: KeyChoice<'_>,
    label: &str,
    audience: &str,
    tag: &Tag,
    actor: &Actor,
    reg: &Registry,
    tenant_create: Option<TenantCreate>,
) -> ApiResult<JobRow> {
    let key = match key {
        KeyChoice::Exact(k) => k.to_string(),
        KeyChoice::FirstFree(base) => first_free_key(&mut *conn, base).await?,
    };
    let key = key.as_str();
    let job_exists = || {
        ApiError::Conflict(
            "job_exists",
            format!("this tenant already has a job {key:?}"),
        )
    };
    let template = templates::resolve_template(&mut *conn, &tag.layer, tag.version).await?;
    let (selection, languages) = match tenant_create {
        Some(c) => (
            Some(
                tenant_selection(
                    &mut *conn,
                    tenant,
                    tag,
                    &template.default_selection,
                    c.departments,
                    c.profile.as_deref(),
                    c.region,
                )
                .await?,
            ),
            Some(job_languages(&mut *conn, tenant, c.languages).await?),
        ),
        None => (None, None),
    };
    let checked = selection.as_ref().unwrap_or(&template.default_selection);
    templates::template_refusal(checked, &template.pins)?;
    templates::region_refusal(&mut *conn, checked).await?;
    let row: JobRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO jobs (key, label, pins, template_layer, template_version, created_by, audience, languages) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, COALESCE($8, '{{en}}'::text[])) RETURNING {JOB_COLUMNS}"
    )))
    .bind(key)
    .bind(label)
    .bind(&tag.pins)
    .bind(&tag.layer)
    .bind(tag.version)
    .bind(&actor.id)
    .bind(audience)
    .bind(languages)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            job_exists()
        } else {
            e.into()
        }
    })?;
    if let Some(industry) = &tag.industry {
        sqlx::query(
            "UPDATE tenants SET industry = $2, updated_at = now() \
              WHERE id = $1 AND industry = 'unassigned'",
        )
        .bind(tenant)
        .bind(industry)
        .execute(&mut *conn)
        .await?;
    }
    jobs::on_job_created(conn, &row, actor, reg, selection).await?;
    Ok(row)
}

/// `POST /api/jobs` body (D123 template create, or the D128 blank job).
#[derive(Deserialize)]
#[serde(untagged)]
enum CreateBody {
    Template(CreateInTenant),
    Blank(CreateBlank),
}

/// D128: a job built straight on the platform's base/industry/department layers.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBlank {
    key: String,
    label: String,
    industry: String,
    departments: Vec<String>,
    region: Option<String>,
    audience: Option<String>,
    profile: Option<String>,
    languages: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateInTenant {
    template: String,
    key: Option<String>,
    label: Option<String>,
    departments: Option<Vec<String>>,
    audience: Option<String>,
    profile: Option<String>,
    languages: Option<Vec<String>>,
    /// The create drawer sends the chosen region; absent = the company region (D124.3).
    region: Option<String>,
}

/// `POST /api/jobs` (D123): a tenant_admin creates a job from a tagged template.
async fn create_in_tenant(
    Requires(t, _): TenantAdmin,
    State(st): State<AppState>,
    ApiJson(body): ApiJson<CreateBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let b = match body {
        CreateBody::Template(b) => b,
        CreateBody::Blank(b) => return create_blank(t, st, b).await,
    };
    let (layer, version) = parse_template(&b.template)?;
    if b.key.as_deref().is_some_and(|k| !valid_key(k)) {
        return Err(ApiError::Unprocessable(
            "invalid_job_key",
            "key must match ^[a-z][a-z0-9_]{1,62}$".into(),
        ));
    }
    let label_given = b.label.as_deref().map(label).transpose()?;
    let audience = audience(b.audience.as_deref())?;
    let on_bus = bus_up(&st).await;
    let mut tx = t.begin(&st.db).await?;
    let tag = tagged(&mut tx, &layer, version).await?;
    let label = label_given.unwrap_or_else(|| tag.label.clone());
    let key = match b.key.as_deref() {
        Some(k) => KeyChoice::Exact(k),
        None => KeyChoice::FirstFree(&tag.key),
    };
    let row = insert(
        &mut tx,
        t.tenant,
        key,
        &label,
        audience,
        &tag,
        &t.actor(),
        &st.design,
        Some(TenantCreate {
            departments: b.departments,
            profile: b.profile,
            languages: b.languages,
            region: b.region,
        }),
    )
    .await?;
    let v = jobs::summary(&mut tx, &row, on_bus).await?;
    tx.commit().await?;
    // B4 T1: the new job's scopes (prod + test) join the runtime registry.
    crate::runtime::scopes::changed(&st, t.tenant).await;
    Ok((StatusCode::CREATED, Json(v)))
}

/// The newest tagged version of each pin family (D128), in order, else 422 `layer_not_published` naming the first family
/// with none; an industry nobody has published at all says so in plain words.
async fn newest_pins(
    conn: &mut PgConnection,
    families: &[String],
) -> ApiResult<Vec<templates::TemplatePin>> {
    let latest = crate::platform::db::tags::latest_tags(&mut *conn).await?;
    let mut pins = Vec::with_capacity(families.len());
    for f in families {
        match latest.get(f) {
            Some(v) => pins.push(templates::TemplatePin {
                layer: f.clone(),
                version: *v,
            }),
            None => {
                let any = latest
                    .keys()
                    .any(|k| k.starts_with("industry/") || k.starts_with("industry_dept/"));
                let message = if any {
                    format!("{f} has no published (tagged) version on this platform")
                } else {
                    "no industry/department has been published on this platform yet".to_string()
                };
                return Err(ApiError::Unprocessable("layer_not_published", message));
            }
        }
    }
    Ok(pins)
}

/// `POST /api/jobs` blank job (D128): validate, pin the newest tagged base/industry/department layers, check the
/// D106/D115/D111 hierarchy, insert with no template, adopt the industry while `unassigned`, run the creation hook.
async fn create_blank(
    t: crate::tenancy::TenantCtx,
    st: AppState,
    b: CreateBlank,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if !valid_key(&b.key) {
        return Err(ApiError::Unprocessable(
            "invalid_job_key",
            "key must match ^[a-z][a-z0-9_]{1,62}$".into(),
        ));
    }
    let label = label(&b.label)?;
    let audience = audience(b.audience.as_deref())?;
    let industry = b.industry.trim().to_string();
    if industry.is_empty() || industry == "unassigned" {
        return Err(ApiError::Unprocessable(
            "hierarchy_incomplete",
            "a job needs an industry".into(),
        ));
    }
    let on_bus = bus_up(&st).await;
    let mut tx = t.begin(&st.db).await?;
    let (company, company_region): (String, Option<String>) =
        sqlx::query_as("SELECT industry, region FROM tenants WHERE id = $1")
            .bind(t.tenant)
            .fetch_one(&mut *tx)
            .await?;
    if company != "unassigned" && company != industry {
        return Err(ApiError::Unprocessable(
            "industry_mismatch",
            format!("this company is {company}; a job of {industry} cannot be created"),
        ));
    }
    let mut sel = Selection {
        industry: industry.clone(),
        departments: b.departments.clone(),
        ..Selection::default()
    };
    templates::hierarchy_refusal(&sel)?;
    let mut families = vec!["base".to_string(), format!("industry/{industry}")];
    families.extend(b.departments.iter().cloned());
    let pins = newest_pins(&mut tx, &families).await?;
    // an explicit region must be known; the company's region applies when the platform knows it (D124.3)
    match b.region {
        Some(r) => {
            sel.region = Some(r);
            templates::region_refusal(&mut tx, &sel).await?;
        }
        None => {
            if let Some(r) = company_region {
                let mut with = sel.clone();
                with.region = Some(r);
                match templates::region_refusal(&mut tx, &with).await {
                    Ok(()) => sel = with,
                    Err(ApiError::Detailed { .. }) => {}
                    Err(e) => return Err(e),
                }
            }
        }
    }
    sel.profile = Some(resolve_profile(&mut tx, &industry, b.profile.as_deref()).await?);
    let languages = job_languages(&mut tx, t.tenant, b.languages).await?;
    // D116: also pin what the departments compose on (base_dept/<d>), or the job's design never compiles
    let pins = templates::complete_pins(&mut tx, pins, &sel).await?;
    templates::template_refusal(&sel, &pins)?;
    let actor = t.actor();
    let row: JobRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO jobs (key, label, pins, template_layer, template_version, created_by, audience, languages) \
         VALUES ($1, $2, $3, NULL, NULL, $4, $5, $6) RETURNING {JOB_COLUMNS}"
    )))
    .bind(&b.key)
    .bind(&label)
    .bind(serde_json::to_value(&pins).map_err(|e| ApiError::Internal(e.into()))?)
    .bind(&actor.id)
    .bind(audience)
    .bind(&languages)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            ApiError::Conflict(
                "job_exists",
                format!("this tenant already has a job {:?}", b.key),
            )
        } else {
            e.into()
        }
    })?;
    sqlx::query(
        "UPDATE tenants SET industry = $2, updated_at = now() WHERE id = $1 AND industry = 'unassigned'",
    )
    .bind(t.tenant)
    .bind(&industry)
    .execute(&mut *tx)
    .await?;
    jobs::on_job_created(&mut tx, &row, &actor, &st.design, Some(sel)).await?;
    let v = jobs::summary(&mut tx, &row, on_bus).await?;
    tx.commit().await?;
    crate::runtime::scopes::changed(&st, t.tenant).await;
    Ok((StatusCode::CREATED, Json(v)))
}

/// `GET /api/job-templates` (D123.2): every tagged job template with the company's recommendation, recommended first.
async fn catalog(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let (industry, company_departments): (String, Vec<String>) =
        sqlx::query_as("SELECT industry, departments FROM tenants WHERE id = $1")
            .bind(t.tenant)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    // job_template_tag and the layer heads are platform data (readable by agent_app); no tenant scope needed.
    let mut c = st.db.acquire().await?;
    let tags: Vec<(String, i32, String, String, Option<String>)> = sqlx::query_as(
        "SELECT layer, version, key, label, industry FROM job_template_tag \
          WHERE status = 'tagged' AND retired_at IS NULL",
    )
    .fetch_all(&mut *c)
    .await?;
    let mut items = Vec::with_capacity(tags.len());
    for (layer, version, key, label, tag_industry) in tags {
        // a tag whose head cannot resolve a default selection cannot create a job either: it is not offered
        let Ok(resolved) = templates::resolve_template(&mut c, &layer, version).await else {
            continue;
        };
        let departments = resolved.default_selection.departments;
        let recommended = tag_industry.as_deref() == Some(industry.as_str())
            && departments.iter().any(|d| company_departments.contains(d));
        items.push((
            recommended,
            label.clone(),
            json!({
                "template": format!("{layer}@v{version}"),
                "key": key,
                "label": label,
                "description": null,
                "industry": tag_industry,
                "departments": departments,
                "recommended": recommended,
            }),
        ));
    }
    items.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2["template"].as_str().cmp(&b.2["template"].as_str()))
    });
    let templates: Vec<Value> = items.into_iter().map(|i| i.2).collect();
    Ok(Json(json!({"templates": templates})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateJob {
    label: Option<String>,
    audience: Option<String>,
}

/// `PATCH /api/jobs/{job}` (D123.4): rename the job. `jobs` has no description column, so only the label.
async fn update(
    JobRequires(j, _): JobRequires<role::TenantAdmin>,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<UpdateJob>,
) -> ApiResult<Json<Value>> {
    let new_label = b.label.as_deref().map(label).transpose()?;
    let new_audience = b
        .audience
        .as_deref()
        .map(|a| audience(Some(a)))
        .transpose()?;
    if new_label.is_none() && new_audience.is_none() {
        return Err(ApiError::Unprocessable(
            "nothing_to_update",
            "send a label or an audience".into(),
        ));
    }
    let on_bus = bus_up(&st).await;
    let mut tx = j.begin(&st.db).await?;
    let old = jobs::by_key(&mut tx, &j.job_key)
        .await?
        .ok_or_else(|| job_not_found(&j.job_key))?;
    let sql = format!(
        "UPDATE jobs SET label = coalesce($2, label), audience = coalesce($3, audience), updated_at = now() \
          WHERE id = $1 RETURNING {JOB_COLUMNS}"
    );
    let row: JobRow = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(old.id)
        .bind(&new_label)
        .bind(new_audience)
        .fetch_one(&mut *tx)
        .await?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("job.update", "job", row.key.clone())
            .job(&row.key)
            .detail("label_from", old.label)
            .detail("label_to", row.label.clone())
            .detail("audience_from", old.audience)
            .detail("audience_to", row.audience.clone()),
    )
    .await?;
    let v = jobs::summary(&mut tx, &row, on_bus).await?;
    tx.commit().await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
pub struct TenantPath {
    tenant_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformCreate {
    template: String,
    key: String,
    label: String,
    audience: Option<String>,
}

/// `POST /api/tenants/{id}/jobs` (mounted by crate::routes::tenants).
pub async fn platform_create(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TenantPath>,
    ApiJson(b): ApiJson<PlatformCreate>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let tenant = tenant_id(&p.tenant_id)?;
    let (layer, version) = parse_template(&b.template)?;
    if !valid_key(&b.key) {
        return Err(ApiError::Unprocessable(
            "invalid_job_key",
            "key must match ^[a-z][a-z0-9_]{1,62}$".into(),
        ));
    }
    let label = label(&b.label)?;
    let audience = audience(b.audience.as_deref())?;
    let on_bus = bus_up(&st).await;
    let (mut tx, row) = crate::routes::tenants::scoped(&st, tenant).await?;
    if row.status == "archived" {
        return Err(ApiError::Conflict(
            "tenant_archived",
            "this tenant is archived".into(),
        ));
    }
    let tag = tagged(&mut tx, &layer, version).await?;
    let job = insert(
        &mut tx,
        tenant,
        KeyChoice::Exact(&b.key),
        &label,
        audience,
        &tag,
        &Actor::person(&c),
        &st.design,
        None,
    )
    .await?;
    let v = jobs::summary(&mut tx, &job, on_bus).await?;
    tx.commit().await?;
    // B4 T1: the new job's scopes (prod + test) join the runtime registry.
    crate::runtime::scopes::changed(&st, tenant).await;
    Ok((StatusCode::CREATED, Json(v)))
}

async fn upgrade_request(
    JobRequires(j, _): JobRequires<role::TenantAdmin>,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let on_bus = bus_up(&st).await;
    let mut tx = j.begin(&st.db).await?;
    let row = jobs::by_key(&mut tx, &j.job_key)
        .await?
        .ok_or_else(|| job_not_found(&j.job_key))?;
    let Some((version, to_pins)) = jobs::newest_tag(&mut tx, &row).await? else {
        return Err(ApiError::Conflict(
            "no_newer_template",
            "no newer tagged template exists for this job".into(),
        ));
    };
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO template_upgrade_request (job_id, from_pins, to_pins, requested_by) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(row.id)
    .bind(&row.pins)
    .bind(&to_pins)
    .bind(&j.t.caller.sub)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            ApiError::Conflict(
                "upgrade_already_requested",
                "an upgrade of this job is already requested".into(),
            )
        } else {
            e.into()
        }
    })?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("job.upgrade_request", "job", row.key.clone())
            .job(&row.key)
            .detail("request_id", id.to_string())
            .detail(
                "to",
                format!("{}@v{version}", row.template_layer.as_deref().unwrap_or("")),
            ),
    )
    .await?;
    let v = jobs::board_row(&mut tx, &st, &row, on_bus).await?;
    tx.commit().await?;
    Ok(Json(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_and_keys() {
        assert_eq!(
            parse_template("job/customer_care_agent@v4").unwrap(),
            ("job/customer_care_agent".to_string(), 4)
        );
        for bad in ["job/x", "@v3", "job/x@vX", "job/x@v-1", ""] {
            assert_eq!(
                parse_template(bad).unwrap_err().code(),
                "invalid_template",
                "{bad}"
            );
        }
        assert!(valid_key("faq_agent"));
        assert!(!valid_key("Faq"));
        assert!(!valid_key("a"));
        assert!(!valid_key("1abc"));
        assert!(!valid_key(&"a".repeat(64)));
        assert_eq!(label(" Ok ").unwrap(), "Ok");
        assert_eq!(label("").unwrap_err().code(), "invalid_label");
    }
}
