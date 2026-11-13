//! Draft operations that are not a design write of a page (spec B2 §4 `discardDraft` / `restoreRolledBack`, D54/D62/
//! D69). Both write the working rows through the targets' `set` (never `track`: the value written is not something a
//! person typed, it is a stored live or published value), so each bumps `jobs.draft_rev` itself and audits its own
//! event. Ports of `fixtures/ledger.ts` `discardLedgerRow` and `areas/release.ts` `discardDraft` /
//! `restoreRolledBack`.
//!
//! Locking: both lock EVERY job of the tenant `ORDER BY id` first — the lock order of `design::track_tenant` — because
//! a tenant-wide kind (D69) re-records the ledger of every job. `design::track` holds one job lock only, so no cycle.
//! Ruling R4 ("`ensure_job` first") is deliberately reordered here: `ensure_job` takes this job's row `FOR UPDATE` on
//! first touch, so it runs right AFTER the tenant-wide lock (a job lock taken before it would invert the id order
//! against a concurrent `track_tenant`, discard or restore: 40P01). The handlers must not call it first.
use std::collections::BTreeSet;

use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::hooks::ensure_job;
use super::ledger::{self, Entry, LedgerRow};
use super::read::{place_of, value};
use super::snapshot::ref_of;
use super::{
    DesignOf, DesignTarget, DraftRef, JobRef, Place, Registry, Scope, Snap, snap_from_json,
};
use crate::audit::{self, Event};
use crate::error::{ApiError, ApiResult};
use crate::runtime::design::nudge_on_commit;
use crate::runtime::scope::Env as NudgeEnv;
use crate::tenancy::JobCtx;

/// The audit resource type of both operations (the design area of the change is a detail: `Event` takes a static
/// resource type).
const RESOURCE: &str = "draft";

/// The ledger id of an API draft id `d-<n>`; anything else is no draft.
pub fn ledger_id(raw: &str) -> Option<i64> {
    raw.strip_prefix("d-")?.parse().ok()
}

/// 404 `draft_not_found` (fixture text).
pub fn draft_not_found(raw: &str) -> ApiError {
    ApiError::Missing("draft_not_found", format!("No unpublished change {raw}."))
}

#[derive(sqlx::FromRow)]
struct TenantJob {
    id: Uuid,
    key: String,
}

/// Lock every job of the tenant, `ORDER BY id` (the `track_tenant` lock order).
async fn lock_tenant_jobs(c: &mut PgConnection, tenant: Uuid) -> ApiResult<Vec<JobRef>> {
    let rows = sqlx::query_as::<_, TenantJob>(
        "SELECT id, key FROM jobs WHERE tenant_id = $1 ORDER BY id FOR UPDATE",
    )
    .bind(tenant)
    .fetch_all(&mut *c)
    .await?;
    Ok(rows
        .into_iter()
        .map(|j| JobRef {
            id: j.id,
            key: j.key,
        })
        .collect())
}

/// Where a target's working row is written: tenant-wide (`job: None`) for a D69 kind, else the job's.
fn working_place<'a>(t: &dyn DesignTarget, tenant: Uuid, job: &'a JobRef) -> Place<'a> {
    Place {
        tenant,
        job: if t.scope() == Scope::Tenant {
            None
        } else {
            Some(job)
        },
    }
}

/// `draft_rev++` on the jobs whose draft changed (D63: their saved tests go stale), and their `test` config nudge
/// queued for when the caller commits (B4 §0 R7: crate::runtime::design::nudge_on_commit).
async fn bump(c: &mut PgConnection, jobs: Vec<Uuid>) -> ApiResult<()> {
    sqlx::query("UPDATE jobs SET draft_rev = draft_rev + 1 WHERE id = ANY($1)")
        .bind(&jobs)
        .execute(&mut *c)
        .await?;
    nudge_on_commit(&mut *c, &jobs, NudgeEnv::Test).await
}

/// The jobs whose draft an operation changed: every locked job when a tenant-wide kind was written, else this one.
fn changed_jobs(tenant_wide: bool, job: &JobRef, jobs: &[JobRef]) -> Vec<Uuid> {
    if tenant_wide {
        jobs.iter().map(|j| j.id).collect()
    } else {
        vec![job.id]
    }
}

