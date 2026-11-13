//! Apply, rollback and re-apply (spec B2 §5 "Apply", D54/D62; a port of the fixture `applyIn` / `directionOf` /
//! `publishedEffect` / `markPublished` / `auditApply` and 05 §4.3–§4.4), plus `rollbackPreview` (fixture
//! `rollbackPreview` + `rollbackChange`). Everything runs in the caller's job-scoped transaction; no upstream call.
//!
//! [`apply`], in order:
//! 1. tenant suspended → 409 `tenant_suspended`; lock the job; the release (404 `release_not_found`); already current
//!    → nothing happens (`applied: false`); the stored bundle must still hash to its `bundle_hash` (else 500 and a
//!    `release.integrity_error` audit row written in its own transaction);
//! 2. no stored snapshot → 422 `bundle_invalid`;
//! 3. `from` = the live design, `to` = the release's stored design;
//! 4. the publish gate over the release's own design and requirements (409 `release_not_ready` + `blocking`);
//! 5. the direction: `apply` (an approved cut, or nothing live), else `rollback` / `reapply` by cut order; which
//!    published changes it takes out of Production / puts back ([`published_effect`], read before anything changes);
//! 6. the previous release → `applied_earlier`, this one → `current` (`released_*` once, `applied_*` always,
//!    `readiness`), `jobs.current_release_id` + `jobs.pins`, open upgrade requests the pins satisfy → `done`; the
//!    job's `prod` config nudge is queued for the caller's commit (B4 §0 R7, crate::runtime::design::nudge_on_commit);
//! 8. an approved cut publishes the drafts it bundled (`published_change`, before/after = the value in from/to);
//! 9. follow live (crate::release::follow; bumps `draft_rev` and queues the `test` nudge when a place moved, B4 R4);
//! 10. each bundled kind's `published` side effect;
//! 7. conflicts (crate::release::conflicts::on_apply) — after follow-live, so the detectors read the working rows
//!    as they now are;
//! 11. the rollback log row (when not a plain apply, or anything was undone, re-applied or moved);
//! 12. the published-row marks;
//! 13. audit: `release.apply`, `job.draft_published` ×n, `release.rollback|reapply`, `job.change_rolled_back` /
//!     `job.change_reapplied` ×n.
use std::collections::BTreeSet;

use agent_model::compile::Selection;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::approve::suspended;
use super::conflicts;
use super::follow::{Indexed, follow_live, plan_moves};
use super::hash::bundle_hash;
use super::summary::{self, Release};
use super::targets::{ViewBases, may_show_base, view_bases, view_text};
use crate::audit::{self, Event};
use crate::design::read::place_of;
use crate::design::snapshot::ref_of;
use crate::design::{self, DesignOf, DraftRef, JobRef, Place, Registry, snap_to_json};
use crate::error::{ApiError, ApiResult};
use crate::platform::templates::TemplatePin;
use crate::readiness;
use crate::runtime::design::nudge_on_commit;
use crate::runtime::scope::Env as NudgeEnv;
use crate::state::AppState;
use crate::tenancy::JobCtx;

/// Fixture `RollbackDirection`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Apply,
    Rollback,
    Reapply,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Apply => "apply",
            Direction::Rollback => "rollback",
            Direction::Reapply => "reapply",
        }
    }
}

/// What [`apply`] did (fixture `ApplyResult` minus the release row).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub release_id: String,
    /// `false` = the release was already current: nothing changed.
    pub applied: bool,
    pub previous: Option<String>,
    pub published: usize,
    pub direction: Direction,
    pub rolled_back: usize,
    pub reapplied: usize,
}

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}

fn bundle_invalid(id: &str) -> ApiError {
    ApiError::Unprocessable(
        "bundle_invalid",
        format!("{id} has no stored bundle; it cannot be applied."),
    )
}

/// The job's releases oldest → newest (fixture `releaseOrder`: cut order).
async fn release_order(c: &mut PgConnection, job: &JobRef) -> ApiResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT release_id FROM job_release WHERE job_id = $1 ORDER BY approved_at, release_id",
    )
    .bind(job.id)
    .fetch_all(&mut *c)
    .await?)
}

/// Fixture `order.indexOf`: -1 for a release not in the list.
fn rank(order: &[String], id: &str) -> i64 {
    order.iter().position(|x| x == id).map_or(-1, |i| i as i64)
}

