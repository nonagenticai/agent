//! The job's draft (B2 T14, spec §4, D54/D62): `draftSummary`, `draftDiff`, `discardDraft`, `publishedChanges` and
//! `restoreRolledBack`.
//!
//! - `GET /api/jobs/{job}/draft` — any member: `DraftSummary {count, changes, pending_kinds}`, newest first.
//! - `GET /api/jobs/{job}/draft/diff` — any member: the `DraftChange[]` of the summary.
//! - `DELETE /api/jobs/{job}/draft/{id}` and `DELETE /api/jobs/{job}/draft` (all) — `job_designer`: the live value
//!   goes back into the working rows (`design::drafts::discard`); 404 `draft_not_found`. Answers the summary.
//! - `GET /api/jobs/{job}/draft/published` — any member: `PublishedChange[]`, newest published first.
//! - `POST /api/jobs/{job}/draft/restore` `{ids: string[] | "all"}` — `job_designer`: 404 `change_not_rolled_back`,
//!   409 `nothing_to_restore` (`design::drafts::restore`). Answers the summary.
//!
//! A change row reads its live value from the active release's snapshot and its draft from the working rows (D62); the
//! ledger only says who changed the place and when. Ports of the fixture `summaryOf` / `changeOf` /
//! `publishedChanges` (`web/src/lib/api/areas/release.ts`). Every read handler runs `ensure_job` first in its job-scoped
//! transaction; discard and restore run it inside `design::drafts` after the tenant-wide job lock (lock order).
use axum::extract::State;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::design::drafts::{self, Restore};
use crate::design::ledger::{self, LedgerRow};
use crate::design::snapshot::ref_of;
use crate::design::{
    self, DesignOf, DesignTarget, DraftRef, JobRef, Place, Registry, Snap, snap_from_json,
};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::release::compile::TemplateCompiler;
use crate::release::targets::{LiveDraftBases, may_show_base, view_text};
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, JobRequires, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/jobs/{job}/draft",
            get(draft_summary).delete(discard_all),
        )
        .route("/api/jobs/{job}/draft/diff", get(draft_diff))
        .route("/api/jobs/{job}/draft/published", get(published_changes))
        .route("/api/jobs/{job}/draft/restore", post(restore_rolled_back))
        .route("/api/jobs/{job}/draft/{id}", delete(discard_one))
}

fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}

pub(super) fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// Fixture `nameOf`, cached per request: [`crate::members::names`] (display name, else email, else the sub); `—`
/// for nobody.
#[derive(Default)]
pub(super) struct Names(crate::members::names::Names);

impl Names {
    pub(super) async fn of(
        &mut self,
        c: &mut PgConnection,
        sub: Option<&str>,
    ) -> ApiResult<String> {
        match sub {
            None => Ok("—".to_string()),
            Some(sub) => self.0.of(c, sub).await,
        }
    }
}

/// A ledger row as the history entry of a diff row (`{id, key, label, at, by, by_name}`).
pub(super) async fn change_ref(
    c: &mut PgConnection,
    names: &mut Names,
    d: &LedgerRow,
) -> ApiResult<Value> {
    let by_name = names.of(&mut *c, Some(d.by_sub.as_str())).await?;
    Ok(json!({
        "id": format!("d-{}", d.id),
        "key": d.key,
        "label": d.label,
        "at": iso(d.at),
        "by": d.by_sub,
        "by_name": by_name,
    }))
}

/// A stored `live_moved.was`: the snap form `{"v": …}`, JSON null for nothing; any other value is taken as the value
/// itself.
fn moved_was(v: Option<&Value>) -> Snap {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::Object(m)) if m.len() == 1 && m.contains_key("v") => m.get("v").cloned(),
        Some(other) => Some(other.clone()),
    }
}

