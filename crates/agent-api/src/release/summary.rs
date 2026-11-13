//! The release read model (spec B2 §4 releaseList / releaseDetail / releaseCompare / releaseStatus / releaseDesign,
//! §2.7 job summary fields): a port of the fixture `rowsOf` / `toRow` / `changeOf` / `eventOf` and the read methods
//! of `web/src/lib/api/areas/release.ts`, over `job_release`, `release_snapshot`, `release_bundled_draft`,
//! `published_change` and `release_rollback`.
//!
//! Every function reads in the caller's job-scoped transaction. Shapes follow `web/src/lib/api/types/release.ts`
//! exactly (`ReleaseRow`, `ReleaseDetail`, `ReleaseCompare`, `ReleaseStatus`, `DraftChange`, `RollbackEvent`,
//! `DesignState`).
use std::collections::BTreeMap;

use agent_model::compile::Selection;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::bundle::BundleSection;
use super::compile::{CompileError, Compiled, TemplateCompiler, selection_error};
use super::targets::{LiveDraftBases, ViewBases, may_show_base, view_text};
use crate::design::hooks::is_template_refusal;
use crate::design::{self, DesignOf, DraftRef, JobRef, KINDS, Place, Registry, Snap};
use crate::error::{ApiError, ApiResult};
use crate::platform::templates::TemplatePin;
use crate::readiness::{self, Readiness};

/// `ReleaseRef` (`types/core.ts`): a job summary's `current_release`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseRef {
    pub id: String,
    pub hash: String,
}

/// One stored release (`job_release`) plus how many snapshot places and bundled drafts it holds.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Release {
    pub tenant_id: Uuid,
    pub release_id: String,
    pub hash: String,
    pub bundle_hash: String,
    /// Schema `agent.bundle/1` (read leniently: B6 may add sections).
    pub bundle: Value,
    /// `ResolvedSelection` (the selection, profile values inlined).
    pub selection: Value,
    pub pins: Value,
    pub provenance: Value,
    pub requires: Value,
    pub causes: Value,
    pub state: String,
    pub approved_by: String,
    pub approved_at: DateTime<Utc>,
    pub released_by: Option<String>,
    pub released_at: Option<DateTime<Utc>>,
    pub applied_by: Option<String>,
    pub last_applied_at: Option<DateTime<Utc>>,
    pub readiness: Option<Value>,
    pub snapshot_kinds: Vec<String>,
    pub rollbacks_seen: i32,
    /// `release_snapshot` rows.
    pub places: i64,
    /// `release_bundled_draft` rows.
    pub drafts: i64,
}

impl Release {
    /// The template pins the release compiled.
    pub fn pins(&self) -> ApiResult<Vec<TemplatePin>> {
        serde_json::from_value(self.pins.clone())
            .map_err(|e| internal(format!("job_release.pins is unreadable: {e}")))
    }

    /// The bundle's display sections (none when the bundle has no `sections`).
    pub fn sections(&self) -> ApiResult<Vec<BundleSection>> {
        match self.bundle.get("sections") {
            None => Ok(Vec::new()),
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| internal(format!("job_release.bundle.sections is unreadable: {e}"))),
        }
    }
}

const COLUMNS: &str = "r.tenant_id, r.release_id, r.hash, r.bundle_hash, r.bundle, r.selection, r.pins, \
     r.provenance, r.requires, r.causes, r.state, r.approved_by, r.approved_at, r.released_by, r.released_at, \
     r.applied_by, r.last_applied_at, r.readiness, r.snapshot_kinds, r.rollbacks_seen, \
     (SELECT count(*) FROM release_snapshot s WHERE s.job_id = r.job_id AND s.release_id = r.release_id) AS places, \
     (SELECT count(*) FROM release_bundled_draft b WHERE b.job_id = r.job_id AND b.release_id = r.release_id) \
        AS drafts";

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// 404 `release_not_found` (fixture `findRelease` text).
pub fn release_not_found(id: &str) -> ApiError {
    ApiError::Missing(
        "release_not_found",
        format!("This job has no release “{id}”."),
    )
}