/// Fixture `directionOf`: `apply` for an approved cut or when nothing is live, else `rollback` to an older cut and
/// `reapply` of a newer one.
fn direction_of(order: &[String], row: &Release, current: Option<&Release>) -> Direction {
    match current {
        _ if row.state == "approved" => Direction::Apply,
        None => Direction::Apply,
        Some(cur) if rank(order, &row.release_id) < rank(order, &cur.release_id) => {
            Direction::Rollback
        }
        Some(_) => Direction::Reapply,
    }
}

/// One published change (`published_change`).
#[derive(Clone, Debug, sqlx::FromRow)]
struct PubRow {
    id: i64,
    key: String,
    kind: String,
    place_ref: Value,
    area: String,
    page: String,
    label: String,
    published_in: String,
    published_at: DateTime<Utc>,
    rolled_back: Option<Value>,
}

/// The rollback log id (`rb-<n>`) a `rolled_back` mark names.
fn mark_rollback(m: &Value) -> Option<i64> {
    m.get("rollback")?
        .as_str()?
        .strip_prefix("rb-")?
        .parse()
        .ok()
}

/// Fixture `publishedEffect`: the published changes making `id` current takes out of Production (`rollback`:
/// published in a later cut and still live) and puts back (rolled back and published up to it, for a `reapply`; for
/// a cut, rolled back by a rollback logged after the cut was taken: index ≥ its `rollbacks_seen`).
async fn published_effect(
    c: &mut PgConnection,
    job: &JobRef,
    order: &[String],
    r: &Release,
    direction: Direction,
) -> ApiResult<(Vec<PubRow>, Vec<PubRow>)> {
    let ox = rank(order, &r.release_id);
    let log: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM release_rollback WHERE job_id = $1 ORDER BY id")
            .bind(job.id)
            .fetch_all(&mut *c)
            .await?;
    let rows = sqlx::query_as::<_, PubRow>(
        "SELECT id, key, kind, ref AS place_ref, area, page, label, published_in, published_at, rolled_back \
           FROM published_change WHERE job_id = $1 ORDER BY id",
    )
    .bind(job.id)
    .fetch_all(&mut *c)
    .await?;
    let seen = i64::from(r.rollbacks_seen);
    let mut undone = Vec::new();
    let mut reapplied = Vec::new();
    for p in rows {
        let pi = rank(order, &p.published_in);
        match &p.rolled_back {
            None if direction == Direction::Rollback && pi > ox => undone.push(p),
            None => {}
            Some(m) if pi <= ox => {
                let logged = mark_rollback(m)
                    .and_then(|n| log.iter().position(|x| *x == n))
                    .map_or(-1, |i| i as i64);
                if direction == Direction::Reapply
                    || (direction == Direction::Apply && logged >= seen)
                {
                    reapplied.push(p);
                }
            }
            Some(_) => {}
        }
    }
    Ok((undone, reapplied))
}

#[derive(sqlx::FromRow)]
struct Bundled {
    key: String,
    kind: String,
    place_ref: Value,
    area: String,
    page: String,
    label: String,
    at: DateTime<Utc>,
    by_sub: String,
}

/// The current release of the job (`None` = nothing live).
async fn current(c: &mut PgConnection, job: &JobRef) -> ApiResult<Option<Release>> {
    let id: Option<String> = sqlx::query_scalar(
        "SELECT release_id FROM job_release WHERE job_id = $1 AND state = 'current'",
    )
    .bind(job.id)
    .fetch_optional(&mut *c)
    .await?;
    match id {
        Some(id) => Ok(Some(summary::load(&mut *c, job, &id).await?)),
        None => Ok(None),
    }
}

/// Open conflicts of the job (`ApplyResult.conflicts_open`).
async fn open_conflicts(c: &mut PgConnection, job: &JobRef) -> ApiResult<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM release_conflict WHERE job_id = $1 AND status = 'open'",
    )
    .bind(job.id)
    .fetch_one(&mut *c)
    .await?)
}

