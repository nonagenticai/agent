//! Generator runs, reads and estimate (B8 T9a; P18):
//!
//! - `GET /api/platform/generator/runs` — super_admin: `Paged<GeneratorRunSummary>` (+ `started_by_name`, `tagged`,
//!   `provisional_tags`), newest first, every tenant's runs (platform transaction).
//! - `GET /api/platform/generator/runs/{id}` — super_admin: `GeneratorRun`; 404 `run_not_found`.
//! - `POST /api/generator/runs/estimate` `{scope, tenant_id?}` — super_admin, or a `job_designer` for a tenant run
//!   of their own tenant (403 `super_admin_required` for a platform estimate): `GeneratorEstimate`; 404
//!   `tenant_not_found`.
//!
//! Names are resolved per run tenant under that tenant's own scope (`begin_scoped`), never as `agent_platform`.
//! `provisional_tags` is empty until B6's provisional `v0` tag writer exists (T10, after KH).
use std::collections::{BTreeMap, BTreeSet};

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::{Caller, Role};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::generator::findings;
use crate::generator::run::{self, RunRow, Scope};
use crate::generator::store;
use crate::members::names;
use crate::page::paged;
use crate::platform_db::begin_platform;
use crate::state::AppState;
use crate::tenancy::{self, SuperAdmin, TenantCtx};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/platform/generator/runs", get(list_runs))
        .route("/api/platform/generator/runs/{id}", get(get_run))
        .route("/api/generator/runs/estimate", post(estimate))
}

/// The display names of `subs` per tenant, each read under that tenant's scope.
async fn names_by_tenant(
    st: &AppState,
    wanted: &BTreeMap<Uuid, BTreeSet<String>>,
) -> ApiResult<BTreeMap<(Uuid, String), String>> {
    let mut out = BTreeMap::new();
    for (tenant, subs) in wanted {
        let subs: Vec<String> = subs.iter().cloned().collect();
        let mut tx = tenancy::begin_scoped(&st.db, *tenant).await?;
        let got = names::lookup(&mut tx, &subs).await?;
        tx.commit().await?;
        for (sub, name) in got {
            out.insert((*tenant, sub), name);
        }
    }
    Ok(out)
}

fn tenant_name<'a>(names: &'a [(Uuid, String)], r: &RunRow) -> Option<&'a str> {
    names
        .iter()
        .find(|(id, _)| *id == r.tenant_id)
        .map(|(_, n)| n.as_str())
}

async fn list_runs(
    SuperAdmin(_c): SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let mut tx = begin_platform(&st.db).await?;
    let runs = store::list(&mut tx).await?;
    let mut details = Vec::with_capacity(runs.len());
    for r in &runs {
        details.push(store::detail(&mut tx, r.id).await?);
    }
    let tenants = store::tenant_names(&mut tx).await?;
    tx.commit().await?;
    let mut wanted: BTreeMap<Uuid, BTreeSet<String>> = BTreeMap::new();
    for r in &runs {
        wanted
            .entry(r.tenant_id)
            .or_default()
            .insert(r.started_by.clone());
    }
    let people = names_by_tenant(&st, &wanted).await?;
    let items = details
        .iter()
        .map(|d| {
            let (_, findings) = run::hydrate(d);
            let mut v = run::summary_json(
                &d.run,
                tenant_name(&tenants, &d.run),
                run::red_of(&findings),
            );
            let name = people
                .get(&(d.run.tenant_id, d.run.started_by.clone()))
                .cloned()
                .unwrap_or_else(|| d.run.started_by.clone());
            if let Some(o) = v.as_object_mut() {
                o.insert("started_by_name".into(), json!(name));
                o.insert("tagged".into(), json!(d.run.tagged));
                o.insert("provisional_tags".into(), json!(Vec::<String>::new()));
            }
            v
        })
        .collect();
    Ok(Json(paged(items, None)))
}

