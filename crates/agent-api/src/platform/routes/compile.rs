//! B6b platform-model area `compile` (plan B6 §5, T9b + T12a, Revision 2026-10-22): the P5 compile page.
//!
//! - `GET /api/compile/options` (compileOptions) → [`CompileOptions`]: industries, departments as full keys
//!   `industry_dept/<i>/<d>` (D115: an ordered multi-select within one industry), jobs, profiles, the dimensions
//!   (area excluded: areas come with their department), the D111 `regions` (KH `region_vocabulary`) and the tag
//!   versions per family for the T12a "as of" select.
//! - `POST /api/compile` (compilePreview), body [`CompileTargetIn`] → the UI `EffectiveConfig` plus the D107
//!   `knowledge` fold. Without `as_of` the head compile ([`templates::compile_head`]); with `as_of` the members of
//!   those tags only ([`templates::compile_pinned_preview`], F-07). Both in [`compile::Mode::Preview`]: the base-only
//!   selection compiles (labelled `base_only_preview`); any other selection needs the D106/D115 hierarchy (422
//!   `hierarchy_incomplete` / `department_industry_mismatch` / `duplicate_department` / `invalid_selection`), a
//!   region of the vocabulary (422 `unknown_region`), and a known profile (404 `profile_not_found`). Every other
//!   failure is 422 `compile_failed` with every [`CompileError`] in `errors[]` (incl. `department_conflict`, and
//!   `unpinned_layer` for an `as_of` that omits a needed family); an `as_of` pin of no tag is 404 `tag_not_found`.
//!   Runs in one `REPEATABLE READ READ ONLY` transaction, rolled back: it never writes.
//! - `GET /api/compile/references` (compileReferences) → [`CompileReference`]`[]`: the `compile_reference_seed`
//!   rows, then one per B2 release in state `current` / `approved` of every tenant job (R10), read tenant by tenant
//!   ([`tenant_jobs`], R3).
//!
//! The wire selection is KH's ([`SelectionIn`], `deny_unknown_fields`); [`to_model`] is the one place that knows
//! the model shape, and [`preview_compile`] the one place that picks the compile.
//!
//! [`tenant_jobs`] is the R3 cross-tenant read (the plan's `usage::for_each_tenant_job`, narrowed to what B6b
//! reads): the tenant list under `agent_platform` (`agent_list_tenants()`, 0031), then each tenant's jobs, targets
//! and releases as `agent_app` with `app.current_tenant` set (RLS) — no definer migration. `platformDimensionUsage`
//! (crate::platform::routes::business) reads it too.
use std::collections::BTreeMap;

use agent_model::compile::{self, CompileError, Compiled, Selection, SelectionDimensions};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::read_tx;
use crate::design::read::selection_of_value;
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::platform::db::{layers, vocab};
use crate::platform::templates::{self, TemplatePin};
use crate::platform_db;
use crate::state::AppState;
use crate::tenancy::{SuperAdmin, set_tenant};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/compile", post(compile_preview))
        .route("/api/compile/options", get(compile_options))
        .route("/api/compile/references", get(compile_references))
}

// --- the selection seam ----------------------------------------------------------------------------------------

/// The UI `Selection` (KH `types/core.ts`) as the compile page sends it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionIn {
    #[serde(default)]
    pub profile: Option<String>,
    /// `null` / `""` = none (only the base-only preview).
    #[serde(default)]
    pub industry: Option<String>,
    /// `industry_dept/<industry>/<dept>`, primary first.
    #[serde(default)]
    pub departments: Vec<String>,
    #[serde(default)]
    pub areas: Vec<String>,
    #[serde(default)]
    pub dimensions: DimensionsIn,
    #[serde(default)]
    pub region: Option<String>,
}

/// `Selection.dimensions` (D111: region is not a dimension any more).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DimensionsIn {
    #[serde(default)]
    pub channel: Vec<String>,
}

/// The wire selection as the model's (KH: field for field; an empty profile / region reads as none). The
/// hierarchy and region refusals are [`preview_compile`]'s, which knows the job.
pub fn to_model(s: SelectionIn) -> Result<Selection, ApiError> {
    Ok(Selection {
        profile: s.profile.filter(|p| !p.trim().is_empty()),
        industry: s.industry.unwrap_or_default(),
        departments: s.departments,
        areas: s.areas,
        dimensions: SelectionDimensions {
            channel: s.dimensions.channel,
        },
        region: s.region.filter(|r| !r.trim().is_empty()),
    })
}