/// Fixture `changeOf(d)` for an unpublished change: live from the active release's snapshot, draft from the working
/// rows, the approved cut that already carries it, and the rollback that moved live under it. `live` and `draft` are
/// values: a place without a tenant value shows the base in force (`bases`, [`view_text`]); `live_moved.was` is a
/// ledger "was" line and keeps the target's text. `uncovered` = the live release when its snapshot does not store
/// the change's place kind ([`design::diff::uncovered_kinds`]): publishing makes this draft value live (owner ruling
/// 2026-10-22), else null.
#[allow(clippy::too_many_arguments)]
async fn change_of(
    c: &mut PgConnection,
    reg: &Registry,
    tenant: Uuid,
    job: &JobRef,
    names: &mut Names,
    bases: &LiveDraftBases,
    uncovered: Option<&(String, Vec<&'static str>)>,
    d: &LedgerRow,
) -> ApiResult<Value> {
    let t: &dyn DesignTarget = reg.target(&d.kind)?;
    let place_kind = design::read::place_of(reg, &d.kind, &d.r#ref)?.kind;
    let uncovered = uncovered
        .filter(|(_, kinds)| kinds.contains(&place_kind.as_str()))
        .map(|(id, _)| id.clone());
    let live = design::value(&mut *c, reg, job, &DesignOf::Live, &d.kind, &d.r#ref).await?;
    let at = Place {
        tenant,
        job: Some(job),
    };
    let draft = t.get(&mut *c, at, &d.r#ref).await?;
    let in_release = ledger::approved_cuts(&mut *c, job, &d.key)
        .await?
        .into_iter()
        .next();
    let live_moved = d.live_moved.as_ref().map(|m| {
        json!({
            "release": m.get("release"),
            "at": m.get("at"),
            "was": t.text(&moved_was(m.get("was")), &d.r#ref),
            "matches_live": design::same(t, &live, &draft),
        })
    });
    let by_name = names.of(&mut *c, Some(d.by_sub.as_str())).await?;
    Ok(json!({
        "id": format!("d-{}", d.id),
        "kind": d.kind,
        "area": d.area,
        "page": d.page,
        "label": d.label,
        "key": d.key,
        "at": iso(d.at),
        "by": d.by_sub,
        "by_name": by_name,
        "live": view_text(t, &live, &d.r#ref, &bases.live),
        "draft": view_text(t, &draft, &d.r#ref, &bases.draft),
        "in_release": in_release,
        "live_moved": live_moved,
        "uncovered": uncovered,
    }))
}

/// The job's unpublished changes, newest first (fixture `summaryOf(..).changes`; ties keep ledger order).
async fn changes(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    tenant: Uuid,
    job: &JobRef,
) -> ApiResult<Vec<Value>> {
    let mut rows = ledger::of_job(&mut *c, job).await?;
    rows.sort_by_key(|a| std::cmp::Reverse(a.at));
    let needed = rows.iter().any(|d| may_show_base(&d.kind));
    let bases = LiveDraftBases::read(&mut *c, compiler, job, needed).await?;
    let uncovered = if rows.is_empty() {
        None
    } else {
        design::diff::uncovered_kinds(&mut *c, reg, job).await?
    };
    let mut names = Names::default();
    let mut out = Vec::with_capacity(rows.len());
    for d in &rows {
        let u = uncovered.as_ref();
        out.push(change_of(&mut *c, reg, tenant, job, &mut names, &bases, u, d).await?);
    }
    Ok(out)
}

/// `DraftSummary`: `pending_kinds` = the ledger kinds no plan has registered yet (D80, `design::PENDING`).
async fn summary(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    tenant: Uuid,
    job: &JobRef,
) -> ApiResult<Value> {
    let changes = changes(c, reg, compiler, tenant, job).await?;
    Ok(json!({
        "count": changes.len(),
        "changes": changes,
        "pending_kinds": design::PENDING,
    }))
}

async fn draft_summary(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let v = summary(&mut tx, &st.design, st.compiler.as_ref(), j.t.tenant, &jr).await?;
    tx.commit().await?;
    Ok(Json(v))
}

async fn draft_diff(JobMember(j): JobMember, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let v = changes(&mut tx, &st.design, st.compiler.as_ref(), j.t.tenant, &jr).await?;
    tx.commit().await?;
    Ok(Json(Value::Array(v)))
}

/// Discard `id` (`None` = every row), then answer the summary.
async fn discard_in(st: &AppState, j: &JobCtx, id: Option<i64>) -> ApiResult<Json<Value>> {
    let jr = job_ref(j);
    let mut tx = j.begin(&st.db).await?;
    // no `ensure_job` here: `discard` runs it after locking every tenant job in id order (lock order)
    drafts::discard(&mut tx, &st.design, j, id).await?;
    let v = summary(&mut tx, &st.design, st.compiler.as_ref(), j.t.tenant, &jr).await?;
    tx.commit().await?;
    Ok(Json(v))
}

async fn discard_one(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let Some(n) = drafts::ledger_id(&id) else {
        return Err(drafts::draft_not_found(&id));
    };
    discard_in(&st, &j, Some(n)).await
}

async fn discard_all(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    discard_in(&st, &j, None).await
}

#[derive(sqlx::FromRow)]
struct Published {
    id: i64,
    key: String,
    kind: String,
    place_ref: Value,
    area: String,
    page: String,
    label: String,
    before: Option<Value>,
    after: Option<Value>,
    at: DateTime<Utc>,
    by_sub: String,
    published_in: String,
    published_at: DateTime<Utc>,
    published_by: String,
    rolled_back: Option<Value>,
    restored_to_draft: Option<Value>,
}

/// A rollback / restore mark (`{to?, at, by}`) with the actor's display name.
async fn mark(
    c: &mut PgConnection,
    names: &mut Names,
    m: Option<&Value>,
    with_to: bool,
) -> ApiResult<Value> {
    let Some(m) = m else {
        return Ok(Value::Null);
    };
    let by = m.get("by").and_then(Value::as_str);
    let by_name = names.of(&mut *c, by).await?;
    let mut out = json!({"at": m.get("at"), "by": m.get("by"), "by_name": by_name});
    if with_to {
        out["to"] = m.get("to").cloned().unwrap_or(Value::Null);
    }
    Ok(out)
}

/// Fixture `publishedChanges`: `changeOf(d, history)` — live = what it replaced, draft = what it published — plus the
/// publish, rollback and restore marks; newest published first.
async fn published_list(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
) -> ApiResult<Vec<Value>> {
    let rows = sqlx::query_as::<_, Published>(
        "SELECT id, key, kind, ref AS place_ref, area, page, label, before, after, at, by_sub, published_in, \
                published_at, published_by, rolled_back, restored_to_draft \
           FROM published_change WHERE job_id = $1 ORDER BY published_at DESC, id",
    )
    .bind(job.id)
    .fetch_all(&mut *c)
    .await?;
    let mut names = Names::default();
    let mut out = Vec::with_capacity(rows.len());
    for p in rows {
        let t = reg.target(&p.kind)?;
        let r: DraftRef = ref_of(p.place_ref)?;
        let before = snap_from_json(p.before)?;
        let after = snap_from_json(p.after)?;
        let by_name = names.of(&mut *c, Some(p.by_sub.as_str())).await?;
        let rolled_back = mark(&mut *c, &mut names, p.rolled_back.as_ref(), true).await?;
        let restored = mark(&mut *c, &mut names, p.restored_to_draft.as_ref(), false).await?;
        out.push(json!({
            "id": format!("pub-{}", p.id),
            "kind": p.kind,
            "area": p.area,
            "page": p.page,
            "label": p.label,
            "key": p.key,
            "at": iso(p.at),
            "by": p.by_sub,
            "by_name": by_name,
            "live": t.text(&before, &r),
            "draft": t.text(&after, &r),
            "in_release": Value::Null,
            "live_moved": Value::Null,
            "published_in": p.published_in,
            "published_at": iso(p.published_at),
            "published_by": p.published_by,
            "rolled_back": rolled_back,
            "restored_to_draft": restored,
        }));
    }
    Ok(out)
}

async fn published_changes(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let v = published_list(&mut tx, &st.design, &jr).await?;
    tx.commit().await?;
    Ok(Json(Value::Array(v)))
}

/// `ids`: the published ids (`pub-<n>`) or the word `"all"`.
#[derive(Deserialize)]
#[serde(untagged)]
enum Ids {
    List(Vec<String>),
    Word(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreBody {
    ids: Ids,
}

async fn restore_rolled_back(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<RestoreBody>,
) -> ApiResult<Json<Value>> {
    let which = match b.ids {
        Ids::List(ids) => Restore::Ids(ids),
        Ids::Word(w) if w == "all" => Restore::All,
        Ids::Word(_) => {
            return Err(ApiError::BadRequest(
                "invalid_body",
                "ids is a list of published change ids or \"all\".".into(),
            ));
        }
    };
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    // no `ensure_job` here: `restore` runs it after locking every tenant job in id order (lock order)
    drafts::restore(&mut tx, &st.design, &j, which).await?;
    let v = summary(&mut tx, &st.design, st.compiler.as_ref(), j.t.tenant, &jr).await?;
    tx.commit().await?;
    Ok(Json(v))
}