/// §5 Apply (module docs): make release `id` current, in the caller's job-scoped transaction (`design::ensure_job`
/// already run). Approve with `apply` calls it in the cut's transaction (§5 step 13).
pub async fn apply(
    c: &mut PgConnection,
    st: &AppState,
    j: &JobCtx,
    id: &str,
) -> ApiResult<Applied> {
    let jr = job_ref(j);
    let reg = st.design.as_ref();
    let tenant = j.t.tenant;
    // 1
    suspended(&mut *c, j).await?;
    let old_pins: Value = sqlx::query_scalar("SELECT pins FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(jr.id)
        .fetch_optional(&mut *c)
        .await?
        .ok_or_else(|| ApiError::Missing("job_not_found", "That job does not exist.".into()))?;
    let old_pins: Vec<TemplatePin> = serde_json::from_value(old_pins)
        .map_err(|e| internal(format!("jobs.pins is unreadable: {e}")))?;
    let row = summary::load(&mut *c, &jr, id).await?;
    let previous = current(&mut *c, &jr).await?;
    if row.state == "current" {
        return Ok(Applied {
            release_id: id.to_string(),
            applied: false,
            previous: None,
            published: 0,
            direction: Direction::Apply,
            rolled_back: 0,
            reapplied: 0,
        });
    }
    let computed = bundle_hash(&row.bundle).ok();
    if computed.as_deref() != Some(row.bundle_hash.as_str()) {
        let e = Event::new("release.integrity_error", "release", id.to_string());
        let e = e
            .job(&jr.key)
            .failed("integrity_error")
            .detail("release_id", id.to_string())
            .detail("stored_bundle_hash", row.bundle_hash.clone())
            .detail("computed_bundle_hash", computed.clone());
        // its own pool connection while this transaction holds the job lock: under pool saturation the 500 waits
        // for the acquire timeout (acceptable; T12 review M5)
        audit::record_alone(&st.db, tenant, &j.t.actor(), e).await?;
        return Err(internal(format!(
            "release {id}: the stored bundle no longer hashes to its bundle_hash"
        )));
    }
    // 2
    if row.snapshot_kinds.is_empty() {
        return Err(bundle_invalid(id));
    }
    // 3
    let from = design::take_snapshot(&mut *c, reg, &jr, &DesignOf::Live).await?;
    let to = design::take_snapshot(&mut *c, reg, &jr, &DesignOf::Release(id.to_string())).await?;
    // 4
    let checked = readiness::checked_release(&mut *c, &jr, id).await?;
    let cx = checked.cx(reg, st.compiler.as_ref());
    st.readiness.gate(&mut *c, &cx, Some(id)).await?;
    // 5
    let was_approved = row.state == "approved";
    let order = release_order(&mut *c, &jr).await?;
    let direction = direction_of(&order, &row, previous.as_ref());
    let (undone, reapplied) = published_effect(&mut *c, &jr, &order, &row, direction).await?;
    // 6
    let now = Utc::now();
    let at = iso(now);
    let sub = j.t.caller.sub.as_str();
    sqlx::query(
        "UPDATE job_release SET state = 'applied_earlier' WHERE job_id = $1 AND state = 'current'",
    )
    .bind(jr.id)
    .execute(&mut *c)
    .await?;
    sqlx::query(
        "UPDATE job_release SET state = 'current', released_by = COALESCE(released_by, $3), \
                released_at = COALESCE(released_at, $4), applied_by = $3, last_applied_at = $4, readiness = $5 \
          WHERE job_id = $1 AND release_id = $2",
    )
    .bind(jr.id)
    .bind(id)
    .bind(sub)
    .bind(now)
    .bind(json!({"ready": true, "checked_at": at, "missing": []}))
    .execute(&mut *c)
    .await?;
    let new_pins = row.pins()?;
    sqlx::query("UPDATE jobs SET current_release_id = $2, pins = $3 WHERE id = $1")
        .bind(jr.id)
        .bind(id)
        .bind(&row.pins)
        .execute(&mut *c)
        .await?;
    // B4 §0 R7: live moved (apply, rollback or re-apply) — the job's prod scope re-reads its design once the
    // caller commits (never for a rolled-back transaction)
    nudge_on_commit(&mut *c, &[jr.id], NudgeEnv::Prod).await?;
    sqlx::query(
        "UPDATE template_upgrade_request SET status = 'done' \
          WHERE job_id = $1 AND status = 'open' AND $2::jsonb @> to_pins",
    )
    .bind(jr.id)
    .bind(&row.pins)
    .execute(&mut *c)
    .await?;
    // 8
    let bundled: Vec<Bundled> = if was_approved {
        sqlx::query_as::<_, Bundled>(
            "SELECT key, kind, ref AS place_ref, area, page, label, at, by_sub FROM release_bundled_draft \
              WHERE job_id = $1 AND release_id = $2 ORDER BY at, ledger_id",
        )
        .bind(jr.id)
        .bind(id)
        .fetch_all(&mut *c)
        .await?
    } else {
        Vec::new()
    };
    let (a, b) = (Indexed::new(&from), Indexed::new(&to));
    let mut published: Vec<(i64, &Bundled)> = Vec::with_capacity(bundled.len());
    for d in &bundled {
        let r = ref_of(d.place_ref.clone())?;
        let before = a.value_in(reg, &d.kind, &r)?;
        let after = b.value_in(reg, &d.kind, &r)?;
        let pid: i64 = sqlx::query_scalar(
            "INSERT INTO published_change (tenant_id, job_id, key, kind, ref, area, page, label, before, after, \
                                           at, by_sub, published_in, published_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) RETURNING id",
        )
        .bind(tenant)
        .bind(jr.id)
        .bind(&d.key)
        .bind(&d.kind)
        .bind(&d.place_ref)
        .bind(&d.area)
        .bind(&d.page)
        .bind(&d.label)
        .bind(snap_to_json(&before))
        .bind(snap_to_json(&after))
        .bind(d.at)
        .bind(&d.by_sub)
        .bind(id)
        .bind(sub)
        .fetch_one(&mut *c)
        .await?;
        published.push((pid, d));
    }
    // 9
    let publishing: BTreeSet<String> = bundled.iter().map(|d| d.key.clone()).collect();
    let moved = follow_live(&mut *c, reg, tenant, &jr, &from, &to, id, &at, &publishing).await?;
    // 10
    let place = Place {
        tenant,
        job: Some(&jr),
    };
    for d in &bundled {
        let r = ref_of(d.place_ref.clone())?;
        reg.target(&d.kind)?
            .published(&mut *c, place, &r, id)
            .await?;
    }
    // 7
    let from_bundle = previous.as_ref().map(|p| &p.bundle);
    let old_release_pins = match &previous {
        Some(p) => p.pins()?,
        None => old_pins,
    };
    let carried: Vec<String> = publishing.iter().cloned().collect();
    conflicts::on_apply(
        &mut *c,
        &st.conflict_detectors,
        tenant,
        &jr,
        id,
        &carried,
        from_bundle,
        &row.bundle,
        (old_release_pins.as_slice(), new_pins.as_slice()),
    )
    .await?;
    // 11–12
    let undone_ids: Vec<i64> = undone.iter().map(|p| p.id).collect();
    let reapplied_ids: Vec<i64> = reapplied.iter().map(|p| p.id).collect();
    let prev_id = previous.as_ref().map(|p| p.release_id.clone());
    let logged = direction != Direction::Apply
        || !undone.is_empty()
        || !reapplied.is_empty()
        || !moved.is_empty();
    let rollback: Option<String> = if logged {
        let n: i64 = sqlx::query_scalar(
            "INSERT INTO release_rollback (tenant_id, job_id, by_sub, direction, from_release, to_release, \
                                           undone, reapplied, draft_moved) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id",
        )
        .bind(tenant)
        .bind(jr.id)
        .bind(sub)
        .bind(direction.as_str())
        .bind(&prev_id)
        .bind(id)
        .bind(&undone_ids)
        .bind(&reapplied_ids)
        .bind(&moved)
        .fetch_one(&mut *c)
        .await?;
        Some(format!("rb-{n}"))
    } else {
        None
    };
    if !undone_ids.is_empty() {
        sqlx::query("UPDATE published_change SET rolled_back = $2 WHERE id = ANY($1)")
            .bind(&undone_ids)
            .bind(json!({"to": id, "at": at, "by": sub, "rollback": rollback.clone()}))
            .execute(&mut *c)
            .await?;
    }
    if !reapplied_ids.is_empty() {
        sqlx::query(
            "UPDATE published_change SET rolled_back = NULL, restored_to_draft = NULL WHERE id = ANY($1)",
        )
        .bind(&reapplied_ids)
        .execute(&mut *c)
        .await?;
    }
    // 13
    let actor = j.t.actor();
    let e = Event::new("release.apply", "release", id.to_string());
    let e = e
        .job(&jr.key)
        .detail("release_id", id.to_string())
        .detail("previous", prev_id.clone())
        .detail("direction", direction.as_str())
        .detail("published", published.len());
    audit::record(&mut *c, &actor, e).await?;
    for (pid, d) in &published {
        let e = Event::new("job.draft_published", "draft", d.key.clone());
        let e = e
            .job(&jr.key)
            .detail("kind", d.kind.clone())
            .detail("area", d.area.clone())
            .detail("page", d.page.clone())
            .detail("release", id.to_string())
            .detail("published_change_id", *pid);
        audit::record(&mut *c, &actor, e).await?;
    }
    if direction != Direction::Apply {
        let e = match direction {
            Direction::Rollback => Event::new("release.rollback", "release", id.to_string()),
            _ => Event::new("release.reapply", "release", id.to_string()),
        };
        let e = e
            .job(&jr.key)
            .detail("from", prev_id.clone())
            .detail("to", id.to_string())
            .detail("rolled_back", undone.len())
            .detail("reapplied", reapplied.len())
            .detail("draft_moved", moved.len())
            .detail("rollback", rollback.clone());
        audit::record(&mut *c, &actor, e).await?;
    }
    for p in &undone {
        let e = Event::new("job.change_rolled_back", "draft", p.key.clone());
        let e = e
            .job(&jr.key)
            .detail("kind", p.kind.clone())
            .detail("area", p.area.clone())
            .detail("page", p.page.clone())
            .detail("published_in", p.published_in.clone())
            .detail("release", id.to_string())
            .detail("rollback", rollback.clone());
        audit::record(&mut *c, &actor, e).await?;
    }
    for p in &reapplied {
        let e = Event::new("job.change_reapplied", "draft", p.key.clone());
        let e = e
            .job(&jr.key)
            .detail("kind", p.kind.clone())
            .detail("area", p.area.clone())
            .detail("page", p.page.clone())
            .detail("published_in", p.published_in.clone())
            .detail("release", id.to_string());
        audit::record(&mut *c, &actor, e).await?;
    }
    Ok(Applied {
        release_id: id.to_string(),
        applied: true,
        previous: prev_id,
        published: published.len(),
        direction,
        rolled_back: undone.len(),
        reapplied: reapplied.len(),
    })
}

/// The `ApplyResult` of `a` (fixture shape: an already-current release answers `{release, applied: false,
/// previous: null, conflicts_open}` only).
pub async fn result(c: &mut PgConnection, job: &JobRef, a: &Applied) -> ApiResult<Value> {
    let release = summary::row(&mut *c, job, &a.release_id).await?;
    let open = open_conflicts(&mut *c, job).await?;
    if !a.applied {
        return Ok(json!({
            "release": release,
            "applied": false,
            "previous": Value::Null,
            "conflicts_open": open,
        }));
    }
    Ok(json!({
        "release": release,
        "applied": true,
        "previous": a.previous,
        "published": a.published,
        "conflicts_open": open,
        "direction": a.direction.as_str(),
        "rolled_back": a.rolled_back,
        "reapplied": a.reapplied,
    }))
}

// ---- rollbackPreview ------------------------------------------------------------------------------------------------

/// Fixture `SELECTION_VALUE_KEYS`.
const SELECTION_VALUE_KEYS: [&str; 3] = ["area", "channel", "region"];

fn diff_values(parts: &mut Vec<String>, label: &str, a: &[String], b: &[String]) {
    for x in b {
        if !a.contains(x) {
            parts.push(format!("{label} + {x}"));
        }
    }
    for x in a {
        if !b.contains(x) {
            parts.push(format!("{label} − {x}"));
        }
    }
}

fn shown(v: &Option<String>) -> &str {
    v.as_deref().unwrap_or("null")
}

/// Fixture `describeChange(old, next)` (`old` present): `channel − telegram · region + de`, or `no change`.
pub fn describe_change(old: &Selection, next: &Selection) -> String {
    let mut parts: Vec<String> = Vec::new();
    if old.profile != next.profile {
        parts.push(format!(
            "profile {} → {}",
            shown(&old.profile),
            shown(&next.profile)
        ));
    }
    if old.industry != next.industry {
        parts.push(format!("industry {} → {}", old.industry, next.industry));
    }
    diff_values(
        &mut parts,
        "department",
        &old.departments,
        &next.departments,
    );
    // D115: the order is part of the selection (primary first)
    if old.departments != next.departments
        && old.departments.len() == next.departments.len()
        && old.departments.iter().all(|d| next.departments.contains(d))
    {
        parts.push(format!(
            "department order {} → {}",
            old.departments.join(", "),
            next.departments.join(", ")
        ));
    }
    for d in SELECTION_VALUE_KEYS {
        diff_values(&mut parts, d, old.values(d), next.values(d));
    }
    if parts.is_empty() {
        "no change".to_string()
    } else {
        parts.join(" · ")
    }
}

fn selection_of(r: &Release) -> ApiResult<Selection> {
    // R15: a release cut before KH keeps the legacy shape; upgraded in memory
    crate::design::read::selection_of_value(&r.selection)
        .map_err(|e| internal(format!("job_release.selection is unreadable: {e}")))
}

fn sections_json(r: Option<&Release>) -> ApiResult<Value> {
    let list = match r {
        Some(r) => r.sections()?,
        None => Vec::new(),
    };
    serde_json::to_value(list)
        .map_err(|e| internal(format!("bundle sections do not serialise: {e}")))
}

/// Fixture `rollbackChange`: one published row as a preview line — what Production runs at its place now (the live
/// design) and after (`to`, the target release's design), each a value over the bases of that design ([`view_text`]).
async fn rollback_change(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    to: &Indexed<'_>,
    bases: (&ViewBases, &ViewBases),
    p: &PubRow,
) -> ApiResult<Value> {
    let t = reg.target(&p.kind)?;
    let r: DraftRef = ref_of(p.place_ref.clone())?;
    let now = design::value(&mut *c, reg, job, &DesignOf::Live, &p.kind, &r).await?;
    let after = to.value_in(reg, &p.kind, &r)?;
    Ok(json!({
        "id": format!("pub-{}", p.id),
        "key": p.key,
        "kind": p.kind,
        "area": p.area,
        "page": p.page,
        "label": p.label,
        "published_in": p.published_in,
        "published_at": iso(p.published_at),
        "now": view_text(t, &now, &r, bases.0),
        "after": view_text(t, &after, &r, bases.1),
    }))
}

/// `rollbackPreview` (`RollbackPreview`, 05 §4.4): what making release `id` current would do, read only. 404
/// `release_not_found`; 422 `bundle_invalid` (no stored snapshot); 409 `already_current`. Its counts are exactly
/// what [`apply`] would then do: `undone` / `reapplied` = `rolled_back` / `reapplied`, `design_changes` = the places
/// whose live value moves, `draft` = the unpublished changes at those places.
pub async fn rollback_preview(
    c: &mut PgConnection,
    st: &AppState,
    job: &JobRef,
    id: &str,
) -> ApiResult<Value> {
    let reg = st.design.as_ref();
    let tenant: Uuid = sqlx::query_scalar("SELECT tenant_id FROM jobs WHERE id = $1")
        .bind(job.id)
        .fetch_one(&mut *c)
        .await?;
    let row = summary::load(&mut *c, job, id).await?;
    if row.snapshot_kinds.is_empty() {
        return Err(bundle_invalid(id));
    }
    if row.state == "current" {
        return Err(ApiError::Conflict(
            "already_current",
            format!("{id} is already live in Production."),
        ));
    }
    let cur = current(&mut *c, job).await?;
    let order = release_order(&mut *c, job).await?;
    let direction = direction_of(&order, &row, cur.as_ref());
    let from = design::take_snapshot(&mut *c, reg, job, &DesignOf::Live).await?;
    let to = design::take_snapshot(&mut *c, reg, job, &DesignOf::Release(id.to_string())).await?;
    let moves = plan_moves(&mut *c, reg, tenant, job, &from, &to).await?;
    let (undone, reapplied) = published_effect(&mut *c, job, &order, &row, direction).await?;
    let (a, b) = (Indexed::new(&from), Indexed::new(&to));
    let keys: BTreeSet<&str> = moves.iter().map(|m| m.place.key.as_str()).collect();
    let place = Place {
        tenant,
        job: Some(job),
    };
    let ledger_rows = design::ledger::of_job(&mut *c, job).await?;
    let needed = undone
        .iter()
        .chain(&reapplied)
        .any(|p| may_show_base(&p.kind))
        || ledger_rows.iter().any(|d| may_show_base(&d.kind));
    let compiler = st.compiler.as_ref();
    let (now_bases, after_bases, draft_bases) = if needed {
        (
            view_bases(&mut *c, compiler, job, &DesignOf::Live).await?,
            view_bases(&mut *c, compiler, job, &DesignOf::Release(id.to_string())).await?,
            view_bases(&mut *c, compiler, job, &DesignOf::Draft).await?,
        )
    } else {
        Default::default()
    };
    let mut draft = Vec::new();
    for d in ledger_rows {
        if !keys.contains(place_of(reg, &d.kind, &d.r#ref)?.key.as_str()) {
            continue;
        }
        let t = reg.target(&d.kind)?;
        let now = a.value_in(reg, &d.kind, &d.r#ref)?;
        let after = b.value_in(reg, &d.kind, &d.r#ref)?;
        let value = t.get(&mut *c, place, &d.r#ref).await?;
        draft.push(json!({
            "id": format!("d-{}", d.id),
            "key": d.key,
            "label": d.label,
            "live_now": view_text(t, &now, &d.r#ref, &now_bases),
            "live_after": view_text(t, &after, &d.r#ref, &after_bases),
            "draft": view_text(t, &value, &d.r#ref, &draft_bases),
            "matches_live": design::same(t, &value, &after),
        }));
    }
    let selection_change = match &cur {
        Some(x) => {
            let change = describe_change(&selection_of(x)?, &selection_of(&row)?);
            (change != "no change").then_some(change)
        }
        None => None,
    };
    let cur_pins = match &cur {
        Some(x) => x.pins()?,
        None => Vec::new(),
    };
    let pins: Vec<Value> = row
        .pins()?
        .into_iter()
        .filter_map(|p| {
            let was = cur_pins
                .iter()
                .find(|x| x.layer == p.layer)
                .map(|x| x.version);
            (was != Some(p.version))
                .then(|| json!({"layer": p.layer, "from": was, "to": p.version}))
        })
        .collect();
    let mut undone_rows = Vec::with_capacity(undone.len());
    for p in &undone {
        undone_rows
            .push(rollback_change(&mut *c, reg, job, &b, (&now_bases, &after_bases), p).await?);
    }
    let mut reapplied_rows = Vec::with_capacity(reapplied.len());
    for p in &reapplied {
        reapplied_rows
            .push(rollback_change(&mut *c, reg, job, &b, (&now_bases, &after_bases), p).await?);
    }
    let checked = readiness::checked_release(&mut *c, job, id).await?;
    let cx = checked.cx(reg, st.compiler.as_ref());
    let missing = st.readiness.blocking(&mut *c, &cx).await?;
    Ok(json!({
        "target": id,
        "current": cur.as_ref().map(|x| x.release_id.clone()),
        "direction": direction.as_str(),
        "bundle_now": sections_json(cur.as_ref())?,
        "bundle_after": sections_json(Some(&row))?,
        "selection_change": selection_change,
        "pins": pins,
        "undone": undone_rows,
        "reapplied": reapplied_rows,
        "draft": draft,
        "missing": missing,
        "design_changes": moves.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(channel: &[&str]) -> Selection {
        let mut s = Selection::default();
        s.dimensions.channel = channel.iter().map(|c| c.to_string()).collect();
        s
    }

    #[test]
    fn describe_change_matches_the_fixture() {
        assert_eq!(
            describe_change(&sel(&["chat", "telegram"]), &sel(&["chat"])),
            "channel − telegram"
        );
        assert_eq!(
            describe_change(&sel(&["chat"]), &sel(&["chat"])),
            "no change"
        );
    }

    #[test]
    fn rank_is_minus_one_when_absent() {
        let order = vec!["a".to_string(), "b".to_string()];
        assert_eq!(rank(&order, "b"), 1);
        assert_eq!(rank(&order, "z"), -1);
    }

    #[test]
    fn rollback_mark_names_its_log_row() {
        assert_eq!(mark_rollback(&json!({"rollback": "rb-7"})), Some(7));
        assert_eq!(mark_rollback(&json!({"rollback": null})), None);
        assert_eq!(mark_rollback(&json!({})), None);
    }
}
