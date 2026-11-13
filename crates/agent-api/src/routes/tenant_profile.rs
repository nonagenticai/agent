//! The company profile (D124): what the tenant's company is, so the platform knowledge that applies can be selected.
//!
//! - `GET /api/tenant/profile` — any member: `{industry, departments, region, languages, description, updated_at, options}`
//!   where `options` = `{industries:[{key,label}], departments:[{key,label,industry}], regions:[{key,label}]}`, read
//!   from the same `SelectionOptions` a job's target uses (`TemplateCompiler::options`), not a second catalog.
//! - `PUT /api/tenant/profile` — `tenant_admin`: `{industry, departments, region, languages?, description?, expected_updated_at?}`.
//!   409 `profile_changed` when `expected_updated_at` is not the stored `profile_updated_at` (null while never set);
//!   422 `unknown_industry` (`unassigned` only without departments), 422 `department_not_in_industry`, 422
//!   `unknown_region` (D111 vocabulary, `platform::templates::region_vocabulary`), 422 `invalid_description`;
//!   409 `industry_in_use` when the industry changes while a job of the tenant is pinned to another industry
//!   (`jobs.pins`' `industry/<i>` pin: the one pin every job carries, D106). Audited `tenant.profile_update {old,new}`.
//!   Existing jobs are never rewritten.
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::audit::{self, Event};
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::platform::templates;
use crate::release::bundle::SelectionOptions;
use crate::release::compile::TemplateCompiler;
use crate::state::AppState;
use crate::tenancy::{Member, Requires, TenantCtx, role};

const UNASSIGNED: &str = "unassigned";

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/tenant/profile", get(read_profile).put(save_profile))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveProfile {
    industry: String,
    departments: Vec<String>,
    region: Option<String>,
    /// D132.4: absent keeps the stored languages.
    #[serde(default)]
    languages: Option<Vec<String>>,
    /// Absent keeps the stored description.
    #[serde(default)]
    description: Option<String>,
    /// The `updated_at` the client last read; `null` while the profile was never saved.
    #[serde(default)]
    expected_updated_at: Option<String>,
}

#[derive(sqlx::FromRow)]
struct ProfileRow {
    industry: String,
    departments: Vec<String>,
    region: Option<String>,
    languages: Vec<String>,
    description: String,
    profile_updated_at: Option<DateTime<Utc>>,
}

fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

async fn row(c: &mut PgConnection, t: &TenantCtx, lock: bool) -> ApiResult<ProfileRow> {
    let sql = if lock {
        "SELECT industry, departments, region, languages, description, profile_updated_at FROM tenants WHERE id = $1 FOR UPDATE"
    } else {
        "SELECT industry, departments, region, languages, description, profile_updated_at FROM tenants WHERE id = $1"
    };
    sqlx::query_as::<_, ProfileRow>(sql)
        .bind(t.tenant)
        .fetch_optional(&mut *c)
        .await?
        .ok_or_else(|| ApiError::Missing("tenant_not_found", format!("no tenant {}", t.tenant)))
}

fn values(r: &ProfileRow) -> Value {
    json!({
        "industry": r.industry,
        "departments": r.departments,
        "region": r.region,
        "languages": r.languages,
        "description": r.description,
    })
}

fn view(r: &ProfileRow, o: &SelectionOptions) -> Value {
    let pick = |l: &[crate::release::bundle::SelectionChoice]| -> Vec<Value> {
        l.iter()
            .map(|c| json!({"key": c.key, "label": c.label}))
            .collect()
    };
    let departments: Vec<Value> = o
        .departments
        .iter()
        .map(|c| json!({"key": c.key, "label": c.label, "industry": c.industry}))
        .collect();
    json!({
        "industry": r.industry,
        "departments": r.departments,
        "region": r.region,
        "languages": r.languages,
        "description": r.description,
        "updated_at": r.profile_updated_at.map(iso),
        "options": {
            "industries": pick(&o.industries),
            "departments": departments,
            "regions": pick(&o.dimensions.region.values),
            // D132: the business profiles a job of this company may use (`industry` null = any industry)
            "profiles": o
                .profiles
                .iter()
                .map(|c| json!({"key": c.key, "label": c.label, "industry": c.industry}))
                .collect::<Vec<_>>(),
        },
    })
}