/// The job's releases, newest first (fixture `rowsOf` order: approved_at, then id, both descending).
pub async fn releases(c: &mut PgConnection, job: &JobRef) -> ApiResult<Vec<Release>> {
    Ok(sqlx::query_as::<_, Release>(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM job_release r WHERE r.job_id = $1 \
          ORDER BY r.approved_at DESC, r.release_id DESC"
    )))
    .bind(job.id)
    .fetch_all(&mut *c)
    .await?)
}

/// One release of the job; 404 `release_not_found`.
pub async fn load(c: &mut PgConnection, job: &JobRef, id: &str) -> ApiResult<Release> {
    sqlx::query_as::<_, Release>(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM job_release r WHERE r.job_id = $1 AND r.release_id = $2"
    )))
    .bind(job.id)
    .bind(id)
    .fetch_optional(&mut *c)
    .await?
    .ok_or_else(|| release_not_found(id))
}

/// The job's current release (§2.7 job summary field `current_release`), `None` = never released.
pub async fn current_ref(c: &mut PgConnection, job: &JobRef) -> ApiResult<Option<ReleaseRef>> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT r.release_id, r.hash FROM jobs j \
           JOIN job_release r ON r.tenant_id = j.tenant_id AND r.job_id = j.id \
                             AND r.release_id = j.current_release_id \
          WHERE j.id = $1",
    )
    .bind(job.id)
    .fetch_optional(&mut *c)
    .await?;
    Ok(row.map(|(id, hash)| ReleaseRef { id, hash }))
}

/// Display names by sub (fixture `nameOf`), cached per request: the one resolution of [`crate::members::names`]
/// (display name, else email, else the sub).
pub use crate::members::names::Names;

#[derive(sqlx::FromRow)]
struct RolledBackRow {
    at: DateTime<Utc>,
    by_sub: String,
    to_release: String,
    undone: i32,
}

/// Fixture `rolledBackFrom`: the last rollback that took Production away from `r` (only while `r` is not current).
async fn rolled_back(
    c: &mut PgConnection,
    names: &mut Names,
    job: &JobRef,
    r: &Release,
) -> ApiResult<Value> {
    if r.state == "current" {
        return Ok(Value::Null);
    }
    let ev = sqlx::query_as::<_, RolledBackRow>(
        "SELECT at, by_sub, to_release, cardinality(undone) AS undone FROM release_rollback \
          WHERE job_id = $1 AND from_release = $2 AND direction = 'rollback' ORDER BY id DESC LIMIT 1",
    )
    .bind(job.id)
    .bind(&r.release_id)
    .fetch_optional(&mut *c)
    .await?;
    let Some(ev) = ev else {
        return Ok(Value::Null);
    };
    let by_name = names.of(&mut *c, &ev.by_sub).await?;
    Ok(json!({
        "at": iso(ev.at),
        "by": ev.by_sub,
        "by_name": by_name,
        "to": ev.to_release,
        "undone": ev.undone,
    }))
}

/// Fixture `rowsOf` D15: the template tags `r` moved forward against the release cut before it (`prev`).
fn upgrades(r: &Release, prev: &Release) -> ApiResult<Value> {
    let (mine, before) = (r.pins()?, prev.pins()?);
    let list: Vec<Value> = mine
        .iter()
        .filter_map(|p| {
            let from = before.iter().find(|x| x.layer == p.layer)?.version;
            (p.version > from).then(|| json!({"layer": p.layer, "from": from, "to": p.version}))
        })
        .collect();
    Ok(Value::Array(list))
}

