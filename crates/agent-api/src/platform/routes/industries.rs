//! Industries and departments (plan 2026-10-30-super-admin-platform-fixes T3): the super_admin's own view of the
//! `industry` and `industry_dept` layers (D106/D107), with no active tenant.
//!
//! - `GET /api/platform/industries` (platformIndustries) → `{base_knowledge: PackRef[], items: PlatformIndustry[]}`:
//!   the packs the `base` layer references, and every industry (region sub-layers left out), by key, with its
//!   departments; each with its label, `extends` (departments), latest tag, `retired_at`, the knowledge packs its
//!   layer references and the tenant jobs that select it.
//! - `POST /api/platform/industries` `{industry, label}` → 201 `PlatformIndustry`: an `industry/<industry>` layer.
//! - `PATCH /api/platform/industries/{industry}` `{label}` → `PlatformIndustry`. The key never changes: tags,
//!   pins and selections hold it.
//! - `POST /api/platform/industries/{industry}/departments` `{department, label, extends?}` → 201
//!   `PlatformDepartment`: an `industry_dept/<industry>/<department>` layer; `extends` names a base
//!   `department_archetype` (404 `parent_layer_not_found` otherwise, `insert_layer`).
//! - `PATCH /api/platform/industries/{industry}/departments/{department}` `{label?, areas?}` → `PlatformDepartment`:
//!   the label and/or the department's `areas` (the release target's area choices). At least one is required (422
//!   `nothing_to_update`); an area is a unique, non-empty key of at most 64 characters (422 `invalid_areas`). This
//!   is the only way to give a department areas after creation — a layer cannot be re-created (`layer_exists`).
//! - `POST …/retire` and `POST …/restore` on both → the item. Retire is refused 409 `in_use{jobs}` while a tenant
//!   job selects the industry / department, 409 `no_change` when it already is (not) retired. A retired one leaves
//!   the tenant choices (`release::compile::db_options`); what pins it keeps compiling.
//!
//! Errors: 422 `invalid_layer_key` (a key that is not a snake key), `invalid_label`; 404 `industry_not_found` /
//! `department_not_found`; 409 `layer_exists`. One platform audit row per write (`industry.*`, `department.*`).
//! Every handler is `SuperAdmin`. Creating content (journeys, knowledge) stays on its own pages.
use std::collections::{BTreeMap, BTreeSet};

use agent_model::layer::{LayerKey, Scope};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::layers::{LatestTags, latest_tags};
use super::{
    Who, audit, blocked, check_label, check_snake_key, conflict, missing, read_tx, unprocessable,
    write_tx,
};
use crate::audit::PlatformEvent;
use crate::error::{ApiJson, ApiPath, ApiResult};
use crate::platform::db::authored::{LayerMeta, insert_layer};
use crate::platform::db::layers::{self, Layer};
use crate::platform::routes::compile::{TenantJob, tenant_jobs};
use crate::state::AppState;
use crate::tenancy::SuperAdmin;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/platform/industries",
            get(list_industries).post(create_industry),
        )
        .route(
            "/api/platform/industries/{industry}",
            patch(rename_industry),
        )
        .route(
            "/api/platform/industries/{industry}/retire",
            post(retire_industry),
        )
        .route(
            "/api/platform/industries/{industry}/restore",
            post(restore_industry),
        )
        .route(
            "/api/platform/industries/{industry}/departments",
            post(create_department),
        )
        .route(
            "/api/platform/industries/{industry}/departments/{department}",
            patch(update_department),
        )
        .route(
            "/api/platform/industries/{industry}/departments/{department}/retire",
            post(retire_department),
        )
        .route(
            "/api/platform/industries/{industry}/departments/{department}/restore",
            post(restore_department),
        )
}

/// A tenant job that selects an industry or department (the `in_use` blockers and the usage list).
#[derive(Debug, Clone, Serialize)]
pub struct JobUse {
    pub tenant_id: String,
    pub tenant_name: String,
    pub job: String,
    pub job_label: String,
}

/// A knowledge pack a layer references: a current `knowledge_pack` row `<stage>|<kind>|<pack>` → `version`
/// (`knowledge::packs::curation::reference_key`; `*` = every stage / kind).
#[derive(Debug, Clone, Serialize)]
pub struct PackRef {
    pub pack: String,
    pub stage: String,
    pub kind: String,
    pub version: Option<i64>,
}