/// Before a tenant-wide kind is written: every job of the tenant has its target row and baseline (a baseline taken
/// after the write would hold the written value).
async fn ensure_all(c: &mut PgConnection, reg: &Registry, jobs: &[JobRef]) -> ApiResult<()> {
    for j in jobs {
        ensure_job(&mut *c, reg, j).await?;
    }
    Ok(())
}

fn job_ref(job: &JobCtx) -> JobRef {
    JobRef {
        id: job.job_id,
        key: job.job_key.clone(),
    }
}

// ---- discard --------------------------------------------------------------------------------------------------------

/// Fixture `discardLedgerRow`: put the live value back at the row's place and drop the row. A tenant-wide row puts
/// back THIS job's live value for the whole tenant, and every other job's ledger is re-recorded against it.
async fn discard_row(
    c: &mut PgConnection,
    reg: &Registry,
    tenant: Uuid,
    job: &JobRef,
    jobs: &[JobRef],
    d: &LedgerRow,
) -> ApiResult<()> {
    let t = reg.target(&d.kind)?;
    let at = working_place(t, tenant, job);
    let was = t.get(&mut *c, at, &d.r#ref).await?;
    let live = value(&mut *c, reg, job, &DesignOf::Live, &d.kind, &d.r#ref).await?;
    t.set(&mut *c, at, &d.r#ref, &live).await?;
    sqlx::query("DELETE FROM draft_ledger WHERE id = $1")
        .bind(d.id)
        .execute(&mut *c)
        .await?;
    if t.scope() != Scope::Tenant {
        return Ok(());
    }
    let label = format!("{} — discarded from the draft in {}", d.label, job.key);
    let e = Entry {
        area: &d.area,
        page: &d.page,
        kind: &d.kind,
        r#ref: &d.r#ref,
        label: &label,
        by_sub: &d.by_sub,
    };
    for other in jobs.iter().filter(|j| j.id != job.id) {
        ledger::record(&mut *c, reg, tenant, other, &e, &was).await?;
    }
    Ok(())
}

/// J16: a conflict decision only a discarded draft carried is gone (the conflict is undecided again); one an
/// approved cut already carries stays decided, without its ledger row (`draft_id` null): it ships with that cut.
async fn clear_decisions(c: &mut PgConnection, job: &JobRef, keys: Vec<String>) -> ApiResult<()> {
    sqlx::query(
        "UPDATE release_conflict c \
            SET decision = CASE WHEN EXISTS ( \
                    SELECT 1 FROM release_bundled_draft b \
                      JOIN job_release r ON r.tenant_id = b.tenant_id AND r.job_id = b.job_id \
                                        AND r.release_id = b.release_id \
                     WHERE b.job_id = c.job_id AND b.key = c.decision ->> 'draft_key' \
                       AND r.state = 'approved') \
                THEN jsonb_set(c.decision, '{draft_id}', 'null'::jsonb) ELSE NULL END \
          WHERE c.job_id = $1 AND c.status = 'open' AND c.decision IS NOT NULL \
            AND c.decision ->> 'draft_key' = ANY($2)",
    )
    .bind(job.id)
    .bind(keys)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// `discardDraft`: drop one ledger row (`Some(id)`, 404 `draft_not_found` when the job has no such row) or all of
/// them (`None`), newest first, each through [`discard_row`]; then the conflict-decision cleanup, `draft_rev++` (every
/// job of the tenant when a tenant-wide row was discarded) and one `job.draft_discarded` audit row per discarded
/// change. Returns how many rows were discarded. Run in the caller's job-scoped transaction; runs `ensure_job` itself,
/// after the tenant-wide job lock (see the module doc).
pub async fn discard(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobCtx,
    id: Option<i64>,
) -> ApiResult<usize> {
    let tenant = job.t.tenant;
    let jr = job_ref(job);
    let jobs = lock_tenant_jobs(&mut *c, tenant).await?;
    ensure_job(&mut *c, reg, &jr).await?;
    let rows = ledger::of_job(&mut *c, &jr).await?;
    let pick: Vec<LedgerRow> = match id {
        None => rows,
        Some(n) => rows.into_iter().filter(|d| d.id == n).collect(),
    };
    if let Some(n) = id
        && pick.is_empty()
    {
        return Err(draft_not_found(&format!("d-{n}")));
    }
    if pick.is_empty() {
        return Ok(0);
    }
    let mut tenant_wide = false;
    for d in &pick {
        tenant_wide |= reg.target(&d.kind)?.scope() == Scope::Tenant;
    }
    if tenant_wide {
        ensure_all(&mut *c, reg, &jobs).await?;
    }
    for d in pick.iter().rev() {
        discard_row(&mut *c, reg, tenant, &jr, &jobs, d).await?;
    }
    clear_decisions(&mut *c, &jr, pick.iter().map(|d| d.key.clone()).collect()).await?;
    bump(&mut *c, changed_jobs(tenant_wide, &jr, &jobs)).await?;
    let actor = job.t.actor();
    for d in &pick {
        let e = Event::new("job.draft_discarded", RESOURCE, d.key.clone());
        let e = e
            .job(&jr.key)
            .detail("kind", d.kind.clone())
            .detail("area", d.area.clone())
            .detail("page", d.page.clone())
            .detail("ledger_id", d.id);
        audit::record(&mut *c, &actor, e).await?;
    }
    Ok(pick.len())
}

// ---- restore --------------------------------------------------------------------------------------------------------

/// Which rolled-back changes `restoreRolledBack` restores.
pub enum Restore {
    All,
    /// API ids `pub-<n>`.
    Ids(Vec<String>),
}

#[derive(sqlx::FromRow)]
struct RolledBack {
    id: i64,
    key: String,
    kind: String,
    place_ref: Value,
    area: String,
    page: String,
    label: String,
    after: Option<Value>,
    published_in: String,
    rolled_back: Option<Value>,
}

/// The value the release that published `p` stores at its place (D62, fixture: "the full value, not the row's
/// partial snapshot"); the row's published `after` when that release does not store the place's kind, or stores
/// nothing there.
async fn published_value(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    p: &RolledBack,
    r: &DraftRef,
) -> ApiResult<Snap> {
    let fallback = snap_from_json(p.after.clone())?;
    let kinds: Option<Vec<String>> = sqlx::query_scalar(
        "SELECT snapshot_kinds FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(job.id)
    .bind(&p.published_in)
    .fetch_optional(&mut *c)
    .await?;
    let place = place_of(reg, &p.kind, r)?;
    match kinds {
        Some(k) if k.contains(&place.kind) => {
            let of = DesignOf::Release(p.published_in.clone());
            Ok(value(&mut *c, reg, job, &of, &p.kind, r)
                .await?
                .or(fallback))
        }
        _ => Ok(fallback),
    }
}

/// `restoreRolledBack`: put changes a rollback took out of Production back in the draft. `Restore::Ids` naming a
/// change that is not rolled back (or already restored) → 404 `change_not_rolled_back`; nothing picked → 409
/// `nothing_to_restore`. One ledger row per place: the newest rolled-back change there wins (cut order of its
/// publishing release), with that release's stored value. A tenant-wide kind is re-recorded in every job's ledger
/// (D69, as `track_tenant`). Every picked change gets `restored_to_draft`, the changed jobs `draft_rev++`, and one
/// `job.change_restored_to_draft` audit row each. Returns how many changes were restored. Runs `ensure_job` itself,
/// after the tenant-wide job lock (see the module doc).
pub async fn restore(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobCtx,
    which: Restore,
) -> ApiResult<usize> {
    let tenant = job.t.tenant;
    let jr = job_ref(job);
    let jobs = lock_tenant_jobs(&mut *c, tenant).await?;
    ensure_job(&mut *c, reg, &jr).await?;
    let all = sqlx::query_as::<_, RolledBack>(
        "SELECT id, key, kind, ref AS place_ref, area, page, label, after, published_in, rolled_back \
           FROM published_change \
          WHERE job_id = $1 AND rolled_back IS NOT NULL AND restored_to_draft IS NULL \
          ORDER BY id",
    )
    .bind(jr.id)
    .fetch_all(&mut *c)
    .await?;
    let pick: Vec<RolledBack> = match which {
        Restore::All => all,
        Restore::Ids(ids) => {
            let wanted: BTreeSet<String> = ids.into_iter().collect();
            let pick: Vec<RolledBack> = all
                .into_iter()
                .filter(|p| wanted.contains(&format!("pub-{}", p.id)))
                .collect();
            if pick.len() != wanted.len() {
                return Err(ApiError::Missing(
                    "change_not_rolled_back",
                    "Only changes a rollback took out of Production can be restored.".into(),
                ));
            }
            pick
        }
    };
    if pick.is_empty() {
        return Err(ApiError::Conflict(
            "nothing_to_restore",
            "No rolled-back change waits to be restored.".into(),
        ));
    }
    let order: Vec<String> = sqlx::query_scalar(
        "SELECT release_id FROM job_release WHERE job_id = $1 ORDER BY approved_at, release_id",
    )
    .bind(jr.id)
    .fetch_all(&mut *c)
    .await?;
    // fixture `order.indexOf`: a release not found ranks below every release
    let rank = |r: &str| order.iter().position(|x| x == r).map_or(-1, |i| i as i64);
    let mut winners: Vec<&RolledBack> = Vec::new();
    for p in &pick {
        let found = winners.iter().position(|w| w.key == p.key);
        match found {
            None => winners.push(p),
            Some(i) if rank(p.published_in.as_str()) >= rank(winners[i].published_in.as_str()) => {
                winners[i] = p
            }
            Some(_) => {}
        }
    }
    let mut tenant_wide = false;
    for p in &winners {
        tenant_wide |= reg.target(&p.kind)?.scope() == Scope::Tenant;
    }
    if tenant_wide {
        ensure_all(&mut *c, reg, &jobs).await?;
    }
    let actor = job.t.actor();
    for p in &winners {
        let t = reg.target(&p.kind)?;
        let r = ref_of(p.place_ref.clone())?;
        let restored = published_value(&mut *c, reg, &jr, p, &r).await?;
        // the jobs whose ledger records it, each against its own live value (read before the write)
        let targets: Vec<&JobRef> = if t.scope() == Scope::Tenant {
            jobs.iter().collect()
        } else {
            vec![&jr]
        };
        let mut lives = Vec::with_capacity(targets.len());
        for j in &targets {
            lives.push(value(&mut *c, reg, j, &DesignOf::Live, &p.kind, &r).await?);
        }
        let at = working_place(t, tenant, &jr);
        t.set(&mut *c, at, &r, &restored).await?;
        let label = format!("Restored after rollback: {}", p.label);
        let e = Entry {
            area: &p.area,
            page: &p.page,
            kind: &p.kind,
            r#ref: &r,
            label: &label,
            by_sub: &actor.id,
        };
        for (j, live) in targets.iter().zip(&lives) {
            ledger::record(&mut *c, reg, tenant, j, &e, live).await?;
        }
    }
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::AutoSi, true);
    let ids: Vec<i64> = pick.iter().map(|p| p.id).collect();
    sqlx::query("UPDATE published_change SET restored_to_draft = $2 WHERE id = ANY($1)")
        .bind(ids)
        .bind(json!({"at": now, "by": actor.id}))
        .execute(&mut *c)
        .await?;
    bump(&mut *c, changed_jobs(tenant_wide, &jr, &jobs)).await?;
    for p in &pick {
        let to = p
            .rolled_back
            .as_ref()
            .and_then(|v| v.get("to"))
            .cloned()
            .unwrap_or(Value::Null);
        let e = Event::new("job.change_restored_to_draft", RESOURCE, p.key.clone());
        let e = e
            .job(&jr.key)
            .detail("kind", p.kind.clone())
            .detail("area", p.area.clone())
            .detail("published_change_id", p.id)
            .detail("published_in", p.published_in.clone())
            .detail("rolled_back_to", to);
        audit::record(&mut *c, &actor, e).await?;
    }
    Ok(pick.len())
}