/// The UI `CompileTarget` + T12a `as_of`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompileTargetIn {
    #[serde(default)]
    pub selection: SelectionIn,
    /// The job layer on top (`customer_care_agent`); null = none.
    #[serde(default)]
    pub job: Option<String>,
    /// F-07: compile the members of these tags instead of the head.
    #[serde(default)]
    pub as_of: Option<Vec<TemplatePin>>,
}

/// THE preview compile of the compile page: D106/D115 (unless base-only) and D111 refusals first, then the head
/// compile, or the pinned one for `as_of`, both in [`compile::Mode::Preview`]. The outer `Err` is the HTTP refusal
/// (422 hierarchy / `unknown_region`, 404 `tag_not_found`, 409 `template_not_tagged` for a draft pin); the inner
/// one the compile's errors ([`compile_failed`]).
pub async fn preview_compile(
    conn: &mut PgConnection,
    selection: &Selection,
    job: Option<&str>,
    as_of: Option<&[TemplatePin]>,
) -> ApiResult<Result<Compiled, Vec<CompileError>>> {
    if !selection.is_base_only_preview(job) {
        templates::hierarchy_refusal(selection)?;
    }
    templates::region_refusal(&mut *conn, selection).await?;
    match as_of {
        Some(pins) => templates::compile_pinned_preview(conn, pins, selection, job).await,
        None => templates::compile_head(conn, selection, job, compile::Mode::Preview).await,
    }
}

/// The refusal of a failed compile: 404 `profile_not_found` or the D106/D115 422 when [`compile::refusal`] says so
/// ([`templates::compile_refusal`]), else 422 `compile_failed` with every error (`{code, key, row_id, message,
/// layers?}`) in `errors[]`.
pub fn compile_failed(errors: &[CompileError]) -> ApiError {
    if compile::refusal(errors).is_some() {
        return templates::compile_refusal(errors);
    }
    let list = serde_json::to_value(errors).unwrap_or_else(|_| Value::Array(Vec::new()));
    let n = errors.len();
    ApiError::detailed(
        StatusCode::UNPROCESSABLE_ENTITY,
        "compile_failed",
        format!(
            "The selection does not compile: {n} error{}.",
            if n == 1 { "" } else { "s" }
        ),
    )
    .with("errors", list)
}

fn to_json<T: Serialize>(v: &T) -> ApiResult<Value> {
    serde_json::to_value(v).map_err(|e| ApiError::Internal(e.into()))
}

async fn compile_preview(
    _sa: SuperAdmin,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<CompileTargetIn>,
) -> ApiResult<Json<Value>> {
    let selection = to_model(b.selection)?;
    let job = b.job.as_deref().map(str::trim).filter(|j| !j.is_empty());
    let mut tx = read_tx(&st).await?;
    let compiled = preview_compile(&mut tx, &selection, job, b.as_of.as_deref()).await?;
    tx.rollback().await?;
    let compiled = compiled.map_err(|e| compile_failed(&e))?;
    let mut body = to_json(&compiled.effective)?;
    if let Some(o) = body.as_object_mut() {
        o.insert("knowledge".to_owned(), to_json(&compiled.knowledge)?);
    }
    Ok(Json(body))
}

// --- compileOptions --------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeyLabel {
    pub key: String,
    pub label: String,
}