/// UI `PlatformDepartment`.
#[derive(Debug, Clone, Serialize)]
pub struct PlatformDepartment {
    pub department: String,
    /// `industry_dept/<industry>/<department>`.
    pub key: String,
    pub label: String,
    pub extends: Option<String>,
    /// `<family>@v<n>`; null until the department is first published (a tenant cannot choose it before).
    pub tag: Option<String>,
    pub retired_at: Option<DateTime<Utc>>,
    pub knowledge: Vec<PackRef>,
    pub jobs: Vec<JobUse>,
}

/// UI `PlatformIndustry`.
#[derive(Debug, Clone, Serialize)]
pub struct PlatformIndustry {
    pub industry: String,
    /// `industry/<industry>`.
    pub key: String,
    pub label: String,
    pub tag: Option<String>,
    pub retired_at: Option<DateTime<Utc>>,
    pub knowledge: Vec<PackRef>,
    pub jobs: Vec<JobUse>,
    pub departments: Vec<PlatformDepartment>,
}

fn industry_key(industry: &str) -> String {
    format!("industry/{industry}")
}

fn department_key(industry: &str, department: &str) -> String {
    format!("industry_dept/{industry}/{department}")
}

fn tag_of(l: &Layer, tags: &LatestTags) -> Option<String> {
    tags.get(&l.family)
        .map(|(v, _)| format!("{}@v{v}", l.family))
}

fn use_of(j: &TenantJob) -> JobUse {
    JobUse {
        tenant_id: j.tenant_id.to_string(),
        tenant_name: j.tenant_name.clone(),
        job: j.key.clone(),
        job_label: j.label.clone(),
    }
}

/// The tenant jobs whose saved selection names `industry` (and, with `department`, that department layer key).
fn selecting(jobs: &[TenantJob], industry: &str, department: Option<&str>) -> Vec<JobUse> {
    jobs.iter()
        .filter(|j| {
            let Some(sel) = j.selection.as_ref() else {
                return false;
            };
            if sel.get("industry").and_then(Value::as_str) != Some(industry) {
                return false;
            }
            match department {
                None => true,
                Some(key) => sel
                    .get("departments")
                    .and_then(Value::as_array)
                    .is_some_and(|ds| ds.iter().any(|d| d.as_str() == Some(key))),
            }
        })
        .map(use_of)
        .collect()
}

/// Layer key → the knowledge packs it currently references, by pack key.
async fn pack_refs(conn: &mut PgConnection) -> ApiResult<BTreeMap<String, Vec<PackRef>>> {
    let rows: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT l.key, r.key, r.fields FROM layer_row r JOIN layer l ON l.id = r.layer_id \
          WHERE r.kind = 'knowledge_pack' AND r.is_current AND r.op <> 'tombstone' \
          ORDER BY l.key, r.key",
    )
    .fetch_all(conn)
    .await?;
    let mut out: BTreeMap<String, Vec<PackRef>> = BTreeMap::new();
    for (layer, key, fields) in rows {
        let mut parts = key.splitn(3, '|');
        let (Some(stage), Some(kind), Some(pack)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        out.entry(layer).or_default().push(PackRef {
            pack: pack.to_owned(),
            stage: stage.to_owned(),
            kind: kind.to_owned(),
            version: fields.get("version").and_then(Value::as_i64),
        });
    }
    Ok(out)
}

/// Every industry with its departments, read on `conn` (the tenant jobs as `agent_app`, tenant by tenant).
async fn read_all(conn: &mut PgConnection) -> ApiResult<Vec<PlatformIndustry>> {
    let tags = latest_tags(&mut *conn).await?;
    let all = layers::layers(&mut *conn).await?;
    let packs = pack_refs(&mut *conn).await?;
    let jobs = tenant_jobs(&mut *conn).await?;
    let knowledge = |key: &str| packs.get(key).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for ind in all
        .iter()
        .filter(|l| l.scope == "industry" && l.region.is_none())
    {
        let Some(industry) = ind.industry.as_deref() else {
            continue;
        };
        let departments = all
            .iter()
            .filter(|l| {
                l.scope == "industry_dept"
                    && l.region.is_none()
                    && l.industry.as_deref() == Some(industry)
            })
            .filter_map(|d| {
                let department = d.department.as_deref()?;
                Some(PlatformDepartment {
                    department: department.to_owned(),
                    key: d.key.clone(),
                    label: d.label.clone().unwrap_or_else(|| department.to_owned()),
                    extends: d.extends.clone(),
                    tag: tag_of(d, &tags),
                    retired_at: d.retired_at,
                    knowledge: knowledge(&d.key),
                    jobs: selecting(&jobs, industry, Some(&d.key)),
                })
            })
            .collect();
        out.push(PlatformIndustry {
            industry: industry.to_owned(),
            key: ind.key.clone(),
            label: ind.label.clone().unwrap_or_else(|| industry.to_owned()),
            tag: tag_of(ind, &tags),
            retired_at: ind.retired_at,
            knowledge: knowledge(&ind.key),
            jobs: selecting(&jobs, industry, None),
            departments,
        });
    }
    Ok(out)
}