/// Both absent, or the same instant; anything unparseable is a mismatch.
fn same_instant(expected: Option<&str>, at: Option<DateTime<Utc>>) -> bool {
    match (expected, at) {
        (None, None) => true,
        (Some(s), Some(t)) => {
            DateTime::parse_from_rfc3339(s.trim()).is_ok_and(|d| d.with_timezone(&Utc) == t)
        }
        _ => false,
    }
}

async fn read_profile(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let r = row(&mut tx, &t, false).await?;
    let o = TemplateCompiler::options(st.compiler.as_ref(), &mut tx).await?;
    tx.commit().await?;
    Ok(Json(view(&r, &o)))
}

async fn save_profile(
    Requires(t, _): Requires<role::TenantAdmin>,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<SaveProfile>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let old = row(&mut tx, &t, true).await?;
    if !same_instant(b.expected_updated_at.as_deref(), old.profile_updated_at) {
        return Err(ApiError::Conflict(
            "profile_changed",
            "Someone else changed the company profile; reload and try again.".into(),
        ));
    }
    let o = TemplateCompiler::options(st.compiler.as_ref(), &mut tx).await?;

    let industry = b.industry.trim().to_string();
    if industry != UNASSIGNED && !o.industries.iter().any(|i| i.key == industry) {
        return Err(ApiError::Unprocessable(
            "unknown_industry",
            format!("Unknown industry “{industry}”."),
        ));
    }
    let mut departments: Vec<String> = Vec::with_capacity(b.departments.len());
    for d in &b.departments {
        if departments.contains(d) {
            continue;
        }
        let belongs = industry != UNASSIGNED
            && o.departments
                .iter()
                .any(|c| &c.key == d && c.industry.as_deref() == Some(industry.as_str()));
        if !belongs {
            return Err(ApiError::Unprocessable(
                "department_not_in_industry",
                format!("Department “{d}” is not a department of industry “{industry}”."),
            ));
        }
        departments.push(d.clone());
    }
    let region = b
        .region
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty());
    if let Some(r) = &region {
        let known = templates::region_vocabulary(&mut tx).await?;
        if !known.contains(r) {
            return Err(ApiError::Unprocessable(
                "unknown_region",
                format!(
                    "Unknown region “{r}”; the platform's regions are {}.",
                    known.join(", ")
                ),
            ));
        }
    }
    let languages = match b.languages {
        Some(l) => crate::routes::jobs::valid_languages(l)?,
        None => old.languages.clone(),
    };
    let description = match b.description {
        Some(d) => {
            let d = d.trim().to_string();
            if d.chars().count() > 2000 {
                return Err(ApiError::Unprocessable(
                    "invalid_description",
                    "The description is at most 2000 characters.".into(),
                ));
            }
            d
        }
        None => old.description.clone(),
    };

    if industry != old.industry {
        // every job pins exactly one `industry/<i>` layer (D106); one of another industry blocks the change
        let in_use: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM jobs j, jsonb_array_elements(j.pins) p \
              WHERE left(p->>'layer', 9) = 'industry/' AND p->>'layer' <> 'industry/' || $1)",
        )
        .bind(&industry)
        .fetch_one(&mut *tx)
        .await?;
        if in_use {
            return Err(ApiError::Conflict(
                "industry_in_use",
                "A job of this company runs on another industry; the industry cannot change."
                    .into(),
            ));
        }
    }

    let next = ProfileRow {
        industry,
        departments,
        region,
        languages,
        description,
        profile_updated_at: old.profile_updated_at,
    };
    let (old_v, new_v) = (values(&old), values(&next));
    let next = if old_v == new_v {
        next
    } else {
        let at: DateTime<Utc> = sqlx::query_scalar(
            "UPDATE tenants SET industry = $2, departments = $3, region = $4, description = $5, languages = $7, \
                    profile_updated_at = clock_timestamp(), profile_updated_by = $6, \
                    updated_at = now(), updated_by = $6 \
              WHERE id = $1 RETURNING profile_updated_at",
        )
        .bind(t.tenant)
        .bind(&next.industry)
        .bind(&next.departments)
        .bind(&next.region)
        .bind(&next.description)
        .bind(&t.caller.sub)
        .bind(&next.languages)
        .fetch_one(&mut *tx)
        .await?;
        audit::record(
            &mut tx,
            &t.actor(),
            Event::new("tenant.profile_update", "tenant", t.tenant.to_string())
                .detail("old", old_v)
                .detail("new", new_v),
        )
        .await?;
        ProfileRow {
            profile_updated_at: Some(at),
            ..next
        }
    };
    tx.commit().await?;
    Ok(Json(view(&next, &o)))
}
