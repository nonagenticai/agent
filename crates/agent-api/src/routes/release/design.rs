//! The design reads (B2 T14, spec §4, D62/D69): `liveDesign`, `draftDesign`, `designDiff` and `tenantDesignDiff`.
//!
//! - `GET /api/jobs/{job}/design/live` — any member: `DesignState` of the active release's stored snapshot (the
//!   baseline of a job never released).
//! - `GET /api/jobs/{job}/design/draft` — any member: `DesignState` of the working rows, with `draft_rev`.
//! - `GET /api/jobs/{job}/design/diff` — any member: `DesignDiff`, the working rows vs the live snapshot place by
//!   place. The ledger only annotates a differing place with its history; it never decides what differs.
//! - `GET /api/design/tenant-diff?kinds=a,b` — any member: `TenantDesignDiff`, the tenant-wide places (D69) whose
//!   draft differs from the live value of at least one job, listing only those jobs. Kinds that are not tenant-wide
//!   (or not registered) are ignored, as in the fixture.
//!
//! Ports of the fixture `designStateOf` / `designDiffOf` / `tenantDesignDiff` (`web/src/lib/api/areas/design.ts`).
use std::collections::BTreeSet;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::draft::{Names, change_ref};
use crate::design::{self, DesignOf, DraftRef, JobRef, Place, Registry, Scope, ledger};
use crate::error::{ApiQuery, ApiResult};
use crate::release::compile::TemplateCompiler;
use crate::release::targets::{
    LiveDraftBases, ViewBases, may_show_base, shows_base, view_bases, view_text,
};
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, Member};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/design/live", get(live_design))
        .route("/api/jobs/{job}/design/draft", get(draft_design))
        .route("/api/jobs/{job}/design/diff", get(design_diff))
        .route("/api/design/tenant-diff", get(tenant_design_diff))
}

fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}

async fn current_release(c: &mut PgConnection, job: &JobRef) -> ApiResult<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT current_release_id FROM jobs WHERE id = $1")
            .bind(job.id)
            .fetch_one(&mut *c)
            .await?,
    )
}