async fn one_industry(conn: &mut PgConnection, industry: &str) -> ApiResult<PlatformIndustry> {
    read_all(conn)
        .await?
        .into_iter()
        .find(|i| i.industry == industry)
        .ok_or_else(|| not_found_industry(industry))
}

async fn one_department(
    conn: &mut PgConnection,
    industry: &str,
    department: &str,
) -> ApiResult<PlatformDepartment> {
    one_industry(conn, industry)
        .await?
        .departments
        .into_iter()
        .find(|d| d.department == department)
        .ok_or_else(|| not_found_department(industry, department))
}

fn not_found_industry(industry: &str) -> crate::error::ApiError {
    missing("industry_not_found", format!("No industry “{industry}”."))
}

fn not_found_department(industry: &str, department: &str) -> crate::error::ApiError {
    missing(
        "department_not_found",
        format!("{industry} has no department “{department}”."),
    )
}

/// The layer for `key`, locked for the write; `None` when there is none.
async fn locked(conn: &mut PgConnection, key: &str) -> ApiResult<Option<(String, bool)>> {
    Ok(sqlx::query_as(
        "SELECT key, retired_at IS NOT NULL FROM layer WHERE key = $1 AND region IS NULL FOR UPDATE",
    )
    .bind(key)
    .fetch_optional(conn)
    .await?)
}

async fn set_label(conn: &mut PgConnection, key: &str, label: &str) -> ApiResult<()> {
    sqlx::query("UPDATE layer SET label = $2 WHERE key = $1")
        .bind(key)
        .bind(label.trim())
        .execute(conn)
        .await?;
    Ok(())
}

async fn set_retired(conn: &mut PgConnection, key: &str, by: Option<&str>) -> ApiResult<()> {
    sqlx::query(
        "UPDATE layer SET retired_at = CASE WHEN $2::text IS NULL THEN NULL ELSE now() END, retired_by = $2 \
          WHERE key = $1",
    )
    .bind(key)
    .bind(by)
    .execute(conn)
    .await?;
    Ok(())
}

// ---- reads ---------------------------------------------------------------------------------------------------------

async fn list_industries(
    SuperAdmin(_c): SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let mut tx = read_tx(&st).await?;
    let base = pack_refs(&mut tx).await?.remove("base").unwrap_or_default();
    let items = read_all(&mut tx).await?;
    Ok(Json(json!({ "base_knowledge": base, "items": items })))
}

// ---- industries ----------------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateIndustry {
    industry: String,
    label: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rename {
    label: String,
}

/// A department's label and/or its `areas`; at least one is required.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateDepartment {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    areas: Option<Vec<String>>,
}

async fn create_industry(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<CreateIndustry>,
) -> ApiResult<(StatusCode, Json<PlatformIndustry>)> {
    let who = Who::of(&sa);
    check_snake_key("invalid_layer_key", &b.industry)?;
    check_label(&b.label)?;
    let mut key = LayerKey::new(Scope::Industry);
    key.industry = Some(b.industry.clone());
    let meta = LayerMeta {
        label: Some(b.label.trim().to_owned()),
        ..LayerMeta::default()
    };
    let mut tx = write_tx(&st).await?;
    let layer = insert_layer(&mut tx, &key, None, &meta).await?;
    audit(
        &mut tx,
        &who,
        PlatformEvent::new("industry.create", layer.key.as_str()).detail("label", b.label.trim()),
    )
    .await?;
    tx.commit().await?;
    let mut tx = read_tx(&st).await?;
    Ok((
        StatusCode::CREATED,
        Json(one_industry(&mut tx, &b.industry).await?),
    ))
}