/// One `ReleaseRow` (fixture `toRow`); `prev` = the release cut before it in the newest-first list.
pub async fn row_of(
    c: &mut PgConnection,
    names: &mut Names,
    job: &JobRef,
    r: &Release,
    prev: Option<&Release>,
) -> ApiResult<Value> {
    let counts: BTreeMap<String, usize> = r
        .sections()?
        .into_iter()
        .map(|s| (s.key, s.items.len()))
        .collect();
    let applied_by = r.applied_by.clone().or_else(|| r.released_by.clone());
    let last_applied_at = r.last_applied_at.or(r.released_at);
    let mut shown = serde_json::Map::new();
    for sub in [
        Some(&r.approved_by),
        r.released_by.as_ref(),
        applied_by.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        let n = names.of(&mut *c, sub).await?;
        shown.insert(sub.clone(), Value::String(n));
    }
    let rolled = rolled_back(&mut *c, names, job, r).await?;
    let places = if r.snapshot_kinds.is_empty() {
        Value::Null
    } else {
        json!(r.places)
    };
    let mut v = json!({
        "id": r.release_id,
        "hash": r.hash,
        "tenant_id": r.tenant_id,
        "job": job.key,
        "state": r.state,
        "approved_by": r.approved_by,
        "approved_at": iso(r.approved_at),
        "released_by": r.released_by,
        "released_at": r.released_at.map(iso),
        "applied_by": applied_by,
        "last_applied_at": last_applied_at.map(iso),
        "counts": counts,
        "names": shown,
        "draft_count": r.drafts,
        "snapshot_places": places,
        "rolled_back": rolled,
    });
    if let Some(p) = prev {
        v["upgrades"] = upgrades(r, p)?;
    }
    Ok(v)
}

/// `releaseList` items: every `ReleaseRow` of the job, newest first.
pub async fn rows(c: &mut PgConnection, job: &JobRef) -> ApiResult<Vec<Value>> {
    let list = releases(&mut *c, job).await?;
    let mut names = Names::default();
    let mut out = Vec::with_capacity(list.len());
    for (i, r) in list.iter().enumerate() {
        out.push(row_of(&mut *c, &mut names, job, r, list.get(i + 1)).await?);
    }
    Ok(out)
}

/// One `ReleaseRow` (approve / apply results); 404 `release_not_found`.
pub async fn row(c: &mut PgConnection, job: &JobRef, id: &str) -> ApiResult<Value> {
    let list = releases(&mut *c, job).await?;
    let i = list
        .iter()
        .position(|r| r.release_id == id)
        .ok_or_else(|| release_not_found(id))?;
    let mut names = Names::default();
    row_of(&mut *c, &mut names, job, &list[i], list.get(i + 1)).await
}

/// A change row a page shows (`DraftChange`): an unpublished ledger row, or a draft a cut bundled.
#[derive(Debug, Clone)]
pub struct ChangeRef {
    pub ledger_id: i64,
    pub key: String,
    pub kind: String,
    pub r#ref: DraftRef,
    pub area: String,
    pub page: String,
    pub label: String,
    pub at: DateTime<Utc>,
    pub by_sub: String,
}