/// Fixture `designStateOf`: the job's design `of` at every snapshot place, one line per value (a setting without an
/// override, an overlay place without a tenant row, shows its base: [`view_text`]).
async fn design_state(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<Value> {
    let rows = design::take_snapshot(&mut *c, reg, job, of).await?;
    let bases = if rows.iter().any(|r| shows_base(&r.kind, &r.snap)) {
        view_bases(&mut *c, compiler, job, of).await?
    } else {
        ViewBases::default()
    };
    let mut values = Vec::with_capacity(rows.len());
    for r in &rows {
        let t = reg.target(&r.kind)?;
        values.push(json!({
            "key": r.key,
            "kind": r.kind,
            "ref": Value::Object(r.r#ref.clone()),
            "text": view_text(t, &r.snap, &r.r#ref, &bases),
        }));
    }
    let (name, release_id, draft_rev) = match of {
        DesignOf::Live => ("live", current_release(&mut *c, job).await?, None),
        DesignOf::Draft => ("draft", None, Some(design::draft_rev(&mut *c, job).await?)),
        DesignOf::Release(id) => ("release", Some(id.clone()), None),
    };
    Ok(json!({
        "of": name,
        "release_id": release_id,
        "draft_rev": draft_rev,
        "values": values,
    }))
}

async fn state_of(j: &JobCtx, st: &AppState, of: DesignOf) -> ApiResult<Json<Value>> {
    let jr = job_ref(j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let v = design_state(&mut tx, &st.design, st.compiler.as_ref(), &jr, &of).await?;
    tx.commit().await?;
    Ok(Json(v))
}

async fn live_design(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    state_of(&j, &st, DesignOf::Live).await
}

async fn draft_design(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    state_of(&j, &st, DesignOf::Draft).await
}

/// Fixture `designDiffOf` (a place without a tenant value shows its base, [`view_text`]).
async fn diff_of(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
) -> ApiResult<Value> {
    let deltas = design::diff(&mut *c, reg, job).await?;
    let needed = deltas.iter().any(|d| may_show_base(&d.place.kind));
    let bases = LiveDraftBases::read(&mut *c, compiler, job, needed).await?;
    let mut names = Names::default();
    let mut rows = Vec::with_capacity(deltas.len());
    for d in &deltas {
        let t = reg.target(&d.place.kind)?;
        let mut changes = Vec::with_capacity(d.changes.len());
        for ch in &d.changes {
            changes.push(change_ref(&mut *c, &mut names, ch).await?);
        }
        rows.push(json!({
            "key": d.place.key,
            "kind": d.place.kind,
            "ref": Value::Object(d.place.r#ref.clone()),
            "live": view_text(t, &d.live, &d.place.r#ref, &bases.live),
            "draft": view_text(t, &d.draft, &d.place.r#ref, &bases.draft),
            "changes": changes,
        }));
    }
    Ok(json!({
        "release_id": current_release(&mut *c, job).await?,
        "draft_rev": design::draft_rev(&mut *c, job).await?,
        "rows": rows,
    }))
}

async fn design_diff(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let v = diff_of(&mut tx, &st.design, st.compiler.as_ref(), &jr).await?;
    tx.commit().await?;
    Ok(Json(v))
}

// ---- tenant-wide (D69) ----------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct TenantDiffQuery {
    /// Comma-separated kinds; absent or empty = every tenant-wide kind.
    #[serde(default)]
    kinds: Option<String>,
}

#[derive(sqlx::FromRow)]
struct TenantJob {
    id: Uuid,
    key: String,
    label: String,
}

/// One tenant-wide place being built: its head fields and the jobs whose live value differs.
struct Row {
    key: String,
    head: Value,
    jobs: Vec<Value>,
}

/// The tenant-wide kinds to report (fixture `want`): the asked kinds that are registered tenant-wide kinds, in the
/// asked order without repeats, or every registered tenant-wide kind when none is asked.
fn wanted(reg: &Registry, asked: Option<&str>) -> ApiResult<Vec<&'static str>> {
    let mut tenant_kinds = Vec::new();
    for k in reg.kinds() {
        if reg.target(k)?.scope() == Scope::Tenant {
            tenant_kinds.push(k);
        }
    }
    let asked: Vec<&str> = asked
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if asked.is_empty() {
        return Ok(tenant_kinds);
    }
    let mut out: Vec<&'static str> = Vec::new();
    for a in asked {
        if let Some(k) = tenant_kinds.iter().copied().find(|k| *k == a)
            && !out.contains(&k)
        {
            out.push(k);
        }
    }
    Ok(out)
}

/// Fixture `tenantDesignDiff`: every place of each wanted kind (the tenant's working rows, plus any place a job's
/// live snapshot still stores), its draft read once tenant-wide, compared with each job's live value.
async fn tenant_diff(
    c: &mut PgConnection,
    reg: &Registry,
    tenant: Uuid,
    asked: Option<&str>,
) -> ApiResult<Value> {
    let want = wanted(reg, asked)?;
    let jobs = sqlx::query_as::<_, TenantJob>(
        "SELECT id, key, label FROM jobs WHERE tenant_id = $1 ORDER BY key",
    )
    .bind(tenant)
    .fetch_all(&mut *c)
    .await?;
    let refs: Vec<JobRef> = jobs
        .iter()
        .map(|j| JobRef {
            id: j.id,
            key: j.key.clone(),
        })
        .collect();
    let mut lives = Vec::with_capacity(refs.len());
    if !want.is_empty() {
        for jr in &refs {
            lives.push(design::take_snapshot(&mut *c, reg, jr, &DesignOf::Live).await?);
        }
    }
    let at = Place { tenant, job: None };
    let mut names = Names::default();
    let mut rows: Vec<Row> = Vec::new();
    for kind in want {
        let t = reg.target(kind)?;
        let mut places: Vec<(String, DraftRef)> = Vec::new();
        let mut seen = BTreeSet::new();
        for r in t.places(&mut *c, at).await?.unwrap_or_default() {
            let key = t.key(&r);
            if seen.insert(key.clone()) {
                places.push((key, r));
            }
        }
        for live in &lives {
            for r in live.iter().filter(|r| r.kind == kind) {
                if seen.insert(r.key.clone()) {
                    places.push((r.key.clone(), r.r#ref.clone()));
                }
            }
        }
        for (key, r) in places {
            let draft = t.get(&mut *c, at, &r).await?;
            for (j, jr) in jobs.iter().zip(&refs) {
                let live = design::value(&mut *c, reg, jr, &DesignOf::Live, kind, &r).await?;
                if design::same(t, &live, &draft) {
                    continue;
                }
                let mut changes = Vec::new();
                if let Some(d) = ledger::find(&mut *c, jr, &key).await? {
                    changes.push(change_ref(&mut *c, &mut names, &d).await?);
                }
                let entry = json!({
                    "job": j.key,
                    "job_label": j.label,
                    "live": t.text(&live, &r),
                    "changes": changes,
                });
                let found = rows.iter().position(|x| x.key == key);
                match found {
                    Some(i) => rows[i].jobs.push(entry),
                    None => rows.push(Row {
                        key: key.clone(),
                        head: json!({
                            "key": key,
                            "kind": kind,
                            "ref": Value::Object(r.clone()),
                            "draft": t.text(&draft, &r),
                        }),
                        jobs: vec![entry],
                    }),
                }
            }
        }
    }
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|row| {
            let mut v = row.head;
            v["jobs"] = Value::Array(row.jobs);
            v
        })
        .collect();
    Ok(json!({ "rows": rows }))
}

async fn tenant_design_diff(
    Member(t): Member,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<TenantDiffQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let v = tenant_diff(&mut tx, &st.design, t.tenant, q.kinds.as_deref()).await?;
    tx.commit().await?;
    Ok(Json(v))
}