async fn rename_industry(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(industry): ApiPath<String>,
    ApiJson(b): ApiJson<Rename>,
) -> ApiResult<Json<PlatformIndustry>> {
    let who = Who::of(&sa);
    check_label(&b.label)?;
    let key = industry_key(&industry);
    let mut tx = write_tx(&st).await?;
    locked(&mut tx, &key)
        .await?
        .ok_or_else(|| not_found_industry(&industry))?;
    set_label(&mut tx, &key, &b.label).await?;
    audit(
        &mut tx,
        &who,
        PlatformEvent::new("industry.rename", key.as_str()).detail("label", b.label.trim()),
    )
    .await?;
    tx.commit().await?;
    let mut tx = read_tx(&st).await?;
    Ok(Json(one_industry(&mut tx, &industry).await?))
}

async fn retire_industry(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(industry): ApiPath<String>,
) -> ApiResult<Json<PlatformIndustry>> {
    industry_retired(sa, st, industry, true).await
}

async fn restore_industry(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(industry): ApiPath<String>,
) -> ApiResult<Json<PlatformIndustry>> {
    industry_retired(sa, st, industry, false).await
}

async fn industry_retired(
    sa: SuperAdmin,
    st: AppState,
    industry: String,
    retire: bool,
) -> ApiResult<Json<PlatformIndustry>> {
    let who = Who::of(&sa);
    let key = industry_key(&industry);
    let mut tx = write_tx(&st).await?;
    let (_, retired) = locked(&mut tx, &key)
        .await?
        .ok_or_else(|| not_found_industry(&industry))?;
    no_change(&key, retired, retire)?;
    if retire {
        let jobs = tenant_jobs(&mut tx).await?;
        let users = selecting(&jobs, &industry, None);
        if !users.is_empty() {
            return Err(in_use(&key, users));
        }
    }
    set_retired(&mut tx, &key, retire.then_some(who.by.as_str())).await?;
    let action = if retire {
        "industry.retire"
    } else {
        "industry.restore"
    };
    audit(&mut tx, &who, PlatformEvent::new(action, key.as_str())).await?;
    tx.commit().await?;
    let mut tx = read_tx(&st).await?;
    Ok(Json(one_industry(&mut tx, &industry).await?))
}

fn no_change(key: &str, retired: bool, retire: bool) -> ApiResult<()> {
    if retired == retire {
        return Err(conflict(
            "no_change",
            format!(
                "{key} is already {}.",
                if retire { "retired" } else { "offered" }
            ),
        ));
    }
    Ok(())
}

fn in_use(key: &str, users: Vec<JobUse>) -> crate::error::ApiError {
    let n = users.len();
    blocked(
        "in_use",
        format!(
            "{n} tenant job{} still select{} {key}: move {} to another before retiring it.",
            if n == 1 { "" } else { "s" },
            if n == 1 { "s" } else { "" },
            if n == 1 { "it" } else { "them" },
        ),
        "jobs",
        json!(users),
    )
}

// ---- departments ---------------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateDepartment {
    department: String,
    label: String,
    extends: Option<String>,
}

async fn create_department(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(industry): ApiPath<String>,
    ApiJson(b): ApiJson<CreateDepartment>,
) -> ApiResult<(StatusCode, Json<PlatformDepartment>)> {
    let who = Who::of(&sa);
    check_snake_key("invalid_layer_key", &b.department)?;
    check_label(&b.label)?;
    let mut tx = write_tx(&st).await?;
    let (_, retired) = locked(&mut tx, &industry_key(&industry))
        .await?
        .ok_or_else(|| not_found_industry(&industry))?;
    if retired {
        return Err(conflict(
            "industry_retired",
            format!("{industry} is retired: restore it before adding a department."),
        ));
    }
    let mut key = LayerKey::new(Scope::IndustryDept);
    key.industry = Some(industry.clone());
    key.department = Some(b.department.clone());
    let meta = LayerMeta {
        label: Some(b.label.trim().to_owned()),
        extends: b.extends.clone().filter(|e| !e.trim().is_empty()),
        ..LayerMeta::default()
    };
    let layer = insert_layer(&mut tx, &key, None, &meta).await?;
    let mut ev =
        PlatformEvent::new("department.create", layer.key.as_str()).detail("label", b.label.trim());
    if let Some(ext) = meta.extends.as_deref() {
        ev = ev.detail("extends", ext);
    }
    audit(&mut tx, &who, ev).await?;
    tx.commit().await?;
    let mut tx = read_tx(&st).await?;
    Ok((
        StatusCode::CREATED,
        Json(one_department(&mut tx, &industry, &b.department).await?),
    ))
}