/// One department choice: `key` is the selection value `industry_dept/<industry>/<department>`.
#[derive(Debug, Clone, Serialize)]
pub struct DepartmentOption {
    pub industry: String,
    pub key: String,
    /// The bare department key (`customer_service`).
    pub department: String,
    pub label: String,
    pub areas: Vec<KeyLabel>,
    /// D60/D107: the base archetype it specialises (compiled in before it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileOption {
    pub key: String,
    pub label: String,
    pub industry: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DimensionOption {
    pub key: String,
    pub label: String,
    pub values: Vec<KeyLabel>,
}

/// T12a: the non-provisional tag versions of a family, newest first.
#[derive(Debug, Clone, Serialize)]
pub struct TagOption {
    pub family: String,
    pub versions: Vec<i32>,
}

/// UI `CompileOptions` + `regions` (D111) + `tags` (T12a).
#[derive(Debug, Clone, Serialize)]
pub struct CompileOptions {
    pub industries: Vec<KeyLabel>,
    pub departments: Vec<DepartmentOption>,
    pub jobs: Vec<KeyLabel>,
    pub profiles: Vec<ProfileOption>,
    pub dimensions: Vec<DimensionOption>,
    pub regions: Vec<KeyLabel>,
    pub tags: Vec<TagOption>,
}

/// A current `profile` row: (layer key, key, fields, the layer's industry).
type ProfileHeadRow = (String, String, Value, Option<String>);

/// A `compile_reference_seed` row: (key, label, note, selection, job).
type SeedRow = (String, String, String, Value, Option<String>);

fn text_field(fields: &Value, key: &str) -> Option<String> {
    fields
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

async fn compile_options(
    _sa: SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<CompileOptions>> {
    let mut tx = read_tx(&st).await?;
    let mut all = layers::layers(&mut tx).await?;
    all.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));

    let dims = vocab::dimensions(&mut tx).await?;
    let mut values: BTreeMap<String, Vec<KeyLabel>> = BTreeMap::new();
    for v in vocab::dimension_values(&mut tx, None).await? {
        values.entry(v.dimension).or_default().push(KeyLabel {
            key: v.key,
            label: v.label,
        });
    }
    let label_in = |dim: &str, key: &str| -> String {
        values
            .get(dim)
            .and_then(|vs| vs.iter().find(|v| v.key == key))
            .map(|v| v.label.clone())
            .unwrap_or_else(|| key.to_owned())
    };

    let industries = all
        .iter()
        .filter(|l| l.scope == "industry" && l.region.is_none())
        .filter_map(|l| {
            let key = l.industry.clone()?;
            let label = l.label.clone().unwrap_or_else(|| key.clone());
            Some(KeyLabel { key, label })
        })
        .collect();
    let departments = all
        .iter()
        .filter(|l| l.scope == "industry_dept" && l.region.is_none())
        .filter_map(|l| {
            let industry = l.industry.clone()?;
            let department = l.department.clone()?;
            Some(DepartmentOption {
                key: compile::department_key(&industry, &department),
                label: l.label.clone().unwrap_or_else(|| department.clone()),
                areas: l
                    .areas
                    .iter()
                    .map(|a| KeyLabel {
                        key: a.clone(),
                        label: label_in("area", a.as_str()),
                    })
                    .collect(),
                extends: l.extends.clone(),
                industry,
                department,
            })
        })
        .collect();

    let job_rows: Vec<(String, Value)> =
        sqlx::query_as("SELECT key, fields FROM layer_head WHERE kind = 'job' AND op = 'add'")
            .fetch_all(&mut *tx)
            .await?;
    let jobs: BTreeMap<String, String> = job_rows
        .into_iter()
        .map(|(key, fields)| {
            let label = text_field(&fields, "label").unwrap_or_else(|| key.clone());
            (key, label)
        })
        .collect();

    let mut profile_rows: Vec<ProfileHeadRow> = sqlx::query_as(
        "SELECT h.layer_key, h.key, h.fields, l.industry FROM layer_head h JOIN layer l ON l.id = h.layer_id \
          WHERE h.kind = 'profile' AND h.op <> 'tombstone'",
    )
    .fetch_all(&mut *tx)
    .await?;
    profile_rows.sort_by(|a, b| (a.1.as_str(), a.0.as_str()).cmp(&(b.1.as_str(), b.0.as_str())));
    let mut profiles: BTreeMap<String, ProfileOption> = BTreeMap::new();
    for (_, key, fields, layer_industry) in profile_rows {
        profiles.insert(
            key.clone(),
            ProfileOption {
                label: text_field(&fields, "label").unwrap_or_else(|| key.clone()),
                industry: text_field(&fields, "industry").or(layer_industry),
                key,
            },
        );
    }

    let dimensions = dims
        .iter()
        .filter(|d| d.key != "area")
        .map(|d| DimensionOption {
            key: d.key.clone(),
            label: d.label.clone(),
            values: values.get(&d.key).cloned().unwrap_or_default(),
        })
        .collect();
    let regions = templates::region_vocabulary(&mut tx)
        .await?
        .into_iter()
        .map(|key| KeyLabel {
            label: label_in("region", key.as_str()),
            key,
        })
        .collect();

    let tag_rows: Vec<(String, i32)> = sqlx::query_as(
        "SELECT family, version FROM layer_tag WHERE NOT provisional ORDER BY family, version DESC",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut tags: BTreeMap<String, Vec<i32>> = BTreeMap::new();
    for (family, version) in tag_rows {
        tags.entry(family).or_default().push(version);
    }
    tx.rollback().await?;

    Ok(Json(CompileOptions {
        industries,
        departments,
        jobs: jobs
            .into_iter()
            .map(|(key, label)| KeyLabel { key, label })
            .collect(),
        profiles: profiles.into_values().collect(),
        dimensions,
        regions,
        tags: tags
            .into_iter()
            .map(|(family, mut versions)| {
                versions.sort_unstable_by(|a, b| b.cmp(a));
                TagOption { family, versions }
            })
            .collect(),
    }))
}

// --- compileReferences -----------------------------------------------------------------------------------------

/// The release a reference was cut into.
#[derive(Debug, Clone, Serialize)]
pub struct ReleaseRef {
    pub id: String,
    pub tenant_id: String,
    pub tenant: String,
    /// The tenant job's key.
    pub job: String,
    /// `current` | `approved`.
    pub status: String,
}

/// UI `CompileReference`: a named selection (+ job layer) the compile page compares against.
#[derive(Debug, Clone, Serialize)]
pub struct CompileReference {
    pub key: String,
    pub label: String,
    pub note: String,
    pub selection: Selection,
    pub job: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<ReleaseRef>,
}

/// The job template a set of pins names: the `job/<key>` family pin (none for a department-only template).
pub fn job_of_pins(pins: &Value) -> Option<String> {
    pins.as_array()?.iter().find_map(|p| {
        let layer = p.get("layer")?.as_str()?;
        let job = layer.strip_prefix("job/")?;
        (!job.is_empty() && !job.contains('/')).then(|| job.to_owned())
    })
}

async fn compile_references(
    _sa: SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Vec<CompileReference>>> {
    let mut tx = read_tx(&st).await?;
    let seeds: Vec<SeedRow> =
        sqlx::query_as("SELECT key, label, note, selection, job FROM compile_reference_seed")
            .fetch_all(&mut *tx)
            .await?;
    let mut out: Vec<CompileReference> = Vec::with_capacity(seeds.len());
    for (key, label, note, selection, job) in seeds {
        let selection = selection_of_value(&selection).map_err(|e| {
            ApiError::Internal(anyhow::anyhow!(
                "compile_reference_seed {key} has an unreadable selection: {e}"
            ))
        })?;
        out.push(CompileReference {
            key,
            label,
            note,
            selection,
            job,
            release: None,
        });
    }
    out.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));

    for j in tenant_jobs(&mut tx).await? {
        for r in j
            .releases
            .iter()
            .filter(|r| matches!(r.state.as_str(), "current" | "approved"))
        {
            // R15: a pre-KH release keeps its shape; upgraded in memory. An unreadable one is skipped, not a 500
            // of the whole page.
            let selection = match selection_of_value(&r.selection) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(tenant = %j.tenant_id, job = %j.key, release = %r.release_id, error = %e,
                        "compile_reference_release_unreadable");
                    continue;
                }
            };
            out.push(CompileReference {
                key: format!("release/{}/{}/{}", j.tenant_id, j.key, r.release_id),
                label: format!("{} · {} · {}", j.tenant_name, j.label, r.state),
                note: format!(
                    "The selection {} of {} cut into release {} ({}).",
                    j.label, j.tenant_name, r.release_id, r.state
                ),
                selection,
                job: job_of_pins(&r.pins),
                release: Some(ReleaseRef {
                    id: r.release_id.clone(),
                    tenant_id: j.tenant_id.to_string(),
                    tenant: j.tenant_name.clone(),
                    job: j.key.clone(),
                    status: r.state.clone(),
                }),
            });
        }
    }
    tx.rollback().await?;
    Ok(Json(out))
}