/// Fixture `changeOf` (not history): `live` = what Production runs at the place now, `draft` = the working rows,
/// one value each ([`view_text`] over `bases`); `in_release` = the approved cut carrying it.
#[allow(clippy::too_many_arguments)]
pub async fn draft_change(
    c: &mut PgConnection,
    reg: &Registry,
    names: &mut Names,
    bases: &LiveDraftBases,
    tenant: Uuid,
    job: &JobRef,
    d: &ChangeRef,
    in_release: Option<&str>,
) -> ApiResult<Value> {
    let t = reg.target(&d.kind)?;
    let live: Snap = design::value(&mut *c, reg, job, &DesignOf::Live, &d.kind, &d.r#ref).await?;
    let at = Place {
        tenant,
        job: Some(job),
    };
    let draft: Snap = t.get(&mut *c, at, &d.r#ref).await?;
    let by_name = names.of(&mut *c, &d.by_sub).await?;
    Ok(json!({
        "id": format!("d-{}", d.ledger_id),
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
        "live_moved": Value::Null,
    }))
}

#[derive(sqlx::FromRow)]
struct BundledRow {
    ledger_id: i64,
    key: String,
    kind: String,
    place_ref: Value,
    area: String,
    page: String,
    label: String,
    at: DateTime<Utc>,
    by_sub: String,
}

#[derive(sqlx::FromRow)]
struct RollbackRow {
    id: i64,
    at: DateTime<Utc>,
    by_sub: String,
    direction: String,
    from_release: Option<String>,
    to_release: String,
    undone: Vec<i64>,
    reapplied: Vec<i64>,
    draft_moved: Vec<String>,
}

#[derive(sqlx::FromRow)]
struct PublishedRow {
    id: i64,
    key: String,
    label: String,
    published_in: String,
}

/// `{id, key, label, published_in}` of the published rows `ids`, in that order (fixture `eventOf` `rowsOf`).
async fn published_rows(c: &mut PgConnection, job: &JobRef, ids: &[i64]) -> ApiResult<Vec<Value>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let found = sqlx::query_as::<_, PublishedRow>(
        "SELECT id, key, label, published_in FROM published_change WHERE job_id = $1 AND id = ANY($2)",
    )
    .bind(job.id)
    .bind(ids)
    .fetch_all(&mut *c)
    .await?;
    Ok(ids
        .iter()
        .filter_map(|i| found.iter().find(|p| p.id == *i))
        .map(|p| {
            json!({
                "id": format!("pub-{}", p.id),
                "key": p.key,
                "label": p.label,
                "published_in": p.published_in,
            })
        })
        .collect())
}

/// Fixture `eventOf`: the rollbacks that moved Production away from release `id` or back to it, newest first.
async fn rollbacks(
    c: &mut PgConnection,
    names: &mut Names,
    job: &JobRef,
    id: &str,
) -> ApiResult<Vec<Value>> {
    let rows = sqlx::query_as::<_, RollbackRow>(
        "SELECT id, at, by_sub, direction, from_release, to_release, undone, reapplied, draft_moved \
           FROM release_rollback WHERE job_id = $1 AND (from_release = $2 OR to_release = $2) \
          ORDER BY id DESC",
    )
    .bind(job.id)
    .bind(id)
    .fetch_all(&mut *c)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for x in rows {
        let by_name = names.of(&mut *c, &x.by_sub).await?;
        let undone = published_rows(&mut *c, job, &x.undone).await?;
        let reapplied = published_rows(&mut *c, job, &x.reapplied).await?;
        out.push(json!({
            "id": format!("rb-{}", x.id),
            "at": iso(x.at),
            "by": x.by_sub,
            "by_name": by_name,
            "direction": x.direction,
            "from": x.from_release,
            "to": x.to_release,
            "undone": undone,
            "reapplied": reapplied,
            "draft_moved": x.draft_moved.len(),
        }));
    }
    Ok(out)
}

/// `releaseDetail` (`ReleaseDetail`): the row plus the stored selection, provenance (+pins), readiness, sections,
/// requirements, the drafts the cut bundled and the rollbacks touching it. 404 `release_not_found`.
pub async fn detail(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
    id: &str,
) -> ApiResult<Value> {
    let list = releases(&mut *c, job).await?;
    let i = list
        .iter()
        .position(|r| r.release_id == id)
        .ok_or_else(|| release_not_found(id))?;
    let r = &list[i];
    let mut names = Names::default();
    let mut v = row_of(&mut *c, &mut names, job, r, list.get(i + 1)).await?;
    let mut provenance = match &r.provenance {
        Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    provenance.insert("pins".to_string(), r.pins.clone());
    let bundled = sqlx::query_as::<_, BundledRow>(
        "SELECT ledger_id, key, kind, ref AS place_ref, area, page, label, at, by_sub \
           FROM release_bundled_draft WHERE job_id = $1 AND release_id = $2 ORDER BY at, ledger_id",
    )
    .bind(job.id)
    .bind(id)
    .fetch_all(&mut *c)
    .await?;
    let in_release = (r.state == "approved").then_some(r.release_id.as_str());
    let needed = bundled.iter().any(|b| may_show_base(&b.kind));
    let bases = LiveDraftBases::read(&mut *c, compiler, job, needed).await?;
    let mut drafts = Vec::with_capacity(bundled.len());
    for b in bundled {
        let place_ref = match b.place_ref {
            Value::Object(m) => m,
            other => {
                return Err(internal(format!(
                    "release_bundled_draft.ref is not an object: {other}"
                )));
            }
        };
        let d = ChangeRef {
            ledger_id: b.ledger_id,
            key: b.key,
            kind: b.kind,
            r#ref: place_ref,
            area: b.area,
            page: b.page,
            label: b.label,
            at: b.at,
            by_sub: b.by_sub,
        };
        drafts.push(
            draft_change(
                &mut *c,
                reg,
                &mut names,
                &bases,
                r.tenant_id,
                job,
                &d,
                in_release,
            )
            .await?,
        );
    }
    let events = rollbacks(&mut *c, &mut names, job, id).await?;
    // R15: a release cut before KH keeps the legacy shape; shown in the hierarchy shape
    v["selection"] = crate::platform::selection::selection_v2(&r.selection);
    v["provenance"] = Value::Object(provenance);
    v["readiness"] = r.readiness.clone().unwrap_or(Value::Null);
    v["bundle"] = serde_json::to_value(r.sections()?)
        .map_err(|e| internal(format!("bundle sections do not serialise: {e}")))?;
    v["requires"] = r.requires.clone();
    v["drafts"] = Value::Array(drafts);
    v["rollbacks"] = Value::Array(events);
    Ok(v)
}

/// `releaseCompare` (`ReleaseCompare`): release `id`'s sections against the current release's (fixture: each of its
/// sections with the items it adds and the items the current one has that it drops). 404 `release_not_found`.
pub async fn compare(c: &mut PgConnection, job: &JobRef, id: &str) -> ApiResult<Value> {
    let list = releases(&mut *c, job).await?;
    let r = list
        .iter()
        .find(|r| r.release_id == id)
        .ok_or_else(|| release_not_found(id))?;
    let cur = list.iter().find(|x| x.state == "current");
    let other_sections = match cur {
        Some(x) => x.sections()?,
        None => Vec::new(),
    };
    let sections: Vec<Value> = r
        .sections()?
        .into_iter()
        .map(|s| {
            let other: Vec<String> = other_sections
                .iter()
                .find(|x| x.key == s.key)
                .map(|x| x.items.clone())
                .unwrap_or_default();
            let added: Vec<&String> = s.items.iter().filter(|i| !other.contains(i)).collect();
            let mut removed: Vec<&String> = Vec::new();
            for i in &other {
                if !s.items.contains(i) && !removed.contains(&i) {
                    removed.push(i);
                }
            }
            json!({"key": s.key, "label": s.label, "added": added, "removed": removed})
        })
        .collect();
    Ok(json!({
        "id": id,
        "current": cur.map(|x| x.release_id.clone()),
        "same_hash": cur.is_some_and(|x| x.hash == r.hash),
        "sections": sections,
    }))
}

/// The draft selection compiled over `pins`; `None` without a selection or when the template refuses it.
pub async fn draft_compiled(
    c: &mut PgConnection,
    compiler: &dyn TemplateCompiler,
    sel: Option<&Selection>,
    pins: &[TemplatePin],
) -> ApiResult<Option<Compiled>> {
    let Some(sel) = sel else {
        return Ok(None);
    };
    match compiler.compile(&mut *c, sel, pins).await {
        Ok(x) => Ok(Some(x)),
        Err(CompileError::Api(e)) if !is_template_refusal(&e) => Err(*e),
        Err(_) => Ok(None),
    }
}

/// `releaseStatus` (`ReleaseStatus`, 05 §4.7):
/// - `base_changed`: the draft's `bundle_hash` differs from the current release's;
/// - `approved_waiting`: the newest release when it is an approved cut;
/// - `selection_error`: what makes the draft selection stale;
/// - `missing_slots`: what the publish gate would refuse the waiting cut on (its own design and requirements);
/// - `draft_hash`: the draft compile's `bundle_hash`;
/// - `newer_pins`: newer tags than the job's pins (`in_release`: the waiting cut already adopts it);
/// - `draft_count`: unpublished ledger rows.
pub async fn status(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    contributors: &Readiness,
    job: &JobRef,
) -> ApiResult<Value> {
    Ok(status_with_draft(c, reg, compiler, contributors, job)
        .await?
        .0)
}

/// [`status`] and the draft compile it made (`None`: no selection, or the template refused it), so a caller that
/// also needs the draft's readiness does not compile it again.
pub async fn status_with_draft(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    contributors: &Readiness,
    job: &JobRef,
) -> ApiResult<(Value, Option<Compiled>)> {
    let list = releases(&mut *c, job).await?;
    let current = list.iter().find(|r| r.state == "current");
    let waiting = list.first().filter(|r| r.state == "approved");
    let (pins, stored): (Value, Option<Value>) = sqlx::query_as(
        "SELECT j.pins, t.selection FROM jobs j LEFT JOIN job_target t ON t.job_id = j.id WHERE j.id = $1",
    )
    .bind(job.id)
    .fetch_one(&mut *c)
    .await?;
    let pins: Vec<TemplatePin> = serde_json::from_value(pins)
        .map_err(|e| internal(format!("jobs.pins is unreadable: {e}")))?;
    let sel: Option<Selection> = stored
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| internal(format!("job_target.selection is unreadable: {e}")))?;
    let draft = draft_compiled(&mut *c, compiler, sel.as_ref(), &pins).await?;
    let draft_hash = draft.as_ref().map(|d| d.bundle_hash.clone());
    let base_changed = current
        .zip(draft_hash.as_ref())
        .is_some_and(|(cur, h)| *h != cur.bundle_hash);
    let options = compiler.options(&mut *c).await?;
    let sel_error = selection_error(&options, sel.as_ref());
    let missing = match waiting {
        Some(w) => {
            let checked = readiness::checked_release(&mut *c, job, &w.release_id).await?;
            let cx = checked.cx(reg, compiler);
            contributors.blocking(&mut *c, &cx).await?
        }
        None => Vec::new(),
    };
    let waiting_pins = match waiting {
        Some(w) => w.pins()?,
        None => Vec::new(),
    };
    let latest = compiler.latest_tags(&mut *c).await?;
    let newer: Vec<Value> = pins
        .iter()
        .filter_map(|p| {
            let newest = latest.get(&p.layer).copied().unwrap_or(p.version);
            if newest <= p.version {
                return None;
            }
            let adopted = waiting_pins
                .iter()
                .find(|x| x.layer == p.layer)
                .is_some_and(|x| x.version > p.version);
            let in_release = waiting.filter(|_| adopted).map(|w| w.release_id.clone());
            Some(json!({
                "layer": p.layer,
                "pinned": p.version,
                "latest": newest,
                "in_release": in_release,
            }))
        })
        .collect();
    let draft_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM draft_ledger WHERE job_id = $1")
            .bind(job.id)
            .fetch_one(&mut *c)
            .await?;
    let status = json!({
        "base_changed": base_changed,
        "approved_waiting": waiting.map(|w| w.release_id.clone()),
        "selection_error": sel_error,
        "missing_slots": missing,
        "draft_hash": draft_hash,
        "newer_pins": newer,
        "draft_count": draft_count,
    });
    Ok((status, draft))
}

/// `releaseDesign` (`DesignState`, D62): the design release `id` stores, one line per place (kinds in
/// `design::KINDS` order). 404 `release_not_found`; 422 `bundle_invalid` when it stores no snapshot.
pub async fn release_design(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    id: &str,
) -> ApiResult<Value> {
    let r = load(&mut *c, job, id).await?;
    if r.snapshot_kinds.is_empty() {
        return Err(ApiError::Unprocessable(
            "bundle_invalid",
            format!("{id} stores no design snapshot."),
        ));
    }
    let mut rows =
        design::take_snapshot(&mut *c, reg, job, &DesignOf::Release(id.to_string())).await?;
    rows.sort_by_key(|r| {
        KINDS
            .iter()
            .position(|k| *k == r.kind)
            .unwrap_or(KINDS.len())
    });
    // a place without a tenant value (setting, overlay row) shows the base this release's bundle carries
    let bases = ViewBases::of_bundle(&r.bundle);
    let mut values = Vec::with_capacity(rows.len());
    for row in rows {
        let text = view_text(reg.target(&row.kind)?, &row.snap, &row.r#ref, &bases);
        values.push(json!({
            "key": row.key,
            "kind": row.kind,
            "ref": Value::Object(row.r#ref),
            "text": text,
        }));
    }
    Ok(json!({
        "of": "release",
        "release_id": id,
        "draft_rev": Value::Null,
        "values": values,
    }))
}