async fn get_run(
    SuperAdmin(_c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<Value>> {
    let n: i64 = id
        .parse()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| store::run_not_found(&id))?;
    let mut tx = begin_platform(&st.db).await?;
    let d = store::detail(&mut tx, n).await?;
    // the hydration plus the grid's findings and rule 7's evidence (an open platform run at Validate or later)
    let f = findings::all(&mut tx, &d, false).await?;
    let tenants = store::tenant_names(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(run::detail_json_with(
        &d,
        tenant_name(&tenants, &d.run),
        Some(f),
    )))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EstimateInput {
    scope: String,
    #[serde(default)]
    tenant_id: Option<String>,
}

fn tenant_not_found() -> ApiError {
    ApiError::Missing("tenant_not_found", "No tenant with that id.".into())
}

/// `estimateGeneratorRun` (fixture): who the run is for decides whose usage it is (D56). A tenant caller's basis is
/// its own tenant's packaged runs (RLS); a super_admin's every tenant's.
async fn estimate(
    caller: Caller,
    t: Result<TenantCtx, ApiError>,
    State(st): State<AppState>,
    ApiJson(input): ApiJson<EstimateInput>,
) -> ApiResult<Json<Value>> {
    let scope = Scope::parse(input.scope.trim()).ok_or_else(|| {
        ApiError::Unprocessable("invalid_input", "scope is platform or tenant.".into())
    })?;
    let asked = match input.tenant_id.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => Some(tenancy::tenant_id(raw)?),
    };
    let (tenant_name, past, funding) = if caller.super_admin {
        let mut tx = begin_platform(&st.db).await?;
        let names = store::tenant_names(&mut tx).await?;
        let (past, funding) = store::packaged_usage(&mut tx, scope.as_str()).await?;
        tx.commit().await?;
        let name = match asked {
            None => None,
            Some(id) => Some(
                names
                    .into_iter()
                    .find(|(t, _)| *t == id)
                    .map(|(_, n)| n)
                    .ok_or_else(tenant_not_found)?,
            ),
        };
        (name, past, funding)
    } else {
        let t = t?;
        t.require(Role::JobDesigner)?;
        if scope != Scope::Tenant {
            return Err(ApiError::Forbidden(
                "super_admin_required",
                "Only a super_admin starts a platform run.".into(),
            ));
        }
        if asked.is_some_and(|a| a != t.tenant) {
            return Err(ApiError::Forbidden("forbidden", "Not your tenant.".into()));
        }
        let mut tx = t.begin(&st.db).await?;
        let name: Option<String> = sqlx::query_scalar("SELECT name FROM tenants WHERE id = $1")
            .bind(t.tenant)
            .fetch_optional(&mut *tx)
            .await?;
        let (past, funding) = store::packaged_usage(&mut tx, scope.as_str()).await?;
        tx.commit().await?;
        (name, past, funding)
    };
    let tokens: Vec<i64> = past.iter().map(|(_, t)| *t).collect();
    let (lo, hi) = run::estimate_range(scope, &tokens);
    let tenant_run = scope == Scope::Tenant;
    // D89: a tenant run drafts on the tenant's own `generator` use (tenant_byo); a platform run on the platform
    // provider record. The lane of the newest past run wins when there is one.
    let funding = funding.unwrap_or_else(|| {
        if tenant_run {
            "tenant_byo".to_string()
        } else {
            "platform_external".to_string()
        }
    });
    Ok(Json(json!({
        "scope": scope.as_str(),
        "tokens_low": lo,
        "tokens_high": hi,
        "funding": funding,
        "attributed_to": if tenant_run { "tenant" } else { "platform" },
        "tenant_name": if tenant_run { tenant_name } else { None },
        "steps": run::steps_for(scope).iter().map(|k| k.as_str()).collect::<Vec<_>>(),
        "llm_steps": run::llm_steps_for(scope).iter().map(|k| k.as_str()).collect::<Vec<_>>(),
        "basis": past.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
    })))
}
