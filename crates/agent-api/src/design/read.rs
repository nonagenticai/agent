//! The read API (spec B2 §2.4): how a job's live design = its active release snapshot. A port of
//! `areas/design.ts` `designViewOf` / `designStateOf` and `fixtures/ledger.ts` `liveSnap` / `valueIn` / `placeOf`.
//!
//! - Production readers use [`DesignOf::Live`]: the `release_snapshot` rows of `jobs.current_release_id`, or the
//!   `release_baseline` of a job never released.
//! - Test-env readers use [`DesignOf::Draft`]: `target.get` on the working rows.
//! - Nothing here reads `draft_ledger`.
//! - A projection kind is read through its parent place (`parent` + `project`).
//!
//! The snapshot LRU ([`SnapshotCache`], 256 entries) lives on the [`Registry`] (`AppState.design`), so every read
//! that has the registry has the cache. It holds release snapshots only, keyed by `(tenant, job, release_id)` and
//! validated against the release's `hash` + `snapshot_kinds` on every hit. Baselines are never cached.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use agent_model::compile::Selection;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::snapshot::{DesignRow, Snapshot, internal, job_state, stored, working_rows};
use super::{DesignOf, DesignTarget, DraftRef, Env, JobRef, Place, Registry, Snap, snap_from_json};
use crate::error::ApiResult;

/// The LRU capacity (spec §2.4).
pub const SNAPSHOT_CACHE_CAP: usize = 256;

type CacheKey = (Uuid, Uuid, String);

#[derive(Default)]
struct CacheInner {
    map: HashMap<CacheKey, Arc<Snapshot>>,
    /// Least recently used first.
    order: VecDeque<CacheKey>,
}

impl CacheInner {
    fn touch(&mut self, key: &CacheKey) {
        self.order.retain(|k| k != key);
        self.order.push_back(key.clone());
    }
}

/// In-process LRU of release snapshots (immutable once stored). Never held across an `.await`.
#[derive(Default)]
pub struct SnapshotCache {
    inner: Mutex<CacheInner>,
}

impl SnapshotCache {
    /// The cached snapshot of `key`, only if it is the release version `hash` + `kinds` names.
    pub(crate) fn get(
        &self,
        key: &CacheKey,
        hash: &str,
        kinds: &[String],
    ) -> Option<Arc<Snapshot>> {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let hit = g
            .map
            .get(key)
            .filter(|s| s.same_version(hash, kinds))
            .cloned();
        if hit.is_some() {
            g.touch(key);
        }
        hit
    }

    pub(crate) fn put(&self, key: CacheKey, snap: Arc<Snapshot>) {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.map.insert(key.clone(), snap);
        g.touch(&key);
        while g.order.len() > SNAPSHOT_CACHE_CAP {
            if let Some(old) = g.order.pop_front() {
                g.map.remove(&old);
            }
        }
    }
}

/// A snapshot place (fixture `DesignPlace`).
#[derive(Clone, Debug, PartialEq)]
pub struct DesignPlace {
    pub key: String,
    pub kind: String,
    pub r#ref: DraftRef,
}

/// The snapshot place a location belongs to (fixture `placeOf`): a projection kind → its parent's place.
pub(crate) fn place_of(reg: &Registry, kind: &str, r: &DraftRef) -> ApiResult<DesignPlace> {
    let t = reg.target(kind)?;
    Ok(match t.parent(r) {
        Some((pkind, pref)) => DesignPlace {
            key: reg.target(pkind)?.key(&pref),
            kind: pkind.to_string(),
            r#ref: pref,
        },
        None => DesignPlace {
            key: t.key(r),
            kind: kind.to_string(),
            r#ref: r.clone(),
        },
    })
}

/// What counts as the same value at a place of `t`'s kind (fixture `sameSnap`): equal `proj`.
pub fn same(t: &dyn DesignTarget, a: &Snap, b: &Snap) -> bool {
    t.proj(a) == t.proj(b)
}