// --- the cross-tenant read (R3) --------------------------------------------------------------------------------

/// One B2 release of a tenant job.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TenantRelease {
    pub job_id: Uuid,
    pub release_id: String,
    /// `approved` | `current` | `applied_earlier`.
    pub state: String,
    /// The frozen selection (pre-KH shape possible: read it with `selection_of_value`).
    pub selection: Value,
    pub pins: Value,
    pub approved_at: DateTime<Utc>,
    pub released_at: Option<DateTime<Utc>>,
}

/// One tenant job with its working selection (`job_target`) and its releases.
#[derive(Debug, Clone)]
pub struct TenantJob {
    pub tenant_id: Uuid,
    pub tenant_name: String,
    pub job_id: Uuid,
    pub key: String,
    pub label: String,
    pub pins: Value,
    /// `job_target.selection`; `None` when the job has no target (or a null one).
    pub selection: Option<Value>,
    pub selection_updated_at: Option<DateTime<Utc>>,
    /// Oldest approval first.
    pub releases: Vec<TenantRelease>,
}

type JobRow = (
    Uuid,
    String,
    String,
    Value,
    Option<Value>,
    Option<DateTime<Utc>>,
);

/// Every tenant job (by tenant name, then job key) with its target selection and releases, read in the caller's
/// transaction: the tenant list as `agent_platform` (the caller's role, `read_tx`), then each tenant as `agent_app`
/// with `app.current_tenant` set (RLS); the caller's role is `agent_platform` again on return.
pub async fn tenant_jobs(conn: &mut PgConnection) -> ApiResult<Vec<TenantJob>> {
    let mut tenants: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, name FROM agent_list_tenants()")
            .fetch_all(&mut *conn)
            .await?;
    tenants.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    platform_db::leave(&mut *conn).await?;
    let mut out = Vec::new();
    for (tenant_id, tenant_name) in tenants {
        set_tenant(&mut *conn, tenant_id).await?;
        let jobs: Vec<JobRow> = sqlx::query_as(
            "SELECT j.id, j.key, j.label, j.pins, t.selection, t.updated_at \
               FROM jobs j LEFT JOIN job_target t ON t.tenant_id = j.tenant_id AND t.job_id = j.id",
        )
        .fetch_all(&mut *conn)
        .await?;
        let mut releases: Vec<TenantRelease> = sqlx::query_as::<_, TenantRelease>(
            "SELECT job_id, release_id, state, selection, pins, approved_at, released_at FROM job_release",
        )
        .fetch_all(&mut *conn)
        .await?;
        releases.sort_by(|a, b| {
            (a.approved_at, a.release_id.as_str()).cmp(&(b.approved_at, b.release_id.as_str()))
        });
        let mut list: Vec<TenantJob> = jobs
            .into_iter()
            .map(
                |(job_id, key, label, pins, selection, selection_updated_at)| TenantJob {
                    tenant_id,
                    tenant_name: tenant_name.clone(),
                    job_id,
                    key,
                    label,
                    pins,
                    selection: selection.filter(|s| !s.is_null()),
                    selection_updated_at,
                    releases: releases
                        .iter()
                        .filter(|r| r.job_id == job_id)
                        .cloned()
                        .collect(),
                },
            )
            .collect();
        list.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
        out.extend(list);
    }
    platform_db::enter(&mut *conn).await?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_model_is_field_for_field() {
        let s: SelectionIn = serde_json::from_value(json!({
            "profile": "retail_ecommerce",
            "industry": "retail",
            "departments": ["industry_dept/retail/customer_service"],
            "areas": ["faq"],
            "dimensions": {"channel": ["chat"]},
            "region": "nl"
        }))
        .unwrap();
        let m = to_model(s).unwrap();
        assert_eq!(m.profile.as_deref(), Some("retail_ecommerce"));
        assert_eq!(m.industry, "retail");
        assert_eq!(m.departments, ["industry_dept/retail/customer_service"]);
        assert_eq!(m.areas, ["faq"]);
        assert_eq!(m.dimensions.channel, ["chat"]);
        assert_eq!(m.region.as_deref(), Some("nl"));
    }

    #[test]
    fn empty_wire_selection_is_base_only() {
        let s: SelectionIn =
            serde_json::from_value(json!({"profile": "", "industry": null, "region": ""})).unwrap();
        let m = to_model(s).unwrap();
        assert!(m.is_base_only_preview(None));
        assert_eq!(m.region, None);
    }

    #[test]
    fn legacy_wire_shape_is_refused() {
        assert!(serde_json::from_value::<SelectionIn>(json!({"industry_depts": []})).is_err());
        assert!(
            serde_json::from_value::<SelectionIn>(json!({"dimensions": {"region": ["nl"]}}))
                .is_err()
        );
        assert!(serde_json::from_value::<CompileTargetIn>(json!({"nope": 1})).is_err());
    }

    #[test]
    fn job_of_pins_names_the_job_family() {
        let pins = json!([
            {"layer": "base", "version": 3},
            {"layer": "industry_dept/retail/customer_service", "version": 2},
            {"layer": "job/customer_care_agent", "version": 4}
        ]);
        assert_eq!(job_of_pins(&pins).as_deref(), Some("customer_care_agent"));
        assert_eq!(job_of_pins(&json!([{"layer": "base", "version": 1}])), None);
        assert_eq!(job_of_pins(&json!({})), None);
    }

    #[test]
    fn compile_failed_lists_errors() {
        let e = CompileError {
            code: compile::code::PATCH_TARGET_MISSING.to_owned(),
            key: "entity:X".to_owned(),
            row_id: None,
            message: "no target".to_owned(),
            layers: Vec::new(),
        };
        let err = compile_failed(&[e.clone(), e]);
        assert_eq!(err.code(), "compile_failed");
        assert!(
            matches!(err, ApiError::Detailed { ref extra, .. } if extra["errors"].as_array().map(Vec::len) == Some(2))
        );
    }
}