/// Rename the department and/or replace its `areas` — the release target's area choices
/// (`release::compile::db_options`). A job on a department with no area cannot cut a release, and the layer cannot
/// be re-created (`layer_exists`), so this is the route that repairs a department created without them.
async fn update_department(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((industry, department)): ApiPath<(String, String)>,
    ApiJson(b): ApiJson<UpdateDepartment>,
) -> ApiResult<Json<PlatformDepartment>> {
    let who = Who::of(&sa);
    let label = match b.label.as_deref() {
        Some(label) => {
            check_label(label)?;
            Some(label.trim().to_owned())
        }
        None => None,
    };
    let areas = b.areas.map(areas_of).transpose()?;
    if label.is_none() && areas.is_none() {
        return Err(unprocessable(
            "nothing_to_update",
            "Send the department's label, its areas, or both.",
        ));
    }
    let key = department_key(&industry, &department);
    let mut tx = write_tx(&st).await?;
    locked(&mut tx, &key)
        .await?
        .ok_or_else(|| not_found_department(&industry, &department))?;
    sqlx::query(
        "UPDATE layer SET label = COALESCE($2::text, label), areas = COALESCE($3::text[], areas) WHERE key = $1",
    )
    .bind(&key)
    .bind(label.as_deref())
    .bind(areas.as_deref())
    .execute(&mut *tx)
    .await?;
    let mut event = PlatformEvent::new("department.update", key.as_str());
    if let Some(label) = label.as_deref() {
        event = event.detail("label", label);
    }
    if let Some(areas) = areas.as_ref() {
        event = event.detail("areas", json!(areas));
    }
    audit(&mut tx, &who, event).await?;
    tx.commit().await?;
    let mut tx = read_tx(&st).await?;
    Ok(Json(one_department(&mut tx, &industry, &department).await?))
}

/// The department's `areas` as selection keys: each is trimmed, non-empty, unique and at most 64 characters.
fn areas_of(raw: Vec<String>) -> ApiResult<Vec<String>> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(raw.len());
    for area in raw {
        let area = area.trim();
        if area.is_empty()
            || area.chars().count() > 64
            || area.chars().any(char::is_control)
            || !seen.insert(area.to_owned())
        {
            return Err(unprocessable(
                "invalid_areas",
                "An area is a unique, non-empty key of at most 64 characters.",
            ));
        }
        out.push(area.to_owned());
    }
    Ok(out)
}

async fn retire_department(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((industry, department)): ApiPath<(String, String)>,
) -> ApiResult<Json<PlatformDepartment>> {
    department_retired(sa, st, industry, department, true).await
}

async fn restore_department(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((industry, department)): ApiPath<(String, String)>,
) -> ApiResult<Json<PlatformDepartment>> {
    department_retired(sa, st, industry, department, false).await
}

async fn department_retired(
    sa: SuperAdmin,
    st: AppState,
    industry: String,
    department: String,
    retire: bool,
) -> ApiResult<Json<PlatformDepartment>> {
    let who = Who::of(&sa);
    let key = department_key(&industry, &department);
    let mut tx = write_tx(&st).await?;
    let (_, retired) = locked(&mut tx, &key)
        .await?
        .ok_or_else(|| not_found_department(&industry, &department))?;
    no_change(&key, retired, retire)?;
    if retire {
        let jobs = tenant_jobs(&mut tx).await?;
        let users = selecting(&jobs, &industry, Some(&key));
        if !users.is_empty() {
            return Err(in_use(&key, users));
        }
    }
    set_retired(&mut tx, &key, retire.then_some(who.by.as_str())).await?;
    let action = if retire {
        "department.retire"
    } else {
        "department.restore"
    };
    audit(&mut tx, &who, PlatformEvent::new(action, key.as_str())).await?;
    tx.commit().await?;
    let mut tx = read_tx(&st).await?;
    Ok(Json(one_department(&mut tx, &industry, &department).await?))
}