/// The value `s` holds at `(kind, r)` (fixture `valueIn`); `None` when `s` does not cover the place's kind (the
/// caller reads the working rows).
fn value_in(
    reg: &Registry,
    s: &Snapshot,
    t: &dyn DesignTarget,
    kind: &str,
    r: &DraftRef,
) -> ApiResult<Option<Snap>> {
    let p = place_of(reg, kind, r)?;
    if !s.covers(&p.kind) {
        return Ok(None);
    }
    let val: Snap = s.row(&p.key).and_then(|row| row.snap.clone());
    Ok(Some(in_place(t, kind, &p, val, r)))
}

/// The value of `(kind, r)` given the value `val` stored at its snapshot place `p`: itself, or a projection kind's
/// field of it (fixture `valueIn`: nothing at the parent → nothing).
fn in_place(t: &dyn DesignTarget, kind: &str, p: &DesignPlace, val: Snap, r: &DraftRef) -> Snap {
    if p.kind == kind || val.is_none() {
        return val;
    }
    t.project(&val, r)
}

/// The value at one place of the job's design `of` (fixture `liveSnap` for `Live`, `target.get` for `Draft`).
pub async fn value(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
    kind: &str,
    r: &DraftRef,
) -> ApiResult<Snap> {
    let t = reg.target(kind)?;
    let st = job_state(&mut *c, job).await?;
    if *of == DesignOf::Live && st.current_release_id.is_none() {
        // never released: live = the stored baseline, read by the place's key (not the whole baseline)
        if let Some(kinds) = &st.baseline_kinds {
            let p = place_of(reg, kind, r)?;
            if kinds.contains(&p.kind) {
                let row: Option<Option<Value>> = sqlx::query_scalar(
                    "SELECT snap FROM release_baseline WHERE job_id = $1 AND key = $2",
                )
                .bind(job.id)
                .bind(&p.key)
                .fetch_optional(&mut *c)
                .await?;
                let val = snap_from_json(row.flatten())?;
                return Ok(in_place(t, kind, &p, val, r));
            }
        }
    } else if let Some(s) = stored(&mut *c, reg, job, &st, of).await? {
        if let Some(v) = value_in(reg, &s, t, kind, r)? {
            return Ok(v);
        }
        // the stored snapshot does not cover the kind: read the working rows (kept behaviour), but say so
        if *of == DesignOf::Live
            && let Some(id) = &st.current_release_id
        {
            uncovered_live_read(st.tenant_id, job, &place_of(reg, kind, r)?.kind, id);
        }
    }
    let at = Place {
        tenant: st.tenant_id,
        job: Some(job),
    };
    t.get(c, at, r).await
}

/// The live release id when the job HAS a live release whose snapshot does not store the place kind of `(kind, r)`
/// (§2.5 coverage), else `None` (covered, or never released: the baseline is live). [`value`] Live falls through to
/// the working rows there, so "draft = live" does not mean "nothing to publish": the ledger keeps such a change
/// ([`super::ledger::record`]) and the J14 draft summary marks it `uncovered` (owner ruling 2026-10-22).
pub(crate) async fn live_uncovered(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    kind: &str,
    r: &DraftRef,
) -> ApiResult<Option<String>> {
    let st = job_state(&mut *c, job).await?;
    let Some(id) = st.current_release_id.clone() else {
        return Ok(None);
    };
    let Some(s) = stored(&mut *c, reg, job, &st, &DesignOf::Live).await? else {
        return Ok(None);
    };
    let p = place_of(reg, kind, r)?;
    Ok((!s.covers(&p.kind)).then_some(id))
}

/// Every place of `kind` in the job's design `of`, with its value. A projection kind has no places of its own (read
/// its parent kind): asking for one is a programming error (500).
pub async fn values_of_kind(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
    kind: &str,
) -> ApiResult<Vec<DesignRow>> {
    let t = reg.target(kind)?;
    if t.is_projection() {
        return Err(internal(format!(
            "{kind} is a projection kind: read its parent kind's places"
        )));
    }
    let st = job_state(&mut *c, job).await?;
    if let Some(s) = stored(&mut *c, reg, job, &st, of).await? {
        if s.covers(kind) {
            return Ok(s
                .rows()
                .iter()
                .filter(|r| r.kind == kind)
                .cloned()
                .collect());
        }
        if *of == DesignOf::Live
            && let Some(id) = &st.current_release_id
        {
            uncovered_live_read(st.tenant_id, job, kind, id);
        }
    }
    working_rows(c, reg, st.tenant_id, job, &[t.kind()]).await
}

/// A Live read of a kind the live release `release` does not store fell through to the working rows (§2.5: Production
/// runs the saved value until `coverage::backfill` covers the kind, or a publish does). Counted every time
/// (`agent_design_live_read_uncovered_total{kind}`); logged at WARN once per (tenant, job, kind, release) per process,
/// DEBUG after, so a hot read path does not flood the log. The set is bounded by the uncovered places a process sees.
fn uncovered_live_read(tenant: Uuid, job: &JobRef, kind: &str, release: &str) {
    metrics::counter!("agent_design_live_read_uncovered_total", "kind" => kind.to_string())
        .increment(1);
    type Seen = std::collections::HashSet<(Uuid, Uuid, String, String)>;
    static SEEN: std::sync::OnceLock<Mutex<Seen>> = std::sync::OnceLock::new();
    let first = SEEN
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert((tenant, job.id, kind.to_string(), release.to_string()));
    if first {
        tracing::warn!(%tenant, job = %job.key, kind, release_id = release, "design_live_read_uncovered");
    } else {
        tracing::debug!(%tenant, job = %job.key, kind, release_id = release, "design_live_read_uncovered");
    }
}

/// [`value`] deserialised into `T`: `None` = nothing at the place; a value of another shape is a 500.
pub async fn typed<T: DeserializeOwned>(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
    kind: &str,
    r: &DraftRef,
) -> ApiResult<Option<T>> {
    match value(c, reg, job, of, kind, r).await? {
        None => Ok(None),
        Some(v) => serde_json::from_value(v).map(Some).map_err(|e| {
            internal(format!(
                "design value of kind {kind} has another shape: {e}"
            ))
        }),
    }
}

/// Ruling R15 (KH-F3): a stored selection as a [`Selection`], read through the one conversion of the hierarchy shape
/// ([`crate::platform::selection::selection_v2`], migration 0132's `kh_selection_v2`). The release identity —
/// `job_release.selection`, `release_snapshot`, `release_baseline` — is immutable (D62) and 0132 does not rewrite
/// it, so a release cut before KH still holds `industry_depts` / `dimensions.region`; every reader of a selection
/// upgrades it in memory before deserialising, so a pre-KH release stays live (and can be rolled back to). A
/// selection still without the hierarchy after the upgrade (no industry, no department) parses and is refused
/// `hierarchy_incomplete` by its compile (Ruling R2) — the upgrade never invents a department. A JSON `null` reads
/// as no selection.
pub fn selection_of_value(v: &Value) -> Result<Selection, serde_json::Error> {
    if v.is_null() {
        return Ok(Selection::default());
    }
    serde_json::from_value(crate::platform::selection::selection_v2(v))
}

/// The job's selection in design `of` ([`value`] of kind `selection`, upgraded by [`selection_of_value`]); no stored
/// selection reads as the empty one (refused `hierarchy_incomplete` by a job-context compile). A value of another
/// shape is a 500.
pub async fn selection(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<Selection> {
    match value(c, reg, job, of, "selection", &DraftRef::new()).await? {
        None => Ok(Selection::default()),
        Some(v) => selection_of_value(&v).map_err(|e| {
            internal(format!(
                "design value of kind selection has another shape: {e}"
            ))
        }),
    }
}

/// D63 case identity for the runtime (B4): Production runs Live (the current release, `None` = never released, its
/// baseline), the Test environment runs the Draft at `draft_rev`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaseDesign {
    Live { release_id: Option<String> },
    Draft { draft_rev: i64 },
}

pub async fn case_design(c: &mut PgConnection, job: &JobRef, env: Env) -> ApiResult<CaseDesign> {
    let st = job_state(c, job).await?;
    Ok(match env {
        Env::Production => CaseDesign::Live {
            release_id: st.current_release_id,
        },
        Env::Test => CaseDesign::Draft {
            draft_rev: st.draft_rev,
        },
    })
}
